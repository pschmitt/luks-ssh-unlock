//! What the helper says back: closed types that serialise to a fixed shape.
//!
//! Nothing here carries a string that came from a configuration file, a
//! command, a transport or the target: a passphrase can arrive from any of
//! those (a file named by the configuration, an error from `ssh`), and
//! removing it from free text afterwards cannot be made reliable. So a result
//! is built from enums and booleans only, and a failure is one of a few
//! [`Failure`] kinds with a fixed message.

use serde::{Deserialize, Serialize};

use super::request::{ErrorCode, ServiceAction};

/// What a target was seen doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Observed {
    /// Booted and answering its healthcheck.
    Booted,
    Unreachable,
    /// SSH answers but it is not the initrd.
    NotInitrd,
    /// In the initrd, but the SSH login failed.
    SshFailed,
    /// In the initrd and fit to be unlocked.
    Initrd,
    /// In the initrd, but it does not match its baseline.
    ChecksumFailed,
    /// Unlock was attempted and did not succeed.
    UnlockFailed,
    Unlocked,
}

/// The state of the integrity check of the initrd.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Integrity {
    /// The target was not in its initrd, or the check was skipped on purpose.
    NotApplicable,
    /// No baseline is configured, so nothing was verified.
    NotConfigured,
    Verified,
    Failed,
}

/// Which protections were in force for the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checks {
    /// An initrd checksum baseline is configured.
    pub baseline: bool,
    pub integrity: Integrity,
    /// Host keys are pinned for both the booted system and the initrd, and
    /// were enforced.
    pub host_keys_pinned: bool,
    /// A valid signature over the baseline is mandatory.
    pub signature_required: bool,
}

/// How a target is set up and what a probe found out about it, for the
/// dashboard's details. Values only, as everything here: names that pass the
/// same filter as `ssh` arguments, enums, numbers and times. No healthcheck
/// command, no text the target printed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Details {
    pub user: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub jumphost: Option<String>,
    pub luks: Option<Luks>,
    pub healthcheck: Option<Health>,
    /// How long the target has been up, in seconds.
    pub uptime_secs: Option<u64>,
    /// When the local checksum baseline was last written (Unix time).
    pub baseline_refreshed: Option<i64>,
    /// The date of the baseline on the target (Unix time).
    pub remote_baseline: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Luks {
    Raw,
    Systemd,
    SystemdTool,
    #[serde(rename = "luks-mount")]
    Mount,
}

/// How the booted system is recognised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "kebab-case")]
pub enum Health {
    Port { port: u16 },
    Command,
    PortOrCommand { port: u16 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Probe {
        state: Observed,
        unlockable: bool,
        checks: Checks,
        /// Absent from an older helper.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Details>,
    },
    Unlock {
        state: Observed,
        unlocked: bool,
        /// The integrity and host-key checks were skipped on request.
        forced: bool,
        checks: Checks,
    },
    Service {
        action: ServiceAction,
        done: bool,
    },
}

/// Why an operation did not produce an [`Outcome`]. Deliberately coarse: it
/// says which stage stopped, never what the stage printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The configuration names something that is not safe to use.
    Config,
    /// The helper's own environment is not one it will run in.
    Environment,
    /// The passphrase could not be loaded.
    Passphrase,
    /// A plain unlock needs a baseline and pinned initrd host keys, and the
    /// configuration has not got both. Only a forced unlock is possible.
    Unverified,
    /// The operation ran out of time.
    Timeout,
    /// The operation failed.
    Operation,
}

impl Failure {
    pub fn code(self) -> ErrorCode {
        match self {
            Self::Config => ErrorCode::UntrustedConfig,
            Self::Environment => ErrorCode::Internal,
            Self::Passphrase | Self::Operation => ErrorCode::Failed,
            Self::Unverified => ErrorCode::Denied,
            Self::Timeout => ErrorCode::Timeout,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::Config => "the target's configuration refers to something that is not trusted",
            Self::Environment => "the helper's environment is not trusted",
            Self::Passphrase => "the passphrase could not be loaded",
            Self::Unverified => {
                "the target has no baseline or no pinned initrd host keys; only a forced unlock is possible"
            }
            Self::Timeout => "the operation timed out",
            Self::Operation => "the operation failed",
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Failure {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn outcomes_have_a_fixed_shape() {
        let checks = Checks {
            baseline: true,
            integrity: Integrity::Verified,
            host_keys_pinned: true,
            signature_required: false,
        };
        let probe = Outcome::Probe {
            state: Observed::ChecksumFailed,
            unlockable: false,
            checks,
            details: None,
        };
        assert_eq!(
            serde_json::to_value(probe).unwrap(),
            json!({
                "kind": "probe",
                "state": "checksum-failed",
                "unlockable": false,
                "checks": {
                    "baseline": true,
                    "integrity": "verified",
                    "host_keys_pinned": true,
                    "signature_required": false,
                },
            })
        );
        let service = Outcome::Service {
            action: ServiceAction::Stop,
            done: true,
        };
        assert_eq!(
            serde_json::to_value(service).unwrap(),
            json!({"kind": "service", "action": "stop", "done": true})
        );
    }

    #[test]
    fn failures_map_to_fixed_codes_and_messages() {
        for failure in [
            Failure::Config,
            Failure::Environment,
            Failure::Passphrase,
            Failure::Unverified,
            Failure::Timeout,
            Failure::Operation,
        ] {
            assert!(!failure.message().is_empty());
            assert_eq!(failure.to_string(), failure.message());
        }
        assert_eq!(Failure::Config.code(), ErrorCode::UntrustedConfig);
        assert_eq!(Failure::Timeout.code(), ErrorCode::Timeout);
        assert_eq!(Failure::Operation.code(), ErrorCode::Failed);
        assert_eq!(Failure::Passphrase.code(), ErrorCode::Failed);
        assert_eq!(Failure::Unverified.code(), ErrorCode::Denied);
        assert_eq!(Failure::Environment.code(), ErrorCode::Internal);
    }
}
