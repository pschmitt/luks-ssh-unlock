//! Initrd integrity: verify the running initrd against a stored baseline
//! before sending the passphrase, and refresh that baseline from the booted
//! target.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::ctx::Ctx;
use crate::process::last_line;
use crate::settings::KnownHostsKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChecksumResult {
    NotConfigured,
    /// Configured, but not run (status probes).
    Skipped,
    Passed,
    Failed(String),
}

impl Ctx {
    fn signature_file(&self, baseline: &Path) -> PathBuf {
        let mut name = baseline.as_os_str().to_owned();
        name.push(".sig");
        PathBuf::from(name)
    }

    /// Verify the `ssh-keygen -Y` signature the target made over its own
    /// baseline with its regular host key. Err is a reason to refuse.
    fn verify_signature(&self, baseline: &Path) -> Result<std::result::Result<(), String>> {
        let required = self.s.initrd.require_signature;
        let skip = |why: String| -> std::result::Result<(), String> {
            if required {
                Err(format!("{why} (signature required)"))
            } else {
                self.log
                    .warn(format!("{why}; skipping signature verification"));
                Ok(())
            }
        };

        let Some(signers) = self.known_hosts_path(KnownHostsKind::Default) else {
            return Ok(skip(format!(
                "no known_hosts configured for {}, cannot verify the baseline signature",
                self.s.hostname
            )));
        };
        let sig = self.signature_file(baseline);
        if fs::File::open(&sig).is_err() {
            return Ok(skip(format!(
                "baseline signature {} is missing",
                sig.display()
            )));
        }
        let data = fs::read(baseline)?;
        let mut cmd = Command::new("ssh-keygen");
        cmd.args(["-Y", "verify", "-f"])
            .arg(signers)
            .args(["-I", &self.s.hostname, "-n", "initrd-checksum", "-s"])
            .arg(&sig);
        let out = self.exec(cmd, Some(zeroize::Zeroizing::new(data)))?;
        if !out.success() {
            return Ok(Err(format!(
                "baseline signature verification FAILED ({}); the baseline may have been tampered with",
                out.error_line().unwrap_or_else(|| "no output".into())
            )));
        }
        self.log.debug(format!(
            "Baseline signature verified for {}",
            self.s.hostname
        ));
        Ok(Ok(()))
    }

    /// Compare the initrd the target is running with the stored baseline.
    pub fn check_initrd_checksum(&self) -> Result<ChecksumResult> {
        let Some(baseline) = self.s.initrd.file.clone() else {
            return Ok(ChecksumResult::NotConfigured);
        };
        if fs::File::open(&baseline).is_err() {
            return Ok(ChecksumResult::Failed(format!(
                "baseline {} is not readable (has it been fetched from the booted host yet?)",
                baseline.display()
            )));
        }
        self.log.info(format!(
            "Verifying the initrd of {} against its baseline",
            self.s.hostname
        ));

        if let Err(why) = self.verify_signature(&baseline)? {
            return Ok(ChecksumResult::Failed(why));
        }

        let script = &self.s.initrd.script;
        let mut cmd = Command::new(script);
        cmd.args(["checksum", "--quiet"])
            .args(self.checksum_ssh_args())
            .arg("--diff")
            .arg(&baseline);
        if self.s.initrd.paranoid {
            cmd.arg("--paranoid");
        }
        if self.log.debug_enabled() {
            cmd.arg("--debug");
        }
        if let Some(dir) = &self.s.initrd.busybox_dir {
            cmd.env("INITRD_CHECKSUM_BUSYBOX_DIR", dir);
        }
        let out = match self.exec(cmd, None) {
            Ok(out) => out,
            Err(e) if crate::process::is_timeout(&e) => return Err(e),
            Err(e) => {
                return Ok(ChecksumResult::Failed(format!(
                    "{e:#} ({})",
                    script.display()
                )));
            }
        };
        let text = out.combined();
        for line in text.lines() {
            self.log.debug(format!("initrd-checksum: {line}"));
        }
        if out.success() {
            self.log.info(format!(
                "The initrd of {} matches its baseline",
                self.s.hostname
            ));
            return Ok(ChecksumResult::Passed);
        }
        let changed = text
            .lines()
            .filter(|l| {
                (l.starts_with('+') && !l.starts_with("+++"))
                    || (l.starts_with('-') && !l.starts_with("---"))
            })
            .count();
        let reason = if changed > 0 {
            format!("the initrd differs from its baseline ({changed} changed entries)")
        } else {
            let errors: Vec<&str> = text.lines().filter(|l| l.starts_with("ERR")).collect();
            errors
                .last()
                .map(|l| l.to_string())
                .or_else(|| last_line(&text))
                .unwrap_or_else(|| format!("initrd-checksum exited with {:?}", out.code))
        };
        Ok(ChecksumResult::Failed(reason))
    }

    /// A small fingerprint of the target's baseline (sizes, mtimes and the
    /// signature), so the whole baseline is only copied when it changed.
    fn remote_baseline_fingerprint(&self) -> Result<Option<String>> {
        let remote = "cd /etc/initrd-checksum && stat -c '%n %s %Y' checksum checksum.sig meta.json 2>/dev/null; cat checksum.sig 2>/dev/null";
        let out = self.exec(
            self.ssh(&self.endpoint(), KnownHostsKind::Default, &[], Some(remote)),
            None,
        )?;
        let text = out.stdout.trim();
        Ok((!text.is_empty()).then(|| text.to_string()))
    }

    /// Copy `/etc/initrd-checksum/` from the booted target into
    /// `<dir>/<hostname>/`, so the next boot can be verified against it.
    /// Skipped while the target's baseline fingerprint is unchanged.
    pub fn fetch_initrd_checksum(&self) -> Result<()> {
        let Some(dir) = &self.s.initrd.dir else {
            return Ok(());
        };
        let host_dir = dir.join(&self.s.hostname);
        let checksum = host_dir.join("initrd-checksum").join("checksum");
        let stamp = host_dir.join(".fingerprint");
        let fingerprint = self.remote_baseline_fingerprint()?;
        if let Some(fp) = &fingerprint
            && checksum.is_file()
            && fs::read_to_string(&stamp).ok().as_deref() == Some(fp.as_str())
        {
            self.log.debug(format!(
                "Initrd baseline of {} unchanged; not fetching",
                self.s.hostname
            ));
            return Ok(());
        }
        let before = fs::read(&checksum).ok();
        if let Err(e) = fs::create_dir_all(&host_dir) {
            self.log
                .warn(format!("Cannot create {}: {e}", host_dir.display()));
            return Ok(());
        }
        let out = self.exec(
            self.scp_from(KnownHostsKind::Default, "/etc/initrd-checksum/", &host_dir),
            None,
        )?;
        if !out.success() {
            self.log.warn(format!(
                "Cannot fetch /etc/initrd-checksum from {}: {}",
                self.s.hostname,
                out.error_line().unwrap_or_else(|| "scp failed".into())
            ));
            return Ok(());
        }
        if let Some(fp) = &fingerprint
            && let Err(e) = fs::write(&stamp, fp)
        {
            self.log
                .warn(format!("Cannot write {}: {e}", stamp.display()));
        }
        if fs::read(&checksum).ok() != before {
            self.log.info(format!(
                "Updated the stored initrd baseline of {}",
                self.s.hostname
            ));
        } else {
            self.log
                .debug(format!("Initrd baseline of {} unchanged", self.s.hostname));
        }
        Ok(())
    }
}
