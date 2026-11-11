//! `status`: what is configured and what the target is doing right now,
//! without sending anything. Shared by the CLI (text or JSON) and the TUI.

use std::fs;
use std::io::IsTerminal;
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Serialize;

use crate::checksum::ChecksumResult;
use crate::ctx::Ctx;
use crate::probe::InitrdProbe;
use crate::settings::KnownHostsKind;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "kebab-case", tag = "state", content = "detail")]
pub enum TargetState {
    /// Booted and unlocked.
    Unlocked(String),
    /// Waiting in its initrd; the detail says whether it may be unlocked.
    Initrd(String),
    /// SSH answers but it is not the initrd.
    NotInitrd,
    /// The initrd SSH login failed.
    SshFailed(String),
    /// Nothing answers.
    Unreachable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warn,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct Note {
    pub severity: Severity,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub hostname: String,
    pub route: String,
    pub luks_type: &'static str,
    pub healthcheck: Option<String>,
    pub checksum_file: Option<String>,
    pub signature_required: bool,
    pub host_keys_pinned: bool,
    pub baseline_refreshed: Option<DateTime<Local>>,
    pub uptime: Option<String>,
    pub remote_baseline_date: Option<String>,
    pub state: TargetState,
    /// The initrd would be unlocked by `run` right now.
    pub unlockable: bool,
    pub notes: Vec<Note>,
    pub checked_at: DateTime<Local>,
    pub duration_ms: u128,
}

impl Report {
    pub fn ok(&self) -> bool {
        matches!(self.state, TargetState::Unlocked(_)) || self.unlockable
    }
}

fn format_date(text: &str) -> String {
    DateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f %z")
        .map(|d| {
            d.with_timezone(&Local)
                .format("%d %b %Y, %H:%M:%S %Z")
                .to_string()
        })
        .unwrap_or_else(|_| text.to_string())
}

impl Ctx {
    /// Probe the target. With VERIFY, a target in its initrd also gets its
    /// checksum verified (slow in paranoid mode, so the TUI skips it).
    pub fn status(&mut self, verify: bool) -> Result<Report> {
        let started = Instant::now();
        self.start_check();
        self.pin_connect_address()?;
        let s = &self.s;
        let mut notes = Vec::new();
        let mut note = |severity, text: String| notes.push(Note { severity, text });

        let healthcheck = match (&s.healthcheck.port, &s.healthcheck.command) {
            (Some(p), Some(c)) => Some(format!("port {p} or `{c}`")),
            (Some(p), None) => Some(format!("port {p}")),
            (None, Some(c)) => Some(format!("`{c}`")),
            (None, None) => None,
        };
        let baseline_refreshed = s
            .initrd
            .file
            .as_ref()
            .and_then(|f| fs::metadata(f).ok())
            .and_then(|m| m.modified().ok())
            .map(DateTime::<Local>::from);

        let mut report = Report {
            hostname: s.hostname.clone(),
            route: s.route(),
            luks_type: s.luks_type.as_str(),
            healthcheck,
            checksum_file: s.initrd.file.as_ref().map(|f| f.display().to_string()),
            signature_required: s.initrd.require_signature,
            host_keys_pinned: s.has_regular_host_keys() && s.has_initrd_host_keys(),
            baseline_refreshed,
            uptime: None,
            remote_baseline_date: None,
            state: TargetState::Unreachable,
            unlockable: false,
            notes: Vec::new(),
            checked_at: Local::now(),
            duration_ms: 0,
        };
        if !report.host_keys_pinned {
            note(
                Severity::Warn,
                "host keys are not pinned for both the booted system and the initrd".into(),
            );
        }
        if s.initrd.file.is_none() {
            note(
                Severity::Warn,
                "initrd checksum validation is not configured".into(),
            );
        }

        let booted = self.healthcheck()?;
        if booted {
            let kind = if s.healthcheck.port.is_some() {
                KnownHostsKind::Default
            } else {
                s.healthcheck.known_hosts
            };
            (report.uptime, report.remote_baseline_date) = self.remote_metadata(kind)?;
            // The command itself is shown once, in the "Healthcheck" row.
            report.state = TargetState::Unlocked("healthcheck passed".into());
        } else {
            if s.healthcheck.port.is_none() && s.healthcheck.command.is_none() {
                note(
                    Severity::Warn,
                    "no healthcheck configured: a booted host cannot be told apart".into(),
                );
            }
            report.state = match self.is_initrd()? {
                InitrdProbe::Yes => {
                    (report.uptime, report.remote_baseline_date) =
                        self.remote_metadata(KnownHostsKind::Initrd)?;
                    let result = if verify || self.s.initrd.file.is_none() {
                        self.check_initrd_checksum()?
                    } else {
                        ChecksumResult::Skipped
                    };
                    match result {
                        ChecksumResult::Skipped => {
                            report.unlockable = true;
                            TargetState::Initrd(
                                "waiting for its passphrase (checksum is verified before unlocking)".into(),
                            )
                        }
                        ChecksumResult::Passed => {
                            report.unlockable = true;
                            TargetState::Initrd("checksum and signature verified".into())
                        }
                        ChecksumResult::NotConfigured => {
                            report.unlockable = true;
                            TargetState::Initrd("waiting for its passphrase (not verified)".into())
                        }
                        ChecksumResult::Failed(reason) => {
                            note(Severity::Error, reason.clone());
                            TargetState::Initrd(format!("refusing to unlock: {reason}"))
                        }
                    }
                }
                InitrdProbe::No => TargetState::NotInitrd,
                InitrdProbe::Unknown(err) => {
                    if self.s.skip_port_check || self.check_ssh_port()? {
                        TargetState::SshFailed(err)
                    } else {
                        TargetState::Unreachable
                    }
                }
            };
        }
        report.remote_baseline_date = report.remote_baseline_date.as_deref().map(format_date);
        report.notes = notes;
        report.duration_ms = started.elapsed().as_millis();
        Ok(report)
    }
}

struct Palette {
    reset: &'static str,
    green: &'static str,
    yellow: &'static str,
    red: &'static str,
    cyan: &'static str,
    magenta: &'static str,
    dim: &'static str,
}

impl Palette {
    fn detect() -> Self {
        let force = std::env::var_os("FORCE_COLOR").is_some_and(|v| !v.is_empty());
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
        let tty = std::io::stdout().is_terminal() && std::env::var("TERM").as_deref() != Ok("dumb");
        if force || (tty && !no_color) {
            Self {
                reset: "\x1b[0m",
                green: "\x1b[32m",
                yellow: "\x1b[33m",
                red: "\x1b[31m",
                cyan: "\x1b[36m",
                magenta: "\x1b[35m",
                dim: "\x1b[2m",
            }
        } else {
            Self {
                reset: "",
                green: "",
                yellow: "",
                red: "",
                cyan: "",
                magenta: "",
                dim: "",
            }
        }
    }
}

pub fn print_text(r: &Report) {
    let c = Palette::detect();
    let row = |label: &str, color: &str, value: &str| {
        println!("  {label:<19} {color}{value}{}", c.reset);
    };
    println!("{}LUKS SSH unlock: {}{}", c.cyan, r.hostname, c.reset);
    row("SSH target", c.cyan, &r.route);
    row("LUKS type", c.magenta, r.luks_type);
    row(
        "Healthcheck",
        c.cyan,
        r.healthcheck.as_deref().unwrap_or("not configured"),
    );
    row(
        "Initrd checksum",
        c.cyan,
        r.checksum_file.as_deref().unwrap_or("not configured"),
    );
    row(
        "Signature check",
        if r.signature_required {
            c.green
        } else {
            c.yellow
        },
        if r.signature_required {
            "required"
        } else {
            "optional"
        },
    );
    row(
        "Host-key checks",
        if r.host_keys_pinned {
            c.green
        } else {
            c.yellow
        },
        if r.host_keys_pinned {
            "regular and initrd keys pinned"
        } else {
            "not fully configured"
        },
    );
    row(
        "Baseline refreshed",
        c.yellow,
        &r.baseline_refreshed
            .map(|d| d.format("%d %b %Y, %H:%M:%S %Z").to_string())
            .unwrap_or_else(|| "not available".into()),
    );

    println!("\n{}Target state{}", c.cyan, c.reset);
    if r.uptime.is_some() || r.remote_baseline_date.is_some() {
        row(
            "Target uptime",
            c.cyan,
            r.uptime.as_deref().unwrap_or("unavailable"),
        );
        row(
            "Checksum date",
            c.yellow,
            r.remote_baseline_date.as_deref().unwrap_or("unavailable"),
        );
    }
    let (color, icon, text) = match &r.state {
        TargetState::Unlocked(d) => (c.green, "✅", format!("Unlocked; {d}")),
        TargetState::Initrd(d) if r.unlockable => (c.yellow, "🔐", format!("In initrd; {d}")),
        TargetState::Initrd(d) => (c.red, "❌", format!("In initrd; {d}")),
        TargetState::NotInitrd => (
            c.yellow,
            "⚠️ ",
            "SSH answers, but this is not the initrd".into(),
        ),
        TargetState::SshFailed(e) => (c.red, "❌", format!("Initrd SSH login failed: {e}")),
        TargetState::Unreachable => (c.red, "❌", "Not reachable over SSH".into()),
    };
    println!("  {color}{icon} {text}{}", c.reset);
    for n in &r.notes {
        let color = match n.severity {
            Severity::Warn => c.yellow,
            Severity::Error => c.red,
        };
        println!("  {color}• {}{}", n.text, c.reset);
    }
    println!(
        "{}Checked in {:.1}s{}",
        c.dim,
        r.duration_ms as f64 / 1000.0,
        c.reset
    );
}
