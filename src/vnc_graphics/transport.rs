//! Kitty APC passthrough and pane-relative cursor translation for tmux.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use super::PaneRect;
use super::detect::{self, Client, Probe};

const VIEWER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const CAPABILITY_POLL_INTERVAL: Duration = Duration::from_secs(5);
// A loaded machine (or a worker that is still starting) can take longer than
// a few milliseconds to answer; a timeout means "no answer", never "no".
const COMMAND_TIMEOUT: Duration = Duration::from_millis(250);
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_COMMAND_OUTPUT: usize = 4096;
const MAX_VIEWERS: usize = 64;
const MAX_VIEWER_LINE: usize = 128;
const MAX_CLIENT_LINE: usize = 512;
const MAX_CLIENTS: usize = 16;

#[derive(Default)]
struct ViewerState {
    viewers: Option<Vec<String>>,
    changed: bool,
    pane_active: Option<bool>,
    pane_origin: Option<(u16, u16)>,
    probe: Option<Probe>,
}

static VIEWER_STATE: OnceLock<Mutex<ViewerState>> = OnceLock::new();
static VIEWER_WORKER: OnceLock<()> = OnceLock::new();
static POLLING_ALLOWED: OnceLock<bool> = OnceLock::new();
static RECHECK_REQUESTED: AtomicU64 = AtomicU64::new(0);
static RECHECK_ANSWERED: AtomicU64 = AtomicU64::new(0);

pub fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

/// The renderer only reads worker-published state; tmux subprocesses never run
/// on the caller's drawing thread. Outside tmux every pane is considered visible.
pub fn viewers_changed() -> bool {
    if !in_tmux() || !ensure_viewer_worker() {
        return false;
    }
    let mut state = viewer_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    std::mem::take(&mut state.changed)
}

pub fn pane_is_active() -> bool {
    if !in_tmux() {
        return true;
    }
    if !ensure_viewer_worker() {
        return true;
    }
    viewer_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .pane_active
        .unwrap_or(true)
}

/// The tmux pane's zero-based origin in the outer terminal. It is populated
/// by the polling worker so image drawing never shells out on the UI thread.
pub fn pane_origin() -> Option<(u16, u16)> {
    if !in_tmux() {
        return Some((0, 0));
    }
    if !ensure_viewer_worker() {
        return None;
    }
    viewer_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .pane_origin
}

/// Can the terminal behind tmux show pictures? `forced` is the user's
/// override for a terminal that cannot be recognised. `false` also covers
/// "not known yet" (see [`tmux_unsupported_reason`]).
pub fn tmux_graphics_supported(forced: bool) -> bool {
    tmux_verdict(forced).is_some_and(|verdict| verdict.is_ok())
}

/// Why pictures cannot be shown, in words for the pane. `None`: it works, or
/// tmux has not answered yet.
pub fn tmux_unsupported_reason(forced: bool) -> Option<String> {
    if !in_tmux() {
        return None;
    }
    if !polling_allowed() {
        return Some(
            "Cannot ask tmux about the terminal from a privileged process. Run the dashboard as your own user."
                .to_string(),
        );
    }
    if validated_pane_target().is_none() {
        return Some(
            "Cannot tell which tmux pane this is (`TMUX_PANE` is not set to a pane id)."
                .to_string(),
        );
    }
    tmux_verdict(forced)?.err()
}

fn tmux_verdict(forced: bool) -> Option<Result<(), String>> {
    if !in_tmux() || !ensure_viewer_worker() {
        return None;
    }
    let state = viewer_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.probe.as_ref().map(|probe| probe.verdict(forced))
}

/// Ask tmux again, soon (after the user has changed an option or moved the
/// session to another terminal). The old answer stays until the new one is in.
pub fn recheck() {
    RECHECK_REQUESTED.fetch_add(1, Ordering::Relaxed);
}

/// True from `recheck()` until the poll thread has stored tmux's new answer
/// (so a decision taken right after an option change is not taken from the
/// old answer). It is only meaningful while that thread runs.
pub fn recheck_pending() -> bool {
    RECHECK_ANSWERED.load(Ordering::Relaxed) < RECHECK_REQUESTED.load(Ordering::Relaxed)
}

fn viewer_state() -> &'static Mutex<ViewerState> {
    VIEWER_STATE.get_or_init(|| Mutex::new(ViewerState::default()))
}

fn ensure_viewer_worker() -> bool {
    if !polling_allowed() {
        return false;
    }
    VIEWER_WORKER.get_or_init(|| {
        if let Some(pane) = validated_pane_target() {
            let _ = thread::Builder::new()
                .name("tmux-viewer-poll".to_owned())
                .spawn(move || viewer_poll_worker(pane));
        }
    });
    validated_pane_target().is_some()
}

fn polling_allowed() -> bool {
    *POLLING_ALLOWED.get_or_init(effective_identity_is_unprivileged)
}

#[cfg(target_os = "linux")]
fn effective_identity_is_unprivileged() -> bool {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return false;
    };
    unprivileged_status(&status)
}

#[cfg(target_os = "linux")]
fn unprivileged_status(status: &str) -> bool {
    let Some(uid_line) = status.lines().find(|line| line.starts_with("Uid:")) else {
        return false;
    };
    let Some(gid_line) = status.lines().find(|line| line.starts_with("Gid:")) else {
        return false;
    };
    let Some(cap_line) = status.lines().find(|line| line.starts_with("CapEff:")) else {
        return false;
    };
    let ids_match = |line: &str| {
        let parsed: Option<Vec<u32>> = line
            .split_whitespace()
            .skip(1)
            .map(|id| id.parse().ok())
            .collect();
        parsed.is_some_and(|ids| ids.len() >= 4 && ids.iter().all(|id| *id == ids[0]))
    };
    let uid_is_non_root = uid_line
        .split_whitespace()
        .nth(1)
        .and_then(|uid| uid.parse::<u32>().ok())
        .is_some_and(|uid| uid != 0);
    let no_effective_capabilities = cap_line
        .split_whitespace()
        .nth(1)
        .and_then(|caps| u64::from_str_radix(caps, 16).ok())
        .is_some_and(|caps| caps == 0);
    uid_is_non_root && ids_match(uid_line) && ids_match(gid_line) && no_effective_capabilities
}

#[cfg(not(target_os = "linux"))]
fn effective_identity_is_unprivileged() -> bool {
    false
}

fn validated_pane_target() -> Option<String> {
    let pane = std::env::var("TMUX_PANE").ok()?;
    let digits = pane.strip_prefix('%')?;
    (!digits.is_empty() && digits.len() <= 10 && digits.bytes().all(|byte| byte.is_ascii_digit()))
        .then_some(pane)
}

fn viewer_poll_worker(pane: String) {
    let mut next_capability_poll = Instant::now();
    loop {
        if !effective_identity_is_unprivileged() {
            return;
        }
        let viewers = query_viewers(&pane);
        let requested = RECHECK_REQUESTED.load(Ordering::Relaxed);
        let asked = requested > RECHECK_ANSWERED.load(Ordering::Relaxed)
            || Instant::now() >= next_capability_poll;
        let capabilities = if asked {
            next_capability_poll = Instant::now() + CAPABILITY_POLL_INTERVAL;
            query_tmux_capabilities(&pane)
        } else {
            None
        };
        if viewers.is_some() || capabilities.is_some() {
            let mut state = viewer_state()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((viewers, pane_active, origin)) = viewers {
                if state.viewers.as_ref() != Some(&viewers) {
                    state.changed = true;
                    state.viewers = Some(viewers);
                }
                if state.pane_origin != Some(origin) {
                    state.changed = true;
                    state.pane_origin = Some(origin);
                }
                state.pane_active = Some(pane_active);
            }
            if capabilities.is_some() {
                state.probe = capabilities;
            }
        }
        if asked {
            RECHECK_ANSWERED.fetch_max(requested, Ordering::Relaxed);
        }
        thread::sleep(VIEWER_POLL_INTERVAL);
    }
}

/// What tmux knows: the pane's *effective* `allow-passthrough` (the pane's own
/// setting, else its window's, else the global one; `on` or `all` both pass
/// images) and every client that shows the pane's window, with the terminal
/// name it reported at startup, its `XTVERSION` answer and, when readable,
/// what its process environment says.
fn query_tmux_capabilities(pane: &str) -> Option<Probe> {
    let state = run_tmux(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{window_id} #{allow-passthrough}",
    ])?;
    let (window, passthrough) = parse_pane_options(std::str::from_utf8(&state).ok()?)?;
    let list = run_tmux(&[
        "list-clients",
        "-F",
        "#{client_pid}|#{window_id}|#{client_termname}|#{client_termtype}",
    ])?;
    let mut clients = parse_clients(std::str::from_utf8(&list).ok()?, &window)?;
    for client in &mut clients {
        client.environment = read_client_environment(client.pid);
    }
    Some(Probe {
        passthrough,
        clients,
    })
}

/// `@3 on` (or `@3 ` when the option does not exist, as in tmux before 3.3).
fn parse_pane_options(text: &str) -> Option<(String, String)> {
    let line = text.trim_end_matches('\n');
    if line.len() > MAX_VIEWER_LINE {
        return None;
    }
    let (window, passthrough) = line.split_once(' ').unwrap_or((line, ""));
    valid_window_id(window).then(|| (window.to_owned(), passthrough.trim().to_owned()))
}

/// Lines of `pid|window|termname|termtype`, for the clients showing `window`.
fn parse_clients(text: &str, window: &str) -> Option<Vec<Client>> {
    if text.len() > MAX_COMMAND_OUTPUT || !valid_window_id(window) {
        return None;
    }
    let mut clients = Vec::new();
    for line in text.lines() {
        if line.len() > MAX_CLIENT_LINE {
            return None;
        }
        let mut fields = line.splitn(4, '|');
        let pid = fields.next()?.trim().parse::<u32>().ok()?;
        let client_window = fields.next()?;
        if !valid_window_id(client_window) {
            return None;
        }
        let term = fields.next()?.trim();
        let version = fields.next()?.trim();
        if client_window == window {
            if clients.len() == MAX_CLIENTS {
                return None;
            }
            clients.push(Client {
                pid,
                term: term.to_owned(),
                version: version.to_owned(),
                environment: None,
            });
        }
    }
    Some(clients)
}

/// The terminal a client process was started from, by its environment. Only
/// a process of the same user can be read (the kernel decides), and only a
/// name is taken from it; nothing of it is executed or kept.
#[cfg(target_os = "linux")]
fn read_client_environment(pid: u32) -> Option<&'static str> {
    if pid == 0 {
        return None;
    }
    let file = std::fs::File::open(format!("/proc/{pid}/environ")).ok()?;
    let mut bytes = Vec::new();
    file.take(detect::MAX_ENVIRON as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    detect::classify_environ(&bytes)
}

#[cfg(not(target_os = "linux"))]
fn read_client_environment(_pid: u32) -> Option<&'static str> {
    None
}

fn query_viewers(pane: &str) -> Option<(Vec<String>, bool, (u16, u16))> {
    let pane_state = run_tmux(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{window_id} #{?window_zoomed_flag,#{pane_active},1} #{pane_left} #{pane_top}",
    ])?;
    let (window, visible, origin) =
        parse_viewer_pane_state(std::str::from_utf8(&pane_state).ok()?)?;
    let client_list = run_tmux(&["list-clients", "-F", "#{client_tty} #{window_id}"])?;
    let clients = std::str::from_utf8(&client_list).ok()?;
    let viewers = visible_viewers(visible, &window, clients)?;
    Some((viewers.clone(), !viewers.is_empty(), origin))
}

fn parse_viewer_pane_state(text: &str) -> Option<(String, bool, (u16, u16))> {
    if text.len() > MAX_VIEWER_LINE {
        return None;
    }
    let mut fields = text.split_whitespace();
    let window = fields.next()?;
    let visible = match fields.next()? {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    let left = fields.next()?.parse::<u16>().ok()?;
    let top = fields.next()?.parse::<u16>().ok()?;
    if fields.next().is_some() || !valid_window_id(window) {
        return None;
    }
    Some((window.to_owned(), visible, (left, top)))
}

fn visible_viewers(visible: bool, window: &str, clients: &str) -> Option<Vec<String>> {
    if clients.len() > MAX_COMMAND_OUTPUT || !valid_window_id(window) {
        return None;
    }
    let mut viewers = Vec::new();
    for line in clients.lines() {
        if line.len() > MAX_VIEWER_LINE {
            return None;
        }
        let mut fields = line.split_whitespace();
        let client = fields.next()?;
        let client_window = fields.next()?;
        if fields.next().is_some() || !valid_window_id(client_window) {
            return None;
        }
        if visible && client_window == window {
            if viewers.len() == MAX_VIEWERS {
                return None;
            }
            viewers.push(client.to_owned());
        }
    }
    viewers.sort();
    Some(viewers)
}

fn valid_window_id(window: &str) -> bool {
    window.strip_prefix('@').is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn run_tmux(args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => thread::sleep(COMMAND_POLL_INTERVAL),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = Vec::new();
    child
        .stdout
        .take()?
        .take((MAX_COMMAND_OUTPUT + 1) as u64)
        .read_to_end(&mut output)
        .ok()?;
    (output.len() <= MAX_COMMAND_OUTPUT).then_some(output)
}

/// Write a composed terminal stream, translating pane-relative placements into
/// outer-terminal coordinates and enclosing Kitty APCs for tmux passthrough.
#[allow(dead_code)] // Public convenience API when callers do not cache the pane origin.
pub fn write(out: &mut impl Write, bytes: &[u8], pane: PaneRect) -> io::Result<()> {
    let origin = if in_tmux() {
        pane_origin().unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    write_with_tmux_origin(out, bytes, in_tmux(), pane, origin)
}

/// Write graphics using the pane origin captured for the frame being drawn.
pub fn write_at_origin(
    out: &mut impl Write,
    bytes: &[u8],
    pane: PaneRect,
    origin: (u16, u16),
) -> io::Result<()> {
    write_with_tmux_origin(out, bytes, in_tmux(), pane, origin)
}

/// Enclose an arbitrary terminal query/reply sentinel for tmux passthrough.
#[allow(dead_code)] // Public transport helper for future terminal queries.
pub fn append_passthrough(out: &mut Vec<u8>, bytes: &[u8]) {
    if in_tmux() {
        wrap(out, bytes).expect("writing into a Vec cannot fail");
    } else {
        out.extend_from_slice(bytes);
    }
}

fn wrap(out: &mut impl Write, mut bytes: &[u8]) -> io::Result<()> {
    out.write_all(b"\x1bPtmux;")?;
    while let Some(escape) = bytes.iter().position(|&byte| byte == 0x1b) {
        out.write_all(&bytes[..escape])?;
        out.write_all(b"\x1b\x1b")?;
        bytes = &bytes[escape + 1..];
    }
    out.write_all(bytes)?;
    out.write_all(b"\x1b\\")
}

fn trailing_cup(bytes: &[u8]) -> Option<(usize, u32, u32)> {
    let body = bytes.strip_suffix(b"H")?;
    let col_start = body.iter().rposition(|b| !b.is_ascii_digit())? + 1;
    let col: u32 = std::str::from_utf8(&body[col_start..]).ok()?.parse().ok()?;
    let row_body = body[..col_start].strip_suffix(b";")?;
    let row_start = row_body.iter().rposition(|b| !b.is_ascii_digit())? + 1;
    let row: u32 = std::str::from_utf8(&row_body[row_start..])
        .ok()?
        .parse()
        .ok()?;
    let prefix = row_body[..row_start].strip_suffix(b"\x1b[")?;
    Some((prefix.len(), row, col))
}

#[cfg(test)]
fn write_with_tmux(
    out: &mut impl Write,
    bytes: &[u8],
    tmux: bool,
    pane: PaneRect,
) -> io::Result<()> {
    write_with_tmux_origin(out, bytes, tmux, pane, (0, 0))
}

fn write_with_tmux_origin(
    out: &mut impl Write,
    mut bytes: &[u8],
    tmux: bool,
    pane: PaneRect,
    origin: (u16, u16),
) -> io::Result<()> {
    let mut cursor = None;
    while let Some(start) = bytes.windows(3).position(|part| part == b"\x1b_G") {
        let Some(end_offset) = bytes[start..].windows(2).position(|part| part == b"\x1b\\") else {
            return write_before_apc(out, bytes, tmux);
        };
        let end = start + end_offset + 2;
        let prefix = &bytes[..start];
        let address = match trailing_cup(prefix) {
            Some((keep, row, col)) => {
                write_before_apc(out, &prefix[..keep], tmux)?;
                Some((row, col))
            }
            None if prefix.is_empty() => cursor,
            None => {
                write_before_apc(out, prefix, tmux)?;
                None
            }
        };
        cursor = address;
        let command = &bytes[start..end];
        if let Some((row, col)) = address {
            let outer_row = u32::from(origin.1) + u32::from(pane.top) + row;
            let outer_col = u32::from(origin.0) + u32::from(pane.left) + col;
            if tmux {
                // APC placement occurs in the outer terminal, whose cursor is separate
                // from tmux's pane cursor. Save/restore it around the translated CUP.
                let mut payload = Vec::with_capacity(command.len() + 32);
                write!(payload, "\x1b7\x1b[{outer_row};{outer_col}H")?;
                payload.extend_from_slice(command);
                payload.extend_from_slice(b"\x1b8");
                wrap(out, &payload)?;
            } else {
                write!(out, "\x1b[{outer_row};{outer_col}H")?;
                out.write_all(command)?;
            }
        } else if tmux {
            wrap(out, command)?;
        } else {
            out.write_all(command)?;
        }
        bytes = &bytes[end..];
    }
    write_before_apc(out, bytes, tmux)
}

fn write_before_apc(out: &mut impl Write, bytes: &[u8], tmux: bool) -> io::Result<()> {
    if tmux {
        write_prefix(out, bytes)
    } else {
        out.write_all(bytes)
    }
}

/// tmux consumes synchronized-output markers locally unless they use passthrough.
/// Forward them to the outer terminal, where the image placements are composited.
fn write_prefix(out: &mut impl Write, mut bytes: &[u8]) -> io::Result<()> {
    const BEGIN: &[u8] = b"\x1b[?2026h";
    const END: &[u8] = b"\x1b[?2026l";
    loop {
        let begin = bytes.windows(BEGIN.len()).position(|part| part == BEGIN);
        let end = bytes.windows(END.len()).position(|part| part == END);
        let next = match (begin, end) {
            (Some(a), Some(b)) if a <= b => Some((a, BEGIN)),
            (Some(_), Some(b)) => Some((b, END)),
            (Some(a), None) => Some((a, BEGIN)),
            (None, Some(b)) => Some((b, END)),
            (None, None) => None,
        };
        let Some((index, marker)) = next else {
            return out.write_all(bytes);
        };
        out.write_all(&bytes[..index])?;
        wrap(out, marker)?;
        bytes = &bytes[index + marker.len()..];
    }
}

#[cfg(test)]
mod tests {
    use super::super::kitty::{self, Placement};
    use super::*;

    const PANE: PaneRect = PaneRect::new(4, 1, 80, 24);

    #[cfg(target_os = "linux")]
    #[test]
    fn polling_is_disabled_for_root_elevated_or_capable_processes() {
        let ordinary =
            "Uid:\t1000 1000 1000 1000\nGid:\t1000 1000 1000 1000\nCapEff:\t0000000000000000\n";
        assert!(unprivileged_status(ordinary));
        assert!(!unprivileged_status(
            "Uid:\t0 0 0 0\nGid:\t0 0 0 0\nCapEff:\t0000000000000000\n"
        ));
        assert!(!unprivileged_status(
            "Uid:\t1000 0 0 0\nGid:\t1000 1000 1000 1000\nCapEff:\t0000000000000000\n"
        ));
        assert!(!unprivileged_status(
            "Uid:\t1000 1000 1000 1000\nGid:\t1000 1000 1000 1000\nCapEff:\t0000000000000001\n"
        ));
    }

    #[test]
    fn pane_options_carry_the_effective_passthrough() {
        assert_eq!(
            parse_pane_options("@3 on\n"),
            Some(("@3".to_owned(), "on".to_owned()))
        );
        assert_eq!(
            parse_pane_options("@12 all\n"),
            Some(("@12".to_owned(), "all".to_owned()))
        );
        // tmux before 3.3 has no such option: the format expands to nothing.
        assert_eq!(
            parse_pane_options("@3 \n"),
            Some(("@3".to_owned(), String::new()))
        );
        assert_eq!(parse_pane_options("garbage on"), None);
        assert_eq!(parse_pane_options(&"x".repeat(MAX_VIEWER_LINE + 1)), None);
    }

    #[test]
    fn clients_are_those_of_the_panes_window_with_their_terminal() {
        let text = "101|@3|screen-256color|\n102|@4|xterm-kitty|kitty(0.35.2)\n103|@3|xterm-256color|WezTerm 20240203 x\n";
        let clients = parse_clients(text, "@3").unwrap();
        assert_eq!(clients.len(), 2);
        assert_eq!(clients[0].pid, 101);
        assert_eq!(clients[0].term, "screen-256color");
        assert_eq!(clients[0].version, "");
        assert_eq!(clients[1].version, "WezTerm 20240203 x");
        // A '|' in the last field stays in it.
        let clients = parse_clients("7|@1|xterm|a|b\n", "@1").unwrap();
        assert_eq!(clients[0].version, "a|b");
        // No client at all is an answer too.
        assert_eq!(parse_clients("", "@1"), Some(Vec::new()));
    }

    #[test]
    fn malformed_or_oversized_client_lists_are_refused() {
        assert_eq!(parse_clients("x|@1|a|b\n", "@1"), None);
        assert_eq!(parse_clients("1|window|a|b\n", "@1"), None);
        assert_eq!(parse_clients("1|@1|a\n", "@1"), None);
        assert_eq!(
            parse_clients(&"1".repeat(MAX_COMMAND_OUTPUT + 1), "@1"),
            None
        );
        let many = "1|@1|a|b\n".repeat(MAX_CLIENTS + 1);
        assert_eq!(parse_clients(&many, "@1"), None);
    }

    #[test]
    fn a_probe_of_a_generic_term_names_what_tmux_reported() {
        let clients = parse_clients("9|@1|screen-256color|\n", "@1").unwrap();
        let probe = Probe {
            passthrough: "all".to_owned(),
            clients,
        };
        let reason = probe.verdict(false).unwrap_err();
        assert!(reason.contains("screen-256color"), "{reason}");
        assert_eq!(probe.verdict(true), Ok(()));
    }

    #[test]
    fn visible_viewers_filter_other_windows_and_zoom_hidden_panes() {
        let clients = "/dev/pts/2 @7\n/dev/pts/1 @3\n";
        assert_eq!(
            visible_viewers(true, "@7", clients),
            Some(vec!["/dev/pts/2".to_owned()])
        );
        assert_eq!(visible_viewers(false, "@7", clients), Some(Vec::new()));
    }

    #[test]
    fn visible_viewers_reject_unbounded_or_malformed_client_output() {
        assert_eq!(
            visible_viewers(true, "@1", &"x".repeat(MAX_COMMAND_OUTPUT + 1)),
            None
        );
        assert_eq!(visible_viewers(true, "@1", "not-a-pane id"), None);
        assert_eq!(
            visible_viewers(true, "@1", &format!("{} @1", "x".repeat(MAX_VIEWER_LINE))),
            None
        );
    }

    #[test]
    fn outside_tmux_non_graphics_bytes_are_unchanged() {
        let input = b"\x1b[2;3Hplain\x1b[?2026h";
        let mut output = Vec::new();
        write_with_tmux(&mut output, input, false, PANE).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn direct_initial_image_uses_outer_pane_origin() {
        let mut input = Vec::new();
        kitty::place_rgb(
            &mut input,
            Placement {
                image_id: 7,
                col: 2,
                row: 3,
                width_px: 1,
                height_px: 1,
                cell_cols: 1,
                cell_rows: 1,
            },
            &[0xff, 0, 0],
        );
        let mut output = Vec::new();
        write_with_tmux(&mut output, &input, false, PANE).unwrap();

        assert!(output.windows(6).any(|bytes| bytes == b"\x1b[5;7H"));
        assert!(output.windows(6).any(|bytes| bytes == b"\x1b_Ga=t"));
        assert!(output.windows(6).any(|bytes| bytes == b"\x1b_Ga=p"));
        assert!(!output.windows(6).any(|bytes| bytes == b"\x1b[4;3H"));
    }

    #[test]
    fn direct_reused_image_uses_outer_pane_origin() {
        let mut input = Vec::new();
        kitty::place_existing(&mut input, 7, 2, 3, 4, 5);
        let mut output = Vec::new();
        write_with_tmux(&mut output, &input, false, PANE).unwrap();

        assert_eq!(output, b"\x1b[5;7H\x1b_Ga=p,q=2,z=-1,i=7,p=1,c=4,r=5\x1b\\");
    }

    #[test]
    fn tmux_overlays_use_explicit_pane_offsets_for_split_and_resized_rectangles() {
        let cases = [
            (
                PaneRect::new(4, 1, 12, 6),
                0,
                0,
                12,
                3,
                b"\x1b\x1b[2;5H".as_slice(),
            ),
            (
                PaneRect::new(20, 1, 12, 6),
                0,
                0,
                12,
                3,
                b"\x1b\x1b[2;21H".as_slice(),
            ),
            (
                PaneRect::new(10, 3, 6, 4),
                0,
                1,
                6,
                1,
                b"\x1b\x1b[5;11H".as_slice(),
            ),
            // This rectangle overlaps the first split's screen area; the
            // placement still remains bounded by the active pane's viewport.
            (
                PaneRect::new(8, 2, 6, 4),
                0,
                0,
                6,
                4,
                b"\x1b\x1b[3;9H".as_slice(),
            ),
        ];
        for (pane, col, row, cols, rows, expected_cup) in cases {
            assert!(u32::from(col) + u32::from(cols) <= u32::from(pane.cols));
            assert!(u32::from(row) + u32::from(rows) <= u32::from(pane.rows));
            let mut input = Vec::new();
            kitty::place_existing_at_z(&mut input, 7, col, row, cols, rows, -1);
            let mut output = Vec::new();
            write_with_tmux(&mut output, &input, true, pane).unwrap();
            assert!(
                output
                    .windows(expected_cup.len())
                    .any(|bytes| bytes == expected_cup),
                "placement missed pane origin {pane:?}"
            );
            assert!(
                output
                    .windows(b"z=-1,i=7".len())
                    .any(|bytes| bytes == b"z=-1,i=7")
            );
        }
    }

    #[test]
    fn tmux_translation_adds_the_actual_outer_pane_origin() {
        let mut input = Vec::new();
        kitty::place_existing(&mut input, 7, 1, 2, 4, 3);
        let mut output = Vec::new();
        write_with_tmux_origin(
            &mut output,
            &input,
            true,
            PaneRect::new(2, 1, 8, 6),
            (20, 7),
        )
        .unwrap();

        assert!(
            output
                .windows(b"\x1b\x1b[11;24H".len())
                .any(|bytes| bytes == b"\x1b\x1b[11;24H")
        );
    }

    #[test]
    fn viewer_pane_state_reports_its_tmux_origin() {
        assert_eq!(
            parse_viewer_pane_state("@3 1 17 5"),
            Some(("@3".to_owned(), true, (17, 5)))
        );
        assert_eq!(parse_viewer_pane_state("@3 1 65536 5"), None);
        assert_eq!(parse_viewer_pane_state("@3 2 17 5"), None);
    }

    #[test]
    fn translate_image_cursor_to_pane_absolute_cell() {
        let mut output = Vec::new();
        write_with_tmux(&mut output, b"\x1b[2;3H\x1b_Ga=T;AAAA\x1b\\", true, PANE).unwrap();
        assert_eq!(
            output,
            b"\x1bPtmux;\x1b\x1b7\x1b\x1b[3;7H\x1b\x1b_Ga=T;AAAA\x1b\x1b\\\x1b\x1b8\x1b\\"
        );
    }

    #[test]
    fn synchronized_output_markers_are_passthrough_too() {
        let mut output = Vec::new();
        write_with_tmux(&mut output, b"a\x1b[?2026hframe\x1b[?2026l", true, PANE).unwrap();
        assert_eq!(
            output,
            b"a\x1bPtmux;\x1b\x1b[?2026h\x1b\\frame\x1bPtmux;\x1b\x1b[?2026l\x1b\\"
        );
    }

    #[test]
    fn wrap_quotes_each_embedded_escape() {
        let mut output = Vec::new();
        wrap(&mut output, b"\x1b_Gx\x1b\\").unwrap();
        assert_eq!(output, b"\x1bPtmux;\x1b\x1b_Gx\x1b\x1b\\\x1b\\");
    }
}
