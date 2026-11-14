//! The real [`Backend`]: the existing settings, status and unlock logic,
//! run on a configuration that has been vetted first.
//!
//! What this relies on, and enforces before every operation:
//!
//! * The settings come from the trusted configuration only. The environment
//!   lookup handed to [`Settings::resolve`] answers from the file; the two
//!   variables that would redirect where things are read or written
//!   (`CREDENTIALS_DIRECTORY`, `TMPDIR`) are never answered, and what the
//!   verb decides (forcing, one-shot, notifications, the baseline refresh) is
//!   overwritten afterwards.
//! * Every file, tree and command the settings refer to passes the [`Vetter`]
//!   first: each component of the path belongs to root and is not writable by
//!   anyone else. The existing logic opens those paths itself, which is only
//!   safe because nobody else can change them; vetting is what makes it so.
//! * The process environment was checked when the backend was built, so what
//!   `ssh` and the checksum script inherit is a `PATH` of vetted directories
//!   and a locale, nothing else.
//! * Nothing that came from a command, a transport, a file or the target
//!   leaves this module: results are [`Outcome`]s and failures are coarse
//!   [`Failure`]s. Errors from the existing logic are matched on their type
//!   (timeout or not) and otherwise dropped.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::ctx::{Ctx, Mode};
use crate::daemon::State;
use crate::log::Logger;
use crate::process;
use crate::settings::{GlobalOpts, Notifications, Settings};
use crate::status::TargetState;

use super::reply::{Checks, Details, Failure, Health, Integrity, Luks, Observed, Outcome};
use super::request::{ServiceAction, Target};
use super::trust::{Environment, TrustError, TrustedConfig, Vetter};
use super::{Backend, CHILD_PATH, apply_child_env, service_argv};

/// How long `systemctl start` or `stop` may take.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(30);

/// Settings keys that are never answered from the configuration, because
/// they would move where scratch files go or where a credential is read.
const IGNORED_KEYS: &[&str] = &["CREDENTIALS_DIRECTORY", "TMPDIR"];

/// Longest accepted host name, user name or alias.
const MAX_IDENTIFIER: usize = 255;

pub struct SystemBackend {
    vetter: Vetter,
    env: Environment,
    scratch: PathBuf,
    default_script: Option<PathBuf>,
}

impl SystemBackend {
    /// For the privileged process: trusts root, and refuses to start unless
    /// its own environment is acceptable (see [`sanitize_process_env`]).
    ///
    /// `scratch` is a directory owned by root where per-call scratch files
    /// (copies of keys, inline known_hosts) are made, and `default_script`
    /// the checksum script to use when a configuration names none.
    pub fn new(scratch: PathBuf, default_script: Option<PathBuf>) -> Result<Self, Failure> {
        Self::build(0, std::env::vars_os(), scratch, default_script)
    }

    fn build(
        uid: u32,
        vars: impl IntoIterator<Item = (OsString, OsString)>,
        scratch: PathBuf,
        default_script: Option<PathBuf>,
    ) -> Result<Self, Failure> {
        let vetter = Vetter::new(uid).map_err(|_| Failure::Environment)?;
        let env = vetter.environment(vars).map_err(|_| Failure::Environment)?;
        Ok(Self {
            vetter,
            env,
            scratch,
            default_script,
        })
    }

    /// Resolve the settings for a call and check everything they refer to.
    fn prepare(
        &self,
        config: &TrustedConfig,
        mode: Mode,
        force: bool,
        passphrase: bool,
    ) -> Result<Ctx, Failure> {
        let mut s = Settings::resolve(&GlobalOpts::default(), &|key| {
            if IGNORED_KEYS.contains(&key) {
                None
            } else {
                config.get(key).map(str::to_string)
            }
        })
        .map_err(|_| Failure::Config)?;

        let own_script = config
            .get("INITRD_CHECKSUM_SCRIPT")
            .is_some_and(|v| !v.trim().is_empty());
        let script = if own_script {
            Some(s.initrd.script.clone())
        } else {
            self.default_script.clone()
        };

        s.force = force;
        s.once = true;
        s.debug = false;
        s.notify = Notifications::default();
        s.initrd.dir = None;
        s.initrd.script = script.clone().unwrap_or_default();
        s.tmpdir = self.scratch.clone();

        identifiers(&s)?;
        self.vet(&s, script.as_deref(), force, passphrase)?;
        self.scratch_dir()?;

        let log = Logger::with_sink(false, Arc::new(|_| {}));
        Ctx::new(s, log, mode).map_err(|_| Failure::Operation)
    }

    /// Create the scratch directory if need be and check it like any other
    /// path: files made in it must not be reachable by anyone else.
    fn scratch_dir(&self) -> Result<(), Failure> {
        match fs::DirBuilder::new().mode(0o700).create(&self.scratch) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(Failure::Environment),
        }
        self.vetter
            .dir(&self.scratch)
            .map_err(|_| Failure::Environment)
    }

    /// Everything the operation will open or run. A failure is a refusal to
    /// act, never a fallback.
    fn vet(
        &self,
        s: &Settings,
        script: Option<&Path>,
        force: bool,
        passphrase: bool,
    ) -> Result<(), Failure> {
        let v = &self.vetter;
        let config = |r: Result<(), TrustError>| r.map_err(|_| Failure::Config);

        config(v.file(&s.key))?;
        if let Some(jump) = &s.jumphost
            && jump.key != s.key
        {
            config(v.file(&jump.key))?;
        }
        for file in [&s.known_hosts_file, &s.initrd_known_hosts_file]
            .into_iter()
            .flatten()
        {
            config(v.file(file))?;
        }
        if passphrase
            && s.passphrase.is_none()
            && let Some(file) = &s.passphrase_file
        {
            config(v.file(file))?;
        }

        self.command("ssh")?;
        if s.jumphost.is_some() {
            // ssh runs a ProxyCommand through the shell.
            v.file(Path::new("/bin/sh"))
                .map_err(|_| Failure::Environment)?;
        }

        // A forced unlock runs neither the check nor anything it needs.
        if !force && let Some(baseline) = &s.initrd.file {
            config(v.optional_file(baseline).map(drop))?;
            let mut signature = baseline.as_os_str().to_owned();
            signature.push(".sig");
            config(v.optional_file(Path::new(&signature)).map(drop))?;
            let script = script.ok_or(Failure::Config)?;
            v.program(script, &self.env.dirs)
                .map_err(|_| Failure::Config)?;
            if let Some(dir) = &s.initrd.busybox_dir {
                config(v.tree(Path::new(dir)))?;
            }
            self.command("ssh-keygen")?;
        }
        Ok(())
    }

    /// A command found through the vetted `PATH`, and its interpreter.
    fn command(&self, name: &str) -> Result<PathBuf, Failure> {
        let path = self
            .vetter
            .command(name, &self.env.dirs)
            .map_err(|_| Failure::Environment)?;
        self.vetter
            .program(&path, &self.env.dirs)
            .map_err(|_| Failure::Environment)?;
        Ok(path)
    }
}

impl Backend for SystemBackend {
    fn probe(&mut self, config: &TrustedConfig) -> Result<Outcome, Failure> {
        let mut ctx = self.prepare(config, Mode::Status, false, false)?;
        let baseline = ctx.s.initrd.file.is_some();
        let report = ctx.status(true).map_err(|e| failure_of(&e))?;
        let (state, integrity) = from_report(&report.state, report.unlockable, baseline);
        Ok(Outcome::Probe {
            state,
            unlockable: report.unlockable,
            checks: Checks {
                baseline,
                integrity,
                host_keys_pinned: report.host_keys_pinned,
                signature_required: report.signature_required,
            },
            details: Some(details(&ctx.s, &report)),
        })
    }

    fn unlock(&mut self, config: &TrustedConfig, force: bool) -> Result<Outcome, Failure> {
        let mut ctx = self.prepare(config, Mode::Run, force, true)?;
        let baseline = ctx.s.initrd.file.is_some();
        let pinned = ctx.s.has_initrd_host_keys();
        if !force && !(baseline && pinned) {
            return Err(Failure::Unverified);
        }
        ctx.load_passphrase().map_err(|_| Failure::Passphrase)?;
        let state = ctx.tick().map_err(|e| failure_of(&e))?;
        Ok(Outcome::Unlock {
            state: observed(state),
            unlocked: state == State::Unlocked,
            forced: force,
            checks: Checks {
                baseline,
                integrity: unlock_integrity(state, force, baseline),
                host_keys_pinned: !force && ctx.s.has_regular_host_keys() && pinned,
                signature_required: !force && ctx.s.initrd.require_signature,
            },
        })
    }

    fn service(&mut self, target: &Target, action: ServiceAction) -> Result<Outcome, Failure> {
        let systemctl = self.command("systemctl")?;
        let argv = service_argv(target, action);
        let mut cmd = Command::new(systemctl);
        cmd.args(&argv[1..]).current_dir("/");
        apply_child_env(&mut cmd, &self.env.path);
        let out = process::run(cmd, None, Instant::now() + SERVICE_TIMEOUT)
            .map_err(|_| Failure::Operation)?;
        if out.timed_out {
            Err(Failure::Timeout)
        } else if out.success() {
            Ok(Outcome::Service { action, done: true })
        } else {
            Err(Failure::Operation)
        }
    }
}

fn failure_of(error: &anyhow::Error) -> Failure {
    if process::is_timeout(error) {
        Failure::Timeout
    } else {
        Failure::Operation
    }
}

fn observed(state: State) -> Observed {
    match state {
        State::Booted => Observed::Booted,
        State::Unreachable => Observed::Unreachable,
        State::NotInitrd => Observed::NotInitrd,
        State::InitrdSshFailed => Observed::SshFailed,
        State::Initrd => Observed::Initrd,
        State::ChecksumFailed => Observed::ChecksumFailed,
        State::UnlockFailed => Observed::UnlockFailed,
        State::Unlocked => Observed::Unlocked,
    }
}

/// What a probe saw, from the structured part of the report only (its detail
/// strings are never looked at).
fn from_report(state: &TargetState, unlockable: bool, baseline: bool) -> (Observed, Integrity) {
    match state {
        TargetState::Unlocked(_) => (Observed::Booted, Integrity::NotApplicable),
        TargetState::Initrd(_) if !unlockable => (Observed::ChecksumFailed, Integrity::Failed),
        TargetState::Initrd(_) if baseline => (Observed::Initrd, Integrity::Verified),
        TargetState::Initrd(_) => (Observed::Initrd, Integrity::NotConfigured),
        TargetState::NotInitrd => (Observed::NotInitrd, Integrity::NotApplicable),
        TargetState::SshFailed(_) => (Observed::SshFailed, Integrity::NotApplicable),
        TargetState::Unreachable => (Observed::Unreachable, Integrity::NotApplicable),
    }
}

/// The integrity check of an unlock. Without `force` the check ran before
/// anything was sent whenever the target was in its initrd.
fn unlock_integrity(state: State, forced: bool, baseline: bool) -> Integrity {
    match state {
        _ if forced => Integrity::NotApplicable,
        State::ChecksumFailed => Integrity::Failed,
        State::Initrd | State::UnlockFailed | State::Unlocked if baseline => Integrity::Verified,
        State::Initrd | State::UnlockFailed | State::Unlocked => Integrity::NotConfigured,
        State::Booted | State::Unreachable | State::NotInitrd | State::InitrdSshFailed => {
            Integrity::NotApplicable
        }
    }
}

/// The details of a probe, as values (see [`Details`]): what a name would not
/// pass as an `ssh` argument is left out, and nothing is text from the target.
fn details(s: &Settings, report: &crate::status::Report) -> Details {
    use crate::settings::LuksType;
    let host = |h: &str| plain(h, HOST_EXTRA).then(|| h.to_string());
    Details {
        user: plain(&s.username, USER_EXTRA).then(|| s.username.clone()),
        host: host(&s.hostname),
        port: Some(s.port),
        jumphost: s.jumphost.as_ref().and_then(|j| host(&j.host)),
        luks: Some(match s.luks_type {
            LuksType::Raw => Luks::Raw,
            LuksType::Systemd => Luks::Systemd,
            LuksType::SystemdTool => Luks::SystemdTool,
            LuksType::LuksMount => Luks::Mount,
        }),
        healthcheck: match (s.healthcheck.port, s.healthcheck.command.is_some()) {
            (Some(port), true) => Some(Health::PortOrCommand { port }),
            (Some(port), false) => Some(Health::Port { port }),
            (None, true) => Some(Health::Command),
            (None, false) => None,
        },
        uptime_secs: report
            .uptime
            .as_deref()
            .and_then(crate::status::uptime_seconds),
        baseline_refreshed: report.baseline_refreshed.map(|d| d.timestamp()),
        remote_baseline: report.remote_baseline_at.map(|d| d.timestamp()),
    }
}

/// Host names, users and aliases end up as `ssh` arguments and in a
/// `ProxyCommand`. Plain characters only, and nothing that starts like an
/// option.
pub(super) fn plain(value: &str, extra: &[u8]) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER
        && !value.starts_with('-')
        && value.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') || extra.contains(&b)
        })
}

pub(super) const HOST_EXTRA: &[u8] = b":%[]";
const USER_EXTRA: &[u8] = b"@";

fn identifiers(s: &Settings) -> Result<(), Failure> {
    let hosts = [
        Some(&s.hostname),
        s.connect_address.as_ref(),
        s.hostkey_alias.as_ref(),
        s.jumphost.as_ref().map(|j| &j.host),
        s.healthcheck.host.as_ref(),
    ];
    let users = [
        Some(&s.username),
        s.jumphost.as_ref().map(|j| &j.user),
        s.healthcheck.user.as_ref(),
    ];
    let ok = hosts.into_iter().flatten().all(|h| plain(h, HOST_EXTRA))
        && users.into_iter().flatten().all(|u| plain(u, USER_EXTRA));
    if ok { Ok(()) } else { Err(Failure::Config) }
}

/// Reduce the environment of the privileged process to what
/// [`SystemBackend::new`] accepts: a fixed `PATH` and locale, and a working
/// directory that is not the caller's.
///
/// # Safety
///
/// Changes the process environment, so no other thread may be reading or
/// writing it. Call it first thing in `main`.
pub unsafe fn sanitize_process_env() {
    let keys: Vec<OsString> = std::env::vars_os().map(|(k, _)| k).collect();
    for key in keys {
        unsafe { std::env::remove_var(key) };
    }
    unsafe {
        std::env::set_var("PATH", CHILD_PATH);
        std::env::set_var("LANG", "C.UTF-8");
        std::env::set_var("LC_ALL", "C.UTF-8");
    }
    let _ = std::env::set_current_dir("/");
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::Stdio;
    use std::sync::{Mutex, MutexGuard};

    use serde_json::{Value, json};

    use super::super::trust::TrustedRoot;
    use super::super::{Policy, serve};
    use super::*;

    #[test]
    fn identifiers_are_plain() {
        for ok in ["host", "a.b-c_d", "10.0.0.1", "fe80::1%eth0", "[::1]"] {
            assert!(plain(ok, HOST_EXTRA), "{ok}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "-",
            "a b",
            "a\tb",
            "a\nb",
            "a;b",
            "$(id)",
            "a'b",
            "a\"b",
            "a/b",
            "a=b",
            "a,b",
            "ä",
            &"x".repeat(256),
        ] {
            assert!(!plain(bad, HOST_EXTRA), "{bad:?}");
        }
        assert!(plain("user@example", USER_EXTRA));
        assert!(!plain("user@example", b""));
        assert!(!plain("-l", USER_EXTRA));
    }

    #[test]
    fn a_probe_is_described_by_structure() {
        use TargetState::*;
        let cases = [
            (
                Unlocked("x".into()),
                true,
                true,
                Observed::Booted,
                Integrity::NotApplicable,
            ),
            (
                Initrd("x".into()),
                true,
                true,
                Observed::Initrd,
                Integrity::Verified,
            ),
            (
                Initrd("x".into()),
                true,
                false,
                Observed::Initrd,
                Integrity::NotConfigured,
            ),
            (
                Initrd("x".into()),
                false,
                true,
                Observed::ChecksumFailed,
                Integrity::Failed,
            ),
            (
                NotInitrd,
                false,
                true,
                Observed::NotInitrd,
                Integrity::NotApplicable,
            ),
            (
                SshFailed("x".into()),
                false,
                true,
                Observed::SshFailed,
                Integrity::NotApplicable,
            ),
            (
                Unreachable,
                false,
                true,
                Observed::Unreachable,
                Integrity::NotApplicable,
            ),
        ];
        for (state, unlockable, baseline, observed, integrity) in cases {
            assert_eq!(
                from_report(&state, unlockable, baseline),
                (observed, integrity)
            );
        }
    }

    #[test]
    fn an_unlock_claims_a_check_only_when_it_ran() {
        assert_eq!(
            unlock_integrity(State::Unlocked, false, true),
            Integrity::Verified
        );
        assert_eq!(
            unlock_integrity(State::UnlockFailed, false, true),
            Integrity::Verified
        );
        assert_eq!(
            unlock_integrity(State::ChecksumFailed, false, true),
            Integrity::Failed
        );
        assert_eq!(
            unlock_integrity(State::NotInitrd, false, true),
            Integrity::NotApplicable
        );
        assert_eq!(
            unlock_integrity(State::Unlocked, true, true),
            Integrity::NotApplicable
        );
        assert_eq!(
            unlock_integrity(State::ChecksumFailed, true, true),
            Integrity::NotApplicable
        );
        assert_eq!(
            unlock_integrity(State::Unlocked, false, false),
            Integrity::NotConfigured
        );
    }

    #[test]
    fn every_daemon_state_has_an_observation() {
        for (state, expect) in [
            (State::Booted, Observed::Booted),
            (State::Unreachable, Observed::Unreachable),
            (State::NotInitrd, Observed::NotInitrd),
            (State::InitrdSshFailed, Observed::SshFailed),
            (State::Initrd, Observed::Initrd),
            (State::ChecksumFailed, Observed::ChecksumFailed),
            (State::UnlockFailed, Observed::UnlockFailed),
            (State::Unlocked, Observed::Unlocked),
        ] {
            assert_eq!(observed(state), expect);
        }
    }

    const PASSPHRASE: &str = "PASSPHRASE-MARKER-7f3a";
    const KEY_LINE: &str = "KEY-MARKER-91c2";
    const OUTPUT_MARKER: &str = "OUTPUT-MARKER-5d10";

    const SSH: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done >> @LOG@/ssh.argv
printf -- '--\n' >> @LOG@/ssh.argv
if [ -e @SCENE@/ssh-broken ]; then
  read -r key < @KEYS@/ssh_key
  printf 'ssh: broken (%s)\n' "$key" >&2
  exit 255
fi
for a in "$@"; do last=$a; done
case "$last" in
  'test -e /etc/initrd-release')
    if [ -e @SCENE@/not-initrd ]; then exit 1; fi
    exit 0 ;;
  uptime*) exit 0 ;;
esac
IFS= read -r pw
printf '%s\n' "$pw" >> @LOG@/ssh.stdin
if [ -e @SCENE@/ssh-fails ]; then
  read -r key < @KEYS@/ssh_key
  printf 'permission denied (%s) %s\n' "$pw" "$key" >&2
  exit 1
fi
exit 0
"#;

    const KEYGEN: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done >> @LOG@/keygen.argv
cat > /dev/null
exit 0
"#;

    const CHECKSUM: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done >> @LOG@/checksum.argv
printf 'PATH=%s HOME=%s LD=%s\n' "$PATH" "${HOME-unset}" "${LD_PRELOAD-unset}" >> @LOG@/checksum.env
if [ -e @SCENE@/checksum-fails ]; then
  read -r pw < @KEYS@/passphrase
  printf '+++ new\n--- old\n+changed\n-gone\nERR %s @OUTPUT@\n' "$pw"
  exit 1
fi
exit 0
"#;

    const SYSTEMCTL: &str = r#"#!/bin/sh
for a in "$@"; do printf '%s\n' "$a"; done >> @LOG@/systemctl.argv
if [ -e @SCENE@/systemctl-fails ]; then
  printf 'systemctl: @OUTPUT@\n' >&2
  exit 1
fi
exit 0
"#;

    const PROBE: &str = r#"{"id":1,"request":{"verb":"probe","name":"alpha"}}"#;
    const UNLOCK: &str = r#"{"id":2,"request":{"verb":"unlock","name":"alpha"}}"#;
    const FORCE: &str = r#"{"id":3,"request":{"verb":"unlock_force","name":"alpha","confirm":"skip_integrity_and_host_key_checks"}}"#;
    const STOP: &str = r#"{"id":4,"request":{"verb":"service","name":"alpha","action":"stop"}}"#;

    static SERIAL: Mutex<()> = Mutex::new(());

    fn euid() -> u32 {
        unsafe { libc::geteuid() }
    }

    /// A directory standing in for the root file system of a host: the
    /// configuration, the commands on `PATH`, the files the configuration
    /// names, and logs of what the fake commands were asked to do. Scenes run
    /// one at a time so that no test forks while another one has a script
    /// open for writing.
    struct Scene {
        _serial: MutexGuard<'static, ()>,
        tmp: tempfile::TempDir,
    }

    struct Run {
        lines: Vec<Value>,
        raw: String,
    }

    impl Run {
        fn first(&self) -> &Value {
            &self.lines[0]
        }

        fn code(&self) -> &str {
            self.first()["code"].as_str().unwrap_or("")
        }
    }

    impl Scene {
        fn new() -> Self {
            let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let tmp = tempfile::Builder::new()
                .prefix("lsu-scene-")
                .tempdir_in("/tmp")
                .unwrap();
            let scene = Self {
                _serial: serial,
                tmp,
            };
            for dir in ["etc", "bin", "lib", "keys", "log", "evil"] {
                let mode = if dir == "evil" { 0o777 } else { 0o755 };
                scene.mkdir(dir, mode);
            }
            scene.put("bin/ssh", &scene.script(SSH), 0o755);
            scene.put("bin/ssh-keygen", &scene.script(KEYGEN), 0o755);
            scene.put("bin/systemctl", &scene.script(SYSTEMCTL), 0o755);
            scene.put("lib/initrd-checksum", &scene.script(CHECKSUM), 0o755);
            scene.put("keys/ssh_key", &format!("{KEY_LINE}\n"), 0o600);
            scene.put(
                "keys/known_hosts",
                "target.example ssh-ed25519 AAAA\n",
                0o644,
            );
            scene.put(
                "keys/initrd_known_hosts",
                "target.example ssh-ed25519 BBBB\n",
                0o644,
            );
            scene.put("keys/passphrase", &format!("{PASSPHRASE}\n"), 0o600);
            scene.put("keys/baseline", "baseline\n", 0o644);
            scene.config(&[], &[]);
            scene
        }

        fn p(&self, rel: &str) -> PathBuf {
            self.tmp.path().join(rel)
        }

        fn script(&self, template: &str) -> String {
            template
                .replace("@LOG@", &self.p("log").display().to_string())
                .replace("@KEYS@", &self.p("keys").display().to_string())
                .replace("@SCENE@", &self.tmp.path().display().to_string())
                .replace("@OUTPUT@", OUTPUT_MARKER)
        }

        fn mkdir(&self, rel: &str, mode: u32) {
            fs::create_dir_all(self.p(rel)).unwrap();
            fs::set_permissions(self.p(rel), fs::Permissions::from_mode(mode)).unwrap();
        }

        fn put(&self, rel: &str, content: &str, mode: u32) {
            let path = self.p(rel);
            let _ = fs::remove_file(&path);
            fs::write(&path, content).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        }

        fn chmod(&self, rel: &str, mode: u32) {
            fs::set_permissions(self.p(rel), fs::Permissions::from_mode(mode)).unwrap();
        }

        fn mark(&self, name: &str) {
            fs::write(self.p(name), "").unwrap();
        }

        /// `etc/alpha.env`: the standard settings without the keys that
        /// start with one of `drop`, plus `add`.
        fn config(&self, drop: &[&str], add: &[String]) {
            let keys = self.p("keys");
            let mut lines = vec![
                "SSH_HOSTNAME=target.example".to_string(),
                format!("SSH_KEY={}", keys.join("ssh_key").display()),
                format!(
                    "SSH_KNOWN_HOSTS_FILE={}",
                    keys.join("known_hosts").display()
                ),
                format!(
                    "SSH_INITRD_KNOWN_HOSTS_FILE={}",
                    keys.join("initrd_known_hosts").display()
                ),
                format!("LUKS_PASSPHRASE_FILE={}", keys.join("passphrase").display()),
                format!("INITRD_CHECKSUM_FILE={}", keys.join("baseline").display()),
                "SKIP_SSH_PORT_CHECK=1".to_string(),
            ];
            lines.retain(|l| !drop.iter().any(|d| l.starts_with(d)));
            lines.extend(add.iter().cloned());
            self.put("etc/alpha.env", &(lines.join("\n") + "\n"), 0o600);
        }

        fn log(&self, name: &str) -> Option<String> {
            fs::read_to_string(self.p("log").join(name)).ok()
        }

        fn ran_nothing(&self) -> bool {
            ["ssh.argv", "ssh.stdin", "checksum.argv", "keygen.argv"]
                .iter()
                .all(|name| self.log(name).is_none())
        }

        fn run(&self, request: &str) -> Run {
            self.run_with(request, &[], None)
        }

        /// Serve `request` in a fresh process that has only `PATH`, the
        /// locale and what `extra` adds, as the privileged process would.
        fn run_with(&self, request: &str, extra: &[(&str, &str)], path: Option<&str>) -> Run {
            fs::write(self.p("request"), format!("{request}\n")).unwrap();
            let _ = fs::remove_file(self.p("response"));
            let path = path
                .map(str::to_string)
                .unwrap_or_else(|| self.p("bin").display().to_string());
            let out = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "helper::backend::tests::scene_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env_clear()
                .env("PATH", path)
                .env("LANG", "C.UTF-8")
                .env("LC_ALL", "C.UTF-8")
                .env("LSU_SCENE", self.tmp.path())
                .envs(extra.iter().copied())
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let raw = fs::read_to_string(self.p("response")).unwrap_or_else(|_| {
                panic!(
                    "no response: {}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
            });
            let lines = raw
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            Run { lines, raw }
        }
    }

    /// The body of the privileged process in the tests: runs only inside the
    /// process that [`Scene::run_with`] starts.
    #[test]
    fn scene_child() {
        let Some(scene) = std::env::var_os("LSU_SCENE") else {
            return;
        };
        unsafe { std::env::remove_var("LSU_SCENE") };
        let scene = PathBuf::from(scene);
        let request = fs::read(scene.join("request")).unwrap();
        let root = TrustedRoot::for_test(scene.join("etc"), euid());
        let policy = Policy {
            allow_force: true,
            allow_service: true,
        };
        let mut out = Vec::new();
        match SystemBackend::build(
            euid(),
            std::env::vars_os(),
            scene.join("scratch"),
            Some(scene.join("lib/initrd-checksum")),
        ) {
            Ok(mut backend) => {
                serve(
                    &root,
                    &policy,
                    &mut backend,
                    &mut Cursor::new(request),
                    &mut out,
                )
                .unwrap();
            }
            Err(failure) => {
                out = format!("{}\n", json!({"build_failed": failure.message()})).into_bytes();
            }
        }
        fs::write(scene.join("response"), out).unwrap();
    }

    fn checks(integrity: &str, pinned: bool) -> Value {
        json!({
            "baseline": integrity != "not-configured",
            "integrity": integrity,
            "host_keys_pinned": pinned,
            "signature_required": false,
        })
    }

    /// The probe answer without its details (checked on their own).
    fn without_details(data: &Value) -> Value {
        let mut data = data.clone();
        if let Some(details) = data.as_object_mut().and_then(|o| o.remove("details")) {
            assert_details_are_values(&details);
        }
        data
    }

    /// Details carry names, numbers and enums only: no text from anywhere else.
    fn assert_details_are_values(details: &Value) {
        let map = details.as_object().expect("details are an object");
        for (key, value) in map {
            match (key.as_str(), value) {
                (_, Value::Null | Value::Number(_)) => {}
                ("user" | "host" | "jumphost", Value::String(s)) => {
                    assert!(plain(s, b":%[]@"), "{key}={s}");
                }
                ("luks", Value::String(s)) => {
                    assert!(["raw", "systemd", "systemd-tool", "luks-mount"].contains(&s.as_str()));
                }
                ("healthcheck", Value::Object(h)) => {
                    assert!(h.keys().all(|k| k == "by" || k == "port"), "{h:?}");
                }
                other => panic!("unexpected detail {other:?}"),
            }
        }
    }

    #[test]
    fn a_probe_verifies_and_sends_nothing() {
        let scene = Scene::new();
        let run = scene.run(PROBE);
        assert_eq!(
            without_details(&run.first()["data"]),
            json!({
                "kind": "probe",
                "state": "initrd",
                "unlockable": true,
                "checks": checks("verified", true),
            })
        );
        let argv = scene.log("checksum.argv").unwrap();
        assert!(argv.contains("--diff"));
        assert!(argv.contains(&scene.p("keys/baseline").display().to_string()));
        assert!(scene.log("ssh.stdin").is_none());
    }

    #[test]
    fn a_probe_reports_other_states() {
        let scene = Scene::new();
        scene.mark("not-initrd");
        assert_eq!(scene.run(PROBE).first()["data"]["state"], "not-initrd");
        drop(scene);
        let scene = Scene::new();
        scene.mark("ssh-broken");
        let run = scene.run(PROBE);
        assert_eq!(run.first()["data"]["state"], "ssh-failed");
        assert!(!run.raw.contains(KEY_LINE));
    }

    #[test]
    fn a_probe_without_a_baseline_says_so() {
        let scene = Scene::new();
        scene.config(
            &["INITRD_CHECKSUM_FILE", "SSH_INITRD_KNOWN_HOSTS_FILE"],
            &[],
        );
        let run = scene.run(PROBE);
        assert_eq!(
            run.first()["data"]["checks"],
            checks("not-configured", false)
        );
        assert!(scene.log("checksum.argv").is_none());
    }

    #[test]
    fn a_probe_that_fails_the_check_is_not_unlockable() {
        let scene = Scene::new();
        scene.mark("checksum-fails");
        let run = scene.run(PROBE);
        assert_eq!(
            without_details(&run.first()["data"]),
            json!({
                "kind": "probe",
                "state": "checksum-failed",
                "unlockable": false,
                "checks": checks("failed", true),
            })
        );
        // What the script printed, including a passphrase, stays with it.
        assert!(!run.raw.contains(PASSPHRASE) && !run.raw.contains(OUTPUT_MARKER));
    }

    #[test]
    fn an_unlock_sends_the_passphrase_to_a_verified_pinned_target() {
        let scene = Scene::new();
        let run = scene.run(UNLOCK);
        assert_eq!(
            run.first()["data"],
            json!({
                "kind": "unlock",
                "state": "unlocked",
                "unlocked": true,
                "forced": false,
                "checks": checks("verified", true),
            })
        );
        assert_eq!(scene.log("ssh.stdin").unwrap(), format!("{PASSPHRASE}\n"));
        assert!(scene.log("checksum.argv").is_some());
        let argv = scene.log("ssh.argv").unwrap();
        assert!(argv.contains("StrictHostKeyChecking=yes"));
        assert!(!argv.contains("StrictHostKeyChecking=no"));
        assert!(!run.raw.contains(PASSPHRASE));
    }

    #[test]
    fn a_failed_check_stops_the_unlock_before_the_passphrase_moves() {
        let scene = Scene::new();
        scene.mark("checksum-fails");
        let run = scene.run(UNLOCK);
        assert_eq!(
            run.first()["data"],
            json!({
                "kind": "unlock",
                "state": "checksum-failed",
                "unlocked": false,
                "forced": false,
                "checks": checks("failed", true),
            })
        );
        assert!(scene.log("ssh.stdin").is_none());
        assert!(!run.raw.contains(PASSPHRASE) && !run.raw.contains(OUTPUT_MARKER));
    }

    #[test]
    fn a_forced_unlock_skips_the_check_and_pinning_and_says_so() {
        let scene = Scene::new();
        scene.mark("checksum-fails");
        // A baseline that is not trusted does not matter when forcing: it is not read.
        scene.chmod("keys/baseline", 0o666);
        let run = scene.run(FORCE);
        assert_eq!(
            run.first()["data"],
            json!({
                "kind": "unlock",
                "state": "unlocked",
                "unlocked": true,
                "forced": true,
                "checks": checks("not-applicable", false),
            })
        );
        assert!(scene.log("checksum.argv").is_none());
        assert_eq!(scene.log("ssh.stdin").unwrap(), format!("{PASSPHRASE}\n"));
        let argv = scene.log("ssh.argv").unwrap();
        assert!(argv.contains("StrictHostKeyChecking=no"));
        assert!(!argv.contains("StrictHostKeyChecking=yes"));
        assert!(argv.contains("GlobalKnownHostsFile=/dev/null"));
    }

    #[test]
    fn a_forced_unlock_still_needs_the_key_and_the_passphrase_to_be_trusted() {
        for rel in ["keys/ssh_key", "keys/passphrase"] {
            let scene = Scene::new();
            scene.chmod(rel, 0o666);
            let run = scene.run(FORCE);
            assert_eq!(run.code(), "untrusted_config", "{rel}");
            assert!(scene.ran_nothing(), "{rel}");
        }
    }

    #[test]
    fn a_plain_unlock_needs_a_baseline_and_pinned_initrd_host_keys() {
        for drop in [
            &["INITRD_CHECKSUM_FILE"][..],
            &["SSH_INITRD_KNOWN_HOSTS_FILE"][..],
        ] {
            let scene = Scene::new();
            scene.config(drop, &[]);
            let run = scene.run(UNLOCK);
            assert_eq!(run.code(), "denied", "{drop:?}");
            assert_eq!(run.first()["message"], Failure::Unverified.message());
            assert!(scene.ran_nothing(), "{drop:?}");
            // Forcing is the explicit way past it.
            assert_eq!(scene.run(FORCE).first()["status"], "ok", "{drop:?}");
        }
    }

    #[test]
    fn nothing_from_a_failing_transport_comes_back() {
        let scene = Scene::new();
        scene.mark("ssh-fails");
        let run = scene.run(UNLOCK);
        assert_eq!(run.first()["data"]["state"], "unlock-failed");
        assert_eq!(run.first()["data"]["unlocked"], false);
        // It was sent, and the failure echoed it together with the key.
        assert_eq!(scene.log("ssh.stdin").unwrap(), format!("{PASSPHRASE}\n"));
        assert!(!run.raw.contains(PASSPHRASE) && !run.raw.contains(KEY_LINE));
    }

    #[test]
    fn untrusted_inputs_are_refused_before_anything_runs() {
        type Mutation = Box<dyn Fn(&Scene)>;
        let cases: Vec<(&str, Mutation)> = vec![
            ("key", Box::new(|s| s.chmod("keys/ssh_key", 0o666))),
            (
                "passphrase",
                Box::new(|s| s.chmod("keys/passphrase", 0o666)),
            ),
            (
                "known_hosts",
                Box::new(|s| s.chmod("keys/known_hosts", 0o666)),
            ),
            (
                "initrd_known_hosts",
                Box::new(|s| s.chmod("keys/initrd_known_hosts", 0o666)),
            ),
            ("baseline", Box::new(|s| s.chmod("keys/baseline", 0o666))),
            (
                "signature",
                Box::new(|s| s.put("keys/baseline.sig", "sig\n", 0o666)),
            ),
            (
                "script",
                Box::new(|s| s.chmod("lib/initrd-checksum", 0o666)),
            ),
            ("script directory", Box::new(|s| s.chmod("lib", 0o777))),
            ("keys directory", Box::new(|s| s.chmod("keys", 0o777))),
            (
                "script interpreter",
                Box::new(|s| {
                    s.put("evil/sh", "", 0o755);
                    let shebang = format!("#!{}\n", s.p("evil/sh").display());
                    s.put("lib/initrd-checksum", &shebang, 0o755);
                }),
            ),
            (
                "key behind a link",
                Box::new(|s| {
                    s.put("evil/key", "k\n", 0o600);
                    fs::remove_file(s.p("keys/ssh_key")).unwrap();
                    symlink(s.p("evil/key"), s.p("keys/ssh_key")).unwrap();
                }),
            ),
            (
                "busybox tree",
                Box::new(|s| {
                    s.mkdir("tree", 0o755);
                    s.put("tree/busybox", "", 0o666);
                    let dir = s.p("tree").display().to_string();
                    s.config(&[], &[format!("INITRD_CHECKSUM_BUSYBOX_DIR={dir}")]);
                }),
            ),
        ];
        for (name, mutate) in cases {
            let scene = Scene::new();
            mutate(&scene);
            let run = scene.run(UNLOCK);
            assert_eq!(run.code(), "untrusted_config", "{name}");
            assert_eq!(run.first()["message"], Failure::Config.message(), "{name}");
            assert!(scene.ran_nothing(), "{name}");
        }
    }

    #[test]
    fn a_probe_vets_only_what_it_uses() {
        let scene = Scene::new();
        scene.chmod("keys/passphrase", 0o666);
        assert_eq!(scene.run(PROBE).first()["status"], "ok");
        scene.chmod("keys/ssh_key", 0o666);
        assert_eq!(scene.run(PROBE).code(), "untrusted_config");
    }

    #[test]
    fn a_missing_signature_is_fine_and_a_present_one_is_checked() {
        let scene = Scene::new();
        assert_eq!(scene.run(UNLOCK).first()["status"], "ok");
        assert!(scene.log("keygen.argv").is_none());
        scene.put("keys/baseline.sig", "sig\n", 0o644);
        assert_eq!(scene.run(UNLOCK).first()["status"], "ok");
        let argv = scene.log("keygen.argv").unwrap();
        assert!(argv.contains("initrd-checksum"));
    }

    #[test]
    fn the_commands_must_be_trusted() {
        let scene = Scene::new();
        scene.chmod("bin/ssh", 0o777);
        let run = scene.run(PROBE);
        assert_eq!(run.code(), "internal");
        assert!(scene.ran_nothing());
        drop(scene);

        let scene = Scene::new();
        scene.put("evil/sh", "", 0o755);
        let shebang = format!("#!{}\n", scene.p("evil/sh").display());
        scene.put("bin/ssh", &shebang, 0o755);
        assert_eq!(scene.run(PROBE).code(), "internal");
        drop(scene);

        let scene = Scene::new();
        scene.chmod("bin/systemctl", 0o777);
        assert_eq!(scene.run(STOP).code(), "internal");
        assert!(scene.log("systemctl.argv").is_none());
    }

    #[test]
    fn a_dirty_environment_stops_the_helper_from_starting() {
        let build_failed = json!({"build_failed": Failure::Environment.message()});
        for (name, value) in [
            ("LD_PRELOAD", "/nonexistent.so"),
            ("LD_LIBRARY_PATH", "/nonexistent"),
            ("BASH_ENV", "/nonexistent"),
            ("SSH_AUTH_SOCK", "/nonexistent"),
            ("HOME", "/nonexistent"),
            ("TMPDIR", "/nonexistent"),
            ("LANG", "$(id)"),
        ] {
            let scene = Scene::new();
            let run = scene.run_with(PROBE, &[(name, value)], None);
            assert_eq!(run.first(), &build_failed, "{name}");
            assert!(scene.ran_nothing(), "{name}");
        }
        let scene = Scene::new();
        let bin = scene.p("bin").display().to_string();
        for path in [
            format!("{bin}:{}", scene.p("evil").display()),
            format!("{bin}:"),
            format!("{bin}:relative"),
            String::new(),
        ] {
            let run = scene.run_with(PROBE, &[], Some(&path));
            assert_eq!(run.first(), &build_failed, "{path:?}");
        }
        assert_eq!(
            scene
                .run_with(PROBE, &[("LANG", "de_DE.UTF-8")], None)
                .first()["status"],
            "ok"
        );
    }

    #[test]
    fn the_children_inherit_only_what_was_vetted() {
        let scene = Scene::new();
        scene.run(UNLOCK);
        let env = scene.log("checksum.env").unwrap();
        assert_eq!(
            env.trim(),
            format!("PATH={} HOME=unset LD=unset", scene.p("bin").display())
        );
    }

    #[test]
    fn redirecting_settings_are_not_honoured() {
        let scene = Scene::new();
        scene.put("evil/luks-passphrase", "EVIL-PASSPHRASE\n", 0o644);
        let evil = scene.p("evil").display().to_string();
        scene.config(
            &[],
            &[
                format!("CREDENTIALS_DIRECTORY={evil}"),
                format!("TMPDIR={evil}"),
            ],
        );
        let run = scene.run(UNLOCK);
        assert_eq!(run.first()["data"]["state"], "unlocked");
        assert_eq!(scene.log("ssh.stdin").unwrap(), format!("{PASSPHRASE}\n"));
        let left: Vec<_> = fs::read_dir(scene.p("evil"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [OsString::from("luks-passphrase")]);
    }

    #[test]
    fn identifiers_that_would_become_options_are_refused() {
        for (key, value) in [
            ("SSH_HOSTNAME", "-oProxyCommand=id"),
            ("SSH_HOSTNAME", "a b"),
            ("SSH_USERNAME", "-l"),
            ("SSH_USERNAME", "root;id"),
            ("SSH_CONNECT_ADDRESS", "-x"),
            ("SSH_HOSTKEY_ALIAS", "a;b"),
            ("SSH_JUMPHOST", "-J"),
            ("SSH_JUMPHOST_USERNAME", "a b"),
            ("HEALTHCHECK_REMOTE_HOSTNAME", "-W"),
        ] {
            let scene = Scene::new();
            let replaced = [key];
            let dropped: &[&str] = if key == "SSH_HOSTNAME" {
                &replaced
            } else {
                &[]
            };
            let mut added = vec![format!("{key}={value}")];
            if key == "SSH_JUMPHOST_USERNAME" {
                added.push("SSH_JUMPHOST=bastion".to_string());
            }
            scene.config(dropped, &added);
            let run = scene.run(PROBE);
            assert_eq!(run.code(), "untrusted_config", "{key}={value}");
            assert!(scene.ran_nothing(), "{key}={value}");
        }
    }

    #[test]
    fn a_service_runs_its_own_unit_only() {
        let scene = Scene::new();
        let run = scene.run(STOP);
        assert_eq!(
            run.first()["data"],
            json!({"kind": "service", "action": "stop", "done": true})
        );
        assert_eq!(
            scene.log("systemctl.argv").unwrap(),
            "--no-ask-password\nstop\n--\nluks-ssh-unlock-alpha.service\n"
        );
    }

    #[test]
    fn a_failing_service_command_says_nothing() {
        let scene = Scene::new();
        scene.mark("systemctl-fails");
        let run = scene.run(STOP);
        assert_eq!(run.code(), "failed");
        assert_eq!(run.first()["message"], Failure::Operation.message());
        assert!(!run.raw.contains(OUTPUT_MARKER));
    }

    #[test]
    fn the_process_environment_can_be_reduced_to_what_is_accepted() {
        if let Some(path) = std::env::var_os("LSU_SANITIZE_OUT") {
            unsafe { sanitize_process_env() };
            let mut keys: Vec<String> = std::env::vars_os()
                .map(|(k, _)| k.to_string_lossy().into_owned())
                .collect();
            keys.sort();
            let built = SystemBackend::build(
                euid(),
                std::env::vars_os(),
                PathBuf::from("/nonexistent"),
                None,
            )
            .is_ok();
            let cwd = std::env::current_dir().unwrap();
            fs::write(
                path,
                format!("{}\n{built}\n{}\n", keys.join(","), cwd.display()),
            )
            .unwrap();
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let out = t.path().join("out");
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "helper::backend::tests::the_process_environment_can_be_reduced_to_what_is_accepted",
                "--test-threads=1",
            ])
            .env_clear()
            .env("PATH", "/tmp")
            .env("LANG", "de_DE.UTF-8")
            .env("LD_LIBRARY_PATH", "/nonexistent")
            .env("HOME", "/tmp")
            .env("SSH_AUTH_SOCK", "/nonexistent")
            .env("TMPDIR", "/nonexistent")
            .env("LSU_SANITIZE_OUT", &out)
            .current_dir("/tmp")
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            fs::read_to_string(out).unwrap(),
            "LANG,LC_ALL,PATH\ntrue\n/\n"
        );
    }
}
