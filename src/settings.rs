//! Command-line options, environment variables and the resolved settings of
//! one unlock target.
//!
//! Every option can be given on the command line or through an environment
//! variable (the historical interface of the shell implementation, used by the
//! NixOS module and the container image). The command line wins.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;

/// Options shared by all subcommands. All values are optional here so that
/// the environment can fill in what the command line leaves out.
#[derive(Args, Debug, Default, Clone)]
pub struct GlobalOpts {
    /// Enable debug logging [env: DEBUG]
    #[arg(long, short = 'D', global = true, help_heading = "General")]
    pub debug: bool,

    /// Make one unlock attempt and exit instead of watching the target [env: RUN_ONCE]
    #[arg(long, global = true, help_heading = "General")]
    pub once: bool,

    /// Skip initrd checksum/signature and SSH host-key checks (needs --once) [env: FORCE]
    #[arg(long, short = 'f', global = true, help_heading = "General")]
    pub force: bool,

    /// Seconds between two checks of the target [env: SLEEP_INTERVAL, default: 10]
    #[arg(
        long,
        short = 'i',
        visible_alias = "sleep",
        alias = "sleep-interval",
        short_alias = 's',
        global = true,
        value_name = "SECONDS",
        help_heading = "General"
    )]
    pub interval: Option<String>,

    /// Abort a check that takes longer than this [env: TICK_TIMEOUT, default: 120]
    #[arg(
        long,
        visible_alias = "timeout",
        global = true,
        value_name = "SECONDS",
        help_heading = "General"
    )]
    pub tick_timeout: Option<String>,

    /// Append notification events to FILE [env: EVENTS_FILE]
    #[arg(
        long,
        short = 'e',
        visible_alias = "event-file",
        alias = "event",
        global = true,
        value_name = "FILE",
        help_heading = "General"
    )]
    pub events_file: Option<String>,

    /// Target host to unlock [env: SSH_HOSTNAME]
    #[arg(long, short = 'H', visible_alias = "ssh-host", aliases = ["ssh-hostname"], global = true, value_name = "HOST", help_heading = "SSH")]
    pub host: Option<String>,

    /// SSH port of the target [env: SSH_PORT, default: 22]
    #[arg(
        long,
        short = 'P',
        visible_alias = "ssh-port",
        global = true,
        value_name = "PORT",
        help_heading = "SSH"
    )]
    pub port: Option<String>,

    /// SSH user [env: SSH_USERNAME, default: root]
    #[arg(long, short = 'u', visible_alias = "user", aliases = ["ssh-user", "ssh-username"], global = true, value_name = "USER", help_heading = "SSH")]
    pub username: Option<String>,

    /// SSH private key [env: SSH_KEY, default: /run/secrets/ssh_key]
    #[arg(long, short = 'k', visible_alias = "key", aliases = ["private-key", "pkey"], global = true, value_name = "FILE", help_heading = "SSH")]
    pub ssh_key: Option<String>,

    /// SSH connect timeout in seconds [env: SSH_CONNECTION_TIMEOUT, default: 5]
    #[arg(long, global = true, value_name = "SECONDS", help_heading = "SSH")]
    pub connect_timeout: Option<String>,

    /// Inline known_hosts content pinning the target's regular host keys [env: SSH_KNOWN_HOSTS]
    #[arg(long, global = true, value_name = "LINES", help_heading = "SSH")]
    pub ssh_known_hosts: Option<String>,

    /// known_hosts file pinning the target's regular host keys [env: SSH_KNOWN_HOSTS_FILE]
    #[arg(long, global = true, value_name = "FILE", help_heading = "SSH")]
    pub ssh_known_hosts_file: Option<String>,

    /// Inline known_hosts content pinning the initrd host keys [env: SSH_INITRD_KNOWN_HOSTS]
    #[arg(long, global = true, value_name = "LINES", help_heading = "SSH")]
    pub ssh_initrd_known_hosts: Option<String>,

    /// known_hosts file pinning the initrd host keys [env: SSH_INITRD_KNOWN_HOSTS_FILE]
    #[arg(long, global = true, value_name = "FILE", help_heading = "SSH")]
    pub ssh_initrd_known_hosts_file: Option<String>,

    /// Use IPv4 only [env: FORCE_IPV4]
    #[arg(
        long,
        short = '4',
        visible_alias = "ipv4",
        global = true,
        conflicts_with = "force_ipv6",
        help_heading = "SSH"
    )]
    pub force_ipv4: bool,

    /// Use IPv6 only [env: FORCE_IPV6]
    #[arg(
        long,
        short = '6',
        visible_alias = "ipv6",
        global = true,
        help_heading = "SSH"
    )]
    pub force_ipv6: bool,

    /// Skip the TCP probe of the SSH port before logging in [env: SKIP_SSH_PORT_CHECK]
    #[arg(
        long,
        visible_alias = "skip-port-check",
        global = true,
        help_heading = "SSH"
    )]
    pub skip_ssh_port_check: bool,

    /// Reach the target through this bastion host [env: SSH_JUMPHOST]
    #[arg(
        long,
        short = 'J',
        visible_alias = "ssh-jumphost",
        global = true,
        value_name = "HOST",
        help_heading = "Jumphost"
    )]
    pub jumphost: Option<String>,

    /// Jumphost SSH user [env: SSH_JUMPHOST_USERNAME, default: root]
    #[arg(long, short = 'U', visible_alias = "ssh-jumphost-username", aliases = ["jusername", "ju"], global = true, value_name = "USER", help_heading = "Jumphost")]
    pub jumphost_username: Option<String>,

    /// Jumphost SSH port [env: SSH_JUMPHOST_PORT, default: the target's port]
    #[arg(long, visible_alias = "ssh-jumphost-port", aliases = ["jport", "jp"], global = true, value_name = "PORT", help_heading = "Jumphost")]
    pub jumphost_port: Option<String>,

    /// Jumphost SSH key [env: SSH_JUMPHOST_KEY, default: the target's key]
    #[arg(long, short = 'K', visible_alias = "ssh-jumphost-key", aliases = ["jkey", "jk"], global = true, value_name = "FILE", help_heading = "Jumphost")]
    pub jumphost_key: Option<String>,

    /// How the passphrase is delivered: raw, systemd, systemd-tool, luks-mount [env: LUKS_TYPE, default: raw]
    #[arg(
        long,
        short = 't',
        visible_alias = "type",
        global = true,
        value_name = "TYPE",
        help_heading = "LUKS"
    )]
    pub luks_type: Option<String>,

    /// LUKS passphrase (insecure: visible in ps; prefer --luks-passphrase-file) [env: LUKS_PASSPHRASE]
    #[arg(
        long,
        short = 'p',
        visible_alias = "password",
        alias = "luks-password",
        global = true,
        value_name = "PASSPHRASE",
        help_heading = "LUKS"
    )]
    pub luks_passphrase: Option<String>,

    /// File holding the LUKS passphrase [env: LUKS_PASSPHRASE_FILE, default: /run/secrets/luks_password_<host>]
    #[arg(
        long,
        short = 'F',
        alias = "luks-password-file",
        global = true,
        value_name = "FILE",
        help_heading = "LUKS"
    )]
    pub luks_passphrase_file: Option<String>,

    /// Expected initrd checksum baseline; the unlock is refused if the initrd differs [env: INITRD_CHECKSUM_FILE]
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        help_heading = "Initrd validation"
    )]
    pub initrd_checksum_file: Option<String>,

    /// Directory where baselines fetched from the booted target are stored [env: INITRD_CHECKSUM_DIR]
    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help_heading = "Initrd validation"
    )]
    pub initrd_checksum_dir: Option<String>,

    /// initrd-checksum helper [env: INITRD_CHECKSUM_SCRIPT, default: next to this binary]
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help_heading = "Initrd validation"
    )]
    pub initrd_checksum_script: Option<String>,

    /// Run the checksum with a minimal BusyBox bundle uploaded to the target [env: PARANOID]
    #[arg(long, global = true, help_heading = "Initrd validation")]
    pub paranoid: bool,

    /// Refuse to unlock unless the baseline carries a valid signature [env: INITRD_CHECKSUM_REQUIRE_SIGNATURE]
    #[arg(long, global = true, help_heading = "Initrd validation")]
    pub require_signature: bool,

    /// TCP port that is only open once the target has booted [env: HEALTHCHECK_PORT]
    #[arg(long, global = true, value_name = "PORT", help_heading = "Healthcheck")]
    pub healthcheck_port: Option<String>,

    /// Command that succeeds over SSH only once the target has booted [env: HEALTHCHECK_REMOTE_CMD]
    #[arg(long, visible_alias = "remote-check", aliases = ["healthcheck-remote-cmd", "remote-command", "remote-cmd", "rcmd"], global = true, value_name = "CMD", help_heading = "Healthcheck")]
    pub healthcheck_cmd: Option<String>,

    /// Host to run the healthcheck command on [env: HEALTHCHECK_REMOTE_HOSTNAME, default: the target]
    #[arg(long, visible_alias = "hc-host", aliases = ["healthcheck-hostname", "hc-hostname"], global = true, value_name = "HOST", help_heading = "Healthcheck")]
    pub healthcheck_host: Option<String>,

    /// User for the healthcheck command [env: HEALTHCHECK_REMOTE_USERNAME, default: the SSH user]
    #[arg(long, visible_alias = "hc-user", aliases = ["healthcheck-username", "hc-username"], global = true, value_name = "USER", help_heading = "Healthcheck")]
    pub healthcheck_user: Option<String>,

    /// Host keys the healthcheck is verified against: default or initrd [env: SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE]
    #[arg(long, global = true, value_name = "TYPE", help_heading = "Healthcheck")]
    pub healthcheck_known_hosts_type: Option<String>,

    /// Apprise API endpoint for notifications [env: APPRISE_URL]
    #[arg(
        long,
        short = 'a',
        visible_alias = "apprise",
        global = true,
        value_name = "URL",
        help_heading = "Notifications"
    )]
    pub apprise_url: Option<String>,

    /// Apprise tag [env: APPRISE_TAG]
    #[arg(
        long,
        alias = "tag",
        global = true,
        value_name = "TAG",
        help_heading = "Notifications"
    )]
    pub apprise_tag: Option<String>,

    /// Apprise title, may contain #placeholders [env: APPRISE_TITLE]
    #[arg(
        long,
        alias = "title",
        global = true,
        value_name = "TITLE",
        help_heading = "Notifications"
    )]
    pub apprise_title: Option<String>,

    /// msmtp account used for mails [env: MSMTP_ACCOUNT]
    #[arg(
        long,
        visible_alias = "email-account",
        global = true,
        value_name = "ACCOUNT",
        help_heading = "Notifications"
    )]
    pub msmtp_email_account: Option<String>,

    /// Mail notifications to this address [env: EMAIL_RECIPIENT]
    #[arg(
        long,
        visible_alias = "email-to",
        global = true,
        value_name = "ADDRESS",
        help_heading = "Notifications"
    )]
    pub email_recipient: Option<String>,

    /// Sender address of notification mails [env: EMAIL_FROM]
    #[arg(
        long,
        visible_alias = "email-sender",
        global = true,
        value_name = "ADDRESS",
        help_heading = "Notifications"
    )]
    pub email_from: Option<String>,

    /// Subject of notification mails, may contain #placeholders [env: EMAIL_SUBJECT]
    #[arg(
        long,
        global = true,
        value_name = "SUBJECT",
        help_heading = "Notifications"
    )]
    pub email_subject: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LuksType {
    /// Write the passphrase to the SSH session's stdin (dropbear/cryptroot-unlock).
    Raw,
    /// Answer systemd's password agent through a remote TTY.
    Systemd,
    /// Open the root device with cryptsetup, then restart its cryptsetup unit.
    SystemdTool,
    /// Pipe the passphrase into `luks-mount` on the target.
    LuksMount,
}

impl LuksType {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "raw" | "direct" => Self::Raw,
            "systemd" | "dracut-systemd" | "dracut-sshd" | "dracut" | "alt" => Self::Systemd,
            "systemd-tool" | "arch" => Self::SystemdTool,
            "luks-mount" => Self::LuksMount,
            other => bail!(
                "unknown LUKS type {other:?} (expected raw, systemd, systemd-tool or luks-mount)"
            ),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Systemd => "systemd",
            Self::SystemdTool => "systemd-tool",
            Self::LuksMount => "luks-mount",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum IpFamily {
    #[default]
    Any,
    V4,
    V6,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum KnownHostsKind {
    #[default]
    Default,
    Initrd,
}

#[derive(Clone, Debug)]
pub struct Jumphost {
    pub host: String,
    pub user: String,
    pub port: u16,
    pub key: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct Healthcheck {
    pub port: Option<u16>,
    pub command: Option<String>,
    pub host: Option<String>,
    pub user: Option<String>,
    pub known_hosts: KnownHostsKind,
}

#[derive(Clone, Debug, Default)]
pub struct InitrdCheck {
    pub file: Option<PathBuf>,
    pub dir: Option<PathBuf>,
    pub script: PathBuf,
    pub busybox_dir: Option<String>,
    pub paranoid: bool,
    pub require_signature: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Notifications {
    pub events_file: Option<PathBuf>,
    pub apprise_url: Option<String>,
    pub apprise_tag: Option<String>,
    pub apprise_title: Option<String>,
    pub email_recipient: Option<String>,
    pub email_from: Option<String>,
    pub email_subject: Option<String>,
    pub msmtp_account: Option<String>,
}

/// Fully resolved configuration of one target.
#[derive(Clone, Debug)]
pub struct Settings {
    pub hostname: String,
    pub connect_address: Option<String>,
    pub hostkey_alias: Option<String>,
    pub port: u16,
    pub username: String,
    pub key: PathBuf,
    pub connect_timeout: u64,
    pub known_hosts: Option<String>,
    pub known_hosts_file: Option<PathBuf>,
    pub initrd_known_hosts: Option<String>,
    pub initrd_known_hosts_file: Option<PathBuf>,
    pub ip_family: IpFamily,
    pub skip_port_check: bool,
    pub jumphost: Option<Jumphost>,
    pub luks_type: LuksType,
    pub passphrase: Option<String>,
    pub passphrase_file: Option<PathBuf>,
    pub debug: bool,
    pub once: bool,
    pub force: bool,
    pub interval: u64,
    pub tick_timeout: u64,
    pub tmpdir: PathBuf,
    pub healthcheck: Healthcheck,
    pub initrd: InitrdCheck,
    pub notify: Notifications,
}

/// Values that mean "off" for boolean environment variables. The shell
/// implementation treated every non-empty value as true, so `FORCE_IPV4=0`
/// enabled IPv4-only mode.
pub fn truthy(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

fn readable(path: &Path) -> bool {
    fs::File::open(path).is_ok()
}

/// The initrd-checksum helper shipped next to this binary, else from PATH.
fn default_checksum_script() -> PathBuf {
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
    {
        for name in ["initrd-checksum", "initrd-checksum.sh"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("initrd-checksum")
}

impl Settings {
    /// Merge command-line options with an environment (the process
    /// environment, or the contents of a systemd environment file).
    pub fn resolve(opts: &GlobalOpts, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let val = |cli: &Option<String>, key: &str| -> Option<String> {
            cli.clone()
                .or_else(|| env(key))
                .filter(|v| !v.trim().is_empty())
        };
        let flag = |cli: bool, key: &str| cli || env(key).is_some_and(|v| truthy(&v));
        let num = |cli: &Option<String>, key: &str, default: u64| -> Result<u64> {
            match val(cli, key) {
                Some(v) => v
                    .trim()
                    .parse()
                    .with_context(|| format!("{key}: {v:?} is not a number")),
                None => Ok(default),
            }
        };
        let port = |cli: &Option<String>, key: &str| -> Result<Option<u16>> {
            val(cli, key)
                .map(|v| {
                    v.trim()
                        .parse::<u16>()
                        .with_context(|| format!("{key}: {v:?} is not a valid port"))
                })
                .transpose()
        };

        let Some(hostname) = val(&opts.host, "SSH_HOSTNAME") else {
            bail!("no target host: pass --host or set SSH_HOSTNAME");
        };
        let ssh_port = port(&opts.port, "SSH_PORT")?.unwrap_or(22);
        let key =
            PathBuf::from(val(&opts.ssh_key, "SSH_KEY").unwrap_or("/run/secrets/ssh_key".into()));

        let ip_family = match (
            flag(opts.force_ipv4, "FORCE_IPV4"),
            flag(opts.force_ipv6, "FORCE_IPV6"),
        ) {
            (true, true) => bail!("FORCE_IPV4 and FORCE_IPV6 are mutually exclusive"),
            (true, false) => IpFamily::V4,
            (false, true) => IpFamily::V6,
            (false, false) => IpFamily::Any,
        };

        let jumphost = match val(&opts.jumphost, "SSH_JUMPHOST") {
            Some(host) => Some(Jumphost {
                host,
                user: val(&opts.jumphost_username, "SSH_JUMPHOST_USERNAME")
                    .unwrap_or("root".into()),
                port: port(&opts.jumphost_port, "SSH_JUMPHOST_PORT")?.unwrap_or(ssh_port),
                key: val(&opts.jumphost_key, "SSH_JUMPHOST_KEY")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| key.clone()),
            }),
            None => None,
        };

        let luks_type =
            LuksType::parse(&val(&opts.luks_type, "LUKS_TYPE").unwrap_or("raw".into()))?;

        // An unset LUKS_PASSPHRASE_FILE defaults to the per-host secret path,
        // an empty one disables the file. A systemd credential takes
        // precedence over the environment, but not over the command line.
        let mut passphrase_file = match (&opts.luks_passphrase_file, env("LUKS_PASSPHRASE_FILE")) {
            (Some(cli), _) => Some(PathBuf::from(cli)),
            (None, Some(v)) if v.trim().is_empty() => None,
            (None, Some(v)) => Some(PathBuf::from(v)),
            (None, None) => Some(PathBuf::from(format!(
                "/run/secrets/luks_password_{hostname}"
            ))),
        };
        if opts.luks_passphrase_file.is_none()
            && let Some(dir) = env("CREDENTIALS_DIRECTORY").filter(|d| !d.is_empty())
        {
            let credential = Path::new(&dir).join("luks-passphrase");
            if readable(&credential) {
                passphrase_file = Some(credential);
            }
        }

        let healthcheck_kh = match val(
            &opts.healthcheck_known_hosts_type,
            "SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE",
        )
        .as_deref()
        {
            None | Some("default") => KnownHostsKind::Default,
            Some("initrd") => KnownHostsKind::Initrd,
            Some(other) => {
                bail!("SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE: {other:?} is neither default nor initrd")
            }
        };

        let checksum_dir = val(&opts.initrd_checksum_dir, "INITRD_CHECKSUM_DIR")
            .filter(|d| d != "/dev/null")
            .map(PathBuf::from);

        let once = flag(opts.once, "RUN_ONCE");
        let force = flag(opts.force, "FORCE");

        Ok(Self {
            connect_address: env("SSH_CONNECT_ADDRESS").filter(|v| !v.is_empty()),
            hostkey_alias: env("SSH_HOSTKEY_ALIAS").filter(|v| !v.is_empty()),
            port: ssh_port,
            username: val(&opts.username, "SSH_USERNAME").unwrap_or("root".into()),
            key,
            connect_timeout: num(&opts.connect_timeout, "SSH_CONNECTION_TIMEOUT", 5)?,
            known_hosts: val(&opts.ssh_known_hosts, "SSH_KNOWN_HOSTS"),
            known_hosts_file: val(&opts.ssh_known_hosts_file, "SSH_KNOWN_HOSTS_FILE")
                .map(PathBuf::from),
            initrd_known_hosts: val(&opts.ssh_initrd_known_hosts, "SSH_INITRD_KNOWN_HOSTS"),
            initrd_known_hosts_file: val(
                &opts.ssh_initrd_known_hosts_file,
                "SSH_INITRD_KNOWN_HOSTS_FILE",
            )
            .map(PathBuf::from),
            ip_family,
            skip_port_check: flag(opts.skip_ssh_port_check, "SKIP_SSH_PORT_CHECK"),
            jumphost,
            luks_type,
            passphrase: val(&opts.luks_passphrase, "LUKS_PASSPHRASE"),
            passphrase_file,
            debug: flag(opts.debug, "DEBUG"),
            once,
            force,
            interval: num(&opts.interval, "SLEEP_INTERVAL", 10)?.max(1),
            tick_timeout: num(&opts.tick_timeout, "TICK_TIMEOUT", 120)?.max(5),
            tmpdir: PathBuf::from(
                env("TMPDIR")
                    .filter(|v| !v.is_empty())
                    .unwrap_or("/tmp".into()),
            ),
            healthcheck: Healthcheck {
                port: port(&opts.healthcheck_port, "HEALTHCHECK_PORT")?,
                command: val(&opts.healthcheck_cmd, "HEALTHCHECK_REMOTE_CMD"),
                host: val(&opts.healthcheck_host, "HEALTHCHECK_REMOTE_HOSTNAME"),
                user: val(&opts.healthcheck_user, "HEALTHCHECK_REMOTE_USERNAME"),
                known_hosts: healthcheck_kh,
            },
            initrd: InitrdCheck {
                file: val(&opts.initrd_checksum_file, "INITRD_CHECKSUM_FILE").map(PathBuf::from),
                dir: checksum_dir,
                script: val(&opts.initrd_checksum_script, "INITRD_CHECKSUM_SCRIPT")
                    .map(PathBuf::from)
                    .unwrap_or_else(default_checksum_script),
                busybox_dir: env("INITRD_CHECKSUM_BUSYBOX_DIR").filter(|v| !v.is_empty()),
                paranoid: flag(opts.paranoid, "PARANOID"),
                require_signature: flag(
                    opts.require_signature,
                    "INITRD_CHECKSUM_REQUIRE_SIGNATURE",
                ),
            },
            notify: Notifications {
                events_file: val(&opts.events_file, "EVENTS_FILE").map(PathBuf::from),
                apprise_url: val(&opts.apprise_url, "APPRISE_URL"),
                apprise_tag: val(&opts.apprise_tag, "APPRISE_TAG"),
                apprise_title: val(&opts.apprise_title, "APPRISE_TITLE"),
                email_recipient: val(&opts.email_recipient, "EMAIL_RECIPIENT"),
                email_from: val(&opts.email_from, "EMAIL_FROM"),
                email_subject: val(&opts.email_subject, "EMAIL_SUBJECT"),
                msmtp_account: val(&opts.msmtp_email_account, "MSMTP_ACCOUNT"),
            },
            hostname,
        })
    }

    /// `user@host:port [via jumphost]`, for messages.
    pub fn route(&self) -> String {
        let mut route = format!("{}@{}:{}", self.username, self.hostname, self.port);
        if let Some(jump) = &self.jumphost {
            route.push_str(" via ");
            route.push_str(&jump.host);
        }
        route
    }

    pub fn has_regular_host_keys(&self) -> bool {
        self.known_hosts.is_some() || self.known_hosts_file.is_some()
    }

    pub fn has_initrd_host_keys(&self) -> bool {
        self.initrd_known_hosts.is_some() || self.initrd_known_hosts_file.is_some()
    }
}

/// Parse a systemd `EnvironmentFile=`: `KEY=value` lines, `#`/`;` comments,
/// single quotes (literal) and double quotes (backslash escapes).
pub fn parse_env_file(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        map.insert(key.to_string(), unquote(value.trim()));
    }
    map
}

fn unquote(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    out.push(c);
                }
            }
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some('n') => out.push('\n'),
                            Some('t') => out.push('\t'),
                            Some(other) => out.push(other),
                            None => {}
                        },
                        other => out.push(other),
                    }
                }
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(pairs: &[(&str, &str)]) -> Result<Settings> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Settings::resolve(&GlobalOpts::default(), &|k| map.get(k).cloned())
    }

    #[test]
    fn env_file_quoting() {
        let map = parse_env_file(
            "# comment\nA=plain\n  B='single $x \"q\"'\nC=\"dq \\\"x\\\" \\\\\"\nD=\nE = spaced\nnot a line\n",
        );
        assert_eq!(map["A"], "plain");
        assert_eq!(map["B"], "single $x \"q\"");
        assert_eq!(map["C"], "dq \"x\" \\");
        assert_eq!(map["D"], "");
        assert_eq!(map["E"], "spaced");
        assert_eq!(map.len(), 5);
    }

    #[test]
    fn defaults() {
        let s = resolve(&[("SSH_HOSTNAME", "target.example")]).unwrap();
        assert_eq!(s.port, 22);
        assert_eq!(s.username, "root");
        assert_eq!(s.luks_type, LuksType::Raw);
        assert_eq!(s.interval, 10);
        assert_eq!(
            s.passphrase_file.as_deref(),
            Some(Path::new("/run/secrets/luks_password_target.example"))
        );
        assert!(s.initrd.dir.is_none());
    }

    #[test]
    fn empty_passphrase_file_disables_default() {
        let s = resolve(&[("SSH_HOSTNAME", "h"), ("LUKS_PASSPHRASE_FILE", "")]).unwrap();
        assert!(s.passphrase_file.is_none());
    }

    #[test]
    fn jumphost_inherits_port_and_key() {
        let s = resolve(&[
            ("SSH_HOSTNAME", "h"),
            ("SSH_PORT", "2222"),
            ("SSH_KEY", "/k"),
            ("SSH_JUMPHOST", "bastion"),
        ])
        .unwrap();
        let j = s.jumphost.unwrap();
        assert_eq!(j.port, 2222);
        assert_eq!(j.key, PathBuf::from("/k"));
        assert_eq!(j.user, "root");
    }

    #[test]
    fn rejects_bad_values() {
        assert!(resolve(&[]).is_err());
        assert!(resolve(&[("SSH_HOSTNAME", "h"), ("LUKS_TYPE", "nope")]).is_err());
        assert!(resolve(&[("SSH_HOSTNAME", "h"), ("SSH_PORT", "70000")]).is_err());
        assert!(
            resolve(&[
                ("SSH_HOSTNAME", "h"),
                ("FORCE_IPV4", "1"),
                ("FORCE_IPV6", "1")
            ])
            .is_err()
        );
    }

    #[test]
    fn falsy_flags() {
        let s = resolve(&[("SSH_HOSTNAME", "h"), ("FORCE_IPV4", "0"), ("DEBUG", "")]).unwrap();
        assert_eq!(s.ip_family, IpFamily::Any);
        assert!(!s.debug);
    }

    #[test]
    fn dev_null_checksum_dir_is_disabled() {
        let s = resolve(&[("SSH_HOSTNAME", "h"), ("INITRD_CHECKSUM_DIR", "/dev/null")]).unwrap();
        assert!(s.initrd.dir.is_none());
    }
}
