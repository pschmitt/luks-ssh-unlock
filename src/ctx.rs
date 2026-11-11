//! Runtime context of one target: resolved settings, private scratch files
//! (pinned known_hosts, permission-fixed keys) and OpenSSH command builders.

use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tempfile::TempDir;
use zeroize::Zeroizing;

use crate::daemon::State;
use crate::log::Logger;
use crate::process::{self, Output, TimedOut, shell_quote};
use crate::settings::{IpFamily, KnownHostsKind, Settings};

/// What the context is used for. Status probes never send notifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Run,
    Status,
}

/// Exit with a specific status code (configuration errors).
#[derive(Debug)]
pub struct Exit(pub i32, pub String);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Exit {}

/// Where an SSH connection goes.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub host: String,
    pub user: String,
    pub connect_address: Option<String>,
    pub hostkey_alias: Option<String>,
}

pub struct Ctx {
    pub s: Settings,
    pub log: Logger,
    pub mode: Mode,
    pub force: bool,
    key: PathBuf,
    jump_key: Option<PathBuf>,
    kh_default: Option<PathBuf>,
    kh_initrd: Option<PathBuf>,
    _scratch: TempDir,
    /// Address pinned for the current check (with --ipv4/--ipv6).
    pub pinned: Option<String>,
    pub deadline: Instant,
    pub state: Option<State>,
    pub passphrase: Option<Zeroizing<String>>,
}

/// Copy KEY into DIR with mode 0400 if its permissions are too open for
/// OpenSSH (mounted secrets often are 0444 or 0644).
fn usable_key(key: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let meta = fs::metadata(key)
        .map_err(|e| Exit(2, format!("SSH key {} is not readable: {e}", key.display())))?;
    fs::File::open(key)
        .map_err(|e| Exit(2, format!("SSH key {} is not readable: {e}", key.display())))?;
    if meta.permissions().mode() & 0o077 == 0 {
        return Ok(key.to_path_buf());
    }
    let copy = dir.join(name);
    let data = Zeroizing::new(fs::read(key)?);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .open(&copy)?;
    file.write_all(&data)?;
    Ok(copy)
}

fn known_hosts(
    inline: &Option<String>,
    file: &Option<PathBuf>,
    dir: &Path,
    name: &str,
) -> Result<Option<PathBuf>> {
    if let Some(lines) = inline {
        let path = dir.join(name);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(f, "{}", lines.trim_end())?;
        return Ok(Some(path));
    }
    if let Some(path) = file {
        fs::File::open(path).map_err(|e| {
            Exit(
                2,
                format!("known_hosts file {} is not readable: {e}", path.display()),
            )
        })?;
        return Ok(Some(path.clone()));
    }
    Ok(None)
}

/// Format a path list for `UserKnownHostsFile` (whitespace-separated).
fn kh_option(paths: &[&Path]) -> String {
    if paths.is_empty() {
        return "/dev/null".into();
    }
    paths
        .iter()
        .map(|p| {
            let p = p.display().to_string();
            if p.contains(char::is_whitespace) {
                format!("\"{p}\"")
            } else {
                p
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn is_ip(s: &str) -> bool {
    s.parse::<IpAddr>().is_ok()
}

impl Ctx {
    pub fn new(s: Settings, log: Logger, mode: Mode) -> Result<Self> {
        fs::create_dir_all(&s.tmpdir).ok();
        let scratch = tempfile::Builder::new()
            .prefix("luks-ssh-unlock.")
            .tempdir_in(&s.tmpdir)
            .with_context(|| {
                format!(
                    "cannot create a scratch directory in {}",
                    s.tmpdir.display()
                )
            })?;
        let dir = scratch.path();
        let key = usable_key(&s.key, dir, "ssh_key")?;
        let jump_key = match &s.jumphost {
            Some(j) if j.key != s.key => Some(usable_key(&j.key, dir, "jumphost_key")?),
            Some(_) => Some(key.clone()),
            None => None,
        };
        let kh_default = known_hosts(&s.known_hosts, &s.known_hosts_file, dir, "known_hosts")?;
        let kh_initrd = known_hosts(
            &s.initrd_known_hosts,
            &s.initrd_known_hosts_file,
            dir,
            "initrd_known_hosts",
        )?;
        Ok(Self {
            force: s.force,
            s,
            log,
            mode,
            key,
            jump_key,
            kh_default,
            kh_initrd,
            _scratch: scratch,
            pinned: None,
            deadline: Instant::now() + Duration::from_secs(3600),
            state: None,
            passphrase: None,
        })
    }

    /// Load the passphrase from the settings or the passphrase file.
    pub fn load_passphrase(&mut self) -> Result<()> {
        if self.passphrase.is_some() {
            return Ok(());
        }
        if let Some(p) = &self.s.passphrase {
            self.passphrase = Some(Zeroizing::new(p.clone()));
            return Ok(());
        }
        let Some(file) = &self.s.passphrase_file else {
            bail!(Exit(
                2,
                "no LUKS passphrase: set LUKS_PASSPHRASE_FILE or --luks-passphrase-file".into()
            ));
        };
        let mut text = Zeroizing::new(fs::read_to_string(file).map_err(|e| {
            Exit(
                3,
                format!(
                    "LUKS passphrase file {} is not readable: {e}",
                    file.display()
                ),
            )
        })?);
        while text.ends_with('\n') || text.ends_with('\r') {
            text.pop();
        }
        if text.is_empty() {
            bail!(Exit(
                2,
                format!("LUKS passphrase file {} is empty", file.display())
            ));
        }
        self.passphrase = Some(text);
        Ok(())
    }

    pub fn known_hosts_path(&self, kind: KnownHostsKind) -> Option<&Path> {
        if self.force {
            return None;
        }
        match kind {
            KnownHostsKind::Default => self.kh_default.as_deref(),
            KnownHostsKind::Initrd => self.kh_initrd.as_deref(),
        }
    }

    /// The target endpoint (with the address pinned for this check).
    pub fn endpoint(&self) -> Endpoint {
        let (connect_address, hostkey_alias) = match &self.pinned {
            Some(addr) => (
                Some(addr.clone()),
                Some(
                    self.s
                        .hostkey_alias
                        .clone()
                        .unwrap_or_else(|| self.s.hostname.clone()),
                ),
            ),
            None => (self.s.connect_address.clone(), self.s.hostkey_alias.clone()),
        };
        Endpoint {
            host: self.s.hostname.clone(),
            user: self.s.username.clone(),
            connect_address,
            hostkey_alias,
        }
    }

    /// The endpoint of the healthcheck command.
    pub fn healthcheck_endpoint(&self) -> Endpoint {
        let mut ep = self.endpoint();
        if let Some(host) = &self.s.healthcheck.host
            && host != &self.s.hostname
        {
            ep.host = host.clone();
            ep.connect_address = None;
            ep.hostkey_alias = None;
        }
        if let Some(user) = &self.s.healthcheck.user {
            ep.user = user.clone();
        }
        ep
    }

    /// Options common to every ssh/scp invocation, verifying host keys
    /// against KH (or not at all when there is none).
    fn base_opts(&self, kh: &[&Path]) -> Vec<String> {
        let mut o: Vec<String> = vec![
            "-F".into(),
            "/dev/null".into(),
            "-o".into(),
            format!("ConnectTimeout={}", self.s.connect_timeout),
            "-o".into(),
            "ControlMaster=no".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            format!("UserKnownHostsFile={}", kh_option(kh)),
            "-o".into(),
            format!(
                "StrictHostKeyChecking={}",
                if kh.is_empty() { "no" } else { "yes" }
            ),
        ];
        if self.force {
            o.extend(["-o".into(), "GlobalKnownHostsFile=/dev/null".into()]);
        }
        match self.s.ip_family {
            IpFamily::V4 => o.push("-4".into()),
            IpFamily::V6 => o.push("-6".into()),
            IpFamily::Any => {}
        }
        o
    }

    /// known_hosts files for the jumphost: it is a regular, booted host, but
    /// older setups pinned it in either file, so both are accepted.
    fn jumphost_known_hosts(&self) -> Vec<&Path> {
        if self.force {
            return vec![];
        }
        [self.kh_default.as_deref(), self.kh_initrd.as_deref()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// `ProxyCommand=ssh ... -W %h:%p` through the jumphost, if any. Unlike
    /// `ProxyJump`, this keeps our host-key settings for the hop.
    pub fn proxy_command(&self) -> Option<String> {
        let jump = self.s.jumphost.as_ref()?;
        let mut parts = vec!["ssh".to_string()];
        parts.extend(self.base_opts(&self.jumphost_known_hosts()));
        parts.extend([
            "-p".into(),
            jump.port.to_string(),
            "-i".into(),
            self.jump_key
                .as_ref()
                .unwrap_or(&self.key)
                .display()
                .to_string(),
            "-l".into(),
            jump.user.clone(),
            jump.host.clone(),
            "-W".into(),
            "%h:%p".into(),
        ]);
        let quoted: Vec<String> = parts.iter().map(|p| shell_quote(p)).collect();
        Some(format!("ProxyCommand={}", quoted.join(" ")))
    }

    /// Endpoint-specific `-o` options (address pinning, jumphost).
    fn endpoint_opts(&self, ep: &Endpoint) -> Vec<String> {
        let mut o = Vec::new();
        if let Some(addr) = &ep.connect_address {
            o.extend(["-o".into(), format!("HostName={addr}")]);
        }
        if let Some(alias) = &ep.hostkey_alias {
            o.extend(["-o".into(), format!("HostKeyAlias={alias}")]);
        }
        if let Some(proxy) = self.proxy_command() {
            o.extend(["-o".into(), proxy]);
        }
        o
    }

    /// `ssh` to EP, verifying against the KIND host keys, running REMOTE
    /// (a shell command line) or nothing (the forced command / login shell).
    pub fn ssh(
        &self,
        ep: &Endpoint,
        kind: KnownHostsKind,
        extra: &[&str],
        remote: Option<&str>,
    ) -> Command {
        let kh: Vec<&Path> = self.known_hosts_path(kind).into_iter().collect();
        let mut cmd = Command::new("ssh");
        cmd.args(self.base_opts(&kh))
            .args(extra)
            .args(self.endpoint_opts(ep))
            .arg("-p")
            .arg(self.s.port.to_string())
            .arg("-i")
            .arg(&self.key)
            .arg("-l")
            .arg(&ep.user)
            .arg("--")
            .arg(&ep.host);
        if let Some(remote) = remote {
            cmd.arg(remote);
        }
        cmd
    }

    /// `ssh` to the jumphost itself.
    pub fn ssh_jumphost(&self, remote: &str) -> Option<Command> {
        let jump = self.s.jumphost.as_ref()?;
        let mut cmd = Command::new("ssh");
        cmd.args(self.base_opts(&self.jumphost_known_hosts()))
            .arg("-p")
            .arg(jump.port.to_string())
            .arg("-i")
            .arg(self.jump_key.as_ref().unwrap_or(&self.key))
            .arg("-l")
            .arg(&jump.user)
            .arg("--")
            .arg(&jump.host)
            .arg(remote);
        Some(cmd)
    }

    /// `scp -r` of REMOTE_PATH on the target into LOCAL.
    pub fn scp_from(&self, kind: KnownHostsKind, remote_path: &str, local: &Path) -> Command {
        let ep = self.endpoint();
        let kh: Vec<&Path> = self.known_hosts_path(kind).into_iter().collect();
        let host = if ep.host.contains(':') {
            format!("[{}]", ep.host)
        } else {
            ep.host.clone()
        };
        let mut cmd = Command::new("scp");
        cmd.args(self.base_opts(&kh))
            .args(self.endpoint_opts(&ep))
            .arg("-P")
            .arg(self.s.port.to_string())
            .arg("-i")
            .arg(&self.key)
            .arg("-r")
            .arg(format!("{}@{}:{}", ep.user, host, remote_path))
            .arg(local);
        cmd
    }

    /// The identity, known_hosts and `-o` options the initrd-checksum helper
    /// needs to reach the target the same way.
    pub fn checksum_ssh_args(&self) -> Vec<String> {
        let ep = self.endpoint();
        let mut args = vec![
            "--host".to_string(),
            ep.host.clone(),
            "--ssh-user".into(),
            ep.user.clone(),
            "--identity".into(),
            self.key.display().to_string(),
        ];
        match self.known_hosts_path(KnownHostsKind::Initrd) {
            Some(path) => args.extend(["--known-hosts-file".into(), path.display().to_string()]),
            None => args.push("--insecure-ssh".into()),
        }
        let mut opts = vec![
            format!("Port={}", self.s.port),
            format!("ConnectTimeout={}", self.s.connect_timeout),
            "BatchMode=yes".into(),
        ];
        if let Some(addr) = &ep.connect_address {
            opts.push(format!("HostName={addr}"));
        }
        if let Some(alias) = &ep.hostkey_alias {
            opts.push(format!("HostKeyAlias={alias}"));
        }
        if let Some(proxy) = self.proxy_command() {
            opts.push(proxy);
        }
        for opt in opts {
            args.extend(["--ssh-option".into(), opt]);
        }
        args
    }

    /// Run CMD within the current check's deadline.
    pub fn exec(&self, cmd: Command, stdin: Option<Zeroizing<Vec<u8>>>) -> Result<Output> {
        if Instant::now() >= self.deadline {
            bail!(TimedOut);
        }
        let program = cmd.get_program().to_string_lossy().into_owned();
        let out = process::run(cmd, stdin, self.deadline)
            .with_context(|| format!("cannot run {program}"))?;
        if out.timed_out {
            bail!(TimedOut);
        }
        Ok(out)
    }

    /// Time left in the current check, capped at CAP.
    pub fn remaining(&self, cap: Duration) -> Result<Duration> {
        let left = self.deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!(TimedOut);
        }
        Ok(left.min(cap))
    }

    pub fn start_check(&mut self) {
        self.deadline = Instant::now() + Duration::from_secs(self.s.tick_timeout);
        self.pinned = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::GlobalOpts;
    use std::collections::HashMap;

    fn ctx(pairs: &[(&str, &str)]) -> (Ctx, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("id");
        fs::write(&key, "KEY").unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o644)).unwrap();
        let mut map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        map.insert("SSH_KEY".into(), key.display().to_string());
        map.insert("TMPDIR".into(), dir.path().display().to_string());
        let s = Settings::resolve(&GlobalOpts::default(), &|k| map.get(k).cloned()).unwrap();
        (Ctx::new(s, Logger::stderr(false), Mode::Run).unwrap(), dir)
    }

    fn args(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn copies_loose_key() {
        let (c, _d) = ctx(&[("SSH_HOSTNAME", "h")]);
        let mode = fs::metadata(&c.key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400);
        assert_ne!(c.key, c.s.key);
    }

    #[test]
    fn strict_with_pinned_keys() {
        let (c, _d) = ctx(&[
            ("SSH_HOSTNAME", "h"),
            ("SSH_PORT", "2222"),
            ("SSH_INITRD_KNOWN_HOSTS", "h ssh-ed25519 AAAA"),
        ]);
        let a = args(&c.ssh(&c.endpoint(), KnownHostsKind::Initrd, &[], Some("true")));
        assert!(a.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(a.windows(2).any(|w| w == ["-p", "2222"]));
        assert_eq!(a.last().unwrap(), "true");
        let a = args(&c.ssh(&c.endpoint(), KnownHostsKind::Default, &[], None));
        assert!(a.contains(&"StrictHostKeyChecking=no".to_string()));
        assert!(a.contains(&"UserKnownHostsFile=/dev/null".to_string()));
    }

    #[test]
    fn force_disables_all_host_keys() {
        let (mut c, _d) = ctx(&[
            ("SSH_HOSTNAME", "h"),
            ("SSH_INITRD_KNOWN_HOSTS", "h ssh-ed25519 AAAA"),
        ]);
        c.force = true;
        let a = args(&c.ssh(&c.endpoint(), KnownHostsKind::Initrd, &[], None));
        assert!(a.contains(&"GlobalKnownHostsFile=/dev/null".to_string()));
        assert!(a.contains(&"StrictHostKeyChecking=no".to_string()));
    }

    #[test]
    fn jumphost_proxy_command() {
        let (c, _d) = ctx(&[
            ("SSH_HOSTNAME", "h"),
            ("SSH_JUMPHOST", "bastion"),
            ("SSH_JUMPHOST_PORT", "2200"),
            ("SSH_KNOWN_HOSTS", "bastion ssh-ed25519 AAAA"),
        ]);
        let proxy = c.proxy_command().unwrap();
        assert!(proxy.starts_with("ProxyCommand=ssh -F /dev/null"));
        assert!(proxy.contains("-p 2200"));
        assert!(proxy.contains("StrictHostKeyChecking=yes"));
        assert!(proxy.ends_with("bastion -W %h:%p"));
    }

    #[test]
    fn pinned_address_keeps_hostname_for_keys() {
        let (mut c, _d) = ctx(&[("SSH_HOSTNAME", "h.example")]);
        c.pinned = Some("192.0.2.1".into());
        let ep = c.endpoint();
        assert_eq!(ep.connect_address.as_deref(), Some("192.0.2.1"));
        assert_eq!(ep.hostkey_alias.as_deref(), Some("h.example"));
    }
}
