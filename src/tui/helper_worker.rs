//! The dashboard's worker for a target whose configuration the user cannot
//! read: every check, unlock and service action goes through the privileged
//! helper, and only its closed answers come back.
//!
//! The dashboard itself stays the invoking user's process, so nothing else it
//! does (the provider consoles, the VNC viewer, the extra commands) changes
//! account or environment. What the user sees about a target here is less
//! than when the configuration is readable: the helper reports states and
//! which checks were in force, not uptimes, baseline dates or routes.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Local;

use super::{Cmd, Msg, UnitInfo};
use crate::daemon::State;
use crate::helper::client::{Client, ClientError};
use crate::helper::reply::{Checks, Details, Health, Integrity, Luks, Observed, Outcome};
use crate::log::Logger;
use crate::status::{Note, Report, Severity, TargetState};

pub(super) struct Spec {
    pub idx: usize,
    pub name: String,
    pub host: String,
    pub refresh: Duration,
}

/// Start the worker and the journal follower of a helper-backed target; the
/// returned sender takes the same commands as an ordinary worker's.
pub(super) fn spawn(spec: Spec, client: Client, logger: Logger, tx: Sender<Msg>) -> Sender<Cmd> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let unit = format!("luks-ssh-unlock-{}.service", spec.name);
    let follow = (unit.clone(), cmd_tx.clone(), tx.clone(), spec.idx);
    thread::spawn(move || worker(spec, client, logger, cmd_rx, tx));
    thread::spawn(move || super::follow_journal(follow.3, follow.0, follow.1, follow.2));
    cmd_tx
}

fn worker(spec: Spec, client: Client, log: Logger, rx: Receiver<Cmd>, tx: Sender<Msg>) {
    let Spec {
        idx,
        name,
        host,
        refresh,
    } = spec;
    let unit = format!("luks-ssh-unlock-{name}.service");
    let mut next = Instant::now();
    let mut last_state: Option<String> = None;
    loop {
        let mut manual = true;
        let cmd = match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => {
                manual = false;
                Cmd::Probe
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match cmd {
            Cmd::Probe => {
                let _ = tx.send(Msg::Busy(idx, Some("checking")));
                let started = Instant::now();
                match client.probe(&name) {
                    Ok(outcome) => match report(&host, &outcome, started.elapsed()) {
                        Some(report) => {
                            let now = super::describe(&report);
                            if manual || last_state.as_deref() != Some(now.as_str()) {
                                log.info(match &last_state {
                                    Some(prev) if *prev != now => {
                                        format!("Check: {prev} → {now}")
                                    }
                                    _ => format!("Check: {now}"),
                                });
                            }
                            last_state = Some(now);
                            let _ = tx.send(Msg::Report(idx, Box::new(report)));
                        }
                        None => log.error(ClientError::Protocol.message()),
                    },
                    Err(e) => {
                        // A repeated failure is logged once, like a state.
                        let text = e.message().to_string();
                        if manual || last_state.as_deref() != Some(text.as_str()) {
                            log.error(format!("Status check failed: {text}"));
                        }
                        last_state = Some(text);
                    }
                }
                let _ = tx.send(Msg::Unit(idx, UnitInfo::query(&unit)));
                next = Instant::now() + refresh;
            }
            Cmd::Service(start) => {
                let action = if start {
                    crate::helper::request::ServiceAction::Start
                } else {
                    crate::helper::request::ServiceAction::Stop
                };
                let verb = if start { "start" } else { "stop" };
                let _ = tx.send(Msg::Busy(
                    idx,
                    Some(if start {
                        "starting service"
                    } else {
                        "stopping service"
                    }),
                ));
                log.info(format!("Asking the helper to {verb} {unit}"));
                match client.service(&name, action) {
                    Ok(true) => log.info(format!(
                        "Service {}",
                        if start { "started" } else { "stopped" }
                    )),
                    Ok(false) => log.error(format!("The helper could not {verb} the service")),
                    Err(e) => log.error(format!("Service {verb} failed: {}", admin_hint(e))),
                }
                let _ = tx.send(Msg::Unit(idx, UnitInfo::query(&unit)));
            }
            Cmd::Unlock { force } => {
                let _ = tx.send(Msg::Busy(
                    idx,
                    Some(if force {
                        "force-unlocking"
                    } else {
                        "unlocking"
                    }),
                ));
                let state = match client.unlock(&name, force) {
                    Ok(Outcome::Unlock {
                        state, unlocked, ..
                    }) => {
                        let state = daemon_state(state);
                        if unlocked {
                            log.info("Unlocked: the passphrase was accepted");
                        } else {
                            log.error(format!("Unlock did not succeed ({})", state_text(state)));
                        }
                        Some(state)
                    }
                    Ok(_) => {
                        log.error(ClientError::Protocol.message());
                        None
                    }
                    Err(ClientError::Denied) if !force => {
                        log.error(
                            "Unlock refused: a plain unlock needs a checksum baseline and pinned initrd host keys; a forced unlock needs the admin helper group",
                        );
                        None
                    }
                    Err(e) => {
                        log.error(format!("Unlock failed: {}", admin_hint(e)));
                        None
                    }
                };
                let _ = tx.send(Msg::Done(idx, state));
                next = Instant::now();
            }
        }
        let _ = tx.send(Msg::Busy(idx, None));
    }
}

fn admin_hint(e: ClientError) -> String {
    match e {
        ClientError::Unavailable => {
            "the admin helper socket is not available (it needs the admin group)".into()
        }
        e => e.message().into(),
    }
}

fn daemon_state(o: Observed) -> State {
    match o {
        Observed::Booted => State::Booted,
        Observed::Unreachable => State::Unreachable,
        Observed::NotInitrd => State::NotInitrd,
        Observed::SshFailed => State::InitrdSshFailed,
        Observed::Initrd => State::Initrd,
        Observed::ChecksumFailed => State::ChecksumFailed,
        Observed::UnlockFailed => State::UnlockFailed,
        Observed::Unlocked => State::Unlocked,
    }
}

fn state_text(state: State) -> &'static str {
    match state {
        State::Booted => "booted",
        State::Unreachable => "unreachable",
        State::NotInitrd => "not in the initrd",
        State::InitrdSshFailed => "SSH login to the initrd failed",
        State::Initrd => "waiting in the initrd",
        State::ChecksumFailed => "the initrd did not match its baseline",
        State::UnlockFailed => "sending the passphrase failed",
        State::Unlocked => "unlocked",
    }
}

/// A report for the dashboard from what the helper said; none when the
/// answer was not a probe.
fn report(host: &str, outcome: &Outcome, took: Duration) -> Option<Report> {
    let Outcome::Probe {
        state,
        unlockable,
        checks,
        details,
    } = outcome
    else {
        return None;
    };
    let mut notes = Vec::new();
    if details.is_none() {
        notes.push(Note {
            severity: Severity::Warn,
            text: "reported by an older privileged helper: no uptime, route or baseline dates"
                .into(),
        });
    }
    notes.extend(check_notes(checks));
    let d = details.clone().unwrap_or_default();
    if details.is_some() && d.healthcheck.is_none() {
        notes.push(Note {
            severity: Severity::Warn,
            text: "no healthcheck configured: a booted host cannot be told apart".into(),
        });
    }
    let target_state = match state {
        Observed::Booted | Observed::Unlocked => TargetState::Unlocked("healthcheck passed".into()),
        Observed::Unreachable => TargetState::Unreachable,
        Observed::NotInitrd => TargetState::NotInitrd,
        Observed::SshFailed => TargetState::SshFailed("login failed".into()),
        Observed::Initrd => TargetState::Initrd(match checks.integrity {
            Integrity::Verified => "checksum verified".into(),
            Integrity::NotConfigured => "no baseline configured".into(),
            _ => "unverified".into(),
        }),
        Observed::ChecksumFailed | Observed::UnlockFailed => {
            TargetState::Initrd("checksum verification failed".into())
        }
    };
    Some(Report {
        hostname: host.to_string(),
        route: route(&d),
        luks_type: match d.luks {
            Some(Luks::Raw) => "raw",
            Some(Luks::Systemd) => "systemd",
            Some(Luks::SystemdTool) => "systemd-tool",
            Some(Luks::Mount) => "luks-mount",
            None => "-",
        },
        healthcheck: d.healthcheck.map(|h| match h {
            Health::Port { port } => format!("port {port}"),
            Health::Command => "a command".into(),
            Health::PortOrCommand { port } => format!("port {port} or a command"),
        }),
        checksum_file: checks.baseline.then(|| "(configured)".to_string()),
        signature_required: checks.signature_required,
        host_keys_pinned: checks.host_keys_pinned,
        baseline_refreshed: d.baseline_refreshed.and_then(local_time),
        uptime: d.uptime_secs.map(crate::status::format_uptime),
        remote_baseline_date: d
            .remote_baseline
            .and_then(local_time)
            .map(|t| crate::status::format_time(&t)),
        remote_baseline_at: d.remote_baseline.and_then(local_time),
        state: target_state,
        unlockable: *unlockable,
        notes,
        checked_at: Local::now(),
        duration_ms: took.as_millis(),
    })
}

/// `user@host:port via jumphost`, through the helper; it says where the
/// connection goes, without the helper in the way of the reader.
fn route(d: &Details) -> String {
    let Some(host) = &d.host else {
        return "via privileged helper".into();
    };
    let mut route = format!(
        "{}@{host}:{}",
        d.user.as_deref().unwrap_or("?"),
        d.port.map_or_else(|| "?".into(), |p| p.to_string())
    );
    if let Some(jump) = &d.jumphost {
        route.push_str(" via ");
        route.push_str(jump);
    }
    route.push_str(" (privileged helper)");
    route
}

fn local_time(unix: i64) -> Option<chrono::DateTime<Local>> {
    chrono::DateTime::from_timestamp(unix, 0).map(|t| t.with_timezone(&Local))
}

fn check_notes(checks: &Checks) -> Vec<Note> {
    let mut notes = Vec::new();
    if !checks.baseline {
        notes.push(Note {
            severity: Severity::Warn,
            text: "no checksum baseline configured: the initrd cannot be verified, so only a forced unlock is possible".into(),
        });
    }
    if !checks.host_keys_pinned {
        notes.push(Note {
            severity: Severity::Warn,
            text: "initrd host keys are not pinned".into(),
        });
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checks(baseline: bool, integrity: Integrity, pinned: bool) -> Checks {
        Checks {
            baseline,
            integrity,
            host_keys_pinned: pinned,
            signature_required: false,
        }
    }

    fn probe(state: Observed, unlockable: bool, checks: Checks) -> Outcome {
        Outcome::Probe {
            state,
            unlockable,
            checks,
            details: None,
        }
    }

    #[test]
    fn a_verified_initrd_is_unlockable_in_the_report() {
        let r = report(
            "h.example",
            &probe(
                Observed::Initrd,
                true,
                checks(true, Integrity::Verified, true),
            ),
            Duration::from_millis(5),
        )
        .unwrap();
        assert!(r.unlockable && r.ok());
        assert!(matches!(r.state, TargetState::Initrd(ref d) if d == "checksum verified"));
        assert_eq!(r.hostname, "h.example");
        assert!(r.host_keys_pinned);
        assert!(r.checksum_file.is_some());
    }

    #[test]
    fn missing_protections_are_called_out() {
        let r = report(
            "h",
            &probe(
                Observed::Initrd,
                false,
                checks(false, Integrity::NotConfigured, false),
            ),
            Duration::ZERO,
        )
        .unwrap();
        assert!(!r.unlockable);
        assert!(r.checksum_file.is_none());
        let text: Vec<_> = r.notes.iter().map(|n| n.text.as_str()).collect();
        assert!(text.iter().any(|t| t.contains("no checksum baseline")));
        assert!(text.iter().any(|t| t.contains("not pinned")));
    }

    #[test]
    fn a_failed_check_is_not_unlockable_and_is_not_shown_as_ok() {
        let r = report(
            "h",
            &probe(
                Observed::ChecksumFailed,
                false,
                checks(true, Integrity::Failed, true),
            ),
            Duration::ZERO,
        )
        .unwrap();
        assert!(!r.ok());
        assert!(matches!(r.state, TargetState::Initrd(ref d) if d.contains("failed")));
    }

    #[test]
    fn every_observation_maps_to_a_state_and_a_text() {
        for o in [
            Observed::Booted,
            Observed::Unreachable,
            Observed::NotInitrd,
            Observed::SshFailed,
            Observed::Initrd,
            Observed::ChecksumFailed,
            Observed::UnlockFailed,
            Observed::Unlocked,
        ] {
            assert!(!state_text(daemon_state(o)).is_empty());
            assert!(
                report(
                    "h",
                    &probe(o, false, checks(true, Integrity::Verified, true)),
                    Duration::ZERO
                )
                .is_some()
            );
        }
    }

    #[test]
    fn details_from_the_helper_fill_the_report() {
        let outcome = Outcome::Probe {
            state: Observed::Booted,
            unlockable: false,
            checks: checks(true, Integrity::NotApplicable, true),
            details: Some(Details {
                user: Some("root".into()),
                host: Some("t.example".into()),
                port: Some(22),
                jumphost: Some("jump.example".into()),
                luks: Some(Luks::Systemd),
                healthcheck: Some(Health::Command),
                uptime_secs: Some(3 * 86400 + 3600),
                baseline_refreshed: Some(1_794_553_201),
                remote_baseline: Some(1_794_553_201),
            }),
        };
        let r = report("t.example", &outcome, Duration::ZERO).unwrap();
        assert_eq!(
            r.route,
            "root@t.example:22 via jump.example (privileged helper)"
        );
        assert_eq!(r.luks_type, "systemd");
        assert_eq!(r.healthcheck.as_deref(), Some("a command"));
        assert_eq!(r.uptime.as_deref(), Some("up 3 days, 1 hour"));
        assert!(r.baseline_refreshed.is_some() && r.remote_baseline_date.is_some());
        assert!(r.notes.iter().all(|n| !n.text.contains("older")));
        // An older helper sends no details: say so instead of making them up.
        let Outcome::Probe {
            state,
            unlockable,
            checks,
            ..
        } = outcome
        else {
            unreachable!()
        };
        let old = Outcome::Probe {
            state,
            unlockable,
            checks,
            details: None,
        };
        let r = report("t.example", &old, Duration::ZERO).unwrap();
        assert_eq!(r.route, "via privileged helper");
        assert!(
            r.notes
                .iter()
                .any(|n| n.text.contains("older privileged helper"))
        );
    }

    #[test]
    fn an_answer_that_is_not_a_probe_gives_no_report() {
        let other = Outcome::Service {
            action: crate::helper::request::ServiceAction::Stop,
            done: true,
        };
        assert!(report("h", &other, Duration::ZERO).is_none());
    }
}
