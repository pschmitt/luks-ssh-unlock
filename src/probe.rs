//! Reachability probes: DNS, TCP ports, the healthcheck and the initrd test.

use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::Result;

use crate::ctx::{Ctx, is_ip};
use crate::process::shell_quote;
use crate::settings::{IpFamily, KnownHostsKind};

/// Result of the `/etc/initrd-release` test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitrdProbe {
    Yes,
    No,
    /// SSH itself failed (bad key, host key mismatch, jumphost down...).
    Unknown(String),
}

fn family_ok(ip: &IpAddr, family: IpFamily) -> bool {
    match family {
        IpFamily::Any => true,
        IpFamily::V4 => ip.is_ipv4(),
        IpFamily::V6 => ip.is_ipv6(),
    }
}

impl Ctx {
    /// Resolve the target to an IP address, through the jumphost if there is
    /// one (its resolver may see names we cannot).
    pub fn resolve_target(&self) -> Result<Option<String>> {
        if let Some(addr) = &self.s.connect_address {
            return Ok(Some(addr.clone()));
        }
        let host = &self.s.hostname;
        if is_ip(host) {
            return Ok(Some(host.clone()));
        }
        if self.s.jumphost.is_some() {
            let rtype = match self.s.ip_family {
                IpFamily::V6 => "AAAA",
                _ => "A",
            };
            let remote = format!("dig +short {rtype} {} 2>/dev/null", shell_quote(host));
            let Some(cmd) = self.ssh_jumphost(&remote) else {
                return Ok(None);
            };
            let out = self.exec(cmd, None)?;
            // dig +short lists CNAMEs before addresses.
            return Ok(out
                .stdout
                .lines()
                .map(str::trim)
                .find(|l| is_ip(l))
                .map(str::to_string));
        }
        let addrs = (host.as_str(), 0)
            .to_socket_addrs()
            .map(|it| it.collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(addrs
            .iter()
            .map(SocketAddr::ip)
            .find(|ip| family_ok(ip, self.s.ip_family))
            .map(|ip| ip.to_string()))
    }

    /// With --ipv4/--ipv6, resolve the target once per check and connect to
    /// that address while verifying host keys under its name. Otherwise a
    /// jumphost resolves `-W host:port` itself and may try stale AAAA records
    /// first, each costing a connect timeout.
    pub fn pin_connect_address(&mut self) -> Result<()> {
        self.pinned = None;
        if self.s.connect_address.is_some()
            || is_ip(&self.s.hostname)
            || self.s.ip_family == IpFamily::Any
        {
            return Ok(());
        }
        match self.resolve_target()? {
            Some(addr) => {
                self.log
                    .debug(format!("Connecting to {} at {addr}", self.s.hostname));
                self.pinned = Some(addr);
            }
            None => self.log.debug(format!(
                "Could not resolve {}; connecting by name",
                self.s.hostname
            )),
        }
        Ok(())
    }

    /// TCP connect to the target (directly, not through the jumphost).
    fn tcp_open(&self, port: u16) -> Result<bool> {
        let timeout = self.remaining(Duration::from_secs(2))?;
        let host = self
            .pinned
            .clone()
            .or_else(|| self.s.connect_address.clone())
            .unwrap_or_else(|| self.s.hostname.clone());
        let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
            self.log.debug(format!("Cannot resolve {host}"));
            return Ok(false);
        };
        for addr in addrs.filter(|a| family_ok(&a.ip(), self.s.ip_family)) {
            if TcpStream::connect_timeout(&addr, timeout).is_ok() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Is the target's SSH port open? Through a jumphost we ask it to connect
    /// and look for the SSH banner.
    pub fn check_ssh_port(&self) -> Result<bool> {
        if self.s.jumphost.is_none() {
            return self.tcp_open(self.s.port);
        }
        // Some nc implementations lack -4/-6, so resolve first.
        let target = self.pinned.clone().map(Ok).unwrap_or_else(|| {
            self.resolve_target()
                .map(|a| a.unwrap_or_else(|| self.s.hostname.clone()))
        })?;
        let remote = format!("nc {} {}", shell_quote(&target), self.s.port);
        let Some(cmd) = self.ssh_jumphost(&remote) else {
            return Ok(false);
        };
        // A newline makes sshd reject our "banner" and hang up right away.
        let out = self.exec(cmd, Some(zeroize::Zeroizing::new(b"\n".to_vec())))?;
        Ok(out
            .combined()
            .lines()
            .any(|l| l.to_ascii_uppercase().starts_with("SSH-")))
    }

    /// Is the target booted and unlocked?
    pub fn healthcheck(&self) -> Result<bool> {
        if let Some(port) = self.s.healthcheck.port {
            if self.tcp_open(port)? {
                self.log.debug(format!("Healthcheck port {port} is open"));
                return Ok(true);
            }
            self.log.debug(format!(
                "Healthcheck port {port} on {} is closed",
                self.s.hostname
            ));
        }
        let Some(command) = &self.s.healthcheck.command else {
            return Ok(false);
        };
        let ep = self.healthcheck_endpoint();
        let remote = format!("sh -c {}", shell_quote(command));
        let cmd = self.ssh(&ep, self.s.healthcheck.known_hosts, &[], Some(&remote));
        let out = self.exec(cmd, None)?;
        let text = out.combined();
        let text = text.trim();
        if out.success() {
            self.log.debug(format!(
                "Healthcheck passed: {}",
                if text.is_empty() { "no output" } else { text }
            ));
            Ok(true)
        } else {
            self.log.debug(format!(
                "Healthcheck failed for {}: {}",
                ep.host,
                if text.is_empty() { "no output" } else { text }
            ));
            Ok(false)
        }
    }

    /// Does the target run its initrd (`/etc/initrd-release` exists)?
    pub fn is_initrd(&self) -> Result<InitrdProbe> {
        let cmd = self.ssh(
            &self.endpoint(),
            KnownHostsKind::Initrd,
            &[],
            Some("test -e /etc/initrd-release"),
        );
        let out = self.exec(cmd, None)?;
        Ok(match out.code {
            Some(0) => InitrdProbe::Yes,
            Some(1) => InitrdProbe::No,
            code => InitrdProbe::Unknown(out.error_line().unwrap_or_else(|| match code {
                Some(c) => format!("ssh exited with code {c}"),
                None => "ssh was killed".into(),
            })),
        })
    }

    /// Uptime and baseline date reported by the target, in one round trip.
    pub fn remote_metadata(
        &self,
        kind: KnownHostsKind,
    ) -> Result<(Option<String>, Option<String>)> {
        let remote =
            "uptime -p 2>/dev/null; echo ---; stat -c %y /etc/initrd-checksum/checksum 2>/dev/null";
        let out = self.exec(self.ssh(&self.endpoint(), kind, &[], Some(remote)), None)?;
        let mut parts = out.stdout.splitn(2, "---");
        let clean = |s: Option<&str>| {
            s.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Ok((clean(parts.next()), clean(parts.next())))
    }
}
