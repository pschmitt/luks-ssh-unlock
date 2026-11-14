//! Graphical (VNC) console of an Oracle Cloud instance: acquisition only.
//!
//! [`open`] makes the console connection the serial console uses
//! ([`super::Supervisor`]: tagged, owned by a helper that deletes it when we
//! are gone), waits for its `vnc-connection-string` and turns that into a
//! local VNC endpoint: an `ssh -N -L` tunnel through the console service to
//! port 5900 of the instance, listening on a Unix-domain socket in a private
//! (`0700`) directory of ours. The caller gets the connected stream from
//! [`VncSession::connect`]; nothing in here speaks RFB.
//!
//! * The provider's string is never handed to a shell. It is tokenised,
//!   validated against the documented shape (`ssh -o ProxyCommand=... -N -L
//!   localhost:5900:INSTANCE:5900 INSTANCE`, for the connection and the
//!   instance of this run) and the `ssh` command line is rebuilt from the
//!   validated parts. The local endpoint the provider proposes is ignored.
//! * The endpoint is not reachable by other local users (the reason it is a
//!   socket and not a loopback TCP port, where any local user could connect
//!   first and take the console, which OCI gives to one client at a time): the
//!   directory is created by `mkdtemp` (mode `0700`, verified, owned by us),
//!   the socket is made `0600` (`StreamLocalBindMask`), never unlinked over
//!   (`StreamLocalBindUnlink=no`), and has to belong to our `ssh` by inode.
//!   [`VncSession::connect`] additionally checks the owner of the listener
//!   through `SO_PEERCRED`. Only root can get around that. Readiness is not
//!   found out by connecting (a probe would take the console slot) but by
//!   `/proc/net/unix`. Linux only.
//! * `ssh` output is never shown, only a category of it ([`Diagnosis`]).
//! * Nothing is left behind: the tunnel dies with us (`PDEATHSIG`) and on
//!   [`VncSession::close`] or drop, the socket directory is removed with it
//!   (a crash leaves an empty-ish `lsu-vnc-*` directory behind) and the
//!   connection is deleted by the helper in every case.
//!
//! The caller owns the policy (when to open, retries, what to show). `open`
//! blocks for as long as the service takes (up to a few minutes): run it on a
//! thread of its own and use a [`Cancel`] token to end it early. A running
//! `oci` call is waited for (it has its own timeout), so cancelling may take
//! a moment; the connection is cleaned up in the background even if the
//! caller does not wait for `open` to return.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use super::{
    Console, OciCli, Options, SSH_GRACE, Supervisor, Target, bad_string, is_ocid_text,
    parse_proxy_command, resolve_identity, sanitize, split_words,
};
use crate::process::shell_quote;

/// The VNC port of an instance, as the service forwards it.
const VNC_PORT: u16 = 5900;
/// How long the tunnel may take to come up (two SSH hops).
const READY_TIMEOUT: Duration = Duration::from_secs(60);
const PROC_UNIX: &str = "/proc/net/unix";
/// File name of the tunnel's socket in its private directory.
const SOCKET_NAME: &str = "vnc.sock";
/// Longest socket path we use (`sun_path` holds 108 bytes, with the NUL).
const MAX_SOCKET_PATH: usize = 100;

/// Settings of one graphical console.
#[derive(Debug, Clone, Default)]
pub struct VncOptions {
    /// Instance, identity, CLI configuration, `keep`, `oci` and `ssh`
    /// binaries: as for the serial console
    pub oci: Options,
    /// Where to create the private directory of the tunnel's socket
    /// (default: `$XDG_RUNTIME_DIR`, else the temporary directory)
    pub runtime_dir: Option<PathBuf>,
}

/// Cancels an [`open`] from another thread. Clones share the flag.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// The error of an [`open`] that was cancelled (test with
/// `err.downcast_ref::<Cancelled>()`). Everything has been cleaned up.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// What `ssh` said, reduced to a category: safe to show, unlike its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnosis {
    HostKey,
    AuthDenied,
    PortInUse,
    /// The service allows one console client at a time
    ConsoleBusy,
    /// The service could not reach the instance's VNC port
    Unavailable,
    Network,
}

impl Diagnosis {
    pub fn message(self) -> &'static str {
        match self {
            Self::HostKey => {
                "the SSH host key of the console service was rejected (check the \
                 known_hosts entry of the instance-console host)"
            }
            Self::AuthDenied => {
                "the console service refused the SSH key (a key with a passphrase \
                 must be loaded in ssh-agent)"
            }
            Self::PortInUse => "the local socket of the tunnel is already in use",
            Self::ConsoleBusy => {
                "the console is in use by another client (OCI allows one at a time, \
                 and a client that died can hold it for about 5 minutes)"
            }
            Self::Unavailable => {
                "the instance does not answer on its VNC port (not running, or no \
                 graphical console available)"
            }
            Self::Network => {
                "the console service could not be reached (OCI console access needs \
                 outbound TCP port 443)"
            }
        }
    }
}

impl fmt::Display for Diagnosis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// The category of one line of `ssh` output, if it is one we know.
fn classify(line: &str) -> Option<Diagnosis> {
    let l = line.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| l.contains(n));
    if has(&[
        "host key verification failed",
        "remote host identification has changed",
    ]) {
        Some(Diagnosis::HostKey)
    } else if has(&["permission denied", "too many authentication failures"]) {
        Some(Diagnosis::AuthDenied)
    } else if has(&[
        "address already in use",
        "cannot listen to port",
        "could not request local forwarding",
    ]) {
        Some(Diagnosis::PortInUse)
    } else if has(&["administratively prohibited", "console access is limited"]) {
        Some(Diagnosis::ConsoleBusy)
    } else if has(&[
        "open failed: connect failed",
        "open failed: unknown channel",
    ]) {
        Some(Diagnosis::Unavailable)
    } else if has(&[
        "connection timed out",
        "connection refused",
        "could not resolve",
        "kex_exchange_identification",
        "connection closed by",
        "connection reset",
        "no route to host",
        "network is unreachable",
        "banner exchange",
    ]) {
        Some(Diagnosis::Network)
    } else {
        None
    }
}

// ------------------------------------------------- connection string parse

/// Parse and strictly validate the graphical console connection string.
///
/// Accepted shape (what the service returns):
/// `ssh -o ProxyCommand='ssh -W %h:%p -p 443 CONN@instance-console.REGION.HOST'
///  -N -L localhost:5900:INSTANCE:5900 INSTANCE`
/// where CONN must be EXPECTED_CONN (the connection this run created) and
/// INSTANCE (twice) EXPECTED_INSTANCE (the instance it was created for). The
/// local port and `-N` are free; the bind address must be a loopback name; the
/// remote port must be 5900. Anything else (more options, a shell syntax, a
/// serial console string) is an error.
fn parse_vnc_connection_string(
    s: &str,
    expected_conn: &str,
    expected_instance: &str,
) -> Result<Console> {
    let bad = bad_string;
    let outer = split_words(s.trim()).map_err(bad)?;
    let mut it = outer.iter();
    if it.next().map(String::as_str) != Some("ssh") {
        return Err(bad("not an ssh command"));
    }
    let mut proxy: Option<&str> = None;
    let mut target: Option<&str> = None;
    let mut forward: Option<&str> = None;
    let mut no_command = false;
    while let Some(word) = it.next() {
        match word.as_str() {
            "-N" => {
                if std::mem::replace(&mut no_command, true) {
                    return Err(bad("more than one -N"));
                }
            }
            "-L" => {
                let spec = it.next().ok_or_else(|| bad("dangling -L"))?;
                if forward.replace(spec).is_some() {
                    return Err(bad("more than one forward"));
                }
            }
            "-o" => {
                let opt = it.next().ok_or_else(|| bad("dangling -o"))?;
                let value = opt
                    .split_once('=')
                    .filter(|(k, _)| k.eq_ignore_ascii_case("ProxyCommand"))
                    .map(|(_, v)| v)
                    .ok_or_else(|| bad("an option other than ProxyCommand"))?;
                if proxy.replace(value).is_some() {
                    return Err(bad("more than one ProxyCommand"));
                }
            }
            w if w.starts_with('-') => return Err(bad("an unsupported ssh option")),
            w => {
                if target.replace(w).is_some() {
                    return Err(bad("more than one destination"));
                }
            }
        }
    }
    let proxy = proxy.ok_or_else(|| bad("no ProxyCommand"))?;
    let target = target.ok_or_else(|| bad("no destination"))?;
    let forward = forward.ok_or_else(|| bad("no forward (not a graphical console)"))?;
    if !target.starts_with("ocid1.instance.") || !is_ocid_text(target) {
        return Err(bad("the destination is not an instance OCID"));
    }
    if target != expected_instance {
        return Err(bad("it is for another instance"));
    }

    let parts: Vec<&str> = forward.split(':').collect();
    let [bind, local, host, remote] = parts[..] else {
        return Err(bad("the forward is not BIND:PORT:HOST:PORT"));
    };
    if !matches!(bind, "localhost" | "127.0.0.1") {
        return Err(bad("the forward does not bind to loopback"));
    }
    if local.parse::<u16>().ok().filter(|p| *p != 0).is_none() {
        return Err(bad("the forward has an invalid local port"));
    }
    if host != expected_instance {
        return Err(bad("the forward leads to another host"));
    }
    if remote.parse::<u16>().ok() != Some(VNC_PORT) {
        return Err(bad("the forward does not lead to the VNC port"));
    }

    let (proxy_user, proxy_host, proxy_port) = parse_proxy_command(proxy, expected_conn)?;
    Ok(Console {
        target: target.to_string(),
        proxy_user,
        proxy_host,
        proxy_port,
    })
}

/// The `ssh` options both hops use. Prompts are impossible (nobody could
/// answer them), so a new host key is accepted and a changed one refused.
const HOP_OPTIONS: [&str; 4] = [
    "IdentitiesOnly=yes",
    "BatchMode=yes",
    "StrictHostKeyChecking=accept-new",
    "ConnectTimeout=20",
];

/// The `ssh` arguments of the tunnel for CONSOLE: the identity is forced, the
/// forward is ours (the Unix socket SOCKET, mode 0600, never unlinked over)
/// and nothing else comes from the provider.
fn tunnel_args(console: &Console, ssh_bin: &str, key: &Path, socket: &Path) -> Vec<OsString> {
    let hop = HOP_OPTIONS
        .iter()
        .map(|o| format!("-o {o}"))
        .collect::<Vec<_>>()
        .join(" ");
    let proxy_port = console
        .proxy_port
        .map_or_else(String::new, |p| format!(" -p {p}"));
    let proxy = format!(
        "{} -i {} {hop} -W %h:%p{proxy_port} {}@{}",
        shell_quote(ssh_bin),
        shell_quote(&key.to_string_lossy()),
        console.proxy_user,
        console.proxy_host
    );
    let mut args: Vec<OsString> = vec!["-N".into(), "-i".into(), key.into()];
    let options = HOP_OPTIONS.iter().copied().chain([
        "ExitOnForwardFailure=yes",
        "StreamLocalBindUnlink=no",
        "StreamLocalBindMask=0177",
        "GatewayPorts=no",
        "PermitLocalCommand=no",
        "ControlMaster=no",
        "ControlPath=none",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
    ]);
    for o in options {
        args.push("-o".into());
        args.push(o.into());
    }
    args.push("-o".into());
    args.push(format!("ProxyCommand={proxy}").into());
    args.push("-L".into());
    let mut forward = OsString::from(socket);
    forward.push(format!(":{}:{VNC_PORT}", console.target));
    args.push(forward);
    args.push(console.target.clone().into());
    args
}

// ------------------------------------------------------ listener ownership

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// One listening stream socket of `/proc/net/unix`.
#[derive(Debug, PartialEq, Eq)]
struct Listening {
    path: String,
    inode: u64,
}

/// The listening stream sockets with a path in TABLE (the text of
/// `/proc/net/unix`: `Num RefCount Protocol Flags Type St Inode Path`).
fn parse_listeners(table: &str) -> Vec<Listening> {
    /// `__SO_ACCEPTCON`: the socket listens
    const ACCEPTCON: u32 = 0x1_0000;
    const SOCK_STREAM: u32 = 1;
    const SS_UNCONNECTED: u32 = 1;
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let hex = |i: usize| u32::from_str_radix(f.get(i)?, 16).ok();
            if hex(3)? & ACCEPTCON == 0 || hex(4)? != SOCK_STREAM || hex(5)? != SS_UNCONNECTED {
                return None;
            }
            Some(Listening {
                path: f.get(7)?.to_string(),
                inode: f.get(6)?.parse().ok()?,
            })
        })
        .collect()
}

/// The socket inodes PID holds.
fn socket_inodes(pid: u32) -> HashSet<u64> {
    let Ok(dir) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return HashSet::new();
    };
    dir.flatten()
        .filter_map(|e| fs::read_link(e.path()).ok())
        .filter_map(|link| {
            link.to_str()?
                .strip_prefix("socket:[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Listener {
    /// Nothing of ours listens on the path yet
    NotYet,
    /// Our `ssh` listens on the path, and nothing else does
    Ours,
    /// The path is not ours alone, or not private: do not use it
    Conflict(Conflict),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Conflict {
    /// Another socket listens on the path too
    Foreign,
    /// The file at the path is not a socket of ours that only we can reach
    Insecure,
}

/// Decide from TABLE (`/proc/net/unix`) and OURS (the inodes of the tunnel
/// process) who listens on PATH. A listener of someone else alone is not a
/// conflict yet: our `ssh` then fails to bind and ends.
fn classify_listeners(table: &str, ours: &HashSet<u64>, path: &str) -> Listener {
    let (mut mine, mut others) = (0, 0);
    for l in parse_listeners(table).iter().filter(|l| l.path == path) {
        if ours.contains(&l.inode) {
            mine += 1;
        } else {
            others += 1;
        }
    }
    if mine > 0 && others > 0 {
        Listener::Conflict(Conflict::Foreign)
    } else if mine > 0 {
        Listener::Ours
    } else {
        Listener::NotYet
    }
}

/// Whether PATH is a socket of ours that nobody else can open (a socket
/// with wider rights than 0600 is narrowed: it is in a private directory
/// anyway).
fn socket_is_ours(path: &Path) -> bool {
    let Ok(md) = fs::symlink_metadata(path) else {
        return false;
    };
    if !md.file_type().is_socket() || md.uid() != euid() {
        return false;
    }
    md.mode() & 0o077 == 0 || fs::set_permissions(path, fs::Permissions::from_mode(0o600)).is_ok()
}

fn listener_state(pid: u32, socket: &Path) -> Listener {
    let (Some(path), Ok(table)) = (socket.to_str(), fs::read_to_string(PROC_UNIX)) else {
        return Listener::NotYet;
    };
    match classify_listeners(&table, &socket_inodes(pid), path) {
        Listener::Ours if !socket_is_ours(socket) => Listener::Conflict(Conflict::Insecure),
        state => state,
    }
}

// --------------------------------------------------------- private directory

/// Create the private directory of the tunnel's socket under BASE (default:
/// `$XDG_RUNTIME_DIR`, else the temporary directory) and return it with the
/// path of the socket in it. The directory is ours alone (mode 0700, checked);
/// it is removed with the returned guard.
fn private_socket_dir(base: Option<&Path>) -> Result<(tempfile::TempDir, PathBuf)> {
    let base = match base {
        Some(b) => b.to_path_buf(),
        None => std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(std::env::temp_dir),
    };
    let base = std::path::absolute(&base).context("cannot resolve the runtime directory")?;
    let dir = tempfile::Builder::new()
        .prefix("lsu-vnc-")
        .tempdir_in(&base)
        .context("cannot create the private directory of the console tunnel")?;
    // Spelled out, not left to the umask or to a default ACL of the parent
    // (which would show up as group bits).
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
        .context("cannot restrict the socket directory")?;
    let md = fs::symlink_metadata(dir.path()).context("cannot inspect the socket directory")?;
    if !md.is_dir() || md.uid() != euid() || md.mode() & 0o777 != 0o700 {
        bail!(
            "the socket directory of the console tunnel is not private (mode 0700, ours; \
             found mode {:o}, owner matches: {})",
            md.mode() & 0o7777,
            md.uid() == euid()
        );
    }
    let socket = dir.path().join(SOCKET_NAME);
    check_socket_path(&socket)?;
    Ok((dir, socket))
}

/// `ssh -L` takes the socket as a word of `:`-separated fields, and the
/// kernel limits its length.
fn check_socket_path(p: &Path) -> Result<()> {
    let s = p.to_str().context("the socket path is not UTF-8")?;
    if !p.is_absolute()
        || s.chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == ':')
    {
        bail!("the socket path of the console tunnel has characters that cannot be used");
    }
    if s.len() > MAX_SOCKET_PATH {
        bail!(
            "the socket path of the console tunnel is too long (set a shorter runtime directory)"
        );
    }
    Ok(())
}

/// The credentials of the process that made the listening socket behind S.
fn peer_cred(s: &UnixStream) -> io::Result<libc::ucred> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: CRED and LEN are valid for the size LEN states.
    let r = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut cred).cast(),
            &mut len,
        )
    };
    if r == 0 {
        Ok(cred)
    } else {
        Err(io::Error::last_os_error())
    }
}

// ------------------------------------------------------------------ tunnel

#[derive(Debug)]
enum StartFail {
    Spawn(String),
    Exited(Option<i32>, Option<Diagnosis>),
    Timeout,
    Conflict(Conflict),
    Cancelled,
}

impl StartFail {
    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Spawn(e) => anyhow!("cannot run ssh for the console tunnel: {e}"),
            Self::Exited(code, diagnosis) => {
                let code = code.map_or_else(|| "killed".to_string(), |c| format!("exit {c}"));
                let why = diagnosis.map_or(
                    "no details (run the ssh command by hand to see the error)",
                    Diagnosis::message,
                );
                anyhow!("the console tunnel (ssh) ended during start ({code}): {why}")
            }
            Self::Timeout => anyhow!(
                "the console tunnel did not come up within {} s",
                READY_TIMEOUT.as_secs()
            ),
            Self::Conflict(Conflict::Foreign) => {
                anyhow!("another process listens on the socket of the tunnel: refusing to use it")
            }
            Self::Conflict(Conflict::Insecure) => {
                anyhow!("the tunnel socket is not a private socket of ours: refusing to use it")
            }
            Self::Cancelled => Cancelled.into(),
        }
    }
}

/// State the owner thread shares with the session.
#[derive(Default)]
struct Shared {
    exited: AtomicBool,
    diagnosis: Mutex<Option<Diagnosis>>,
}

/// A running `ssh -N -L`: it is spawned, watched and ended by one thread of
/// ours (`PDEATHSIG` ends the process with the thread that spawned it, so that
/// thread must live as long as the tunnel). It owns the private directory of
/// its socket, which goes after `ssh` has.
struct Tunnel {
    stop: Arc<AtomicBool>,
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
    socket: PathBuf,
    /// Process id of `ssh`, the listener of the socket
    pid: u32,
    _dir: tempfile::TempDir,
}

impl Tunnel {
    /// End `ssh` (SIGTERM, then SIGKILL) and wait until it is gone.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start `ssh` with ARGS, and return when it listens on SOCKET (in DIR).
/// CANCELLED is polled meanwhile.
fn start_tunnel(
    bin: &Path,
    args: Vec<OsString>,
    dir: tempfile::TempDir,
    socket: PathBuf,
    cancelled: &dyn Fn() -> bool,
) -> Result<Tunnel, StartFail> {
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Shared::default());
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = {
        let (bin, stop, shared) = (bin.to_path_buf(), stop.clone(), shared.clone());
        let socket = socket.clone();
        thread::Builder::new()
            .name("lsu-oci-vnc-ssh".into())
            .spawn(move || {
                tunnel_thread(&bin, &args, &socket, &stop, &shared, &ready_tx);
                shared.exited.store(true, Ordering::SeqCst);
            })
            .map_err(|e| StartFail::Spawn(e.to_string()))?
    };
    let mut tunnel = Tunnel {
        stop,
        shared,
        thread: Some(thread),
        socket,
        pid: 0,
        _dir: dir,
    };
    loop {
        if cancelled() {
            drop(tunnel);
            return Err(StartFail::Cancelled);
        }
        match ready_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(pid)) => {
                tunnel.pid = pid;
                return Ok(tunnel);
            }
            Ok(Err(fail)) => return Err(fail),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(StartFail::Exited(None, None)),
        }
    }
}

/// Collect the categories of what `ssh` writes to stderr into SHARED. The
/// text itself is dropped line by line.
fn watch_stderr(mut r: impl Read, shared: &Shared) {
    fn flush(line: &mut Vec<u8>, shared: &Shared) {
        if let Some(d) = classify(&String::from_utf8_lossy(line)) {
            *shared.diagnosis.lock().unwrap_or_else(|e| e.into_inner()) = Some(d);
        }
        line.clear();
    }
    let (mut line, mut buf) = (Vec::new(), [0u8; 4096]);
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' {
                flush(&mut line, shared);
            } else if line.len() < 4096 {
                line.push(b);
            }
        }
    }
    flush(&mut line, shared);
}

/// SIGTERM to the process group of CHILD (it leads one: `ssh` and its
/// ProxyCommand), SIGKILL after the grace period; reaped.
fn terminate(child: &mut Child) {
    let group = -(child.id() as i32);
    // SAFETY: signalling the process group our child leads.
    unsafe { libc::kill(group, libc::SIGTERM) };
    let end = Instant::now() + SSH_GRACE;
    while Instant::now() < end {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above.
    unsafe { libc::kill(group, libc::SIGKILL) };
    let _ = child.kill();
    let _ = child.wait();
}

/// Wait up to a short while for H (a stderr reader) to see the end of its
/// stream: a lingering ProxyCommand can hold it open.
fn settle(h: Option<thread::JoinHandle<()>>) {
    if let Some(h) = h {
        let end = Instant::now() + Duration::from_millis(500);
        while !h.is_finished() && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        if h.is_finished() {
            let _ = h.join();
        }
    }
}

fn tunnel_thread(
    bin: &Path,
    args: &[OsString],
    socket: &Path,
    stop: &AtomicBool,
    shared: &Arc<Shared>,
    ready: &mpsc::Sender<Result<u32, StartFail>>,
) {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    // SAFETY: prctl is async-signal-safe. ssh must not outlive us.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(StartFail::Spawn(format!(
                "{}: {}",
                sanitize(&bin.display().to_string()),
                e.kind()
            ))));
            return;
        }
    };
    let mut reader = child.stderr.take().map(|err| {
        let shared = shared.clone();
        thread::spawn(move || watch_stderr(err, &shared))
    });
    let started = Instant::now();
    let mut announced = false;
    // The failure to report to the opener, with ssh ended already or not.
    let mut failure: Option<(StartFail, bool)> = None;
    loop {
        if stop.load(Ordering::SeqCst) {
            terminate(&mut child);
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                if !announced {
                    settle(reader.take());
                }
                let diagnosis = *shared.diagnosis.lock().unwrap_or_else(|e| e.into_inner());
                failure = Some((StartFail::Exited(status.code(), diagnosis), true));
                break;
            }
            Ok(None) => {}
            Err(_) => {
                failure = Some((StartFail::Exited(None, None), false));
                break;
            }
        }
        if !announced {
            match listener_state(child.id(), socket) {
                Listener::Ours => {
                    announced = true;
                    let _ = ready.send(Ok(child.id()));
                }
                Listener::Conflict(c) => {
                    failure = Some((StartFail::Conflict(c), false));
                    break;
                }
                Listener::NotYet => {}
            }
            if !announced && started.elapsed() >= READY_TIMEOUT {
                failure = Some((StartFail::Timeout, false));
                break;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    if let Some((fail, ended)) = failure {
        if !ended {
            terminate(&mut child);
        }
        if !announced {
            let _ = ready.send(Err(fail));
        }
    }
    settle(reader);
}

// ------------------------------------------------------------------ session

/// Request a graphical console of the instance and tunnel it to a private
/// Unix socket. Blocks (see the module documentation); `cancel` ends it early
/// with a [`Cancelled`] error. On any error everything has been cleaned up, or
/// is being cleaned up by the helper in the background.
pub fn open(opts: &VncOptions, cancel: &Cancel) -> Result<VncSession> {
    if fs::metadata(PROC_UNIX).is_err() {
        bail!("the graphical console needs {PROC_UNIX} (Linux only)");
    }
    // Before anything is created at the provider.
    let (dir, socket) = private_socket_dir(opts.runtime_dir.as_deref())?;
    let interrupt = || -> Result<()> {
        if cancel.is_cancelled() {
            Err(Cancelled.into())
        } else {
            Ok(())
        }
    };
    let (key, pubkey) = resolve_identity(opts.oci.identity.as_deref())?;
    let cli = OciCli::new(&opts.oci);
    let target = cli.resolve_instance(&opts.oci)?;
    interrupt()?;

    // From here on, whatever happens (also to us), the helper deletes the
    // connection that gets created.
    let mut owner = Supervisor::start(&cli, &target, &pubkey, opts.oci.keep, opts.oci.keep_others)?;
    owner.quiet = true;
    let ctx = Establish {
        cli: &cli,
        target: &target,
        key: &key,
        opts,
        cancel,
    };
    match establish(&ctx, &mut owner, dir, socket, &interrupt) {
        Ok((tunnel, connection)) => Ok(VncSession {
            connection,
            tunnel: Some(tunnel),
            owner: Some(owner),
        }),
        Err(err) => {
            owner.finish(true);
            Err(err)
        }
    }
}

struct Establish<'a> {
    cli: &'a OciCli,
    target: &'a Target,
    key: &'a Path,
    opts: &'a VncOptions,
    cancel: &'a Cancel,
}

fn establish(
    ctx: &Establish,
    owner: &mut Supervisor,
    dir: tempfile::TempDir,
    socket: PathBuf,
    interrupt: &dyn Fn() -> Result<()>,
) -> Result<(Tunnel, String)> {
    let id = owner
        .wait_created_with(interrupt)
        .context("no console connection")?;
    interrupt()?;
    let connstr = ctx
        .cli
        .wait_active_field(&id, "vnc-connection-string", interrupt)?;
    interrupt()?;
    let console = parse_vnc_connection_string(&connstr, &id, &ctx.target.id)?;
    drop(connstr);
    let ssh_bin = ctx
        .opts
        .oci
        .ssh_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("ssh"));
    let args = tunnel_args(&console, &ssh_bin.to_string_lossy(), ctx.key, &socket);
    let tunnel = start_tunnel(&ssh_bin, args, dir, socket, &|| ctx.cancel.is_cancelled())
        .map_err(StartFail::into_error)?;
    Ok((tunnel, id))
}

/// An open graphical console: the tunnel and the connection behind it.
///
/// Get a stream to the VNC server with [`VncSession::connect`], again for a
/// reconnect: the tunnel outlives client connections (the service lets one
/// client in at a time, and frees the slot a few moments after one leaves;
/// one that died uncleanly can hold it for about 5 minutes). Ended with
/// [`close`], or by drop: the tunnel stops, its socket directory goes and the
/// connection is deleted (not with `keep`).
///
/// [`close`]: VncSession::close
pub struct VncSession {
    connection: String,
    tunnel: Option<Tunnel>,
    owner: Option<Supervisor>,
}

impl fmt::Debug for VncSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VncSession")
            .field("connection", &self.connection)
            .finish_non_exhaustive()
    }
}

impl VncSession {
    /// A connected stream to the VNC server of the instance, to be spoken RFB
    /// on (a std stream: it is blocking; for tokio, `set_nonblocking(true)`
    /// and `tokio::net::UnixStream::from_std`).
    ///
    /// This is the console client: it takes the service's one slot (`ssh`
    /// opens the channel to the instance when we connect), so connect when
    /// the viewer is to be shown and not before. A busy slot shows as a
    /// closed stream and in [`diagnosis`](Self::diagnosis).
    ///
    /// The listener is checked first: the socket must be ours, and the peer
    /// of the stream must be our `ssh` (`SO_PEERCRED`).
    pub fn connect(&self) -> io::Result<UnixStream> {
        let tunnel = self
            .tunnel
            .as_ref()
            .filter(|t| !t.shared.exited.load(Ordering::SeqCst))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "the console tunnel ended")
            })?;
        let denied = |what: &str| io::Error::new(io::ErrorKind::PermissionDenied, what.to_string());
        if !socket_is_ours(&tunnel.socket) {
            return Err(denied("the tunnel socket is not a private socket of ours"));
        }
        let stream = UnixStream::connect(&tunnel.socket)?;
        let cred = peer_cred(&stream)?;
        if cred.uid != euid() || u32::try_from(cred.pid).ok() != Some(tunnel.pid) {
            return Err(denied("the tunnel socket is not served by our ssh"));
        }
        Ok(stream)
    }

    /// Where the tunnel listens (a private directory): for display. To talk
    /// to the console use [`connect`](Self::connect), which does the checks.
    #[allow(dead_code)] // part of the module API, not used by the dashboard
    pub fn socket_path(&self) -> Option<&Path> {
        self.tunnel.as_ref().map(|t| t.socket.as_path())
    }

    /// OCID of the console connection
    #[allow(dead_code)] // part of the module API, not used by the dashboard
    pub fn connection_id(&self) -> &str {
        &self.connection
    }

    /// Whether the tunnel is still up (`ssh` ends when the service drops it)
    #[allow(dead_code)] // part of the module API, not used by the dashboard
    pub fn is_alive(&self) -> bool {
        self.tunnel
            .as_ref()
            .is_some_and(|t| !t.shared.exited.load(Ordering::SeqCst))
    }

    /// The latest category of what `ssh` complained about, if any: why a
    /// client gets no picture (a busy console, an instance without a VNC
    /// server) or why the tunnel ended.
    pub fn diagnosis(&self) -> Option<Diagnosis> {
        let t = self.tunnel.as_ref()?;
        *t.shared.diagnosis.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Stop the tunnel (which frees the service's client slot, and removes
    /// the socket) and have the connection deleted. Waits for that (up to a
    /// minute or so).
    #[allow(dead_code)] // part of the module API, not used by the dashboard
    pub fn close(mut self) {
        if let Some(mut tunnel) = self.tunnel.take() {
            tunnel.stop();
        }
        if let Some(mut owner) = self.owner.take() {
            owner.finish(false);
        }
    }
}

impl Drop for VncSession {
    fn drop(&mut self) {
        let (tunnel, owner) = (self.tunnel.take(), self.owner.take());
        if tunnel.is_none() && owner.is_none() {
            return;
        }
        // Not on the caller's thread (it may be a UI): the tunnel goes first,
        // then the connection is deleted (by the helper, whatever happens to
        // this thread: it only ends the wait).
        let _ = thread::Builder::new()
            .name("lsu-oci-vnc-close".into())
            .spawn(move || {
                drop(tunnel);
                if let Some(mut owner) = owner {
                    owner.finish(true);
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oci::RUN_LOCK;
    use crate::oci::tests::{
        CONN, Fixture, HOST, INST, OTHER_INST, eventually, fixture, instances,
    };

    fn good() -> String {
        format!(
            "ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}' -N -L \
             localhost:5900:{INST}:5900 {INST}"
        )
    }

    fn parse(s: &str) -> Result<Console> {
        parse_vnc_connection_string(s, CONN, INST)
    }

    #[test]
    fn parses_the_documented_shape() {
        let c = parse(&good()).unwrap();
        assert_eq!(c.target, INST);
        assert_eq!(c.proxy_user, CONN);
        assert_eq!(c.proxy_host, HOST);
        assert_eq!(c.proxy_port, Some(443));
        // The local port and the loopback spelling are free, -N is optional.
        let other = format!(
            "ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}' -L \
             127.0.0.1:12345:{INST}:5900 {INST}"
        );
        assert!(parse(&other).is_ok());
    }

    #[test]
    fn rejects_anything_else() {
        let proxy = format!("-o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}'");
        let fwd = format!("-N -L localhost:5900:{INST}:5900");
        let bad: Vec<(&str, String)> = vec![
            ("empty", String::new()),
            ("not ssh", format!("scp {proxy} {fwd} {INST}")),
            ("the serial console", format!("ssh {proxy} {INST}")),
            ("no proxy", format!("ssh {fwd} {INST}")),
            ("no destination", format!("ssh {proxy} {fwd}")),
            (
                "two destinations",
                format!("ssh {proxy} {fwd} {INST} {INST}"),
            ),
            (
                "another instance",
                format!("ssh {proxy} {fwd} {OTHER_INST}"),
            ),
            (
                "forward to another host",
                format!("ssh {proxy} -N -L localhost:5900:{OTHER_INST}:5900 {INST}"),
            ),
            (
                "forward to another port",
                format!("ssh {proxy} -N -L localhost:5900:{INST}:22 {INST}"),
            ),
            (
                "forward bound wide",
                format!("ssh {proxy} -N -L 0.0.0.0:5900:{INST}:5900 {INST}"),
            ),
            (
                "forward bound to all interfaces",
                format!("ssh {proxy} -N -L *:5900:{INST}:5900 {INST}"),
            ),
            (
                "forward without a bind address",
                format!("ssh {proxy} -N -L 5900:{INST}:5900 {INST}"),
            ),
            (
                "port 0",
                format!("ssh {proxy} -N -L localhost:0:{INST}:5900 {INST}"),
            ),
            (
                "two forwards",
                format!("ssh {proxy} {fwd} -L localhost:5901:{INST}:5900 {INST}"),
            ),
            ("two -N", format!("ssh {proxy} -N {fwd} {INST}")),
            (
                "a remote forward",
                format!("ssh {proxy} -N -R 5900:{INST}:5900 {INST}"),
            ),
            (
                "a dynamic forward",
                format!("ssh {proxy} {fwd} -D 1080 {INST}"),
            ),
            (
                "another option",
                format!("ssh {proxy} {fwd} -o LocalCommand=id {INST}"),
            ),
            ("a jump host", format!("ssh {proxy} {fwd} -J evil {INST}")),
            (
                "an identity",
                format!("ssh {proxy} {fwd} -i /etc/shadow {INST}"),
            ),
            ("a command", format!("ssh {proxy} {fwd} {INST} id")),
            ("a subshell", format!("ssh {proxy} {fwd} {INST}$(id)")),
            ("a chain", format!("ssh {proxy} {fwd} {INST}; id")),
            ("a backtick", format!("ssh {proxy} {fwd} `id`")),
            ("a redirect", format!("ssh {proxy} {fwd} {INST} > /tmp/x")),
            (
                "another connection",
                format!(
                    "ssh -o ProxyCommand='ssh -W %h:%p -p 443 {}@{HOST}' {fwd} {INST}",
                    crate::oci::tests::FOREIGN
                ),
            ),
            (
                "a proxy host of someone else",
                format!(
                    "ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@evil.example.com' {fwd} {INST}"
                ),
            ),
            (
                "a proxy with more in it",
                format!("ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}; id' {fwd} {INST}"),
            ),
            (
                "an unterminated quote",
                format!("ssh -o ProxyCommand='ssh -W %h:%p {CONN}@{HOST} {fwd} {INST}"),
            ),
        ];
        for (what, s) in &bad {
            assert!(parse(s).is_err(), "accepted ({what}): {s}");
        }
        // Errors say what is wrong, not what the string was.
        let err = parse(&format!("ssh {proxy} {fwd} {INST} SECRETMARK"))
            .unwrap_err()
            .to_string();
        assert!(!err.contains("SECRETMARK"), "{err}");
    }

    #[test]
    fn the_tunnel_command_is_rebuilt_from_validated_parts() {
        let console = parse(&good()).unwrap();
        let key = Path::new("/home/u/.ssh/id rsa");
        let sock = Path::new("/run/user/1000/lsu-vnc-abc123/vnc.sock");
        let args: Vec<String> = tunnel_args(&console, "/usr/bin/ssh", key, sock)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let proxy = format!(
            "ProxyCommand=/usr/bin/ssh -i '/home/u/.ssh/id rsa' -o IdentitiesOnly=yes \
             -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=20 \
             -W %h:%p -p 443 {CONN}@{HOST}"
        );
        let expected: Vec<String> = [
            "-N",
            "-i",
            "/home/u/.ssh/id rsa",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ConnectTimeout=20",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "StreamLocalBindUnlink=no",
            "-o",
            "StreamLocalBindMask=0177",
            "-o",
            "GatewayPorts=no",
            "-o",
            "PermitLocalCommand=no",
            "-o",
            "ControlMaster=no",
            "-o",
            "ControlPath=none",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "-o",
            proxy.as_str(),
            "-L",
            &format!("/run/user/1000/lsu-vnc-abc123/vnc.sock:{INST}:5900"),
            INST,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(args, expected);
        // Without a proxy port the option is left out.
        let mut c = console;
        c.proxy_port = None;
        let args = tunnel_args(&c, "ssh", key, sock);
        assert!(!args.iter().any(|a| a.to_string_lossy().contains(" -p ")));
    }

    const SOCK: &str = "/run/user/1000/lsu-vnc-abc123/vnc.sock";

    /// A `/proc/net/unix` of ROWS: (path, flags, type, state, inode).
    fn table(rows: &[(&str, u32, u32, u32, u64)]) -> String {
        let mut t = String::from("Num       RefCount Protocol Flags    Type St Inode Path\n");
        for (i, (path, flags, kind, st, inode)) in rows.iter().enumerate() {
            t.push_str(&format!(
                "ffff8881{i:08x}: 00000002 00000000 {flags:08X} {kind:04X} {st:02X} {inode} {path}\n"
            ));
        }
        t
    }

    const LISTEN: u32 = 0x1_0000;

    #[test]
    fn listeners_are_read_from_the_kernel_table() {
        let t = table(&[
            (SOCK, LISTEN, 1, 1, 11),
            // Connected, not listening
            ("/run/other.sock", 0, 1, 3, 12),
            // Datagram
            ("/run/dgram.sock", LISTEN, 2, 1, 13),
            ("@abstract", LISTEN, 1, 1, 14),
        ]);
        assert_eq!(
            parse_listeners(&t),
            vec![
                Listening {
                    path: SOCK.into(),
                    inode: 11
                },
                Listening {
                    path: "@abstract".into(),
                    inode: 14
                },
            ]
        );
        // Listeners without a path are skipped.
        let nameless = "Num RefCount Protocol Flags Type St Inode Path\n\
                        ffff0: 00000002 00000000 00010000 0001 01 99\n";
        assert!(parse_listeners(nameless).is_empty());
        assert!(parse_listeners("garbage\nmore garbage\n").is_empty());
        assert!(parse_listeners("").is_empty());
    }

    #[test]
    fn listener_ownership_is_decided_by_inode_and_path() {
        let mine: HashSet<u64> = [11].into();
        let check =
            |rows: &[(&str, u32, u32, u32, u64)]| classify_listeners(&table(rows), &mine, SOCK);
        assert_eq!(check(&[]), Listener::NotYet);
        assert_eq!(check(&[(SOCK, LISTEN, 1, 1, 11)]), Listener::Ours);
        // Someone else's socket alone: our ssh is not up (it will fail).
        assert_eq!(check(&[(SOCK, LISTEN, 1, 1, 99)]), Listener::NotYet);
        // Ours, and someone else's too: not safe.
        assert_eq!(
            check(&[(SOCK, LISTEN, 1, 1, 11), (SOCK, LISTEN, 1, 1, 99)]),
            Listener::Conflict(Conflict::Foreign)
        );
        assert_eq!(
            check(&[(SOCK, LISTEN, 1, 1, 99), (SOCK, LISTEN, 1, 1, 11)]),
            Listener::Conflict(Conflict::Foreign)
        );
        // Another path, or not listening.
        assert_eq!(
            check(&[("/run/user/1000/x/vnc.sock", LISTEN, 1, 1, 11)]),
            Listener::NotYet
        );
        assert_eq!(check(&[(SOCK, 0, 1, 3, 11)]), Listener::NotYet);
    }

    #[test]
    fn a_real_listener_is_ours_by_its_inode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.sock");
        let _l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let path = path.to_str().unwrap();
        let table = fs::read_to_string(PROC_UNIX).unwrap();
        let ours = socket_inodes(std::process::id());
        assert_eq!(classify_listeners(&table, &ours, path), Listener::Ours);
        assert_eq!(
            classify_listeners(&table, &HashSet::new(), path),
            Listener::NotYet
        );
        assert!(socket_inodes(u32::MAX).is_empty());
        // The file is a socket of ours, and is narrowed when it is not.
        let p = Path::new(path);
        fs::set_permissions(p, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(socket_is_ours(p));
        assert_eq!(fs::metadata(p).unwrap().mode() & 0o777, 0o600);
        // A regular file, a missing one and a link are not.
        let file = dir.path().join("f");
        fs::write(&file, "x").unwrap();
        assert!(!socket_is_ours(&file));
        assert!(!socket_is_ours(&dir.path().join("none")));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(p, &link).unwrap();
        assert!(!socket_is_ours(&link));
    }

    #[test]
    fn the_peer_of_a_stream_is_identified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.sock");
        let _l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let s = UnixStream::connect(&path).unwrap();
        let cred = peer_cred(&s).unwrap();
        assert_eq!(cred.uid, euid());
        assert_eq!(cred.pid as u32, std::process::id());
    }

    #[test]
    fn the_socket_directory_is_private() {
        let base = tempfile::tempdir().unwrap();
        let (dir, socket) = private_socket_dir(Some(base.path())).unwrap();
        let md = fs::metadata(dir.path()).unwrap();
        assert_eq!(md.mode() & 0o777, 0o700);
        assert_eq!(md.uid(), euid());
        assert_eq!(socket, dir.path().join("vnc.sock"));
        assert!(socket.starts_with(base.path()));
        let kept = dir.path().to_path_buf();
        drop(dir);
        assert!(!kept.exists(), "the directory is left behind");
        // A base that is not there is an error, not a fallback.
        assert!(private_socket_dir(Some(&base.path().join("none"))).is_err());
        // A relative base is made absolute.
        let rel = Path::new("target");
        if rel.is_dir() {
            let (_d, s) = private_socket_dir(Some(rel)).unwrap();
            assert!(s.is_absolute());
        }
    }

    #[test]
    fn socket_paths_that_ssh_or_the_kernel_would_misread_are_refused() {
        assert!(check_socket_path(Path::new(SOCK)).is_ok());
        for bad in [
            "relative/vnc.sock",
            "/run/user/1000/a:b/vnc.sock",
            "/run/user/1000/a b/vnc.sock",
            "/run/user/1000/a\nb/vnc.sock",
        ] {
            assert!(check_socket_path(Path::new(bad)).is_err(), "{bad}");
        }
        let long = format!("/{}/vnc.sock", "d".repeat(MAX_SOCKET_PATH));
        assert!(check_socket_path(Path::new(&long)).is_err());
        let exact = format!("/{}", "d".repeat(MAX_SOCKET_PATH - 1));
        assert_eq!(exact.len(), MAX_SOCKET_PATH);
        assert!(check_socket_path(Path::new(&exact)).is_ok());
    }

    #[test]
    fn ssh_output_is_reduced_to_a_category() {
        let cases = [
            ("Host key verification failed.", Some(Diagnosis::HostKey)),
            (
                "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!",
                Some(Diagnosis::HostKey),
            ),
            (
                "CONN@host: Permission denied (publickey).",
                Some(Diagnosis::AuthDenied),
            ),
            (
                "bind [127.0.0.1]:5900: Address already in use",
                Some(Diagnosis::PortInUse),
            ),
            (
                "channel 2: open failed: administratively prohibited: console access is \
                 limited to one connection at a time",
                Some(Diagnosis::ConsoleBusy),
            ),
            (
                "channel 3: open failed: connect failed: Connection refused",
                Some(Diagnosis::Unavailable),
            ),
            (
                "ssh_exchange_identification: Connection closed by remote host",
                Some(Diagnosis::Network),
            ),
            (
                "ssh: connect to host x port 443: Connection timed out",
                Some(Diagnosis::Network),
            ),
            ("Warning: Permanently added 'x' to the list", None),
            ("", None),
        ];
        for (line, want) in cases {
            assert_eq!(classify(line), want, "{line}");
        }
    }

    #[test]
    fn stderr_is_watched_without_being_kept() {
        let shared = Shared::default();
        let text = "noise\nchannel 2: open failed: administratively prohibited: x\nmore noise";
        watch_stderr(text.as_bytes(), &shared);
        assert_eq!(
            *shared.diagnosis.lock().unwrap(),
            Some(Diagnosis::ConsoleBusy)
        );
        // A huge line is bounded, not buffered.
        let shared = Shared::default();
        let long = "x".repeat(10 * 1024 * 1024);
        watch_stderr(long.as_bytes(), &shared);
        assert_eq!(*shared.diagnosis.lock().unwrap(), None);
    }

    // ------------------------------------------- fixtures: fake oci and ssh

    /// The test that the fake `ssh` runs as: it listens on the socket (or
    /// fails to) like `ssh -L` does, greets whoever connects, and keeps doing
    /// so until it is killed. A no-op in a normal test run.
    #[test]
    fn fake_listener() {
        use std::io::Write;
        use std::os::unix::net::UnixListener;
        let Ok(path) = std::env::var("LSU_FAKE_LISTEN_SOCKET") else {
            return;
        };
        match UnixListener::bind(&path) {
            Ok(l) => {
                if std::env::var_os("LSU_FAKE_REPLACE").is_some() {
                    // Someone swaps the socket for a plain file.
                    fs::remove_file(&path).unwrap();
                    fs::write(&path, "x").unwrap();
                    thread::sleep(Duration::from_secs(600));
                }
                for conn in l.incoming().flatten() {
                    let mut conn = conn;
                    let _ = conn.write_all(b"RFB 003.008\n");
                    // Held until the process ends.
                    std::mem::forget(conn);
                }
            }
            Err(_) => {
                eprintln!("Could not request local forwarding: Address already in use");
                std::process::exit(255);
            }
        }
    }

    /// Shell for the fake ssh: record the pid and the call, find the socket
    /// of the forward, then PRELUDE (shell, may `exit`), then be the listener
    /// of the socket, with the environment ENV (`NAME=value` words).
    fn listening_ssh(fx_dir: &Path, env: &str, prelude: &str) -> String {
        let d = fx_dir.display();
        let exe = std::env::current_exe().unwrap();
        format!(
            r#"echo $$ > '{d}/ssh.pid'
echo call >> '{d}/ssh.calls'
for a in "$@"; do case "$a" in /*:*:5900) spec=$a ;; esac; done
socket=${{spec%%:*}}
{prelude}
LSU_FAKE_LISTEN_SOCKET="$socket" {env} exec '{exe}' --exact \
  oci::vnc::tests::fake_listener --test-threads=1 --nocapture
"#,
            exe = exe.display()
        )
    }

    /// A fixture whose connection answers with the graphical string. The ssh
    /// script has to know the fixture's directory, which only exists after
    /// the fixture: the script is replaced.
    fn vnc_fixture(connstr: &str, ssh: impl Fn(&Path) -> String) -> Fixture {
        let fx = fixture("unused", instances(), "exit 0");
        let json = serde_json::json!({"data": {
            "id": CONN, "lifecycle-state": "ACTIVE", "vnc-connection-string": connstr
        }});
        fx.write("conn.json", &json.to_string());
        write_ssh(&fx, &ssh(fx.dir.path()));
        fx
    }

    fn write_ssh(fx: &Fixture, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        // Not while another test forks (see `fixture`).
        let _lock = RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = fx.path("ssh");
        let tmp = fx.path("ssh.new");
        let d = fx.dir.path().display();
        fs::write(
            &tmp,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/ssh.args'\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, &path).unwrap();
    }

    fn vnc_opts(fx: &Fixture) -> VncOptions {
        VncOptions {
            oci: fx.opts.clone(),
            runtime_dir: None,
        }
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn ssh_pid(fx: &Fixture) -> Option<u32> {
        fs::read_to_string(fx.path("ssh.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    fn gone(pid: u32) -> bool {
        eventually(|| !Path::new(&format!("/proc/{pid}")).exists())
    }

    fn calls(fx: &Fixture) -> usize {
        fs::read_to_string(fx.path("ssh.calls"))
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn open_check_close() {
        let fx = vnc_fixture(&good(), |d| listening_ssh(d, "", ""));
        let _lock = lock();
        let session = open(&vnc_opts(&fx), &Cancel::new()).unwrap();
        let socket = session.socket_path().unwrap().to_path_buf();
        let dir = socket.parent().unwrap().to_path_buf();
        assert_eq!(session.connection_id(), CONN);
        assert!(session.is_alive());
        assert_eq!(session.diagnosis(), None);
        let pid = ssh_pid(&fx).expect("ssh did not run");
        // The socket is in a directory of ours that nobody else can enter,
        // and closed to everyone else itself.
        let md = fs::metadata(&dir).unwrap();
        assert_eq!(md.mode() & 0o777, 0o700);
        assert_eq!(md.uid(), euid());
        let md = fs::symlink_metadata(&socket).unwrap();
        assert!(md.file_type().is_socket());
        assert_eq!(md.mode() & 0o077, 0);
        // The command line is ours, not the string's.
        let args = fx.ssh_args().unwrap();
        assert!(args.contains(&format!("{}:{INST}:5900", socket.display())));
        assert!(args.contains(&"StreamLocalBindUnlink=no".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("127.0.0.1:")));
        assert_eq!(fx.deletes().len(), 0);
        assert_eq!(calls(&fx), 1);
        // No client was connected so far: a probe would have used the slot.
        session.close();
        assert_eq!(fx.deletes().len(), 1, "{:?}", fx.log());
        assert!(gone(pid), "ssh {pid} is still there");
        assert!(!dir.exists(), "the socket directory is left behind");
    }

    #[test]
    fn connect_hands_out_a_stream_to_our_ssh() {
        use std::io::Read;
        let fx = vnc_fixture(&good(), |d| listening_ssh(d, "", ""));
        let _lock = lock();
        let session = open(&vnc_opts(&fx), &Cancel::new()).unwrap();
        for _ in 0..2 {
            // More than once: a viewer can reconnect.
            let mut stream = session.connect().unwrap();
            let mut banner = [0u8; 12];
            stream.read_exact(&mut banner).unwrap();
            assert_eq!(&banner, b"RFB 003.008\n");
        }
        let pid = ssh_pid(&fx).unwrap();
        // SAFETY: ending the fake ssh, as the service dropping us would.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        assert!(eventually(|| !session.is_alive()));
        let err = session.connect().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotConnected);
        session.close();
    }

    #[test]
    fn a_listener_that_is_not_our_ssh_is_not_connected_to() {
        let fx = vnc_fixture(&good(), |d| listening_ssh(d, "", ""));
        let _lock = lock();
        let mut session = open(&vnc_opts(&fx), &Cancel::new()).unwrap();
        // The tunnel claims another process as its ssh: whoever listens is a
        // stranger to it.
        session.tunnel.as_mut().unwrap().pid += 1;
        let err = session.connect().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        session.tunnel.as_mut().unwrap().pid -= 1;
        assert!(session.connect().is_ok());
        session.close();
    }

    #[test]
    fn dropping_a_session_cleans_up_too() {
        let fx = vnc_fixture(&good(), |d| listening_ssh(d, "", ""));
        let _lock = lock();
        let session = open(&vnc_opts(&fx), &Cancel::new()).unwrap();
        let pid = ssh_pid(&fx).unwrap();
        let dir = session
            .socket_path()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        drop(session);
        assert!(gone(pid), "ssh {pid} is still there");
        assert!(eventually(|| !dir.exists()), "the socket directory is left");
        assert!(eventually(|| fx.deletes().len() == 1), "{:?}", fx.log());
    }

    #[test]
    fn the_runtime_directory_is_chosen_by_the_caller() {
        let base = tempfile::tempdir().unwrap();
        let fx = vnc_fixture(&good(), |d| listening_ssh(d, "", ""));
        let _lock = lock();
        let mut opts = vnc_opts(&fx);
        opts.runtime_dir = Some(base.path().to_path_buf());
        let session = open(&opts, &Cancel::new()).unwrap();
        assert!(session.socket_path().unwrap().starts_with(base.path()));
        session.close();
        assert_eq!(fs::read_dir(base.path()).unwrap().count(), 0);
        // One that cannot be used fails before anything is made at the
        // provider.
        let creates = fx.log().len();
        opts.runtime_dir = Some(base.path().join("missing"));
        assert!(open(&opts, &Cancel::new()).is_err());
        assert_eq!(fx.log().len(), creates);
    }

    #[test]
    fn a_socket_swapped_for_something_else_is_refused() {
        let fx = vnc_fixture(&good(), |d| listening_ssh(d, "LSU_FAKE_REPLACE=1", ""));
        let _lock = lock();
        let err = format!("{:#}", open(&vnc_opts(&fx), &Cancel::new()).unwrap_err());
        assert!(err.contains("not a private socket of ours"), "{err}");
        let pid = ssh_pid(&fx).unwrap();
        assert!(gone(pid), "ssh {pid} is still there");
        assert!(eventually(|| fx.deletes().len() == 1));
    }

    #[test]
    fn a_hostile_string_is_refused_and_ssh_never_runs() {
        let evil = format!(
            "ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}' -N -L \
             localhost:5900:{INST}:5900 {INST}; id"
        );
        let fx = vnc_fixture(&evil, |d| listening_ssh(d, "", ""));
        let _lock = lock();
        let err = open(&vnc_opts(&fx), &Cancel::new()).unwrap_err();
        assert!(format!("{err:#}").contains("connection string"), "{err:#}");
        assert!(fx.ssh_args().is_none(), "ssh ran");
        assert!(eventually(|| fx.deletes().len() == 1), "{:?}", fx.log());
    }

    #[test]
    fn a_serial_string_is_not_a_graphical_one() {
        let serial = format!("ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}' {INST}");
        let fx = vnc_fixture(&serial, |d| listening_ssh(d, "", ""));
        let _lock = lock();
        assert!(open(&vnc_opts(&fx), &Cancel::new()).is_err());
        assert!(fx.ssh_args().is_none());
        assert!(eventually(|| fx.deletes().len() == 1));
    }

    #[test]
    fn a_missing_vnc_string_is_reported_and_cleaned_up() {
        let fx = vnc_fixture(&good(), |d| listening_ssh(d, "", ""));
        let json = serde_json::json!({"data": {
            "id": CONN, "lifecycle-state": "ACTIVE", "connection-string": "ssh x"
        }});
        fx.write("conn.json", &json.to_string());
        let _lock = lock();
        let err = open(&vnc_opts(&fx), &Cancel::new()).unwrap_err();
        assert!(
            format!("{err:#}").contains("vnc-connection-string"),
            "{err:#}"
        );
        assert!(fx.ssh_args().is_none());
        assert!(eventually(|| fx.deletes().len() == 1));
    }

    #[test]
    fn ssh_failures_are_explained_without_its_text() {
        let denied = vnc_fixture(&good(), |_| {
            "echo 'CONN@host: Permission denied (publickey). SECRETMARK' >&2; exit 255".into()
        });
        let strange = vnc_fixture(&good(), |_| "echo 'strange SECRETMARK' >&2; exit 3".into());
        let _lock = lock();
        let err = format!(
            "{:#}",
            open(&vnc_opts(&denied), &Cancel::new()).unwrap_err()
        );
        assert!(err.contains("refused the SSH key"), "{err}");
        assert!(!err.contains("SECRETMARK"), "{err}");
        assert!(eventually(|| denied.deletes().len() == 1));
        // An unknown failure still says that ssh ended.
        let err = format!(
            "{:#}",
            open(&vnc_opts(&strange), &Cancel::new()).unwrap_err()
        );
        assert!(err.contains("exit 3"), "{err}");
        assert!(!err.contains("SECRETMARK"), "{err}");
    }

    #[test]
    fn a_taken_socket_is_a_failure_not_a_retry() {
        let fx = vnc_fixture(&good(), |d| {
            // What ssh says when the socket exists (StreamLocalBindUnlink=no).
            let prelude = "echo 'Could not request local forwarding: Address already in \
                           use' >&2; exit 255";
            listening_ssh(d, "", prelude)
        });
        let _lock = lock();
        let err = format!("{:#}", open(&vnc_opts(&fx), &Cancel::new()).unwrap_err());
        assert!(err.contains("already in use"), "{err}");
        assert_eq!(calls(&fx), 1);
        assert!(eventually(|| fx.deletes().len() == 1));
    }

    #[test]
    fn cancelling_while_the_connection_is_made() {
        let fx = vnc_fixture(&good(), |d| listening_ssh(d, "", ""));
        let json = serde_json::json!({"data": {
            "id": CONN, "lifecycle-state": "CREATING"
        }});
        fx.write("conn.json", &json.to_string());
        let _lock = lock();
        let cancel = Cancel::new();
        let c2 = cancel.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(600));
            c2.cancel();
        });
        let err = open(&vnc_opts(&fx), &cancel).unwrap_err();
        canceller.join().unwrap();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
        assert!(fx.ssh_args().is_none());
        assert!(eventually(|| fx.deletes().len() == 1), "{:?}", fx.log());
    }

    #[test]
    fn cancelling_while_the_tunnel_comes_up() {
        let fx = vnc_fixture(&good(), |d| {
            format!("echo $$ > '{}/ssh.pid'\nexec sleep 600", d.display())
        });
        let _lock = lock();
        let cancel = Cancel::new();
        let (c2, dir) = (cancel.clone(), fx.dir.path().to_path_buf());
        let canceller = thread::spawn(move || {
            let up = eventually(|| dir.join("ssh.pid").exists());
            thread::sleep(Duration::from_millis(200));
            c2.cancel();
            up
        });
        let err = open(&vnc_opts(&fx), &cancel).unwrap_err();
        assert!(canceller.join().unwrap(), "ssh did not start");
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
        let pid = ssh_pid(&fx).unwrap();
        assert!(gone(pid), "ssh {pid} is still there");
        assert!(eventually(|| fx.deletes().len() == 1), "{:?}", fx.log());
    }

    #[test]
    fn a_dropped_tunnel_shows_in_the_session() {
        let fx = vnc_fixture(&good(), |d| {
            let prelude = "(sleep 1; echo 'channel 2: open failed: administratively \
                           prohibited: console access is limited' >&2) &";
            listening_ssh(d, "", prelude)
        });
        let _lock = lock();
        let session = open(&vnc_opts(&fx), &Cancel::new()).unwrap();
        // ssh says that a client was turned away: the viewer sees no picture
        // and the caller can tell it why.
        assert!(
            eventually(|| session.diagnosis() == Some(Diagnosis::ConsoleBusy)),
            "{:?}",
            session.diagnosis()
        );
        assert!(session.is_alive());
        let pid = ssh_pid(&fx).unwrap();
        // SAFETY: ending the fake ssh, as the service dropping us would.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        assert!(eventually(|| !session.is_alive()));
        session.close();
        assert_eq!(fx.deletes().len(), 1);
    }
}
