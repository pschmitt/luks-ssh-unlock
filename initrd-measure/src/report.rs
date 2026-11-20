//! What a measurement is made of, which paths it leaves out, and how it is
//! printed.

use std::io::{self, Write};

/// The output formats. Format 1 is what the original shell implementation
/// printed: regular files only, paths verbatim. Format 2 adds symlinks and
/// mount points, escapes paths, and leaves out only exact paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    V1,
    V2,
}

/// The first line of a format 2 measurement. The checker reads it from the
/// baseline to measure the target in the same format.
pub const V2_HEADER: &[u8] = b"# initrd-measure format 2\n";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file and the SHA-256 of its content.
    File([u8; 32]),
    /// A symlink and its target.
    Symlink(Vec<u8>),
    /// A directory on another file system than the measured root (live
    /// measurements only; an image has no mounts).
    Mount,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The path relative to the measured root, without a leading slash.
    pub rel: Vec<u8>,
    pub kind: Kind,
}

/// Top-level directories that only hold runtime state; never measured.
pub const PRUNED: &[&[u8]] = &[b"dev", b"proc", b"sys", b"run", b"tmp"];

/// Paths that legitimately differ between the image and the running initrd.
/// An entry that ends with `/` covers everything below it.
const IGNORED: &[&[u8]] = &[
    // machine-id is regenerated each boot
    b"etc/machine-id",
    // empty root history file in NixOS initrd
    b"var/empty/.bash_history",
    // initrd ssh host keys
    b"etc/ssh/initrd/ssh_host_ed25519_key",
    b"etc/ssh/initrd/ssh_host_ed25519_key.pub",
    b"etc/ssh/initrd/ssh_host_rsa_key",
    b"etc/ssh/initrd/ssh_host_rsa_key.pub",
    // initrd SSH host keys (initrd secrets)
    b".initrd-secrets/etc/ssh/initrd/ssh_host_ed25519_key",
    b".initrd-secrets/etc/ssh/initrd/ssh_host_rsa_key",
    // rendered Wi-Fi supplicant config may legitimately differ across boots/build inputs
    b"etc/wpa_supplicant/",
    b".initrd-secrets/etc/wpa_supplicant/",
    // written by the systemd initrd once it found the system closure (after
    // the root file system is mounted), and rewritten each time
    b"etc/switch-root.conf",
    b"nixos-closure",
    // the root file system itself, once mounted (never descended into)
    b"sysroot",
    // systemd-resolved links it to its stub in /run at runtime
    b"etc/resolv.conf",
    // early AMD/Intel microcode blob living in concatenated initrd payload
    b"kernel/x86/microcode/AuthenticAMD.bin",
    b"kernel/x86/microcode/GenuineIntel.bin",
];

/// Initrd secret staging content differs from the unpacked target paths.
const IGNORED_PREFIX: &[u8] = b".initrd-secrets/";
/// Encrypted SOPS payloads are secret inputs, not stable runtime artifacts.
const IGNORED_INFIX: &[u8] = b".sops.";

pub fn ignored(rel: &[u8], format: Format) -> bool {
    let listed = IGNORED.iter().any(|ignore| match format {
        // The shell matched `"$ignore"*`: a plain prefix, so
        // `etc/machine-id.evil` was left out too.
        Format::V1 => rel.starts_with(ignore),
        Format::V2 if ignore.ends_with(b"/") => rel.starts_with(ignore),
        Format::V2 => rel == *ignore,
    });
    listed
        || rel.starts_with(IGNORED_PREFIX)
        || rel.windows(IGNORED_INFIX.len()).any(|w| w == IGNORED_INFIX)
}

pub fn hex(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    digest.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Bytes that would make a line ambiguous are written as `\xNN`. `field`
/// also escapes the space: a value before the path must be a single word.
fn escape(out: &mut Vec<u8>, bytes: &[u8], field: bool) {
    for &b in bytes {
        if b == b'\\' {
            out.extend_from_slice(b"\\\\");
        } else if b < 0x20 || b == 0x7f || (field && b == b' ') {
            out.extend_from_slice(format!("\\x{b:02x}").as_bytes());
        } else {
            out.push(b);
        }
    }
}

/// Render the measurement: ignored paths dropped, sorted bytewise by path,
/// one `VALUE  /path` line each.
pub fn render(mut entries: Vec<Entry>, format: Format) -> Vec<u8> {
    entries.retain(|e| !ignored(&e.rel, format));
    if format == Format::V1 {
        entries.retain(|e| matches!(e.kind, Kind::File(_)));
    }
    entries.sort_by(|a, b| a.rel.cmp(&b.rel));

    let mut out = Vec::with_capacity(entries.len() * 100);
    if format == Format::V2 {
        out.extend_from_slice(V2_HEADER);
    }
    for e in &entries {
        match &e.kind {
            Kind::File(digest) => out.extend_from_slice(hex(digest).as_bytes()),
            Kind::Symlink(target) => {
                out.extend_from_slice(b"symlink:");
                escape(&mut out, target, true);
            }
            Kind::Mount => out.extend_from_slice(b"mount"),
        }
        out.extend_from_slice(b"  /");
        match format {
            Format::V1 => out.extend_from_slice(&e.rel),
            Format::V2 => escape(&mut out, &e.rel, false),
        }
        out.push(b'\n');
    }
    out
}

pub fn write(entries: Vec<Entry>, format: Format, out: &mut impl Write) -> io::Result<()> {
    out.write_all(&render(entries, format))?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(rel: &str) -> Entry {
        Entry {
            rel: rel.as_bytes().to_vec(),
            kind: Kind::File([0xab; 32]),
        }
    }

    #[test]
    fn v1_matches_the_shell_output() {
        let entries = vec![
            file("etc/b"),
            file("etc/a"),
            Entry {
                rel: b"init".to_vec(),
                kind: Kind::Symlink(b"/nix/store/x-init".to_vec()),
            },
            file("etc/machine-id.old"),
        ];
        let text = String::from_utf8(render(entries, Format::V1)).unwrap();
        let ab = "ab".repeat(32);
        assert_eq!(text, format!("{ab}  /etc/a\n{ab}  /etc/b\n"));
    }

    #[test]
    fn v2_lists_links_and_mounts_and_escapes() {
        let entries = vec![
            file("a\nb"),
            Entry {
                rel: b"init".to_vec(),
                kind: Kind::Symlink(b"/nix/store/x y".to_vec()),
            },
            Entry {
                rel: b"mnt".to_vec(),
                kind: Kind::Mount,
            },
            file("etc/machine-id.old"),
            file("etc/machine-id"),
        ];
        let text = String::from_utf8(render(entries, Format::V2)).unwrap();
        let ab = "ab".repeat(32);
        assert_eq!(
            text,
            format!(
                "# initrd-measure format 2\n{ab}  /a\\x0ab\n{ab}  /etc/machine-id.old\nsymlink:/nix/store/x\\x20y  /init\nmount  /mnt\n"
            )
        );
    }

    #[test]
    fn ignores() {
        for format in [Format::V1, Format::V2] {
            assert!(ignored(b"etc/machine-id", format));
            assert!(ignored(b"etc/wpa_supplicant/x.conf", format));
            assert!(ignored(b".initrd-secrets/anything", format));
            assert!(ignored(b"etc/secrets/key.sops.yaml", format));
            assert!(!ignored(b"etc/passwd", format));
        }
        assert!(ignored(b"etc/machine-id2", Format::V1));
        assert!(!ignored(b"etc/machine-id2", Format::V2));
    }
}
