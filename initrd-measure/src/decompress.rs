//! Compressed segments of an initrd image.

use std::cell::Cell;
use std::io::{self, BufRead, BufReader, Read};
use std::rc::Rc;

use crate::{Error, Result};

const GZIP: &[u8] = b"\x1f\x8b";
const ZSTD: &[u8] = b"\x28\xb5\x2f\xfd";

/// Compression formats the kernel can unpack that this tool cannot.
const UNSUPPORTED: &[(&[u8], &str)] = &[
    (b"\xfd7zXZ\x00", "xz"),
    (b"\x5d\x00\x00", "lzma"),
    (b"BZh", "bzip2"),
    (b"\x89LZO", "lzo"),
    (b"\x02\x21\x4c\x18", "lz4"),
];

/// Decompress the segment at the start of `data` and hand its content to
/// `parse`, which must read it to the end. Returns the number of compressed
/// bytes it took up, so the caller continues right after it.
pub fn segment<F>(data: &[u8], parse: F) -> Result<usize>
where
    F: FnOnce(&mut dyn BufRead) -> Result<()>,
{
    if data.starts_with(GZIP) {
        // One gzip member; a following member is the next segment, as in
        // the kernel.
        let mut src = data;
        let decoder = flate2::bufread::GzDecoder::new(&mut src);
        let mut reader = BufReader::with_capacity(1 << 16, decoder);
        parse(&mut reader).map_err(|e| e.context("in gzip segment"))?;
        drop(reader);
        return Ok(data.len() - src.len());
    }
    if data.starts_with(ZSTD) {
        let pos = Rc::new(Cell::new(0));
        let mut reader = BufReader::with_capacity(
            1 << 16,
            Frames {
                data,
                pos: pos.clone(),
                frame: None,
            },
        );
        parse(&mut reader).map_err(|e| e.context("in zstd segment"))?;
        drop(reader);
        return Ok(pos.get());
    }
    for (magic, name) in UNSUPPORTED {
        if data.starts_with(magic) {
            return Err(Error::Failed(format!(
                "{name}-compressed initrd segments are not supported (use zstd or gzip)"
            )));
        }
    }
    Err(Error::Failed(
        "invalid magic at start of compressed archive".into(),
    ))
}

/// A cursor over the image that frame decoders share, so the position after
/// the last frame is known.
struct Cursor<'a> {
    data: &'a [u8],
    pos: Rc<Cell<usize>>,
}

impl Read for Cursor<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let pos = self.pos.get();
        let rest = &self.data[pos.min(self.data.len())..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.pos.set(pos + n);
        Ok(n)
    }
}

type FrameDecoder<'a> =
    ruzstd::decoding::StreamingDecoder<Cursor<'a>, ruzstd::decoding::FrameDecoder>;

/// Consecutive zstd frames as one stream, as `zstd -d` and the kernel read
/// them. Skippable frames are skipped; anything else ends the segment.
struct Frames<'a> {
    data: &'a [u8],
    pos: Rc<Cell<usize>>,
    frame: Option<FrameDecoder<'a>>,
}

impl Frames<'_> {
    /// Start the next frame; false at the end of the segment.
    fn next_frame(&mut self) -> io::Result<bool> {
        loop {
            let pos = self.pos.get();
            let rest = &self.data[pos.min(self.data.len())..];
            if rest.starts_with(ZSTD) {
                let cursor = Cursor {
                    data: self.data,
                    pos: self.pos.clone(),
                };
                let frame = ruzstd::decoding::StreamingDecoder::new(cursor)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
                self.frame = Some(frame);
                return Ok(true);
            }
            // Skippable frames: magic 0x184D2A5?, little endian, then a size.
            if rest.len() >= 8 && rest[1..4] == [0x2a, 0x4d, 0x18] && rest[0] & 0xf0 == 0x50 {
                let size = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
                self.pos.set(pos + 8 + size);
                continue;
            }
            return Ok(false);
        }
    }
}

impl Read for Frames<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(frame) = &mut self.frame {
                let n = frame.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                self.frame = None;
            }
            if !self.next_frame()? {
                return Ok(0);
            }
        }
    }
}
