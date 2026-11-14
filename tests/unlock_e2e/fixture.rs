//! Test support: a throwaway sshd on loopback that plays an initrd, synthetic
//! keys and a synthetic passphrase. Nothing here touches the network beyond
//! 127.0.0.1, any real key, or any real host.
//!
//! The "initrd" is an unprivileged `sshd` run by the test user on a random
//! loopback port with its own throwaway host key. Its client key is forced to
//! run a small shell script (like the dropbear/openssh initrd of a real
//! system, where the key is bound to `cryptroot-unlock`). The script answers
//! the probes the unlocker sends and checks the passphrase it receives
//! against a synthetic one, using fake `systemctl`, `cryptsetup`,
//! `luks-mount` and `systemd-tty-ask-password-agent` executables placed
//! first in the remote `PATH`.

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The synthetic passphrase the fake initrd expects. It deliberately has
/// spaces, quotes and a dollar sign to prove nothing in the chain re-parses it.
pub const PASSPHRASE: &str = "synthetic pass $phrase 'q' \"dq\" 7f3a";
/// A passphrase the fake initrd rejects.
pub const WRONG_PASSPHRASE: &str = "synthetic wrong passphrase 91c2";
/// The device the fake `systemctl cat` reports for the cryptsetup unit.
pub const DEVICE: &str = "/dev/disk/by-uuid/0000-SYNTHETIC";

/// Where to find OpenSSH tools: PATH first, then the usual system places.
fn find_tool(name: &str) -> PathBuf {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    for d in [
        "/run/current-system/sw/bin",
        "/usr/sbin",
        "/usr/local/sbin",
        "/usr/bin",
        "/usr/lib/openssh",
        "/usr/lib/ssh",
    ] {
        dirs.push(PathBuf::from(d));
    }
    dirs.into_iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| {
            panic!(
                "the unlock e2e fixture needs OpenSSH's `{name}` (install openssh); \
                 it is not on PATH or in the usual system directories"
            )
        })
}

/// Is an executable called NAME on PATH?
pub fn have_tool(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

fn run_ok(cmd: &mut Command) -> String {
    let out = cmd.output().expect("cannot run command");
    assert!(
        out.status.success(),
        "{:?} failed: {}",
        cmd.get_program(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Generate a passphrase-less ed25519 key pair; returns (private path,
/// `ssh-ed25519 AAAA...` public key without comment).
fn keygen(dir: &Path, name: &str) -> (PathBuf, String) {
    let path = dir.join(name);
    run_ok(
        Command::new(find_tool("ssh-keygen"))
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "synthetic-e2e", "-f"])
            .arg(&path),
    );
    let public = fs::read_to_string(path.with_extension("pub")).unwrap_or_else(|_| {
        fs::read_to_string(format!("{}.pub", path.display())).expect("public key")
    });
    let mut words = public.split_whitespace();
    let line = format!("{} {}", words.next().unwrap(), words.next().unwrap());
    (path, line)
}

fn write_file(path: &Path, mode: u32, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .unwrap();
    f.write_all(content.as_bytes()).unwrap();
    // `mode` is masked by the umask on creation; fix it for good.
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// How the fake sshd authorizes its client key.
pub enum Login {
    /// Force the fake initrd script for this key (the usual initrd setup).
    ForcedInitrd,
    /// Authorize a different key only: the unlocker's key is refused.
    OtherKey,
}

/// An unprivileged sshd on a random loopback port. Killed on drop.
pub struct Sshd {
    pub port: u16,
    /// `ssh-ed25519 AAAA...` of its host key.
    pub host_key: String,
    child: Child,
    pub log: PathBuf,
}

impl Sshd {
    fn start(dir: &Path, name: &str, authorized_keys: &str) -> Self {
        let (host_key_path, host_key) = keygen(dir, &format!("{name}_host_key"));
        let ak = dir.join(format!("{name}_authorized_keys"));
        write_file(&ak, 0o600, authorized_keys);
        let log = dir.join(format!("{name}_sshd.log"));
        let mut last_err = String::new();
        for _ in 0..8 {
            let port = free_port();
            let conf = dir.join(format!("{name}_sshd_config"));
            write_file(
                &conf,
                0o600,
                &format!(
                    "Port {port}\n\
                     ListenAddress 127.0.0.1\n\
                     HostKey {host_key}\n\
                     PidFile none\n\
                     AuthorizedKeysFile {ak}\n\
                     PubkeyAuthentication yes\n\
                     PasswordAuthentication no\n\
                     KbdInteractiveAuthentication no\n\
                     UsePAM no\n\
                     StrictModes no\n\
                     PermitTTY yes\n\
                     AllowTcpForwarding yes\n\
                     LogLevel VERBOSE\n",
                    host_key = host_key_path.display(),
                    ak = ak.display(),
                ),
            );
            let mut child = Command::new(find_tool("sshd"))
                .arg("-D")
                .arg("-f")
                .arg(&conf)
                .arg("-E")
                .arg(&log)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("cannot start sshd");
            match wait_for_banner(&mut child, port) {
                Ok(()) => {
                    return Self {
                        port,
                        host_key,
                        child,
                        log,
                    };
                }
                Err(e) => {
                    last_err = e;
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        panic!(
            "sshd did not start: {last_err}\n{}",
            fs::read_to_string(&log).unwrap_or_default()
        );
    }

    /// The `known_hosts` line that pins this sshd's host key.
    pub fn known_hosts_line(&self) -> String {
        format!("[127.0.0.1]:{} {}", self.port, self.host_key)
    }

    pub fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn free_port() -> u16 {
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .and_then(|l| l.local_addr())
        .expect("no free loopback port")
        .port()
}

fn wait_for_banner(child: &mut Child, port: u16) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("sshd exited early with {status}"));
        }
        if let Ok(mut s) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut buf = [0u8; 4];
            if s.read_exact(&mut buf).is_ok() && &buf == b"SSH-" {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err("sshd did not answer in time".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One finished run of the binary under test.
pub struct Run {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn output(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// Everything one scenario needs.
pub struct Fixture {
    pub dir: TempDir,
    pub user: String,
    pub client_key: PathBuf,
    client_pub: String,
    /// The "booted system" host key (not the initrd's): signs baselines and
    /// serves as the wrong key in host-key mismatch scenarios.
    pub regular_key: PathBuf,
    pub regular_host_key: String,
    pub initrd: Sshd,
    pub passphrase_file: PathBuf,
}

impl Fixture {
    /// An initrd that expects [`PASSPHRASE`] and is in its initrd state.
    pub fn new() -> Self {
        Self::with_login(Login::ForcedInitrd)
    }

    pub fn with_login(login: Login) -> Self {
        // Short path: sshd and ssh sockets/paths stay well below limits.
        let dir = tempfile::Builder::new()
            .prefix("lsu-e2e.")
            .tempdir()
            .expect("tempdir");
        let d = dir.path();
        fs::set_permissions(d, fs::Permissions::from_mode(0o700)).unwrap();
        let user = run_ok(Command::new("id").arg("-un")).trim().to_string();

        let (client_key, client_pub) = keygen(d, "client_key");
        let (_other_key, other_pub) = keygen(d, "other_client_key");
        let (regular_key, regular_host_key) = keygen(d, "regular_host_key");

        install_initrd_script(d);
        let forced = d.join("forced-command");
        let authorized = match login {
            Login::ForcedInitrd => format!(
                "command=\"{}\",no-port-forwarding,no-agent-forwarding,no-X11-forwarding {client_pub}\n",
                forced.display()
            ),
            Login::OtherKey => format!("{other_pub}\n"),
        };
        let initrd = Sshd::start(d, "initrd", &authorized);

        write_file(&d.join("expected"), 0o600, PASSPHRASE);
        let passphrase_file = d.join("passphrase");
        write_file(&passphrase_file, 0o600, &format!("{PASSPHRASE}\n"));
        let fixture = Self {
            dir,
            user,
            client_key,
            client_pub,
            regular_key,
            regular_host_key,
            initrd,
            passphrase_file,
        };
        fixture.set_initrd(true);
        fixture
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn client_pub(&self) -> &str {
        &self.client_pub
    }

    /// Start another sshd that lets the client key in as a plain shell and
    /// forward TCP (a jumphost).
    pub fn jumphost(&self) -> Sshd {
        // Commands run with the test's own PATH (so `nc` is found as on a
        // real bastion); `-W` forwarding is not affected by a forced command.
        let script = self.path().join("jump-command");
        write_file(
            &script,
            0o700,
            &format!(
                "#!/bin/sh\nPATH='{}'\nexport PATH\nexec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\"\n",
                std::env::var("PATH").unwrap_or_default().replace('\'', "")
            ),
        );
        Sshd::start(
            self.path(),
            "jump",
            &format!("command=\"{}\" {}\n", script.display(), self.client_pub),
        )
    }

    /// Does `/etc/initrd-release` "exist" on the fake target?
    pub fn set_initrd(&self, present: bool) {
        let flag = self.path().join("state/initrd");
        if present {
            write_file(&flag, 0o600, "");
        } else {
            let _ = fs::remove_file(flag);
        }
    }

    /// Make the fake target "booted": the test healthcheck command passes.
    pub fn set_booted(&self, booted: bool) {
        let flag = self.booted_flag();
        if booted {
            write_file(&flag, 0o600, "");
        } else {
            let _ = fs::remove_file(flag);
        }
    }

    pub fn booted_flag(&self) -> PathBuf {
        self.path().join("state/booted")
    }

    /// Passphrase attempts the fake initrd received (one line per attempt:
    /// `ok` or `rejected`). It never records the passphrase itself.
    pub fn attempts(&self) -> Vec<String> {
        fs::read_to_string(self.path().join("state/attempts"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn unlocked(&self) -> bool {
        self.path().join("state/unlocked").exists()
    }

    pub fn restarted(&self) -> bool {
        self.path().join("state/restarted").exists()
    }

    /// Every remote command line the fake initrd was asked to run.
    pub fn remote_commands(&self) -> Vec<String> {
        fs::read_to_string(self.path().join("state/commands"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn cryptsetup_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path().join("state/cryptsetup"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Write a file under the fixture directory and return its path.
    pub fn write(&self, name: &str, mode: u32, content: &str) -> PathBuf {
        let path = self.path().join(name);
        write_file(&path, mode, content);
        path
    }

    /// The environment of a `run --once` against the fake initrd: luks type
    /// `raw`, the initrd host key pinned, no healthcheck, no checksum gate.
    pub fn base_env(&self) -> Vec<(String, String)> {
        let tmp = self.path().join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        [
            ("SSH_HOSTNAME", "127.0.0.1".to_string()),
            ("SSH_PORT", self.initrd.port.to_string()),
            ("SSH_USERNAME", self.user.clone()),
            ("SSH_KEY", self.client_key.display().to_string()),
            ("SSH_INITRD_KNOWN_HOSTS", self.initrd.known_hosts_line()),
            (
                "LUKS_PASSPHRASE_FILE",
                self.passphrase_file.display().to_string(),
            ),
            ("LUKS_TYPE", "raw".to_string()),
            ("SSH_CONNECTION_TIMEOUT", "5".to_string()),
            ("TICK_TIMEOUT", "60".to_string()),
            ("RUN_ONCE", "1".to_string()),
            ("TMPDIR", tmp.display().to_string()),
            (
                "EVENTS_FILE",
                self.path().join("events").display().to_string(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// The events the tool recorded (`success`, `failure`, `warning`).
    pub fn events(&self) -> Vec<String> {
        fs::read_to_string(self.path().join("events"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .map(str::to_string)
            .collect()
    }

    /// Run the binary with ARGS. The environment is cleared first (no ssh
    /// agent, no inherited settings), then [`Self::base_env`] and EXTRA are
    /// applied, later entries winning.
    pub fn run_args(&self, extra: &[(&str, &str)], args: &[&str]) -> Run {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_luks-ssh-unlock"));
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.path())
            .env("LUKS_SSH_UNLOCK_NO_SUDO", "1")
            .env("NO_COLOR", "1");
        for (k, v) in self.base_env() {
            cmd.env(k, v);
        }
        for (k, v) in extra {
            cmd.env(k, v);
        }
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cmd.spawn().expect("cannot run luks-ssh-unlock");
        let pid = child.id();
        // Watchdog: a hang must fail the test, not the whole suite.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if rx.recv_timeout(Duration::from_secs(120)).is_err() {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
        });
        let out = child.wait_with_output().expect("wait");
        let _ = tx.send(());
        let _ = watchdog.join();
        Run {
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// `run --once` with the default environment plus EXTRA.
    pub fn run_once(&self, extra: &[(&str, &str)]) -> Run {
        self.run_args(extra, &["run"])
    }

    /// Sign FILE like the booted target does for its initrd baseline
    /// (`ssh-keygen -Y sign`, namespace `initrd-checksum`, the regular host
    /// key), writing `FILE.sig`.
    pub fn sign_baseline(&self, file: &Path) {
        run_ok(
            Command::new(find_tool("ssh-keygen"))
                .args(["-Y", "sign", "-q", "-n", "initrd-checksum", "-f"])
                .arg(&self.regular_key)
                .arg(file),
        );
    }
}

/// Write the forced command and the fake remote executables.
fn install_initrd_script(dir: &Path) {
    let d = dir.display();
    let state = dir.join("state");
    fs::create_dir_all(&state).unwrap();
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();

    // Shared by the fake tools: check the passphrase on stdin line $1.
    write_file(
        &bin.join("lib.sh"),
        0o600,
        &format!(
            r#"D='{d}'
check() {{
    expected=$(cat "$D/expected")
    if [ "$1" = "$expected" ]; then
        echo ok >> "$D/state/attempts"
        : > "$D/state/unlocked"
        echo "Passphrase accepted."
        return 0
    fi
    echo rejected >> "$D/state/attempts"
    # A careless target that echoes what it was given back: the unlocker
    # must not let that reach its output.
    printf 'Wrong passphrase: %s\n' "$1"
    return 1
}}
"#
        ),
    );

    // Forced command: the only thing this "initrd" ever runs.
    write_file(
        &dir.join("forced-command"),
        0o700,
        &format!(
            r#"#!/bin/sh
D='{d}'
PATH="$D/bin:$PATH"
export PATH
printf '%s\n' "$SSH_ORIGINAL_COMMAND" >> "$D/state/commands"
case "$SSH_ORIGINAL_COMMAND" in
    "test -e /etc/initrd-release")
        [ -e "$D/state/initrd" ]
        exit $?
        ;;
    "")
        # No command: what cryptroot-unlock is bound to in an initrd.
        exec "$D/bin/cryptroot-unlock"
        ;;
    *)
        exec /bin/sh -c "$SSH_ORIGINAL_COMMAND"
        ;;
esac
"#
        ),
    );

    let tool = |name: &str, body: &str| {
        write_file(
            &bin.join(name),
            0o700,
            &format!("#!/bin/sh\n. '{d}/bin/lib.sh'\n{body}\n"),
        );
    };
    let read_and_check = "IFS= read -r pass || exit 3\ncheck \"$pass\"";
    tool("cryptroot-unlock", read_and_check);
    tool("luks-mount", read_and_check);
    tool("systemd-tty-ask-password-agent", read_and_check);
    tool(
        "systemctl",
        r##"case "$*" in
    "--no-pager --no-legend list-unit-files systemd-cryptsetup@*")
        echo "systemd-cryptsetup@root.service generated -"
        ;;
    "cat --no-pager systemd-cryptsetup@root.service")
        echo "# /run/systemd/generator/systemd-cryptsetup@root.service"
        echo "[Service]"
        echo "ExecStart=/usr/lib/systemd/systemd-cryptsetup attach 'root' '/dev/disk/by-uuid/0000-SYNTHETIC' 'none' ''"
        ;;
    "restart systemd-cryptsetup@root.service")
        # Restarting before the volume was opened would not boot anything.
        [ -e "$D/state/unlocked" ] || { echo "not unlocked yet"; exit 1; }
        : > "$D/state/restarted"
        ;;
    *)
        echo "unexpected systemctl call: $*" >&2
        exit 2
        ;;
esac"##,
    );
    tool(
        "cryptsetup",
        r#"printf '%s|%s|%s|%s\n' "$1" "$2" "$3" "$4" >> "$D/state/cryptsetup"
[ "$1" = luksOpen ] && [ "$2" = "/dev/disk/by-uuid/0000-SYNTHETIC" ] && [ "$3" = root ] && [ "$4" = - ] || { echo "unexpected cryptsetup arguments" >&2; exit 2; }
IFS= read -r pass || exit 3
check "$pass""#,
    );
}
