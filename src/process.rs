//! Run external commands (ssh, scp, ssh-keygen, the checksum helper) with a
//! hard deadline, capturing their output.

use std::fmt;
use std::io::{self, Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

/// The current check ran past its deadline and was aborted.
#[derive(Debug)]
pub struct TimedOut;

impl fmt::Display for TimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("timed out")
    }
}

impl std::error::Error for TimedOut {}

pub fn is_timeout(err: &anyhow::Error) -> bool {
    err.downcast_ref::<TimedOut>().is_some()
}

#[derive(Debug, Default)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// stdout followed by stderr.
    pub fn combined(&self) -> String {
        let mut text = self.stdout.clone();
        if !text.is_empty() && !text.ends_with('\n') && !self.stderr.is_empty() {
            text.push('\n');
        }
        text.push_str(&self.stderr);
        text
    }

    /// Last meaningful stderr line, without ssh's "Permanently added" noise.
    pub fn error_line(&self) -> Option<String> {
        last_line(&self.stderr).or_else(|| last_line(&self.stdout))
    }
}

pub fn last_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"))
        .map(str::to_string)
}

fn spawn_reader<R: Read + Send + 'static>(mut reader: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = reader.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// Wait for CHILD until DEADLINE; kill it when the deadline passes.
pub fn wait_until(child: &mut Child, deadline: Instant) -> io::Result<(Option<i32>, bool)> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok((status.code(), false));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let status = child.wait()?;
            return Ok((status.code(), true));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Run CMD, feeding STDIN (wiped from memory afterwards), and give up at
/// DEADLINE.
pub fn run(
    mut cmd: Command,
    stdin: Option<Zeroizing<Vec<u8>>>,
    deadline: Instant,
) -> io::Result<Output> {
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    if let (Some(data), Some(mut pipe)) = (stdin, child.stdin.take()) {
        thread::spawn(move || {
            let _ = pipe.write_all(&data);
            drop(data);
        });
    }
    let stdout = child.stdout.take().map(spawn_reader);
    let stderr = child.stderr.take().map(spawn_reader);
    let (code, timed_out) = wait_until(&mut child, deadline)?;
    Ok(Output {
        code,
        timed_out,
        stdout: stdout.and_then(|h| h.join().ok()).unwrap_or_default(),
        stderr: stderr.and_then(|h| h.join().ok()).unwrap_or_default(),
    })
}

/// Quote S for a POSIX shell.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("plain/path-1"), "plain/path-1");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("%h:%p"), "%h:%p");
    }

    #[test]
    fn deadline_kills() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let started = Instant::now();
        let out = run(cmd, None, Instant::now() + Duration::from_millis(200)).unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn captures_and_feeds_stdin() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "cat; echo err >&2; exit 3"]);
        let input = Zeroizing::new(b"hello\n".to_vec());
        let out = run(cmd, Some(input), Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hello\n");
        assert_eq!(out.error_line().as_deref(), Some("err"));
    }
}
