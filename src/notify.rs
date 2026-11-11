//! Notifications: the events file, Apprise and mail.
//!
//! A failed notification is logged and never stops the unlocker.

use std::fs;
use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::ctx::{Ctx, Mode};
use crate::log::Level;
use crate::process;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Success,
    Warning,
    Failure,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Failure => "failure",
        }
    }

    fn level(self) -> Level {
        match self {
            Self::Success => Level::Info,
            Self::Warning => Level::Warning,
            Self::Failure => Level::Error,
        }
    }
}

impl Ctx {
    /// Expand `#placeholders` in a notification title or subject.
    pub fn template(&self, text: &str, event: Event, message: &str) -> String {
        let s = &self.s;
        let jump = s.jumphost.as_ref();
        let jport = jump.map(|j| j.port.to_string()).unwrap_or_default();
        let sleep = s.interval.to_string();
        let port = s.port.to_string();
        let pairs: [(&str, &str); 16] = [
            ("#self", "luks-ssh-unlock"),
            ("#event_type", event.as_str()),
            ("#event", message),
            ("#sleep_interval", &sleep),
            ("#hostname", &s.hostname),
            ("#username", &s.username),
            ("#port", &port),
            (
                "#jumphost",
                jump.map(|j| j.host.as_str()).unwrap_or_default(),
            ),
            (
                "#jusername",
                jump.map(|j| j.user.as_str()).unwrap_or_default(),
            ),
            ("#jport", &jport),
            ("#luks_type", s.luks_type.as_str()),
            ("#luks_password", "[REDACTED]"),
            (
                "#remote_cmd",
                s.healthcheck.command.as_deref().unwrap_or_default(),
            ),
            (
                "#remote_hostname",
                s.healthcheck.host.as_deref().unwrap_or_default(),
            ),
            (
                "#remote_username",
                s.healthcheck.user.as_deref().unwrap_or_default(),
            ),
            ("{{SSH_HOSTNAME}}", &s.hostname),
        ];
        let mut out = text.to_string();
        for (key, value) in pairs {
            out = out.replace(key, value);
        }
        out
    }

    /// Log MESSAGE and, outside status probes, send it to every configured
    /// notification channel.
    pub fn notify(&self, event: Event, message: &str) {
        self.log.log(event.level(), message);
        if self.mode == Mode::Status {
            return;
        }
        let n = &self.s.notify;

        if let Some(path) = &n.events_file {
            let result = path
                .parent()
                .map_or(Ok(()), fs::create_dir_all)
                .and_then(|_| fs::OpenOptions::new().create(true).append(true).open(path))
                .and_then(|mut f| {
                    writeln!(
                        f,
                        "{} {} {}",
                        chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
                        event.as_str(),
                        message
                    )
                });
            if let Err(e) = result {
                self.log.warn(format!(
                    "Cannot write the events file {}: {e}",
                    path.display()
                ));
            }
        }

        if let Some(url) = &n.apprise_url {
            let mut body = serde_json::json!({ "body": message, "type": event.as_str() });
            if let Some(tag) = &n.apprise_tag {
                body["tag"] = tag.clone().into();
            }
            if let Some(title) = &n.apprise_title {
                body["title"] = self.template(title, event, message).into();
            }
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent(concat!("luks-ssh-unlock/", env!("CARGO_PKG_VERSION")))
                .build();
            if let Err(e) = agent.post(url).send_json(body) {
                self.log.warn(format!("Apprise notification failed: {e}"));
            }
        }

        if let Some(recipient) = &n.email_recipient
            && let Err(e) = self.send_mail(recipient, event, message)
        {
            self.log
                .warn(format!("Mail notification to {recipient} failed: {e}"));
        }
    }

    fn send_mail(&self, recipient: &str, event: Event, message: &str) -> std::io::Result<()> {
        let n = &self.s.notify;
        let mut mail = String::new();
        if let Some(from) = &n.email_from {
            mail.push_str(&format!("From: {from}\n"));
        }
        mail.push_str(&format!("To: {recipient}\n"));
        let subject = n
            .email_subject
            .as_deref()
            .map(|s| self.template(s, event, message))
            .unwrap_or_else(|| format!("luks-ssh-unlock: {} {}", self.s.hostname, event.as_str()));
        mail.push_str(&format!("Subject: {subject}\n"));
        mail.push_str("Content-Type: text/plain; charset=utf-8\n\n");
        mail.push_str(&format!(
            "Event type: {}\n\n{message}\n",
            event.as_str().to_uppercase()
        ));
        if self.log.debug_enabled() {
            mail.push_str(&format!(
                "\nTarget: {}\nLUKS method: {}\n",
                self.s.route(),
                self.s.luks_type.as_str()
            ));
        }

        let mut cmd = match &n.msmtp_account {
            Some(account) => {
                let mut c = Command::new("msmtp");
                c.args(["-a", account]);
                c
            }
            None => Command::new("sendmail"),
        };
        cmd.arg("--").arg(recipient);
        let deadline = Instant::now() + Duration::from_secs(60);
        let out = process::run(cmd, Some(Zeroizing::new(mail.into_bytes())), deadline)?;
        if out.success() {
            Ok(())
        } else if out.timed_out {
            Err(std::io::Error::other("timed out"))
        } else {
            Err(std::io::Error::other(
                out.error_line()
                    .unwrap_or_else(|| format!("exit code {:?}", out.code)),
            ))
        }
    }
}
