//! A narrowly scoped privileged helper.
//!
//! The TUI should not run as root. The operations that need privileges are
//! few and well defined: reading a target's root-owned configuration, probing
//! and unlocking with it, and starting or stopping that target's own service.
//! This module decides what is allowed and does it; it is the body of a small
//! root-side process that talks to its caller over a pipe or socket. Who may
//! start that process, and with which [`Policy`], is decided outside of it.
//!
//! * [`request`]: the bounded, typed wire protocol. No free-form arguments,
//!   no paths, only validated target names.
//! * [`trust`]: the only way configuration is read, and the check applied to
//!   every file, tree and command the configuration refers to. Fixed
//!   directory, no environment or path override, race-free resolution of the
//!   NixOS symbolic-link chain, ownership and mode checks.
//! * [`reply`]: what is sent back. Closed types and fixed messages only;
//!   command output, transport errors and configuration values never reach
//!   the caller, so there is nothing to scrub.
//! * [`backend`]: the real [`Backend`], built on the existing settings,
//!   status and unlock logic.
//! * [`serve`]: the request loop. It applies a [`Policy`] (forcing and
//!   service control are denied unless a session is explicitly given them),
//!   loads the configuration and calls the [`Backend`].

pub mod backend;
pub mod client;
pub mod reply;
pub mod request;
pub mod server;
pub mod trust;

use std::io::{self, BufRead, Write};
use std::process::Command;

use serde_json::json;

use self::reply::{Failure, Outcome};
use self::request::{
    Envelope, ErrorCode, Frame, FrameError, MAX_FRAME, Request, Response, ServiceAction, Target,
    parse_request, read_frame,
};
use self::trust::{TrustError, TrustedConfig, TrustedRoot};

/// Longest response line the helper will send.
pub const MAX_RESPONSE: usize = 256 * 1024;

/// What a session may do. Everything that changes state or skips a check is
/// off unless the caller turns it on for that session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Policy {
    /// Allow `unlock_force` (skips the integrity and host-key checks).
    pub allow_force: bool,
    /// Allow `service` (start/stop of the target's own unit).
    pub allow_service: bool,
}

/// The privileged operations, behind a trait so that the request handling is
/// testable and so that the helper cannot do anything the trait lacks. Every
/// method gets the validated, trusted configuration (or just the validated
/// name) and nothing from the caller beyond that, and answers with a closed
/// [`Outcome`] or a [`Failure`]: there is no way to return text.
pub trait Backend {
    /// Check the target without sending anything.
    fn probe(&mut self, config: &TrustedConfig) -> Result<Outcome, Failure>;
    /// Unlock. `force` skips the integrity and host-key checks and is only
    /// ever `true` for an acknowledged `unlock_force` allowed by the policy.
    fn unlock(&mut self, config: &TrustedConfig, force: bool) -> Result<Outcome, Failure>;
    /// Start or stop `luks-ssh-unlock-NAME.service`.
    fn service(&mut self, target: &Target, action: ServiceAction) -> Result<Outcome, Failure>;
}

/// Serve one session: read requests from `input`, answer on `output`, until
/// the input ends, a line is too long (the stream cannot be resynchronised) or
/// writing fails. One malformed request does not end the session.
pub fn serve<R: BufRead, W: Write>(
    root: &TrustedRoot,
    policy: &Policy,
    backend: &mut dyn Backend,
    input: &mut R,
    output: &mut W,
) -> io::Result<()> {
    loop {
        let line = match read_frame(input, MAX_FRAME) {
            Ok(Frame::Line(line)) => line,
            Ok(Frame::Eof) => return Ok(()),
            Err(FrameError::TooLong) => {
                send(
                    output,
                    &Response::error(None, ErrorCode::TooLarge, "request is too long"),
                )?;
                return Ok(());
            }
            Err(FrameError::Truncated) => return Ok(()),
            Err(FrameError::Io(e)) => return Err(e),
        };
        let response = match parse_request(&line) {
            Ok(envelope) => handle(root, policy, backend, envelope),
            Err(()) => Response::error(None, ErrorCode::BadRequest, "malformed request"),
        };
        send(output, &response)?;
    }
}

fn send<W: Write>(output: &mut W, response: &Response) -> io::Result<()> {
    let mut line = serde_json::to_vec(response).map_err(io::Error::other)?;
    if line.len() > MAX_RESPONSE {
        let id = match response {
            Response::Ok { id, .. } => Some(*id),
            Response::Error { id, .. } => *id,
        };
        line = serde_json::to_vec(&Response::error(
            id,
            ErrorCode::Internal,
            "response is too large",
        ))
        .map_err(io::Error::other)?;
    }
    line.push(b'\n');
    output.write_all(&line)?;
    output.flush()
}

fn handle(
    root: &TrustedRoot,
    policy: &Policy,
    backend: &mut dyn Backend,
    envelope: Envelope,
) -> Response {
    let id = envelope.id;
    // The policy comes first: a denied request touches nothing.
    match &envelope.request {
        Request::UnlockForce { .. } if !policy.allow_force => {
            return Response::error(
                Some(id),
                ErrorCode::Denied,
                "forced unlock is not allowed in this session",
            );
        }
        Request::Service { .. } if !policy.allow_service => {
            return Response::error(
                Some(id),
                ErrorCode::Denied,
                "service control is not allowed in this session",
            );
        }
        _ => {}
    }
    if matches!(envelope.request, Request::List {}) {
        return match root.list() {
            Ok(listing) => Response::Ok {
                id,
                data: json!({
                    "targets": listing
                        .targets
                        .iter()
                        .map(|t| list_entry(root, t))
                        .collect::<Vec<_>>(),
                    "rejected": listing
                        .rejected
                        .iter()
                        .map(|r| json!({"name": r.name, "reason": r.reason}))
                        .collect::<Vec<_>>(),
                }),
            },
            Err(e) => trust_error(id, &e),
        };
    }
    let (Request::Probe { name }
    | Request::Unlock { name }
    | Request::UnlockForce { name, .. }
    | Request::Service { name, .. }) = &envelope.request
    else {
        unreachable!("list was handled above");
    };
    let config = match root.load(name) {
        Ok(config) => config,
        Err(e) => return trust_error(id, &e),
    };
    let result = match envelope.request {
        Request::Probe { .. } => backend.probe(&config),
        Request::Unlock { .. } => backend.unlock(&config, false),
        Request::UnlockForce { .. } => backend.unlock(&config, true),
        Request::Service { action, .. } => backend.service(config.target(), action),
        Request::List {} => unreachable!(),
    };
    match result.map(serde_json::to_value) {
        Ok(Ok(data)) => Response::Ok { id, data },
        Ok(Err(_)) => Response::error(Some(id), ErrorCode::Internal, "could not encode the result"),
        Err(failure) => Response::error(Some(id), failure.code(), failure.message()),
    }
}

/// One target of a listing: its name, and its host name when that is a plain
/// one. A host name is the only configuration value a listing shows, because
/// the dashboard needs something to call the target by.
fn list_entry(root: &TrustedRoot, target: &Target) -> serde_json::Value {
    let host = root
        .load(target)
        .ok()
        .and_then(|c| c.get("SSH_HOSTNAME").map(str::to_string))
        .filter(|h| backend::plain(h, backend::HOST_EXTRA));
    match host {
        Some(host) => json!({"name": target.as_str(), "host": host}),
        None => json!({"name": target.as_str()}),
    }
}

/// A configuration problem, as a fixed message: the text of a [`TrustError`]
/// is for the log of whoever runs the helper, not for the caller.
fn trust_error(id: u64, e: &TrustError) -> Response {
    match e {
        TrustError::NotFound => {
            Response::error(Some(id), ErrorCode::UnknownTarget, "no such target")
        }
        TrustError::Untrusted(_) | TrustError::Invalid(_) => Response::error(
            Some(id),
            ErrorCode::UntrustedConfig,
            "the configuration was refused",
        ),
        TrustError::Io(_) => Response::error(
            Some(id),
            ErrorCode::Internal,
            "could not read configuration",
        ),
    }
}

/// Process-level hardening for the root-side process: no core dumps, no
/// ptrace/`/proc` access by the invoking user, and files created private.
pub fn harden_process() {
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        let none = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &none);
        libc::umask(0o077);
    }
}

/// `PATH` of the helper process: fixed, never inherited from the caller.
/// Entries that do not exist on the host are dropped when it is checked.
pub const CHILD_PATH: &str =
    "/run/wrappers/bin:/run/current-system/sw/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Start a child from a clean environment with the already vetted `path`.
/// Nothing the invoking user set (`LD_PRELOAD`, `PATH`, `SSH_*`, ...) reaches
/// it.
pub fn apply_child_env(cmd: &mut Command, path: &str) {
    cmd.env_clear()
        .env("PATH", path)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8");
}

/// The argument vector for starting or stopping a target's own unit. The
/// unit name is derived from the validated target, and `--` ends option
/// parsing, so nothing in it can be taken for a flag.
pub fn service_argv(target: &Target, action: ServiceAction) -> Vec<String> {
    let verb = match action {
        ServiceAction::Start => "start",
        ServiceAction::Stop => "stop",
    };
    vec![
        "systemctl".into(),
        "--no-ask-password".into(),
        verb.into(),
        "--".into(),
        target.unit_name(),
    ]
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    use serde_json::Value;

    use super::reply::{Checks, Integrity, Observed};

    fn probe_outcome() -> Outcome {
        Outcome::Probe {
            state: Observed::Initrd,
            unlockable: true,
            checks: Checks {
                baseline: true,
                integrity: Integrity::Verified,
                host_keys_pinned: true,
                signature_required: false,
            },
            details: None,
        }
    }

    #[derive(Default)]
    struct Mock {
        calls: Vec<String>,
        reply: Option<Result<Outcome, Failure>>,
    }

    impl Mock {
        fn answer(&mut self, call: String) -> Result<Outcome, Failure> {
            self.calls.push(call);
            self.reply.clone().unwrap_or_else(|| Ok(probe_outcome()))
        }
    }

    impl Backend for Mock {
        fn probe(&mut self, c: &TrustedConfig) -> Result<Outcome, Failure> {
            self.answer(format!("probe {}", c.target()))
        }
        fn unlock(&mut self, c: &TrustedConfig, force: bool) -> Result<Outcome, Failure> {
            self.answer(format!("unlock {} force={force}", c.target()))
        }
        fn service(&mut self, t: &Target, a: ServiceAction) -> Result<Outcome, Failure> {
            self.answer(format!("service {t} {a:?}"))
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        dir: std::path::PathBuf,
        root: TrustedRoot,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::Builder::new()
            .prefix("lsu-helper-")
            .tempdir_in("/tmp")
            .unwrap();
        let dir = tmp.path().join("etc/luks-ssh-unlock");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let root = TrustedRoot::for_test(dir.clone(), unsafe { libc::geteuid() });
        Fixture {
            _tmp: tmp,
            dir,
            root,
        }
    }

    impl Fixture {
        fn config(&self, name: &str, body: &str, mode: u32) {
            let p = self.dir.join(format!("{name}.env"));
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        /// Run a session over the given request lines; the parsed responses.
        fn session(&self, policy: Policy, mock: &mut Mock, input: &str) -> Vec<Value> {
            let mut out = Vec::new();
            serve(
                &self.root,
                &policy,
                mock,
                &mut Cursor::new(input.as_bytes().to_vec()),
                &mut out,
            )
            .unwrap();
            let text = String::from_utf8(out).unwrap();
            assert!(text.is_empty() || text.ends_with('\n'));
            text.lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
    }

    fn all() -> Policy {
        Policy {
            allow_force: true,
            allow_service: true,
        }
    }

    #[test]
    fn the_default_policy_denies_force_and_service() {
        assert_eq!(
            Policy::default(),
            Policy {
                allow_force: false,
                allow_service: false
            }
        );
    }

    #[test]
    fn list_probe_and_unlock_work() {
        let f = fixture();
        f.config("alpha", "SSH_HOSTNAME=a\n", 0o600);
        f.config("beta", "SSH_HOSTNAME=b\n", 0o600);
        let mut m = Mock::default();
        let r = f.session(
            Policy::default(),
            &mut m,
            concat!(
                r#"{"id":1,"request":{"verb":"list"}}"#,
                "\n",
                r#"{"id":2,"request":{"verb":"probe","name":"alpha"}}"#,
                "\n",
                r#"{"id":3,"request":{"verb":"unlock","name":"beta"}}"#,
                "\n",
            ),
        );
        assert_eq!(r.len(), 3);
        assert_eq!(r[0]["status"], "ok");
        assert_eq!(r[0]["id"], 1);
        assert_eq!(
            r[0]["data"]["targets"],
            json!([{"name": "alpha", "host": "a"}, {"name": "beta", "host": "b"}])
        );
        assert_eq!(r[1]["id"], 2);
        assert_eq!(r[2]["id"], 3);
        assert_eq!(m.calls, ["probe alpha", "unlock beta force=false"]);
    }

    #[test]
    fn force_is_denied_by_default_and_touches_nothing() {
        let f = fixture();
        f.config("a", "SSH_HOSTNAME=a\n", 0o600);
        let req = r#"{"id":9,"request":{"verb":"unlock_force","name":"a","confirm":"skip_integrity_and_host_key_checks"}}"#;
        let mut m = Mock::default();
        let r = f.session(Policy::default(), &mut m, &format!("{req}\n"));
        assert_eq!(r[0]["status"], "error");
        assert_eq!(r[0]["code"], "denied");
        assert_eq!(r[0]["id"], 9);
        assert!(m.calls.is_empty());
        // Even for a target that does not exist: the policy answers first.
        let req = req.replace("\"a\"", "\"nonexistent\"");
        let r = f.session(Policy::default(), &mut m, &format!("{req}\n"));
        assert_eq!(r[0]["code"], "denied");
        // Allowed, it is a forced unlock and nothing else.
        let r = f.session(
            Policy {
                allow_force: true,
                allow_service: false,
            },
            &mut m,
            &format!("{}\n", req.replace("nonexistent", "a")),
        );
        assert_eq!(r[0]["status"], "ok");
        assert_eq!(m.calls, ["unlock a force=true"]);
    }

    #[test]
    fn a_plain_unlock_can_never_be_forced() {
        let f = fixture();
        f.config("a", "SSH_HOSTNAME=a\n", 0o600);
        let mut m = Mock::default();
        let r = f.session(
            all(),
            &mut m,
            concat!(
                r#"{"id":1,"request":{"verb":"unlock","name":"a","force":true}}"#,
                "\n",
                r#"{"id":2,"request":{"verb":"unlock","name":"a","confirm":"skip_integrity_and_host_key_checks"}}"#,
                "\n",
            ),
        );
        assert!(r.iter().all(|v| v["code"] == "bad_request"));
        assert!(m.calls.is_empty());
    }

    #[test]
    fn service_is_denied_by_default_and_scoped_when_allowed() {
        let f = fixture();
        f.config("a", "SSH_HOSTNAME=a\n", 0o600);
        let req = r#"{"id":1,"request":{"verb":"service","name":"a","action":"stop"}}"#;
        let mut m = Mock::default();
        let r = f.session(Policy::default(), &mut m, &format!("{req}\n"));
        assert_eq!(r[0]["code"], "denied");
        assert!(m.calls.is_empty());
        let r = f.session(all(), &mut m, &format!("{req}\n"));
        assert_eq!(r[0]["status"], "ok");
        assert_eq!(m.calls, ["service a Stop"]);
    }

    #[test]
    fn unknown_and_untrusted_targets_never_reach_the_backend() {
        let f = fixture();
        f.config("loose", "SSH_HOSTNAME=a\n", 0o666);
        f.config("evil", "LD_PRELOAD=/tmp/x.so\n", 0o600);
        f.config("relative", "SSH_KEY=key\n", 0o600);
        let mut m = Mock::default();
        let line = |verb: &str, name: &str| {
            format!("{{\"id\":1,\"request\":{{\"verb\":\"{verb}\",\"name\":\"{name}\"}}}}\n")
        };
        for (name, code) in [
            ("missing", "unknown_target"),
            ("loose", "untrusted_config"),
            ("evil", "untrusted_config"),
            ("relative", "untrusted_config"),
        ] {
            for verb in ["probe", "unlock"] {
                let r = f.session(all(), &mut m, &line(verb, name));
                assert_eq!(r[0]["code"], code, "{verb} {name}");
            }
        }
        assert!(m.calls.is_empty());
    }

    #[test]
    fn a_target_cannot_be_named_by_path() {
        let f = fixture();
        f.config("a", "SSH_HOSTNAME=a\n", 0o600);
        let mut m = Mock::default();
        let r = f.session(
            all(),
            &mut m,
            concat!(
                r#"{"id":1,"request":{"verb":"probe","name":"../luks-ssh-unlock/a"}}"#,
                "\n",
                r#"{"id":2,"request":{"verb":"probe","name":"/etc/passwd"}}"#,
                "\n",
                r#"{"id":3,"request":{"verb":"probe","name":"a.env"}}"#,
                "\n",
            ),
        );
        assert_eq!(r[0]["code"], "bad_request");
        assert_eq!(r[1]["code"], "bad_request");
        // `a.env` is a valid name, but there is no `a.env.env`.
        assert_eq!(r[2]["code"], "unknown_target");
        assert!(m.calls.is_empty());
    }

    #[test]
    fn a_bad_request_does_not_end_the_session() {
        let f = fixture();
        f.config("a", "SSH_HOSTNAME=a\n", 0o600);
        let mut m = Mock::default();
        let r = f.session(
            Policy::default(),
            &mut m,
            concat!(
                "not json\n",
                "\n",
                r#"{"id":1,"request":{"verb":"rm"}}"#,
                "\n",
                r#"{"id":2,"request":{"verb":"probe","name":"a"}}"#,
                "\n",
            ),
        );
        assert_eq!(r.len(), 4);
        assert_eq!(r[0]["code"], "bad_request");
        assert!(r[0]["id"].is_null());
        assert_eq!(r[1]["code"], "bad_request");
        assert_eq!(r[2]["code"], "bad_request");
        assert_eq!(r[3]["status"], "ok");
    }

    #[test]
    fn an_oversized_line_ends_the_session() {
        let f = fixture();
        f.config("a", "SSH_HOSTNAME=a\n", 0o600);
        let mut m = Mock::default();
        let big = format!("{}\n", "x".repeat(MAX_FRAME + 1));
        let follow = "{\"id\":2,\"request\":{\"verb\":\"probe\",\"name\":\"a\"}}\n";
        let r = f.session(all(), &mut m, &format!("{big}{follow}"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0]["code"], "too_large");
        assert!(m.calls.is_empty());
        // A request that ends without a newline is not executed.
        let r = f.session(all(), &mut m, follow.trim_end());
        assert!(r.is_empty());
        assert!(m.calls.is_empty());
    }

    #[test]
    fn results_and_failures_are_fixed_shapes() {
        let f = fixture();
        // Configuration values, including a passphrase, never take part.
        f.config(
            "a",
            "LUKS_PASSPHRASE=hunter2-very-secret\nSSH_HOSTNAME=a\n",
            0o600,
        );
        let req = "{\"id\":1,\"request\":{\"verb\":\"probe\",\"name\":\"a\"}}\n";
        let mut m = Mock::default();
        let r = f.session(Policy::default(), &mut m, req);
        assert_eq!(
            r[0]["data"],
            json!({
                "kind": "probe",
                "state": "initrd",
                "unlockable": true,
                "checks": {
                    "baseline": true,
                    "integrity": "verified",
                    "host_keys_pinned": true,
                    "signature_required": false,
                },
            })
        );
        for (failure, code) in [
            (Failure::Config, "untrusted_config"),
            (Failure::Environment, "internal"),
            (Failure::Passphrase, "failed"),
            (Failure::Unverified, "denied"),
            (Failure::Timeout, "timeout"),
            (Failure::Operation, "failed"),
        ] {
            m.reply = Some(Err(failure));
            let r = f.session(Policy::default(), &mut m, req);
            assert_eq!(r[0]["status"], "error");
            assert_eq!(r[0]["code"], code);
            assert_eq!(r[0]["message"], failure.message());
            assert!(!r[0].to_string().contains("hunter2"));
        }
    }

    #[test]
    fn list_gives_a_kind_not_a_message() {
        let f = fixture();
        f.config("ok", "SSH_HOSTNAME=a\n", 0o600);
        f.config("loose", "SSH_HOSTNAME=a\n", 0o666);
        let mut m = Mock::default();
        let r = f.session(
            Policy::default(),
            &mut m,
            "{\"id\":1,\"request\":{\"verb\":\"list\"}}\n",
        );
        assert_eq!(
            r[0]["data"]["targets"],
            json!([{"name": "ok", "host": "a"}])
        );
        assert_eq!(
            r[0]["data"]["rejected"],
            json!([{"name": "loose", "reason": "untrusted"}])
        );
    }

    #[test]
    fn untrusted_config_errors_do_not_echo_values() {
        let f = fixture();
        f.config("a", "LD_PRELOAD=/tmp/SECRET-MARKER.so\n", 0o600);
        let mut m = Mock::default();
        let r = f.session(
            Policy::default(),
            &mut m,
            "{\"id\":1,\"request\":{\"verb\":\"probe\",\"name\":\"a\"}}\n",
        );
        assert_eq!(r[0]["code"], "untrusted_config");
        assert!(!r[0].to_string().contains("SECRET-MARKER"));
    }

    #[test]
    fn an_oversized_response_is_replaced() {
        let mut out = Vec::new();
        send(
            &mut out,
            &Response::Ok {
                id: 4,
                data: json!("x".repeat(MAX_RESPONSE + 1)),
            },
        )
        .unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["code"], "internal");
        assert_eq!(v["id"], 4);
    }

    #[test]
    fn every_response_is_a_single_line() {
        let mut out = Vec::new();
        send(
            &mut out,
            &Response::Ok {
                id: 1,
                data: json!("line one\nline two\r\n"),
            },
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches('\n').count(), 1);
    }

    #[test]
    fn service_argv_is_fixed_and_unambiguous() {
        let t = Target::parse("web-1").unwrap();
        assert_eq!(
            service_argv(&t, ServiceAction::Stop),
            [
                "systemctl",
                "--no-ask-password",
                "stop",
                "--",
                "luks-ssh-unlock-web-1.service"
            ]
        );
        let start = service_argv(&t, ServiceAction::Start);
        assert_eq!(start[2], "start");
    }

    #[test]
    fn child_environment_is_clean() {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "export -p"])
            .env("LD_PRELOAD", "/tmp/evil.so")
            .env("SSH_AUTH_SOCK", "/tmp/agent")
            .env("PATH", "/tmp/evil");
        apply_child_env(&mut cmd, CHILD_PATH);
        let out = cmd.output().unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(!text.contains("LD_PRELOAD"), "{text}");
        assert!(!text.contains("SSH_AUTH_SOCK"), "{text}");
        assert!(!text.contains("/tmp/evil"), "{text}");
        assert!(text.contains(CHILD_PATH), "{text}");
    }

    /// Hardening changes the whole process (umask, dumpable), so it is
    /// checked in a child process of the test binary.
    #[test]
    fn hardening_applies() {
        const FLAG: &str = "LSU_HELPER_HARDEN_CHILD";
        if std::env::var_os(FLAG).is_some() {
            harden_process();
            assert_eq!(unsafe { libc::prctl(libc::PR_GET_DUMPABLE) }, 0);
            let mut lim = libc::rlimit {
                rlim_cur: 1,
                rlim_max: 1,
            };
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_CORE, &mut lim) }, 0);
            assert_eq!((lim.rlim_cur, lim.rlim_max), (0, 0));
            let old = unsafe { libc::umask(0o022) };
            assert_eq!(old, 0o077);
            return;
        }
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "helper::tests::hardening_applies", "--nocapture"])
            .env(FLAG, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
