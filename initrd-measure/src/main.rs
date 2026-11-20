//! `initrd-measure`: hash every file of an initrd image, or of the running
//! initrd, so the two can be compared.
//!
//! `initrd-checksum` uploads this static binary to the target and runs it
//! there, so the measurement does not depend on the target's shell, cpio,
//! decompressors or coreutils. It unpacks images in memory the way the
//! kernel does, and prints one `VALUE  /path` line per entry.

mod cpio;
mod decompress;
mod live;
mod report;
mod vfs;

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use report::Format;

const USAGE: &str = "\
Usage: initrd-measure [--format 1|2] live-root [--root DIR]
       initrd-measure [--format 1|2] image [PATH]
       initrd-measure [--format 1|2] auto [PATH]
       initrd-measure --version

  live-root  Hash the running system's / (needs root), or DIR.
  image      Hash the files an initrd image unpacks to (default:
             /run/current-system/initrd, else /run/booted-system/initrd).
  auto       live-root inside an initrd (/etc/initrd-release exists),
             image otherwise.

  --format 1 Regular files only, paths verbatim (the original format).
  --format 2 Also symlinks and mount points, with escaped paths (default).

Prints `SHA256  /path` lines sorted by path; exits 1 if anything cannot be
measured, 2 on a usage error.";

const DEFAULT_IMAGE: &str = "/run/current-system/initrd";
const BOOTED_IMAGE: &str = "/run/booted-system/initrd";

#[derive(Debug)]
pub enum Error {
    Usage(String),
    Failed(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io(what: &str, e: io::Error) -> Self {
        Error::Failed(format!("{what}: {e}"))
    }

    pub fn context(self, what: &str) -> Self {
        match self {
            Error::Failed(msg) => Error::Failed(format!("{msg} ({what})")),
            usage => usage,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(m) | Error::Failed(m) => f.write_str(m),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    LiveRoot(Option<PathBuf>),
    Image(Option<PathBuf>),
    Auto(Option<PathBuf>),
    Version,
}

fn parse(args: Vec<OsString>) -> Result<(Format, Mode)> {
    let usage = |m: &str| Error::Usage(m.to_string());
    let mut format = Format::V2;
    let mut args = args.into_iter();
    let mut mode = None;
    let mut path = None;
    let mut root = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--format") => {
                format = match args.next().as_ref().and_then(|v| v.to_str()) {
                    Some("1") => Format::V1,
                    Some("2") => Format::V2,
                    _ => return Err(usage("--format takes 1 or 2")),
                };
            }
            Some("--root") => {
                root = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| usage("--root takes a directory"))?,
                ));
            }
            Some("--version" | "-V") => return Ok((format, Mode::Version)),
            Some("--help" | "-h") => return Err(usage("")),
            Some(m @ ("live-root" | "image" | "auto")) if mode.is_none() => {
                mode = Some(m.to_string());
            }
            Some(s) if s.starts_with('-') => return Err(usage(&format!("unknown option {s}"))),
            _ if mode.is_some() && path.is_none() => path = Some(PathBuf::from(arg)),
            _ => {
                return Err(usage(&format!(
                    "unexpected argument {}",
                    arg.to_string_lossy()
                )));
            }
        }
    }
    let mode = match mode.as_deref() {
        Some("live-root") if path.is_none() => Mode::LiveRoot(root),
        Some("image") if root.is_none() => Mode::Image(path),
        Some("auto") if root.is_none() => Mode::Auto(path),
        Some(_) => {
            return Err(usage(
                "--root goes with live-root, a PATH with image or auto",
            ));
        }
        None => return Err(usage("missing mode")),
    };
    Ok((format, mode))
}

fn warn(msg: &str) {
    eprintln!("WRN {msg}");
}

/// The image to hash: the given path, else the current system's, falling
/// back to the booted system's (nixos-install has no current system yet).
fn image_path(path: Option<PathBuf>) -> PathBuf {
    if let Some(path) = path {
        return path;
    }
    let default = Path::new(DEFAULT_IMAGE);
    if !default.exists() && Path::new(BOOTED_IMAGE).exists() {
        warn(&format!(
            "initrd not found at {DEFAULT_IMAGE}, falling back to {BOOTED_IMAGE}"
        ));
        return BOOTED_IMAGE.into();
    }
    default.into()
}

fn measure_image(path: &Path) -> Result<Vec<report::Entry>> {
    let data =
        std::fs::read(path).map_err(|e| Error::io(&format!("initrd {}", path.display()), e))?;
    let mut unpacker = cpio::Unpacker::new();
    unpacker
        .image(&data)
        .map_err(|e| e.context(&format!("unpacking {}", path.display())))?;
    Ok(unpacker.vfs.entries())
}

fn measure_live(root: Option<PathBuf>) -> Result<Vec<report::Entry>> {
    let root = match root {
        Some(root) => root,
        None => {
            // SAFETY: geteuid has no preconditions and cannot fail.
            if unsafe { libc::geteuid() } != 0 {
                return Err(Error::Failed(
                    "live root measurement needs root privileges".into(),
                ));
            }
            PathBuf::from("/")
        }
    };
    live::measure(&root)
}

fn run(args: Vec<OsString>) -> Result<()> {
    let (format, mode) = parse(args)?;
    let entries = match mode {
        Mode::Version => {
            println!("initrd-measure {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Mode::LiveRoot(root) => measure_live(root)?,
        Mode::Image(path) => measure_image(&image_path(path))?,
        Mode::Auto(path) => {
            if Path::new("/etc/initrd-release").exists() {
                measure_live(None)?
            } else {
                measure_image(&image_path(path))?
            }
        }
    };
    report::write(entries, format, &mut io::stdout().lock())
        .map_err(|e| Error::io("cannot write the measurement", e))
}

fn main() -> ExitCode {
    match run(std::env::args_os().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(msg)) => {
            if !msg.is_empty() {
                eprintln!("ERR {msg}");
            }
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
        Err(Error::Failed(msg)) => {
            eprintln!("ERR {msg}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpio::tests::Writer;
    use std::io::Write;

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_modes() {
        assert_eq!(
            parse(args(&["--format", "1", "image", "/x"])).unwrap(),
            (Format::V1, Mode::Image(Some("/x".into())))
        );
        assert_eq!(
            parse(args(&["auto"])).unwrap(),
            (Format::V2, Mode::Auto(None))
        );
        assert_eq!(
            parse(args(&["live-root", "--root", "/r"])).unwrap(),
            (Format::V2, Mode::LiveRoot(Some("/r".into())))
        );
        for bad in [
            &["--format", "3", "auto"][..],
            &["image", "a", "b"],
            &["live-root", "x"],
            &["image", "--root", "/r"],
            &[],
            &["--bogus", "auto"],
        ] {
            assert!(matches!(parse(args(bad)), Err(Error::Usage(_))), "{bad:?}");
        }
    }

    fn zstd(data: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// The layout NixOS builds: an uncompressed early archive (microcode),
    /// padding, then the compressed main archive, and possibly appended
    /// secrets after that.
    fn nixos_like(compress: fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
        let mut img = Writer::default()
            .dir("kernel")
            .file("kernel/x86/microcode/GenuineIntel.bin", b"ucode")
            .trailer();
        img.extend(std::iter::repeat_n(0u8, 512));
        let main = Writer::default()
            .dir("etc")
            .file("etc/initrd-release", b"main\n")
            .file("etc/machine-id", b"build\n")
            .symlink("init", "/nix/store/x-init")
            .trailer();
        if compress(b"").starts_with(b"\x28\xb5\x2f\xfd") {
            // Consecutive zstd frames are one stream: split mid-entry.
            let (a, b) = main.split_at(100);
            img.extend(compress(a));
            img.extend(compress(b));
        } else {
            // Each gzip member is a segment of its own, as in the kernel.
            img.extend(compress(&main));
        }
        let secrets = Writer::default()
            .file("etc/ssh/initrd/ssh_host_ed25519_key", b"secret")
            .trailer();
        img.extend(compress(&secrets));
        img
    }

    #[test]
    fn nixos_layouts_measure_the_same_in_both_compressions() {
        let mut outputs = Vec::new();
        for compress in [zstd as fn(&[u8]) -> Vec<u8>, gzip] {
            let img = nixos_like(compress);
            let mut u = cpio::Unpacker::new();
            u.image(&img).unwrap();
            outputs.push(report::render(u.vfs.entries(), Format::V2));
        }
        assert_eq!(outputs[0], outputs[1]);
        let text = String::from_utf8(outputs.remove(0)).unwrap();
        let release = report::hex(&cpio::tests::sha(b"main\n"));
        assert_eq!(
            text,
            format!(
                "# initrd-measure format 2\n{release}  /etc/initrd-release\nsymlink:/nix/store/x-init  /init\n"
            )
        );
    }

    #[test]
    fn junk_after_a_segment_fails() {
        let mut img = nixos_like(zstd);
        img.extend_from_slice(b"\x01junk");
        assert!(cpio::Unpacker::new().image(&img).is_err());
        let mut img = Writer::default().file("a", b"x").trailer();
        img.extend(zstd(b"not cpio"));
        assert!(cpio::Unpacker::new().image(&img).is_err());
    }

    #[test]
    fn an_image_and_its_unpacked_tree_measure_the_same() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::write(root.join("etc/initrd-release"), b"main\n").unwrap();
        fs::write(root.join("usr/lib/libc.so"), b"\x7fELF").unwrap();
        symlink("usr/lib", root.join("lib")).unwrap();
        fs::write(root.join("etc/machine-id"), b"runtime\n").unwrap();

        let img = Writer::default()
            .dir("etc")
            .dir("usr")
            .dir("usr/lib")
            .file("etc/initrd-release", b"main\n")
            .file("usr/lib/libc.so", b"\x7fELF")
            .symlink("lib", "usr/lib")
            .trailer();
        let mut u = cpio::Unpacker::new();
        u.image(&zstd(&img)).unwrap();
        for format in [Format::V1, Format::V2] {
            assert_eq!(
                report::render(u.vfs.entries(), format),
                report::render(live::measure(root).unwrap(), format),
            );
        }
    }
}
