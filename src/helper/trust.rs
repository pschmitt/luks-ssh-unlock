//! Trusted, race-free access to the root-owned configuration directory.
//!
//! The helper runs with privileges and must never act on configuration an
//! unprivileged user could have written or redirected. The directory is
//! fixed (`/etc/luks-ssh-unlock`), there is no environment or path override,
//! and every lookup is resolved here, component by component, relative to
//! file descriptors:
//!
//! * Every directory that is traversed must be owned by root and either not
//!   writable by group/other or sticky (a sticky directory owned by root lets
//!   nobody else replace root's entries, which is what `/nix/store` is).
//! * Every entry that is traversed (directory, symbolic link, file) must be
//!   owned by root. A symbolic link is followed only when root owns it and
//!   lives in a directory that passed the check above, so nobody else could
//!   have created or replaced it.
//! * This is exactly the NixOS layout: `/etc/luks-ssh-unlock/NAME.env` is a
//!   root-owned link to `/etc/static/...`, which is a root-owned link into
//!   `/nix/store`. A blanket `O_NOFOLLOW` would reject legitimate targets;
//!   instead each link is read with `readlinkat` and resolved by this walker,
//!   and a link into any directory a user could write is refused.
//! * Link targets may be absolute (resolution restarts at `/`, itself
//!   checked) or relative to the link's directory; `..` is refused, and the
//!   number of links is bounded.
//! * Each component is opened with `O_NOFOLLOW` relative to the previous
//!   one's descriptor and compared (device and inode) with the entry that was
//!   inspected, and the final file is validated with `fstat` on the descriptor
//!   that is then read. Nothing is checked by path and used by path.
//! * The final file must be a regular file owned by root, without group/other
//!   write or setuid/setgid/sticky bits, of bounded size. Hard links are not a
//!   concern: such a file cannot be modified by anyone else.

use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use zeroize::Zeroize;

use super::request::Target;

/// The only directory the helper reads configuration from.
pub const SYSTEM_DIR: &str = "/etc/luks-ssh-unlock";
/// Upper bound for a configuration file.
pub const MAX_CONFIG_BYTES: u64 = 64 * 1024;
/// Upper bound for the number of symbolic links followed in one lookup.
const MAX_LINKS: u32 = 16;
/// Upper bound for the number of path components processed in one lookup.
const MAX_STEPS: u32 = 256;
/// Upper bound for the number of targets listed.
const MAX_LISTED: usize = 1024;

#[derive(Debug)]
pub enum TrustError {
    NotFound,
    /// Present, but not something a privileged process may use.
    Untrusted(String),
    Invalid(String),
    Io(io::Error),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("not found"),
            Self::Untrusted(why) => write!(f, "untrusted: {why}"),
            Self::Invalid(why) => write!(f, "invalid: {why}"),
            Self::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for TrustError {}

impl TrustError {
    /// A fixed word for the kind of problem, safe to hand to a caller: unlike
    /// the `Display` text it can never carry a name or a value.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Untrusted(_) => "untrusted",
            Self::Invalid(_) => "invalid",
            Self::Io(_) => "io",
        }
    }
}

impl From<io::Error> for TrustError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::NotFound {
            TrustError::NotFound
        } else if e.raw_os_error() == Some(libc::ELOOP) {
            TrustError::Untrusted("symbolic link where none is allowed".into())
        } else {
            TrustError::Io(e)
        }
    }
}

/// The reason given when an entry was replaced between inspecting and opening
/// it. Harmless and expected during an atomic replacement (NixOS switches
/// `/etc` links that way), so lookups are retried; it is still an error, never
/// an acceptance, if it persists.
const CHANGED: &str = "entry changed during lookup";
/// Attempts for a lookup that keeps being overtaken by replacements.
const ATTEMPTS: u32 = 16;

fn retry<T>(mut attempt: impl FnMut() -> Result<T, TrustError>) -> Result<T, TrustError> {
    let mut last = None;
    for _ in 0..ATTEMPTS {
        match attempt() {
            Err(TrustError::Untrusted(why)) if why == CHANGED => {
                last = Some(TrustError::Untrusted(why));
            }
            other => return other,
        }
    }
    Err(last.unwrap_or_else(|| untrusted(CHANGED)))
}

fn untrusted(why: &str) -> TrustError {
    TrustError::Untrusted(why.into())
}

fn invalid(why: &str) -> TrustError {
    TrustError::Invalid(why.into())
}

/// What a lookup is expected to end in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Want {
    Dir,
    File,
    /// Whatever the path ends in, a regular file or a directory.
    Any,
}

enum Found {
    Dir(OwnedFd),
    File(File),
}

fn cstring(name: &OsStr) -> Result<CString, TrustError> {
    CString::new(name.as_bytes()).map_err(|_| invalid("path contains a NUL byte"))
}

fn fstat(fd: &impl AsRawFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn fstatat_nofollow(dir: &OwnedFd, name: &CString) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn openat(dir: &OwnedFd, name: &CString, flags: libc::c_int) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn readlinkat(dir: &OwnedFd, name: &CString) -> Result<Vec<u8>, TrustError> {
    let mut buf = vec![0u8; 4096];
    let n = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let n = n as usize;
    if n == 0 || n >= buf.len() {
        return Err(invalid("symbolic link target is empty or too long"));
    }
    buf.truncate(n);
    Ok(buf)
}

fn same_inode(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_dev == b.st_dev && a.st_ino == b.st_ino
}

/// Production never trusts anyone but root and the configured user.
#[cfg(not(test))]
fn unmapped_root(_uid: u32) -> bool {
    false
}

/// Test builds only. In a user namespace that does not map root (the Nix
/// build sandbox: `/`, `/tmp` and the store belong to root outside), root's
/// files show up under the kernel's overflow uid. Without this the
/// tests, which trust only root and the running user, would refuse every path
/// starting at `/`. Where root is mapped (a normal shell, a root-run test)
/// nothing is added, so a file given to the overflow uid stays foreign.
#[cfg(test)]
fn unmapped_root(uid: u32) -> bool {
    use std::sync::OnceLock;

    static OVERFLOW: OnceLock<Option<u32>> = OnceLock::new();
    let overflow = OVERFLOW.get_or_init(|| {
        let map = std::fs::read_to_string("/proc/self/uid_map").ok()?;
        let maps_root = map.lines().any(|line| {
            let mut f = line.split_whitespace().map(|n| n.parse::<u64>());
            match (f.next(), f.next(), f.next()) {
                (Some(Ok(inside)), Some(Ok(_)), Some(Ok(count))) => inside == 0 && count > 0,
                _ => false,
            }
        });
        if maps_root {
            return None;
        }
        std::fs::read_to_string("/proc/sys/kernel/overflowuid")
            .ok()?
            .trim()
            .parse()
            .ok()
    });
    *overflow == Some(uid)
}

/// The walker and its notion of "trusted". `uid` is root in production; the
/// tests substitute the running user so they need no privileges.
struct Walker {
    uid: u32,
}

impl Walker {
    fn owner_ok(&self, st: &libc::stat) -> bool {
        st.st_uid == 0 || st.st_uid == self.uid || unmapped_root(st.st_uid)
    }

    fn dir_ok(&self, st: &libc::stat) -> Result<(), TrustError> {
        if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(invalid("not a directory"));
        }
        if !self.owner_ok(st) {
            return Err(untrusted("directory is owned by another user"));
        }
        if st.st_mode & 0o022 != 0 && st.st_mode & libc::S_ISVTX == 0 {
            return Err(untrusted("directory is writable by group or others"));
        }
        Ok(())
    }

    fn file_ok(&self, st: &libc::stat) -> Result<(), TrustError> {
        if st.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(invalid("not a regular file"));
        }
        if !self.owner_ok(st) {
            return Err(untrusted("file is owned by another user"));
        }
        if st.st_mode & 0o022 != 0 {
            return Err(untrusted("file is writable by group or others"));
        }
        if st.st_mode & (libc::S_ISUID | libc::S_ISGID | libc::S_ISVTX) != 0 {
            return Err(untrusted("file has setuid, setgid or sticky bits"));
        }
        Ok(())
    }

    fn open_root(&self) -> Result<OwnedFd, TrustError> {
        let root = CString::new("/").unwrap();
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        self.dir_ok(&fstat(&fd)?)?;
        Ok(fd)
    }

    /// Resolve `path` (relative to `start`, which must already be trusted;
    /// absolute link targets restart at `root`) and open the result.
    fn walk(
        &self,
        root: &OwnedFd,
        start: &OwnedFd,
        path: &OsStr,
        want: Want,
    ) -> Result<Found, TrustError> {
        let mut cur = start.try_clone()?;
        let mut stack: Vec<Vec<u8>> = components(path.as_bytes());
        let mut links = 0u32;
        let mut steps = 0u32;
        let mut result_dir: Option<OwnedFd> = None;
        while let Some(component) = stack.pop() {
            steps += 1;
            if steps > MAX_STEPS {
                return Err(invalid("path is too deep"));
            }
            let is_last = stack.is_empty();
            if component == b".." {
                return Err(untrusted("parent references are not followed"));
            }
            let name = cstring(OsStr::from_bytes(&component))?;
            let st = fstatat_nofollow(&cur, &name)?;
            if !self.owner_ok(&st) {
                return Err(untrusted("entry is owned by another user"));
            }
            match st.st_mode & libc::S_IFMT {
                libc::S_IFLNK => {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(invalid("too many symbolic links"));
                    }
                    let target = readlinkat(&cur, &name)?;
                    if target.starts_with(b"/") {
                        cur = root.try_clone()?;
                    }
                    stack.extend(components(&target));
                }
                libc::S_IFDIR => {
                    let fd = openat(&cur, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
                    let opened = fstat(&fd)?;
                    if !same_inode(&st, &opened) {
                        return Err(untrusted(CHANGED));
                    }
                    self.dir_ok(&opened)?;
                    if is_last {
                        result_dir = Some(fd);
                        break;
                    }
                    cur = fd;
                }
                libc::S_IFREG => {
                    if !is_last {
                        return Err(invalid("a file where a directory is expected"));
                    }
                    if want == Want::Dir {
                        return Err(invalid("a file where a directory is expected"));
                    }
                    // O_NONBLOCK: never hang on a FIFO that slipped through.
                    let fd = openat(&cur, &name, libc::O_RDONLY | libc::O_NONBLOCK)?;
                    let opened = fstat(&fd)?;
                    if !same_inode(&st, &opened) {
                        return Err(untrusted(CHANGED));
                    }
                    self.file_ok(&opened)?;
                    return Ok(Found::File(File::from(fd)));
                }
                _ => return Err(invalid("neither a file, directory nor link")),
            }
        }
        match (result_dir, want) {
            (Some(fd), Want::Dir | Want::Any) => Ok(Found::Dir(fd)),
            (Some(_), Want::File) => Err(invalid("a directory where a file is expected")),
            // The path resolved to `start` itself (empty or only dots).
            (None, Want::Dir | Want::Any) => Ok(Found::Dir(cur)),
            (None, Want::File) => Err(invalid("no file named")),
        }
    }
}

/// Split into components in reverse, for use as a stack. `.` and empty
/// components vanish; `..` is kept so the walker refuses it.
fn components(path: &[u8]) -> Vec<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = path
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
        .collect();
    parts.reverse();
    parts
}

/// Variables that would change how a child process is loaded or how it
/// finds its helpers. No legitimate target configuration sets them, and the
/// helper refuses to accept any file that does.
const FORBIDDEN_KEYS: &[&str] = &[
    "PATH",
    "IFS",
    "ENV",
    "BASH_ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "HOME",
    "SSH_AUTH_SOCK",
    "SSH_ASKPASS",
    "HOSTALIASES",
    "LOCALDOMAIN",
    "RES_OPTIONS",
];
const FORBIDDEN_PREFIXES: &[&str] = &[
    "LD_", "DYLD_", "GCONV_", "MALLOC_", "PYTHON", "PERL", "RUBY",
];

/// Settings that name a file or directory. Each must be absolute when set.
const PATH_KEYS: &[&str] = &[
    "SSH_KEY",
    "SSH_JUMPHOST_KEY",
    "SSH_KNOWN_HOSTS_FILE",
    "SSH_INITRD_KNOWN_HOSTS_FILE",
    "LUKS_PASSPHRASE_FILE",
    "INITRD_CHECKSUM_DIR",
    "INITRD_CHECKSUM_FILE",
    "INITRD_CHECKSUM_SCRIPT",
    "INITRD_CHECKSUM_BUSYBOX_DIR",
    "EVENTS_FILE",
    "TMPDIR",
];

fn validate_vars(vars: &BTreeMap<String, String>) -> Result<(), TrustError> {
    for (key, value) in vars {
        if FORBIDDEN_KEYS.contains(&key.as_str())
            || FORBIDDEN_PREFIXES.iter().any(|p| key.starts_with(p))
        {
            return Err(TrustError::Untrusted(format!("forbidden variable {key}")));
        }
        if value.contains('\0') {
            return Err(TrustError::Invalid(format!("{key} contains a NUL byte")));
        }
        if PATH_KEYS.contains(&key.as_str()) && !value.is_empty() && !value.starts_with('/') {
            return Err(TrustError::Invalid(format!(
                "{key} must be an absolute path"
            )));
        }
    }
    Ok(())
}

/// A target's configuration, read from a trusted file and nothing else: not
/// from the process environment, not from the request. Values may hold a
/// passphrase, so `Debug` lists keys only and the values are zeroized on drop.
pub struct TrustedConfig {
    target: Target,
    vars: BTreeMap<String, String>,
}

impl TrustedConfig {
    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    #[cfg(test)]
    pub fn vars(&self) -> &BTreeMap<String, String> {
        &self.vars
    }
}

impl std::fmt::Debug for TrustedConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedConfig")
            .field("target", &self.target)
            .field("keys", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Drop for TrustedConfig {
    fn drop(&mut self) {
        for value in std::mem::take(&mut self.vars).values_mut() {
            value.zeroize();
        }
    }
}

/// One entry of the directory that could not be used, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub name: String,
    /// A [`TrustError::kind`].
    pub reason: &'static str,
}

#[derive(Debug, Default)]
pub struct Listing {
    pub targets: Vec<Target>,
    pub rejected: Vec<Rejected>,
}

/// The entry names of an open directory, read through a duplicate of the
/// descriptor (no path, no `/proc`).
fn read_names(dir: &OwnedFd) -> Result<Vec<Vec<u8>>, TrustError> {
    let dup = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(dup) };
    if stream.is_null() {
        let err = io::Error::last_os_error();
        unsafe { libc::close(dup) };
        return Err(err.into());
    }
    let mut names = Vec::new();
    let mut failure = None;
    loop {
        // readdir signals an error only through errno.
        unsafe { *libc::__errno_location() = 0 };
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(0) {
                failure = Some(err);
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_bytes()
            .to_vec();
        if name != b"." && name != b".." {
            names.push(name);
        }
        if names.len() > 4 * MAX_LISTED {
            failure = Some(io::Error::other("directory has too many entries"));
            break;
        }
    }
    unsafe { libc::closedir(stream) };
    match failure {
        Some(e) => Err(e.into()),
        None => Ok(names),
    }
}

/// The configuration directory and the user that may own what is in it.
pub struct TrustedRoot {
    dir: PathBuf,
    uid: u32,
}

impl TrustedRoot {
    /// `/etc/luks-ssh-unlock`, trusting root. The only constructor outside
    /// the tests.
    pub fn system() -> Self {
        Self {
            dir: PathBuf::from(SYSTEM_DIR),
            uid: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test(dir: PathBuf, uid: u32) -> Self {
        Self { dir, uid }
    }

    fn walker(&self) -> Walker {
        Walker { uid: self.uid }
    }

    /// The directory, resolved through the same checks as a file.
    fn open_dir(&self, walker: &Walker, root: &OwnedFd) -> Result<OwnedFd, TrustError> {
        match retry(|| walker.walk(root, root, self.dir.as_os_str(), Want::Dir))? {
            Found::Dir(fd) => Ok(fd),
            Found::File(_) => Err(invalid("configuration directory is a file")),
        }
    }

    fn open_env(
        &self,
        walker: &Walker,
        root: &OwnedFd,
        dir: &OwnedFd,
        target: &Target,
    ) -> Result<File, TrustError> {
        let name = target.env_file_name();
        match retry(|| walker.walk(root, dir, OsStr::new(&name), Want::File))? {
            Found::File(f) => Ok(f),
            Found::Dir(_) => Err(invalid("configuration is a directory")),
        }
    }

    /// Targets that have a usable `NAME.env`, and the entries that do not.
    pub fn list(&self) -> Result<Listing, TrustError> {
        let walker = self.walker();
        let root = walker.open_root()?;
        let dir = self.open_dir(&walker, &root)?;
        // Enumerate through the descriptor we validated, not by path.
        let mut names: Vec<String> = Vec::new();
        for raw in read_names(&dir)? {
            let Ok(file) = String::from_utf8(raw) else {
                continue;
            };
            if let Some(stem) = file.strip_suffix(".env") {
                names.push(stem.to_string());
            }
            if names.len() > MAX_LISTED {
                return Err(invalid("too many entries"));
            }
        }
        names.sort();
        let mut listing = Listing::default();
        for name in names {
            let Ok(target) = Target::parse(&name) else {
                continue;
            };
            match self.open_env(&walker, &root, &dir, &target) {
                Ok(_) => listing.targets.push(target),
                Err(e) => listing.rejected.push(Rejected {
                    name,
                    reason: e.kind(),
                }),
            }
        }
        Ok(listing)
    }

    /// Read and validate `NAME.env`.
    pub fn load(&self, target: &Target) -> Result<TrustedConfig, TrustError> {
        let walker = self.walker();
        let root = walker.open_root()?;
        let dir = self.open_dir(&walker, &root)?;
        let file = self.open_env(&walker, &root, &dir, target)?;
        let mut raw = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut raw)?;
        let result = parse(&raw, target);
        raw.zeroize();
        result
    }
}

fn parse(raw: &[u8], target: &Target) -> Result<TrustedConfig, TrustError> {
    if raw.len() as u64 > MAX_CONFIG_BYTES {
        return Err(invalid("configuration is too large"));
    }
    let text = std::str::from_utf8(raw).map_err(|_| invalid("configuration is not UTF-8"))?;
    if text.contains('\0') {
        return Err(invalid("configuration contains a NUL byte"));
    }
    // The same parser the service and the dashboard use, so that what the
    // helper acts on is what they would have seen.
    let vars: BTreeMap<String, String> =
        crate::settings::parse_env_file(text).into_iter().collect();
    validate_vars(&vars)?;
    Ok(TrustedConfig {
        target: target.clone(),
        vars,
    })
}

/// Upper bound for the number of entries in a vetted tree.
const MAX_TREE_ENTRIES: usize = 4096;
/// Upper bound for the depth of a vetted tree.
const MAX_TREE_DEPTH: u32 = 8;
/// How much of a script is read to find its interpreter.
const SHEBANG_BYTES: usize = 256;

/// Variables a privileged process may be started with. Anything else, in
/// particular the loader and shell variables in [`FORBIDDEN_KEYS`], is a
/// reason to refuse to run at all.
const ALLOWED_ENV: &[&str] = &["PATH", "LANG", "LC_ALL"];

/// The process environment, once it has been checked.
#[derive(Debug, Clone)]
pub struct Environment {
    /// `PATH` as the process has it, restricted to directories that were
    /// vetted (a directory that does not exist is dropped).
    pub path: String,
    pub dirs: Vec<PathBuf>,
}

/// Checks the files, trees and commands a privileged operation will use, with
/// the same walker as the configuration: every component of an absolute path
/// is owned by a trusted user and not writable by anyone else, links are
/// followed only when they are trusted, and nothing is judged by a path that
/// is then used again by something else.
///
/// What the check buys is this: a path that passes can only be changed by a
/// trusted user. A later `open` or `exec` of the same path by code that takes
/// paths (the existing unlock logic does) therefore cannot be redirected by an
/// unprivileged user, which is the invariant the helper relies on instead of
/// passing descriptors through that code.
pub struct Vetter {
    walker: Walker,
    root: OwnedFd,
}

impl Vetter {
    /// Trusting `uid` (and root). Root in production.
    pub fn new(uid: u32) -> Result<Self, TrustError> {
        let walker = Walker { uid };
        let root = walker.open_root()?;
        Ok(Self { walker, root })
    }

    fn lookup(&self, path: &Path, want: Want) -> Result<Found, TrustError> {
        if !path.is_absolute() {
            return Err(invalid("path is not absolute"));
        }
        retry(|| {
            self.walker
                .walk(&self.root, &self.root, path.as_os_str(), want)
        })
    }

    /// A regular file nobody but a trusted user can modify.
    pub fn file(&self, path: &Path) -> Result<(), TrustError> {
        match self.lookup(path, Want::File)? {
            Found::File(_) => Ok(()),
            Found::Dir(_) => Err(invalid("a directory where a file is expected")),
        }
    }

    /// Like [`Vetter::file`], and `false` when there is no such file.
    pub fn optional_file(&self, path: &Path) -> Result<bool, TrustError> {
        match self.file(path) {
            Ok(()) => Ok(true),
            Err(TrustError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// A directory nobody but a trusted user can modify.
    pub fn dir(&self, path: &Path) -> Result<(), TrustError> {
        match self.lookup(path, Want::Dir)? {
            Found::Dir(_) => Ok(()),
            Found::File(_) => Err(invalid("a file where a directory is expected")),
        }
    }

    /// A directory and everything below it. A directory's mode says nothing
    /// about what is inside it, so each entry is vetted, links included.
    pub fn tree(&self, path: &Path) -> Result<(), TrustError> {
        let Found::Dir(dir) = self.lookup(path, Want::Dir)? else {
            return Err(invalid("a file where a directory is expected"));
        };
        let mut budget = MAX_TREE_ENTRIES;
        self.tree_below(&dir, 0, &mut budget)
    }

    fn tree_below(&self, dir: &OwnedFd, depth: u32, budget: &mut usize) -> Result<(), TrustError> {
        if depth > MAX_TREE_DEPTH {
            return Err(invalid("tree is too deep"));
        }
        for name in read_names(dir)? {
            *budget = budget
                .checked_sub(1)
                .ok_or_else(|| invalid("tree is too large"))?;
            let name = OsStr::from_bytes(&name);
            match retry(|| self.walker.walk(&self.root, dir, name, Want::Any))? {
                Found::File(_) => {}
                Found::Dir(sub) => self.tree_below(&sub, depth + 1, budget)?,
            }
        }
        Ok(())
    }

    /// The first executable called `name` in `dirs`, as `exec` would find it,
    /// vetted. A match that is not trustworthy is an error rather than a
    /// reason to look further: it is what would have been run.
    pub fn command(&self, name: &str, dirs: &[PathBuf]) -> Result<PathBuf, TrustError> {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(invalid("not a plain command name"));
        }
        for dir in dirs {
            let dir_fd = match self.lookup(dir, Want::Dir) {
                Ok(Found::Dir(fd)) => fd,
                Ok(Found::File(_)) | Err(TrustError::NotFound) => continue,
                Err(e) => return Err(e),
            };
            match retry(|| {
                self.walker
                    .walk(&self.root, &dir_fd, OsStr::new(name), Want::File)
            }) {
                Ok(Found::File(file)) => {
                    if fstat(&file)?.st_mode & 0o111 != 0 {
                        return Ok(dir.join(name));
                    }
                }
                Ok(Found::Dir(_)) | Err(TrustError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Err(TrustError::NotFound)
    }

    /// A script (or program) that will be executed: the file itself and, for
    /// a `#!` script, the interpreter it names (looked up in `dirs` when
    /// started through `env`).
    pub fn program(&self, path: &Path, dirs: &[PathBuf]) -> Result<(), TrustError> {
        let Found::File(mut file) = self.lookup(path, Want::File)? else {
            return Err(invalid("a directory where a program is expected"));
        };
        if fstat(&file)?.st_mode & 0o111 == 0 {
            return Err(invalid("program is not executable"));
        }
        let mut head = vec![0u8; SHEBANG_BYTES];
        let n = file.read(&mut head)?;
        head.truncate(n);
        let Some(rest) = head.strip_prefix(b"#!") else {
            return Ok(());
        };
        let line = rest.split(|b| *b == b'\n').next().unwrap_or_default();
        let line =
            std::str::from_utf8(line).map_err(|_| invalid("interpreter line is not UTF-8"))?;
        let mut words = line.split_ascii_whitespace();
        let interpreter = Path::new(words.next().ok_or_else(|| invalid("empty interpreter"))?);
        self.file(interpreter)?;
        if interpreter.file_name() == Some(OsStr::new("env")) {
            let program = words
                .find(|w| !w.starts_with('-'))
                .ok_or_else(|| invalid("env without a program"))?;
            self.command(program, dirs)?;
        }
        Ok(())
    }

    /// Check a process environment: only `PATH`, `LANG` and `LC_ALL`, the
    /// latter two plain locale names, and a `PATH` of absolute, vetted
    /// directories. Nothing the invoking user could have planted in the
    /// environment of a privileged process is accepted.
    pub fn environment(
        &self,
        vars: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Environment, TrustError> {
        let mut path = None;
        for (key, value) in vars {
            let key = key
                .into_string()
                .map_err(|_| invalid("environment variable name is not UTF-8"))?;
            if !ALLOWED_ENV.contains(&key.as_str()) {
                return Err(TrustError::Untrusted(format!(
                    "unexpected environment variable {key}"
                )));
            }
            let value = value
                .into_string()
                .map_err(|_| TrustError::Invalid(format!("{key} is not UTF-8")))?;
            if key == "PATH" {
                path = Some(value);
            } else if value.len() > 32
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'@' | b'-'))
            {
                return Err(TrustError::Invalid(format!("{key} is not a locale name")));
            }
        }
        let path = path.ok_or_else(|| invalid("PATH is not set"))?;
        let mut dirs = Vec::new();
        for entry in path.split(':') {
            // An empty entry means the working directory.
            if !entry.starts_with('/') {
                return Err(invalid("PATH has an empty or relative entry"));
            }
            match self.lookup(Path::new(entry), Want::Dir) {
                Ok(Found::Dir(_)) => dirs.push(PathBuf::from(entry)),
                // Nobody but a trusted user can create it: its parents were
                // vetted on the way to finding it missing.
                Ok(Found::File(_)) | Err(TrustError::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        let path = dirs
            .iter()
            .map(|d| d.to_string_lossy())
            .collect::<Vec<_>>()
            .join(":");
        Ok(Environment { path, dirs })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    fn euid() -> u32 {
        unsafe { libc::geteuid() }
    }

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("lsu-helper-")
            .tempdir_in("/tmp")
            .unwrap()
    }

    fn mkdir(path: &std::path::Path, mode: u32) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn write(path: &std::path::Path, content: &str, mode: u32) {
        std::fs::write(path, content).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Like `write`, but for modes with a setuid or setgid bit. The Nix build
    /// sandbox forbids those bits (`EPERM`): there can be no such file there,
    /// so `false` tells the test to skip the case instead of failing.
    fn write_with_id_bit(path: &std::path::Path, content: &str, mode: u32) -> bool {
        std::fs::write(path, content).unwrap();
        match std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => false,
            Err(e) => panic!("{e}"),
        }
    }

    /// A configuration directory under a temporary root that trusts the
    /// running user (the tests need no privileges).
    fn setup() -> (tempfile::TempDir, PathBuf, TrustedRoot) {
        let t = tmp();
        let dir = t.path().join("etc/luks-ssh-unlock");
        mkdir(&dir, 0o755);
        let root = TrustedRoot::for_test(dir.clone(), euid());
        (t, dir, root)
    }

    fn target(name: &str) -> Target {
        Target::parse(name).unwrap()
    }

    #[test]
    fn plain_file_loads() {
        let (_t, dir, root) = setup();
        write(
            &dir.join("alpha.env"),
            "SSH_HOSTNAME=alpha.example\nLUKS_PASSPHRASE='s3cr3t'\n",
            0o600,
        );
        let cfg = root.load(&target("alpha")).unwrap();
        assert_eq!(cfg.get("SSH_HOSTNAME"), Some("alpha.example"));
    }

    #[test]
    fn debug_lists_keys_never_values() {
        let (_t, dir, root) = setup();
        write(&dir.join("a.env"), "LUKS_PASSPHRASE=hunter2\n", 0o600);
        let cfg = root.load(&target("a")).unwrap();
        let text = format!("{cfg:?}");
        assert!(text.contains("LUKS_PASSPHRASE") && !text.contains("hunter2"));
    }

    /// `/etc/NAME.env -> /etc/static/NAME.env -> /nix/store/HASH-NAME.env`,
    /// with the sticky, group-writable store directory.
    #[test]
    fn nixos_link_chain_is_accepted() {
        let t = tmp();
        let base = t.path();
        let store = base.join("nix/store");
        mkdir(&store, 0o1775);
        write(
            &store.join("h4sh-nixos.env"),
            "SSH_HOSTNAME=nix.example\n",
            0o444,
        );
        let item = store.join("h4sh-etc");
        mkdir(&item.join("etc/luks-ssh-unlock"), 0o755);
        symlink(
            store.join("h4sh-nixos.env"),
            item.join("etc/luks-ssh-unlock/nixos.env"),
        )
        .unwrap();
        mkdir(&base.join("etc/luks-ssh-unlock"), 0o755);
        symlink(item.join("etc"), base.join("etc/static")).unwrap();
        symlink(
            base.join("etc/static/luks-ssh-unlock/nixos.env"),
            base.join("etc/luks-ssh-unlock/nixos.env"),
        )
        .unwrap();
        let root = TrustedRoot::for_test(base.join("etc/luks-ssh-unlock"), euid());
        let cfg = root.load(&target("nixos")).unwrap();
        assert_eq!(cfg.get("SSH_HOSTNAME"), Some("nix.example"));
        let listing = root.list().unwrap();
        assert_eq!(listing.targets, [target("nixos")]);
        assert!(listing.rejected.is_empty());
        // The same chain with a store that users could write into is refused.
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(matches!(
            root.load(&target("nixos")).unwrap_err(),
            TrustError::Untrusted(_)
        ));
    }

    #[test]
    fn the_directory_itself_may_be_a_trusted_link() {
        let t = tmp();
        let real = t.path().join("real");
        mkdir(&real, 0o755);
        write(&real.join("a.env"), "SSH_HOSTNAME=h\n", 0o644);
        symlink(&real, t.path().join("etc-lsu")).unwrap();
        let root = TrustedRoot::for_test(t.path().join("etc-lsu"), euid());
        assert!(root.load(&target("a")).is_ok());
    }

    #[test]
    fn relative_link_in_the_directory_is_followed() {
        let (_t, dir, root) = setup();
        write(&dir.join("real.cfg"), "SSH_HOSTNAME=h\n", 0o644);
        symlink("real.cfg", dir.join("a.env")).unwrap();
        assert!(root.load(&target("a")).is_ok());
    }

    #[test]
    fn link_into_a_user_writable_directory_is_refused() {
        for mode in [0o777, 0o775, 0o757, 0o722] {
            let (t, dir, root) = setup();
            let evil = t.path().join("evil");
            mkdir(&evil, mode);
            write(&evil.join("x"), "SSH_HOSTNAME=evil\n", 0o644);
            symlink(evil.join("x"), dir.join("a.env")).unwrap();
            let err = root.load(&target("a")).unwrap_err();
            assert!(matches!(err, TrustError::Untrusted(_)), "{mode:o}: {err}");
        }
    }

    #[test]
    fn link_into_a_sticky_directory_is_accepted_only_when_sticky() {
        let (t, dir, root) = setup();
        let sticky = t.path().join("sticky");
        mkdir(&sticky, 0o1777);
        write(&sticky.join("x"), "SSH_HOSTNAME=h\n", 0o644);
        symlink(sticky.join("x"), dir.join("a.env")).unwrap();
        assert!(root.load(&target("a")).is_ok());
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(root.load(&target("a")).is_err());
    }

    #[test]
    fn dot_dot_loops_and_deep_chains_are_refused() {
        let (t, dir, root) = setup();
        write(&t.path().join("outside"), "SSH_HOSTNAME=h\n", 0o644);
        symlink("../../outside", dir.join("a.env")).unwrap();
        assert!(matches!(
            root.load(&target("a")).unwrap_err(),
            TrustError::Untrusted(_)
        ));
        symlink("loop2.env", dir.join("loop1.env")).unwrap();
        symlink("loop1.env", dir.join("loop2.env")).unwrap();
        assert!(root.load(&target("loop1")).is_err());
        // 20 links in a row exceed the bound even without a loop.
        write(&dir.join("end"), "SSH_HOSTNAME=h\n", 0o644);
        let mut next = "end".to_string();
        for i in 0..20 {
            let name = format!("hop{i}");
            symlink(&next, dir.join(&name)).unwrap();
            next = name;
        }
        symlink(&next, dir.join("deep.env")).unwrap();
        assert!(root.load(&target("deep")).is_err());
    }

    #[test]
    fn dot_dot_in_a_link_is_refused_even_when_harmless() {
        let (_t, dir, root) = setup();
        mkdir(&dir.join("sub"), 0o755);
        write(&dir.join("real"), "SSH_HOSTNAME=h\n", 0o644);
        symlink("sub/../real", dir.join("a.env")).unwrap();
        assert!(root.load(&target("a")).is_err());
    }

    #[test]
    fn file_mode_and_type_are_checked() {
        let (_t, dir, root) = setup();
        for (name, mode) in [("w", 0o666), ("g", 0o664), ("o", 0o602), ("s", 0o4644)] {
            let path = dir.join(format!("{name}.env"));
            if !write_with_id_bit(&path, "SSH_HOSTNAME=h\n", mode) {
                continue;
            }
            assert!(
                matches!(
                    root.load(&target(name)).unwrap_err(),
                    TrustError::Untrusted(_)
                ),
                "{mode:o}"
            );
        }
        mkdir(&dir.join("d.env"), 0o755);
        assert!(root.load(&target("d")).is_err());
        let fifo = CString::new(dir.join("f.env").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        // Must fail promptly, not block on the FIFO.
        assert!(root.load(&target("f")).is_err());
        assert!(matches!(
            root.load(&target("missing")).unwrap_err(),
            TrustError::NotFound
        ));
    }

    #[test]
    fn untrusted_owner_is_refused() {
        if euid() == 0 {
            return;
        }
        // Pretend that a different user is the trusted one: what the running
        // user owns is then foreign (root-owned ancestors stay acceptable).
        let (_t, dir, _root) = setup();
        write(&dir.join("a.env"), "SSH_HOSTNAME=h\n", 0o644);
        let other = TrustedRoot::for_test(dir, euid() + 1);
        let err = other.load(&target("a")).unwrap_err();
        assert!(matches!(err, TrustError::Untrusted(_)), "{err}");
    }

    #[test]
    fn untrusted_directory_is_refused() {
        for mode in [0o777, 0o775, 0o770 | 0o002] {
            let (_t, dir, root) = setup();
            write(&dir.join("a.env"), "SSH_HOSTNAME=h\n", 0o644);
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(
                matches!(
                    root.load(&target("a")).unwrap_err(),
                    TrustError::Untrusted(_)
                ),
                "{mode:o}"
            );
            assert!(root.list().is_err());
        }
    }

    #[test]
    fn an_untrusted_ancestor_is_refused() {
        let t = tmp();
        let outer = t.path().join("outer");
        let dir = outer.join("inner");
        mkdir(&dir, 0o755);
        write(&dir.join("a.env"), "SSH_HOSTNAME=h\n", 0o644);
        let root = TrustedRoot::for_test(dir, euid());
        assert!(root.load(&target("a")).is_ok());
        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            root.load(&target("a")).unwrap_err(),
            TrustError::Untrusted(_)
        ));
    }

    #[test]
    fn size_encoding_and_content_are_checked() {
        let (_t, dir, root) = setup();
        write(
            &dir.join("big.env"),
            &format!("A={}\n", "x".repeat(70_000)),
            0o644,
        );
        assert!(matches!(
            root.load(&target("big")).unwrap_err(),
            TrustError::Invalid(_)
        ));
        std::fs::write(dir.join("bin.env"), [0xff, 0xfe, b'=', b'x']).unwrap();
        assert!(root.load(&target("bin")).is_err());
        std::fs::write(dir.join("nul.env"), b"A=b\0c\n").unwrap();
        assert!(root.load(&target("nul")).is_err());
    }

    #[test]
    fn loader_and_path_variables_are_refused() {
        let (_t, dir, root) = setup();
        for (i, body) in [
            "LD_PRELOAD=/tmp/x.so",
            "LD_LIBRARY_PATH=/tmp",
            "PATH=/tmp",
            "BASH_ENV=/tmp/x",
            "PYTHONPATH=/tmp",
            "SSH_KEY=relative/key",
            "INITRD_CHECKSUM_SCRIPT=./script",
        ]
        .iter()
        .enumerate()
        {
            let name = format!("v{i}");
            write(&dir.join(format!("{name}.env")), body, 0o644);
            assert!(root.load(&target(&name)).is_err(), "{body}");
        }
        write(
            &dir.join("ok.env"),
            "SSH_KEY=/run/secrets/k\nLUKS_PASSPHRASE_FILE=\n",
            0o644,
        );
        assert!(root.load(&target("ok")).is_ok());
    }

    #[test]
    fn process_environment_never_reaches_the_configuration() {
        let (_t, dir, root) = setup();
        write(&dir.join("a.env"), "SSH_HOSTNAME=from-file\n", 0o644);
        // SAFETY: tests in this module do not read these variables elsewhere.
        unsafe {
            std::env::set_var("SSH_HOSTNAME", "from-env");
            std::env::set_var("LUKS_SSH_UNLOCK_CONFIG_DIR", "/tmp/elsewhere");
        }
        let cfg = root.load(&target("a")).unwrap();
        assert_eq!(cfg.get("SSH_HOSTNAME"), Some("from-file"));
        assert_eq!(cfg.vars().len(), 1);
        unsafe {
            std::env::remove_var("SSH_HOSTNAME");
            std::env::remove_var("LUKS_SSH_UNLOCK_CONFIG_DIR");
        }
    }

    #[test]
    fn list_reports_usable_and_rejected_entries() {
        let (_t, dir, root) = setup();
        write(&dir.join("good.env"), "SSH_HOSTNAME=h\n", 0o644);
        write(&dir.join("loose.env"), "SSH_HOSTNAME=h\n", 0o666);
        write(&dir.join(".hidden.env"), "SSH_HOSTNAME=h\n", 0o644);
        write(&dir.join("a b.env"), "SSH_HOSTNAME=h\n", 0o644);
        write(&dir.join("ignored.txt"), "x", 0o644);
        let listing = root.list().unwrap();
        assert_eq!(listing.targets, [target("good")]);
        assert_eq!(listing.rejected.len(), 1);
        assert_eq!(listing.rejected[0].name, "loose");
    }

    /// Replace the file by renaming over it while it is being read: every
    /// read is one complete version, never an error and never a mixture.
    #[test]
    fn replacement_during_lookup_never_yields_a_mixture() {
        let (_t, dir, root) = setup();
        write(&dir.join("a.env"), "SSH_HOSTNAME=one\n", 0o644);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flipper = {
            let (dir, stop) = (dir.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let tmp = dir.join(".swap");
                    let v = if n % 2 == 0 { "two" } else { "one" };
                    write(&tmp, &format!("SSH_HOSTNAME={v}\n"), 0o644);
                    std::fs::rename(&tmp, dir.join("a.env")).unwrap();
                    n += 1;
                }
            })
        };
        for _ in 0..500 {
            let cfg = root.load(&target("a")).unwrap();
            assert!(matches!(cfg.get("SSH_HOSTNAME"), Some("one" | "two")));
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        flipper.join().unwrap();
    }

    #[test]
    fn a_foreign_owner_in_a_chain_is_refused_as_root() {
        // Needs root to create a foreign-owned entry; the rworker runs this.
        if euid() != 0 {
            return;
        }
        let (_t, dir, _r) = setup();
        write(&dir.join("a.env"), "SSH_HOSTNAME=h\n", 0o644);
        let path = CString::new(dir.join("a.env").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(path.as_ptr(), 65534, 65534) }, 0);
        // The trusted user must differ from root and from the file owner.
        let root = TrustedRoot::for_test(dir, 1);
        assert!(matches!(
            root.load(&target("a")).unwrap_err(),
            TrustError::Untrusted(_)
        ));
    }

    fn vetter() -> Vetter {
        Vetter::new(euid()).unwrap()
    }

    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    fn vars(list: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        list.iter().map(|(k, v)| (os(k), os(v))).collect()
    }

    #[test]
    fn vetted_files_and_directories() {
        let t = tmp();
        let d = t.path().join("d");
        mkdir(&d, 0o755);
        write(&d.join("ok"), "x", 0o600);
        write(&d.join("open"), "x", 0o666);
        write(&d.join("group"), "x", 0o660);
        let suid = write_with_id_bit(&d.join("suid"), "x", 0o4755);
        let v = vetter();
        v.file(&d.join("ok")).unwrap();
        v.dir(&d).unwrap();
        for bad in ["open", "group", "suid"] {
            if bad == "suid" && !suid {
                continue;
            }
            assert!(
                matches!(v.file(&d.join(bad)), Err(TrustError::Untrusted(_))),
                "{bad}"
            );
        }
        assert!(v.file(&d).is_err());
        assert!(v.dir(&d.join("ok")).is_err());
        assert!(v.file(Path::new("relative")).is_err());
        assert!(matches!(
            v.file(&d.join("missing")),
            Err(TrustError::NotFound)
        ));
        assert!(!v.optional_file(&d.join("missing")).unwrap());
        assert!(v.optional_file(&d.join("ok")).unwrap());
        assert!(v.optional_file(&d.join("open")).is_err());
    }

    #[test]
    fn a_file_behind_a_writable_directory_is_refused() {
        let t = tmp();
        let d = t.path().join("d");
        mkdir(&d, 0o777);
        write(&d.join("f"), "x", 0o600);
        let v = vetter();
        assert!(v.file(&d.join("f")).is_err());
        // A missing entry in a writable directory is not "absent", because
        // anybody could create it.
        assert!(v.optional_file(&d.join("g")).is_err());
    }

    #[test]
    fn links_are_followed_only_when_trusted() {
        let t = tmp();
        let good = t.path().join("good");
        let evil = t.path().join("evil");
        mkdir(&good, 0o755);
        mkdir(&evil, 0o777);
        write(&good.join("secret"), "x", 0o600);
        write(&evil.join("secret"), "x", 0o600);
        symlink(good.join("secret"), t.path().join("via-good")).unwrap();
        symlink(evil.join("secret"), t.path().join("via-evil")).unwrap();
        symlink(&evil, t.path().join("dir-via-evil")).unwrap();
        let v = vetter();
        v.file(&t.path().join("via-good")).unwrap();
        assert!(v.file(&t.path().join("via-evil")).is_err());
        assert!(
            v.file(&t.path().join("dir-via-evil").join("secret"))
                .is_err()
        );
    }

    #[test]
    fn a_tree_is_vetted_entry_by_entry() {
        let t = tmp();
        let tree = t.path().join("tree");
        mkdir(&tree.join("bin"), 0o755);
        write(&tree.join("bin/tool"), "x", 0o755);
        write(&tree.join("lib"), "x", 0o644);
        let v = vetter();
        v.tree(&tree).unwrap();

        write(&tree.join("bin/loose"), "x", 0o666);
        assert!(v.tree(&tree).is_err());
        std::fs::remove_file(tree.join("bin/loose")).unwrap();
        v.tree(&tree).unwrap();

        mkdir(&tree.join("bin/inner"), 0o777);
        assert!(v.tree(&tree).is_err());
        std::fs::remove_dir(tree.join("bin/inner")).unwrap();

        let outside = t.path().join("outside");
        mkdir(&outside, 0o777);
        write(&outside.join("x"), "x", 0o644);
        symlink(outside.join("x"), tree.join("link")).unwrap();
        assert!(v.tree(&tree).is_err());
        std::fs::remove_file(tree.join("link")).unwrap();
        v.tree(&tree).unwrap();
    }

    #[test]
    fn a_tree_has_bounds() {
        let t = tmp();
        let mut deep = t.path().join("deep");
        for _ in 0..(MAX_TREE_DEPTH + 2) {
            deep = deep.join("d");
        }
        mkdir(&deep, 0o755);
        assert!(vetter().tree(&t.path().join("deep")).is_err());

        let wide = t.path().join("wide");
        mkdir(&wide, 0o755);
        for i in 0..(MAX_TREE_ENTRIES + 1) {
            std::fs::write(wide.join(format!("f{i}")), "").unwrap();
        }
        assert!(vetter().tree(&wide).is_err());
    }

    #[test]
    fn a_command_is_the_first_executable_in_order() {
        let t = tmp();
        let a = t.path().join("a");
        let b = t.path().join("b");
        let missing = t.path().join("missing");
        mkdir(&a, 0o755);
        mkdir(&b, 0o755);
        write(&a.join("tool"), "x", 0o644);
        write(&b.join("tool"), "x", 0o755);
        write(&a.join("other"), "x", 0o755);
        write(&b.join("other"), "x", 0o755);
        let dirs = vec![missing, a.clone(), b.clone()];
        let v = vetter();
        // Not executable in `a`, so exec would go on to `b`.
        assert_eq!(v.command("tool", &dirs).unwrap(), b.join("tool"));
        assert_eq!(v.command("other", &dirs).unwrap(), a.join("other"));
        assert!(matches!(
            v.command("nothing", &dirs),
            Err(TrustError::NotFound)
        ));
        for bad in ["", "a/b", "../x", "x\0y"] {
            assert!(v.command(bad, &dirs).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_untrusted_first_match_is_not_skipped() {
        let t = tmp();
        let a = t.path().join("a");
        let b = t.path().join("b");
        mkdir(&a, 0o755);
        mkdir(&b, 0o755);
        write(&a.join("tool"), "x", 0o757);
        write(&b.join("tool"), "x", 0o755);
        let dirs = vec![a, b];
        assert!(matches!(
            vetter().command("tool", &dirs),
            Err(TrustError::Untrusted(_))
        ));
    }

    #[test]
    fn a_writable_command_directory_is_refused() {
        let t = tmp();
        let a = t.path().join("a");
        let b = t.path().join("b");
        mkdir(&a, 0o777);
        mkdir(&b, 0o755);
        write(&b.join("tool"), "x", 0o755);
        assert!(vetter().command("tool", &[a, b]).is_err());
    }

    #[test]
    fn a_program_and_its_interpreter_are_vetted() {
        let t = tmp();
        let bin = t.path().join("bin");
        let evil = t.path().join("evil");
        mkdir(&bin, 0o755);
        mkdir(&evil, 0o777);
        let sh = bin.join("sh");
        write(&sh, "", 0o755);
        write(&evil.join("sh"), "", 0o755);
        let dirs = vec![bin.clone()];
        let v = vetter();

        let plain = bin.join("plain");
        write(&plain, "binary", 0o755);
        v.program(&plain, &dirs).unwrap();

        let script = bin.join("script");
        write(&script, &format!("#!{}\necho\n", sh.display()), 0o755);
        v.program(&script, &dirs).unwrap();

        let bad = bin.join("bad");
        write(
            &bad,
            &format!("#!{}\necho\n", evil.join("sh").display()),
            0o755,
        );
        assert!(v.program(&bad, &dirs).is_err());

        let loose = bin.join("loose");
        write(&loose, "#!/bin/sh\n", 0o644);
        assert!(v.program(&loose, &dirs).is_err());
        assert!(v.program(&bin, &dirs).is_err());
    }

    #[test]
    fn env_scripts_are_resolved_through_the_given_directories() {
        let t = tmp();
        let bin = t.path().join("bin");
        let evil = t.path().join("evil");
        mkdir(&bin, 0o755);
        mkdir(&evil, 0o777);
        let env = bin.join("env");
        write(&env, "", 0o755);
        write(&bin.join("python"), "", 0o755);
        write(&evil.join("ruby"), "", 0o755);
        let script = bin.join("s");
        write(&script, &format!("#!{} -S python\n", env.display()), 0o755);
        let other = bin.join("o");
        write(&other, &format!("#!{} ruby\n", env.display()), 0o755);
        let v = vetter();
        v.program(&script, std::slice::from_ref(&bin)).unwrap();
        // Found nowhere in the vetted directories.
        assert!(v.program(&other, std::slice::from_ref(&bin)).is_err());
        // Found only in a directory that is not trusted.
        assert!(v.program(&other, &[evil, bin]).is_err());
    }

    #[test]
    fn a_clean_environment_is_accepted() {
        let t = tmp();
        let a = t.path().join("a");
        let b = t.path().join("b");
        mkdir(&a, 0o755);
        mkdir(&b, 0o755);
        let path = format!(
            "{}:{}:{}",
            a.display(),
            t.path().join("none").display(),
            b.display()
        );
        let e = vetter()
            .environment(vars(&[
                ("PATH", &path),
                ("LANG", "en_US.UTF-8"),
                ("LC_ALL", "C"),
            ]))
            .unwrap();
        assert_eq!(e.dirs, vec![a.clone(), b.clone()]);
        assert_eq!(e.path, format!("{}:{}", a.display(), b.display()));
    }

    #[test]
    fn a_dirty_environment_is_refused() {
        let t = tmp();
        let a = t.path().join("a");
        let w = t.path().join("w");
        mkdir(&a, 0o755);
        mkdir(&w, 0o777);
        let good = a.display().to_string();
        let writable = w.display().to_string();
        let v = vetter();
        let cases: Vec<Vec<(&str, String)>> = vec![
            vec![("PATH", good.clone()), ("LD_PRELOAD", "/x.so".into())],
            vec![("PATH", good.clone()), ("LD_LIBRARY_PATH", "/x".into())],
            vec![("PATH", good.clone()), ("BASH_ENV", "/x".into())],
            vec![("PATH", good.clone()), ("TMPDIR", "/x".into())],
            vec![("PATH", good.clone()), ("HOME", "/x".into())],
            vec![("PATH", good.clone()), ("SSH_AUTH_SOCK", "/x".into())],
            vec![("PATH", good.clone()), ("LANG", "$(id)".into())],
            vec![("PATH", good.clone()), ("LC_ALL", "a b".into())],
            vec![("PATH", good.clone()), ("LANG", "x".repeat(33))],
            vec![("PATH", format!("{good}:"))],
            vec![("PATH", format!(":{good}"))],
            vec![("PATH", format!("{good}::{good}"))],
            vec![("PATH", format!("{good}:relative/bin"))],
            vec![("PATH", String::new())],
            vec![("PATH", writable.clone())],
            vec![("PATH", format!("{good}:{writable}"))],
            vec![("LANG", "C".into())],
            vec![],
        ];
        for case in cases {
            let list: Vec<(&str, &str)> = case.iter().map(|(k, v)| (*k, v.as_str())).collect();
            assert!(v.environment(vars(&list)).is_err(), "{case:?}");
        }
    }

    #[test]
    fn a_non_utf8_environment_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let t = tmp();
        let a = t.path().join("a");
        mkdir(&a, 0o755);
        let v = vetter();
        let mut list = vars(&[("PATH", &a.display().to_string())]);
        list.push((os("LANG"), OsString::from_vec(vec![0xff, 0xfe])));
        assert!(v.environment(list).is_err());
        let mut list = vars(&[("PATH", &a.display().to_string())]);
        list.push((OsString::from_vec(vec![0xff]), os("x")));
        assert!(v.environment(list).is_err());
    }
}
