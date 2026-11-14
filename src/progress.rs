//! Progress of a console command, for whoever shows it.
//!
//! Run by hand, a step is a line on stderr. Run by the dashboard (which sets
//! [`TUI_ENV`]), it is a private OSC sequence instead: the dashboard shows the
//! step on its loading screen and takes the sequence out of the output, and
//! [`ready`] tells it that the console itself is there.

use std::io::Write;

/// Set (to `1`) by the dashboard for the commands it runs.
pub const TUI_ENV: &str = "LUKS_SSH_UNLOCK_TUI";

/// The sequences: `ESC ] 777 ; lsu ; status ; TEXT BEL` and `... ; ready BEL`.
pub const PREFIX: &[u8] = b"\x1b]777;lsu;";
pub const END: u8 = 0x07;

fn in_tui() -> bool {
    std::env::var_os(TUI_ENV).is_some_and(|v| v == "1")
}

/// Printable text only: no control characters (they would end or corrupt the
/// sequence, or reach the terminal).
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// What is happening now: a line `PREFIX TEXT` by hand, a step for the
/// dashboard otherwise.
pub fn step(prefix: &str, text: &str) {
    let mut err = std::io::stderr().lock();
    if in_tui() {
        let _ = write!(err, "\x1b]777;lsu;status;{}\x07", clean(text));
    } else {
        let _ = writeln!(err, "{prefix}{text}");
    }
    let _ = err.flush();
}

/// The console only shows output (a log): the dashboard says so and keeps the
/// keyboard to itself.
pub fn read_only() {
    if in_tui() {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\x1b]777;lsu;readonly\x07");
        let _ = err.flush();
    }
}

/// The console is there: the dashboard stops showing the loading screen.
pub fn ready() {
    if in_tui() {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\x1b]777;lsu;ready\x07");
        let _ = err.flush();
    }
}

/// What the dashboard learnt from a chunk of a command's output.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The output without the progress sequences.
    pub data: Vec<u8>,
    /// The last step reported in the chunk.
    pub status: Option<String>,
    pub ready: bool,
    pub read_only: bool,
}

/// Take the progress sequences out of DATA. A sequence cut at the end of the
/// chunk is kept in PENDING and completed by the next one.
pub fn scan(pending: &mut Vec<u8>, data: &[u8]) -> Scan {
    let mut buf = std::mem::take(pending);
    buf.extend_from_slice(data);
    let mut out = Scan::default();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0x1b {
            let rest = &buf[i..];
            if rest.len() < PREFIX.len() && PREFIX.starts_with(rest) {
                // Possibly the start of a sequence: wait for the rest.
                *pending = rest.to_vec();
                break;
            }
            if rest.starts_with(PREFIX) {
                match rest.iter().position(|b| *b == END) {
                    Some(end) => {
                        let body = String::from_utf8_lossy(&rest[PREFIX.len()..end]);
                        if body == "ready" {
                            out.ready = true;
                        } else if body == "readonly" {
                            out.read_only = true;
                        } else if let Some(text) = body.strip_prefix("status;") {
                            out.status = Some(clean(text));
                        }
                        i += end + 1;
                        continue;
                    }
                    None if rest.len() < 512 => {
                        *pending = rest.to_vec();
                        break;
                    }
                    // Not ours after all (or broken): pass it on.
                    None => {}
                }
            }
        }
        out.data.push(buf[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_are_taken_out_and_reported() {
        let mut pending = Vec::new();
        let s = scan(
            &mut pending,
            b"a\x1b]777;lsu;status;step one\x07b\x1b]777;lsu;ready\x07c\x1b]777;lsu;readonly\x07",
        );
        assert_eq!(s.data, b"abc");
        assert_eq!(s.status.as_deref(), Some("step one"));
        assert!(s.ready && s.read_only);
    }

    #[test]
    fn a_sequence_split_between_chunks_is_completed() {
        let mut pending = Vec::new();
        let a = scan(&mut pending, b"x\x1b]77");
        assert_eq!(a.data, b"x");
        let b = scan(&mut pending, b"7;lsu;status;hel");
        assert!(b.data.is_empty() && b.status.is_none());
        let c = scan(&mut pending, b"lo\x07y");
        assert_eq!(c.data, b"y");
        assert_eq!(c.status.as_deref(), Some("hello"));
    }

    #[test]
    fn other_escapes_pass_through() {
        let mut pending = Vec::new();
        let s = scan(&mut pending, b"\x1b[31mred\x1b]0;title\x07");
        assert_eq!(s.data, b"\x1b[31mred\x1b]0;title\x07");
        assert!(pending.is_empty());
    }

    #[test]
    fn control_characters_never_get_into_a_step() {
        assert_eq!(clean("a\x1b]0;x\x07b\nc"), "a]0;xbc");
    }
}
