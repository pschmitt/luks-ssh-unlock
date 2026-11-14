//! Typed configuration for a target's console.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::tui::ExtraCmd;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Console {
    Command {
        parameters: CommandParameters,
    },
    Oci {
        parameters: OciParameters,
    },
    Openstack {
        parameters: OpenstackSerialParameters,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandParameters {
    command: String,
    #[serde(default)]
    interactive: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OciParameters {
    instance: Option<String>,
    identity: Option<String>,
    config_file: Option<String>,
    cli_rc_file: Option<String>,
    profile: Option<String>,
    compartment_id: Option<String>,
    #[serde(default)]
    keep: bool,
    /// Close the instance's other console connections when they stand in the
    /// way (OCI allows one per instance). On unless set to false.
    close_others: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpenstackSerialParameters {
    /// Server name or ID; the target's name when absent.
    server: Option<String>,
    cloud: Option<String>,
    clouds_file: Option<String>,
    ca_file: Option<String>,
    #[serde(default)]
    allow_insecure: bool,
    openstack_bin: Option<String>,
    /// `serial`, `log` (the polled console log) or `auto` (the serial console,
    /// the log when it is not available; the default).
    mode: Option<LogMode>,
}

/// What an OpenStack console view shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum LogMode {
    /// The serial console, else the console log.
    #[default]
    Auto,
    /// The serial console only.
    Serial,
    /// The console log only, polled.
    Log,
}

/// A sidecar's optional top-level `"default": true` (show this view first),
/// taken out so that the strict typed parsing below never sees it.
fn take_default(data: &[u8]) -> Result<(Vec<u8>, bool)> {
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_slice(data) else {
        // Not an object: let the typed parser say what is wrong.
        return Ok((data.to_vec(), false));
    };
    let default = match map.remove("default") {
        None => false,
        Some(serde_json::Value::Bool(b)) => b,
        Some(_) => bail!("\"default\" must be true or false"),
    };
    Ok((serde_json::to_vec(&map)?, default))
}

/// A view read from a sidecar, and whether it is the target's default view.
pub type Sidecar = Option<(ExtraCmd, bool)>;

pub fn read(path: &Path, target: &str) -> Result<Sidecar> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let (data, default) = take_default(&data)
        .with_context(|| format!("invalid console configuration in {}", path.display()))?;
    let config: Console = serde_json::from_slice(&data)
        .with_context(|| format!("invalid console configuration in {}", path.display()))?;
    let (command, interactive) = match config {
        Console::Command { parameters } => (parameters.command, parameters.interactive),
        Console::Oci { parameters } => {
            let binary = std::env::current_exe().context("cannot find the LSU executable")?;
            let mut args = vec![
                binary.to_string_lossy().into_owned(),
                "console".into(),
                "oci".into(),
                "--instance".into(),
                parameters.instance.unwrap_or_else(|| target.to_string()),
            ];
            for (flag, value) in [
                ("--identity", parameters.identity),
                ("--config-file", parameters.config_file),
                ("--cli-rc-file", parameters.cli_rc_file),
                ("--profile", parameters.profile),
                ("--compartment-id", parameters.compartment_id),
            ] {
                if let Some(value) = value {
                    args.extend([flag.to_string(), value]);
                }
            }
            if parameters.keep {
                args.push("--keep".into());
            }
            if parameters.close_others == Some(false) {
                args.push("--keep-others".into());
            }
            (
                args.iter()
                    .map(|s| {
                        crate::process::shell_quote(s)
                            .replace("{host}", "{'\"\"'host}")
                            .replace("{name}", "{'\"\"'name}")
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                true,
            )
        }
        Console::Openstack { parameters } => {
            let console = OpenStackConsole::from_parameters(
                parameters.server.unwrap_or_else(|| target.to_string()),
                parameters.cloud,
                parameters.clouds_file,
                parameters.ca_file,
                parameters.allow_insecure,
                parameters.openstack_bin,
            )?;
            let binary = std::env::current_exe().context("cannot find the LSU executable")?;
            let mut args = vec![
                binary.to_string_lossy().into_owned(),
                "console".into(),
                "openstack".into(),
                "--server".into(),
                console.server.clone(),
            ];
            for (flag, value) in [
                ("--cloud", console.cloud.clone()),
                (
                    "--clouds-file",
                    console
                        .clouds_file
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                ),
                (
                    "--ca-file",
                    console
                        .ca_file
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                ),
                (
                    "--openstack-bin",
                    console
                        .openstack_bin
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned()),
                ),
            ] {
                if let Some(value) = value {
                    args.extend([flag.to_string(), value]);
                }
            }
            if console.allow_insecure {
                args.push("--allow-insecure".into());
            }
            match parameters.mode.unwrap_or_default() {
                LogMode::Auto => {}
                LogMode::Serial => args.extend(["--mode".into(), "serial".into()]),
                LogMode::Log => args.extend(["--mode".into(), "log".into()]),
            }
            (
                args.iter()
                    .map(|s| {
                        crate::process::shell_quote(s)
                            .replace("{host}", "{'\"\"'host}")
                            .replace("{name}", "{'\"\"'name}")
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                true,
            )
        }
    };
    if command.trim().is_empty() {
        bail!("console command cannot be empty");
    }
    Ok(Some((
        ExtraCmd {
            label: "console".into(),
            command,
            interactive,
            vnc: None,
        },
        default,
    )))
}

/// Label of the VNC pane in the dashboard.
pub const VNC_LABEL: &str = "vnc";
/// Text standing in for the command of a VNC pane: it runs no command.
pub const VNC_COMMAND: &str = "(built-in VNC viewer)";
/// The default RFB port.
const VNC_PORT: u16 = 5900;
/// A VNC password file is a few bytes; refuse anything larger.
const PASSWORD_FILE_MAX: u64 = 4096;

/// Where a graphical console comes from, and how the viewer's settings apply
/// to it. The settings are the same for every source; what differs is how the
/// RFB byte stream is obtained.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VncEndpoint {
    pub source: VncSource,
    /// A file holding the password; it is read when the session starts and
    /// never appears in a command line, a message or the configuration.
    pub password_file: Option<PathBuf>,
    /// Start without forwarding the keyboard and mouse (`Ctrl+T` toggles).
    pub view_only: bool,
    /// Tight JPEG quality, 0 to 9.
    pub quality: Option<u8>,
    /// Tight compression level, 0 to 9.
    pub compression: Option<u8>,
    /// Ask the remote desktop to take the size of the pane.
    pub resize_remote: bool,
    /// The guest's keyboard layout; the local console's when not set.
    pub keymap: Option<crate::vnc::keymap::Layout>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VncSource {
    /// A host and port that speak plain RFB.
    Direct(DirectVnc),
    /// The Nova noVNC console of a server, reached with a fresh single-use
    /// token each time the session starts.
    OpenStack(OpenStackConsole),
    /// The graphical console of an OCI instance, reached through a private
    /// Unix socket that an `ssh` tunnel listens on.
    Oci(OciConsole),
}

/// A direct VNC endpoint, independent of any cloud provider: a host and port
/// that speak plain RFB.
///
/// RFB is not encrypted and its password authentication is weak, so the
/// endpoint must be a loopback address (an SSH tunnel or a local proxy)
/// unless `allow_remote` was set on purpose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectVnc {
    pub host: String,
    pub port: u16,
    /// The endpoint is not a loopback address, and says so.
    pub allow_remote: bool,
}

/// An OpenStack server whose console the dashboard opens natively: the
/// `openstack` client yields a short-lived console URL, and the WebSocket to
/// the console proxy named by it carries the bytes. Nothing listens locally
/// and the URL (a bearer token) is never written anywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenStackConsole {
    /// Server name or ID (not a secret).
    pub server: String,
    /// An entry of `clouds.yaml`; the `OS_*` variables apply without one.
    pub cloud: Option<String>,
    pub clouds_file: Option<PathBuf>,
    /// Extra CA certificates for a console proxy with a private CA.
    pub ca_file: Option<PathBuf>,
    /// Accept a plain `ws://` console proxy that is not on loopback.
    pub allow_insecure: bool,
    /// The `openstack` client to run (PATH by default).
    pub openstack_bin: Option<PathBuf>,
}

impl OpenStackConsole {
    /// Checked settings from the configuration: absolute paths, a server and
    /// cloud that cannot be taken for command-line options.
    fn from_parameters(
        server: String,
        cloud: Option<String>,
        clouds_file: Option<String>,
        ca_file: Option<String>,
        allow_insecure: bool,
        openstack_bin: Option<String>,
    ) -> Result<Self> {
        let path = |what: &str, v: Option<String>| -> Result<Option<PathBuf>> {
            v.map(|v| {
                let p = PathBuf::from(&v);
                if v.is_empty() || v.contains('\0') || !p.is_absolute() {
                    bail!("{what} must be an absolute path");
                }
                Ok(p)
            })
            .transpose()
        };
        let console = Self {
            server,
            cloud,
            clouds_file: path("cloudsFile", clouds_file)?,
            ca_file: path("caFile", ca_file)?,
            allow_insecure,
            openstack_bin: path("openstackBin", openstack_bin)?,
        };
        let mut check = crate::openstack::Options::new(console.server.clone());
        check.cloud.clone_from(&console.cloud);
        check.validate().map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(console)
    }

    /// Options for the OpenStack module. The `openstack` client runs as the
    /// user who started sudo when the dashboard runs as root through it.
    pub fn options(&self) -> crate::openstack::Options {
        let mut o = crate::openstack::Options::new(self.server.clone());
        o.cloud.clone_from(&self.cloud);
        o.clouds_file.clone_from(&self.clouds_file);
        o.ca_file.clone_from(&self.ca_file);
        o.allow_insecure = self.allow_insecure;
        o.openstack_bin.clone_from(&self.openstack_bin);
        o.run_as = crate::openstack::RunAs::from_sudo_env();
        o
    }
}

/// An OCI instance whose graphical console the dashboard opens natively.
/// The module creates a console connection, tunnels the VNC port into a
/// private socket (mode 0600, in a 0700 directory) and deletes the connection
/// again; no TCP port is opened on this machine and no key or token appears
/// in a command line. OCI serves one console client at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OciConsole {
    /// Instance display name or OCID (not a secret).
    pub instance: String,
    /// RSA private key (`<identity>.pub` must exist); `~/.ssh/id_rsa` when absent.
    pub identity: Option<PathBuf>,
    pub config_file: Option<PathBuf>,
    pub cli_rc_file: Option<PathBuf>,
    pub profile: Option<String>,
    pub compartment_id: Option<String>,
    /// Keep the console connection on exit instead of deleting it.
    pub keep: bool,
    /// Leave the instance's other console connections alone.
    pub keep_others: bool,
}

impl OciConsole {
    /// Checked settings from the configuration: absolute paths, names that
    /// cannot be taken for command-line options.
    fn from_parameters(p: OciVncSettings) -> Result<Self> {
        let path = |what: &str, v: Option<String>, tilde: bool| -> Result<Option<PathBuf>> {
            v.map(|v| {
                let ok = !v.is_empty()
                    && !v.contains('\0')
                    && (Path::new(&v).is_absolute() || (tilde && v.starts_with("~/")));
                if !ok {
                    bail!("{what} must be an absolute path");
                }
                Ok(PathBuf::from(v))
            })
            .transpose()
        };
        let name = |what: &str, v: &str| -> Result<()> {
            if v.is_empty() || v.starts_with('-') || v.chars().any(char::is_control) {
                bail!("{what} must not be empty, start with \"-\" or contain control characters");
            }
            Ok(())
        };
        name("instance", &p.instance)?;
        for (what, v) in [
            ("profile", &p.profile),
            ("compartmentId", &p.compartment_id),
        ] {
            if let Some(v) = v {
                name(what, v)?;
            }
        }
        let identity = path("identity", p.identity, true)?;
        if identity
            .as_ref()
            .is_some_and(|i| i.starts_with("/nix/store"))
        {
            bail!("identity must not be in the Nix store (it is world-readable)");
        }
        Ok(Self {
            identity,
            config_file: path("configFile", p.config_file, false)?,
            cli_rc_file: path("cliRcFile", p.cli_rc_file, false)?,
            instance: p.instance,
            profile: p.profile,
            compartment_id: p.compartment_id,
            keep: p.keep,
            keep_others: p.keep_others,
        })
    }

    /// Options for the OCI module.
    pub fn options(&self) -> crate::oci::vnc::VncOptions {
        crate::oci::vnc::VncOptions {
            oci: crate::oci::Options {
                instance: self.instance.clone(),
                identity: self.identity.clone(),
                config_file: self.config_file.clone(),
                cli_rc_file: self.cli_rc_file.clone(),
                profile: self.profile.clone(),
                compartment_id: self.compartment_id.clone(),
                keep: self.keep,
                keep_others: self.keep_others,
                ..Default::default()
            },
            runtime_dir: None,
        }
    }
}

/// The checked-but-unparsed OCI settings of the VNC configuration.
struct OciVncSettings {
    instance: String,
    identity: Option<String>,
    config_file: Option<String>,
    cli_rc_file: Option<String>,
    profile: Option<String>,
    compartment_id: Option<String>,
    keep: bool,
    keep_others: bool,
}

impl VncEndpoint {
    /// Is the connection one that leaves the machine unencrypted? For a
    /// direct endpoint that is a non-loopback host; for OpenStack, only a
    /// console proxy without TLS is accepted when `allow_insecure` is set,
    /// and without it the module refuses one.
    pub fn unencrypted_remote(&self) -> bool {
        match &self.source {
            VncSource::Direct(d) => !d.is_local(),
            VncSource::OpenStack(o) => o.allow_insecure,
            // A private Unix socket, then SSH through the provider's bastion.
            VncSource::Oci(_) => false,
        }
    }

    /// What the pane says while it starts. Never a URL or a token.
    pub fn first_step(&self) -> String {
        match &self.source {
            VncSource::Direct(d) => format!("connecting to {}", d.address()),
            VncSource::OpenStack(_) => "asking OpenStack for a console URL".into(),
            VncSource::Oci(_) => {
                "requesting an OCI console connection (this can take a minute)".into()
            }
        }
    }
}

impl DirectVnc {
    /// Is the endpoint reachable without leaving this machine?
    pub fn is_local(&self) -> bool {
        host_is_loopback(&self.host)
    }

    /// `host:port`, with an IPv6 literal in brackets.
    pub fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The address to connect to. Without `allow_remote` the host is
    /// resolved here and every answer has to be a loopback address, so that a
    /// name which stops pointing at this machine is refused instead of
    /// followed; the connection then goes to the resolved address, not to
    /// the name again.
    pub async fn resolve(&self) -> Result<String> {
        if self.allow_remote && !self.is_local() {
            return Ok(self.address());
        }
        let mut found = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .map_err(|e| anyhow::anyhow!("cannot resolve the VNC host: {e}"))?
            .collect::<Vec<SocketAddr>>();
        if found.is_empty() || found.iter().any(|a| !a.ip().is_loopback()) {
            bail!(
                "the VNC host does not resolve to a loopback address (set allowRemote only if an unencrypted connection is acceptable)"
            );
        }
        // IPv4 first: a tunnel is usually bound to 127.0.0.1.
        found.sort_by_key(|a| a.is_ipv6());
        Ok(found[0].to_string())
    }
}

fn host_is_loopback(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// A host name or an IP literal, nothing else: no scheme, path, credentials,
/// port or query, which is how a provider URL would sneak in.
fn valid_host(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum VncConfig {
    Vnc { parameters: VncParameters },
    Openstack { parameters: OpenstackVncParameters },
    Oci { parameters: OciVncParameters },
}

/// Settings every graphical source shares.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VncParameters {
    host: Option<String>,
    port: Option<u32>,
    password_file: Option<String>,
    #[serde(default)]
    view_only: bool,
    quality: Option<u8>,
    compression: Option<u8>,
    #[serde(default)]
    resize_remote: bool,
    keymap: Option<String>,
    #[serde(default)]
    allow_remote: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OpenstackVncParameters {
    /// Server name or ID; the target's name when absent.
    server: Option<String>,
    cloud: Option<String>,
    clouds_file: Option<String>,
    ca_file: Option<String>,
    #[serde(default)]
    allow_insecure: bool,
    openstack_bin: Option<String>,
    password_file: Option<String>,
    #[serde(default)]
    view_only: bool,
    quality: Option<u8>,
    compression: Option<u8>,
    #[serde(default)]
    resize_remote: bool,
    keymap: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OciVncParameters {
    /// Instance display name or OCID; the target's name when absent.
    instance: Option<String>,
    identity: Option<String>,
    config_file: Option<String>,
    cli_rc_file: Option<String>,
    profile: Option<String>,
    compartment_id: Option<String>,
    #[serde(default)]
    keep: bool,
    close_others: Option<bool>,
    password_file: Option<String>,
    #[serde(default)]
    view_only: bool,
    quality: Option<u8>,
    compression: Option<u8>,
    #[serde(default)]
    resize_remote: bool,
    keymap: Option<String>,
}

/// The target's graphical console, from `<name>.vnc.json`:
/// `{"type": "vnc", "parameters": {"host": "127.0.0.1", "port": 5900, ...}}`
/// for a direct endpoint, or `{"type": "openstack", "parameters": {"cloud":
/// "...", ...}}` for the noVNC console of an OpenStack server (named like the
/// target unless `server` says otherwise), or `{"type": "oci", "parameters":
/// {"profile": "...", ...}}` for the graphical console of an OCI instance
/// (likewise named like the target unless `instance` says otherwise). `None` when there is no such file.
/// It lives next to, and independent of, the `console.json` (serial console),
/// so a target can offer both.
pub fn read_vnc(path: &Path, target: &str) -> Result<Sidecar> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let (data, default) = take_default(&data)
        .with_context(|| format!("invalid VNC configuration in {}", path.display()))?;
    let endpoint = parse_vnc(&data, target)
        .with_context(|| format!("invalid VNC configuration in {}", path.display()))?;
    Ok(Some((
        ExtraCmd {
            label: VNC_LABEL.into(),
            command: VNC_COMMAND.into(),
            interactive: false,
            vnc: Some(endpoint),
        },
        default,
    )))
}

fn check_vnc_numbers(quality: Option<u8>, compression: Option<u8>) -> Result<()> {
    for (what, v) in [("quality", quality), ("compression", compression)] {
        if v.is_some_and(|v| v > 9) {
            bail!("{what} must be between 0 and 9");
        }
    }
    Ok(())
}

fn check_keymap(name: Option<String>) -> Result<Option<crate::vnc::keymap::Layout>> {
    let Some(name) = name else { return Ok(None) };
    crate::vnc::keymap::Layout::parse(&name)
        .map(Some)
        .with_context(|| format!("unsupported keymap \"{name}\": use \"de\" or \"us\""))
}

fn check_password_file(file: Option<String>) -> Result<Option<PathBuf>> {
    let Some(f) = file else { return Ok(None) };
    let path = PathBuf::from(&f);
    if f.is_empty() || f.contains('\0') || !path.is_absolute() {
        bail!("passwordFile must be an absolute path");
    }
    if path.starts_with("/nix/store") {
        bail!("passwordFile must not be in the Nix store (it is world-readable)");
    }
    Ok(Some(path))
}

fn parse_vnc(data: &[u8], target: &str) -> Result<VncEndpoint> {
    // Say what is not supported instead of what serde cannot match.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(data)
        && let Some(kind) = v.get("type").and_then(|t| t.as_str())
        && !matches!(kind, "vnc" | "openstack" | "oci")
    {
        bail!(
            "unsupported VNC type \"{kind}\": only a direct VNC endpoint (type \"vnc\", host and port), an OpenStack server (type \"openstack\") and an OCI instance (type \"oci\") are supported"
        );
    }
    match serde_json::from_slice(data)? {
        VncConfig::Vnc { parameters: p } => {
            let host = p.host.unwrap_or_else(|| Ipv4Addr::LOCALHOST.to_string());
            if !valid_host(&host) {
                bail!("host must be a host name or an IP address, not a URL");
            }
            let port = match p.port {
                None => VNC_PORT,
                Some(n) => u16::try_from(n)
                    .ok()
                    .filter(|n| *n != 0)
                    .context("port must be between 1 and 65535")?,
            };
            check_vnc_numbers(p.quality, p.compression)?;
            let direct = DirectVnc {
                host,
                port,
                allow_remote: p.allow_remote,
            };
            if !direct.is_local() && !direct.allow_remote {
                bail!(
                    "VNC is not encrypted: the host must be a loopback address (reach a remote desktop through an SSH tunnel), or set allowRemote to accept an unencrypted connection"
                );
            }
            Ok(VncEndpoint {
                source: VncSource::Direct(direct),
                password_file: check_password_file(p.password_file)?,
                view_only: p.view_only,
                quality: p.quality,
                compression: p.compression,
                resize_remote: p.resize_remote,
                keymap: check_keymap(p.keymap)?,
            })
        }
        VncConfig::Openstack { parameters: p } => {
            check_vnc_numbers(p.quality, p.compression)?;
            let console = OpenStackConsole::from_parameters(
                p.server.unwrap_or_else(|| target.to_string()),
                p.cloud,
                p.clouds_file,
                p.ca_file,
                p.allow_insecure,
                p.openstack_bin,
            )?;
            Ok(VncEndpoint {
                source: VncSource::OpenStack(console),
                password_file: check_password_file(p.password_file)?,
                view_only: p.view_only,
                quality: p.quality,
                compression: p.compression,
                resize_remote: p.resize_remote,
                keymap: check_keymap(p.keymap)?,
            })
        }
        VncConfig::Oci { parameters: p } => {
            check_vnc_numbers(p.quality, p.compression)?;
            let console = OciConsole::from_parameters(OciVncSettings {
                instance: p.instance.unwrap_or_else(|| target.to_string()),
                identity: p.identity,
                config_file: p.config_file,
                cli_rc_file: p.cli_rc_file,
                profile: p.profile,
                compartment_id: p.compartment_id,
                keep: p.keep,
                keep_others: p.close_others == Some(false),
            })?;
            Ok(VncEndpoint {
                source: VncSource::Oci(console),
                password_file: check_password_file(p.password_file)?,
                view_only: p.view_only,
                quality: p.quality,
                compression: p.compression,
                resize_remote: p.resize_remote,
                keymap: check_keymap(p.keymap)?,
            })
        }
    }
}

/// The password of an endpoint, read from its password file (`None` when
/// the endpoint has none). The file must be a regular file, not a symbolic
/// link, readable by nobody but its owner, and owned by the user running
/// this, root, or (when run through sudo) the user who started sudo. Only
/// the first line is used, and it never appears in an error.
pub fn read_vnc_password(endpoint: &VncEndpoint) -> Result<Option<zeroize::Zeroizing<String>>> {
    let Some(path) = &endpoint.password_file else {
        return Ok(None);
    };
    read_password_file(path).map(Some)
}

fn read_password_file(path: &Path) -> Result<zeroize::Zeroizing<String>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("VNC password file {} is not readable", path.display()))?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("VNC password file {} is not a regular file", path.display());
    }
    if meta.mode() & 0o077 != 0 {
        bail!(
            "VNC password file {} is accessible to other users (use mode 0600)",
            path.display()
        );
    }
    let euid = unsafe { libc::geteuid() };
    let sudo_uid = (euid == 0)
        .then(|| std::env::var("SUDO_UID").ok()?.parse::<u32>().ok())
        .flatten();
    if meta.uid() != euid && meta.uid() != 0 && Some(meta.uid()) != sudo_uid {
        bail!(
            "VNC password file {} belongs to another user",
            path.display()
        );
    }
    if meta.len() > PASSWORD_FILE_MAX {
        bail!("VNC password file {} is too large", path.display());
    }
    let mut buf = zeroize::Zeroizing::new(Vec::with_capacity(meta.len() as usize));
    (&mut file)
        .take(PASSWORD_FILE_MAX + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("cannot read VNC password file {}", path.display()))?;
    let text = std::str::from_utf8(&buf)
        .map_err(|_| anyhow::anyhow!("VNC password file {} is not text", path.display()))?;
    let first = text.lines().next().unwrap_or_default();
    if first.is_empty() {
        bail!("VNC password file {} is empty", path.display());
    }
    Ok(zeroize::Zeroizing::new(first.to_string()))
}

// ------------------------------------------------------- OpenStack serial

/// `console openstack`: the serial console of an OpenStack server on this
/// terminal. The dashboard runs it in an interactive pane, like the OCI one.
#[derive(clap::Args, Debug)]
pub struct OpenstackSerialArgs {
    /// Server name or ID
    #[arg(long)]
    pub server: String,
    /// Entry of clouds.yaml (the OS_* variables apply otherwise)
    #[arg(long)]
    pub cloud: Option<String>,
    /// clouds.yaml to use (sets OS_CLIENT_CONFIG_FILE for the client)
    #[arg(long)]
    pub clouds_file: Option<PathBuf>,
    /// Extra PEM CA certificates for the console proxy
    #[arg(long)]
    pub ca_file: Option<PathBuf>,
    /// Accept a plain ws:// console proxy that is not on loopback
    #[arg(long)]
    pub allow_insecure: bool,
    /// The openstack client to run [default: openstack from PATH]
    #[arg(long)]
    pub openstack_bin: Option<PathBuf>,
    /// The serial console, the polled console log, or the serial console and
    /// the log when there is none
    #[arg(long, value_enum, default_value_t = LogMode::Auto)]
    pub mode: LogMode,
}

/// Ctrl+] ends the session (the dashboard takes it first, so this matters
/// only when run by hand).
const DETACH: u8 = 0x1d;

/// Puts the terminal on stdin into raw mode, and back at the end.
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enter() -> Self {
        // SAFETY: tcgetattr/tcsetattr work on fd 0 and an owned termios.
        unsafe {
            if libc::isatty(0) != 1 {
                return Self(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Self(None);
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return Self(None);
            }
            Self(Some(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            // SAFETY: restores the attributes read by `enter`.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
    }
}

/// Run the serial console until it closes or the user detaches. Returns the
/// exit status.
/// The console log comes with the ESC bytes of the guest's escape sequences
/// taken out, which leaves their visible rest (`[0;32m`, `[?7h`, `]104\`) in
/// the text. Remove what is left of control sequences (`[` parameters and a
/// final letter) and of OSC strings (`]` digits up to `\`); text such as
/// `[  OK  ]` or `[   1.234]` stays.
fn strip_escape_remnants(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'[' {
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_digit() || matches!(b[j], b';' | b'?' | b'!')) {
                j += 1;
            }
            // At least one parameter: `[Install]` is text, `[0J` is not.
            if j > i + 1 && j < b.len() && b[j].is_ascii_alphabetic() {
                i = j + 1;
                continue;
            }
        }
        if b[i] == b']'
            && b.get(i + 1).is_some_and(u8::is_ascii_digit)
            && let Some(end) = line[i..].find('\\')
        {
            let body = &line[i + 1..i + end];
            let digits = body.bytes().take_while(u8::is_ascii_digit).count();
            if body[digits..].is_empty() || body[digits..].starts_with(';') {
                i += end + 1;
                continue;
            }
        }
        let ch = line[i..].chars().next().expect("in bounds");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Show the server's console log, and what is added to it, until killed.
async fn stream_log(console: &OpenStackConsole) -> i32 {
    use std::io::Write;
    const LINES: u16 = 200;
    const EVERY: std::time::Duration = std::time::Duration::from_secs(3);
    let opts = console.options();
    let mut prev = String::new();
    let mut failures = 0;
    loop {
        match crate::openstack::fetch_log(&opts, LINES).await {
            Ok(text) => {
                if failures == 0 && prev.is_empty() {
                    crate::progress::ready();
                }
                failures = 0;
                let old: Vec<&str> = prev.lines().collect();
                let new: Vec<&str> = text.lines().collect();
                let mut out = std::io::stdout().lock();
                for line in crate::openstack::new_log_lines(&old, &new) {
                    // A raw terminal: lines need the carriage return too.
                    let _ = write!(out, "{}\r\n", strip_escape_remnants(line));
                }
                let _ = out.flush();
                prev = text;
            }
            Err(e) => {
                failures += 1;
                let _ = writeln!(std::io::stderr(), "console log: {e}\r");
                if failures >= 5 {
                    return 1;
                }
            }
        }
        tokio::time::sleep(EVERY).await;
    }
}

pub fn run_openstack_serial(args: &OpenstackSerialArgs) -> Result<i32> {
    use crate::openstack::{Kind, RelayEnd};
    use std::io::{Read, Write};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let console = OpenStackConsole::from_parameters(
        args.server.clone(),
        args.cloud.clone(),
        args.clouds_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        args.ca_file
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        args.allow_insecure,
        args.openstack_bin
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
    )?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("cannot start the runtime")?;
    let handle = runtime.handle().clone();
    if args.mode == LogMode::Log {
        crate::progress::step("", &format!("Console log of {}:", console.server));
    } else {
        crate::progress::step(
            "",
            &format!(
                "Connecting to the serial console of {} (Ctrl+] detaches)...",
                console.server
            ),
        );
    }
    let code = runtime.block_on(async {
        if args.mode == LogMode::Log {
            return stream_log(&console).await;
        }
        let opened = crate::openstack::open(&console.options(), Kind::Serial).await;
        let established = match opened {
            Ok(e) => e,
            Err(crate::openstack::Error::Cancelled) => return 1,
            Err(e) if args.mode == LogMode::Auto => {
                crate::progress::step(
                    "",
                    &format!("No serial console ({e}); showing the console log instead."),
                );
                return stream_log(&console).await;
            }
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "{e}");
                return 1;
            }
        };
        if !established.encrypted {
            let _ = writeln!(
                std::io::stderr(),
                "warning: the console connection is not encrypted"
            );
        }
        crate::progress::ready();
        let _ = writeln!(std::io::stderr(), "Connected. Press Enter to get a prompt.");
        // Terminal input and output go through threads: tokio's stdio needs
        // a feature that is not enabled, and a blocked read must not hold up
        // the end of the session.
        let (mut in_w, in_r) = tokio::io::duplex(1024);
        std::thread::spawn({
            let handle = handle.clone();
            move || {
                let mut buf = [0u8; 1024];
                let mut stdin = std::io::stdin();
                while let Ok(n) = stdin.read(&mut buf) {
                    if n == 0 || handle.block_on(in_w.write_all(&buf[..n])).is_err() {
                        break;
                    }
                }
            }
        });
        let (out_w, mut out_r) = tokio::io::duplex(8192);
        let writer = std::thread::spawn({
            let handle = handle.clone();
            move || {
                let mut buf = vec![0u8; 8192];
                let mut stdout = std::io::stdout();
                while let Ok(n) = handle.block_on(out_r.read(&mut buf)) {
                    if n == 0
                        || stdout
                            .write_all(&buf[..n])
                            .and_then(|()| stdout.flush())
                            .is_err()
                    {
                        break;
                    }
                }
            }
        });
        let raw = RawMode::enter();
        let end = crate::openstack::relay(established.stream, in_r, out_w, Some(DETACH)).await;
        drop(raw);
        // `relay` consumed the stream and the output half: the writer thread
        // sees the end once what is left is written.
        let _ = writer.join();
        let _ = writeln!(std::io::stderr());
        match end {
            Ok(RelayEnd::Detached) => {
                let _ = writeln!(std::io::stderr(), "[detached]");
                0
            }
            Ok(RelayEnd::InputClosed) => 0,
            Ok(RelayEnd::ConsoleClosed) => match established.outcome.get() {
                Some(Err(reason)) => {
                    let _ = writeln!(std::io::stderr(), "console connection failed: {reason}");
                    1
                }
                _ => {
                    let _ = writeln!(std::io::stderr(), "[console closed]");
                    0
                }
            },
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "console error: {e}");
                1
            }
        }
    });
    // The input thread may be blocked in read(2): do not wait for it.
    runtime.shutdown_background();
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_configuration_preserves_legacy_commands() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            read(&dir.path().join("missing.console.json"), "target")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn command_configuration_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.console.json");
        std::fs::write(
            &path,
            r#"{"type":"command","parameters":{"command":"printf hello","interactive":true}}"#,
        )
        .unwrap();
        let command = read(&path, "target").unwrap().unwrap().0;
        assert_eq!(command.label, "console");
        assert_eq!(command.command, "printf hello");
        assert!(command.interactive);
        std::fs::write(&path, r#"{"type":"command","parameters":{"command":" "}}"#).unwrap();
        assert!(read(&path, "target").is_err());
        std::fs::write(
            &path,
            r#"{"type":"command","parameters":{"command":"true","typo":true}}"#,
        )
        .unwrap();
        assert!(read(&path, "target").is_err());
    }

    #[test]
    fn oci_configuration_builds_interactive_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.console.json");
        std::fs::write(&path, r#"{"type":"oci","parameters":{"identity":"/path with spaces/key","profile":"profile","keep":true}}"#).unwrap();
        let command = read(&path, "target").unwrap().unwrap().0;
        assert!(command.interactive);
        assert!(command.command.contains("--instance"));
        assert!(command.command.contains("target"));
        assert!(command.command.contains("'/path with spaces/key'"));
        assert!(command.command.contains("--profile"));
        assert!(command.command.contains("--keep"));
        assert!(
            !command.command.contains("--keep-others"),
            "closing others is the default"
        );
        std::fs::write(
            &path,
            r#"{"type":"oci","parameters":{"closeOthers":false}}"#,
        )
        .unwrap();
        let command = read(&path, "target").unwrap().unwrap().0;
        assert!(command.command.contains("--keep-others"));
        std::fs::write(
            &path,
            r#"{"type":"oci","parameters":{"instance":"{name}","profile":"{host}"}}"#,
        )
        .unwrap();
        let command = read(&path, "target").unwrap().unwrap().0;
        assert!(!command.command.contains("{name}"));
        assert!(!command.command.contains("{host}"));
    }

    fn vnc(json: &str) -> Result<VncEndpoint> {
        parse_vnc(json.as_bytes(), "target")
    }

    fn direct(e: &VncEndpoint) -> &DirectVnc {
        match &e.source {
            VncSource::Direct(d) => d,
            other => panic!("not a direct endpoint: {other:?}"),
        }
    }

    #[test]
    fn vnc_defaults_to_loopback_and_the_standard_port() {
        let e = vnc(r#"{"type":"vnc","parameters":{}}"#).unwrap();
        assert_eq!(direct(&e).host, "127.0.0.1");
        assert_eq!(direct(&e).port, 5900);
        assert!(
            direct(&e).is_local()
                && !e.view_only
                && !e.resize_remote
                && !direct(&e).allow_remote
                && !e.unencrypted_remote()
        );
        assert_eq!(e.first_step(), "connecting to 127.0.0.1:5900");
        let e = vnc(r#"{"type":"vnc","parameters":{"host":"::1","port":5901}}"#).unwrap();
        assert_eq!(e.first_step(), "connecting to [::1]:5901");
    }

    #[test]
    fn vnc_rejects_remote_hosts_unless_allowed() {
        let err = vnc(r#"{"type":"vnc","parameters":{"host":"desktop.example.com"}}"#).unwrap_err();
        assert!(format!("{err:#}").contains("not encrypted"));
        let e =
            vnc(r#"{"type":"vnc","parameters":{"host":"10.0.0.5","allowRemote":true}}"#).unwrap();
        assert!(!direct(&e).is_local() && direct(&e).allow_remote && e.unencrypted_remote());
        assert!(vnc(r#"{"type":"vnc","parameters":{"host":"localhost"}}"#).is_ok());
    }

    #[test]
    fn vnc_rejects_urls_and_bad_numbers() {
        for host in [
            "vnc://127.0.0.1",
            "http://example.com/x?token=1",
            "user@127.0.0.1",
            "127.0.0.1:5900",
            "a b",
            "",
        ] {
            let json =
                serde_json::json!({"type":"vnc","parameters":{"host":host,"allowRemote":true}});
            assert!(vnc(&json.to_string()).is_err(), "{host}");
        }
        for params in [
            r#"{"port":0}"#,
            r#"{"port":65536}"#,
            r#"{"quality":10}"#,
            r#"{"compression":10}"#,
            r#"{"typo":1}"#,
        ] {
            let json = format!(r#"{{"type":"vnc","parameters":{params}}}"#);
            assert!(vnc(&json).is_err(), "{params}");
        }
    }

    #[test]
    fn vnc_names_unsupported_provider_types() {
        for kind in ["gcp", "rdp"] {
            let json =
                format!(r#"{{"type":"{kind}","parameters":{{"url":"https://x/?token=secret"}}}}"#);
            let err = format!("{:#}", vnc(&json).unwrap_err());
            assert!(err.contains("unsupported"), "{err}");
            assert!(err.contains("\"oci\""), "{err}");
            assert!(!err.contains("secret"), "{err}");
        }
    }

    fn os_console(e: &VncEndpoint) -> &OpenStackConsole {
        match &e.source {
            VncSource::OpenStack(o) => o,
            other => panic!("not an OpenStack endpoint: {other:?}"),
        }
    }

    #[test]
    fn openstack_vnc_names_the_server_after_the_target_by_default() {
        let e = vnc(r#"{"type":"openstack","parameters":{}}"#).unwrap();
        assert_eq!(os_console(&e).server, "target");
        assert_eq!(e.first_step(), "asking OpenStack for a console URL");
        assert!(!e.unencrypted_remote());
        let e = vnc(
            r#"{"type":"openstack","parameters":{"server":"web-1","cloud":"prod","cloudsFile":"/etc/clouds.yaml","caFile":"/etc/ca.pem","viewOnly":true,"quality":5}}"#,
        )
        .unwrap();
        let o = os_console(&e);
        assert_eq!(
            (o.server.as_str(), o.cloud.as_deref()),
            ("web-1", Some("prod"))
        );
        assert_eq!(o.clouds_file, Some(PathBuf::from("/etc/clouds.yaml")));
        assert!(e.view_only && e.quality == Some(5));
        let opts = o.options();
        assert_eq!(opts.server, "web-1");
        assert!(!opts.allow_insecure);
        // Plain ws:// to a remote proxy has to be asked for, and is flagged.
        let e = vnc(r#"{"type":"openstack","parameters":{"allowInsecure":true}}"#).unwrap();
        assert!(e.unencrypted_remote());
    }

    #[test]
    fn openstack_settings_are_validated() {
        for params in [
            r#"{"server":"-oops"}"#,
            r#"{"server":""}"#,
            r#"{"cloud":"-x"}"#,
            r#"{"cloudsFile":"relative.yaml"}"#,
            r#"{"caFile":"ca.pem"}"#,
            r#"{"openstackBin":"openstack"}"#,
            r#"{"quality":10}"#,
            r#"{"url":"https://cloud/?token=secret"}"#,
            r#"{"host":"127.0.0.1"}"#,
        ] {
            let json = format!(r#"{{"type":"openstack","parameters":{params}}}"#);
            let err = vnc(&json).unwrap_err();
            assert!(!format!("{err:#}").contains("secret"), "{params}");
        }
    }

    fn oci_console(e: &VncEndpoint) -> &OciConsole {
        match &e.source {
            VncSource::Oci(o) => o,
            other => panic!("not an OCI endpoint: {other:?}"),
        }
    }

    #[test]
    fn oci_vnc_names_the_instance_after_the_target_by_default() {
        let e = vnc(r#"{"type":"oci","parameters":{}}"#).unwrap();
        assert_eq!(oci_console(&e).instance, "target");
        assert!(
            e.first_step()
                .starts_with("requesting an OCI console connection")
        );
        // A private socket behind an SSH tunnel: nothing unencrypted leaves.
        assert!(!e.unencrypted_remote());
        let e = vnc(
            r#"{"type":"oci","parameters":{"instance":"web","identity":"/keys/id_rsa","configFile":"/c/config","cliRcFile":"/c/oci_cli_rc","profile":"prod","compartmentId":"ocid1.compartment.oc1..x","keep":true,"viewOnly":true,"quality":3,"resizeRemote":true}}"#,
        )
        .unwrap();
        let o = oci_console(&e);
        assert_eq!(o.instance, "web");
        assert!(e.view_only && e.resize_remote && e.quality == Some(3));
        let opts = o.options();
        assert_eq!(opts.oci.instance, "web");
        assert_eq!(opts.oci.identity, Some(PathBuf::from("/keys/id_rsa")));
        assert_eq!(opts.oci.config_file, Some(PathBuf::from("/c/config")));
        assert_eq!(opts.oci.cli_rc_file, Some(PathBuf::from("/c/oci_cli_rc")));
        assert_eq!(opts.oci.profile.as_deref(), Some("prod"));
        assert!(opts.oci.keep);
        assert!(opts.runtime_dir.is_none());
        let e = vnc(r#"{"type":"oci","parameters":{"identity":"~/.ssh/oci"}}"#).unwrap();
        assert_eq!(oci_console(&e).identity, Some(PathBuf::from("~/.ssh/oci")));
    }

    #[test]
    fn oci_settings_are_validated() {
        for params in [
            r#"{"instance":"-oops"}"#,
            r#"{"instance":""}"#,
            r#"{"instance":"a\nb"}"#,
            r#"{"profile":"-x"}"#,
            r#"{"compartmentId":""}"#,
            r#"{"identity":"id_rsa"}"#,
            r#"{"identity":"/nix/store/x-key/id_rsa"}"#,
            r#"{"configFile":"relative"}"#,
            r#"{"cliRcFile":"~/rc"}"#,
            r#"{"quality":10}"#,
            r#"{"passwordFile":"/nix/store/x-pw"}"#,
            r#"{"host":"127.0.0.1"}"#,
            r#"{"url":"https://console/?token=secret"}"#,
        ] {
            let json = format!(r#"{{"type":"oci","parameters":{params}}}"#);
            let err = vnc(&json).unwrap_err();
            assert!(!format!("{err:#}").contains("secret"), "{params}");
        }
    }

    #[test]
    fn openstack_serial_console_builds_an_interactive_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.console.json");
        std::fs::write(
            &path,
            r#"{"type":"openstack","parameters":{"cloud":"prod","caFile":"/path with spaces/ca.pem","allowInsecure":true}}"#,
        )
        .unwrap();
        let c = read(&path, "web").unwrap().unwrap().0;
        assert!(c.interactive && c.vnc.is_none());
        for needle in [
            "console openstack",
            "--server web",
            "--cloud prod",
            "'/path with spaces/ca.pem'",
            "--allow-insecure",
        ] {
            assert!(c.command.contains(needle), "{needle} in {}", c.command);
        }
        // Placeholders in values are not expanded by the dashboard.
        std::fs::write(
            &path,
            r#"{"type":"openstack","parameters":{"server":"{name}","cloud":"{host}"}}"#,
        )
        .unwrap();
        let c = read(&path, "web").unwrap().unwrap().0;
        assert!(!c.command.contains("{name}") && !c.command.contains("{host}"));
        std::fs::write(
            &path,
            r#"{"type":"openstack","parameters":{"server":"-x"}}"#,
        )
        .unwrap();
        assert!(read(&path, "web").is_err());
        std::fs::write(&path, r#"{"type":"openstack","parameters":{"typo":1}}"#).unwrap();
        assert!(read(&path, "web").is_err());
    }

    #[test]
    fn vnc_password_file_must_be_absolute_and_outside_the_store() {
        for f in ["relative", "/nix/store/abc-secret", ""] {
            let json = serde_json::json!({"type":"vnc","parameters":{"passwordFile":f}});
            assert!(vnc(&json.to_string()).is_err(), "{f}");
        }
        let e = vnc(r#"{"type":"vnc","parameters":{"passwordFile":"/run/secrets/vnc"}}"#).unwrap();
        assert_eq!(e.password_file, Some(PathBuf::from("/run/secrets/vnc")));
    }

    #[test]
    fn read_vnc_builds_a_command_free_extra() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.vnc.json");
        assert!(read_vnc(&path, "target").unwrap().is_none());
        std::fs::write(&path, r#"{"type":"vnc","parameters":{"viewOnly":true}}"#).unwrap();
        let cmd = read_vnc(&path, "target").unwrap().unwrap().0;
        assert_eq!(cmd.label, VNC_LABEL);
        assert!(!cmd.interactive);
        assert!(cmd.vnc.as_ref().unwrap().view_only);
        std::fs::write(&path, "{").unwrap();
        assert!(read_vnc(&path, "target").is_err());
    }

    #[test]
    fn escape_remnants_are_taken_out_of_the_console_log() {
        assert_eq!(
            strip_escape_remnants("[[0;32m  OK  [0m] Started [0;1;39mfoo[0m."),
            "[  OK  ] Started foo."
        );
        assert_eq!(strip_escape_remnants("x[!p]104\\[0m[?7h[1G[0Jy"), "xy");
        assert_eq!(strip_escape_remnants("a]3008;start=bf06\\b"), "ab");
        // Text that only looks a bit like it stays.
        for keep in [
            "[   1.234] kernel",
            "[  OK  ]",
            "array[3] = x",
            "ä [Ü] ö",
            "a]b",
            "[Install]",
        ] {
            assert_eq!(strip_escape_remnants(keep), keep);
        }
    }

    #[test]
    fn a_sidecar_can_mark_its_view_as_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let vnc = dir.path().join("t.vnc.json");
        std::fs::write(&vnc, r#"{"type":"vnc","parameters":{}}"#).unwrap();
        assert!(!read_vnc(&vnc, "t").unwrap().unwrap().1);
        std::fs::write(&vnc, r#"{"default":true,"type":"vnc","parameters":{}}"#).unwrap();
        let (cmd, default) = read_vnc(&vnc, "t").unwrap().unwrap();
        assert!(default && cmd.vnc.is_some());
        std::fs::write(&vnc, r#"{"default":"yes","type":"vnc","parameters":{}}"#).unwrap();
        assert!(read_vnc(&vnc, "t").is_err());
        let console = dir.path().join("t.console.json");
        std::fs::write(
            &console,
            r#"{"type":"command","default":true,"parameters":{"command":"echo"}}"#,
        )
        .unwrap();
        assert!(read(&console, "t").unwrap().unwrap().1);
        // Anything else unknown is still refused.
        std::fs::write(&vnc, r#"{"type":"vnc","dflt":true,"parameters":{}}"#).unwrap();
        assert!(read_vnc(&vnc, "t").is_err());
    }

    #[test]
    fn password_file_checks_mode_type_and_content() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw");
        std::fs::write(&file, "hunter2\r\nsecond line\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = format!("{:#}", read_password_file(&file).unwrap_err());
        assert!(err.contains("accessible to other users") && !err.contains("hunter2"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_password_file(&file).unwrap().as_str(), "hunter2");
        std::fs::write(&file, "\n").unwrap();
        assert!(read_password_file(&file).is_err());
        std::fs::write(&file, vec![b'a'; 5000]).unwrap();
        assert!(read_password_file(&file).is_err());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(read_password_file(&link).is_err());
        assert!(read_password_file(dir.path()).is_err());
    }
}
