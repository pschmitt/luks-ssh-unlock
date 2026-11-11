//! The unlock loop: check the target, and unlock it when it waits in its
//! initrd.

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;

use crate::checksum::ChecksumResult;
use crate::ctx::Ctx;
use crate::log::Level;
use crate::notify::Event;
use crate::probe::InitrdProbe;
use crate::process::is_timeout;

/// What a check found. Repeated states are logged once, so a host that stays
/// up (or down) does not fill the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    /// Booted and unlocked: the healthcheck passed.
    Booted,
    /// The SSH port does not answer.
    Unreachable,
    /// SSH answers, but this is not the initrd (still booting/shutting down).
    NotInitrd,
    /// SSH login failed; cannot tell whether it is the initrd.
    InitrdSshFailed,
    /// Waiting in the initrd for its passphrase.
    Initrd,
    /// The initrd does not match its baseline; not unlocking.
    ChecksumFailed,
    /// Sending the passphrase failed.
    UnlockFailed,
    /// The passphrase was accepted.
    Unlocked,
}

impl State {
    /// One-shot exit status: is there nothing (left) to do?
    pub fn ok(self) -> bool {
        matches!(self, Self::Booted | Self::NotInitrd | Self::Unlocked)
    }
}

impl Ctx {
    fn retry_hint(&self) -> String {
        if self.s.once {
            String::new()
        } else {
            format!(" Retrying every {}s.", self.s.interval)
        }
    }

    /// Record STATE and log MESSAGE unless the state did not change (long
    /// running mode only). With EVENT, also notify.
    fn transition(&mut self, state: State, level: Level, event: Option<Event>, message: &str) {
        if self.state == Some(state) && !self.s.once {
            self.log.debug(format!("{message} (unchanged)"));
            return;
        }
        self.state = Some(state);
        match event {
            Some(event) => self.notify(event, message),
            None => self.log.log(level, message),
        }
    }

    /// One check of the target, unlocking it if it waits in its initrd. A
    /// failed healthcheck alone never triggers an unlock: the initrd must be
    /// confirmed by `/etc/initrd-release` first.
    pub fn tick(&mut self) -> Result<State> {
        self.start_check();
        self.pin_connect_address()?;
        let host = self.s.hostname.clone();

        if self.healthcheck()? {
            self.fetch_initrd_checksum()?;
            self.transition(
                State::Booted,
                Level::Info,
                None,
                &format!("{host} is up and unlocked (healthcheck passed); nothing to do"),
            );
            return Ok(State::Booted);
        }

        if !self.s.skip_port_check && !self.check_ssh_port()? {
            let msg = format!(
                "{host} is down: SSH on {} is not reachable.{}",
                self.s.route(),
                self.retry_hint()
            );
            self.transition(State::Unreachable, Level::Warning, None, &msg);
            return Ok(State::Unreachable);
        }

        match self.is_initrd()? {
            InitrdProbe::No => {
                let msg = format!(
                    "{host} answers on SSH but is not in its initrd (no /etc/initrd-release) and the healthcheck failed. Not unlocking; is it still booting or shutting down?"
                );
                self.transition(State::NotInitrd, Level::Warning, None, &msg);
                return Ok(State::NotInitrd);
            }
            InitrdProbe::Unknown(err) => {
                let err = err.trim_end_matches('.');
                let msg = format!(
                    "Cannot tell whether {host} waits for its LUKS passphrase: SSH login to {} failed: {err}. If it is in its initrd, check that this unlocker's key and the initrd host keys are set up for the initrd SSH server.",
                    self.s.route()
                );
                self.transition(
                    State::InitrdSshFailed,
                    Level::Error,
                    Some(Event::Failure),
                    &msg,
                );
                return Ok(State::InitrdSshFailed);
            }
            InitrdProbe::Yes => {}
        }

        // Keep a failure as the current state while we retry, so a host that
        // keeps failing is reported once, not on every check.
        if !matches!(
            self.state,
            Some(State::UnlockFailed | State::ChecksumFailed)
        ) {
            self.transition(
                State::Initrd,
                Level::Info,
                None,
                &format!("{host} is waiting in its initrd for the LUKS passphrase"),
            );
        }

        if self.force {
            self.log.warn(format!(
                "Forced unlock: skipping the initrd checksum/signature and SSH host-key checks for {host} (client-key authentication still applies)"
            ));
        } else if let ChecksumResult::Failed(reason) = self.check_initrd_checksum()? {
            let msg = format!("Not unlocking {host}: {reason}.{}", self.retry_hint());
            self.transition(
                State::ChecksumFailed,
                Level::Warning,
                Some(Event::Warning),
                &msg,
            );
            return Ok(State::ChecksumFailed);
        }

        self.log.info(format!(
            "Sending the LUKS passphrase to {host} (method: {})",
            self.s.luks_type.as_str()
        ));
        match self.unlock() {
            Ok(()) => {
                let msg = format!("Unlocked {host}; it should finish booting shortly");
                self.transition(State::Unlocked, Level::Info, Some(Event::Success), &msg);
                Ok(State::Unlocked)
            }
            Err(e) if is_timeout(&e) => Err(e),
            Err(e) => {
                let msg = format!(
                    "Failed to unlock {host} (method: {}): {e:#}.{}",
                    self.s.luks_type.as_str(),
                    self.retry_hint()
                );
                self.transition(
                    State::UnlockFailed,
                    Level::Error,
                    Some(Event::Failure),
                    &msg,
                );
                Ok(State::UnlockFailed)
            }
        }
    }

    /// One check; timeouts and errors become log messages.
    pub fn tick_logged(&mut self) -> Option<State> {
        match self.tick() {
            Ok(state) => Some(state),
            Err(e) if is_timeout(&e) => {
                self.log.warn(format!(
                    "Check of {} took longer than {}s; aborted it",
                    self.s.hostname, self.s.tick_timeout
                ));
                None
            }
            Err(e) => {
                self.log
                    .error(format!("Check of {} failed: {e:#}", self.s.hostname));
                None
            }
        }
    }

    /// `run --once`: one attempt, exit status tells whether it succeeded.
    pub fn run_once(&mut self) -> Result<bool> {
        self.load_passphrase()?;
        self.log.info(format!(
            "Checking {} once ({}, LUKS method: {})",
            self.s.hostname,
            self.s.route(),
            self.s.luks_type.as_str()
        ));
        Ok(self.tick_logged().is_some_and(State::ok))
    }

    /// `run`: watch the target forever.
    pub fn run_forever(&mut self) -> Result<()> {
        self.load_passphrase()?;
        self.log.info(format!(
            "Watching {} ({}, LUKS method: {}), checking every {}s",
            self.s.hostname,
            self.s.route(),
            self.s.luks_type.as_str(),
            self.s.interval
        ));
        loop {
            self.tick_logged();
            std::thread::sleep(Duration::from_secs(self.s.interval));
        }
    }
}
