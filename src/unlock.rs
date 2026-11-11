//! Deliver the LUKS passphrase to a target waiting in its initrd.

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use zeroize::Zeroizing;

use crate::ctx::Ctx;
use crate::process::{self, Output, TimedOut, shell_quote};
use crate::settings::{KnownHostsKind, LuksType};

/// Strip what must not reach the logs from remote output: the passphrase,
/// ANSI escapes, control characters and password-mask bullets (which would
/// reveal the passphrase length). Empty lines are dropped.
pub fn sanitize(text: &str, passphrase: Option<&str>) -> Vec<String> {
    let redacted;
    let text = match passphrase.filter(|p| !p.is_empty()) {
        Some(p) => {
            redacted = text.replace(p, "[REDACTED]");
            redacted.as_str()
        }
        None => text,
    };
    text.lines()
        .map(|line| {
            let mut out = String::with_capacity(line.len());
            let mut chars = line.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    // CSI: ESC [ params final-byte
                    if chars.peek() == Some(&'[') {
                        chars.next();
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    }
                    continue;
                }
                if c.is_control() && c != '\t' {
                    continue;
                }
                out.push(c);
            }
            strip_mask_runs(&out).trim_end().to_string()
        })
        .filter(|l| !l.is_empty())
        .collect()
}

/// Remove runs of two or more `*` or `•`.
fn strip_mask_runs(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' || c == '•' {
            let start = i;
            while i < chars.len() && (chars[i] == '*' || chars[i] == '•') {
                i += 1;
            }
            if i - start == 1 {
                out.push(c);
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Undo systemd unit-name escaping (`luks\x2dabc` -> `luks-abc`).
pub fn unit_unescape(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1] == b'x' {
            let hex = std::str::from_utf8(&bytes[i + 2..i + 4]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `systemctl list-unit-files` for cryptsetup units.
pub fn cryptsetup_units(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|unit| unit.strip_prefix("systemd-cryptsetup@"))
        .filter_map(|rest| rest.strip_suffix(".service"))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Find the `/dev/disk/by-uuid/...` device in a cryptsetup unit's ExecStart.
pub fn cryptsetup_device(unit_text: &str) -> Option<String> {
    unit_text
        .lines()
        .filter(|l| l.trim_start().starts_with("ExecStart="))
        .find_map(|l| {
            let start = l.find("/dev/disk/by-uuid/")?;
            let dev: String = l[start..]
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '\'' && *c != '"')
                .collect();
            Some(dev)
        })
}

impl Ctx {
    fn passphrase_bytes(&self) -> Result<Zeroizing<Vec<u8>>> {
        let p = self
            .passphrase
            .as_ref()
            .ok_or_else(|| anyhow!("the LUKS passphrase is not loaded"))?;
        let mut bytes = Zeroizing::new(Vec::with_capacity(p.len() + 1));
        bytes.extend_from_slice(p.as_bytes());
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Log sanitized remote output at debug level; return its last line.
    fn log_remote(&self, text: &str) -> Option<String> {
        let lines = sanitize(text, self.passphrase.as_deref().map(String::as_str));
        for line in &lines {
            self.log.debug(format!("{}: {line}", self.s.hostname));
        }
        lines.last().cloned()
    }

    fn ssh_initrd(
        &self,
        remote: Option<&str>,
        stdin: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<Output> {
        // -T: never ask for a TTY (stdin is a pipe; ssh would only warn).
        let cmd = self.ssh(&self.endpoint(), KnownHostsKind::Initrd, &["-T"], remote);
        self.exec(cmd, stdin)
    }

    /// Send the passphrase. On failure the error names the last line the
    /// target printed.
    pub fn unlock(&self) -> Result<()> {
        match self.s.luks_type {
            LuksType::Raw => self.unlock_stdin(None),
            LuksType::LuksMount => self.unlock_stdin(Some("luks-mount")),
            LuksType::SystemdTool => self.unlock_cryptsetup(),
            LuksType::Systemd => self.unlock_tty_agent(),
        }
    }

    fn unlock_stdin(&self, remote: Option<&str>) -> Result<()> {
        let out = self.ssh_initrd(remote, Some(self.passphrase_bytes()?))?;
        // Prefer what the target said over ssh's own diagnostics.
        let tail = self
            .log_remote(&out.stdout)
            .or(self.log_remote(&out.stderr));
        if out.success() {
            return Ok(());
        }
        Err(remote_failure(out.code, tail))
    }

    fn unlock_cryptsetup(&self) -> Result<()> {
        let out = self.ssh_initrd(
            Some("systemctl --no-pager --no-legend list-unit-files 'systemd-cryptsetup@*'"),
            None,
        )?;
        let units = cryptsetup_units(&out.stdout);
        let Some(unit_name) = units.first() else {
            bail!("no systemd-cryptsetup@ unit found on {}", self.s.hostname);
        };
        if units.len() > 1 {
            self.log.warn(format!(
                "{} has several cryptsetup units ({}); unlocking {unit_name}",
                self.s.hostname,
                units.join(", ")
            ));
        }
        let unit = format!("systemd-cryptsetup@{unit_name}.service");
        let out = self.ssh_initrd(
            Some(&format!("systemctl cat --no-pager {}", shell_quote(&unit))),
            None,
        )?;
        let Some(device) = cryptsetup_device(&out.stdout) else {
            bail!(
                "cannot find the encrypted device of {unit} on {}",
                self.s.hostname
            );
        };
        let mapper = unit_unescape(unit_name);
        let open = format!(
            "cryptsetup luksOpen {} {} -",
            shell_quote(&device),
            shell_quote(&mapper)
        );
        let out = self.ssh_initrd(Some(&open), Some(self.passphrase_bytes()?))?;
        let tail = self.log_remote(&out.combined());
        if !out.success() {
            return Err(anyhow!(
                "cryptsetup could not open {device}{}",
                tail.map(|t| format!(": {t}")).unwrap_or_default()
            ));
        }
        let out = self.ssh_initrd(
            Some(&format!("systemctl restart {}", shell_quote(&unit))),
            None,
        )?;
        let tail = self.log_remote(&out.combined());
        if out.success() {
            Ok(())
        } else {
            Err(remote_failure(out.code, tail))
        }
    }

    /// Answer systemd's password prompt through `systemd-tty-ask-password-agent`
    /// on a remote TTY. The passphrase is only sent once the TTY has echo
    /// disabled, signalled by a random marker line.
    fn unlock_tty_agent(&self) -> Result<()> {
        let marker = format!("LUKS-SSH-UNLOCK-READY-{}-{}", std::process::id(), nonce());
        let remote = format!(
            "stty -echo && printf '\\n%s\\n' '{marker}' && exec systemd-tty-ask-password-agent"
        );
        let mut cmd = self.ssh(
            &self.endpoint(),
            KnownHostsKind::Initrd,
            &["-o", "LogLevel=ERROR", "-tt"],
            Some(&remote),
        );
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if Instant::now() >= self.deadline {
            bail!(TimedOut);
        }
        let mut child = cmd.spawn().map_err(|e| anyhow!("cannot run ssh: {e}"))?;
        let mut stdin = child.stdin.take().expect("piped stdin");

        let (tx, rx) = mpsc::channel::<String>();
        for pipe in [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let tx = tx.clone();
            thread::spawn(move || {
                for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send(line.trim_end_matches('\r').to_string()).is_err() {
                        break;
                    }
                }
            });
        }
        drop(tx);

        let mut tail = None;
        let mut ready = false;
        loop {
            let wait = self.deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(line) if !ready && line == marker => {
                    ready = true;
                    let pass = self.passphrase_bytes()?;
                    if stdin.write_all(&pass).and_then(|_| stdin.flush()).is_err() {
                        break;
                    }
                }
                Ok(line) => {
                    if let Some(t) = self.log_remote(&line) {
                        tail = Some(t);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(TimedOut);
                }
            }
        }
        drop(stdin);
        let (code, timed_out) = process::wait_until(&mut child, self.deadline)?;
        if timed_out {
            bail!(TimedOut);
        }
        if !ready {
            bail!(
                "could not prepare the remote terminal for the passphrase{}",
                tail.map(|t| format!(": {t}")).unwrap_or_default()
            );
        }
        if code == Some(0) {
            Ok(())
        } else {
            Err(remote_failure(code, tail))
        }
    }
}

fn remote_failure(code: Option<i32>, tail: Option<String>) -> anyhow::Error {
    let tail = tail.map(|t| t.trim_end_matches('.').to_string());
    let status = match code {
        Some(c) => format!("exit code {c}"),
        None => "killed".into(),
    };
    match tail {
        Some(t) => anyhow!("{status}; last output: {t}"),
        None => anyhow!("{status}"),
    }
}

fn nonce() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut h = RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_noise_and_secret() {
        let lines = sanitize(
            "\x1b[1;31mPlease enter passphrase:\x1b[0m ********\r\nhunter2 ok\n\n\x07  \nsingle * star",
            Some("hunter2"),
        );
        assert_eq!(
            lines,
            vec!["Please enter passphrase:", "[REDACTED] ok", "single * star"]
        );
        assert_eq!(sanitize("•••• done", None), vec![" done"]);
    }

    #[test]
    fn unescape_unit_names() {
        assert_eq!(unit_unescape(r"luks\x2d1234"), "luks-1234");
        assert_eq!(unit_unescape("root"), "root");
        assert_eq!(unit_unescape(r"bad\xZZ"), r"bad\xZZ");
    }

    #[test]
    fn parses_cryptsetup_units() {
        let listing = "UNIT FILE STATE PRESET\nsystemd-cryptsetup@root.service generated -\nsystemd-cryptsetup@luks\\x2dab.service generated -\nother.service enabled\n";
        assert_eq!(cryptsetup_units(listing), vec!["root", r"luks\x2dab"]);
        let unit = "[Service]\nExecStart=/usr/lib/systemd/systemd-cryptsetup attach 'root' '/dev/disk/by-uuid/1234-abcd' 'none' ''\n";
        assert_eq!(
            cryptsetup_device(unit).as_deref(),
            Some("/dev/disk/by-uuid/1234-abcd")
        );
    }
}
