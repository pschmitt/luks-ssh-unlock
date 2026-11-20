//! Unpacking an initrd image the way the kernel's `unpack_to_rootfs` does:
//! a sequence of newc entries, zero padding and compressed segments, each
//! compressed segment itself a sequence of newc entries.

use std::collections::HashMap;
use std::io::{self, BufRead};

use sha2::{Digest, Sha256};

use crate::decompress;
use crate::vfs::Vfs;
use crate::{Error, Result};

const HEADER_LEN: usize = 110;
/// Longer names and symlink targets are skipped by the kernel.
const PATH_MAX: u64 = 4096;

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFLNK: u32 = 0o120_000;

/// A byte stream that knows its offset, which newc alignment is relative to.
pub struct Stream<R> {
    inner: R,
    pos: u64,
}

impl<R: BufRead> Stream<R> {
    pub fn new(inner: R, pos: u64) -> Self {
        Self { inner, pos }
    }

    pub fn pos(&self) -> u64 {
        self.pos
    }

    fn peek(&mut self) -> io::Result<Option<u8>> {
        Ok(self.inner.fill_buf()?.first().copied())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_exact(buf)?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    /// Feed `len` bytes to `sink`; an early EOF is an error.
    fn take(&mut self, mut len: u64, mut sink: impl FnMut(&[u8])) -> io::Result<()> {
        while len > 0 {
            let buf = self.inner.fill_buf()?;
            if buf.is_empty() {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let n = buf.len().min(usize::try_from(len).unwrap_or(usize::MAX));
            sink(&buf[..n]);
            self.inner.consume(n);
            self.pos += n as u64;
            len -= n as u64;
        }
        Ok(())
    }

    fn skip(&mut self, len: u64) -> io::Result<()> {
        self.take(len, |_| {})
    }

    /// Skip to the next multiple of four. EOF inside the padding is
    /// tolerated, as the kernel does at the end of the buffer.
    fn align(&mut self) -> io::Result<()> {
        let pad = (4 - self.pos % 4) % 4;
        match self.skip(pad) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
            other => other,
        }
    }

    /// Skip zero bytes; the kernel's `do_reset`.
    fn skip_zeros(&mut self) -> io::Result<()> {
        loop {
            let buf = self.inner.fill_buf()?;
            let n = buf.iter().take_while(|&&b| b == 0).count();
            if n == 0 {
                return Ok(());
            }
            let all = n == buf.len();
            self.inner.consume(n);
            self.pos += n as u64;
            if !all {
                return Ok(());
            }
        }
    }
}

struct Header {
    ino: u32,
    mode: u32,
    nlink: u32,
    filesize: u64,
    devmajor: u32,
    devminor: u32,
    namesize: u64,
    /// The sum of the data bytes, for the `070702` format.
    check: Option<u32>,
}

fn hex_field(h: &[u8], idx: usize) -> Result<u32> {
    let start = 6 + idx * 8;
    let field = &h[start..start + 8];
    std::str::from_utf8(field)
        .ok()
        .and_then(|s| u32::from_str_radix(s, 16).ok())
        .ok_or_else(|| Error::Failed(format!("malformed cpio header field {idx}")))
}

fn parse_header(h: &[u8; HEADER_LEN]) -> Result<Header> {
    let check = match &h[..6] {
        b"070701" => None,
        b"070702" => Some(hex_field(h, 12)?),
        b"070707" => {
            return Err(Error::Failed(
                "old binary cpio format; only newc is supported".into(),
            ));
        }
        _ => return Err(Error::Failed("no cpio magic".into())),
    };
    Ok(Header {
        ino: hex_field(h, 0)?,
        mode: hex_field(h, 1)?,
        nlink: hex_field(h, 4)?,
        filesize: hex_field(h, 6)?.into(),
        devmajor: hex_field(h, 7)?,
        devminor: hex_field(h, 8)?,
        namesize: hex_field(h, 11)?.into(),
        check,
    })
}

type LinkKey = (u32, u32, u32, u32);

/// The unpacker's state across the whole image: the tree, and the names of
/// hard-linked inodes seen since the last `TRAILER!!!`.
pub struct Unpacker {
    pub vfs: Vfs,
    links: HashMap<LinkKey, Vec<u8>>,
}

impl Default for Unpacker {
    fn default() -> Self {
        Self::new()
    }
}

impl Unpacker {
    pub fn new() -> Self {
        Self {
            vfs: Vfs::new(),
            links: HashMap::new(),
        }
    }

    /// One newc entry, positioned at its magic.
    fn entry<R: BufRead>(&mut self, s: &mut Stream<R>) -> Result<()> {
        let mut raw = [0u8; HEADER_LEN];
        s.read_exact(&mut raw)
            .map_err(|e| Error::io("truncated cpio header", e))?;
        let h = parse_header(&raw)?;
        let fmt = h.mode & S_IFMT;
        let truncated = |e| Error::io("truncated cpio entry", e);

        // Entries the kernel skips whole.
        let skip = h.namesize == 0
            || h.namesize > PATH_MAX
            || (fmt == S_IFLNK && h.filesize > PATH_MAX)
            || (fmt != S_IFREG && fmt != S_IFLNK && h.filesize != 0);
        if skip {
            s.skip(h.namesize).map_err(truncated)?;
            s.align().map_err(truncated)?;
            s.skip(h.filesize).map_err(truncated)?;
            s.align().map_err(truncated)?;
            return Ok(());
        }

        let mut name = vec![0u8; usize::try_from(h.namesize).unwrap_or(0)];
        s.read_exact(&mut name).map_err(truncated)?;
        s.align().map_err(truncated)?;
        if name.pop() != Some(0) {
            return Err(Error::Failed("cpio name without a NUL terminator".into()));
        }
        // A C string: anything after an embedded NUL is not part of it.
        if let Some(nul) = name.iter().position(|&b| b == 0) {
            name.truncate(nul);
        }

        match fmt {
            S_IFLNK => {
                let mut target = Vec::new();
                s.take(h.filesize, |b| target.extend_from_slice(b))
                    .map_err(truncated)?;
                s.align().map_err(truncated)?;
                if let Some(nul) = target.iter().position(|&b| b == 0) {
                    target.truncate(nul);
                }
                self.vfs.symlink(&name, &target);
                return Ok(());
            }
            _ if name == b"TRAILER!!!" => {
                self.links.clear();
            }
            S_IFREG => {
                let mut hasher = Sha256::new();
                let mut sum = 0u32;
                s.take(h.filesize, |b| {
                    hasher.update(b);
                    if h.check.is_some() {
                        sum = b.iter().fold(sum, |acc, &x| acc.wrapping_add(x.into()));
                    }
                })
                .map_err(truncated)?;
                if let Some(check) = h.check
                    && check != sum
                {
                    return Err(Error::Failed(format!(
                        "bad data checksum for {}",
                        String::from_utf8_lossy(&name)
                    )));
                }
                let digest: [u8; 32] = hasher.finalize().into();
                let key = (h.devmajor, h.devminor, h.ino, fmt);
                let linked = if h.nlink >= 2 {
                    match self.links.get(&key) {
                        Some(old) => {
                            let old = old.clone();
                            let data = (h.filesize > 0).then_some(digest);
                            // A failed link drops the entry; either way it
                            // is not created as a new file.
                            self.vfs.link(&name, &old, data);
                            true
                        }
                        None => {
                            self.links.insert(key, name.clone());
                            false
                        }
                    }
                } else {
                    false
                };
                if !linked {
                    self.vfs.file(&name, digest);
                }
            }
            S_IFDIR => self.vfs.mkdir(&name),
            _ => {
                // Device nodes, FIFOs and sockets can be hard links too;
                // either way nothing is measured.
                let key = (h.devmajor, h.devminor, h.ino, fmt);
                if h.nlink < 2 || self.links.insert(key, name.clone()).is_none() {
                    self.vfs.special(&name);
                }
            }
        }
        s.align().map_err(truncated)?;
        Ok(())
    }

    /// Entries separated by zero padding until EOF: the content of one
    /// compressed segment. Anything else is junk.
    pub fn stream<R: BufRead>(&mut self, s: &mut Stream<R>) -> Result<()> {
        loop {
            s.skip_zeros()
                .map_err(|e| Error::io("cannot read the initrd", e))?;
            match s
                .peek()
                .map_err(|e| Error::io("cannot read the initrd", e))?
            {
                None => return Ok(()),
                Some(b'0') if s.pos().is_multiple_of(4) => self.entry(s)?,
                Some(_) if !s.pos().is_multiple_of(4) => {
                    return Err(Error::Failed("broken padding in cpio archive".into()));
                }
                Some(_) => {
                    return Err(Error::Failed("junk within compressed archive".into()));
                }
            }
        }
    }

    /// A whole image: uncompressed entries, zero padding and compressed
    /// segments, in any order.
    pub fn image(&mut self, data: &[u8]) -> Result<()> {
        let mut pos = 0usize;
        while pos < data.len() {
            let rest = &data[pos..];
            if rest[0] == b'0' && pos.is_multiple_of(4) {
                let mut s = Stream::new(rest, pos as u64);
                self.entry(&mut s)?;
                pos = usize::try_from(s.pos()).unwrap_or(data.len());
            } else if rest[0] == 0 {
                pos += 1;
            } else {
                let used = decompress::segment(rest, |r| {
                    let mut s = Stream::new(r, 0);
                    self.stream(&mut s)
                })
                .map_err(|e| e.context(&format!("at offset {pos}")))?;
                pos += used.max(1);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::report::Kind;

    /// A newc archive writer for the tests.
    #[derive(Default)]
    pub struct Writer {
        pub buf: Vec<u8>,
        ino: u32,
    }

    impl Writer {
        pub fn raw(&mut self, name: &[u8], mode: u32, nlink: u32, ino: u32, data: &[u8]) {
            let fields = [
                ino,
                mode,
                0,
                0,
                nlink,
                0,
                data.len() as u32,
                0,
                0,
                0,
                0,
                name.len() as u32 + 1,
                0,
            ];
            self.buf.extend_from_slice(b"070701");
            for f in fields {
                self.buf.extend_from_slice(format!("{f:08x}").as_bytes());
            }
            self.buf.extend_from_slice(name);
            self.buf.push(0);
            self.pad();
            self.buf.extend_from_slice(data);
            self.pad();
        }

        fn pad(&mut self) {
            while !self.buf.len().is_multiple_of(4) {
                self.buf.push(0);
            }
        }

        fn next_ino(&mut self) -> u32 {
            self.ino += 1;
            self.ino
        }

        pub fn dir(&mut self, name: &str) -> &mut Self {
            let ino = self.next_ino();
            self.raw(name.as_bytes(), 0o040_755, 2, ino, b"");
            self
        }

        pub fn file(&mut self, name: &str, data: &[u8]) -> &mut Self {
            let ino = self.next_ino();
            self.raw(name.as_bytes(), 0o100_644, 1, ino, data);
            self
        }

        pub fn symlink(&mut self, name: &str, target: &str) -> &mut Self {
            let ino = self.next_ino();
            self.raw(name.as_bytes(), 0o120_777, 1, ino, target.as_bytes());
            self
        }

        pub fn trailer(&mut self) -> Vec<u8> {
            self.raw(b"TRAILER!!!", 0, 1, 0, b"");
            // GNU cpio pads to 512-byte blocks.
            while !self.buf.len().is_multiple_of(512) {
                self.buf.push(0);
            }
            std::mem::take(&mut self.buf)
        }
    }

    pub fn sha(data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }

    pub fn unpack(data: &[u8]) -> Result<Vec<(String, Kind)>> {
        let mut u = Unpacker::new();
        u.image(data)?;
        Ok(u.vfs
            .entries()
            .into_iter()
            .map(|e| (String::from_utf8(e.rel).unwrap(), e.kind))
            .collect())
    }

    #[test]
    fn plain_archive() {
        let img = Writer::default()
            .dir(".")
            .dir("etc")
            .file("etc/initrd-release", b"main\n")
            .symlink("init", "/nix/store/x-init")
            .raw_dev("dev/console")
            .trailer();
        assert_eq!(
            unpack(&img).unwrap(),
            vec![
                ("etc/initrd-release".into(), Kind::File(sha(b"main\n"))),
                ("init".into(), Kind::Symlink(b"/nix/store/x-init".to_vec())),
            ]
        );
    }

    impl Writer {
        fn raw_dev(&mut self, name: &str) -> &mut Self {
            let ino = self.next_ino();
            self.raw(name.as_bytes(), 0o020_600, 1, ino, b"");
            self
        }
    }

    #[test]
    fn hard_links_carry_data_on_the_last_name() {
        let mut w = Writer::default();
        w.raw(b"a", 0o100_644, 2, 7, b"");
        w.raw(b"b", 0o100_644, 2, 7, b"shared");
        let img = w.trailer();
        assert_eq!(
            unpack(&img).unwrap(),
            vec![
                ("a".into(), Kind::File(sha(b"shared"))),
                ("b".into(), Kind::File(sha(b"shared"))),
            ]
        );
    }

    #[test]
    fn links_are_forgotten_at_the_trailer() {
        let mut w = Writer::default();
        w.raw(b"a", 0o100_644, 2, 7, b"one");
        let mut img = w.trailer();
        let mut w = Writer::default();
        w.raw(b"b", 0o100_644, 2, 7, b"two");
        img.extend(w.trailer());
        assert_eq!(
            unpack(&img).unwrap(),
            vec![
                ("a".into(), Kind::File(sha(b"one"))),
                ("b".into(), Kind::File(sha(b"two"))),
            ]
        );
    }

    #[test]
    fn checksummed_format_is_verified() {
        let mut w = Writer::default();
        w.file("a", b"abc");
        let mut img = w.trailer();
        img[5] = b'2';
        // check field: 0x61 + 0x62 + 0x63 = 0x126
        img[102..110].copy_from_slice(b"00000126");
        assert!(unpack(&img).is_ok());
        img[102..110].copy_from_slice(b"00000127");
        assert!(unpack(&img).is_err());
    }

    #[test]
    fn truncated_and_garbage_input_fail() {
        let img = Writer::default().file("a", b"0123456789").trailer();
        assert!(unpack(&img[..120]).is_err());
        assert!(unpack(b"0707xyz").is_err());
        assert!(unpack(b"\x01\x02\x03\x04garbage").is_err());
    }
}
