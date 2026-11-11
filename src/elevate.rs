//! Re-run under sudo when an interactive invocation lacks the permissions to
//! read the target configuration, SSH keys or passphrase files.
//!
//! Never done without a terminal (services, scripts and CI keep their exact
//! behavior and exit status), and it can be turned off with `--no-sudo` or
//! `LUKS_SSH_UNLOCK_NO_SUDO=1`.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use clap::CommandFactory;

use crate::ctx::Exit;

pub fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn interactive() -> bool {
    unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 }
}

fn disabled() -> bool {
    std::env::args_os().any(|a| a == "--no-sudo")
        || std::env::var("LUKS_SSH_UNLOCK_NO_SUDO").is_ok_and(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
}

/// Does this error come from missing permissions that root would have?
pub fn needs_root(e: &anyhow::Error) -> bool {
    if is_root() {
        return false;
    }
    if let Some(Exit(_, msg)) = e.downcast_ref::<Exit>() {
        return msg.contains("not readable");
    }
    e.chain().any(|c| {
        c.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
    })
}

/// The executable as the user started it (the Nix wrapper that sets PATH,
/// not the wrapped binary), made absolute because sudo resets PATH.
fn self_path() -> Option<PathBuf> {
    let arg0 = PathBuf::from(std::env::args_os().next()?);
    let found = if arg0.components().count() > 1 {
        Some(arg0)
    } else {
        std::env::var_os("PATH").and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(&arg0))
                .find(|c| c.is_file())
        })
    };
    found
        .and_then(|p| p.canonicalize().ok())
        .or_else(|| std::env::current_exe().ok())
}

/// Names from `[env: A, B, default: x]` in an option's help text.
fn parse_env_names(help: &str) -> Vec<String> {
    let Some(rest) = help.split("[env: ").nth(1) else {
        return Vec::new();
    };
    rest.split(']')
        .next()
        .unwrap_or_default()
        .split(", ")
        .take_while(|p| !p.contains(':') && !p.contains(' '))
        .map(str::to_string)
        .collect()
}

/// The documented settings variables that are set, so that sudo keeps them
/// (by name: values never appear in the command line).
fn env_to_keep() -> Vec<String> {
    let mut names: Vec<String> = crate::Cli::command()
        .get_arguments()
        .filter_map(|a| a.get_help().map(|h| parse_env_names(&h.to_string())))
        .flatten()
        .filter(|n| std::env::var_os(n).is_some())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Replace this process with `sudo luks-ssh-unlock <same arguments>`.
/// Returns only when that is not possible or not wanted.
pub fn reexec() {
    if is_root() || disabled() || !interactive() {
        return;
    }
    let Some(exe) = self_path() else { return };
    let mut cmd = Command::new("sudo");
    let keep = env_to_keep();
    if !keep.is_empty() {
        cmd.arg(format!("--preserve-env={}", keep.join(",")));
    }
    cmd.arg("--").arg(exe).args(std::env::args_os().skip(1));
    let err = cmd.exec();
    eprintln!("luks-ssh-unlock: cannot run sudo: {err}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_env_names() {
        assert_eq!(
            parse_env_names("Seconds between checks [env: SLEEP_INTERVAL, default: 10]"),
            ["SLEEP_INTERVAL"]
        );
        assert_eq!(parse_env_names("Host [env: A, B]"), ["A", "B"]);
        assert!(parse_env_names("No variable").is_empty());
    }

    #[test]
    fn every_option_documents_its_env() {
        // The sudo hand-over relies on the help texts naming the variables.
        let all: Vec<String> = crate::Cli::command()
            .get_arguments()
            .filter_map(|a| a.get_help().map(|h| parse_env_names(&h.to_string())))
            .flatten()
            .collect();
        assert!(all.contains(&"SSH_HOSTNAME".to_string()));
    }

    #[test]
    fn permission_errors_need_root() {
        if is_root() {
            return;
        }
        let denied = anyhow::Error::new(io::Error::from(io::ErrorKind::PermissionDenied));
        assert!(needs_root(&denied));
        assert!(needs_root(&anyhow::Error::new(Exit(
            2,
            "SSH key /k is not readable: denied".into()
        ))));
        assert!(!needs_root(&anyhow::anyhow!("something else")));
    }
}
