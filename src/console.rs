//! Typed configuration for a target's console.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::tui::ExtraCmd;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Console {
    Command { parameters: CommandParameters },
    Oci { parameters: OciParameters },
    Openstack { parameters: serde_json::Value },
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
}

pub fn read(path: &Path, target: &str) -> Result<Option<ExtraCmd>> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
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
            let _ = parameters;
            bail!("OpenStack consoles are not supported yet")
        }
    };
    if command.trim().is_empty() {
        bail!("console command cannot be empty");
    }
    Ok(Some(ExtraCmd {
        label: "console".into(),
        command,
        interactive,
    }))
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
        let command = read(&path, "target").unwrap().unwrap();
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
        std::fs::write(&path, r#"{"type":"openstack","parameters":{}}"#).unwrap();
        assert!(read(&path, "target").is_err());
    }

    #[test]
    fn oci_configuration_builds_interactive_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target.console.json");
        std::fs::write(&path, r#"{"type":"oci","parameters":{"identity":"/path with spaces/key","profile":"profile","keep":true}}"#).unwrap();
        let command = read(&path, "target").unwrap().unwrap();
        assert!(command.interactive);
        assert!(command.command.contains("--instance"));
        assert!(command.command.contains("target"));
        assert!(command.command.contains("'/path with spaces/key'"));
        assert!(command.command.contains("--profile"));
        assert!(command.command.contains("--keep"));
        std::fs::write(
            &path,
            r#"{"type":"oci","parameters":{"instance":"{name}","profile":"{host}"}}"#,
        )
        .unwrap();
        let command = read(&path, "target").unwrap().unwrap();
        assert!(!command.command.contains("{name}"));
        assert!(!command.command.contains("{host}"));
    }
}
