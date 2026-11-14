//! End-to-end unlock flows against a throwaway sshd on loopback.
//!
//! Every test starts its own unprivileged `sshd` (random 127.0.0.1 port,
//! throwaway host key) that plays an initrd, generates synthetic keys and uses
//! a synthetic passphrase, then runs the real `luks-ssh-unlock run --once`
//! binary against it. No live system, real key or real passphrase is involved;
//! see `fixture.rs` for how the fake initrd works.

#![allow(dead_code)]

mod fixture;

use fixture::{DEVICE, Fixture, Login, PASSPHRASE, Run, WRONG_PASSPHRASE, free_port, have_tool};

/// A failure message that shows what the tool said.
fn why(run: &Run, fx: &Fixture) -> String {
    format!(
        "exit {:?}\n--- stdout\n{}\n--- stderr\n{}\n--- remote commands\n{:#?}\n--- initrd sshd\n{}",
        run.code,
        run.stdout,
        run.stderr,
        fx.remote_commands(),
        fx.initrd.log_text()
    )
}

/// Neither secret may show up in the tool's output, the events file or any
/// command the fake initrd was asked to run.
fn assert_no_leak(fx: &Fixture, run: &Run) {
    let events = std::fs::read_to_string(fx.path().join("events")).unwrap_or_default();
    let commands = fx.remote_commands().join("\n");
    for secret in [PASSPHRASE, WRONG_PASSPHRASE] {
        assert!(
            !run.output().contains(secret),
            "passphrase leaked into the output:\n{}",
            why(run, fx)
        );
        assert!(!events.contains(secret), "passphrase leaked into events");
        assert!(
            !commands.contains(secret),
            "passphrase sent as part of a remote command:\n{commands}"
        );
    }
}

fn assert_unlocked(fx: &Fixture, run: &Run) {
    assert_eq!(run.code, Some(0), "{}", why(run, fx));
    assert_eq!(fx.attempts(), ["ok"], "{}", why(run, fx));
    assert!(fx.unlocked());
    assert_eq!(fx.events(), ["success"], "{}", why(run, fx));
    assert!(
        run.stderr.contains("Unlocked 127.0.0.1"),
        "{}",
        why(run, fx)
    );
    assert_no_leak(fx, run);
}

fn assert_refused(fx: &Fixture, run: &Run) {
    assert_eq!(run.code, Some(1), "{}", why(run, fx));
    assert!(fx.attempts().is_empty(), "{}", why(run, fx));
    assert!(!fx.unlocked());
    assert_no_leak(fx, run);
}

fn set_passphrase(fx: &Fixture, passphrase: &str) {
    fx.write("passphrase", 0o600, &format!("{passphrase}\n"));
}

// ---------------------------------------------------------------- methods

#[test]
fn raw_unlock() {
    let fx = Fixture::new();
    let run = fx.run_once(&[("DEBUG", "1")]);
    assert_unlocked(&fx, &run);
    // The probe, then a login without a command (what an initrd binds).
    assert_eq!(fx.remote_commands(), ["test -e /etc/initrd-release", ""]);
}

#[test]
fn luks_mount_unlock() {
    let fx = Fixture::new();
    let run = fx.run_once(&[("LUKS_TYPE", "luks-mount"), ("DEBUG", "1")]);
    assert_unlocked(&fx, &run);
    assert_eq!(
        fx.remote_commands(),
        ["test -e /etc/initrd-release", "luks-mount"]
    );
}

#[test]
fn systemd_tool_unlock() {
    let fx = Fixture::new();
    let run = fx.run_once(&[("LUKS_TYPE", "systemd-tool"), ("DEBUG", "1")]);
    assert_unlocked(&fx, &run);
    assert_eq!(fx.cryptsetup_calls(), [format!("luksOpen|{DEVICE}|root|-")]);
    assert!(fx.restarted(), "the cryptsetup unit was not restarted");
}

#[test]
fn systemd_tty_agent_unlock() {
    let fx = Fixture::new();
    let run = fx.run_once(&[("LUKS_TYPE", "systemd"), ("DEBUG", "1")]);
    assert_unlocked(&fx, &run);
    let commands = fx.remote_commands();
    assert!(
        commands
            .last()
            .is_some_and(|c| c.starts_with("stty -echo && printf")
                && c.ends_with("exec systemd-tty-ask-password-agent")),
        "unexpected remote command: {commands:?}"
    );
}

// ---------------------------------------------------------- wrong secrets

#[test]
fn wrong_passphrase_is_reported_but_never_leaked() {
    for method in ["raw", "luks-mount", "systemd-tool", "systemd"] {
        let fx = Fixture::new();
        set_passphrase(&fx, WRONG_PASSPHRASE);
        // The fake initrd echoes the rejected passphrase back at us.
        let run = fx.run_once(&[("LUKS_TYPE", method), ("DEBUG", "1")]);
        assert_eq!(run.code, Some(1), "{method}: {}", why(&run, &fx));
        assert_eq!(fx.attempts(), ["rejected"], "{method}: {}", why(&run, &fx));
        assert!(!fx.unlocked() && !fx.restarted(), "{method}");
        assert_eq!(fx.events(), ["failure"], "{method}: {}", why(&run, &fx));
        assert!(
            run.stderr.contains("Failed to unlock 127.0.0.1"),
            "{method}: {}",
            why(&run, &fx)
        );
        assert!(
            run.output().contains("[REDACTED]"),
            "{method}: the echoed passphrase was not redacted:\n{}",
            why(&run, &fx)
        );
        assert_no_leak(&fx, &run);
    }
}

#[test]
fn unusable_passphrase_file_is_refused_and_touches_nothing() {
    let fx = Fixture::new();
    let missing = fx.path().join("does-not-exist");
    let run = fx.run_once(&[("LUKS_PASSPHRASE_FILE", missing.to_str().unwrap())]);
    assert_eq!(run.code, Some(3), "{}", why(&run, &fx));
    assert!(fx.remote_commands().is_empty(), "{}", why(&run, &fx));
    assert!(fx.attempts().is_empty());

    let empty = fx.path().join("empty-passphrase");
    std::fs::write(&empty, "\n").unwrap();
    let run = fx.run_once(&[("LUKS_PASSPHRASE_FILE", empty.to_str().unwrap())]);
    assert_eq!(run.code, Some(2), "{}", why(&run, &fx));
    assert!(fx.remote_commands().is_empty(), "{}", why(&run, &fx));
    assert!(fx.attempts().is_empty());
}

// --------------------------------------------------------- state machine

#[test]
fn not_in_initrd_never_sends_the_passphrase() {
    let fx = Fixture::new();
    fx.set_initrd(false);
    let run = fx.run_once(&[]);
    assert_eq!(run.code, Some(0), "{}", why(&run, &fx));
    assert!(
        run.stderr.contains("not in its initrd"),
        "{}",
        why(&run, &fx)
    );
    assert!(fx.attempts().is_empty());
    assert_eq!(fx.remote_commands(), ["test -e /etc/initrd-release"]);
    assert!(fx.events().is_empty());
}

#[test]
fn healthcheck_passing_wins_over_the_initrd() {
    let fx = Fixture::new();
    fx.set_booted(true);
    let cmd = format!("test -e {}", fx.booted_flag().display());
    let run = fx.run_once(&[
        ("HEALTHCHECK_REMOTE_CMD", &cmd),
        ("SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE", "initrd"),
    ]);
    assert_eq!(run.code, Some(0), "{}", why(&run, &fx));
    assert!(run.stderr.contains("up and unlocked"), "{}", why(&run, &fx));
    assert!(fx.attempts().is_empty());
    // Only the healthcheck ran; the initrd probe was never needed.
    assert_eq!(fx.remote_commands().len(), 1, "{}", why(&run, &fx));
}

#[test]
fn failing_healthcheck_in_the_initrd_unlocks() {
    let fx = Fixture::new();
    fx.set_booted(false);
    let cmd = format!("test -e {}", fx.booted_flag().display());
    let run = fx.run_once(&[
        ("HEALTHCHECK_REMOTE_CMD", &cmd),
        ("SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE", "initrd"),
    ]);
    assert_unlocked(&fx, &run);
}

#[test]
fn unlock_then_boot_is_idempotent() {
    let fx = Fixture::new();
    let first = fx.run_once(&[]);
    assert_unlocked(&fx, &first);

    // The machine boots: no initrd any more, the healthcheck passes.
    fx.set_initrd(false);
    fx.set_booted(true);
    let cmd = format!("test -e {}", fx.booted_flag().display());
    let second = fx.run_once(&[
        ("HEALTHCHECK_REMOTE_CMD", &cmd),
        ("SSH_HEALTHCHECK_KNOWN_HOSTS_TYPE", "initrd"),
    ]);
    assert_eq!(second.code, Some(0), "{}", why(&second, &fx));
    assert_eq!(fx.attempts(), ["ok"], "the passphrase was sent twice");
}

#[test]
fn unreachable_target_is_reported() {
    let fx = Fixture::new();
    let port = free_port().to_string();
    let run = fx.run_once(&[("SSH_PORT", &port)]);
    assert_eq!(run.code, Some(1), "{}", why(&run, &fx));
    assert!(run.stderr.contains("not reachable"), "{}", why(&run, &fx));
    assert!(fx.remote_commands().is_empty());
}

#[test]
fn status_never_unlocks() {
    let fx = Fixture::new();
    let run = fx.run_args(&[], &["status"]);
    assert!(run.code.is_some(), "{}", why(&run, &fx));
    assert!(fx.attempts().is_empty(), "{}", why(&run, &fx));
    assert!(!fx.unlocked());
    assert_no_leak(&fx, &run);
}

// ----------------------------------------------------------- trust checks

#[test]
fn wrong_initrd_host_key_is_refused_before_authenticating() {
    let fx = Fixture::new();
    let pinned = format!("[127.0.0.1]:{} {}", fx.initrd.port, fx.regular_host_key);
    let run = fx.run_once(&[("SSH_INITRD_KNOWN_HOSTS", &pinned)]);
    assert_refused(&fx, &run);
    assert!(
        run.stderr.contains("Cannot tell whether"),
        "{}",
        why(&run, &fx)
    );
    assert_eq!(fx.events(), ["failure"]);
    assert!(
        !fx.initrd.log_text().contains("Accepted publickey"),
        "the client authenticated against an untrusted host key:\n{}",
        fx.initrd.log_text()
    );
    assert!(fx.remote_commands().is_empty());
}

#[test]
fn unauthorized_client_key_is_refused() {
    let fx = Fixture::with_login(Login::OtherKey);
    let run = fx.run_once(&[]);
    assert_refused(&fx, &run);
    assert!(
        run.stderr.contains("Cannot tell whether"),
        "{}",
        why(&run, &fx)
    );
    assert!(fx.remote_commands().is_empty());
}

#[test]
fn force_skips_host_key_pinning_but_not_key_authentication() {
    let fx = Fixture::new();
    let pinned = format!("[127.0.0.1]:{} {}", fx.initrd.port, fx.regular_host_key);
    let run = fx.run_once(&[("SSH_INITRD_KNOWN_HOSTS", &pinned), ("FORCE", "1")]);
    assert_unlocked(&fx, &run);
    assert!(run.stderr.contains("Forced unlock"), "{}", why(&run, &fx));

    let fx = Fixture::with_login(Login::OtherKey);
    let run = fx.run_once(&[("FORCE", "1")]);
    assert_refused(&fx, &run);
}

// ------------------------------------------------ checksum and signature

/// A stand-in for the initrd-checksum helper that keeps its contract: it is
/// called as `checksum --quiet ... --diff BASELINE`, exits 0 when the initrd
/// matches and prints a diff and exits 1 when it does not. "The initrd" is
/// the content of `current-initrd`.
fn install_checksum_script(fx: &Fixture) -> String {
    let d = fx.path().display();
    let script = fx.write(
        "initrd-checksum",
        0o700,
        &format!(
            r#"#!/bin/sh
printf '%s\n' "$@" > '{d}/state/checksum-argv'
while [ $# -gt 0 ]; do
    if [ "$1" = --diff ]; then base=$2; break; fi
    shift
done
[ -n "$base" ] || {{ echo "ERR no baseline" >&2; exit 2; }}
if [ "$(cat "$base")" = "$(cat '{d}/current-initrd')" ]; then
    exit 0
fi
echo "-baseline-entry"
echo "+current-entry"
exit 1
"#
        ),
    );
    script.display().to_string()
}

fn checksum_argv(fx: &Fixture) -> Vec<String> {
    std::fs::read_to_string(fx.path().join("state/checksum-argv"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn matching_baseline_unlocks_and_gets_the_right_arguments() {
    let fx = Fixture::new();
    let script = install_checksum_script(&fx);
    fx.write("current-initrd", 0o600, "initrd-v1");
    let baseline = fx.write("baseline", 0o600, "initrd-v1");
    let run = fx.run_once(&[
        ("INITRD_CHECKSUM_SCRIPT", &script),
        ("INITRD_CHECKSUM_FILE", baseline.to_str().unwrap()),
    ]);
    assert_unlocked(&fx, &run);
    let argv = checksum_argv(&fx);
    let has = |s: &str| argv.iter().any(|a| a == s);
    assert!(has("checksum") && has("--quiet"), "{argv:?}");
    assert!(has("--host") && has("127.0.0.1"), "{argv:?}");
    assert!(
        has("--identity") && has(fx.client_key.to_str().unwrap()),
        "{argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--diff" && w[1] == baseline.to_str().unwrap()),
        "{argv:?}"
    );
    assert!(!argv.join("\n").contains(PASSPHRASE));
}

#[test]
fn changed_initrd_is_not_unlocked() {
    let fx = Fixture::new();
    let script = install_checksum_script(&fx);
    fx.write("current-initrd", 0o600, "initrd-v2-tampered");
    let baseline = fx.write("baseline", 0o600, "initrd-v1");
    let run = fx.run_once(&[
        ("INITRD_CHECKSUM_SCRIPT", &script),
        ("INITRD_CHECKSUM_FILE", baseline.to_str().unwrap()),
    ]);
    assert_refused(&fx, &run);
    assert!(
        run.stderr
            .contains("differs from its baseline (2 changed entries)"),
        "{}",
        why(&run, &fx)
    );
    assert_eq!(fx.events(), ["warning"]);
    // The initrd was probed (to know it is the initrd), but never logged into
    // for the unlock itself.
    assert_eq!(fx.remote_commands(), ["test -e /etc/initrd-release"]);
}

#[test]
fn missing_baseline_is_not_unlocked() {
    let fx = Fixture::new();
    let script = install_checksum_script(&fx);
    let baseline = fx.path().join("never-fetched");
    let run = fx.run_once(&[
        ("INITRD_CHECKSUM_SCRIPT", &script),
        ("INITRD_CHECKSUM_FILE", baseline.to_str().unwrap()),
    ]);
    assert_refused(&fx, &run);
    assert!(run.stderr.contains("not readable"), "{}", why(&run, &fx));
}

#[test]
fn force_skips_the_checksum_gate() {
    let fx = Fixture::new();
    let script = install_checksum_script(&fx);
    fx.write("current-initrd", 0o600, "initrd-v2-tampered");
    let baseline = fx.write("baseline", 0o600, "initrd-v1");
    let run = fx.run_once(&[
        ("INITRD_CHECKSUM_SCRIPT", &script),
        ("INITRD_CHECKSUM_FILE", baseline.to_str().unwrap()),
        ("FORCE", "1"),
    ]);
    assert_unlocked(&fx, &run);
    assert!(
        checksum_argv(&fx).is_empty(),
        "the helper ran despite --force"
    );
}

/// A signed baseline: the booted target signs its baseline with its regular
/// host key; the unlocker verifies it against the regular known_hosts.
fn signed_setup(fx: &Fixture) -> (String, std::path::PathBuf, String) {
    let script = install_checksum_script(fx);
    fx.write("current-initrd", 0o600, "initrd-v1");
    let baseline = fx.write("baseline", 0o600, "initrd-v1");
    fx.sign_baseline(&baseline);
    let known_hosts = format!("127.0.0.1 {}", fx.regular_host_key);
    (script, baseline, known_hosts)
}

#[test]
fn valid_signature_unlocks() {
    let fx = Fixture::new();
    let (script, baseline, kh) = signed_setup(&fx);
    let run = fx.run_once(&[
        ("INITRD_CHECKSUM_SCRIPT", &script),
        ("INITRD_CHECKSUM_FILE", baseline.to_str().unwrap()),
        ("INITRD_CHECKSUM_REQUIRE_SIGNATURE", "1"),
        ("SSH_KNOWN_HOSTS", &kh),
    ]);
    assert_unlocked(&fx, &run);
}

#[test]
fn tampered_baseline_fails_its_signature() {
    let fx = Fixture::new();
    let (script, baseline, kh) = signed_setup(&fx);
    // Baseline and "current initrd" agree, but the baseline was changed after
    // it was signed.
    fx.write("baseline", 0o600, "initrd-v1-evil");
    fx.write("current-initrd", 0o600, "initrd-v1-evil");
    let run = fx.run_once(&[
        ("INITRD_CHECKSUM_SCRIPT", &script),
        ("INITRD_CHECKSUM_FILE", baseline.to_str().unwrap()),
        ("SSH_KNOWN_HOSTS", &kh),
    ]);
    assert_refused(&fx, &run);
    assert!(
        run.stderr.contains("signature verification FAILED"),
        "{}",
        why(&run, &fx)
    );
    assert!(checksum_argv(&fx).is_empty());
}

#[test]
fn signature_from_the_wrong_key_is_refused() {
    let fx = Fixture::new();
    let (script, baseline, _) = signed_setup(&fx);
    // known_hosts pins a different key than the one that signed.
    let kh = format!("127.0.0.1 {}", fx.initrd.host_key);
    let run = fx.run_once(&[
        ("INITRD_CHECKSUM_SCRIPT", &script),
        ("INITRD_CHECKSUM_FILE", baseline.to_str().unwrap()),
        ("SSH_KNOWN_HOSTS", &kh),
    ]);
    assert_refused(&fx, &run);
    assert!(
        run.stderr.contains("signature verification FAILED"),
        "{}",
        why(&run, &fx)
    );
}

#[test]
fn missing_signature_depends_on_require_signature() {
    let fx = Fixture::new();
    let (script, baseline, kh) = signed_setup(&fx);
    std::fs::remove_file(format!("{}.sig", baseline.display())).unwrap();
    let env = [
        ("INITRD_CHECKSUM_SCRIPT", script.as_str()),
        ("INITRD_CHECKSUM_FILE", baseline.to_str().unwrap()),
        ("SSH_KNOWN_HOSTS", kh.as_str()),
    ];

    let mut strict = env.to_vec();
    strict.push(("INITRD_CHECKSUM_REQUIRE_SIGNATURE", "1"));
    let run = fx.run_once(&strict);
    assert_refused(&fx, &run);
    assert!(
        run.stderr.contains("signature required"),
        "{}",
        why(&run, &fx)
    );

    let run = fx.run_once(&env);
    assert_eq!(run.code, Some(0), "{}", why(&run, &fx));
    assert_eq!(fx.attempts(), ["ok"], "{}", why(&run, &fx));
    assert_eq!(fx.events(), ["warning", "success"], "{}", why(&run, &fx));
    assert_no_leak(&fx, &run);
    assert!(
        run.stderr.contains("skipping signature verification"),
        "{}",
        why(&run, &fx)
    );
}

// --------------------------------------------------------------- jumphost

fn jumphost_env(fx: &Fixture, jump: &fixture::Sshd) -> (String, String, String) {
    let pinned = format!(
        "{}\n{}",
        fx.initrd.known_hosts_line(),
        jump.known_hosts_line()
    );
    (pinned, jump.port.to_string(), fx.user.clone())
}

#[test]
fn unlock_through_a_jumphost() {
    let fx = Fixture::new();
    let jump = fx.jumphost();
    let (pinned, port, user) = jumphost_env(&fx, &jump);
    let run = fx.run_once(&[
        ("SSH_JUMPHOST", "127.0.0.1"),
        ("SSH_JUMPHOST_PORT", &port),
        ("SSH_JUMPHOST_USERNAME", &user),
        ("SSH_INITRD_KNOWN_HOSTS", &pinned),
        // The port check runs `nc` on the jumphost; see the next test.
        ("SKIP_SSH_PORT_CHECK", "1"),
    ]);
    assert_unlocked(&fx, &run);
    assert!(
        jump.log_text().contains("Accepted publickey"),
        "the unlock did not go through the jumphost:\n{}",
        jump.log_text()
    );
}

#[test]
fn jumphost_port_check_uses_nc_on_the_jumphost() {
    if !have_tool("nc") {
        eprintln!("skipped: no `nc` on PATH (the SSH port check runs it on the jumphost)");
        return;
    }
    let fx = Fixture::new();
    let jump = fx.jumphost();
    let (pinned, port, user) = jumphost_env(&fx, &jump);
    let run = fx.run_once(&[
        ("SSH_JUMPHOST", "127.0.0.1"),
        ("SSH_JUMPHOST_PORT", &port),
        ("SSH_JUMPHOST_USERNAME", &user),
        ("SSH_INITRD_KNOWN_HOSTS", &pinned),
    ]);
    assert_unlocked(&fx, &run);
}

#[test]
fn jumphost_with_an_unpinned_host_key_is_refused() {
    let fx = Fixture::new();
    let jump = fx.jumphost();
    let (_, port, user) = jumphost_env(&fx, &jump);
    // Only the initrd is pinned; the jumphost's key is unknown.
    let run = fx.run_once(&[
        ("SSH_JUMPHOST", "127.0.0.1"),
        ("SSH_JUMPHOST_PORT", &port),
        ("SSH_JUMPHOST_USERNAME", &user),
        ("SKIP_SSH_PORT_CHECK", "1"),
    ]);
    assert_refused(&fx, &run);
    assert!(
        !jump.log_text().contains("Accepted publickey"),
        "authenticated to a jumphost whose host key is not pinned"
    );
}
