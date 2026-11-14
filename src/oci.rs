//! Serial console of an Oracle Cloud (OCI) instance, through the external
//! `oci` CLI and `ssh`.
//!
//! A port of the `oci::console` shell helper, with these differences:
//!
//! * Only a connection this run created is ever used or deleted. Existing
//!   connections (possibly someone else's) are never listed, reused or
//!   removed.
//! * The connection string the service returns is never handed to a shell. It
//!   is tokenised, validated against the exact shape Oracle documents, and the
//!   `ssh` command line is rebuilt from the validated parts.
//! * Errors from `oci` are reduced to the service's error code and HTTP
//!   status: raw output (which may hold tokens or connection strings) is never
//!   echoed.
//! * The connection is owned by a small helper (`sh`) in a session of its own.
//!   It creates the connection (tagged with a nonce), reports its OCID, and
//!   deletes it as soon as its stdin is closed. Whatever happens to us (a
//!   SIGTERM to the process group, the SIGKILL that follows it, a crash), the
//!   pipe closes and the helper still deletes the connection, also when a
//!   create request was still in flight: it then waits for the request, finds
//!   the connection by its nonce and deletes it.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, Read};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::process::shell_quote;

pub mod vnc;

/// How long one read-only `oci` call may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(120);
const GET_TIMEOUT: Duration = Duration::from_secs(60);
/// How long we wait for the helper to report the new connection (it gives the
/// create request 90 s itself).
const CREATE_WAIT: Duration = Duration::from_secs(120);
/// How long we wait for the helper to delete the connection after a normal
/// end of the session, and after a signal. The helper goes on regardless.
const DELETE_WAIT: Duration = Duration::from_secs(75);
const CLEANUP_WAIT: Duration = Duration::from_secs(8);
/// How long a new connection may take to become ACTIVE.
const ACTIVE_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// After SIGTERM, how long `ssh` has to end before it is killed.
const SSH_GRACE: Duration = Duration::from_secs(3);
const MAX_OUTPUT: u64 = 8 * 1024 * 1024;

/// Settings of one serial console session.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Instance display name, or its OCID (`ocid1.instance.`...). Required.
    pub instance: String,
    /// RSA private key; `<identity>.pub` must exist next to it. Default:
    /// `~/.ssh/id_rsa`. A leading `~` is expanded.
    pub identity: Option<PathBuf>,
    /// `oci --config-file`
    pub config_file: Option<PathBuf>,
    /// `oci --cli-rc-file`
    pub cli_rc_file: Option<PathBuf>,
    /// `oci --profile`
    pub profile: Option<String>,
    /// Where `config` and `cli_rc` are looked for when the two options above
    /// are not given, as the shell `oci` wrapper does. Default:
    /// `$XDG_CONFIG_HOME/oci` (`~/.config/oci`). A file that is not there is
    /// skipped, and so is one the CLI's own environment variable
    /// (`OCI_CLI_CONFIG_FILE`, `OCI_CLI_RC_FILE`) already names.
    pub config_dir: Option<PathBuf>,
    /// `--compartment-id` for the lookup by name (default: what the CLI
    /// configuration says, as with the shell helper)
    pub compartment_id: Option<String>,
    /// Do not delete the console connection on exit
    pub keep: bool,
    /// The `oci` binary (default: `oci` from the PATH)
    pub oci_bin: Option<PathBuf>,
    /// The `ssh` binary (default: `ssh` from the PATH)
    pub ssh_bin: Option<PathBuf>,
}

/// Open the serial console and block until the session ends. Returns the exit
/// code: `ssh`'s own, or 128 plus the signal number when a signal ended the
/// session. Errors are already sanitised.
pub fn run(opts: &Options) -> Result<i32> {
    // The signal state is process-wide: one session at a time.
    let _lock = RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _signals = SignalGuard::install();
    match session(opts) {
        Ok(code) => Ok(code),
        Err(err) => match err.downcast_ref::<Interrupted>() {
            Some(Interrupted(sig)) => {
                info(&format!("interrupted (signal {sig})"));
                Ok(128 + sig)
            }
            None => Err(err),
        },
    }
}

static RUN_LOCK: Mutex<()> = Mutex::new(());
static SIGNAL: AtomicI32 = AtomicI32::new(0);

fn info(msg: &str) {
    eprintln!("oci: {msg}");
}

// ---------------------------------------------------------------- signals

#[derive(Debug)]
struct Interrupted(i32);

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interrupted by signal {}", self.0)
    }
}

impl std::error::Error for Interrupted {}

extern "C" fn on_signal(sig: libc::c_int) {
    SIGNAL.store(sig, Ordering::SeqCst);
}

/// Catches SIGINT, SIGTERM and SIGHUP into [`SIGNAL`] for its lifetime, and
/// puts the previous handlers back afterwards.
struct SignalGuard {
    saved: Vec<(libc::c_int, libc::sigaction)>,
}

impl SignalGuard {
    fn install() -> Self {
        SIGNAL.store(0, Ordering::SeqCst);
        let mut saved = Vec::new();
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: plain sigaction(2) with an async-signal-safe handler
            // that only stores into an atomic.
            unsafe {
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
                libc::sigemptyset(&mut new.sa_mask);
                new.sa_flags = 0;
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &new, &mut old) == 0 {
                    saved.push((sig, old));
                }
            }
        }
        Self { saved }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (sig, old) in &self.saved {
            // SAFETY: restores what sigaction returned earlier.
            unsafe { libc::sigaction(*sig, old, std::ptr::null_mut()) };
        }
        SIGNAL.store(0, Ordering::SeqCst);
    }
}

fn check_signal() -> Result<()> {
    match SIGNAL.load(Ordering::SeqCst) {
        0 => Ok(()),
        sig => Err(Interrupted(sig).into()),
    }
}

/// Sleep for TOTAL, in short steps, ending early with INTERRUPT's error.
fn sleep_with(total: Duration, interrupt: &dyn Fn() -> Result<()>) -> Result<()> {
    let end = Instant::now() + total;
    loop {
        interrupt()?;
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(Duration::from_millis(100)));
    }
}

// ------------------------------------------------------- running commands

#[derive(Debug)]
enum RunError {
    Spawn(io::Error),
    Timeout,
    Interrupted(i32),
    Failed { code: Option<i32>, stderr: String },
}

fn read_bounded<R: Read + Send + 'static>(mut r: R) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut r).take(MAX_OUTPUT).read_to_end(&mut buf);
        let _ = io::copy(&mut r, &mut io::sink());
        buf
    })
}

/// Run the read-only command CMD to the end and return its stdout. It is
/// killed when a signal arrives (there is nothing to clean up after it). A
/// non-zero exit is an error that carries stderr (never shown as is, see
/// [`explain`]).
fn capture(mut cmd: Command, timeout: Duration) -> Result<Vec<u8>, RunError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(RunError::Spawn)?;
    let stdout = child.stdout.take().map(read_bounded);
    let stderr = child.stderr.take().map(read_bounded);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(RunError::Spawn(e)),
        }
        let sig = SIGNAL.load(Ordering::SeqCst);
        if sig != 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Interrupted(sig));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RunError::Timeout);
        }
        thread::sleep(Duration::from_millis(20));
    };
    let out = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    let err = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    if status.success() {
        Ok(out)
    } else {
        Err(RunError::Failed {
            code: status.code(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
        })
    }
}

/// Turn a failure of WHAT into an error that is safe to print.
fn explain(what: &str, err: RunError) -> anyhow::Error {
    match err {
        RunError::Spawn(e) => anyhow!("cannot run {what}: {e}"),
        RunError::Timeout => anyhow!("{what} timed out"),
        RunError::Interrupted(sig) => Interrupted(sig).into(),
        RunError::Failed { code, stderr } => {
            let code = code.map_or_else(|| "killed".to_string(), |c| format!("exit {c}"));
            anyhow!("{what} failed ({code}): {}", describe_failure(&stderr))
        }
    }
}

/// A short word safe to print: what the service uses for error codes and
/// lifecycle states.
fn is_safe_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// What the stderr of a failed `oci` is reduced to. The raw text is never
/// shown, not even redacted: it can carry anything (request signatures,
/// tokens, key material, connection strings), and no filter catches all of
/// it. Only the service error's code and HTTP status are kept, and only when
/// they look like a code and a number.
fn describe_failure(stderr: &str) -> String {
    const NO_DETAILS: &str = "no details (run the oci command by hand to see the error)";
    let Some(start) = stderr.find('{') else {
        return NO_DETAILS.to_string();
    };
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&stderr[start..]) else {
        return NO_DETAILS.to_string();
    };
    let code = obj
        .get("code")
        .and_then(Value::as_str)
        .filter(|c| is_safe_token(c));
    let status = obj.get("status").and_then(|s| match s {
        Value::Number(n) => n.as_u64(),
        Value::String(t) => t.parse().ok(),
        _ => None,
    });
    match (code, status) {
        (Some(c), Some(s)) => format!("{c} (HTTP {s})"),
        (Some(c), None) => c.to_string(),
        (None, Some(s)) => format!("HTTP {s}"),
        (None, None) => NO_DETAILS.to_string(),
    }
}

/// Make TEXT (typed by the user) safe to print on a terminal: no control
/// characters (escape sequences), bounded. This is not a redactor: it is never
/// applied to output of the service or of `oci`.
fn sanitize(text: &str) -> String {
    let joined: String = text
        .split_whitespace()
        .map(|w| {
            w.chars()
                .map(|c| if c.is_control() { '?' } else { c })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ");
    if joined.chars().count() > 300 {
        let mut short: String = joined.chars().take(300).collect();
        short.push_str("...");
        short
    } else {
        joined
    }
}

// ------------------------------------------------------------- oci client

struct OciCli {
    bin: PathBuf,
    global: Vec<OsString>,
}

impl OciCli {
    fn new(opts: &Options) -> Self {
        let mut global: Vec<OsString> = Vec::new();
        let mut push = |flag: &str, value: &OsStr| {
            let mut arg = OsString::from(format!("--{flag}="));
            arg.push(value);
            global.push(arg);
        };
        let dir = opts.config_dir.clone().or_else(default_config_dir);
        let fallback = |name: &str, env: &str| {
            dir.as_ref()
                .map(|d| d.join(name))
                .filter(|p| std::env::var_os(env).is_none() && p.is_file())
        };
        if let Some(p) = opts
            .config_file
            .clone()
            .or_else(|| fallback("config", "OCI_CLI_CONFIG_FILE"))
        {
            push("config-file", p.as_os_str());
        }
        if let Some(p) = opts
            .cli_rc_file
            .clone()
            .or_else(|| fallback("cli_rc", "OCI_CLI_RC_FILE"))
        {
            push("cli-rc-file", p.as_os_str());
        }
        if let Some(p) = &opts.profile {
            push("profile", OsStr::new(p));
        }
        Self {
            bin: opts.oci_bin.clone().unwrap_or_else(|| PathBuf::from("oci")),
            global,
        }
    }

    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(&self.bin);
        cmd.args(&self.global).args(args);
        cmd
    }

    /// The `oci` command with its global options: what the helper runs.
    fn prefix(&self) -> Vec<OsString> {
        let mut v = vec![self.bin.clone().into_os_string()];
        v.extend(self.global.iter().cloned());
        v
    }

    /// Run a read-only `oci` command and parse its JSON output. Empty output
    /// (what the CLI prints for an empty list) is `null`.
    fn json(&self, what: &str, cmd: Command, timeout: Duration) -> Result<Value> {
        let out = capture(cmd, timeout).map_err(|e| explain(what, e))?;
        if out.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&out).map_err(|_| anyhow!("{what}: the output is not JSON"))
    }

    fn resolve_instance(&self, opts: &Options) -> Result<Target> {
        let name = opts.instance.trim();
        if name.is_empty() {
            bail!("no instance given (name or OCID)");
        }
        if name.starts_with("ocid1.") {
            if !name.starts_with("ocid1.instance.") || !is_ocid_text(name) {
                bail!("not an instance OCID: {}", sanitize(name));
            }
            let compartment = match &opts.compartment_id {
                Some(c) => Some(c.clone()),
                None => self.instance_compartment(name)?,
            };
            return Ok(Target {
                id: name.to_string(),
                compartment,
            });
        }
        if name.starts_with('-') || name.chars().any(char::is_control) {
            bail!("invalid instance name: {}", sanitize(name));
        }
        let mut args = vec![
            "compute".to_string(),
            "instance".to_string(),
            "list".to_string(),
            format!("--display-name={name}"),
            "--all".to_string(),
        ];
        if let Some(c) = &opts.compartment_id {
            args.push(format!("--compartment-id={c}"));
        }
        let listed = self.json("listing instances", self.command(&args), LIST_TIMEOUT)?;
        let mut found = Vec::new();
        for item in listed
            .get("data")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let text = |k: &str| item.get(k).and_then(Value::as_str);
            let live = !matches!(text("lifecycle-state"), Some("TERMINATED" | "TERMINATING"));
            if live
                && text("display-name") == Some(name)
                && let Some(id) = text("id")
                && id.starts_with("ocid1.instance.")
                && is_ocid_text(id)
            {
                let compartment = opts.compartment_id.clone().or_else(|| {
                    text("compartment-id")
                        .filter(|c| c.starts_with("ocid1.") && is_ocid_text(c))
                        .map(str::to_string)
                });
                found.push(Target {
                    id: id.to_string(),
                    compartment,
                });
            }
        }
        match found.len() {
            0 => bail!("instance not found: {}", sanitize(name)),
            1 => Ok(found.remove(0)),
            n => bail!("{n} instances are named {}: use the OCID", sanitize(name)),
        }
    }

    /// The compartment of instance ID, which the helper needs to find a
    /// connection again by its tag. `None` when it cannot be told (the CLI's
    /// own default compartment applies then).
    fn instance_compartment(&self, id: &str) -> Result<Option<String>> {
        let id_arg = format!("--instance-id={id}");
        let cmd = self.command([
            "compute",
            "instance",
            "get",
            &id_arg,
            "--query",
            "data.\"compartment-id\"",
            "--raw-output",
        ]);
        match capture(cmd, LIST_TIMEOUT) {
            Ok(out) => Ok(Some(String::from_utf8_lossy(&out).trim().to_string())
                .filter(|c| c.starts_with("ocid1.") && is_ocid_text(c))),
            Err(RunError::Interrupted(sig)) => Err(Interrupted(sig).into()),
            Err(_) => Ok(None),
        }
    }

    /// Wait until connection ID is ACTIVE; return its connection string.
    fn wait_active(&self, id: &str) -> Result<String> {
        self.wait_active_field(id, "connection-string", &check_signal)
    }

    /// Wait until connection ID is ACTIVE; return its string FIELD
    /// (`connection-string`: serial, `vnc-connection-string`: graphical).
    /// INTERRUPT is checked between the steps and ends the wait with its error.
    fn wait_active_field(
        &self,
        id: &str,
        field: &str,
        interrupt: &dyn Fn() -> Result<()>,
    ) -> Result<String> {
        let deadline = Instant::now() + ACTIVE_TIMEOUT;
        loop {
            interrupt()?;
            let cmd = self.command([
                "compute".to_string(),
                "instance-console-connection".to_string(),
                "get".to_string(),
                format!("--instance-console-connection-id={id}"),
            ]);
            let got = self.json("reading the console connection", cmd, GET_TIMEOUT)?;
            let data = got.get("data");
            let state = data
                .and_then(|d| d.get("lifecycle-state"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            match state {
                "ACTIVE" => {
                    return data
                        .and_then(|d| d.get(field))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| anyhow!("the console connection has no {field}"));
                }
                "FAILED" | "DELETING" | "DELETED" => {
                    bail!("the console connection is {state} (not usable)")
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                bail!("the console connection did not become active in time");
            }
            sleep_with(POLL_INTERVAL, interrupt)?;
        }
    }
}

/// The instance a console connection is made for.
struct Target {
    id: String,
    compartment: Option<String>,
}

/// `$XDG_CONFIG_HOME/oci`, else `~/.config/oci`.
fn default_config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().ok().map(|h| h.join(".config")))?;
    Some(base.join("oci"))
}

/// Characters an OCID may contain.
fn is_ocid_text(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

// ------------------------------------------------------ connection owner

/// The helper that owns one console connection, run with `sh -c` in a session
/// of its own (so that signals to our process group, SIGKILL included, do not
/// reach it) with the `oci` command and its global options as arguments.
///
/// It creates the connection, tagged with a nonce, and prints `id OCID`. Then
/// it waits for a line or EOF on stdin: that is the end of the session, or of
/// its owner (a closed pipe is also what a killed parent leaves behind). It
/// then deletes the connection (unless LSU_KEEP is 1), printing `deleted`,
/// `delete-failed N` or `kept`.
///
/// When the create request fails or its answer cannot be read, the request may
/// have gone through nevertheless (it is not known whether it is still in
/// flight, whether it timed out on our side, or whether the answer got lost).
/// The helper then looks for a connection of the instance that carries the
/// nonce, and takes that one. It never lists anything else, and deletes
/// nothing but the one connection it created or found by its nonce.
///
/// The service's answers are only ever compared with a strict OCID pattern,
/// never evaluated.
const SUPERVISOR: &str = r##"
trap '' PIPE HUP INT
tc= to=
if command -v timeout >/dev/null 2>&1; then
  tc='timeout -k 5 90'
  to='timeout -k 5 60'
fi
tag="{\"luks-ssh-unlock\":\"$LSU_NONCE\"}"

valid_id() {
  case $1 in
    ocid1.instanceconsoleconnection.*) ;;
    *) return 1 ;;
  esac
  case $1 in
    *[!A-Za-z0-9._-]*) return 1 ;;
  esac
  return 0
}

find_ours() {
  if [ -n "$LSU_COMPARTMENT" ]; then
    set -- "$@" "--compartment-id=$LSU_COMPARTMENT"
  fi
  found=$($to "$@" compute instance-console-connection list \
    "--instance-id=$LSU_INSTANCE" --all \
    --query "data[?\"freeform-tags\".\"luks-ssh-unlock\"=='$LSU_NONCE'].id | [0]" \
    --raw-output </dev/null 2>/dev/null)
  if valid_id "$found"; then
    printf '%s\n' "$found"
  fi
}

out=$($tc "$@" compute instance-console-connection create \
  "--instance-id=$LSU_INSTANCE" "--ssh-public-key-file=$LSU_PUBKEY" \
  "--freeform-tags=$tag" --query data.id --raw-output </dev/null)
rc=$?
id=
if [ "$rc" -eq 0 ] && valid_id "$out"; then
  id=$out
else
  id=$(find_ours "$@")
fi
if [ -z "$id" ]; then
  printf 'failed %s\n' "$rc" 2>/dev/null
  exit 1
fi
printf 'id %s\n' "$id" 2>/dev/null

read -r _line
if [ "$LSU_KEEP" = 1 ]; then
  printf 'kept\n' 2>/dev/null
  exit 0
fi
n=0
while :; do
  $to "$@" compute instance-console-connection delete \
    "--instance-console-connection-id=$id" --force </dev/null >/dev/null 2>&1
  rc=$?
  if [ "$rc" -eq 0 ]; then
    break
  fi
  n=$((n + 1))
  if [ "$n" -ge 3 ]; then
    break
  fi
  sleep 2
done
if [ "$rc" -eq 0 ]; then
  printf 'deleted\n' 2>/dev/null
else
  printf 'delete-failed %s\n' "$rc" 2>/dev/null
fi
exit 0
"##;

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Id(String),
    Failed(i32),
    Kept,
    Deleted,
    DeleteFailed(i32),
}

fn is_connection_ocid(s: &str) -> bool {
    s.starts_with("ocid1.instanceconsoleconnection.") && is_ocid_text(s)
}

fn parse_event(line: &str) -> Option<Event> {
    let line = line.trim_end();
    let num = |s: &str| s.trim().parse::<i32>().unwrap_or(-1);
    match line.split_once(' ') {
        Some(("id", id)) if is_connection_ocid(id) => Some(Event::Id(id.to_string())),
        Some(("failed", n)) => Some(Event::Failed(num(n))),
        Some(("delete-failed", n)) => Some(Event::DeleteFailed(num(n))),
        None if line == "kept" => Some(Event::Kept),
        None if line == "deleted" => Some(Event::Deleted),
        _ => None,
    }
}

/// A running helper: the owner of the console connection of this session.
/// Closing it (explicitly with [`Supervisor::finish`], or by dropping it, or by
/// our death) makes the helper delete the connection.
struct Supervisor {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Event>,
    stderr: Option<thread::JoinHandle<Vec<u8>>>,
    id: Option<String>,
    keep: bool,
    /// The helper gave up (the request failed): nothing to clean up
    ended: bool,
    /// Print no notices (a full-screen UI owns the terminal)
    quiet: bool,
}

impl Supervisor {
    fn start(cli: &OciCli, target: &Target, pubkey: &Path, keep: bool) -> Result<Self> {
        let nonce = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(SUPERVISOR)
            .arg("luks-ssh-unlock-oci")
            .args(cli.prefix())
            .env("LSU_INSTANCE", &target.id)
            .env("LSU_PUBKEY", pubkey)
            .env("LSU_NONCE", &nonce)
            .env(
                "LSU_COMPARTMENT",
                target.compartment.as_deref().unwrap_or(""),
            )
            .env("LSU_KEEP", if keep { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .context("cannot run sh for the console helper")?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().context("no stdout for the helper")?;
        let stderr = child.stderr.take().map(read_bounded);
        let (tx, events) = mpsc::channel();
        thread::spawn(move || {
            let lines = io::BufReader::new(stdout.take(64 * 1024)).lines();
            for line in lines.map_while(Result::ok) {
                if let Some(event) = parse_event(&line)
                    && tx.send(event).is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            events,
            stderr,
            id: None,
            keep,
            ended: false,
            quiet: false,
        })
    }

    /// Wait for the new connection; return its OCID.
    fn wait_created(&mut self) -> Result<String> {
        self.wait_created_with(&check_signal)
    }

    /// Like [`Supervisor::wait_created`], with INTERRUPT checked in the loop:
    /// its error ends the wait (the connection is still cleaned up by
    /// [`Supervisor::finish`] or by dropping the supervisor).
    fn wait_created_with(&mut self, interrupt: &dyn Fn() -> Result<()>) -> Result<String> {
        let deadline = Instant::now() + CREATE_WAIT;
        loop {
            interrupt()?;
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Id(id)) => {
                    self.id = Some(id.clone());
                    return Ok(id);
                }
                Ok(Event::Failed(rc)) => {
                    self.ended = true;
                    if rc == 0 {
                        bail!("creating the console connection: unexpected response from oci");
                    }
                    bail!(
                        "creating the console connection failed (exit {rc}): {}",
                        describe_failure(&self.failure_text())
                    );
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        bail!(
                            "creating the console connection timed out; if the request goes \
                             through after all, the connection is deleted in the background"
                        );
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.ended = true;
                    bail!(
                        "the console helper ended unexpectedly: {}",
                        describe_failure(&self.failure_text())
                    );
                }
            }
        }
    }

    /// Wait up to WAIT for the helper to end; true if it has.
    fn reap(&mut self, wait: Duration) -> bool {
        let end = Instant::now() + wait;
        loop {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return true;
            }
            if Instant::now() >= end {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// What the helper's `oci` calls wrote to stderr.
    fn failure_text(&mut self) -> String {
        if !self.reap(Duration::from_secs(5)) {
            return String::new();
        }
        self.stderr
            .take()
            .and_then(|h| h.join().ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    }

    fn say(&self, msg: &str) {
        if !self.quiet {
            info(msg);
        }
    }

    /// End of the session: have the connection deleted (kept, with `keep`),
    /// and wait for that, not for long after a signal. The helper does the
    /// job in any case, also when we are gone before it is done.
    fn finish(&mut self, interrupted: bool) {
        drop(self.stdin.take());
        if self.ended {
            self.reap(Duration::from_secs(2));
            return;
        }
        match (&self.id, self.keep) {
            (Some(_), false) => self.say("cleaning up the console connection"),
            (None, false) => self.say(
                "the console connection, if it is created after all, is deleted in \
                 the background",
            ),
            _ => {}
        }
        let wait = if interrupted {
            CLEANUP_WAIT
        } else {
            DELETE_WAIT
        };
        let mut deadline = Instant::now() + wait;
        let mut shortened = interrupted;
        loop {
            if !shortened && SIGNAL.load(Ordering::SeqCst) != 0 {
                deadline = deadline.min(Instant::now() + CLEANUP_WAIT);
                shortened = true;
            }
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(Event::Id(id)) => self.id = Some(id),
                Ok(Event::Failed(_) | Event::Deleted) => break,
                Ok(Event::Kept) => {
                    self.say(&format!(
                        "keeping console connection {}",
                        self.id.as_deref().unwrap_or("?")
                    ));
                    break;
                }
                Ok(Event::DeleteFailed(rc)) => {
                    self.say(&format!(
                        "could not delete console connection {} (oci exit {rc}): delete it by hand",
                        self.id.as_deref().unwrap_or("?")
                    ));
                    break;
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        self.say("the console connection is deleted in the background");
                        return;
                    }
                }
            }
        }
        self.reap(Duration::from_secs(2));
    }
}

// ------------------------------------------------- connection string parse

/// What a serial console connection string contains, validated.
#[derive(Debug, PartialEq, Eq)]
struct Console {
    /// The instance OCID `ssh` connects to (through the proxy)
    target: String,
    /// The connection OCID: the user name at the console service
    proxy_user: String,
    proxy_host: String,
    proxy_port: Option<u16>,
}

fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-_./:@%=,+".contains(c)
}

/// Split S into words like a POSIX shell would, but accept only the plainest
/// syntax: bare words of safe characters, '...' and "..." without any
/// expansion. Everything a shell could act on (`$`, backticks, `\`, `;`, `&`,
/// `|`, redirections, globs, newlines, ...) is an error.
fn split_words(s: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' | '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        None => return Err("unterminated quote"),
                        Some(q) if q == c => break,
                        Some(q) if q.is_control() => return Err("control character"),
                        Some('$' | '`' | '\\') if c == '"' => {
                            return Err("shell expansion in a quoted string");
                        }
                        Some(q) => cur.push(q),
                    }
                }
            }
            c if is_word_char(c) => {
                in_word = true;
                cur.push(c);
            }
            _ => return Err("unexpected character"),
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

fn is_host_text(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Parse and strictly validate the ProxyCommand of a console connection string
/// (`ssh -W %h:%p [-p PORT] CONN@instance-console.HOST`, the same for the
/// serial and the graphical console): returns the user (EXPECTED_CONN), the
/// host and the optional port.
fn parse_proxy_command(proxy: &str, expected_conn: &str) -> Result<(String, String, Option<u16>)> {
    let bad = bad_string;
    let inner = split_words(proxy).map_err(bad)?;
    let mut it = inner.iter();
    if it.next().map(String::as_str) != Some("ssh") {
        return Err(bad("the ProxyCommand is not ssh"));
    }
    let (mut forward, mut port, mut dest) = (false, None, None);
    while let Some(word) = it.next() {
        match word.as_str() {
            "-W" => {
                if it.next().map(String::as_str) != Some("%h:%p") || forward {
                    return Err(bad("an unexpected -W in the ProxyCommand"));
                }
                forward = true;
            }
            "-p" => {
                let p = it
                    .next()
                    .and_then(|p| p.parse::<u16>().ok())
                    .filter(|p| *p != 0)
                    .ok_or_else(|| bad("an invalid port in the ProxyCommand"))?;
                if port.replace(p).is_some() {
                    return Err(bad("more than one port in the ProxyCommand"));
                }
            }
            w if w.starts_with('-') => {
                return Err(bad("an unsupported option in the ProxyCommand"));
            }
            w => {
                if dest.replace(w).is_some() {
                    return Err(bad("more than one destination in the ProxyCommand"));
                }
            }
        }
    }
    if !forward {
        return Err(bad("the ProxyCommand does not forward (-W)"));
    }
    let dest = dest.ok_or_else(|| bad("no destination in the ProxyCommand"))?;
    let (user, host) = dest
        .split_once('@')
        .ok_or_else(|| bad("no user in the ProxyCommand"))?;
    if user != expected_conn {
        return Err(bad("it belongs to another console connection"));
    }
    if !host.starts_with("instance-console.") || !is_host_text(host) {
        return Err(bad(
            "the console service host is not an instance-console host",
        ));
    }
    Ok((user.to_string(), host.to_string(), port))
}

fn bad_string(what: &str) -> anyhow::Error {
    anyhow!("unexpected console connection string: {what}")
}

/// Parse and strictly validate the serial console connection string.
///
/// Accepted shape (what the service returns):
/// `ssh -o ProxyCommand='ssh -W %h:%p -p 443 CONN@instance-console.REGION.HOST' INSTANCE`
/// where CONN must be EXPECTED_CONN, the connection this run created, and
/// INSTANCE must be EXPECTED_INSTANCE, the instance it was created for.
fn parse_connection_string(
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
    while let Some(word) = it.next() {
        match word.as_str() {
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
    let proxy = proxy.ok_or_else(|| bad("no ProxyCommand (not a serial console)"))?;
    let target = target.ok_or_else(|| bad("no destination"))?;
    if !target.starts_with("ocid1.instance.") || !is_ocid_text(target) {
        return Err(bad("the destination is not an instance OCID"));
    }
    if target != expected_instance {
        return Err(bad("it is for another instance"));
    }

    let (proxy_user, proxy_host, proxy_port) = parse_proxy_command(proxy, expected_conn)?;
    Ok(Console {
        target: target.to_string(),
        proxy_user,
        proxy_host,
        proxy_port,
    })
}

/// The `ssh` arguments for CONSOLE: the identity is forced, and other keys of
/// the agent are ignored, for the connection and for its ProxyCommand alike.
fn ssh_args(console: &Console, ssh_bin: &str, key: &Path) -> Vec<OsString> {
    let key_q = shell_quote(&key.to_string_lossy());
    let port = console
        .proxy_port
        .map_or_else(String::new, |p| format!(" -p {p}"));
    let proxy = format!(
        "{} -i {key_q} -o IdentitiesOnly=yes -W %h:%p{port} {}@{}",
        shell_quote(ssh_bin),
        console.proxy_user,
        console.proxy_host
    );
    vec![
        "-i".into(),
        key.into(),
        "-o".into(),
        "IdentitiesOnly=yes".into(),
        "-o".into(),
        format!("ProxyCommand={proxy}").into(),
        console.target.clone().into(),
    ]
}

// ------------------------------------------------------------------ keys

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set: give the identity with an absolute path"))
}

/// The private key and its `.pub`, checked: both readable, RSA (the only type
/// console connections accept).
fn resolve_identity(identity: Option<&Path>) -> Result<(PathBuf, PathBuf)> {
    let key = match identity {
        Some(p) => match p.strip_prefix("~") {
            Ok(rest) => home_dir()?.join(rest),
            Err(_) => p.to_path_buf(),
        },
        None => home_dir()?.join(".ssh/id_rsa"),
    };
    let key = std::path::absolute(&key).unwrap_or(key);
    let mut pubkey = key.clone().into_os_string();
    pubkey.push(".pub");
    let pubkey = PathBuf::from(pubkey);
    let not_found = || {
        anyhow!(
            "SSH key {} (and its .pub) not found or unreadable; console connections \
             need an RSA key (use the identity option)",
            key.display()
        )
    };
    fs::File::open(&key).map_err(|_| not_found())?;
    let mut head = Vec::new();
    fs::File::open(&pubkey)
        .map_err(|_| not_found())?
        .take(16 * 1024)
        .read_to_end(&mut head)
        .map_err(|_| not_found())?;
    let head = String::from_utf8_lossy(&head);
    if head.split_whitespace().next() != Some("ssh-rsa") {
        bail!(
            "{} is not an RSA public key: console connections only accept RSA",
            pubkey.display()
        );
    }
    Ok((key, pubkey))
}

// --------------------------------------------------------------- session

fn session(opts: &Options) -> Result<i32> {
    let (key, pubkey) = resolve_identity(opts.identity.as_deref())?;
    let cli = OciCli::new(opts);

    let target = cli.resolve_instance(opts)?;
    check_signal()?;
    info(&format!(
        "connecting to {} ({}) using {}",
        sanitize(opts.instance.trim()),
        target.id,
        key.display()
    ));

    info("requesting a new console connection");
    // From here on, whatever happens (also to us), the helper deletes the
    // connection that gets created.
    let mut owner = Supervisor::start(&cli, &target, &pubkey, opts.keep)?;
    let result = serve(&cli, &mut owner, &target, &key, opts);
    let interrupted = matches!(&result, Err(e) if e.downcast_ref::<Interrupted>().is_some());
    owner.finish(interrupted);
    result
}

/// The session proper, with the connection OWNER has created.
fn serve(
    cli: &OciCli,
    owner: &mut Supervisor,
    target: &Target,
    key: &Path,
    opts: &Options,
) -> Result<i32> {
    let id = owner.wait_created().context("no console connection")?;
    check_signal()?;

    let connstr = cli.wait_active(&id)?;
    check_signal()?;
    let console = parse_connection_string(&connstr, &id, &target.id)?;
    drop(connstr);

    let ssh_bin = opts.ssh_bin.clone().unwrap_or_else(|| PathBuf::from("ssh"));
    let args = ssh_args(&console, &ssh_bin.to_string_lossy(), key);
    info("serial console: press Enter for a prompt, disconnect with ~.");
    run_ssh(&ssh_bin, &args)
}

/// Run ssh on our terminal until it ends. A signal for us is passed on to it
/// (SIGTERM, then SIGKILL after a grace period).
fn run_ssh(bin: &Path, args: &[OsString]) -> Result<i32> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    // SAFETY: prctl is async-signal-safe. ssh must not outlive us.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run {}", bin.display()))?;
    let mut terminated: Option<Instant> = None;
    let mut killed = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for ssh")? {
            break status;
        }
        if SIGNAL.load(Ordering::SeqCst) != 0 {
            match terminated {
                None => {
                    // SAFETY: signalling our own child.
                    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
                    terminated = Some(Instant::now());
                }
                Some(at) if !killed && at.elapsed() >= SSH_GRACE => {
                    let _ = child.kill();
                    killed = true;
                }
                Some(_) => {}
            }
        }
        thread::sleep(Duration::from_millis(50));
    };
    let sig = SIGNAL.load(Ordering::SeqCst);
    if sig != 0 {
        return Err(Interrupted(sig).into());
    }
    Ok(status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    pub(super) const CONN: &str = "ocid1.instanceconsoleconnection.oc1.eu-frankfurt-1.own";
    pub(super) const FOREIGN: &str = "ocid1.instanceconsoleconnection.oc1.eu-frankfurt-1.foreign";
    pub(super) const INST: &str = "ocid1.instance.oc1.eu-frankfurt-1.inst";
    pub(super) const OTHER_INST: &str = "ocid1.instance.oc1.eu-frankfurt-1.other";
    pub(super) const COMP: &str = "ocid1.compartment.oc1..comp";
    pub(super) const HOST: &str = "instance-console.eu-frankfurt-1.oci.oraclecloud.com";

    fn good() -> String {
        format!("ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}' {INST}")
    }

    #[test]
    fn parses_the_documented_shape() {
        let c = parse_connection_string(&good(), CONN, INST).unwrap();
        assert_eq!(
            c,
            Console {
                target: INST.into(),
                proxy_user: CONN.into(),
                proxy_host: HOST.into(),
                proxy_port: Some(443),
            }
        );
        // Quoted as one word, and with a trailing newline
        let alt = format!("ssh -o \"ProxyCommand=ssh -W %h:%p -p 443 {CONN}@{HOST}\" {INST}\n");
        assert_eq!(parse_connection_string(&alt, CONN, INST).unwrap(), c);
    }

    #[test]
    fn rejects_anything_else() {
        let proxy = |inner: &str| format!("ssh -o ProxyCommand='{inner}' {INST}");
        let ok_inner = format!("ssh -W %h:%p -p 443 {CONN}@{HOST}");
        let cases = [
            // shell syntax
            format!("ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}'; id {INST}"),
            format!("ssh -o ProxyCommand='{ok_inner}; touch /tmp/x' {INST}"),
            format!("ssh -o ProxyCommand='{ok_inner}' $(id)"),
            format!("ssh -o ProxyCommand='{ok_inner}' `id`"),
            format!("ssh -o ProxyCommand=\"{ok_inner} $HOME\" {INST}"),
            format!("ssh -o ProxyCommand=\"{ok_inner} \\\" x\" {INST}"),
            format!("ssh -o ProxyCommand='{ok_inner}' {INST} | sh"),
            format!("ssh -o ProxyCommand='{ok_inner}' {INST} > /etc/x"),
            format!("ssh -o ProxyCommand='{ok_inner}'\n{INST}"),
            format!("ssh -o ProxyCommand='{ok_inner} {INST}"),
            // other options
            format!("ssh -o LocalCommand=id -o ProxyCommand='{ok_inner}' {INST}"),
            format!("ssh -o PermitLocalCommand=yes -o ProxyCommand='{ok_inner}' {INST}"),
            format!("ssh -F /tmp/evil -o ProxyCommand='{ok_inner}' {INST}"),
            format!("ssh -o ProxyCommand='{ok_inner}' -o ProxyCommand='{ok_inner}' {INST}"),
            // wrong destinations
            proxy(&format!("ssh -W %h:%p -p 443 {FOREIGN}@{HOST}")),
            proxy(&format!("ssh -W %h:%p -p 443 {CONN}@evil.example.com")),
            proxy(&format!("ssh -W %h:%p -p 443 {CONN}@instance-console.-x")),
            proxy(&format!("ssh -W %h:%p -p 0 {CONN}@{HOST}")),
            proxy(&format!("ssh -W %h:%p -p 99999 {CONN}@{HOST}")),
            proxy(&format!("ssh -W evil:22 -p 443 {CONN}@{HOST}")),
            proxy(&format!("ssh -p 443 {CONN}@{HOST}")),
            proxy(&format!("ssh -W %h:%p -o ProxyCommand=id {CONN}@{HOST}")),
            proxy(&format!("sh -W %h:%p -p 443 {CONN}@{HOST}")),
            format!("ssh -o ProxyCommand='{ok_inner}' {OTHER_INST}"),
            format!("ssh -o ProxyCommand='{ok_inner}' root@host"),
            format!("ssh -o ProxyCommand='{ok_inner}' -- {INST}"),
            format!("ssh -o ProxyCommand='{ok_inner}'"),
            format!("ssh {INST}"),
            // the graphical console is not supported
            format!("ssh -N -L localhost:5900:{INST}:5900 -o ProxyCommand='{ok_inner}' {INST}"),
            "sh -c id".to_string(),
            String::new(),
        ];
        for case in cases {
            let err = parse_connection_string(&case, CONN, INST)
                .unwrap_err()
                .to_string();
            assert!(
                !err.contains(CONN) && !err.contains(HOST),
                "error leaks the string: {err}"
            );
        }
    }

    #[test]
    fn rebuilt_command_is_quoted_and_forces_the_key() {
        let console = parse_connection_string(&good(), CONN, INST).unwrap();
        let args = ssh_args(&console, "/usr/bin/ssh", Path::new("/home/a b/id_rsa"));
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "-i");
        assert_eq!(args[1], "/home/a b/id_rsa");
        assert_eq!(args[2..4], ["-o", "IdentitiesOnly=yes"]);
        assert_eq!(
            args[5],
            format!(
                "ProxyCommand=/usr/bin/ssh -i '/home/a b/id_rsa' -o IdentitiesOnly=yes \
                 -W %h:%p -p 443 {CONN}@{HOST}"
            )
        );
        assert_eq!(args[6], INST);
        assert_eq!(args.len(), 7);
    }

    #[test]
    fn only_the_expected_instance_is_accepted() {
        let other = format!("ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}' {OTHER_INST}");
        let err = parse_connection_string(&other, CONN, INST)
            .unwrap_err()
            .to_string();
        assert!(err.contains("another instance"), "{err}");
        assert!(parse_connection_string(&other, CONN, OTHER_INST).is_ok());
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("a\x1b[31mred\x07 b"), "a?[31mred? b");
        assert!(sanitize(&"word ".repeat(200)).chars().count() <= 303);

        // Only a code and a status make it out of the stderr of oci.
        let err = r#"ServiceError:
{"code": "NotAuthorizedOrNotFound", "message": "Authorization failed", "status": 404}"#;
        assert_eq!(describe_failure(err), "NotAuthorizedOrNotFound (HTTP 404)");
        let no_details = describe_failure("");
        assert!(no_details.starts_with("no details"));
        for raw in [
            "ERROR: Authorization: Bearer abc",
            "boom\n\n",
            "-----BEGIN RSA PRIVATE KEY-----",
            r#"{"message": "token=abc signature=x"}"#,
            r#"{"code": "bad code with spaces Bearer abc"}"#,
            r#"{"code": "x\u001b[31m"}"#,
        ] {
            assert_eq!(describe_failure(raw), no_details, "{raw}");
        }
        assert_eq!(
            describe_failure(r#"{"code": "Throttled", "message": "Bearer abc"}"#),
            "Throttled"
        );
    }

    #[test]
    fn helper_events() {
        assert_eq!(
            parse_event(&format!("id {CONN}")),
            Some(Event::Id(CONN.into()))
        );
        assert_eq!(parse_event("id ocid1.instance.x"), None);
        assert_eq!(parse_event("id ocid1.instanceconsoleconnection.x;id"), None);
        assert_eq!(parse_event("failed 3"), Some(Event::Failed(3)));
        assert_eq!(parse_event("delete-failed 1"), Some(Event::DeleteFailed(1)));
        assert_eq!(parse_event("deleted"), Some(Event::Deleted));
        assert_eq!(parse_event("kept"), Some(Event::Kept));
        assert_eq!(parse_event("whatever"), None);
    }

    // ------------------------------------------------ fake oci and ssh

    pub(super) struct Fixture {
        pub(super) dir: tempfile::TempDir,
        pub(super) opts: Options,
    }

    impl Fixture {
        pub(super) fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        pub(super) fn write(&self, name: &str, content: &str) {
            fs::write(self.path(name), content).unwrap();
        }

        pub(super) fn log(&self) -> Vec<String> {
            fs::read_to_string(self.path("oci.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        pub(super) fn deletes(&self) -> Vec<String> {
            self.log()
                .into_iter()
                .filter(|l| l.contains("instance-console-connection delete"))
                .collect()
        }

        pub(super) fn lists(&self) -> Vec<String> {
            self.log()
                .into_iter()
                .filter(|l| l.contains("instance-console-connection list"))
                .collect()
        }

        pub(super) fn ssh_args(&self) -> Option<Vec<String>> {
            fs::read_to_string(self.path("ssh.args"))
                .ok()
                .map(|s| s.lines().map(str::to_string).collect())
        }
    }

    /// Wait up to 15 s for COND.
    pub(super) fn eventually(cond: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    fn write_exec(path: &Path, body: &str) {
        fs::write(path, body).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A sandbox with a fake `oci` and a fake `ssh` running SSH_BODY. The fake
    /// `oci` logs every call, and answers:
    /// * `compute instance list` from `instances.json`, `compute instance get`
    ///   with the compartment
    /// * `create` with the connection id, after `create_sleep` seconds, unless
    ///   `create_fail` (stderr, exit 1) or `create_out` (the stdout) exist
    /// * `get` from `conn.json`, `delete` with nothing
    /// * `list` only when it is filtered by the tag (else exit 97), with the
    ///   contents of `list_result`
    pub(super) fn fixture(connstr: &str, instances: Value, ssh_body: &str) -> Fixture {
        // Scripts are written while no other test spawns a process: a child
        // forked meanwhile would hold the file open ("text file busy").
        let _lock = RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_string_lossy().into_owned();
        let conn = serde_json::json!({"data": {
            "id": CONN, "lifecycle-state": "ACTIVE", "connection-string": connstr
        }});
        fs::write(dir.path().join("conn.json"), conn.to_string()).unwrap();
        fs::write(dir.path().join("instances.json"), instances.to_string()).unwrap();
        fs::write(dir.path().join("id_rsa"), "private").unwrap();
        fs::write(dir.path().join("id_rsa.pub"), "ssh-rsa AAAAB3 test@host\n").unwrap();
        write_exec(
            &dir.path().join("oci"),
            &format!(
                r#"#!/bin/sh
echo "$*" >> '{d}/oci.log'
case "$*" in
  *"instance-console-connection list"*)
    case "$*" in *"--query"*"luks-ssh-unlock"*) ;; *) exit 97 ;; esac
    if [ -f '{d}/list_result' ]; then cat '{d}/list_result'; fi
    exit 0 ;;
  *"instance-console-connection create"*)
    if [ -f '{d}/create_sleep' ]; then sleep "$(cat '{d}/create_sleep')"; fi
    touch '{d}/create_done'
    if [ -f '{d}/create_fail' ]; then cat '{d}/create_fail' >&2; exit 1; fi
    if [ -f '{d}/create_out' ]; then cat '{d}/create_out'; exit 0; fi
    echo '{CONN}' ;;
  *"instance-console-connection get"*) cat '{d}/conn.json' ;;
  *"instance-console-connection delete"*)
    if [ -f '{d}/create_done' ]; then touch '{d}/after_create'; fi ;;
  *"compute instance list"*) cat '{d}/instances.json' ;;
  *"compute instance get"*) echo '{COMP}' ;;
  *) echo unexpected >&2; exit 98 ;;
esac
"#
            ),
        );
        write_exec(
            &dir.path().join("ssh"),
            &format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/ssh.args'\n{ssh_body}\n"),
        );
        let opts = Options {
            instance: "web".into(),
            identity: Some(dir.path().join("id_rsa")),
            config_dir: Some(dir.path().join("no-oci-config")),
            oci_bin: Some(dir.path().join("oci")),
            ssh_bin: Some(dir.path().join("ssh")),
            ..Options::default()
        };
        Fixture { dir, opts }
    }

    pub(super) fn instances() -> Value {
        serde_json::json!({"data": [
            {"id": "ocid1.instance.oc1.eu-frankfurt-1.old", "display-name": "web",
             "lifecycle-state": "TERMINATED", "compartment-id": COMP},
            {"id": OTHER_INST, "display-name": "db",
             "lifecycle-state": "RUNNING", "compartment-id": COMP},
            {"id": INST, "display-name": "web", "lifecycle-state": "RUNNING",
             "compartment-id": COMP},
        ]})
    }

    /// The nonce of the tag, from the create call in LOG.
    fn nonce_of(log: &[String]) -> String {
        let create = log
            .iter()
            .find(|l| l.contains("instance-console-connection create"))
            .expect("no create call");
        let (_, rest) = create.split_once(r#"{"luks-ssh-unlock":""#).unwrap();
        rest.split_once('"').unwrap().0.to_string()
    }

    #[test]
    fn serial_console_roundtrip() {
        let mut fx = fixture(&good(), instances(), "exit 7");
        fx.opts.profile = Some("work".into());
        fx.opts.config_file = Some("/cfg/config".into());
        fx.opts.cli_rc_file = Some("/cfg/rc".into());
        fx.opts.compartment_id = Some("ocid1.compartment.oc1..c".into());
        assert_eq!(run(&fx.opts).unwrap(), 7);

        let log = fx.log();
        assert_eq!(log.len(), 4, "{log:#?}");
        let global = "--config-file=/cfg/config --cli-rc-file=/cfg/rc --profile=work ";
        assert!(log.iter().all(|l| l.starts_with(global)), "{log:#?}");
        assert!(log[0].contains("compute instance list --display-name=web --all"));
        assert!(log[0].contains("--compartment-id=ocid1.compartment.oc1..c"));
        assert!(log[1].contains("instance-console-connection create"));
        assert!(log[1].contains(&format!("--instance-id={INST}")));
        assert!(log[1].contains(&format!(
            "--ssh-public-key-file={}",
            fx.path("id_rsa.pub").display()
        )));
        assert!(log[1].contains("--freeform-tags={\"luks-ssh-unlock\":"));
        assert!(!log[1].contains("--wait-for-state"));
        assert!(log[2].contains(&format!(
            "instance-console-connection get --instance-console-connection-id={CONN}"
        )));
        // Only our own connection is deleted, and nothing is ever listed.
        assert_eq!(fx.deletes().len(), 1, "{log:#?}");
        assert!(
            fx.deletes()[0].contains(&format!("--instance-console-connection-id={CONN} --force"))
        );
        assert!(log.iter().all(|l| !l.contains(FOREIGN)));
        assert!(fx.lists().is_empty(), "{log:#?}");

        let key = fx.path("id_rsa");
        let ssh = fx.path("ssh");
        assert_eq!(
            fx.ssh_args().unwrap(),
            vec![
                "-i".to_string(),
                key.display().to_string(),
                "-o".into(),
                "IdentitiesOnly=yes".into(),
                "-o".into(),
                format!(
                    "ProxyCommand={} -i {} -o IdentitiesOnly=yes -W %h:%p -p 443 {CONN}@{HOST}",
                    shell_quote(&ssh.to_string_lossy()),
                    shell_quote(&key.to_string_lossy())
                ),
                INST.into(),
            ]
        );
    }

    #[test]
    fn default_config_files_come_from_the_config_dir() {
        let mut fx = fixture(&good(), instances(), "exit 0");
        let cfg = fx.path("cfg");
        fs::create_dir(&cfg).unwrap();
        fs::write(cfg.join("config"), "[DEFAULT]\n").unwrap();
        fs::write(cfg.join("cli_rc"), "[DEFAULT]\n").unwrap();
        fx.opts.config_dir = Some(cfg.clone());
        // Only the cli_rc is overridden explicitly.
        fx.opts.cli_rc_file = Some("/x/rc".into());
        assert_eq!(run(&fx.opts).unwrap(), 0);
        if std::env::var_os("OCI_CLI_CONFIG_FILE").is_none() {
            // The helper's calls get the same global options.
            for line in fx.log().iter() {
                assert!(
                    line.starts_with(&format!(
                        "--config-file={}/config --cli-rc-file=/x/rc ",
                        cfg.display()
                    )),
                    "{line}"
                );
            }
        }
    }

    #[test]
    fn keep_leaves_the_connection() {
        let mut fx = fixture(&good(), instances(), "exit 0");
        fx.opts.keep = true;
        assert_eq!(run(&fx.opts).unwrap(), 0);
        assert!(fx.deletes().is_empty());
    }

    #[test]
    fn an_ocid_looks_up_the_compartment_not_the_instance() {
        let mut fx = fixture(&good(), instances(), "exit 0");
        fx.opts.instance = INST.into();
        assert_eq!(run(&fx.opts).unwrap(), 0);
        let log = fx.log();
        assert!(log.iter().all(|l| !l.contains("compute instance list")));
        assert!(log.iter().any(|l| l.contains("compute instance get")));
        assert_eq!(fx.deletes().len(), 1);

        // Given on the command line, it is not looked up at all.
        fx.opts.compartment_id = Some(COMP.into());
        fs::remove_file(fx.path("oci.log")).unwrap();
        assert_eq!(run(&fx.opts).unwrap(), 0);
        assert!(
            fx.log().iter().all(
                |l| !l.contains("compute instance get") && !l.contains("compute instance list")
            )
        );
    }

    #[test]
    fn a_hostile_connection_string_is_refused_but_cleaned_up() {
        let evil = format!(
            "ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}; touch {{d}}/pwned' {INST}"
        );
        let fx = fixture(&evil, instances(), "exit 0");
        let evil = evil.replace("{d}", &fx.dir.path().to_string_lossy());
        fs::write(
            fx.path("conn.json"),
            serde_json::json!({"data": {"id": CONN, "lifecycle-state": "ACTIVE",
                "connection-string": evil}})
            .to_string(),
        )
        .unwrap();
        let err = run(&fx.opts).unwrap_err().to_string();
        assert!(
            err.contains("unexpected console connection string"),
            "{err}"
        );
        assert!(!err.contains("pwned"));
        assert!(fx.ssh_args().is_none(), "ssh must not run");
        assert!(!fx.path("pwned").exists());
        assert_eq!(fx.deletes().len(), 1, "our connection is still deleted");
    }

    #[test]
    fn another_connections_or_instances_string_is_refused() {
        for other in [
            format!("ssh -o ProxyCommand='ssh -W %h:%p -p 443 {FOREIGN}@{HOST}' {INST}"),
            format!("ssh -o ProxyCommand='ssh -W %h:%p -p 443 {CONN}@{HOST}' {OTHER_INST}"),
        ] {
            let fx = fixture(&other, instances(), "exit 0");
            let err = run(&fx.opts).unwrap_err().to_string();
            assert!(
                err.contains("unexpected console connection string"),
                "{err}"
            );
            assert!(fx.ssh_args().is_none());
            let deletes = fx.deletes();
            assert_eq!(deletes.len(), 1);
            assert!(deletes[0].contains(CONN) && !deletes[0].contains(FOREIGN));
        }
    }

    #[test]
    fn create_failure_shows_the_code_only_and_deletes_nothing() {
        let fx = fixture(&good(), instances(), "exit 0");
        let token = "T".repeat(80);
        fx.write(
            "create_fail",
            &format!(
                "ServiceError:\n{{\"code\": \"NotAuthenticated\", \"status\": 401, \
                 \"message\": \"Bearer {token}\"}}\n"
            ),
        );
        let err = format!("{:#}", run(&fx.opts).unwrap_err());
        assert!(err.contains("NotAuthenticated (HTTP 401)"), "{err}");
        assert!(!err.contains(&token) && !err.contains("Bearer"), "{err}");
        assert!(fx.deletes().is_empty());
        assert!(fx.ssh_args().is_none());
        // The helper looked for a connection that the request may have made:
        // only ours (by its tag), only in this instance.
        let nonce = nonce_of(&fx.log());
        let lists = fx.lists();
        assert_eq!(lists.len(), 1, "{lists:#?}");
        assert!(lists[0].contains(&format!("--instance-id={INST}")));
        assert!(lists[0].contains(&format!("--compartment-id={COMP}")));
        assert!(lists[0].contains(&format!("=='{nonce}'")), "{}", lists[0]);
    }

    #[test]
    fn a_lost_create_answer_does_not_leak_the_connection() {
        // The request may have gone through although oci failed, or printed
        // something unreadable: the connection is found again by its tag.
        for (fail, out) in [
            (Some("ServiceError:\n{\"code\": \"Timeout\"}\n"), None),
            (None, Some("{garbage")),
            (
                None,
                Some("ocid1.instanceconsoleconnection.x$(touch PWNED)"),
            ),
        ] {
            let fx = fixture(&good(), instances(), "exit 0");
            fx.write("list_result", &format!("{CONN}\n"));
            if let Some(f) = fail {
                fx.write("create_fail", f);
            }
            if let Some(o) = out {
                fx.write("create_out", o);
            }
            assert_eq!(run(&fx.opts).unwrap(), 0, "{:#?}", fx.log());
            let deletes = fx.deletes();
            assert_eq!(deletes.len(), 1, "{:#?}", fx.log());
            assert!(deletes[0].contains(CONN));
            assert_eq!(fx.lists().len(), 1);
            assert!(!fx.path("PWNED").exists());
        }
    }

    #[test]
    fn an_unusable_create_answer_is_not_acted_upon() {
        let fx = fixture(&good(), instances(), "exit 0");
        let bad = "ocid1.instanceconsoleconnection.x$(touch PWNED)";
        fx.write("create_out", bad);
        let err = format!("{:#}", run(&fx.opts).unwrap_err());
        assert!(err.contains("unexpected response"), "{err}");
        assert!(!err.contains("PWNED"));
        assert!(fx.deletes().is_empty(), "nothing to delete");
        assert!(!fx.path("PWNED").exists());
        assert!(fx.ssh_args().is_none());
    }

    #[test]
    fn instance_lookup_errors() {
        let fx = fixture(&good(), serde_json::json!({"data": []}), "exit 0");
        let err = run(&fx.opts).unwrap_err().to_string();
        assert!(err.contains("instance not found"), "{err}");

        let two = serde_json::json!({"data": [
            {"id": INST, "display-name": "web", "lifecycle-state": "RUNNING"},
            {"id": "ocid1.instance.oc1.eu-frankfurt-1.two", "display-name": "web",
             "lifecycle-state": "STOPPED"},
        ]});
        let fx = fixture(&good(), two, "exit 0");
        let err = run(&fx.opts).unwrap_err().to_string();
        assert!(err.contains("2 instances"), "{err}");
        assert!(fx.log().iter().all(|l| !l.contains("create")));

        let mut fx = fixture(&good(), instances(), "exit 0");
        fx.opts.instance = "ocid1.volume.oc1..x".into();
        assert!(run(&fx.opts).is_err());
        fx.opts.instance = "-oProxyCommand=x".into();
        assert!(run(&fx.opts).is_err());
        assert!(fx.log().is_empty());
    }

    #[test]
    fn identity_checks() {
        let mut fx = fixture(&good(), instances(), "exit 0");
        fs::write(fx.path("id_rsa.pub"), "ssh-ed25519 AAAA x\n").unwrap();
        let err = run(&fx.opts).unwrap_err().to_string();
        assert!(err.contains("RSA"), "{err}");
        fs::remove_file(fx.path("id_rsa.pub")).unwrap();
        let err = run(&fx.opts).unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
        fx.opts.identity = Some(fx.path("nope"));
        assert!(run(&fx.opts).is_err());
        assert!(fx.log().is_empty(), "nothing is contacted without a key");
    }

    /// Raise SIGTERM for this process once READY exists and RUN is still
    /// going on (so that it is the handler of run() that gets it). Returns
    /// the flag to set when run() is over, and the thread.
    fn sigterm_when(
        ready: impl Fn() -> bool + Send + 'static,
    ) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
        let over = Arc::new(AtomicBool::new(false));
        let flag = over.clone();
        let waiter = thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(20);
            while !ready() && Instant::now() < end {
                thread::sleep(Duration::from_millis(10));
            }
            if !flag.load(Ordering::SeqCst) && ready() {
                // SAFETY: signalling ourselves, while run() has its handler in.
                unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
            }
        });
        (over, waiter)
    }

    #[test]
    fn a_group_sigterm_stops_ssh_and_cleans_up() {
        let fx = fixture(
            &good(),
            instances(),
            "touch \"$(dirname \"$0\")/ready\"\nexec sleep 30",
        );
        let ready = fx.path("ready");
        let (over, waiter) = sigterm_when(move || ready.exists());
        let started = Instant::now();
        let result = run(&fx.opts);
        over.store(true, Ordering::SeqCst);
        waiter.join().unwrap();
        assert_eq!(result.unwrap(), 128 + libc::SIGTERM);
        assert!(started.elapsed() < Duration::from_secs(15));
        let deletes = fx.deletes();
        assert_eq!(deletes.len(), 1, "{:#?}", fx.log());
        assert!(deletes[0].contains(CONN));
    }

    #[test]
    fn a_signal_during_the_create_request_still_deletes_the_connection() {
        let fx = fixture(&good(), instances(), "exit 0");
        fx.write("create_sleep", "2");
        let oci_log = fx.path("oci.log");
        let (over, waiter) = sigterm_when(move || {
            fs::read_to_string(&oci_log)
                .is_ok_and(|l| l.contains("instance-console-connection create"))
        });
        let started = Instant::now();
        let result = run(&fx.opts);
        over.store(true, Ordering::SeqCst);
        waiter.join().unwrap();
        assert_eq!(result.unwrap(), 128 + libc::SIGTERM);
        assert!(fx.ssh_args().is_none());
        // The request ran to its end, and only then was the connection deleted.
        let log = fx.log();
        assert_eq!(fx.deletes().len(), 1, "{log:#?}");
        assert!(fx.deletes()[0].contains(CONN));
        assert!(fx.path("after_create").exists(), "{log:#?}");
        assert!(started.elapsed() < CLEANUP_WAIT + Duration::from_secs(5));
    }

    /// Spawn the helper without going through `run`.
    fn start_owner(fx: &Fixture, keep: bool) -> Supervisor {
        let _lock = RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let target = Target {
            id: INST.into(),
            compartment: Some(COMP.into()),
        };
        let mut opts = fx.opts.clone();
        opts.keep = keep;
        Supervisor::start(&OciCli::new(&opts), &target, &fx.path("id_rsa.pub"), keep).unwrap()
    }

    #[test]
    fn a_dead_owner_leaves_nothing_behind() {
        // What a SIGKILL of the whole process group does to us: the helper is
        // in a session of its own, our end of its pipe closes, and the create
        // request is still in flight.
        let fx = fixture(&good(), instances(), "exit 0");
        fx.write("create_sleep", "1");
        let owner = start_owner(&fx, false);
        assert_ne!(
            unsafe { libc::getsid(owner.child.id() as i32) },
            unsafe { libc::getsid(0) },
            "the helper must not share our session"
        );
        assert_ne!(unsafe { libc::getpgid(owner.child.id() as i32) }, unsafe {
            libc::getpgrp()
        });
        let log = fx.path("oci.log");
        assert!(eventually(|| {
            fs::read_to_string(&log).is_ok_and(|l| l.contains("instance-console-connection create"))
        }));
        drop(owner);
        assert!(eventually(|| fx.deletes().len() == 1), "{:#?}", fx.log());
        assert!(fx.deletes()[0].contains(CONN));
        assert!(fx.path("after_create").exists());
        assert!(fx.lists().is_empty(), "the id was known: no lookup");
    }

    #[test]
    fn a_dead_owner_with_a_lost_answer_is_found_by_its_tag() {
        let fx = fixture(&good(), instances(), "exit 0");
        fx.write("create_sleep", "1");
        fx.write("create_fail", "ServiceError:\n{\"code\": \"Timeout\"}\n");
        fx.write("list_result", &format!("{CONN}\n"));
        drop(start_owner(&fx, false));
        assert!(eventually(|| fx.deletes().len() == 1), "{:#?}", fx.log());
        assert!(fx.deletes()[0].contains(CONN));
        let nonce = nonce_of(&fx.log());
        assert!(fx.lists()[0].contains(&format!("=='{nonce}'")));
    }

    #[test]
    fn a_dead_owner_keeps_what_it_was_told_to_keep() {
        let fx = fixture(&good(), instances(), "exit 0");
        let mut owner = start_owner(&fx, true);
        assert_eq!(owner.wait_created().unwrap(), CONN);
        drop(owner);
        thread::sleep(Duration::from_millis(1500));
        assert!(fx.deletes().is_empty(), "{:#?}", fx.log());
    }

    #[test]
    fn a_finished_owner_deletes_once_and_reports() {
        let fx = fixture(&good(), instances(), "exit 0");
        let mut owner = start_owner(&fx, false);
        assert_eq!(owner.wait_created().unwrap(), CONN);
        assert!(fx.deletes().is_empty(), "nothing is deleted while in use");
        owner.finish(false);
        assert_eq!(fx.deletes().len(), 1);
        assert!(owner.reap(Duration::from_secs(5)));
    }

    #[test]
    fn timeouts_and_failures() {
        // The signal state is process-wide: hold the lock that keeps run()
        // out, and start from a clean state.
        let _lock = RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        SIGNAL.store(0, Ordering::SeqCst);
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        assert!(matches!(
            capture(cmd, Duration::from_millis(100)),
            Err(RunError::Timeout)
        ));
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo oops >&2; exit 3"]);
        match capture(cmd, Duration::from_secs(5)) {
            Err(RunError::Failed { code, stderr }) => {
                assert_eq!(code, Some(3));
                assert_eq!(stderr, "oops\n");
            }
            other => panic!("{other:?}"),
        }
    }
}
