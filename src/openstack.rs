//! Native OpenStack console acquisition and WebSocket transport.
//!
//! A Nova console is reached in two steps:
//!
//! 1. The `openstack` client asks the compute API for a console URL
//!    (`openstack console url show --novnc|--serial SERVER`). The URL carries a
//!    short-lived, single-use token: it is a credential.
//! 2. This module opens a WebSocket to the console proxy named by that URL and
//!    hands the caller an in-process byte stream. For a VNC console the bytes are
//!    the RFB protocol (feed the stream to the VNC connector); for a serial
//!    console they are the raw terminal bytes.
//!
//! There is no local listener: the stream is a [`tokio::io::DuplexStream`] pumped
//! by a task, so no other local user can race the session.
//!
//! # Security model
//!
//! * The token URL lives in [`Zeroizing`] memory behind types whose `Debug` is
//!   redacted. It is never put on a command line, in a file, in a log or in an
//!   error. [`Error`] holds only fixed strings, status codes and enums; output of
//!   the `openstack` client and of the proxy is reduced to such a classification
//!   and never echoed, not even redacted.
//! * [`parse_target`] ports the same-origin check of the `console-tui` bridge:
//!   the WebSocket host, port and scheme must equal those of the console URL, so
//!   a `path=//elsewhere/...` parameter cannot redirect the session. No userinfo,
//!   no fragment, bounded length, no control characters.
//! * `wss` is verified against the bundled web PKI roots plus an optional
//!   `ca_file` (for private CAs). There is no switch that disables verification.
//!   Plain `ws` to a non-loopback host needs [`Options::allow_insecure`].
//! * No redirects (a non-101 answer is an error carrying its status code only),
//!   no proxies, no compression, bounded frame and message sizes, binary frames
//!   only (unless [`Options::allow_text_frames`]), bounded handshake time.
//! * The `openstack` client runs with a bounded output, a timeout and
//!   kill-on-drop. When the dashboard runs as root through sudo,
//!   [`RunAs::from_sudo_env`] makes it run as the invoking user, so root neither
//!   reads nor needs the user's cloud credentials.
//!
//! Zeroizing is best effort: the WebSocket library keeps its own copy of the
//! request while the handshake runs.

use std::ffi::OsStr;
use std::io::{self, Read};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use url::{Host, Url};
use zeroize::Zeroizing;

/// The longest console URL accepted. Real ones are a few hundred bytes.
const MAX_URL: usize = 4096;
/// The most the `openstack` client may print on stdout (one URL) or stderr.
const MAX_CLI_OUTPUT: u64 = 64 * 1024;
/// A frame or message larger than this closes the session. websockify forwards
/// what it reads from the guest in small chunks; this leaves room for a full
/// raw framebuffer update.
const MAX_FRAME: usize = 4 * 1024 * 1024;
const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// A PEM bundle bigger than this is not a CA file.
const MAX_CA_FILE: u64 = 1024 * 1024;
/// Buffer between the pump and the consumer, and chunk size towards the proxy.
const PIPE_BUFFER: usize = 64 * 1024;
/// How long the closing handshake may take.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// The subprotocol websockify (novnc proxy, serial proxy) expects.
/// The most of an HTTP response head to read during the handshake.
const MAX_RESPONSE_HEAD: usize = 16 * 1024;
const SUBPROTOCOL: &str = "binary";

// ------------------------------------------------------------------- errors

/// Why running or reading the `openstack` client failed. Derived from the exit
/// status and a classification of stderr; no text of either is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliFailure {
    /// The client could not be started (not installed?).
    Spawn {
        not_found: bool,
    },
    Timeout,
    /// More output than any console URL needs.
    TooMuchOutput,
    /// The cloud does not offer this console type for the server (for a serial
    /// console, `[serial_console] enabled` is off or the guest has none).
    Unavailable,
    /// The server is not in a state that allows a console (HTTP 409).
    NotReady,
    Unauthorized,
    Forbidden,
    ServerNotFound,
    /// The named cloud is not configured.
    CloudNotFound,
    /// Any other failure; the exit code is the only detail.
    Exit(Option<i32>),
}

impl std::fmt::Display for CliFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { not_found: true } => {
                f.write_str("the openstack client is not installed (or not in PATH)")
            }
            Self::Spawn { not_found: false } => f.write_str("cannot run the openstack client"),
            Self::Timeout => f.write_str("the openstack client timed out"),
            Self::TooMuchOutput => f.write_str("the openstack client printed too much"),
            Self::Unavailable => f.write_str(
                "the cloud offers no such console for this server \
                 (serial consoles are often disabled)",
            ),
            Self::NotReady => f.write_str("the server is not ready for a console (HTTP 409)"),
            Self::Unauthorized => f.write_str("OpenStack authentication failed (HTTP 401)"),
            Self::Forbidden => f.write_str("OpenStack refused the console request (HTTP 403)"),
            Self::ServerNotFound => f.write_str("the OpenStack server was not found"),
            Self::CloudNotFound => f.write_str("the OpenStack cloud is not configured"),
            Self::Exit(Some(code)) => write!(
                f,
                "the openstack client failed (exit {code}; run it by hand to see why)"
            ),
            Self::Exit(None) => f.write_str("the openstack client was killed"),
        }
    }
}

/// Every message is a fixed string or a number: never a URL, a token, a
/// certificate detail or text from the cloud.
#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid OpenStack console setting: {0}")]
    Setting(&'static str),
    #[error("{0}")]
    Cli(CliFailure),
    #[error("the console URL is not usable: {0}")]
    Url(&'static str),
    #[error(
        "refusing an unencrypted console connection to a non-loopback host \
         (set allow_insecure to override)"
    )]
    Insecure,
    #[error("cannot use the CA file: {0}")]
    CaFile(&'static str),
    #[error("cannot reach the console proxy ({0})")]
    Connect(&'static str),
    #[error("the console proxy refused the connection (HTTP {0})")]
    Http(u16),
    #[error("the console connection failed: {0}")]
    Protocol(&'static str),
    #[error("cancelled")]
    Cancelled,
}

// ----------------------------------------------------------- configuration

/// The kind of console to open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The graphical console: RFB over WebSocket (noVNC proxy).
    Vnc,
    /// The serial console: raw bytes over WebSocket (nova-serialproxy).
    Serial,
}

impl Kind {
    fn flag(self) -> &'static str {
        match self {
            Self::Vnc => "--novnc",
            Self::Serial => "--serial",
        }
    }
}

/// An identity for the `openstack` child to run as (see [`RunAs::from_sudo_env`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAs {
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

impl RunAs {
    /// When this process is root because of sudo, the user who ran sudo
    /// (`SUDO_UID`/`SUDO_GID`, home from the passwd database); otherwise
    /// `None`. Root itself is never returned.
    pub fn from_sudo_env() -> Option<Self> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return None;
        }
        let uid = std::env::var("SUDO_UID").ok()?;
        let gid = std::env::var("SUDO_GID").ok()?;
        Self::lookup(parse_id(&uid)?, parse_id(&gid)?)
    }

    /// UID and GID with the home directory from the passwd database. `None`
    /// for root, or when there is no passwd entry.
    pub fn lookup(uid: u32, gid: u32) -> Option<Self> {
        if uid == 0 || gid == 0 {
            return None;
        }
        let mut buf = vec![0u8; 4096];
        loop {
            // SAFETY: an all-zero passwd is a valid out-parameter for getpwuid_r.
            let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            // SAFETY: pwd, buf and found are valid for the call; buf's length
            // is passed. The result is only read while buf is alive.
            let rc = unsafe {
                libc::getpwuid_r(
                    uid,
                    &mut pwd,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    &mut found,
                )
            };
            if rc == libc::ERANGE && buf.len() < 1 << 20 {
                buf.resize(buf.len() * 2, 0);
                continue;
            }
            if rc != 0 || found.is_null() || pwd.pw_dir.is_null() {
                return None;
            }
            // SAFETY: pw_dir points into buf and is NUL-terminated.
            let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
            let dir = dir.to_str().ok()?;
            return (!dir.is_empty()).then(|| Self {
                uid,
                gid,
                home: PathBuf::from(dir),
            });
        }
    }
}

fn parse_id(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 10 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// Everything needed to open the console of one server.
#[derive(Debug, Clone)]
pub struct Options {
    /// Server name or ID. Must not start with `-` (it would be an option).
    pub server: String,
    /// `--os-cloud` (an entry of `clouds.yaml`); `OS_*` variables apply otherwise.
    pub cloud: Option<String>,
    /// Sets `OS_CLIENT_CONFIG_FILE` for the client only.
    pub clouds_file: Option<PathBuf>,
    /// Extra PEM CA certificates to trust for `wss` (a private CA of the
    /// cloud's console proxy). Added to the web PKI roots, never replacing them.
    pub ca_file: Option<PathBuf>,
    /// Allow plain `ws://` to a host that is not loopback.
    pub allow_insecure: bool,
    /// Accept text frames as data. Off: the bridge this replaces treated one as
    /// fatal, and RFB never uses them.
    pub allow_text_frames: bool,
    /// The client binary (default: `openstack` from PATH). For tests and for
    /// wrappers.
    pub openstack_bin: Option<PathBuf>,
    /// Run the client as this user (see [`RunAs::from_sudo_env`]).
    pub run_as: Option<RunAs>,
    /// How long the client may take to produce the URL.
    pub cli_timeout: Duration,
    /// How long TCP, TLS and the WebSocket handshake may take together.
    pub connect_timeout: Duration,
}

impl Options {
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            cloud: None,
            clouds_file: None,
            ca_file: None,
            allow_insecure: false,
            allow_text_frames: false,
            openstack_bin: None,
            run_as: None,
            cli_timeout: Duration::from_secs(60),
            connect_timeout: Duration::from_secs(15),
        }
    }

    /// Reject values that could be taken for options or that carry control
    /// characters. Run by [`fetch_url`].
    pub fn validate(&self) -> Result<(), Error> {
        check_word(&self.server, "server", 255)?;
        if let Some(cloud) = &self.cloud {
            check_word(cloud, "cloud", 128)?;
        }
        Ok(())
    }
}

fn check_word(value: &str, what: &'static str, max: usize) -> Result<(), Error> {
    let err = |m| Err(Error::Setting(m));
    if value.is_empty() {
        return err(match what {
            "server" => "the server is empty",
            _ => "the cloud is empty",
        });
    }
    if value.starts_with('-') {
        return err(match what {
            "server" => "the server must not start with '-'",
            _ => "the cloud must not start with '-'",
        });
    }
    if value.len() > max || value.chars().any(char::is_control) {
        return err(match what {
            "server" => "the server is too long or has control characters",
            _ => "the cloud is too long or has control characters",
        });
    }
    Ok(())
}

// ------------------------------------------------------------ secret holders

/// The console URL as printed by the client. A bearer credential.
pub struct ConsoleUrl(Zeroizing<String>);

impl ConsoleUrl {
    /// Wrap a URL obtained elsewhere (tests, other acquisition paths).
    pub fn new(url: impl Into<String>) -> Self {
        Self(Zeroizing::new(url.into()))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ConsoleUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConsoleUrl(<redacted>)")
    }
}

/// A validated WebSocket destination. The URL (with the token) is secret; the
/// rest is not.
pub struct Target {
    /// Request target (path and query): carries the token.
    uri: Zeroizing<String>,
    origin: String,
    host: String,
    port: u16,
    secure: bool,
}

impl Target {
    /// `http(s)://host[:port]` of the console, sent as the `Origin` header.
    #[cfg(test)]
    pub fn origin(&self) -> &str {
        &self.origin
    }

    #[cfg(test)]
    fn ws_url(&self) -> String {
        let authority = self.origin.split_once("://").map_or("", |(_, a)| a);
        let scheme = if self.secure { "wss" } else { "ws" };
        format!("{scheme}://{authority}{}", *self.uri)
    }

    /// Whether the connection is TLS protected.
    #[cfg(test)]
    pub fn is_encrypted(&self) -> bool {
        self.secure
    }
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("url", &"<redacted>")
            .field("origin", &self.origin)
            .field("secure", &self.secure)
            .finish()
    }
}

// --------------------------------------------------------------- URL parsing

/// Validate RAW (what `openstack console url show -f value -c url` printed) and
/// derive the WebSocket destination.
///
/// * `ws://` and `wss://` URLs (serial consoles) are used as they are.
/// * `http(s)://` noVNC page URLs name the WebSocket in their `path` query
///   parameter (`vnc_lite.html?path=websockify%3Ftoken%3D...`) or carry only a
///   `token` (`vnc_auto.html?token=...`, then the path is `websockify`). The
///   scheme becomes `ws(s)`.
/// * Whatever the form, the WebSocket must have the page's scheme class, host
///   and port, and a non-empty `token` query parameter.
pub fn parse_target(raw: &ConsoleUrl, allow_insecure: bool) -> Result<Target, Error> {
    let bad = Error::Url;
    let raw = raw.as_str().trim();
    if raw.is_empty()
        || raw.len() > MAX_URL
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(bad("malformed"));
    }
    let page = Url::parse(raw).map_err(|_| bad("malformed"))?;
    let secure = match page.scheme() {
        "https" | "wss" => true,
        "http" | "ws" => false,
        _ => return Err(bad("unsupported scheme")),
    };
    if page.host().is_none() {
        return Err(bad("no host"));
    }
    if !page.username().is_empty() || page.password().is_some() || page.fragment().is_some() {
        return Err(bad("unsupported URL parts"));
    }
    let host_text = page.host_str().ok_or_else(|| bad("no host"))?;
    let authority = match page.port() {
        Some(port) => format!("{host_text}:{port}"),
        None => host_text.to_string(),
    };
    let ws_scheme = if secure { "wss" } else { "ws" };

    let target = if matches!(page.scheme(), "ws" | "wss") {
        page.clone()
    } else {
        let path = page
            .query_pairs()
            .find(|(k, _)| k == "path")
            .map(|(_, v)| v.into_owned())
            .filter(|v| !v.is_empty());
        let path = match path {
            Some(p) => p,
            None => {
                let token = page
                    .query_pairs()
                    .find(|(k, v)| k == "token" && !v.is_empty())
                    .ok_or_else(|| bad("neither a path nor a token"))?
                    .1;
                let mut query = url::form_urlencoded::Serializer::new(String::new());
                query.append_pair("token", &token);
                format!("websockify?{}", query.finish())
            }
        };
        let base =
            Url::parse(&format!("{ws_scheme}://{authority}/")).map_err(|_| bad("malformed"))?;
        base.join(&path).map_err(|_| bad("malformed path"))?
    };

    // Same origin, whatever `path` said (`//other/`, `\\other`, absolute URLs).
    if target.scheme() != ws_scheme
        || target.host() != page.host()
        || target.port_or_known_default() != page.port_or_known_default()
        || !target.username().is_empty()
        || target.password().is_some()
        || target.fragment().is_some()
    {
        return Err(bad("the WebSocket is not on the console origin"));
    }
    if !target
        .query_pairs()
        .any(|(k, v)| k == "token" && !v.is_empty())
    {
        return Err(bad("no token"));
    }
    if target.as_str().len() > MAX_URL {
        return Err(bad("malformed"));
    }

    let (host, loopback) = match target.host() {
        Some(Host::Domain(d)) => (d.to_string(), d.eq_ignore_ascii_case("localhost")),
        Some(Host::Ipv4(a)) => (a.to_string(), a.is_loopback() || a == Ipv4Addr::LOCALHOST),
        Some(Host::Ipv6(a)) => (a.to_string(), a.is_loopback() || a == Ipv6Addr::LOCALHOST),
        None => return Err(bad("no host")),
    };
    if !secure && !loopback && !allow_insecure {
        return Err(Error::Insecure);
    }
    let port = target
        .port_or_known_default()
        .ok_or_else(|| bad("no port"))?;
    Ok(Target {
        uri: Zeroizing::new(match target.query() {
            Some(q) => format!("{}?{q}", target.path()),
            None => target.path().to_string(),
        }),
        origin: format!("{}://{authority}", if secure { "https" } else { "http" }),
        host,
        port,
        secure,
    })
}

// ------------------------------------------------------- the openstack client

/// Ask the cloud for a fresh console URL of the server. Every call returns a
/// new single-use token; request one per connection attempt and never cache it.
///
/// Runs `openstack [--os-cloud C] console url show --novnc|--serial -f value
/// -c url SERVER` on a blocking thread. Dropping the future kills the client.
pub async fn fetch_url(opts: &Options, kind: Kind) -> Result<ConsoleUrl, Error> {
    opts.validate()?;
    let mut cmd = Command::new(
        opts.openstack_bin
            .as_deref()
            .unwrap_or(Path::new("openstack")),
    );
    if let Some(cloud) = &opts.cloud {
        cmd.arg("--os-cloud").arg(cloud);
    }
    cmd.args([
        "console",
        "url",
        "show",
        kind.flag(),
        "-f",
        "value",
        "-c",
        "url",
    ])
    .arg(&opts.server);
    if let Some(file) = &opts.clouds_file {
        cmd.env("OS_CLIENT_CONFIG_FILE", file);
    }
    if let Some(run_as) = &opts.run_as {
        drop_privileges(&mut cmd, run_as);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    let timeout = opts.cli_timeout;
    let out = tokio::task::spawn_blocking(move || run_cli(cmd, timeout, &cancel))
        .await
        .map_err(|_| Error::Cancelled)??;
    let text = String::from_utf8(out)
        .map_err(|_| Error::Url("not text"))
        .map(Zeroizing::new)?;
    let url = text.trim();
    if url.is_empty() || url.contains(char::is_whitespace) {
        return Err(Error::Url("the client printed no single URL"));
    }
    Ok(ConsoleUrl::new(url))
}

/// The last LINES lines of the server's console log (`openstack [--os-cloud C]
/// console log show --lines N SERVER`), as text. For servers without a serial
/// console: what the kernel and the boot print, polled.
pub async fn fetch_log(opts: &Options, lines: u16) -> Result<String, Error> {
    opts.validate()?;
    let mut cmd = Command::new(
        opts.openstack_bin
            .as_deref()
            .unwrap_or(Path::new("openstack")),
    );
    if let Some(cloud) = &opts.cloud {
        cmd.arg("--os-cloud").arg(cloud);
    }
    cmd.args(["console", "log", "show", "--lines"])
        .arg(lines.to_string())
        .arg(&opts.server);
    if let Some(file) = &opts.clouds_file {
        cmd.env("OS_CLIENT_CONFIG_FILE", file);
    }
    if let Some(run_as) = &opts.run_as {
        drop_privileges(&mut cmd, run_as);
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let _guard = CancelOnDrop(cancel.clone());
    let timeout = opts.cli_timeout;
    let out = tokio::task::spawn_blocking(move || run_cli(cmd, timeout, &cancel))
        .await
        .map_err(|_| Error::Cancelled)??;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The lines of NEXT that come after what PREV already showed: the console log
/// is fetched as a window of its last lines, so consecutive windows overlap.
/// When they do not (much new output, or a restarted log), all of NEXT is new.
pub fn new_log_lines<'a>(prev: &[&str], next: &'a [&'a str]) -> &'a [&'a str] {
    if prev.is_empty() {
        return next;
    }
    // The longest tail of PREV that is a prefix-aligned run inside NEXT.
    for start in 0..prev.len() {
        let tail = &prev[start..];
        if let Some(at) = next.windows(tail.len()).position(|w| w == tail)
            && (at == 0 || start == 0 || tail.len() >= 3)
        {
            return &next[at + tail.len()..];
        }
    }
    next
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Make CMD run as RUN_AS with that user's HOME and without inherited XDG
/// directories (they would point at root's).
fn drop_privileges(cmd: &mut Command, run_as: &RunAs) {
    cmd.env("HOME", &run_as.home);
    for var in ["XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME"] {
        cmd.env_remove(var);
    }
    let (uid, gid) = (run_as.uid, run_as.gid);
    // SAFETY: the closure runs between fork and exec and only makes the async
    // signal safe calls setgroups, setgid and setuid, with plain integers.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn read_bounded<R: Read + Send + 'static>(mut r: R) -> thread::JoinHandle<(Vec<u8>, bool)> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut r).take(MAX_CLI_OUTPUT + 1).read_to_end(&mut buf);
        let over = buf.len() as u64 > MAX_CLI_OUTPUT;
        // Keep draining so the client never blocks on a full pipe.
        let _ = io::copy(&mut r, &mut io::sink());
        buf.truncate(MAX_CLI_OUTPUT as usize);
        (buf, over)
    })
}

/// Run CMD to the end and return its stdout. The failure is classified from
/// the exit status and stderr; neither is kept.
fn run_cli(mut cmd: Command, timeout: Duration, cancel: &AtomicBool) -> Result<Vec<u8>, Error> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        Error::Cli(CliFailure::Spawn {
            not_found: e.kind() == io::ErrorKind::NotFound,
        })
    })?;
    let stdout = child.stdout.take().map(read_bounded);
    let stderr = child.stderr.take().map(read_bounded);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => return Err(Error::Cli(CliFailure::Spawn { not_found: false })),
        }
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Cancelled);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Cli(CliFailure::Timeout));
        }
        thread::sleep(Duration::from_millis(20));
    };
    let (out, out_over) = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    let (err, _) = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    if !status.success() {
        let err = Zeroizing::new(String::from_utf8_lossy(&err).into_owned());
        return Err(Error::Cli(classify_failure(&err, status.code())));
    }
    if out_over {
        return Err(Error::Cli(CliFailure::TooMuchOutput));
    }
    Ok(out)
}

/// Reduce the stderr of a failed client to a [`CliFailure`]. Only keywords and
/// the `(HTTP nnn)` suffix of the API error are looked at; nothing is kept.
fn classify_failure(stderr: &str, code: Option<i32>) -> CliFailure {
    let s = stderr.to_ascii_lowercase();
    let http = s.find("(http ").and_then(|at| {
        let digits: String = s[at + 6..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse::<u16>().ok()
    });
    if s.contains("console type") && (s.contains("unavailable") || s.contains("not available"))
        || s.contains("consoletypeunavailable")
    {
        return CliFailure::Unavailable;
    }
    if s.contains("cloud") && s.contains("was not found") {
        return CliFailure::CloudNotFound;
    }
    match http {
        Some(401) => return CliFailure::Unauthorized,
        Some(403) => return CliFailure::Forbidden,
        Some(404) => return CliFailure::ServerNotFound,
        Some(409) => return CliFailure::NotReady,
        _ => {}
    }
    if s.contains("no server with a name or id") {
        return CliFailure::ServerNotFound;
    }
    if s.contains("missing value auth-url") || s.contains("authentication required") {
        return CliFailure::Unauthorized;
    }
    CliFailure::Exit(code)
}

// ------------------------------------------------------------------ transport

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// How a session ended, once it has. Read it after the stream hit EOF to tell a
/// clean close from a failure.
#[derive(Debug, Clone, Default)]
pub struct Outcome(Arc<Mutex<Option<Result<(), String>>>>);

impl Outcome {
    fn set(&self, result: Result<(), &'static str>) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(result.map_err(str::to_string));
        }
    }

    /// `None` while the session runs; `Some(Ok)` after a normal close;
    /// `Some(Err(reason))` after a failure (a fixed reason, no remote text).
    pub fn get(&self) -> Option<Result<(), String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// An open console: a byte stream plus what is known about it.
#[derive(Debug)]
pub struct Established {
    /// RFB bytes for [`Kind::Vnc`], terminal bytes for [`Kind::Serial`].
    /// Reads return EOF when the proxy closes; dropping it closes the session.
    pub stream: DuplexStream,
    pub outcome: Outcome,
    /// Whether the connection to the proxy is TLS protected.
    pub encrypted: bool,
}

/// Fetch a fresh URL and open the console: [`fetch_url`], [`parse_target`],
/// [`connect`].
pub async fn open(opts: &Options, kind: Kind) -> Result<Established, Error> {
    let url = fetch_url(opts, kind).await?;
    let target = parse_target(&url, opts.allow_insecure)?;
    drop(url);
    connect(&target, opts).await
}

/// Open the WebSocket to TARGET and start pumping it into the returned stream.
/// Must run inside a tokio runtime (the pump is a spawned task).
pub async fn connect(target: &Target, opts: &Options) -> Result<Established, Error> {
    let tls = if target.secure {
        Some(TlsConnector::from(Arc::new(tls_config(
            opts.ca_file.as_deref(),
        )?)))
    } else {
        None
    };
    let ws = tokio::time::timeout(opts.connect_timeout, handshake(target, tls))
        .await
        .map_err(|_| Error::Connect("timed out"))??;
    let (local, remote) = tokio::io::duplex(PIPE_BUFFER);
    let outcome = Outcome::default();
    tokio::spawn(pump(ws, remote, outcome.clone(), opts.allow_text_frames));
    Ok(Established {
        stream: local,
        outcome,
        encrypted: target.secure,
    })
}

fn tls_config(ca_file: Option<&Path>) -> Result<ClientConfig, Error> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = ca_file {
        let meta = std::fs::metadata(path).map_err(|_| Error::CaFile("cannot read it"))?;
        if !meta.is_file() || meta.len() > MAX_CA_FILE {
            return Err(Error::CaFile("not a PEM bundle"));
        }
        let pem = std::fs::read(path).map_err(|_| Error::CaFile("cannot read it"))?;
        let mut added = 0usize;
        for cert in CertificateDer::pem_slice_iter(&pem) {
            let cert = cert.map_err(|_| Error::CaFile("not valid PEM"))?;
            roots
                .add(cert)
                .map_err(|_| Error::CaFile("not a valid certificate"))?;
            added += 1;
        }
        if added == 0 {
            return Err(Error::CaFile("no certificate in it"));
        }
    }
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::Connect("no usable TLS version"))
        .map(|b| b.with_root_certificates(roots).with_no_client_auth())
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_frame_size(Some(MAX_FRAME))
        .max_message_size(Some(MAX_MESSAGE))
}

async fn handshake(
    target: &Target,
    tls: Option<TlsConnector>,
) -> Result<WebSocketStream<Box<dyn Io>>, Error> {
    let tcp = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .map_err(|_| Error::Connect("TCP connection failed"))?;
    let _ = tcp.set_nodelay(true);
    let io: Box<dyn Io> = match tls {
        Some(connector) => {
            let name = ServerName::try_from(target.host.clone())
                .map_err(|_| Error::Url("the host is not a valid TLS name"))?;
            let stream = connector
                .connect(name, tcp)
                .await
                .map_err(|_| Error::Connect("TLS handshake or certificate verification failed"))?;
            Box::new(stream)
        }
        None => Box::new(tcp),
    };

    let mut io = io;
    let key = tungstenite::handshake::client::generate_key();
    let authority = target.origin.split_once("://").map_or("", |(_, a)| a);
    let request = Zeroizing::new(format!(
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nOrigin: {}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: {SUBPROTOCOL}\r\n\r\n",
        *target.uri, target.origin
    ));
    io.write_all(request.as_bytes())
        .await
        .map_err(|_| Error::Connect("connection lost during handshake"))?;
    io.flush()
        .await
        .map_err(|_| Error::Connect("connection lost during handshake"))?;
    let head = read_response_head(&mut io).await?;
    verify_response(&head, &key)?;
    Ok(WebSocketStream::from_raw_socket(io, Role::Client, Some(ws_config())).await)
}

/// Read the HTTP response head, one byte at a time so that nothing of the
/// WebSocket stream behind it is consumed. Bounded.
async fn read_response_head(io: &mut Box<dyn Io>) -> Result<Vec<u8>, Error> {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_RESPONSE_HEAD {
            return Err(Error::Protocol("the handshake response is too large"));
        }
        match io.read(&mut byte).await {
            Ok(1) => head.push(byte[0]),
            _ => return Err(Error::Connect("connection lost during handshake")),
        }
    }
    Ok(head)
}

/// Check the handshake response (RFC 6455 section 4.1). Unlike the WebSocket
/// library, a missing `Sec-WebSocket-Protocol` is accepted: proxies differ
/// on echoing the requested subprotocol, and the Python bridge this replaces
/// accepted that too. A different protocol than the one requested is not.
fn verify_response(head: &[u8], key: &str) -> Result<(), Error> {
    let text = std::str::from_utf8(head).map_err(|_| bad_response())?;
    let mut lines = text.split("\r\n");
    let status = lines.next().unwrap_or_default();
    let mut parts = status.split(' ');
    if !parts.next().is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(bad_response());
    }
    let code: u16 = parts
        .next()
        .and_then(|c| c.parse().ok())
        .ok_or_else(bad_response)?;
    if code != 101 {
        return Err(Error::Http(code));
    }
    let (mut upgrade, mut connection, mut accept, mut protocol) = (false, false, false, true);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "upgrade" => upgrade |= value.eq_ignore_ascii_case("websocket"),
            "connection" => {
                connection |= value
                    .split(',')
                    .any(|t| t.trim().eq_ignore_ascii_case("upgrade"));
            }
            "sec-websocket-accept" => {
                accept = value == tungstenite::handshake::derive_accept_key(key.as_bytes());
            }
            "sec-websocket-protocol" => protocol = value == SUBPROTOCOL,
            "sec-websocket-extensions" => return Err(bad_response()),
            _ => {}
        }
    }
    if upgrade && connection && accept && protocol {
        Ok(())
    } else {
        Err(bad_response())
    }
}

fn bad_response() -> Error {
    Error::Protocol("WebSocket handshake failed")
}

/// Relay between the WebSocket and REMOTE (the pump's end of the duplex pipe)
/// until either side ends. The two directions run concurrently so a consumer
/// that writes while it should be reading cannot deadlock the pair.
async fn pump(
    ws: WebSocketStream<Box<dyn Io>>,
    remote: DuplexStream,
    outcome: Outcome,
    allow_text: bool,
) {
    let (mut sink, mut source) = ws.split();
    let (mut from_app, mut to_app) = tokio::io::split(remote);

    let down = async {
        while let Some(message) = source.next().await {
            let data = match message {
                Ok(Message::Binary(data)) => data,
                Ok(Message::Text(text)) if allow_text => text.as_bytes().to_vec().into(),
                Ok(Message::Text(_)) => return Err("unexpected text frame"),
                Ok(Message::Close(_)) => return Ok(()),
                Ok(_) => continue,
                Err(tungstenite::Error::ConnectionClosed) => return Ok(()),
                Err(tungstenite::Error::Capacity(_)) => return Err("frame or message too large"),
                Err(tungstenite::Error::Io(_)) => return Err("connection lost"),
                Err(_) => return Err("WebSocket protocol error"),
            };
            if to_app.write_all(&data).await.is_err() {
                return Ok(());
            }
        }
        Ok(())
    };
    let up = async {
        let mut buf = vec![0u8; PIPE_BUFFER];
        loop {
            match from_app.read(&mut buf).await {
                Ok(0) | Err(_) => return Ok(()),
                Ok(n) => {
                    if sink
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        return Err("connection lost");
                    }
                }
            }
        }
    };
    let result: Result<(), &'static str> = tokio::select! {
        r = down => r,
        r = up => r,
    };
    outcome.set(result);
    let code = if result.is_ok() {
        CloseCode::Normal
    } else {
        CloseCode::Protocol
    };
    let _ = tokio::time::timeout(
        CLOSE_WAIT,
        sink.send(Message::Close(Some(CloseFrame {
            code,
            reason: "".into(),
        }))),
    )
    .await;
    // Dropping the halves closes the pipe: the consumer sees EOF, after the
    // outcome is set.
}

// -------------------------------------------------------------- serial relay

/// How [`relay`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayEnd {
    /// The console closed.
    ConsoleClosed,
    /// The input ended.
    InputClosed,
    /// The detach byte was typed.
    Detached,
}

/// Connect INPUT (a terminal's stdin) and OUTPUT (its stdout) to a serial
/// console stream until one side ends. A DETACH byte in the input (Ctrl+] is
/// 0x1d) ends the relay without being sent. The caller owns the terminal
/// mode: put the tty in raw mode first and restore it after.
pub async fn relay<S, I, O>(
    console: S,
    mut input: I,
    mut output: O,
    detach: Option<u8>,
) -> io::Result<RelayEnd>
where
    S: AsyncRead + AsyncWrite + Unpin,
    I: AsyncRead + Unpin,
    O: AsyncWrite + Unpin,
{
    let (mut console_r, mut console_w) = tokio::io::split(console);
    let to_output = async {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = console_r.read(&mut buf).await?;
            if n == 0 {
                return Ok::<_, io::Error>(RelayEnd::ConsoleClosed);
            }
            output.write_all(&buf[..n]).await?;
            output.flush().await?;
        }
    };
    let to_console = async {
        let mut buf = vec![0u8; 1024];
        loop {
            let n = input.read(&mut buf).await?;
            if n == 0 {
                return Ok(RelayEnd::InputClosed);
            }
            let chunk = &buf[..n];
            if let Some(at) = detach.and_then(|d| chunk.iter().position(|&b| b == d)) {
                console_w.write_all(&chunk[..at]).await?;
                return Ok(RelayEnd::Detached);
            }
            console_w.write_all(chunk).await?;
        }
    };
    tokio::select! {
        r = to_output => r,
        r = to_console => r,
    }
}

/// `true` when the string is something to show a user as the name of OsStr.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn is_send<T: Send>() {}
    is_send::<Established>();
    is_send::<Target>();
    let _ = OsStr::new("");
}

// ----------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::SocketAddr;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    const TOKEN: &str = "5a1f0c2e-9c3d-4b61-8f5e-0123456789ab";

    /// Held while a fake client script is written and while a client runs: a
    /// child forked by a parallel test would keep a script being written open
    /// ("text file busy"). Never held for longer than one of those steps and
    /// never nested, so a test may own several fake clients.
    static SCRIPT_LOCK: Mutex<()> = Mutex::new(());

    fn script_lock() -> std::sync::MutexGuard<'static, ()> {
        SCRIPT_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn url(raw: &str) -> ConsoleUrl {
        ConsoleUrl::new(raw)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    // ------------------------------------------------------------- URL parsing

    #[test]
    fn parse_accepts_the_known_url_shapes() {
        let cases = [
            // noVNC with the WebSocket path in `path`.
            (
                format!(
                    "https://cloud.example:6080/vnc_lite.html?path=websockify%3Ftoken%3D{TOKEN}"
                ),
                format!("wss://cloud.example:6080/websockify?token={TOKEN}"),
                "https://cloud.example:6080",
            ),
            (
                format!("https://cloud.example/vnc_lite.html?path=%3Ftoken%3D{TOKEN}"),
                format!("wss://cloud.example/?token={TOKEN}"),
                "https://cloud.example",
            ),
            // Only a token: the path is websockify.
            (
                format!("https://cloud.example:6080/vnc_auto.html?token={TOKEN}"),
                format!("wss://cloud.example:6080/websockify?token={TOKEN}"),
                "https://cloud.example:6080",
            ),
            (
                format!("http://127.0.0.1:6080/vnc_auto.html?token={TOKEN}"),
                format!("ws://127.0.0.1:6080/websockify?token={TOKEN}"),
                "http://127.0.0.1:6080",
            ),
            // Serial: a WebSocket URL.
            (
                format!("ws://127.0.0.1:6083/?token={TOKEN}"),
                format!("ws://127.0.0.1:6083/?token={TOKEN}"),
                "http://127.0.0.1:6083",
            ),
            (
                format!("wss://cloud.example:6083/?token={TOKEN}"),
                format!("wss://cloud.example:6083/?token={TOKEN}"),
                "https://cloud.example:6083",
            ),
            (
                format!("ws://[::1]:6083/?token={TOKEN}"),
                format!("ws://[::1]:6083/?token={TOKEN}"),
                "http://[::1]:6083",
            ),
        ];
        for (input, ws, origin) in cases {
            let target = parse_target(&url(&format!("  {input}\n")), false)
                .unwrap_or_else(|_| panic!("rejected {input}"));
            assert_eq!(target.ws_url(), ws, "{input}");
            assert_eq!(target.origin(), origin, "{input}");
        }
    }

    #[test]
    fn parse_rejects_unsafe_urls_without_leaking_the_token() {
        let long = format!("https://h/vnc.html?token={}", "a".repeat(MAX_URL));
        let cases = [
            String::new(),
            "not a url".to_string(),
            format!("ftp://h/?token={TOKEN}"),
            format!("file:///etc/passwd?token={TOKEN}"),
            format!("https://user:pw@h/vnc.html?token={TOKEN}"),
            format!("https://user@h/vnc.html?token={TOKEN}"),
            format!("https://h/vnc.html?token={TOKEN}#frag"),
            format!("wss://h/?token={TOKEN}#frag"),
            // The WebSocket may not leave the console origin.
            format!("https://h/vnc.html?path=%2F%2Fevil.example%2Fws%3Ftoken%3D{TOKEN}"),
            format!("https://h/vnc.html?path=https%3A%2F%2Fevil.example%2Fws%3Ftoken%3D{TOKEN}"),
            format!("https://h/vnc.html?path=wss%3A%2F%2Fevil.example%2Fws%3Ftoken%3D{TOKEN}"),
            format!("https://h/vnc.html?path=%5C%5Cevil.example%5Cws%3Ftoken%3D{TOKEN}"),
            format!("https://h/vnc.html?path=%2F%2Fh%3A1%2Fws%3Ftoken%3D{TOKEN}"),
            format!("https://h/vnc.html?path=%2F%2Fuser%40evil.example%2Fws%3Ftoken%3D{TOKEN}"),
            format!("https://h/vnc.html?path=ws%3A%2F%2Fh%2Fws%3Ftoken%3D{TOKEN}"),
            format!("https://h/vnc.html?path=websockify%3Ftoken%3D{TOKEN}%23x"),
            // No token at all, or an empty one.
            "https://h/vnc.html".to_string(),
            "https://h/vnc.html?token=".to_string(),
            "wss://h/".to_string(),
            "wss://h/?other=1".to_string(),
            "https://h/vnc.html?path=websockify".to_string(),
            // Whitespace and control characters inside.
            format!("https://h/vnc.html?token={TOKEN}\n&x=1"),
            format!("https://h/vnc.html?token={TOKEN}\u{1b}[0m"),
            format!("https://h/vnc.html?to ken={TOKEN}"),
            long,
        ];
        for case in &cases {
            let Err(err) = parse_target(&url(case), false) else {
                panic!("accepted {case:.80}");
            };
            for text in [err.to_string(), format!("{err:?}")] {
                assert!(!text.contains(TOKEN), "token in error for {case:.80}");
                assert!(!text.contains("evil.example"), "host in error: {text}");
            }
        }
    }

    #[test]
    fn unencrypted_non_loopback_needs_the_override() {
        for raw in [
            format!("ws://cloud.example:6083/?token={TOKEN}"),
            format!("http://10.1.2.3:6080/vnc_auto.html?token={TOKEN}"),
        ] {
            assert!(matches!(
                parse_target(&url(&raw), false),
                Err(Error::Insecure)
            ));
            let target = parse_target(&url(&raw), true).unwrap();
            assert!(!target.is_encrypted());
        }
        for raw in [
            format!("ws://localhost:6083/?token={TOKEN}"),
            format!("ws://127.0.0.1:6083/?token={TOKEN}"),
            format!("ws://[::1]:6083/?token={TOKEN}"),
        ] {
            assert!(parse_target(&url(&raw), false).is_ok(), "{raw}");
        }
        let target = parse_target(&url(&format!("wss://h/?token={TOKEN}")), false).unwrap();
        assert!(target.is_encrypted());
    }

    #[test]
    fn secrets_have_redacted_debug() {
        let raw = url(&format!("wss://h:1/?token={TOKEN}"));
        assert!(!format!("{raw:?}").contains(TOKEN));
        let target = parse_target(&raw, false).unwrap();
        let shown = format!("{target:?}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("redacted"));
    }

    // ----------------------------------------------------------- the CLI client

    struct FakeCli {
        dir: tempfile::TempDir,
    }

    impl FakeCli {
        /// A client script: BODY runs with the arguments in "$@". It records
        /// them in `args` (one per line) and the config file variable in `env`
        /// first.
        fn new(body: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = format!(
                "#!/bin/sh\nd=\"$(dirname \"$0\")\"\n\
                 for a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$d/args\"\n\
                 printf '%s\\n' \"$OS_CLIENT_CONFIG_FILE\" > \"$d/env\"\n{body}\n"
            );
            let path = dir.path().join("openstack");
            {
                let _guard = script_lock();
                fs::write(&path, script).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self { dir }
        }

        fn opts(&self, server: &str) -> Options {
            let mut opts = Options::new(server);
            opts.openstack_bin = Some(self.dir.path().join("openstack"));
            opts.cli_timeout = Duration::from_secs(10);
            opts
        }

        fn args(&self) -> Vec<String> {
            fs::read_to_string(self.dir.path().join("args"))
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn config_env(&self) -> String {
            fs::read_to_string(self.dir.path().join("env"))
                .unwrap()
                .trim()
                .to_string()
        }
    }

    fn fetch(opts: &Options, kind: Kind) -> Result<ConsoleUrl, Error> {
        let _guard = script_lock();
        rt().block_on(fetch_url(opts, kind))
    }

    #[test]
    fn fetch_runs_the_client_with_exact_arguments() {
        let cli = FakeCli::new(&format!("echo 'wss://h:6083/?token={TOKEN}'"));
        let mut opts = cli.opts("my server");
        opts.cloud = Some("prod".into());
        opts.clouds_file = Some("/srv/clouds.yaml".into());
        let got = fetch(&opts, Kind::Serial).unwrap();
        assert_eq!(got.as_str(), format!("wss://h:6083/?token={TOKEN}"));
        assert_eq!(
            cli.args(),
            [
                "--os-cloud",
                "prod",
                "console",
                "url",
                "show",
                "--serial",
                "-f",
                "value",
                "-c",
                "url",
                "my server"
            ]
        );
        assert_eq!(cli.config_env(), "/srv/clouds.yaml");

        let cli = FakeCli::new(&format!("echo 'https://h/vnc_auto.html?token={TOKEN}'"));
        fetch(&cli.opts("srv"), Kind::Vnc).unwrap();
        assert_eq!(
            cli.args(),
            [
                "console", "url", "show", "--novnc", "-f", "value", "-c", "url", "srv"
            ]
        );
        assert_eq!(cli.config_env(), "");
    }

    #[test]
    fn fetch_rejects_bad_settings_before_running_anything() {
        let cli = FakeCli::new("echo should-not-run");
        for server in ["", "-x", "--help", "a\nb", "esc\u{1b}[0m"] {
            let err = fetch(&cli.opts(server), Kind::Vnc).unwrap_err();
            assert!(matches!(err, Error::Setting(_)), "{server:?}: {err}");
        }
        let mut opts = cli.opts("srv");
        opts.cloud = Some("-oops".into());
        assert!(matches!(fetch(&opts, Kind::Vnc), Err(Error::Setting(_))));
        assert!(!cli.dir.path().join("args").exists(), "the client ran");
    }

    #[test]
    fn fetch_failures_are_classified_and_never_echo_output() {
        let cases: [(&str, CliFailure); 9] = [
            (
                "Unavailable console type serial. (HTTP 400) (Request-ID: req-1)",
                CliFailure::Unavailable,
            ),
            ("Unauthorized (HTTP 401)", CliFailure::Unauthorized),
            ("Forbidden (HTTP 403)", CliFailure::Forbidden),
            ("Instance is not ready (HTTP 409)", CliFailure::NotReady),
            ("something (HTTP 404)", CliFailure::ServerNotFound),
            (
                "No server with a name or ID of 'x' exists.",
                CliFailure::ServerNotFound,
            ),
            ("Cloud nope was not found.", CliFailure::CloudNotFound),
            (
                "Missing value auth-url required for auth plugin password",
                CliFailure::Unauthorized,
            ),
            ("boom", CliFailure::Exit(Some(3))),
        ];
        for (stderr, expected) in cases {
            // The token and a password are in stderr and stdout: neither may
            // surface.
            let cli = FakeCli::new(&format!(
                "echo 'wss://h/?token={TOKEN} password=hunter2'; \
                 echo '{stderr} token={TOKEN} password=hunter2' >&2; exit 3"
            ));
            let err = fetch(&cli.opts("srv"), Kind::Vnc).unwrap_err();
            let Error::Cli(got) = &err else {
                panic!("{err:?}")
            };
            assert_eq!(*got, expected, "{stderr}");
            for text in [err.to_string(), format!("{err:?}")] {
                assert!(!text.contains(TOKEN) && !text.contains("hunter2"), "{text}");
            }
        }
    }

    #[test]
    fn fetch_bounds_time_and_output_and_reports_a_missing_client() {
        let cli = FakeCli::new("sleep 30");
        let mut opts = cli.opts("srv");
        opts.cli_timeout = Duration::from_millis(300);
        let started = Instant::now();
        assert!(matches!(
            fetch(&opts, Kind::Vnc),
            Err(Error::Cli(CliFailure::Timeout))
        ));
        assert!(started.elapsed() < Duration::from_secs(10));

        let cli = FakeCli::new("head -c 200000 /dev/zero | tr '\\0' 'a'");
        assert!(matches!(
            fetch(&cli.opts("srv"), Kind::Vnc),
            Err(Error::Cli(CliFailure::TooMuchOutput))
        ));

        for body in ["true", "echo 'two words'", "printf '\\377'"] {
            let cli = FakeCli::new(body);
            assert!(
                matches!(fetch(&cli.opts("srv"), Kind::Vnc), Err(Error::Url(_))),
                "{body}"
            );
        }

        let mut opts = Options::new("srv");
        opts.openstack_bin = Some("/nonexistent/openstack".into());
        assert!(matches!(
            fetch(&opts, Kind::Vnc),
            Err(Error::Cli(CliFailure::Spawn { not_found: true }))
        ));
    }

    #[test]
    fn dropping_the_fetch_kills_the_client() {
        let cli = FakeCli::new("echo $$ > \"$d/pid\"; sleep 30");
        let opts = cli.opts("srv");
        let pid_file = cli.dir.path().join("pid");
        {
            let _guard = script_lock();
            rt().block_on(async {
                let fetching = fetch_url(&opts, Kind::Vnc);
                let _ = tokio::time::timeout(Duration::from_millis(500), fetching).await;
            });
        }
        let pid: i32 = fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: signal 0 only probes for the process.
            let alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(Instant::now() < end, "the client is still running");
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn only_new_console_log_lines_are_printed() {
        let a = ["boot", "one", "two"];
        assert_eq!(new_log_lines(&[], &a), &a);
        let b = ["one", "two", "three"];
        assert_eq!(new_log_lines(&a, &b), &["three"]);
        assert!(new_log_lines(&b, &b).is_empty());
        let c = ["x", "y"];
        assert_eq!(new_log_lines(&b, &c), &c);
    }

    #[test]
    fn run_as_parsing() {
        assert_eq!(parse_id("1000"), Some(1000));
        for bad in ["", "-1", "1e3", "99999999999", "0x10", " 1"] {
            assert_eq!(parse_id(bad), None, "{bad:?}");
        }
        assert_eq!(RunAs::lookup(0, 100), None);
        assert_eq!(RunAs::lookup(100, 0), None);
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        if uid != 0 {
            // SAFETY: getgid has no preconditions.
            let gid = unsafe { libc::getgid() };
            if let Some(me) = RunAs::lookup(uid, gid) {
                assert!(me.home.is_absolute());
            }
        }
        // Not root here (or no sudo variables): nothing to drop to.
        if unsafe { libc::geteuid() } != 0 {
            assert_eq!(RunAs::from_sudo_env(), None);
        }
    }

    // ------------------------------------------------------------ the transport

    #[derive(Debug, Default, Clone)]
    struct Seen {
        uri: String,
        origin: String,
        protocol: String,
    }

    #[derive(Clone, Copy)]
    enum Script {
        /// Send a banner, then echo whatever arrives.
        Echo(&'static [u8]),
        /// Send one text frame.
        Text,
        /// Send a binary frame bigger than the message limit.
        Oversized,
        /// Send nothing and close.
        Close,
        /// Send LOT bytes and count what arrives meanwhile (deadlock test).
        Flood,
    }

    const FLOOD: usize = 4 * 1024 * 1024;

    async fn serve_ws<S>(stream: S, seen: Arc<Mutex<Seen>>, script: Script, echo_subprotocol: bool)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        #[allow(clippy::result_large_err)]
        let callback = |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
            let header = |name: &str| {
                req.headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string()
            };
            *seen.lock().unwrap() = Seen {
                uri: req.uri().to_string(),
                origin: header("origin"),
                protocol: header("sec-websocket-protocol"),
            };
            if echo_subprotocol {
                resp.headers_mut()
                    .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("binary"));
            }
            Ok(resp)
        };
        let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
            return;
        };
        match script {
            Script::Echo(banner) => {
                if !banner.is_empty() {
                    let _ = ws.send(Message::Binary(banner.to_vec().into())).await;
                }
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Binary(data) = msg
                        && ws.send(Message::Binary(data)).await.is_err()
                    {
                        break;
                    }
                }
            }
            Script::Text => {
                let _ = ws.send(Message::Text("hello".into())).await;
                let _ = ws.next().await;
            }
            Script::Oversized => {
                let _ = ws
                    .send(Message::Binary(vec![0u8; MAX_MESSAGE + 1].into()))
                    .await;
                let _ = ws.next().await;
            }
            Script::Close => {
                let _ = ws.close(None).await;
            }
            Script::Flood => {
                let (mut sink, mut source) = ws.split();
                let sender = async {
                    for _ in 0..FLOOD / 65536 {
                        if sink
                            .send(Message::Binary(vec![7u8; 65536].into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                };
                let receiver = async {
                    let mut got = 0usize;
                    while let Some(Ok(Message::Binary(data))) = source.next().await {
                        got += data.len();
                        if got >= FLOOD {
                            break;
                        }
                    }
                    got
                };
                let (_, got) = tokio::join!(sender, receiver);
                let _ = sink
                    .send(Message::Binary(got.to_string().into_bytes().into()))
                    .await;
                let _ = sink.close().await;
            }
        }
    }

    struct Server {
        addr: SocketAddr,
        seen: Arc<Mutex<Seen>>,
    }

    async fn plain_server(script: Script, echo_subprotocol: bool) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let task_seen = seen.clone();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                serve_ws(stream, task_seen, script, echo_subprotocol).await;
            }
        });
        Server { addr, seen }
    }

    fn target_for(addr: SocketAddr, scheme: &str, host: &str) -> Target {
        let raw = format!("{scheme}://{host}:{}/?token={TOKEN}", addr.port());
        parse_target(&url(&raw), false).unwrap()
    }

    async fn read_exact_n(stream: &mut DuplexStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut buf))
            .await
            .expect("timed out")
            .unwrap();
        buf
    }

    async fn drain_to_eof(stream: &mut DuplexStream) -> usize {
        let mut total = 0;
        let mut buf = vec![0u8; 8192];
        loop {
            match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return total,
                Ok(Ok(n)) => total += n,
                Err(_) => panic!("timed out"),
            }
        }
    }

    #[test]
    fn serial_session_round_trips_and_sends_origin_subprotocol_and_token() {
        rt().block_on(async {
            let server = plain_server(Script::Echo(b"login: "), true).await;
            let target = target_for(server.addr, "ws", "127.0.0.1");
            let mut est = connect(&target, &Options::new("srv")).await.unwrap();
            assert!(!est.encrypted);
            assert_eq!(read_exact_n(&mut est.stream, 7).await, b"login: ");
            est.stream.write_all(b"root\r").await.unwrap();
            assert_eq!(read_exact_n(&mut est.stream, 5).await, b"root\r");

            let seen = server.seen.lock().unwrap().clone();
            assert_eq!(seen.uri, format!("/?token={TOKEN}"));
            assert_eq!(
                seen.origin,
                format!("http://127.0.0.1:{}", server.addr.port())
            );
            assert_eq!(seen.protocol, "binary");

            // Dropping the stream ends the session cleanly.
            let outcome = est.outcome.clone();
            drop(est);
            let end = Instant::now() + Duration::from_secs(10);
            while outcome.get().is_none() {
                assert!(Instant::now() < end);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(outcome.get(), Some(Ok(())));
        });
    }

    #[test]
    fn a_server_that_does_not_echo_the_subprotocol_is_accepted() {
        rt().block_on(async {
            let server = plain_server(Script::Echo(b"hi"), false).await;
            let target = target_for(server.addr, "ws", "127.0.0.1");
            let mut est = connect(&target, &Options::new("srv")).await.unwrap();
            assert_eq!(read_exact_n(&mut est.stream, 2).await, b"hi");
        });
    }

    #[test]
    fn handshake_responses_are_verified() {
        rt().block_on(async {
            let cases: [(&str, bool); 4] = [
                // Wrong accept key.
                (
                    "Sec-WebSocket-Accept: AAAAAAAAAAAAAAAAAAAAAAAAAAA=\r\n",
                    false,
                ),
                // Missing accept key.
                ("", false),
                // Another subprotocol than requested.
                ("Sec-WebSocket-Protocol: base64\r\n", false),
                // A negotiated extension nobody asked for.
                ("Sec-WebSocket-Extensions: permessage-deflate\r\n", false),
            ];
            for (extra, ok) in cases {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move {
                    let (mut s, _) = listener.accept().await.unwrap();
                    let mut buf = vec![0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let reply = format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                         Connection: Upgrade\r\n{extra}\r\n"
                    );
                    let _ = s.write_all(reply.as_bytes()).await;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                });
                let target = target_for(addr, "ws", "127.0.0.1");
                let res = connect(&target, &Options::new("srv")).await;
                assert_eq!(res.is_ok(), ok, "{extra}");
                if let Err(err) = res {
                    assert!(matches!(err, Error::Protocol(_)), "{err:?}");
                }
            }
        });
    }

    #[test]
    fn text_frames_are_fatal_unless_allowed() {
        rt().block_on(async {
            let server = plain_server(Script::Text, true).await;
            let target = target_for(server.addr, "ws", "127.0.0.1");
            let mut est = connect(&target, &Options::new("srv")).await.unwrap();
            assert_eq!(drain_to_eof(&mut est.stream).await, 0);
            assert_eq!(
                est.outcome.get(),
                Some(Err("unexpected text frame".to_string()))
            );

            let server = plain_server(Script::Text, true).await;
            let target = target_for(server.addr, "ws", "127.0.0.1");
            let mut opts = Options::new("srv");
            opts.allow_text_frames = true;
            let mut est = connect(&target, &opts).await.unwrap();
            assert_eq!(read_exact_n(&mut est.stream, 5).await, b"hello");
        });
    }

    #[test]
    fn an_oversized_message_ends_the_session_with_an_error() {
        rt().block_on(async {
            let server = plain_server(Script::Oversized, true).await;
            let target = target_for(server.addr, "ws", "127.0.0.1");
            let mut est = connect(&target, &Options::new("srv")).await.unwrap();
            assert_eq!(drain_to_eof(&mut est.stream).await, 0);
            assert!(matches!(est.outcome.get(), Some(Err(_))));
        });
    }

    #[test]
    fn a_remote_close_is_a_clean_eof() {
        rt().block_on(async {
            let server = plain_server(Script::Close, true).await;
            let target = target_for(server.addr, "ws", "127.0.0.1");
            let mut est = connect(&target, &Options::new("srv")).await.unwrap();
            assert_eq!(drain_to_eof(&mut est.stream).await, 0);
            assert_eq!(est.outcome.get(), Some(Ok(())));
        });
    }

    #[test]
    fn simultaneous_bulk_traffic_in_both_directions_does_not_deadlock() {
        rt().block_on(async {
            let server = plain_server(Script::Flood, true).await;
            let target = target_for(server.addr, "ws", "127.0.0.1");
            let est = connect(&target, &Options::new("srv")).await.unwrap();
            let (mut rd, mut wr) = tokio::io::split(est.stream);
            let writer = async {
                let chunk = vec![9u8; 65536];
                for _ in 0..FLOOD / chunk.len() {
                    wr.write_all(&chunk).await.unwrap();
                }
            };
            let reader = async {
                let mut all = Vec::new();
                tokio::time::timeout(Duration::from_secs(30), rd.read_to_end(&mut all))
                    .await
                    .expect("deadlock")
                    .unwrap();
                all
            };
            let (_, all) = tokio::join!(writer, reader);
            assert_eq!(all.len(), FLOOD + FLOOD.to_string().len());
            assert_eq!(&all[FLOOD..], FLOOD.to_string().as_bytes());
        });
    }

    #[test]
    fn handshake_failures_report_only_a_status_and_follow_no_redirect() {
        rt().block_on(async {
            for status in [403u16, 302, 404] {
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move {
                    let (mut s, _) = listener.accept().await.unwrap();
                    let mut buf = vec![0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let reply = format!(
                        "HTTP/1.1 {status} X\r\nLocation: ws://evil.example/?token=stolen\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(reply.as_bytes()).await;
                });
                let target = target_for(addr, "ws", "127.0.0.1");
                let err = connect(&target, &Options::new("srv")).await.unwrap_err();
                assert!(matches!(err, Error::Http(s) if s == status), "{err:?}");
                let shown = format!("{err} {err:?}");
                assert!(!shown.contains("evil") && !shown.contains(TOKEN), "{shown}");
            }
        });
    }

    #[test]
    fn connection_problems_are_bounded_and_do_not_leak() {
        rt().block_on(async {
            // Nothing listens.
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            drop(l);
            let target = target_for(addr, "ws", "127.0.0.1");
            let err = connect(&target, &Options::new("srv")).await.unwrap_err();
            assert!(matches!(err, Error::Connect(_)), "{err:?}");
            assert!(!format!("{err} {err:?}").contains(TOKEN));

            // Accepts but never answers the handshake.
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap();
            tokio::spawn(async move {
                let (s, _) = l.accept().await.unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(s);
            });
            let target = target_for(addr, "ws", "127.0.0.1");
            let mut opts = Options::new("srv");
            opts.connect_timeout = Duration::from_millis(300);
            let started = Instant::now();
            let err = connect(&target, &opts).await.unwrap_err();
            assert!(matches!(err, Error::Connect("timed out")), "{err:?}");
            assert!(started.elapsed() < Duration::from_secs(10));
        });
    }

    // ---------------------------------------------------------------------- TLS

    struct TlsFixture {
        cert_pem: String,
        acceptor: tokio_rustls::TlsAcceptor,
    }

    fn tls_fixture() -> TlsFixture {
        use tokio_rustls::rustls::ServerConfig;
        use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let der = PrivatePkcs8KeyDer::from(key.signing_key.serialize_der());
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![key.cert.der().clone()], PrivateKeyDer::Pkcs8(der))
            .unwrap();
        TlsFixture {
            cert_pem: key.cert.pem(),
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
        }
    }

    async fn tls_server(fixture: &TlsFixture, script: Script) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (task_seen, acceptor) = (seen.clone(), fixture.acceptor.clone());
        tokio::spawn(async move {
            if let Ok((tcp, _)) = listener.accept().await
                && let Ok(stream) = acceptor.accept(tcp).await
            {
                serve_ws(stream, task_seen, script, true).await;
            }
        });
        Server { addr, seen }
    }

    #[test]
    fn wss_needs_a_trusted_certificate_and_works_with_the_ca_file() {
        rt().block_on(async {
            let fixture = tls_fixture();

            // Unknown CA: refused, with no detail of the certificate.
            let server = tls_server(&fixture, Script::Echo(b"ok")).await;
            let target = target_for(server.addr, "wss", "localhost");
            let err = connect(&target, &Options::new("srv")).await.unwrap_err();
            assert!(matches!(err, Error::Connect(_)), "{err:?}");
            assert!(server.seen.lock().unwrap().uri.is_empty(), "handshake ran");

            // The CA file adds trust.
            let dir = tempfile::tempdir().unwrap();
            let ca = dir.path().join("ca.pem");
            fs::write(&ca, &fixture.cert_pem).unwrap();
            let server = tls_server(&fixture, Script::Echo(b"ok")).await;
            let target = target_for(server.addr, "wss", "localhost");
            let mut opts = Options::new("srv");
            opts.ca_file = Some(ca.clone());
            let mut est = connect(&target, &opts).await.unwrap();
            assert!(est.encrypted);
            assert_eq!(read_exact_n(&mut est.stream, 2).await, b"ok");
            assert_eq!(
                server.seen.lock().unwrap().origin,
                format!("https://localhost:{}", server.addr.port())
            );
        });
    }

    #[test]
    fn wss_to_a_plain_server_fails_and_ca_files_are_validated() {
        rt().block_on(async {
            let server = plain_server(Script::Close, true).await;
            let target = target_for(server.addr, "wss", "localhost");
            let err = connect(&target, &Options::new("srv")).await.unwrap_err();
            assert!(matches!(err, Error::Connect(_)), "{err:?}");
        });
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.pem");
        fs::write(&junk, "not pem at all").unwrap();
        let empty = dir.path().join("empty.pem");
        fs::write(&empty, "").unwrap();
        let broken = dir.path().join("broken.pem");
        fs::write(
            &broken,
            "-----BEGIN CERTIFICATE-----\n!!!!\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let big = dir.path().join("big.pem");
        fs::write(&big, vec![b'a'; MAX_CA_FILE as usize + 1]).unwrap();
        for path in [
            &junk,
            &empty,
            &broken,
            &big,
            &dir.path().join("missing.pem"),
        ] {
            assert!(
                matches!(tls_config(Some(path)), Err(Error::CaFile(_))),
                "{path:?}"
            );
        }
        assert!(tls_config(None).is_ok());
    }

    // ------------------------------------------------------- end to end, relay

    #[test]
    fn open_fetches_a_url_and_connects() {
        let rt = rt();
        let server = rt.block_on(plain_server(Script::Echo(b"boot\r\n"), true));
        let cli = FakeCli::new(&format!(
            "echo 'ws://127.0.0.1:{}/?token={TOKEN}'",
            server.addr.port()
        ));
        let mut est = {
            let _guard = script_lock();
            rt.block_on(open(&cli.opts("srv"), Kind::Serial)).unwrap()
        };
        let banner = rt.block_on(read_exact_n(&mut est.stream, 6));
        assert_eq!(banner, b"boot\r\n");
        assert_eq!(server.seen.lock().unwrap().uri, format!("/?token={TOKEN}"));

        // A URL on a remote plain host is refused before any connection.
        let cli = FakeCli::new(&format!("echo 'ws://cloud.example:6083/?token={TOKEN}'"));
        let err = {
            let _guard = script_lock();
            rt.block_on(open(&cli.opts("srv"), Kind::Serial))
                .unwrap_err()
        };
        assert!(matches!(err, Error::Insecure), "{err:?}");
    }

    #[test]
    fn relay_copies_both_ways_and_honours_the_detach_key() {
        rt().block_on(async {
            // Console output reaches the terminal, typed input the console.
            let (console, mut far) = tokio::io::duplex(1024);
            let (mut typed, input) = tokio::io::duplex(1024);
            let (output, mut screen) = tokio::io::duplex(1024);
            let relaying = tokio::spawn(relay(console, input, output, Some(0x1d)));
            far.write_all(b"prompt# ").await.unwrap();
            let mut got = [0u8; 8];
            screen.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"prompt# ");
            typed.write_all(b"ls\r").await.unwrap();
            let mut got = [0u8; 3];
            far.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"ls\r");
            typed.write_all(b"ab\x1dcd").await.unwrap();
            assert_eq!(relaying.await.unwrap().unwrap(), RelayEnd::Detached);
            let mut rest = Vec::new();
            drop(far.shutdown().await);
            let mut got = [0u8; 2];
            far.read_exact(&mut got).await.unwrap_or_default();
            rest.extend_from_slice(&got);
            assert_eq!(&rest, b"ab");
        });
    }

    #[test]
    fn relay_ends_when_either_side_closes() {
        rt().block_on(async {
            let (console, far) = tokio::io::duplex(64);
            let (_typed, input) = tokio::io::duplex(64);
            let (output, _screen) = tokio::io::duplex(64);
            drop(far);
            assert_eq!(
                relay(console, input, output, None).await.unwrap(),
                RelayEnd::ConsoleClosed
            );
            let (console, _far) = tokio::io::duplex(64);
            let (typed, input) = tokio::io::duplex(64);
            let (output, _screen) = tokio::io::duplex(64);
            drop(typed);
            assert_eq!(
                relay(console, input, output, None).await.unwrap(),
                RelayEnd::InputClosed
            );
        });
    }
}
