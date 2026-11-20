//! Measuring a mounted tree, normally the running initrd's `/`.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::report::{Entry, Kind, PRUNED};
use crate::{Error, Result};

/// Every regular file, symlink and mount point below `root`, as `find -xdev`
/// sees it: directories on other file systems are not descended into, and
/// the top-level runtime directories are left out.
pub fn measure(root: &Path) -> Result<Vec<Entry>> {
    let meta = fs::symlink_metadata(root).map_err(|e| Error::io(&root.display().to_string(), e))?;
    let mut out = Vec::new();
    walk(root, &[], meta.dev(), &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, rel: &[u8], dev: u64, out: &mut Vec<Entry>) -> Result<()> {
    let at = |path: &Path| path.display().to_string();
    let entries = fs::read_dir(dir).map_err(|e| Error::io(&at(dir), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(&at(dir), e))?;
        let name = entry.file_name();
        let name = name.as_bytes();
        if rel.is_empty() && PRUNED.contains(&name) {
            continue;
        }
        let path = dir.join(OsStr::from_bytes(name));
        let mut child = rel.to_vec();
        if !child.is_empty() {
            child.push(b'/');
        }
        child.extend_from_slice(name);

        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            // Gone since the directory was read: nothing left to measure.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::io(&at(&path), e)),
        };
        let ft = meta.file_type();
        if ft.is_file() {
            let digest = hash_file(&path)
                .map_err(|e| Error::io(&format!("failed to hash {} (need root?)", at(&path)), e))?;
            out.push(Entry {
                rel: child,
                kind: Kind::File(digest),
            });
        } else if ft.is_symlink() {
            let target = fs::read_link(&path).map_err(|e| Error::io(&at(&path), e))?;
            out.push(Entry {
                rel: child,
                kind: Kind::Symlink(target.as_os_str().as_bytes().to_vec()),
            });
        } else if ft.is_dir() {
            if meta.dev() == dev {
                walk(&path, &child, dev, out)?;
            } else {
                out.push(Entry {
                    rel: child,
                    kind: Kind::Mount,
                });
            }
        }
    }
    Ok(())
}

pub fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    // Never follow a symlink swapped in after the lstat; never block on a
    // FIFO swapped in either.
    let mut file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(io::Error::other("no longer a regular file"));
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 18];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpio::tests::sha;
    use std::os::unix::fs::symlink;

    #[test]
    fn walks_like_find() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("etc/run")).unwrap();
        fs::create_dir_all(root.join("run")).unwrap();
        fs::write(root.join("etc/run/x"), b"x").unwrap();
        fs::write(root.join("run/y"), b"y").unwrap();
        fs::write(root.join("tmp"), b"a file named tmp").unwrap();
        symlink("/nix/store/x-init", root.join("init")).unwrap();
        let mut got: Vec<_> = measure(root)
            .unwrap()
            .into_iter()
            .map(|e| (String::from_utf8(e.rel).unwrap(), e.kind))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                ("etc/run/x".into(), Kind::File(sha(b"x"))),
                ("init".into(), Kind::Symlink(b"/nix/store/x-init".to_vec())),
            ]
        );
    }
}
