//! The root-side process behind a socket.
//!
//! One process per accepted connection (systemd socket activation with
//! `Accept=yes`, the connection as standard input and output). Who may
//! connect is decided by the permissions of the socket file, and which
//! [`Policy`] applies by which socket it is: nothing in a request can change
//! either. This process still checks both before it does anything, so that a
//! unit or a socket that is more open than intended stops working instead of
//! giving a user more than they were granted.

use std::fs;
use std::io::{self, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::backend::{SystemBackend, sanitize_process_env};
use super::reply::{Failure, Outcome};
use super::request::{ServiceAction, Target};
use super::trust::{TrustedConfig, TrustedRoot};
use super::{Backend, Policy, harden_process, serve};

/// How long a connection may sit without sending a request.
const IDLE: Duration = Duration::from_secs(120);
/// How long a response may take to be accepted by the peer.
const SEND: Duration = Duration::from_secs(30);

/// Why the helper refused to serve. Fixed texts: they go to the journal.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused(pub &'static str);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Refused {}

/// Who is on the other end of the socket, as the kernel saw it when the
/// connection was made.
#[derive(Clone, Copy, Debug)]
pub struct Peer {
    pub uid: u32,
    pub pid: i32,
}

/// Serve the connection on standard input and output. `checksum_script` is
/// the initrd checksum program to use when a configuration names none.
///
/// Changes the process environment: call it before any other thread exists.
pub fn run_stdio(policy: Policy, checksum_script: Option<PathBuf>) -> Result<(), Refused> {
    harden_process();
    if unsafe { libc::geteuid() } != 0 {
        return Err(Refused("the helper must run as root"));
    }
    let scratch = std::env::var_os("RUNTIME_DIRECTORY")
        .and_then(|v| {
            v.to_str()
                .and_then(|v| v.split(':').next().map(PathBuf::from))
        })
        .filter(|p| p.is_absolute());
    // Everything the unit or its caller put in the environment goes.
    unsafe { sanitize_process_env() };
    let scratch = scratch.ok_or(Refused("no runtime directory for scratch files"))?;
    let stdin = dup_stdin().map_err(|_| Refused("standard input is not usable"))?;
    let peer = peer_of(stdin.as_raw_fd()).map_err(|_| Refused("standard input is not a socket"))?;
    let bound = bound_path(stdin.as_raw_fd()).map_err(|_| Refused("socket has no path"))?;
    check_socket(&bound, 0, policy)?;
    let backend = SystemBackend::new(scratch, checksum_script)
        .map_err(|_| Refused("the helper's environment is not trusted"))?;
    let stream = UnixStream::from(stdin);
    session(
        stream,
        peer,
        &TrustedRoot::system(),
        policy,
        &mut Audited::new(backend, peer),
    )
    .map_err(|_| Refused("the connection failed"))
}

fn dup_stdin() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Run the request loop on a connection, with the timeouts that keep a stuck
/// peer from holding the process.
pub fn session(
    stream: UnixStream,
    peer: Peer,
    root: &TrustedRoot,
    policy: Policy,
    backend: &mut dyn Backend,
) -> io::Result<()> {
    eprintln!(
        "helper: session from uid {} pid {} (force {}, service {})",
        peer.uid, peer.pid, policy.allow_force, policy.allow_service
    );
    stream.set_read_timeout(Some(IDLE))?;
    stream.set_write_timeout(Some(SEND))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let result = serve(root, &policy, backend, &mut reader, &mut writer);
    eprintln!("helper: session from uid {} ended", peer.uid);
    result
}

pub fn peer_of(fd: RawFd) -> io::Result<Peer> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Peer {
        uid: cred.uid,
        pid: cred.pid,
    })
}

/// The path the socket is bound to; an unnamed or abstract socket has none.
fn bound_path(fd: RawFd) -> io::Result<PathBuf> {
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    let rc =
        unsafe { libc::getsockname(fd, (&mut addr as *mut libc::sockaddr_un).cast(), &mut len) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    let used = (len as usize).saturating_sub(offset);
    let bytes: Vec<u8> = addr.sun_path[..used.min(addr.sun_path.len())]
        .iter()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .collect();
    if bytes.is_empty() || bytes.first() == Some(&0) {
        return Err(io::Error::other("unnamed socket"));
    }
    Ok(PathBuf::from(
        <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(bytes),
    ))
}

/// The socket a connection came in on must be one this policy can safely be
/// served on: owned by the trusted user, closed to everyone but its owner and
/// one group, and in a directory nobody else can write to. A socket that
/// anyone could connect to, or whose path someone else could replace, is a
/// configuration mistake, and it is refused instead of served.
pub fn check_socket(path: &Path, trusted_uid: u32, _policy: Policy) -> Result<(), Refused> {
    let meta = fs::symlink_metadata(path).map_err(|_| Refused("cannot inspect the socket"))?;
    if meta.uid() != trusted_uid {
        return Err(Refused("the socket is not owned by root"));
    }
    if meta.mode() & 0o007 != 0 {
        return Err(Refused("the socket is accessible to everyone"));
    }
    let parent = path
        .parent()
        .ok_or(Refused("the socket has no directory"))?;
    let dir = fs::metadata(parent).map_err(|_| Refused("cannot inspect the socket directory"))?;
    if dir.uid() != trusted_uid || dir.mode() & 0o022 != 0 {
        return Err(Refused("the socket directory can be changed by others"));
    }
    Ok(())
}

/// Logs what each request came to, to the journal: who, what, for which
/// target and how it ended. Never what the operation printed.
struct Audited {
    inner: SystemBackend,
    peer: Peer,
}

impl Audited {
    fn new(inner: SystemBackend, peer: Peer) -> Self {
        Self { inner, peer }
    }

    fn note(&self, verb: &str, target: &Target, result: &Result<Outcome, Failure>) {
        let end = match result {
            Ok(_) => "ok".to_string(),
            Err(f) => format!("{:?}", f.code()),
        };
        eprintln!(
            "helper: uid {} pid {} {verb} {target}: {end}",
            self.peer.uid, self.peer.pid
        );
    }
}

impl Backend for Audited {
    fn probe(&mut self, config: &TrustedConfig) -> Result<Outcome, Failure> {
        let r = self.inner.probe(config);
        self.note("probe", config.target(), &r);
        r
    }

    fn unlock(&mut self, config: &TrustedConfig, force: bool) -> Result<Outcome, Failure> {
        let r = self.inner.unlock(config, force);
        self.note(
            if force { "unlock_force" } else { "unlock" },
            config.target(),
            &r,
        );
        r
    }

    fn service(&mut self, target: &Target, action: ServiceAction) -> Result<Outcome, Failure> {
        let r = self.inner.service(target, action);
        self.note(
            match action {
                ServiceAction::Start => "service start",
                ServiceAction::Stop => "service stop",
            },
            target,
            &r,
        );
        r
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    use super::*;

    fn euid() -> u32 {
        unsafe { libc::geteuid() }
    }

    fn socket_in(dir_mode: u32, socket_mode: u32) -> (tempfile::TempDir, PathBuf, UnixListener) {
        let tmp = tempfile::Builder::new()
            .prefix("lsu-sock-")
            .tempdir_in("/tmp")
            .unwrap();
        let dir = tmp.path().join("run");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(dir_mode)).unwrap();
        let path = dir.join("basic.sock");
        let listener = UnixListener::bind(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(socket_mode)).unwrap();
        (tmp, path, listener)
    }

    #[test]
    fn a_closed_socket_in_a_closed_directory_is_served() {
        for mode in [0o600, 0o660, 0o640] {
            let (_t, path, _l) = socket_in(0o755, mode);
            assert_eq!(check_socket(&path, euid(), Policy::default()), Ok(()));
        }
    }

    #[test]
    fn a_socket_anyone_can_reach_is_refused() {
        for mode in [0o666, 0o662, 0o664, 0o601] {
            let (_t, path, _l) = socket_in(0o755, mode);
            assert!(
                check_socket(&path, euid(), Policy::default()).is_err(),
                "{mode:o}"
            );
        }
    }

    #[test]
    fn a_directory_others_can_change_is_refused() {
        for mode in [0o775, 0o757, 0o777] {
            let (_t, path, _l) = socket_in(mode, 0o660);
            assert!(
                check_socket(&path, euid(), Policy::default()).is_err(),
                "{mode:o}"
            );
        }
    }

    #[test]
    fn a_socket_owned_by_someone_else_is_refused() {
        let (_t, path, _l) = socket_in(0o755, 0o660);
        assert!(check_socket(&path, euid() + 1, Policy::default()).is_err());
    }

    #[test]
    fn the_bound_path_and_peer_of_a_connection_are_found() {
        let (_t, path, listener) = socket_in(0o755, 0o660);
        let _client = UnixStream::connect(&path).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        assert_eq!(bound_path(accepted.as_raw_fd()).unwrap(), path);
        let peer = peer_of(accepted.as_raw_fd()).unwrap();
        assert_eq!(peer.uid, euid());
        assert_eq!(peer.pid, std::process::id() as i32);
    }

    #[test]
    fn something_that_is_not_a_socket_has_no_peer() {
        let file = fs::File::open("/dev/null").unwrap();
        assert!(peer_of(file.as_raw_fd()).is_err());
        assert!(bound_path(file.as_raw_fd()).is_err());
    }
}
