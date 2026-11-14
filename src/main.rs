//! luks-ssh-unlock: unlock LUKS-encrypted hosts by feeding their initrd the
//! passphrase over SSH, after making sure it is the initrd we expect.

mod checksum;
mod console;
mod ctx;
mod daemon;
mod dhcp;
mod elevate;
mod helper;
mod log;
mod notify;
mod oci;
mod openstack;
mod probe;
mod process;
mod settings;
mod status;
mod tui;
mod unlock;
mod vnc;
mod vnc_graphics;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{CommandFactory, Parser, Subcommand};

use crate::ctx::{Ctx, Exit, Mode};
use crate::log::Logger;
use crate::settings::{GlobalOpts, Settings};

/// Unlock LUKS-encrypted hosts over SSH.
///
/// Watches a host and, whenever it waits in its initrd, verifies that initrd
/// (host keys, optional checksum baseline and signature) and sends it the
/// passphrase. Every option can also be set through the environment variable
/// named in its help.
#[derive(Parser, Debug)]
#[command(
    name = "luks-ssh-unlock",
    version,
    propagate_version = true,
    max_term_width = 100
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Do not re-run with sudo when root is needed to read keys or
    /// passphrases [env: LUKS_SSH_UNLOCK_NO_SUDO]
    #[arg(long, global = true)]
    no_sudo: bool,

    #[command(flatten)]
    opts: GlobalOpts,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Watch the target and unlock it when it waits in its initrd (default)
    Run,
    /// Show the configuration and what the target is doing, without unlocking
    Status {
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Interactive dashboard of all targets
    #[command(visible_alias = "ui")]
    Tui {
        /// Environment files or directories of targets
        /// [default: /etc/luks-ssh-unlock/*.env, else the current options]
        files: Vec<PathBuf>,
        /// Seconds between two status checks of a target
        #[arg(long, default_value_t = 30, value_name = "SECONDS")]
        refresh: u64,
        /// Only show targets whose name or host contains this text
        /// (case-insensitive); `/` changes it in the dashboard
        #[arg(long, value_name = "TEXT")]
        filter: Option<String>,
        /// Show a command's output in a pane below the activity
        /// (repeatable; `c` switches). `{host}` and `{name}` are replaced by
        /// the selected target's host and name. Run with `sh -c` while the target is
        /// selected; the output is streamed.
        /// [default: /etc/luks-ssh-unlock/tui-commands]
        #[arg(long = "extra-command", value_name = "[LABEL=]COMMAND")]
        extra_commands: Vec<String>,
    },
    /// Connect to a provider's interactive console
    Console {
        #[command(subcommand)]
        provider: ConsoleProvider,
    },
    /// Print shell completions
    Completions { shell: clap_complete::Shell },
    /// Start a one-shot unlock when the target's initrd requests a DHCP lease
    #[command(hide = true)]
    DhcpListener(dhcp::DhcpArgs),
    /// Serve the privileged helper on the connection given as standard input
    /// (started by its systemd socket unit, never by hand)
    #[command(hide = true, subcommand)]
    Helper(HelperCmd),
}

#[derive(Subcommand, Debug)]
enum HelperCmd {
    /// Answer the requests of one connection
    Serve {
        /// What the connection may do: `basic` (list, probe, unlock) or
        /// `admin` (also forced unlock and service start and stop)
        #[arg(long, value_enum)]
        policy: HelperPolicy,
        /// The initrd checksum program for configurations that name none
        #[arg(long, value_name = "PATH")]
        checksum_script: Option<PathBuf>,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum HelperPolicy {
    Basic,
    Admin,
}

#[derive(Subcommand, Debug)]
enum ConsoleProvider {
    /// Connect to an OpenStack serial console
    Openstack(console::OpenstackSerialArgs),
    /// Connect to an Oracle Cloud Infrastructure serial console
    Oci {
        /// Instance display name or OCID
        #[arg(long)]
        instance: String,
        /// RSA private key (defaults to ~/.ssh/id_rsa)
        #[arg(long)]
        identity: Option<PathBuf>,
        /// OCI CLI configuration file
        #[arg(long)]
        config_file: Option<PathBuf>,
        /// OCI CLI defaults file
        #[arg(long)]
        cli_rc_file: Option<PathBuf>,
        /// OCI CLI profile
        #[arg(long)]
        profile: Option<String>,
        /// Compartment for instance name lookup
        #[arg(long)]
        compartment_id: Option<String>,
        /// Keep the console connection after disconnecting
        #[arg(long)]
        keep: bool,
    },
}

fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn context(opts: &GlobalOpts, mode: Mode) -> Result<Ctx> {
    let settings = Settings::resolve(opts, &process_env).map_err(|e| Exit(2, format!("{e:#}")))?;
    let log = Logger::stderr(settings.debug);
    if settings.passphrase.is_some() && opts.luks_passphrase.is_some() {
        log.warn(
            "Passing the LUKS passphrase on the command line exposes it to other users (ps, /proc/*/cmdline) and shell history; use --luks-passphrase-file instead",
        );
    }
    Ctx::new(settings, log, mode)
}

fn real_main() -> Result<bool> {
    let cli = Cli::parse();
    if matches!(cli.command, Some(Cmd::Console { .. } | Cmd::Helper(_))) {
        elevate::forbid();
    }
    match cli.command.unwrap_or(Cmd::Run) {
        Cmd::Run => {
            let mut ctx = context(&cli.opts, Mode::Run)?;
            if ctx.s.force && !ctx.s.once {
                bail!(Exit(2, "--force can only be used with --once".into()));
            }
            if ctx.s.once {
                ctx.run_once()
            } else {
                ctx.run_forever().map(|()| true)
            }
        }
        Cmd::Status { json } => {
            let mut ctx = context(&cli.opts, Mode::Status)?;
            ctx.force = false;
            let report = ctx.status(true)?;
            if json {
                let mut out = std::io::stdout().lock();
                serde_json::to_writer_pretty(&mut out, &report)?;
                writeln!(out)?;
            } else {
                status::print_text(&report);
            }
            Ok(report.ok())
        }
        Cmd::Tui {
            files,
            refresh,
            filter,
            extra_commands,
        } => {
            // The dashboard and the consoles it starts act as the invoking
            // user, with that user's keys and environment: root is neither
            // wanted nor accepted, and what needs it goes to the helper.
            elevate::forbid();
            if elevate::is_root() {
                bail!(Exit(
                    2,
                    "the dashboard must not run as root: run it as your own user, with the privileged helper enabled for the targets whose configuration is protected".into()
                ));
            }
            let mut sources = tui::discover(&files)?;
            if sources.is_empty()
                && (cli.opts.host.is_some() || process_env("SSH_HOSTNAME").is_some())
            {
                let name = cli
                    .opts
                    .host
                    .clone()
                    .or_else(|| process_env("SSH_HOSTNAME"))
                    .unwrap_or_default();
                sources.push(tui::Source {
                    name,
                    env: None,
                    extras: Vec::new(),
                    privileged: tui::Privileged::Never,
                });
            }
            let extras = if extra_commands.is_empty() {
                tui::default_extras()
            } else {
                extra_commands
                    .iter()
                    .map(|s| tui::ExtraCmd::parse(s))
                    .collect()
            };
            tui::run(
                sources,
                &cli.opts,
                Duration::from_secs(refresh.max(5)),
                extras,
                filter.unwrap_or_default(),
            )?;
            Ok(true)
        }
        Cmd::Console {
            provider:
                ConsoleProvider::Oci {
                    instance,
                    identity,
                    config_file,
                    cli_rc_file,
                    profile,
                    compartment_id,
                    keep,
                },
        } => {
            let code = oci::run(&oci::Options {
                instance,
                identity,
                config_file,
                cli_rc_file,
                profile,
                compartment_id,
                keep,
                ..Default::default()
            })?;
            if code != 0 {
                bail!(Exit(code, "OCI console ended unsuccessfully".into()));
            }
            Ok(true)
        }
        Cmd::Console {
            provider: ConsoleProvider::Openstack(args),
        } => {
            let code = console::run_openstack_serial(&args)?;
            if code != 0 {
                bail!(Exit(code, "OpenStack console ended unsuccessfully".into()));
            }
            Ok(true)
        }
        Cmd::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "luks-ssh-unlock",
                &mut std::io::stdout(),
            );
            Ok(true)
        }
        Cmd::Helper(HelperCmd::Serve {
            policy,
            checksum_script,
        }) => {
            let policy = match policy {
                HelperPolicy::Basic => helper::Policy::default(),
                HelperPolicy::Admin => helper::Policy {
                    allow_force: true,
                    allow_service: true,
                },
            };
            helper::server::run_stdio(policy, checksum_script)
                .map_err(|e| Exit(1, format!("helper: {e}")))?;
            Ok(true)
        }
        Cmd::DhcpListener(args) => {
            let port = match cli.opts.port.as_deref().map(str::parse::<u16>) {
                None => 22,
                Some(Ok(p)) => p,
                Some(Err(e)) => bail!(Exit(2, format!("--ssh-port: {e}"))),
            };
            dhcp::run(args, port, Logger::stderr(cli.opts.debug))?;
            Ok(true)
        }
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            let needs_root = elevate::needs_root(&e);
            if needs_root {
                // Only returns without a terminal, with --no-sudo, or when
                // sudo cannot be run.
                elevate::reexec();
            }
            let code = if let Some(Exit(code, msg)) = e.downcast_ref::<Exit>() {
                eprintln!("luks-ssh-unlock: {msg}");
                *code as u8
            } else {
                eprintln!("luks-ssh-unlock: {e:#}");
                1
            };
            if needs_root {
                eprintln!("luks-ssh-unlock: hint: run as root, for example with sudo");
            }
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn oci_console_invocation_parses() {
        let cli = Cli::try_parse_from([
            "luks-ssh-unlock",
            "console",
            "oci",
            "--instance",
            "example",
            "--identity",
            "/home/user/.ssh/id_rsa",
            "--profile",
            "DEFAULT",
            "--keep",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Console {
                provider: ConsoleProvider::Oci { keep: true, .. }
            })
        ));
    }

    #[test]
    fn openstack_console_invocation_parses() {
        let cli = Cli::try_parse_from([
            "luks-ssh-unlock",
            "console",
            "openstack",
            "--server",
            "example",
            "--cloud",
            "example-cloud",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Cmd::Console {
                provider: ConsoleProvider::Openstack(_)
            })
        ));
    }

    #[test]
    fn legacy_invocations_parse() {
        for argv in [
            vec!["luks-ssh-unlock"],
            vec!["luks-ssh-unlock", "run", "--once", "-f"],
            vec!["luks-ssh-unlock", "--once", "run"],
            vec![
                "luks-ssh-unlock",
                "-H",
                "h",
                "-u",
                "root",
                "-J",
                "jump",
                "--jport",
                "22",
                "-t",
                "systemd",
            ],
            vec![
                "luks-ssh-unlock",
                "status",
                "--hc-host",
                "x",
                "--rcmd",
                "true",
            ],
            vec![
                "luks-ssh-unlock",
                "--sleep-interval",
                "5",
                "--event-file",
                "/tmp/e",
                "--tag",
                "t",
            ],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
    }
}
