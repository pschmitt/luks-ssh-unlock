//! The helper's wire protocol: bounded, typed, line-delimited JSON.
//!
//! One request per line, one response per line. A request is
//! `{"id": 7, "request": {"verb": "probe", "name": "alpha"}}`. Everything is
//! typed: unknown verbs and unknown fields are errors, the target is a
//! validated name (never a path), there are no free-form arguments, and a
//! request line is at most [`MAX_FRAME`] bytes.

use std::fmt;
use std::io::{self, BufRead};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

/// Longest accepted request line, in bytes (without the newline).
pub const MAX_FRAME: usize = 1024;
/// Longest target name.
pub const MAX_NAME: usize = 64;

/// A validated target name: what `NAME.env` is called, and the instance part
/// of `luks-ssh-unlock-NAME.service`. It cannot contain a path separator,
/// start with a dot or a dash, be empty or long, or hold anything but ASCII
/// letters, digits, `.`, `_` and `-`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Target(String);

#[derive(Debug, PartialEq, Eq)]
pub struct NameError;

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid target name")
    }
}

impl std::error::Error for NameError {}

impl Target {
    pub fn parse(name: &str) -> Result<Self, NameError> {
        let bytes = name.as_bytes();
        let ok = !bytes.is_empty()
            && bytes.len() <= MAX_NAME
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            && !name.contains("..")
            && !name.ends_with('.');
        if ok {
            Ok(Self(name.to_string()))
        } else {
            Err(NameError)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `NAME.env`, a single path component.
    pub fn env_file_name(&self) -> String {
        format!("{}.env", self.0)
    }

    /// The systemd unit that belongs to this target, derived here and never
    /// taken from the caller.
    pub fn unit_name(&self) -> String {
        format!("luks-ssh-unlock-{}.service", self.0)
    }
}

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Target({})", self.0)
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Target {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Do not echo the rejected value.
        Target::parse(&s).map_err(|_| de::Error::custom("invalid target name"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceAction {
    Start,
    Stop,
}

/// Explicit acknowledgement that forcing skips the integrity and host-key
/// checks. A separate verb and a mandatory, exact field: a request cannot
/// become a forced unlock by an omitted or defaulted flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum ForceAck {
    #[serde(rename = "skip_integrity_and_host_key_checks")]
    SkipIntegrityAndHostKeyChecks,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// Targets with a trusted configuration. A struct variant (not a unit
    /// one) so that `deny_unknown_fields` applies to it too.
    List {},
    /// Check a target without sending anything.
    Probe { name: Target },
    /// Unlock with every check applied.
    Unlock { name: Target },
    /// Unlock skipping the integrity and host-key checks.
    UnlockForce { name: Target, confirm: ForceAck },
    /// Start or stop the target's own service unit.
    Service { name: Target, action: ServiceAction },
}

#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub id: u64,
    pub request: Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Not valid JSON, an unknown verb or field, or a bad name.
    BadRequest,
    /// The line was longer than the limit; the session ends.
    TooLarge,
    /// The policy of this session does not allow the verb.
    Denied,
    /// No such target.
    UnknownTarget,
    /// The target's configuration is not something to act on.
    UntrustedConfig,
    /// The operation itself failed.
    Failed,
    /// The operation did not finish in time.
    Timeout,
    /// The helper failed.
    Internal,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok {
        id: u64,
        data: serde_json::Value,
    },
    Error {
        id: Option<u64>,
        code: ErrorCode,
        message: String,
    },
}

impl Response {
    pub fn error(id: Option<u64>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            id,
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Line(Vec<u8>),
    Eof,
}

#[derive(Debug)]
pub enum FrameError {
    TooLong,
    Truncated,
    Io(io::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "request is longer than {MAX_FRAME} bytes"),
            Self::Truncated => f.write_str("input ended in the middle of a request"),
            Self::Io(e) => write!(f, "i/o error: {e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Read one line of at most `max` bytes. The buffer never grows past the
/// limit, whatever the peer sends.
pub fn read_frame<R: BufRead>(input: &mut R, max: usize) -> Result<Frame, FrameError> {
    let mut line = Vec::new();
    loop {
        let available = match input.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if available.is_empty() {
            return if line.is_empty() {
                Ok(Frame::Eof)
            } else {
                Err(FrameError::Truncated)
            };
        }
        match available.iter().position(|b| *b == b'\n') {
            Some(end) => {
                if line.len() + end > max {
                    return Err(FrameError::TooLong);
                }
                line.extend_from_slice(&available[..end]);
                input.consume(end + 1);
                return Ok(Frame::Line(line));
            }
            None => {
                if line.len() + available.len() > max {
                    return Err(FrameError::TooLong);
                }
                line.extend_from_slice(available);
                let n = available.len();
                input.consume(n);
            }
        }
    }
}

/// Parse one request line.
pub fn parse_request(line: &[u8]) -> Result<Envelope, ()> {
    serde_json::from_slice(line).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn names_are_plain_identifiers() {
        for ok in [
            "a",
            "alpha",
            "web-1",
            "db_2",
            "host.example",
            "A1",
            &"x".repeat(64),
        ] {
            assert!(Target::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "-rf",
            "_x",
            "a/b",
            "../etc/passwd",
            "/etc/passwd",
            "a\\b",
            "a b",
            "a\0b",
            "a\nb",
            "a..b",
            "trailing.",
            "ümlaut",
            "a;b",
            "$(id)",
            "a:b",
            "a*",
            &"x".repeat(65),
        ] {
            assert!(Target::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn derived_names_have_no_separators() {
        let t = Target::parse("alpha-1").unwrap();
        assert_eq!(t.env_file_name(), "alpha-1.env");
        assert_eq!(t.unit_name(), "luks-ssh-unlock-alpha-1.service");
    }

    fn parse(json: &str) -> Result<Envelope, ()> {
        parse_request(json.as_bytes())
    }

    #[test]
    fn well_formed_requests_parse() {
        let e = parse(r#"{"id":1,"request":{"verb":"list"}}"#).unwrap();
        assert_eq!(e.request, Request::List {});
        let e = parse(r#"{"id":2,"request":{"verb":"probe","name":"a"}}"#).unwrap();
        assert_eq!(e.id, 2);
        assert!(matches!(e.request, Request::Probe { .. }));
        let e =
            parse(r#"{"id":3,"request":{"verb":"service","name":"a","action":"stop"}}"#).unwrap();
        assert!(matches!(
            e.request,
            Request::Service {
                action: ServiceAction::Stop,
                ..
            }
        ));
    }

    #[test]
    fn everything_else_is_rejected() {
        for bad in [
            // unknown verb, extra and missing fields, wrong types
            r#"{"id":1,"request":{"verb":"shell","name":"a"}}"#,
            r#"{"id":1,"request":{"verb":"probe","name":"a","extra":1}}"#,
            r#"{"id":1,"request":{"verb":"list","name":"a"}}"#,
            r#"{"id":1,"request":{"verb":"probe"}}"#,
            r#"{"id":1,"request":{"verb":"probe","name":5}}"#,
            r#"{"id":1,"request":{"verb":"probe","name":null}}"#,
            r#"{"id":"1","request":{"verb":"list"}}"#,
            r#"{"id":-1,"request":{"verb":"list"}}"#,
            r#"{"request":{"verb":"list"}}"#,
            r#"{"id":1}"#,
            r#"{"id":1,"request":{"verb":"list"},"x":1}"#,
            r#"{"id":1,"verb":"list"}"#,
            r#"[]"#,
            r#""list""#,
            "",
            "not json",
            // path-like and hostile names
            r#"{"id":1,"request":{"verb":"probe","name":"../../etc/shadow"}}"#,
            r#"{"id":1,"request":{"verb":"probe","name":"/etc/passwd"}}"#,
            r#"{"id":1,"request":{"verb":"unlock","name":"a/../b"}}"#,
            r#"{"id":1,"request":{"verb":"unlock","name":"a\u0000b"}}"#,
            // arguments that must not exist
            r#"{"id":1,"request":{"verb":"unlock","name":"a","env":{"X":"1"}}}"#,
            r#"{"id":1,"request":{"verb":"unlock","name":"a","args":["--force"]}}"#,
            r#"{"id":1,"request":{"verb":"unlock","name":"a","force":true}}"#,
            r#"{"id":1,"request":{"verb":"unlock","name":"a","config":"/tmp/x.env"}}"#,
            r#"{"id":1,"request":{"verb":"probe","name":"a","path":"/tmp/x"}}"#,
            // service: only the two actions
            r#"{"id":1,"request":{"verb":"service","name":"a","action":"restart"}}"#,
            r#"{"id":1,"request":{"verb":"service","name":"a","action":"stop;id"}}"#,
            r#"{"id":1,"request":{"verb":"service","name":"a"}}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn force_needs_its_exact_acknowledgement() {
        let ok = r#"{"id":1,"request":{"verb":"unlock_force","name":"a","confirm":"skip_integrity_and_host_key_checks"}}"#;
        assert!(matches!(
            parse(ok).unwrap().request,
            Request::UnlockForce { .. }
        ));
        for bad in [
            r#"{"id":1,"request":{"verb":"unlock_force","name":"a"}}"#,
            r#"{"id":1,"request":{"verb":"unlock_force","name":"a","confirm":true}}"#,
            r#"{"id":1,"request":{"verb":"unlock_force","name":"a","confirm":"yes"}}"#,
            r#"{"id":1,"request":{"verb":"unlock_force","name":"a","confirm":""}}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn frames_are_bounded() {
        let mut input = Cursor::new(b"hello\nworld\n".to_vec());
        assert_eq!(
            read_frame(&mut input, 16).unwrap(),
            Frame::Line(b"hello".to_vec())
        );
        assert_eq!(
            read_frame(&mut input, 16).unwrap(),
            Frame::Line(b"world".to_vec())
        );
        assert_eq!(read_frame(&mut input, 16).unwrap(), Frame::Eof);
        // Exactly at the limit is fine, one over is not.
        let mut input = Cursor::new(b"12345\n".to_vec());
        assert!(read_frame(&mut input, 5).is_ok());
        let mut input = Cursor::new(b"123456\n".to_vec());
        assert!(matches!(
            read_frame(&mut input, 5),
            Err(FrameError::TooLong)
        ));
    }

    #[test]
    fn endless_input_does_not_grow_the_buffer() {
        struct Endless;
        impl io::Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                buf.fill(b'a');
                Ok(buf.len())
            }
        }
        let mut input = io::BufReader::with_capacity(64, Endless);
        assert!(matches!(
            read_frame(&mut input, MAX_FRAME),
            Err(FrameError::TooLong)
        ));
    }

    #[test]
    fn truncated_input_is_not_a_request() {
        let mut input = Cursor::new(b"{\"id\":1".to_vec());
        assert!(matches!(
            read_frame(&mut input, MAX_FRAME),
            Err(FrameError::Truncated)
        ));
    }

    #[test]
    fn a_rejected_name_is_not_echoed() {
        let err = serde_json::from_str::<Envelope>(
            r#"{"id":1,"request":{"verb":"probe","name":"../SECRET-MARKER"}}"#,
        )
        .unwrap_err();
        assert!(!err.to_string().contains("SECRET-MARKER"));
    }
}
