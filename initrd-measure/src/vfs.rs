//! The file tree an initrd image unpacks to, kept in memory.
//!
//! It follows what the kernel's initramfs unpacker does to its rootfs, so an
//! image measures the same as the initrd that booted from it: later entries
//! replace earlier ones, a file written over an existing file of the same
//! path reuses its inode, hard links share content, and symlinks in parent
//! components are followed. Nothing is written to disk, so an archive cannot
//! reach outside the tree, whatever names or link targets it holds.

use std::collections::{BTreeMap, VecDeque};

use crate::report::{Entry, Kind, PRUNED};

type Dir = BTreeMap<Vec<u8>, Node>;

#[derive(Debug)]
enum Node {
    Dir(Dir),
    /// An index into `Vfs::inodes`.
    File(usize),
    Symlink(Vec<u8>),
    /// Device nodes, FIFOs and sockets: never measured.
    Other,
}

/// What a new entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    Dir,
    File,
    Symlink,
    Other,
}

impl Node {
    fn type_(&self) -> Type {
        match self {
            Node::Dir(_) => Type::Dir,
            Node::File(_) => Type::File,
            Node::Symlink(_) => Type::Symlink,
            Node::Other => Type::Other,
        }
    }
}

/// A resolved location: the canonical parent directory and the final name.
pub struct Location {
    parent: Vec<Vec<u8>>,
    name: Vec<u8>,
}

#[derive(Default)]
pub struct Vfs {
    root: Dir,
    /// The content digest of each regular file inode.
    inodes: Vec<[u8; 32]>,
}

/// Symlinks followed while resolving one path, as Linux allows.
const MAX_LINKS: usize = 40;

fn components(path: &[u8]) -> impl DoubleEndedIterator<Item = &[u8]> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
}

impl Vfs {
    pub fn new() -> Self {
        Self::default()
    }

    fn dir(&self, path: &[Vec<u8>]) -> Option<&Dir> {
        let mut dir = &self.root;
        for c in path {
            match dir.get(c)? {
                Node::Dir(d) => dir = d,
                _ => return None,
            }
        }
        Some(dir)
    }

    fn dir_mut(&mut self, path: &[Vec<u8>]) -> Option<&mut Dir> {
        let mut dir = &mut self.root;
        for c in path {
            match dir.get_mut(c)? {
                Node::Dir(d) => dir = d,
                _ => return None,
            }
        }
        Some(dir)
    }

    /// Resolve an archive name to its parent directory and final component,
    /// following symlinks in the parent components (never in the final one),
    /// relative to the tree's root like a chroot. Missing parents are
    /// created, as `cpio -d` does. None if a parent is not a directory or
    /// the name is the root itself.
    pub fn locate(&mut self, name: &[u8]) -> Option<Location> {
        let mut todo: VecDeque<Vec<u8>> = components(name).map(<[u8]>::to_vec).collect();
        let last = todo.pop_back()?;
        if last == b".." {
            return None;
        }
        let mut cur: Vec<Vec<u8>> = Vec::new();
        let mut links = 0;
        while let Some(c) = todo.pop_front() {
            if c == b".." {
                cur.pop();
                continue;
            }
            let dir = self.dir_mut(&cur)?;
            match dir.get(&c) {
                None => {
                    dir.insert(c.clone(), Node::Dir(Dir::new()));
                    cur.push(c);
                }
                Some(Node::Dir(_)) => cur.push(c),
                Some(Node::Symlink(target)) => {
                    links += 1;
                    if links > MAX_LINKS {
                        return None;
                    }
                    if target.starts_with(b"/") {
                        cur.clear();
                    }
                    let target = target.clone();
                    for t in components(&target).rev() {
                        todo.push_front(t.to_vec());
                    }
                }
                Some(_) => return None,
            }
        }
        Some(Location {
            parent: cur,
            name: last,
        })
    }

    /// The kernel's `clean_path`: an existing node of another type than
    /// `new` is removed first (a directory only when it is empty); `None`
    /// (mode 0) removes any node. False when the path stays blocked by a
    /// non-empty directory.
    fn clean(&mut self, loc: &Location, new: Option<Type>) -> bool {
        let Some(dir) = self.dir_mut(&loc.parent) else {
            return false;
        };
        match dir.get(&loc.name) {
            None => true,
            Some(node) if Some(node.type_()) == new => true,
            Some(Node::Dir(d)) if !d.is_empty() => false,
            Some(_) => {
                dir.remove(&loc.name);
                true
            }
        }
    }

    pub fn mkdir(&mut self, name: &[u8]) {
        let Some(loc) = self.locate(name) else { return };
        if self.clean(&loc, Some(Type::Dir))
            && let Some(dir) = self.dir_mut(&loc.parent)
        {
            dir.entry(loc.name).or_insert_with(|| Node::Dir(Dir::new()));
        }
    }

    pub fn symlink(&mut self, name: &[u8], target: &[u8]) {
        let Some(loc) = self.locate(name) else { return };
        if self.clean(&loc, None)
            && let Some(dir) = self.dir_mut(&loc.parent)
        {
            dir.insert(loc.name, Node::Symlink(target.to_vec()));
        }
    }

    pub fn special(&mut self, name: &[u8]) {
        let Some(loc) = self.locate(name) else { return };
        if self.clean(&loc, Some(Type::Other))
            && let Some(dir) = self.dir_mut(&loc.parent)
        {
            dir.entry(loc.name).or_insert(Node::Other);
        }
    }

    /// A regular file with the given content digest. An existing file at the
    /// path is truncated and rewritten in place (O_TRUNC), so its hard links
    /// see the new content too.
    pub fn file(&mut self, name: &[u8], digest: [u8; 32]) {
        let Some(loc) = self.locate(name) else { return };
        if !self.clean(&loc, Some(Type::File)) {
            return;
        }
        let Some(dir) = self.dir(&loc.parent) else {
            return;
        };
        if let Some(&Node::File(inode)) = dir.get(&loc.name) {
            self.inodes[inode] = digest;
            return;
        }
        let inode = self.inodes.len();
        self.inodes.push(digest);
        if let Some(dir) = self.dir_mut(&loc.parent) {
            dir.insert(loc.name, Node::File(inode));
        }
    }

    fn lookup_file(&mut self, name: &[u8]) -> Option<usize> {
        let loc = self.locate(name)?;
        match self.dir(&loc.parent)?.get(&loc.name)? {
            Node::File(inode) => Some(*inode),
            _ => None,
        }
    }

    /// A hard link from `name` to the file at `old` (an earlier name with
    /// the same inode in the archive). The kernel removes whatever is at
    /// `name` first, then links. When the entry carries data (`Some`), it
    /// is written into the shared inode; a zero-size link keeps the
    /// content. False when the link fails (`old` is no regular file any
    /// more): the kernel then drops the entry and its data.
    pub fn link(&mut self, name: &[u8], old: &[u8], digest: Option<[u8; 32]>) -> bool {
        let Some(loc) = self.locate(name) else {
            return false;
        };
        // clean_path(collected, mode), then clean_path(collected, 0).
        if !self.clean(&loc, None) {
            return false;
        }
        let Some(inode) = self.lookup_file(old) else {
            return false;
        };
        let Some(dir) = self.dir_mut(&loc.parent) else {
            return false;
        };
        dir.insert(loc.name, Node::File(inode));
        if let Some(digest) = digest {
            self.inodes[inode] = digest;
        }
        true
    }

    /// Every measured node, top-level runtime directories left out.
    pub fn entries(&self) -> Vec<Entry> {
        fn walk(vfs: &Vfs, dir: &Dir, prefix: &[u8], out: &mut Vec<Entry>) {
            for (name, node) in dir {
                if prefix.is_empty() && PRUNED.contains(&name.as_slice()) {
                    continue;
                }
                let mut rel = prefix.to_vec();
                if !rel.is_empty() {
                    rel.push(b'/');
                }
                rel.extend_from_slice(name);
                match node {
                    Node::Dir(d) => walk(vfs, d, &rel, out),
                    Node::File(inode) => out.push(Entry {
                        rel,
                        kind: Kind::File(vfs.inodes[*inode]),
                    }),
                    Node::Symlink(target) => out.push(Entry {
                        rel,
                        kind: Kind::Symlink(target.clone()),
                    }),
                    Node::Other => {}
                }
            }
        }
        let mut out = Vec::new();
        walk(self, &self.root, b"", &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: u8) -> [u8; 32] {
        [n; 32]
    }

    fn listing(vfs: &Vfs) -> Vec<(String, Kind)> {
        vfs.entries()
            .into_iter()
            .map(|e| (String::from_utf8(e.rel).unwrap(), e.kind))
            .collect()
    }

    #[test]
    fn parents_through_symlinks_stay_inside() {
        let mut v = Vfs::new();
        v.mkdir(b"usr");
        v.symlink(b"lib", b"usr");
        v.symlink(b"escape", b"/../../..");
        v.file(b"lib/a", d(1));
        v.file(b"escape/etc/passwd", d(2));
        v.file(b"./x/../y", d(3));
        assert_eq!(
            listing(&v),
            vec![
                ("escape".into(), Kind::Symlink(b"/../../..".to_vec())),
                ("etc/passwd".into(), Kind::File(d(2))),
                ("lib".into(), Kind::Symlink(b"usr".to_vec())),
                ("usr/a".into(), Kind::File(d(1))),
                ("y".into(), Kind::File(d(3))),
            ]
        );
    }

    #[test]
    fn later_entries_replace_earlier_ones() {
        let mut v = Vfs::new();
        v.file(b"a", d(1));
        v.symlink(b"a", b"t");
        v.file(b"b", d(1));
        v.file(b"b", d(2));
        v.mkdir(b"c");
        v.file(b"c/x", d(1));
        // A non-empty directory blocks a file.
        v.file(b"c", d(9));
        v.symlink(b"loop", b"loop");
        v.file(b"loop/x", d(1));
        assert_eq!(
            listing(&v),
            vec![
                ("a".into(), Kind::Symlink(b"t".to_vec())),
                ("b".into(), Kind::File(d(2))),
                ("c/x".into(), Kind::File(d(1))),
                ("loop".into(), Kind::Symlink(b"loop".to_vec())),
            ]
        );
    }

    #[test]
    fn hard_links_share_content() {
        let mut v = Vfs::new();
        v.file(b"a", d(0));
        assert!(v.link(b"b", b"a", Some(d(5))));
        assert!(!v.link(b"c", b"missing", None));
        // Rewriting one name rewrites the inode.
        v.file(b"a", d(7));
        assert_eq!(
            listing(&v),
            vec![
                ("a".into(), Kind::File(d(7))),
                ("b".into(), Kind::File(d(7)))
            ]
        );
    }

    #[test]
    fn runtime_directories_are_pruned_at_the_top_only() {
        let mut v = Vfs::new();
        v.file(b"run", d(1));
        v.file(b"tmp/x", d(1));
        v.file(b"etc/run/x", d(1));
        assert_eq!(listing(&v), vec![("etc/run/x".into(), Kind::File(d(1)))]);
    }
}
