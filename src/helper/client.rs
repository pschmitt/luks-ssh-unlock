//! The unprivileged side: talks to the helper over a unix socket.
//!
//! Authentication runs in this direction too. A path being root-owned does not
//! show that whatever listens on it is the helper (a directory that is not
//! root-only lets anyone put their own socket there), so the connection is
//! only used when the kernel reports the other end as the trusted user.
//! Nothing the helper says is shown: replies are parsed into closed types
//! and a failure becomes one of a few [`ClientError`] kinds with a fixed text.

use std::fs;
use std::io::{self, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use super::MAX_RESPONSE;
use super::reply::Outcome;
use super::request::{ErrorCode, Frame, ServiceAction, Target, read_frame};
use super::server::peer_of;

/// The socket for the verbs that cannot change what a unlock verifies.
pub const BASIC_SOCKET: &str = "/run/luks-ssh-unlock/basic.sock";
/// The socket that also allows forced unlocks and service control.
pub const ADMIN_SOCKET: &str = "/run/luks-ssh-unlock/admin.sock";
/// Replaces both socket paths. Meant for testing a helper that was started by
/// hand; the other end must still be the trusted user.
pub const SOCKET_ENV: &str = "LUKS_SSH_UNLOCK_HELPER_SOCKET";

/// Most requests in flight at once: each one is a process on the other side.
const MAX_IN_FLIGHT: usize = 4;

/// Why a request did not produce an answer. The texts are fixed: they are
/// shown to the user, so nothing the helper (or something posing as it) sent
/// is ever part of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// No helper socket could be reached: it is not enabled, or the user is
    /// not in the group that may use it.
    Unavailable,
    /// Something is listening, but it is not the helper, or is not where the
    /// helper's socket would be.
    Untrusted,
    /// The helper does not allow this.
    Denied,
    UnknownTarget,
    /// The helper refused the target's configuration.
    UntrustedConfig,
    Failed,
    Timeout,
    /// The helper failed, or its answer was not what was asked for.
    Protocol,
}

impl ClientError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "the privileged helper is not available (is it enabled, and are you in its group?)"
            }
            Self::Untrusted => "the helper socket is not served by the trusted user",
            Self::Denied => "the helper does not allow this",
            Self::UnknownTarget => "the helper does not know this target",
            Self::UntrustedConfig => "the helper refused the target's configuration",
            Self::Failed => "the helper reports that the operation failed",
            Self::Timeout => "the helper timed out",
            Self::Protocol => "the helper gave an unexpected answer",
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ClientError {}

impl From<ErrorCode> for ClientError {
    fn from(code: ErrorCode) -> Self {
        match code {
            ErrorCode::Denied => Self::Denied,
            ErrorCode::UnknownTarget => Self::UnknownTarget,
            ErrorCode::UntrustedConfig => Self::UntrustedConfig,
            ErrorCode::Failed => Self::Failed,
            ErrorCode::Timeout => Self::Timeout,
            ErrorCode::BadRequest | ErrorCode::TooLarge | ErrorCode::Internal => Self::Protocol,
        }
    }
}

/// A target the helper has a trusted configuration for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    /// The host name, when the helper judged it plain enough to show.
    pub host: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Reply {
    Ok { id: u64, data: Value },
    Error { id: Option<u64>, code: ErrorCode },
}

#[derive(Deserialize)]
struct ListData {
    targets: Vec<ListEntry>,
}

#[derive(Deserialize)]
struct ListEntry {
    name: String,
    #[serde(default)]
    host: Option<String>,
}

/// Which sockets a verb may be sent to, in the order they are tried.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Needs {
    Basic,
    Admin,
}

pub struct Client {
    basic: PathBuf,
    admin: PathBuf,
    /// The user the other end must be.
    server_uid: u32,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

impl Client {
    /// The production sockets, or the one named by [`SOCKET_ENV`].
    pub fn system() -> Self {
        match std::env::var_os(SOCKET_ENV).filter(|v| !v.is_empty()) {
            Some(path) => Self::with_paths(PathBuf::from(&path), PathBuf::from(path), 0),
            None => Self::with_paths(BASIC_SOCKET.into(), ADMIN_SOCKET.into(), 0),
        }
    }

    pub fn with_paths(basic: PathBuf, admin: PathBuf, server_uid: u32) -> Self {
        Self {
            basic,
            admin,
            server_uid,
        }
    }

    pub fn list(&self) -> Result<Vec<Listed>, ClientError> {
        let data = self.call(
            json!({"verb": "list"}),
            Needs::Basic,
            Duration::from_secs(20),
        )?;
        let data: ListData = serde_json::from_value(data).map_err(|_| ClientError::Protocol)?;
        data.targets
            .into_iter()
            .map(|t| {
                // The names are ones this process will send back and show.
                Target::parse(&t.name).map_err(|_| ClientError::Protocol)?;
                Ok(Listed {
                    name: t.name,
                    host: t
                        .host
                        .filter(|h| h.len() <= 253 && !h.chars().any(char::is_control)),
                })
            })
            .collect()
    }

    pub fn probe(&self, name: &str) -> Result<Outcome, ClientError> {
        let name = target(name)?;
        let data = self.call(
            json!({"verb": "probe", "name": name.as_str()}),
            Needs::Basic,
            Duration::from_secs(150),
        )?;
        outcome(data, |o| matches!(o, Outcome::Probe { .. }))
    }

    pub fn unlock(&self, name: &str, force: bool) -> Result<Outcome, ClientError> {
        let name = target(name)?;
        let (request, needs) = if force {
            (
                json!({
                    "verb": "unlock_force",
                    "name": name.as_str(),
                    "confirm": "skip_integrity_and_host_key_checks",
                }),
                Needs::Admin,
            )
        } else {
            (
                json!({"verb": "unlock", "name": name.as_str()}),
                Needs::Basic,
            )
        };
        let data = self.call(request, needs, Duration::from_secs(240))?;
        outcome(
            data,
            |o| matches!(o, Outcome::Unlock { forced, .. } if *forced == force),
        )
    }

    pub fn service(&self, name: &str, action: ServiceAction) -> Result<bool, ClientError> {
        let name = target(name)?;
        let data = self.call(
            json!({"verb": "service", "name": name.as_str(), "action": action}),
            Needs::Admin,
            Duration::from_secs(60),
        )?;
        match outcome(
            data,
            |o| matches!(o, Outcome::Service { action: a, .. } if *a == action),
        )? {
            Outcome::Service { done, .. } => Ok(done),
            _ => Err(ClientError::Protocol),
        }
    }

    fn call(&self, request: Value, needs: Needs, timeout: Duration) -> Result<Value, ClientError> {
        let _slot = Slot::take();
        let candidates: &[&Path] = match needs {
            Needs::Basic => &[&self.basic, &self.admin],
            Needs::Admin => &[&self.admin],
        };
        for path in candidates {
            match self.exchange(path, &request, timeout) {
                // A user may be in only one of the two groups.
                Err(ClientError::Unavailable) => {}
                // A socket that is not the helper's is a reason to stop, not
                // to quietly try the next one.
                other => return other,
            }
        }
        Err(ClientError::Unavailable)
    }

    fn exchange(
        &self,
        path: &Path,
        request: &Value,
        timeout: Duration,
    ) -> Result<Value, ClientError> {
        check_path(path, self.server_uid)?;
        let stream = UnixStream::connect(path).map_err(|_| ClientError::Unavailable)?;
        let peer = peer_of(stream.as_raw_fd()).map_err(|_| ClientError::Untrusted)?;
        if peer.uid != self.server_uid {
            return Err(ClientError::Untrusted);
        }
        stream
            .set_read_timeout(Some(timeout))
            .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(10))))
            .map_err(|_| ClientError::Protocol)?;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mut line = serde_json::to_vec(&json!({"id": id, "request": request}))
            .map_err(|_| ClientError::Protocol)?;
        line.push(b'\n');
        let mut writer = &stream;
        writer
            .write_all(&line)
            .and_then(|()| writer.flush())
            .map_err(|e| io_error(&e))?;
        let mut reader = BufReader::new(&stream);
        let reply = match read_frame(&mut reader, MAX_RESPONSE) {
            Ok(Frame::Line(line)) => line,
            Ok(Frame::Eof) => return Err(ClientError::Protocol),
            Err(super::request::FrameError::Io(e)) => return Err(io_error(&e)),
            Err(_) => return Err(ClientError::Protocol),
        };
        match serde_json::from_slice::<Reply>(&reply).map_err(|_| ClientError::Protocol)? {
            Reply::Ok { id: got, data } if got == id => Ok(data),
            Reply::Error { id: got, code } if got == Some(id) || got.is_none() => Err(code.into()),
            _ => Err(ClientError::Protocol),
        }
    }
}

fn target(name: &str) -> Result<Target, ClientError> {
    Target::parse(name).map_err(|_| ClientError::UnknownTarget)
}

fn outcome(data: Value, expected: impl Fn(&Outcome) -> bool) -> Result<Outcome, ClientError> {
    let outcome: Outcome = serde_json::from_value(data).map_err(|_| ClientError::Protocol)?;
    if expected(&outcome) {
        Ok(outcome)
    } else {
        Err(ClientError::Protocol)
    }
}

fn io_error(e: &io::Error) -> ClientError {
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => ClientError::Timeout,
        _ => ClientError::Protocol,
    }
}

/// What can be told about a socket without connecting to it: that it is a
/// socket (not a link to somewhere else) and the trusted user's. This is not
/// what authenticates the helper (the peer check is); it stops a stray file
/// or a link from being connected to at all.
fn check_path(path: &Path, server_uid: u32) -> Result<(), ClientError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return Err(ClientError::Unavailable),
    };
    if !meta.file_type().is_socket() || meta.uid() != server_uid {
        return Err(ClientError::Untrusted);
    }
    Ok(())
}

/// A place in the queue of requests in flight.
struct Slot;

static IN_FLIGHT: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());

impl Slot {
    fn take() -> Self {
        let (lock, cv) = &IN_FLIGHT;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= MAX_IN_FLIGHT {
            n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        Slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (lock, cv) = &IN_FLIGHT;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        cv.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::thread;

    use super::super::backend::plain;
    use super::super::reply::{Checks, Failure, Integrity, Observed};
    use super::super::request::ServiceAction;
    use super::super::server::{Peer, session};
    use super::super::trust::{TrustedConfig, TrustedRoot};
    use super::super::{Backend, Policy};
    use super::*;

    fn euid() -> u32 {
        unsafe { libc::geteuid() }
    }

    fn checks() -> Checks {
        Checks {
            baseline: true,
            integrity: Integrity::Verified,
            host_keys_pinned: true,
            signature_required: false,
        }
    }

    struct Fake;

    impl Backend for Fake {
        fn probe(&mut self, _: &TrustedConfig) -> Result<Outcome, Failure> {
            Ok(Outcome::Probe {
                state: Observed::Initrd,
                unlockable: true,
                checks: checks(),
            })
        }
        fn unlock(&mut self, _: &TrustedConfig, force: bool) -> Result<Outcome, Failure> {
            Ok(Outcome::Unlock {
                state: Observed::Unlocked,
                unlocked: true,
                forced: force,
                checks: checks(),
            })
        }
        fn service(&mut self, _: &Target, action: ServiceAction) -> Result<Outcome, Failure> {
            Ok(Outcome::Service { action, done: true })
        }
    }

    struct Env {
        _tmp: tempfile::TempDir,
        basic: PathBuf,
        admin: PathBuf,
    }

    /// A real server loop per socket, on fixture configuration, with the
    /// policy that the socket stands for.
    fn env() -> Env {
        let tmp = tempfile::Builder::new()
            .prefix("lsu-client-")
            .tempdir_in("/tmp")
            .unwrap();
        let cfg = tmp.path().join("etc");
        fs::create_dir(&cfg).unwrap();
        fs::set_permissions(&cfg, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(cfg.join("alpha.env"), "SSH_HOSTNAME=a.example\n").unwrap();
        fs::write(cfg.join("beta.env"), "SSH_HOSTNAME=\"$(id)\"\n").unwrap();
        for f in ["alpha.env", "beta.env"] {
            fs::set_permissions(cfg.join(f), fs::Permissions::from_mode(0o644)).unwrap();
        }
        let run = tmp.path().join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        let mut paths = Vec::new();
        for (name, policy) in [
            ("basic.sock", Policy::default()),
            (
                "admin.sock",
                Policy {
                    allow_force: true,
                    allow_service: true,
                },
            ),
        ] {
            let path = run.join(name);
            let listener = UnixListener::bind(&path).unwrap();
            let cfg = cfg.clone();
            thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { return };
                    let cfg = cfg.clone();
                    thread::spawn(move || {
                        let root = TrustedRoot::for_test(cfg, euid());
                        let peer = Peer {
                            uid: euid(),
                            pid: 0,
                        };
                        let _ = session(conn, peer, &root, policy, &mut Fake);
                    });
                }
            });
            paths.push(path);
        }
        let admin = paths.pop().unwrap();
        let basic = paths.pop().unwrap();
        Env {
            _tmp: tmp,
            basic,
            admin,
        }
    }

    fn client(env: &Env) -> Client {
        Client::with_paths(env.basic.clone(), env.admin.clone(), euid())
    }

    #[test]
    fn list_probe_and_unlock_work_over_the_basic_socket() {
        let env = env();
        let c = client(&env);
        let listed = c.list().unwrap();
        assert_eq!(listed[0].name, "alpha");
        assert_eq!(listed[0].host.as_deref(), Some("a.example"));
        assert!(matches!(
            c.probe("alpha").unwrap(),
            Outcome::Probe {
                unlockable: true,
                ..
            }
        ));
        assert!(matches!(
            c.unlock("alpha", false).unwrap(),
            Outcome::Unlock {
                forced: false,
                unlocked: true,
                ..
            }
        ));
    }

    #[test]
    fn a_host_that_is_not_plain_is_not_listed() {
        let env = env();
        let listed = client(&env).list().unwrap();
        let beta = listed.iter().find(|t| t.name == "beta");
        assert!(beta.is_none_or(|t| t.host.is_none()));
        assert!(!plain("$(id)", super::super::backend::HOST_EXTRA));
    }

    #[test]
    fn force_and_service_only_go_to_the_admin_socket() {
        let env = env();
        let c = client(&env);
        assert!(matches!(
            c.unlock("alpha", true).unwrap(),
            Outcome::Unlock { forced: true, .. }
        ));
        assert_eq!(c.service("alpha", ServiceAction::Stop), Ok(true));
        // Only a basic socket to talk to: the admin verbs are denied there,
        // and are never resent anywhere else.
        let basic_only = Client::with_paths(env.basic.clone(), env.basic.clone(), euid());
        assert_eq!(
            basic_only.unlock("alpha", true).unwrap_err(),
            ClientError::Denied
        );
        assert_eq!(
            basic_only
                .service("alpha", ServiceAction::Start)
                .unwrap_err(),
            ClientError::Denied
        );
    }

    #[test]
    fn a_user_without_the_basic_group_falls_back_to_the_admin_socket() {
        let env = env();
        let gone = env.basic.with_file_name("nope.sock");
        let c = Client::with_paths(gone, env.admin.clone(), euid());
        assert!(c.probe("alpha").is_ok());
    }

    #[test]
    fn a_missing_socket_is_reported_as_unavailable() {
        let c = Client::with_paths("/nonexistent/a".into(), "/nonexistent/b".into(), euid());
        assert_eq!(c.list().unwrap_err(), ClientError::Unavailable);
        assert_eq!(c.probe("alpha").unwrap_err(), ClientError::Unavailable);
    }

    #[test]
    fn a_server_that_is_not_the_trusted_user_is_not_used() {
        let env = env();
        // The same helper, but the client expects a different user.
        let c = Client::with_paths(env.basic.clone(), env.admin.clone(), euid() + 1);
        assert_eq!(c.list().unwrap_err(), ClientError::Untrusted);
    }

    #[test]
    fn a_link_to_a_socket_is_not_followed() {
        let env = env();
        let link = env.basic.with_file_name("link.sock");
        std::os::unix::fs::symlink(&env.basic, &link).unwrap();
        let c = Client::with_paths(link.clone(), link, euid());
        assert_eq!(c.list().unwrap_err(), ClientError::Untrusted);
    }

    #[test]
    fn a_regular_file_is_not_connected_to() {
        let env = env();
        let file = env.basic.with_file_name("file");
        fs::write(&file, "").unwrap();
        let c = Client::with_paths(file.clone(), file, euid());
        assert_eq!(c.list().unwrap_err(), ClientError::Untrusted);
    }

    #[test]
    fn invalid_names_never_reach_the_socket() {
        let c = Client::with_paths("/nonexistent/a".into(), "/nonexistent/b".into(), euid());
        for bad in ["", "../x", "a/b", "-x", ".x", "a b"] {
            assert_eq!(
                c.probe(bad).unwrap_err(),
                ClientError::UnknownTarget,
                "{bad:?}"
            );
            assert_eq!(c.unlock(bad, true).unwrap_err(), ClientError::UnknownTarget);
        }
    }

    /// A socket that answers with whatever the test puts in, to the one request.
    fn rogue(reply: impl Fn(u64) -> Vec<u8> + Send + 'static) -> (tempfile::TempDir, Client) {
        let tmp = tempfile::Builder::new()
            .prefix("lsu-rogue-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { return };
                let mut line = String::new();
                let _ = BufReader::new(&conn).read_line(&mut line);
                let id = serde_json::from_str::<Value>(&line)
                    .ok()
                    .and_then(|v| v["id"].as_u64())
                    .unwrap_or(0);
                let _ = conn.write_all(&reply(id));
            }
        });
        let c = Client::with_paths(path.clone(), path, euid());
        (tmp, c)
    }

    #[test]
    fn answers_that_do_not_fit_are_a_protocol_error() {
        let replies: Vec<Box<dyn Fn(u64) -> Vec<u8> + Send>> = vec![
            Box::new(|_| b"not json\n".to_vec()),
            Box::new(|_| b"\n".to_vec()),
            Box::new(|_| b"{\"status\":\"ok\",\"id\":999999,\"data\":{}}\n".to_vec()),
            Box::new(|id| {
                format!("{{\"status\":\"ok\",\"id\":{id},\"data\":{{\"kind\":\"service\",\"action\":\"stop\",\"done\":true}}}}\n").into_bytes()
            }),
            Box::new(|id| {
                format!("{{\"status\":\"ok\",\"id\":{id},\"data\":{{\"kind\":\"probe\",\"state\":\"nonsense\"}}}}\n").into_bytes()
            }),
            Box::new(|id| {
                format!("{{\"status\":\"error\",\"id\":{id},\"code\":\"bad_request\",\"message\":\"secret\"}}\n").into_bytes()
            }),
            Box::new(|id| {
                format!("{{\"status\":\"ok\",\"id\":{id},\"data\":{{\"targets\":[{{\"name\":\"../etc\"}}]}}}}\n").into_bytes()
            }),
        ];
        for reply in replies {
            let (_t, c) = rogue(reply);
            // `probe` is asked: a service answer, a bad state, no answer and a
            // wrong id are all refused.
            assert_eq!(c.probe("alpha").unwrap_err(), ClientError::Protocol);
        }
        let (_t, c) = rogue(|id| {
            format!("{{\"status\":\"ok\",\"id\":{id},\"data\":{{\"targets\":[{{\"name\":\"../etc\"}}]}}}}\n").into_bytes()
        });
        assert_eq!(c.list().unwrap_err(), ClientError::Protocol);
    }

    #[test]
    fn an_endless_answer_is_cut_off() {
        let (_t, c) = rogue(|_| vec![b'x'; 2 * MAX_RESPONSE]);
        assert_eq!(c.probe("alpha").unwrap_err(), ClientError::Protocol);
    }

    #[test]
    fn a_coded_error_never_shows_the_text_it_came_with() {
        let (_t, c) = rogue(|id| {
            format!(
                "{{\"status\":\"error\",\"id\":{id},\"code\":\"failed\",\"message\":\"hunter2\"}}\n"
            )
            .into_bytes()
        });
        let e = c.probe("alpha").unwrap_err();
        assert_eq!(e, ClientError::Failed);
        assert!(!e.to_string().contains("hunter2"));
    }

    #[test]
    fn a_silent_server_times_out() {
        let tmp = tempfile::Builder::new()
            .prefix("lsu-silent-")
            .tempdir_in("/tmp")
            .unwrap();
        let path = tmp.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let mut held = Vec::new();
            for conn in listener.incoming().flatten() {
                held.push(conn);
            }
        });
        let c = Client::with_paths(path.clone(), path.clone(), euid());
        let r = c.exchange(&path, &json!({"verb": "list"}), Duration::from_millis(200));
        assert_eq!(r.unwrap_err(), ClientError::Timeout);
    }
}
