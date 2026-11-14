//! Interactive dashboard: every configured target with its live state,
//! details (a dialog), activity (the service's journal plus its own actions), and one-key (confirmed) unlocks.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::ctx::{Ctx, Mode};
use crate::daemon::State;
use crate::log::{Level, LogLine, Logger};
use crate::settings::{GlobalOpts, Settings, parse_env_file};
use crate::status::{Report, Severity, TargetState};
use crate::vnc_graphics::{self, Renderer};
use chrono::{DateTime, Local, TimeZone};

mod helper_worker;
mod viewer;
mod vnc;

const LOG_LINES: usize = 500;
const EXTRA_LINES: usize = 2000;
/// How long an extra command gets to clean up after SIGTERM before SIGKILL.
const EXTRA_GRACE: Duration = Duration::from_secs(5);
/// Minimum time between two checks triggered by new journal lines.
const EVENT_PROBE_GAP: Duration = Duration::from_secs(2);
const WHEEL_LINES: usize = 3;
pub const DEFAULT_CONFIG_DIR: &str = "/etc/luks-ssh-unlock";

/// An extra command whose output is shown in an optional fourth pane.
#[derive(Clone)]
pub struct ExtraCmd {
    pub label: String,
    pub command: String,
    /// Runs on a pseudo-terminal in an emulated screen, and gets the
    /// keyboard while its pane has the focus (`@` before the command).
    pub interactive: bool,
    /// A direct VNC endpoint, shown by the built-in viewer: no command runs
    /// (`command` is only a description), and the pane has the keyboard
    /// like an interactive console does.
    pub vnc: Option<crate::console::VncEndpoint>,
}

impl ExtraCmd {
    /// Does this pane take the keyboard (an interactive command or VNC)?
    fn is_console(&self) -> bool {
        self.interactive || self.vnc.is_some()
    }

    /// `[LABEL=][@]COMMAND`. Without a label, the command's first word is
    /// used. `@` makes the command interactive (a console you type into).
    pub fn parse(spec: &str) -> Self {
        let (label, command) = match spec.split_once('=') {
            Some((label, command))
                if !label.is_empty()
                    && label
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')) =>
            {
                (Some(label), command)
            }
            _ => (None, spec),
        };
        let command = command.trim();
        let (interactive, command) = match command.strip_prefix('@') {
            Some(rest) => (true, rest.trim_start()),
            None => (false, command),
        };
        Self {
            label: label
                .map(str::to_string)
                .unwrap_or_else(|| command.split_whitespace().next().unwrap_or(command).into()),
            command: command.into(),
            interactive,
            vnc: None,
        }
    }
}

/// Extra commands from a file: one `[LABEL=]COMMAND` per line, `#` comments.
fn read_extras(path: &std::path::Path) -> Vec<ExtraCmd> {
    fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(ExtraCmd::parse)
                .collect()
        })
        .unwrap_or_default()
}

/// Extra commands from `/etc/luks-ssh-unlock/tui-commands`, used when none
/// are given on the command line. The NixOS module writes it.
pub fn default_extras() -> Vec<ExtraCmd> {
    read_extras(&PathBuf::from(DEFAULT_CONFIG_DIR).join("tui-commands"))
}

/// A target's commands: the common ones, with its own `<name>.tui-commands`
/// replacing those of the same label or adding new ones. An empty command
/// removes the label for this target.
fn merge_extras(common: &[ExtraCmd], own: Vec<ExtraCmd>) -> Vec<ExtraCmd> {
    let mut out = common.to_vec();
    for cmd in own {
        match out.iter_mut().find(|c| c.label == cmd.label) {
            Some(slot) => *slot = cmd,
            None => out.push(cmd),
        }
    }
    out.retain(|c| !c.command.is_empty());
    out
}

/// Output of one extra command for one target.
#[derive(Default)]
struct ExtraOut {
    lines: Vec<Line<'static>>,
    /// Which run the lines belong to; 0 when none is live.
    run_id: u64,
    running: bool,
    /// The next line of the run replaces the old output.
    restart: bool,
    finished_at: Option<Instant>,
    /// Interactive commands: the emulated terminal screen, with scrollback.
    screen: Option<vt100::Parser>,
    /// The end of the last output, in case a terminal query is split in two.
    query_tail: Vec<u8>,
    /// Until the console is there: its current step, and since when.
    loading: Option<(String, Instant)>,
    /// The console only shows output (it said so): it never gets the keyboard.
    read_only: bool,
    /// The start of a progress sequence cut at the end of a chunk.
    progress_tail: Vec<u8>,
    /// VNC panes: what the session last said (progress, or why it ended).
    vnc_note: String,
    /// VNC panes: the keyboard toggle (`Ctrl+T`); the configured value until set.
    vnc_view_only: Option<bool>,
}

/// The extra command that is running now (at most one: the selected
/// target's, in the active pane).
struct ExtraRun {
    t: usize,
    c: usize,
    /// Process group, signalled as a whole when the selection changes.
    pgid: i32,
    /// Set once the command has been reaped.
    ended: Arc<AtomicBool>,
    /// The pseudo-terminal of an interactive command.
    pty: Option<Pty>,
}

/// Events and pixel bytes taken from a VNC session per turn of the loop.
const VNC_EVENTS: usize = 64;
const VNC_BYTES: usize = 8 << 20;
/// How long the pane size has to hold still before the remote desktop is
/// asked to follow it (only with `resizeRemote`).
/// How long a tmux session gets to say whether it passes images through.
const VNC_TMUX_PROBE: Duration = Duration::from_secs(2);
const VNC_RESIZE_DELAY: Duration = Duration::from_millis(400);
/// Smallest size worth asking a remote desktop for.
const VNC_RESIZE_MIN: u16 = 160;

/// The built-in VNC viewer's live session (at most one: the selected
/// target's, in the active pane).
struct VncRun {
    t: usize,
    c: usize,
    session: vnc::Session,
    /// The first pixels have arrived: the image can be drawn.
    framed: bool,
    layout: Option<crate::vnc::ScreenLayout>,
    /// Pointer buttons held on the remote desktop, and where it is.
    buttons: u8,
    pointer: (u16, u16),
    /// The pane size wanted by the remote desktop, and since when; the last
    /// size that was asked for.
    resize: Option<((u16, u16), Instant)>,
    resized: Option<(u16, u16)>,
    /// The guest's keyboard layout, and whether the server takes scancodes
    /// (keys then go as the guest's layout types them).
    keymap: crate::vnc::keymap::Layout,
    scancodes: bool,
    /// When the session was started, for the loading screen.
    started: Instant,
}

/// Everything the VNC viewer keeps between frames.
#[derive(Default)]
struct VncState {
    run: Option<VncRun>,
    /// Terminal graphics were looked for since the viewer last (re)started.
    checked: bool,
    /// When the terminal was first asked about graphics (inside tmux that is
    /// answered by a background thread, a moment later).
    probing: Option<Instant>,
    /// What was found, or why the viewer cannot draw.
    transport: Option<vnc_graphics::Transport>,
    problem: Option<String>,
    renderer: Option<Renderer>,
    /// A renderer that was let go of, whose images are deleted at the next draw.
    retired: Option<Renderer>,
    /// The inside of the pane the image goes in; set while drawing a frame, and
    /// only when the image is to be seen.
    area: Option<Rect>,
    /// The image is drawn with Unicode placeholders (tmux), not placed.
    placeholders: bool,
    /// Drawing waits for the rate limit.
    more: bool,
}

impl VncState {
    /// Let go of the renderer; its images go away at the next draw.
    fn retire(&mut self) {
        if let Some(r) = self.renderer.take() {
            self.retired = Some(r);
        }
        self.area = None;
        self.placeholders = false;
        self.more = false;
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn sgr(mut style: Style, params: &str) -> Style {
    let nums: Vec<u16> = params
        .split([';', ':'])
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let mut i = 0;
    while i < nums.len() {
        let n = nums[i];
        match n {
            0 => style = Style::new(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            30..=37 => style = style.fg(Color::Indexed((n - 30) as u8)),
            90..=97 => style = style.fg(Color::Indexed((n - 90 + 8) as u8)),
            39 => style = style.fg(Color::Reset),
            40..=47 => style = style.bg(Color::Indexed((n - 40) as u8)),
            100..=107 => style = style.bg(Color::Indexed((n - 100 + 8) as u8)),
            49 => style = style.bg(Color::Reset),
            38 | 48 => {
                let color = match nums.get(i + 1) {
                    Some(5) => {
                        let c = nums.get(i + 2).map(|&v| Color::Indexed(v as u8));
                        i += 2;
                        c
                    }
                    Some(2) if i + 4 < nums.len() => {
                        let c = Color::Rgb(nums[i + 2] as u8, nums[i + 3] as u8, nums[i + 4] as u8);
                        i += 4;
                        Some(c)
                    }
                    _ => None,
                };
                if let Some(c) = color {
                    style = if n == 38 { style.fg(c) } else { style.bg(c) };
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

/// One line of command output with its colors (SGR escapes) kept and every
/// other escape sequence and control character dropped: they would garble
/// the screen.
fn ansi_line(text: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::new();
    let mut cur = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut params = String::new();
                    let mut last = None;
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            last = Some(n);
                            break;
                        }
                        params.push(n);
                    }
                    if last == Some('m') {
                        if !cur.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut cur), style));
                        }
                        style = sgr(style, &params);
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\t' => cur.push_str("    "),
            c if c.is_control() => {}
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, style));
    }
    Line::from(spans)
}

/// The `sh -c` invocation of an extra command, with `{host}` and `{name}`
/// replaced by the quoted values and also exported as
/// `LUKS_SSH_UNLOCK_HOST` and `LUKS_SSH_UNLOCK_NAME`. Standard streams and
/// the process group are up to the caller.
fn extra_shell(command: &str, (name, host): (&str, &str), console: bool) -> Command {
    let script = command
        .replace("{host}", &shell_quote(host))
        .replace("{name}", &shell_quote(name));
    // Under sudo (the dashboard re-runs itself as root), run the command as
    // the user who started it: their tools and credentials live there.
    let user = std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.is_empty() && u != "root" && unsafe { libc::geteuid() } == 0)
        .unwrap_or_default();
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        r#"exec 2>&1
if [ -n "$2" ]; then
  exec sudo -n -H -u "$2" -- env "LUKS_SSH_UNLOCK_NAME=$3" "LUKS_SSH_UNLOCK_HOST=$4" "LUKS_SSH_UNLOCK_TUI=$5" sh -c "$1"
fi
LUKS_SSH_UNLOCK_NAME=$3 LUKS_SSH_UNLOCK_HOST=$4 LUKS_SSH_UNLOCK_TUI=$5
export LUKS_SSH_UNLOCK_NAME LUKS_SSH_UNLOCK_HOST LUKS_SSH_UNLOCK_TUI
exec sh -c "$1""#,
        "sh",
        &script,
        &user,
        name,
        host,
        // Our own console commands report their progress to the dashboard
        // (crate::progress) when they run on its terminal.
        if console { "1" } else { "" },
    ]);
    cmd
}

/// Do not leave the command running when the dashboard dies.
fn die_with_parent() -> std::io::Result<()> {
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
    Ok(())
}

/// Start an extra command for a target and stream its output (stdout and
/// stderr) to the dashboard line by line. Returns its process group and a
/// flag that is set once it has been reaped.
fn spawn_extra(
    command: &str,
    (t, c, run_id): (usize, usize, u64),
    target: (&str, &str),
    tx: Sender<Msg>,
) -> std::io::Result<(i32, Arc<AtomicBool>)> {
    let mut cmd = extra_shell(command, target, false);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    unsafe { cmd.pre_exec(die_with_parent) };
    let mut child = cmd.spawn()?;
    let pgid = child.id() as i32;
    let out = child.stdout.take().expect("stdout is piped");
    let ended = Arc::new(AtomicBool::new(false));
    let done = ended.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(out);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buf);
                    let line = ansi_line(text.trim_end_matches(['\n', '\r']));
                    if tx.send(Msg::ExtraLine(t, c, run_id, line)).is_err() {
                        break;
                    }
                }
            }
        }
        let code = child.wait().ok().and_then(|s| s.code());
        done.store(true, Ordering::SeqCst);
        let _ = tx.send(Msg::ExtraDone(t, c, run_id, code));
    });
    Ok((pgid, ended))
}

/// Input the child has not read yet is dropped beyond this many bytes.
const PTY_PENDING_MAX: usize = 256 * 1024;

/// The master side of an interactive command's pseudo-terminal.
struct Pty {
    fd: OwnedFd,
    /// Input the child has not taken yet (the master never blocks).
    pending: Vec<u8>,
    /// Rows and columns the child was last told.
    size: (u16, u16),
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

impl Pty {
    /// Tell the child about a new window size (it gets SIGWINCH).
    fn resize(&mut self, rows: u16, cols: u16) {
        if self.size != (rows, cols) {
            self.size = (rows, cols);
            let ws = winsize(rows, cols);
            unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        }
    }

    /// Send input to the child.
    fn write(&mut self, bytes: &[u8]) {
        if self.pending.len() + bytes.len() <= PTY_PENDING_MAX {
            self.pending.extend_from_slice(bytes);
        }
        self.flush();
    }

    /// Hand over what the child will take now; the rest waits.
    fn flush(&mut self) {
        while !self.pending.is_empty() {
            let n = unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    self.pending.as_ptr().cast(),
                    self.pending.len(),
                )
            };
            if n > 0 {
                self.pending.drain(..n as usize);
            } else if n < 0 && last_errno() == libc::EINTR {
                continue;
            } else {
                // Full for now (EAGAIN), or the child is gone.
                if n < 0 && last_errno() != libc::EAGAIN {
                    self.pending.clear();
                }
                break;
            }
        }
    }
}

/// Start an interactive extra command on a new pseudo-terminal of the given
/// size, as the leader of its own session. Its output (raw bytes) goes to
/// the dashboard, which feeds it to a terminal emulator. Returns its process
/// group, a flag that is set once it has been reaped, and the terminal.
fn spawn_console(
    command: &str,
    (t, c, run_id): (usize, usize, u64),
    target: (&str, &str),
    (rows, cols): (u16, u16),
    tx: Sender<Msg>,
    force_reload: bool,
) -> std::io::Result<(i32, Arc<AtomicBool>, Pty)> {
    let (mut m, mut s) = (-1, -1);
    let ws = winsize(rows, cols);
    if unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null(), &ws) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openpty returned two new descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    for fd in [&master, &slave] {
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    // Reading waits in poll(); writing must never block the dashboard.
    unsafe {
        let fl = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(master.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
    }
    let mut cmd = extra_shell(command, target, true);
    if force_reload {
        cmd.env("LUKS_SSH_UNLOCK_OCI_CLOSE_OTHERS", "1");
    }
    cmd.env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    unsafe {
        cmd.pre_exec(|| {
            // A session of its own, with the terminal on stdin as its
            // controlling terminal: job control and Ctrl+C work in it.
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 0);
            die_with_parent()
        });
    }
    let mut child = cmd.spawn()?;
    // The parent's copies of the slave live in `cmd`: without them, the
    // master sees the end of the output when the child is gone.
    drop(cmd);
    let pgid = child.id() as i32;
    let reader = master.try_clone()?;
    let ended = Arc::new(AtomicBool::new(false));
    let done = ended.clone();
    thread::spawn(move || {
        let fd = reader.as_raw_fd();
        let mut buf = [0u8; 8192];
        let mut status = None;
        loop {
            let mut p = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut p, 1, 100) };
            if ready < 0 {
                if last_errno() == libc::EINTR {
                    continue;
                }
                break;
            }
            if ready == 0 {
                // Quiet: stop when the command is gone, even if something it
                // started still holds the terminal.
                status = child.try_wait().ok().flatten();
                if status.is_some() {
                    break;
                }
                continue;
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                let msg = Msg::ConsoleData(t, c, run_id, buf[..n as usize].to_vec());
                if tx.send(msg).is_err() {
                    break;
                }
            } else if n == 0 || !matches!(last_errno(), libc::EAGAIN | libc::EINTR) {
                break; // the end of the output (EIO once the child is gone)
            }
        }
        let code = match status {
            Some(s) => s.code(),
            None => child.wait().ok().and_then(|s| s.code()),
        };
        done.store(true, Ordering::SeqCst);
        let _ = tx.send(Msg::ExtraDone(t, c, run_id, code));
    });
    Ok((
        pgid,
        ended,
        Pty {
            fd: master,
            pending: Vec::new(),
            size: (rows, cols),
        },
    ))
}

/// The keys the dashboard keeps for itself while a console has the keyboard:
/// `Ctrl+]` hands it back to the dashboard (like telnet's escape; legacy
/// terminals send it as `Ctrl+5`, which is the same byte).
fn is_console_escape(code: KeyCode, mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char(']' | '5'))
}

/// `Ctrl+R` or `F5` restart the console.
fn is_console_restart(code: KeyCode, mods: KeyModifiers) -> bool {
    (mods.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('r' | 'R')))
        || (code == KeyCode::F(5) && mods.is_empty())
}

/// The control character of `Ctrl+KEY`. crossterm reports the legacy bytes
/// 0x1c to 0x1f as `Ctrl+4` to `Ctrl+7`.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        ' ' | '@' => Some(0),
        '[' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// The bytes a terminal sends for a key (xterm style). `app_cursor` is the
/// program's cursor keys mode (DECCKM).
fn encode_key(code: KeyCode, mods: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    let shift = mods.contains(KeyModifiers::SHIFT);
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    // xterm's modifier parameter: 1 plus shift 1, alt 2, ctrl 4.
    let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let cursor = |f: char| match (m, app_cursor) {
        (1, false) => format!("\x1b[{f}"),
        (1, true) => format!("\x1bO{f}"),
        _ => format!("\x1b[1;{m}{f}"),
    };
    let tilde = |n: u8| match m {
        1 => format!("\x1b[{n}~"),
        _ => format!("\x1b[{n};{m}~"),
    };
    let meta = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };
    match code {
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let plain = c.encode_utf8(&mut buf).as_bytes().to_vec();
            meta(match ctrl.then(|| ctrl_byte(c)).flatten() {
                Some(b) => vec![b],
                None => plain,
            })
        }
        KeyCode::Enter => meta(vec![b'\r']),
        KeyCode::Tab => meta(vec![b'\t']),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => meta(vec![0x7f]),
        KeyCode::Esc => meta(vec![0x1b]),
        KeyCode::Up => cursor('A').into_bytes(),
        KeyCode::Down => cursor('B').into_bytes(),
        KeyCode::Right => cursor('C').into_bytes(),
        KeyCode::Left => cursor('D').into_bytes(),
        KeyCode::Home => cursor('H').into_bytes(),
        KeyCode::End => cursor('F').into_bytes(),
        KeyCode::Insert => tilde(2).into_bytes(),
        KeyCode::Delete => tilde(3).into_bytes(),
        KeyCode::PageUp => tilde(5).into_bytes(),
        KeyCode::PageDown => tilde(6).into_bytes(),
        KeyCode::F(n @ 1..=4) => {
            let f = ['P', 'Q', 'R', 'S'][usize::from(n) - 1];
            match m {
                1 => format!("\x1bO{f}"),
                _ => format!("\x1b[1;{m}{f}"),
            }
            .into_bytes()
        }
        KeyCode::F(n @ 5..=12) => {
            tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n) - 5]).into_bytes()
        }
        KeyCode::Null => vec![0],
        _ => Vec::new(),
    }
}

/// Answers to the terminal queries in program output that the emulator does
/// not answer itself: cursor position (`CSI 6 n`), status (`CSI 5 n`) and
/// device attributes (`CSI c`). Shells and editors wait for these.
fn terminal_replies(output: &[u8], (row, col): (u16, u16)) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, _) in output.iter().enumerate().filter(|(_, b)| **b == 0x1b) {
        let rest = &output[i + 1..];
        if rest.starts_with(b"[6n") {
            out.extend(format!("\x1b[{};{}R", row + 1, col + 1).bytes());
        } else if rest.starts_with(b"[5n") {
            out.extend(b"\x1b[0n");
        } else if rest.starts_with(b"[c") || rest.starts_with(b"[0c") {
            out.extend(b"\x1b[?1;2c");
        }
    }
    out
}

/// Where a terminal query that may continue in the next chunk of output
/// starts, if one is cut off at the end of `output`.
fn partial_query(output: &[u8]) -> usize {
    output
        .iter()
        .rposition(|&b| b == 0x1b)
        .filter(|&p| {
            output.len() - p <= 3
                && !output[p..]
                    .iter()
                    .skip(2)
                    .any(|b| (b'@'..=b'~').contains(b))
        })
        .unwrap_or(output.len())
}

/// Draw the emulated screen into the area.
fn paint_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
    let color = |c: vt100::Color| match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    };
    for row in 0..area.height {
        for col in 0..area.width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut style = Style::new()
                .fg(color(cell.fgcolor()))
                .bg(color(cell.bgcolor()));
            for (on, m) in [
                (cell.bold(), Modifier::BOLD),
                (cell.italic(), Modifier::ITALIC),
                (cell.underline(), Modifier::UNDERLINED),
                (cell.inverse(), Modifier::REVERSED),
            ] {
                if on {
                    style = style.add_modifier(m);
                }
            }
            let text = cell.contents();
            let dst = &mut buf[(area.x + col, area.y + row)];
            dst.set_symbol(if text.is_empty() { " " } else { text });
            dst.set_style(style);
        }
    }
}

/// Whether, and how, the privileged helper acts for a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Privileged {
    /// Never: the target is not the system one (another directory, or the
    /// current options), so the helper knows nothing about it.
    Never,
    /// The system configuration was readable. The helper takes over only if
    /// something it refers to (key, passphrase, baseline) is not.
    IfNeeded,
    /// Not even the configuration is readable by the user; the helper's
    /// listing supplied the host name.
    Always(String),
}

/// One target to show: a name and the environment describing it.
pub struct Source {
    pub name: String,
    /// This target's own extra commands (`<name>.tui-commands`).
    pub extras: Vec<ExtraCmd>,
    pub env: Option<std::collections::HashMap<String, String>>,
    pub privileged: Privileged,
    /// The label of the view to show first (a sidecar's `"default": true`).
    pub default_view: Option<String>,
}

/// Put the default view first, so that it is what the target opens with.
fn default_first(mut cmds: Vec<ExtraCmd>, default: Option<&str>) -> Vec<ExtraCmd> {
    if let Some(i) = default.and_then(|label| cmds.iter().position(|c| c.label == label)) {
        let cmd = cmds.remove(i);
        cmds.insert(0, cmd);
    }
    cmds
}

fn permission_denied(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
}

fn helper_listing() -> Result<Vec<crate::helper::client::Listed>> {
    crate::helper::client::Client::system()
        .list()
        .map_err(anyhow::Error::new)
}

/// Environment files given on the command line, or every `*.env` in the
/// NixOS module's directory. What the user cannot read there (the directory,
/// or a file in it) is listed by the privileged helper instead; the sidecar
/// files next to the environment files are always read directly, as the user,
/// since they hold no protected material.
pub fn discover(files: &[PathBuf]) -> Result<Vec<Source>> {
    let system_dir = std::path::Path::new(DEFAULT_CONFIG_DIR);
    let mut listing: Option<Vec<crate::helper::client::Listed>> = None;
    let paths: Vec<PathBuf> = if files.is_empty() {
        let mut found: Vec<PathBuf> = match fs::read_dir(DEFAULT_CONFIG_DIR) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "env"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let listed = helper_listing().with_context(|| {
                    format!(
                        "cannot read {DEFAULT_CONFIG_DIR}, and the privileged helper cannot list it"
                    )
                })?;
                let paths = listed
                    .iter()
                    .map(|t| system_dir.join(format!("{}.env", t.name)))
                    .collect();
                listing = Some(listed);
                paths
            }
            Err(_) => Vec::new(),
        };
        found.sort();
        found
    } else {
        let mut out = Vec::new();
        for f in files {
            if f.is_dir() {
                let mut found: Vec<PathBuf> = fs::read_dir(f)?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|e| e == "env"))
                    .collect();
                found.sort();
                out.extend(found);
            } else {
                out.push(f.clone());
            }
        }
        out
    };
    paths
        .iter()
        .map(|p| {
            let name = p
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            let in_system = p.parent() == Some(system_dir);
            let (env, privileged) = match fs::read_to_string(p) {
                Ok(text) => (
                    parse_env_file(&text),
                    if in_system {
                        Privileged::IfNeeded
                    } else {
                        Privileged::Never
                    },
                ),
                Err(e) if in_system && e.kind() == std::io::ErrorKind::PermissionDenied => {
                    if listing.is_none() {
                        listing = Some(helper_listing().with_context(|| {
                            format!(
                                "cannot read {}, and the privileged helper cannot list it",
                                p.display()
                            )
                        })?);
                    }
                    let host = listing
                        .iter()
                        .flatten()
                        .find(|t| t.name == name)
                        .and_then(|t| t.host.clone())
                        .with_context(|| {
                            format!(
                                "cannot read {}, and the privileged helper does not know it",
                                p.display()
                            )
                        })?;
                    (
                        std::collections::HashMap::from([(
                            "SSH_HOSTNAME".to_string(),
                            host.clone(),
                        )]),
                        Privileged::Always(host),
                    )
                }
                Err(e) => {
                    return Err(
                        anyhow::Error::new(e).context(format!("cannot read {}", p.display()))
                    );
                }
            };
            // The sidecars are the user's to read; the ones of a target whose
            // directory is closed to the user are simply absent.
            let lenient = matches!(privileged, Privileged::Always(_));
            let mut extras = read_extras(&p.with_extension("tui-commands"));
            let console = crate::console::read(&p.with_extension("console.json"), &name);
            let vnc = crate::console::read_vnc(&p.with_extension("vnc.json"), &name);
            let mut default_view = None;
            for sidecar in [console, vnc] {
                match sidecar {
                    Ok(Some((extra, default))) => {
                        if default {
                            default_view = Some(extra.label.clone());
                        }
                        extras = merge_extras(&extras, vec![extra]);
                    }
                    Ok(None) => {}
                    Err(e) if lenient && permission_denied(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(Source {
                extras,
                name,
                env: Some(env),
                privileged,
                default_view,
            })
        })
        .collect()
}

enum Cmd {
    Probe,
    Unlock {
        force: bool,
    },
    /// Start (`true`) or stop the target's systemd service.
    Service(bool),
}

enum Msg {
    Report(usize, Box<Report>),
    Log(usize, LogLine),
    Busy(usize, Option<&'static str>),
    Done(usize, Option<State>),
    /// The service journal's recent history, once at start.
    History(usize, Vec<Entry>),
    /// One new entry of the service journal.
    Service(usize, Entry),
    Unit(usize, Option<UnitInfo>),
    /// Output of extra command `.1` of target `.0`, from run `.2`.
    ExtraLine(usize, usize, u64, Line<'static>),
    /// Raw terminal output of interactive command `.1` of target `.0`, run `.2`.
    ConsoleData(usize, usize, u64, Vec<u8>),
    /// The run ended; the exit code, or none when it was killed by a signal.
    ExtraDone(usize, usize, u64, Option<i32>),
}

/// What systemd says about the target's service.
struct UnitInfo {
    load: String,
    active: String,
    sub: String,
    enabled: String,
}

impl UnitInfo {
    fn query(unit: &str) -> Option<Self> {
        let out = Command::new("systemctl")
            .args([
                "show",
                "--property=LoadState,ActiveState,SubState,UnitFileState",
                unit,
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let get = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
                .unwrap_or_default()
                .to_string()
        };
        Some(Self {
            load: get("LoadState"),
            active: get("ActiveState"),
            sub: get("SubState"),
            enabled: get("UnitFileState"),
        })
    }

    /// Why the service will not unlock this target on its own, if it won't.
    fn warning(&self) -> Option<String> {
        if self.load == "not-found" {
            return None; // not run as a service here
        }
        if self.active != "active" {
            return Some(format!("service {} ({})", self.active, self.sub));
        }
        match self.enabled.as_str() {
            "disabled" | "masked" => Some(format!(
                "service is {} and will not start at boot",
                self.enabled
            )),
            _ => None,
        }
    }
}

struct Target {
    name: String,
    host: String,
    error: Option<String>,
    report: Option<Report>,
    busy: Option<&'static str>,
    last_unlock: Option<(Instant, Option<State>)>,
    logs: VecDeque<Entry>,
    unit: Option<UnitInfo>,
    tx: Option<Sender<Cmd>>,
    /// Extra commands of this target, and their output.
    cmds: Vec<ExtraCmd>,
    extra: Vec<ExtraOut>,
}

/// One line of the activity pane: something the service logged to its
/// journal, or something this dashboard did.
#[derive(Clone)]
struct Entry {
    at: DateTime<Local>,
    level: Level,
    service: bool,
    message: String,
}

impl Entry {
    fn own(l: LogLine) -> Self {
        Self {
            at: l.at,
            level: l.level,
            service: false,
            message: l.message,
        }
    }

    fn service_note(level: Level, message: String) -> Self {
        Self {
            at: Local::now(),
            level,
            service: true,
            message,
        }
    }

    /// From one line of `journalctl --output=json`.
    fn from_journal(line: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let message = v.get("MESSAGE")?.as_str()?.lines().next()?.to_string();
        let micros: i64 = v.get("__REALTIME_TIMESTAMP")?.as_str()?.parse().ok()?;
        let level = match v
            .get("PRIORITY")
            .and_then(|p| p.as_str())
            .and_then(|p| p.parse::<u8>().ok())
        {
            Some(0..=3) => Level::Error,
            Some(4) => Level::Warning,
            Some(7) => Level::Debug,
            _ => Level::Info,
        };
        Some(Self {
            at: Local.timestamp_micros(micros).single()?,
            level,
            service: true,
            message,
        })
    }
}

/// Which pane keyboard input is aimed at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Activity,
    Extra,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// A pane border that can be dragged with the mouse.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// Between the console (above) and the activity.
    Extra,
}

struct App {
    targets: Vec<Target>,
    list: ListState,
    confirm: Option<(usize, bool)>,
    refresh: Duration,
    quit: bool,
    /// Lines scrolled up from the bottom of the log pane.
    log_scroll: usize,
    /// Narrow or tall terminal (a phone): panes are stacked.
    portrait: bool,
    focus: Focus,
    /// The activity fills the whole screen (`l`).
    logs_full: bool,
    /// The extra command pane fills the whole screen (`C`).
    extra_full: bool,
    /// The key help is shown (`?`).
    help: bool,
    /// The selected target's details are shown (`i`).
    info: bool,
    drag: Option<Edge>,
    /// Screen areas of the last frame, for mouse hit-testing.
    hit: Hit,
    /// The host switcher (`Ctrl+P`, `/`): its filter, whether it is open, and
    /// the host that was selected when it opened (Esc goes back to it).
    filter: String,
    filtering: bool,
    switch_from: Option<usize>,
    /// Optional extra commands, shown in a fourth pane; `c` switches.
    has_extras: bool,
    extra_idx: usize,
    extra_scroll: usize,
    extra_run: Option<ExtraRun>,
    extra_gen: u64,
    /// Rows and columns of the console pane at the last frame, for the next
    /// interactive command to start with.
    console_size: (u16, u16),
    /// A console was asked to restart: wait until the old one has ended.
    restart_wait: Option<Arc<AtomicBool>>,
    /// The next console start is a restart (Ctrl+R): it closes the
    /// instance's other OCI console connections whatever the configuration.
    force_reload: bool,
    /// Height of the extra pane, in percent of the space it shares with the activity.
    extra_pct: u16,
    /// The built-in VNC viewer.
    vnc: VncState,
    msg_tx: Sender<Msg>,
}

#[derive(Default)]
struct Hit {
    body: Rect,
    /// The host line on top (a click opens the switcher).
    header: Rect,
    /// The switcher popup, and its rows of hosts.
    switcher: Rect,
    list: Rect,
    logs: Rect,
    extra: Rect,
    /// The activity and the extra pane together, and the border between them.
    split: Rect,
    edge_extra: Rect,
    yes: Rect,
    cancel: Rect,
    dialog: Rect,
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn worker(
    idx: usize,
    mut ctx: Ctx,
    unit: String,
    rx: Receiver<Cmd>,
    tx: Sender<Msg>,
    refresh: Duration,
) {
    let mut next = Instant::now();
    let mut last_state: Option<String> = None;
    loop {
        let mut manual = true;
        let cmd = match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => {
                manual = false;
                Cmd::Probe
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match cmd {
            Cmd::Probe => {
                let _ = tx.send(Msg::Busy(idx, Some("checking")));
                ctx.mode = Mode::Status;
                match ctx.status(false) {
                    Ok(report) => {
                        // Periodic checks only log when something changed.
                        let now = describe(&report);
                        if manual || last_state.as_deref() != Some(now.as_str()) {
                            ctx.log.info(match &last_state {
                                Some(prev) if *prev != now => format!("Check: {prev} → {now}"),
                                _ => format!("Check: {now}"),
                            });
                        }
                        last_state = Some(now);
                        let _ = tx.send(Msg::Report(idx, Box::new(report)));
                    }
                    Err(e) => ctx.log.error(format!("Status check failed: {e:#}")),
                }
                let _ = tx.send(Msg::Unit(idx, UnitInfo::query(&unit)));
                next = Instant::now() + refresh;
            }
            Cmd::Service(start) => {
                let verb = if start { "start" } else { "stop" };
                let _ = tx.send(Msg::Busy(
                    idx,
                    Some(if start {
                        "starting service"
                    } else {
                        "stopping service"
                    }),
                ));
                ctx.log.info(format!("systemctl {verb} {unit}"));
                match Command::new("systemctl")
                    .args([verb, "--no-ask-password", &unit])
                    .stdin(Stdio::null())
                    .output()
                {
                    Ok(o) if o.status.success() => {
                        ctx.log.info(format!(
                            "Service {}",
                            if start { "started" } else { "stopped" }
                        ));
                    }
                    Ok(o) => {
                        let err = String::from_utf8_lossy(&o.stderr);
                        ctx.log.error(format!(
                            "systemctl {verb} failed: {}{}",
                            err.lines().next().unwrap_or("no output"),
                            if unsafe { libc::geteuid() } == 0 {
                                ""
                            } else {
                                " (run as root?)"
                            }
                        ));
                    }
                    Err(e) => ctx.log.error(format!("cannot run systemctl: {e}")),
                }
                let _ = tx.send(Msg::Unit(idx, UnitInfo::query(&unit)));
            }
            Cmd::Unlock { force } => {
                let _ = tx.send(Msg::Busy(
                    idx,
                    Some(if force {
                        "force-unlocking"
                    } else {
                        "unlocking"
                    }),
                ));
                ctx.mode = Mode::Run;
                ctx.force = force;
                ctx.state = None;
                let state = match ctx.load_passphrase() {
                    Ok(()) => ctx.tick_logged(),
                    Err(e) => {
                        ctx.log.error(format!("{e:#}"));
                        None
                    }
                };
                ctx.force = false;
                ctx.passphrase = None;
                let _ = tx.send(Msg::Done(idx, state));
                next = Instant::now();
            }
        }
        let _ = tx.send(Msg::Busy(idx, None));
    }
}

/// Show the service's journal live, and re-check the target whenever the
/// service logs something: it only logs on state changes and unlocks, so
/// this catches them without waiting for the next periodic check.
fn follow_journal(idx: usize, unit: String, probe: Sender<Cmd>, tx: Sender<Msg>) {
    let history = Command::new("journalctl")
        .args(["--no-pager", "--output=json", "-n", "200", "-u", &unit])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(Entry::from_journal)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let _ = tx.send(Msg::History(idx, history));

    let mut cmd = Command::new("journalctl");
    cmd.args(["--no-pager", "--output=json", "-n", "0", "-f", "-u", &unit])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Do not leave journalctl running when the dashboard exits.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Msg::Service(
                idx,
                Entry::service_note(Level::Warning, format!("journalctl failed: {e}")),
            ));
            return;
        }
    };
    let Some(out) = child.stdout.take() else {
        return;
    };
    let mut last_probe: Option<Instant> = None;
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        let Some(entry) = Entry::from_journal(&line) else {
            continue;
        };
        if tx.send(Msg::Service(idx, entry)).is_err() {
            break;
        }
        if last_probe.is_none_or(|t| t.elapsed() >= EVENT_PROBE_GAP) {
            last_probe = Some(Instant::now());
            if probe.send(Cmd::Probe).is_err() {
                break;
            }
        }
    }
    let _ = child.kill();
    let status = child.wait();
    let _ = tx.send(Msg::Service(
        idx,
        Entry::service_note(
            Level::Warning,
            format!(
                "journal follow ended ({}); service events are no longer shown, checks fall back to the periodic refresh (root or systemd-journal group needed)",
                status.map_or("unknown".into(), |s| s.to_string())
            ),
        ),
    ));
}

/// Set by the SIGTERM/SIGHUP handler; the event loop turns it into a quit,
/// so the screen, the mouse mode and any Kitty image are restored.
/// Holds the signal number, 0 while none arrived.
static TERMINATE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn on_terminate(sig: libc::c_int) {
    TERMINATE.store(sig, std::sync::atomic::Ordering::SeqCst);
}

/// Route SIGTERM and SIGHUP to [`on_terminate`]. No `SA_RESTART`, so a
/// blocking wait for input is interrupted rather than resumed.
fn install_terminate_handler() {
    // SAFETY: the handler only stores into an atomic, which is signal-safe.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_terminate as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        for sig in [libc::SIGTERM, libc::SIGHUP] {
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

/// Did the terminal input stream close? Crossterm's Unix event poller can loop
/// forever on EOF, so the main loop checks this independently of its reader.
fn terminal_input_hung_up(fd: RawFd) -> bool {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll only borrows this one initialized pollfd for this call.
    let ready = unsafe { libc::poll(&mut pollfd, 1, 0) };
    if ready < 0 {
        return last_errno() != libc::EINTR;
    }
    if ready == 0 {
        return false;
    }
    if pollfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return true;
    }
    if pollfd.revents & libc::POLLIN == 0 {
        return false;
    }

    // Some PTY implementations report EOF as readable with zero buffered
    // bytes, not as POLLHUP. FIONREAD distinguishes that from queued keys.
    let mut available: libc::c_int = 0;
    // SAFETY: ioctl writes one integer to this initialized, writable location.
    unsafe { libc::ioctl(fd, libc::FIONREAD, &mut available) == 0 && available == 0 }
}

/// Crossterm's poll/read path may spin forever when stdin reaches EOF. Keep
/// that path off the UI thread; the UI independently watches stdin for HUP and
/// can finish cleanup without waiting for this detached reader to return.
fn start_event_reader() -> Result<(Receiver<std::io::Result<TermEvent>>, Arc<AtomicBool>)> {
    let (tx, rx) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let thread_stopped = Arc::clone(&stopped);
    thread::Builder::new()
        .name("terminal-events".into())
        .spawn(move || {
            while !thread_stopped.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(false) => continue,
                    Ok(true) => {
                        let event = event::read();
                        let failed = event.is_err();
                        if tx.send(event).is_err() || failed {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        })?;
    Ok((rx, stopped))
}

pub fn run(
    sources: Vec<Source>,
    opts: &GlobalOpts,
    refresh: Duration,
    extras: Vec<ExtraCmd>,
    filter: String,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let mut targets = Vec::new();
    let mut helper_state: Option<Result<()>> = None;
    for (idx, src) in sources.into_iter().enumerate() {
        let helper_host = match &src.privileged {
            Privileged::Always(host) => Some(host.clone()),
            _ => None,
        };
        let settings = match &src.env {
            _ if helper_host.is_some() => Err(anyhow::anyhow!("configuration read by the helper")),
            Some(map) => Settings::resolve(
                &GlobalOpts {
                    debug: opts.debug,
                    ..Default::default()
                },
                &|k| map.get(k).cloned(),
            ),
            None => Settings::resolve(opts, &|k| std::env::var(k).ok()),
        };
        let log_tx = tx.clone();
        let logger = Logger::with_sink(
            opts.debug || settings.as_ref().is_ok_and(|s| s.debug),
            Arc::new(move |line| {
                let _ = log_tx.send(Msg::Log(idx, line));
            }),
        );
        let host = helper_host.clone().unwrap_or_else(|| {
            settings
                .as_ref()
                .map(|s| s.hostname.clone())
                .unwrap_or_default()
        });
        let spec_host = host.clone();
        let cmds = default_first(
            merge_extras(&extras, src.extras.clone()),
            src.default_view.as_deref(),
        );
        let mut target = Target {
            name: src.name.clone(),
            host,
            error: None,
            report: None,
            busy: None,
            last_unlock: None,
            logs: VecDeque::new(),
            unit: None,
            tx: None,
            extra: (0..cmds.len()).map(|_| ExtraOut::default()).collect(),
            cmds,
        };
        let direct = match helper_host {
            Some(_) => None,
            None => Some(settings.and_then(|mut s| {
                s.once = true;
                Ctx::new(s, logger.clone(), Mode::Status)
            })),
        };
        let spec = || helper_worker::Spec {
            idx,
            name: src.name.clone(),
            host: spec_host.clone(),
            refresh,
        };
        match direct {
            Some(Ok(ctx)) => {
                let (ctx_tx, ctx_rx) = mpsc::channel();
                let tx = tx.clone();
                let unit = format!("luks-ssh-unlock-{}.service", src.name);
                let msg_tx = tx.clone();
                let worker_unit = unit.clone();
                thread::spawn(move || worker(idx, ctx, worker_unit, ctx_rx, tx, refresh));
                let probe_tx = ctx_tx.clone();
                thread::spawn(move || follow_journal(idx, unit, probe_tx, msg_tx));
                target.tx = Some(ctx_tx);
            }
            Some(Err(e)) => {
                let mut msg = format!("{e:#}");
                let protected = msg.contains("not readable");
                if protected && src.privileged == Privileged::IfNeeded {
                    // The user's own process stays as it is; the protected
                    // part is handled by the helper, if there is one.
                    match helper_state.get_or_insert_with(|| helper_listing().map(|_| ())) {
                        Ok(()) => {
                            target.tx = Some(helper_worker::spawn(
                                spec(),
                                crate::helper::client::Client::system(),
                                logger,
                                tx.clone(),
                            ));
                        }
                        Err(h) => {
                            msg.push_str(&format!(
                                " (and the privileged helper is not usable: {h:#})"
                            ));
                        }
                    }
                } else if protected {
                    msg.push_str(" (the dashboard does not run as root)");
                }
                if target.tx.is_none() {
                    target.error = Some(msg);
                }
            }
            None => {
                target.tx = Some(helper_worker::spawn(
                    spec(),
                    crate::helper::client::Client::system(),
                    logger,
                    tx.clone(),
                ));
            }
        }
        targets.push(target);
    }

    let has_extras = targets.iter().any(|t| !t.cmds.is_empty());
    let mut app = App {
        targets,
        list: ListState::default().with_selected(Some(0)),
        confirm: None,
        refresh,
        quit: false,
        log_scroll: 0,
        portrait: false,
        focus: Focus::List,
        logs_full: false,
        extra_full: false,
        help: false,
        info: false,
        drag: None,
        hit: Hit::default(),
        switch_from: None,
        filter,
        filtering: false,
        has_extras,
        extra_idx: 0,
        extra_scroll: 0,
        extra_run: None,
        extra_gen: 0,
        console_size: (24, 80),
        restart_wait: None,
        force_reload: false,
        extra_pct: 80,
        vnc: VncState::default(),
        msg_tx: tx,
    };
    let mut terminal = ratatui::init();
    // ratatui's panic hook restores the screen but knows nothing of the mouse.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if terminal_input_hung_up(libc::STDIN_FILENO) {
            eprintln!("panic after PTY hangup: {info}");
        } else {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
            hook(info);
        }
    }));
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    install_terminate_handler();
    let (event_rx, event_reader_stopped) = match start_event_reader() {
        Ok(reader) => reader,
        Err(error) => {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
            ratatui::restore();
            return Err(error);
        }
    };
    let result = event_loop(&mut terminal, &mut app, &rx, &event_rx);
    let terminal_hung_up = matches!(&result, Ok(true));
    // Do not join: Crossterm's event source can be stuck reading EOF. The UI
    // exits promptly on the independent HUP check, and process exit reaps it.
    event_reader_stopped.store(true, Ordering::Relaxed);
    app.stop_extra(true);
    app.shutdown_vnc();
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    // The terminal is no longer writable after a PTY hangup; restoring it
    // would only emit a misleading I/O error. Cleanup of child sessions and
    // Kitty graphics above still runs.
    if terminal_hung_up || terminal_input_hung_up(libc::STDIN_FILENO) {
        // DefaultTerminal's drop tries to show the cursor, which fails once its
        // PTY master is gone. This TUI invocation is ending with the process.
        std::mem::forget(terminal);
    } else {
        ratatui::restore();
    }
    // Die of the signal after cleaning up, so the exit status says so.
    let sig = TERMINATE.load(std::sync::atomic::Ordering::SeqCst);
    if sig != 0 {
        // SAFETY: restoring the default action, then re-raising.
        unsafe {
            libc::signal(sig, libc::SIG_DFL);
            libc::raise(sig);
        }
    }
    result.map(|_| ())
}

fn draw_app(terminal: &mut DefaultTerminal, app: &mut App) -> Result<bool> {
    match terminal.draw(|f| app.draw(f)) {
        Ok(_) => Ok(true),
        Err(_) if terminal_input_hung_up(libc::STDIN_FILENO) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &Receiver<Msg>,
    event_rx: &Receiver<std::io::Result<TermEvent>>,
) -> Result<bool> {
    // The first frame tells a console how big it is before it starts.
    if !draw_app(terminal, app)? {
        return Ok(true);
    }
    while !app.quit {
        if TERMINATE.load(std::sync::atomic::Ordering::SeqCst) != 0 {
            break;
        }
        if terminal_input_hung_up(libc::STDIN_FILENO) {
            return Ok(true);
        }
        // Not all of a flood of console output at once: keep the keys responsive.
        for _ in 0..256 {
            let Ok(msg) = rx.try_recv() else { break };
            app.apply(msg);
        }
        app.pump_vnc();
        app.fetch_extra();
        if !draw_app(terminal, app)? {
            return Ok(true);
        }
        // The picture of a remote desktop goes on after the frame's text.
        app.draw_vnc_image();
        // The event reader owns all Crossterm calls. This thread can continue
        // UI and PTY cleanup if Crossterm spins internally after input EOF.
        let event = match event_rx.recv_timeout(Duration::from_millis(app.poll_ms())) {
            Ok(Ok(event)) => Some(event),
            Ok(Err(_)) if terminal_input_hung_up(libc::STDIN_FILENO) => return Ok(true),
            Ok(Err(error)) => return Err(error.into()),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) if terminal_input_hung_up(libc::STDIN_FILENO) => {
                return Ok(true);
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!(
                    "terminal event reader stopped unexpectedly"
                ));
            }
        };
        if let Some(event) = event {
            match event {
                TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    app.key(key.code, key.modifiers)
                }
                TermEvent::Mouse(m) => app.mouse(m),
                TermEvent::Resize(..) => {
                    app.vnc_resized();
                    if let Err(error) = terminal.clear() {
                        if terminal_input_hung_up(libc::STDIN_FILENO) {
                            return Ok(true);
                        }
                        return Err(error.into());
                    }
                }
                _ => {}
            }
        }
    }
    Ok(false)
}

impl App {
    /// Indexes of the targets matching the filter, in display order.
    fn visible(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        (0..self.targets.len())
            .filter(|&i| {
                let t = &self.targets[i];
                f.is_empty()
                    || t.name.to_lowercase().contains(&f)
                    || t.host.to_lowercase().contains(&f)
            })
            .collect()
    }

    /// Index (into `targets`) of the selected target; 0 when the filter
    /// matches nothing (callers check `visible()` first).
    fn selected(&self) -> usize {
        let v = self.visible();
        let pos = self.list.selected().unwrap_or(0);
        v.get(pos).or(v.last()).copied().unwrap_or(0)
    }

    fn set_filter(&mut self) {
        self.list.select(Some(0));
        self.log_scroll = 0;
        self.extra_scroll = 0;
    }

    /// Which of the target's extra commands is shown.
    fn extra_c(&self, t: usize) -> usize {
        self.extra_idx
            .min(self.targets[t].cmds.len().saturating_sub(1))
    }

    /// Stop the running extra command, if any: SIGTERM first, so its
    /// cleanup (such as deleting a console connection) runs, and SIGKILL
    /// after a grace period. With `wait`, return only once it has ended.
    fn stop_extra(&mut self, wait: bool) {
        if let Some(run) = self.extra_run.take() {
            unsafe { libc::kill(-run.pgid, libc::SIGTERM) };
            let (pgid, ended) = (run.pgid, run.ended);
            let end = Instant::now() + EXTRA_GRACE;
            if wait {
                while !ended.load(Ordering::SeqCst) && Instant::now() < end {
                    thread::sleep(Duration::from_millis(20));
                }
                if !ended.load(Ordering::SeqCst) {
                    unsafe { libc::kill(-pgid, libc::SIGKILL) };
                }
            } else {
                thread::spawn(move || {
                    while !ended.load(Ordering::SeqCst) && Instant::now() < end {
                        thread::sleep(Duration::from_millis(100));
                    }
                    if !ended.load(Ordering::SeqCst) {
                        unsafe { libc::kill(-pgid, libc::SIGKILL) };
                    }
                });
            }
            let slot = &mut self.targets[run.t].extra[run.c];
            slot.run_id = 0;
            slot.running = false;
            slot.finished_at = None;
        }
    }

    /// Keep the selected target's extra command running: stream it while the
    /// target is selected, and start it again every refresh interval once it
    /// has ended.
    fn fetch_extra(&mut self) {
        if let Some(pty) = self.extra_run.as_mut().and_then(|r| r.pty.as_mut()) {
            pty.flush();
        }
        let want = if !self.has_extras || self.visible().is_empty() {
            None
        } else {
            let t = self.selected();
            (!self.targets[t].cmds.is_empty()).then(|| (t, self.extra_c(t)))
        };
        if self
            .extra_run
            .as_ref()
            .is_some_and(|r| Some((r.t, r.c)) != want)
        {
            self.stop_extra(false);
        }
        if self
            .vnc
            .run
            .as_ref()
            .is_some_and(|r| Some((r.t, r.c)) != want)
        {
            self.stop_vnc();
        }
        if let Some(old) = &self.restart_wait {
            if !old.load(Ordering::SeqCst) {
                return;
            }
            self.restart_wait = None;
        }
        let Some((t, c)) = want else { return };
        let refresh = self.refresh;
        let target = &mut self.targets[t];
        if target.error.is_some() {
            return;
        }
        if target.cmds[c].vnc.is_some() {
            self.fetch_vnc(t, c);
            return;
        }
        let interactive = target.cmds[c].interactive;
        let slot = &mut target.extra[c];
        // A console that ended stays as it is (its screen shows how); the
        // output of other commands is fetched again every refresh interval.
        if slot.running
            || slot
                .finished_at
                .is_some_and(|a| interactive || a.elapsed() < refresh)
        {
            return;
        }
        self.extra_gen += 1;
        let run_id = self.extra_gen;
        slot.run_id = run_id;
        slot.running = true;
        slot.restart = true;
        slot.finished_at = None;
        slot.screen = None;
        slot.query_tail.clear();
        slot.loading = Some(("starting…".into(), Instant::now()));
        slot.read_only = false;
        slot.progress_tail.clear();
        let started = if interactive {
            let (rows, cols) = self.console_size;
            spawn_console(
                &target.cmds[c].command,
                (t, c, run_id),
                (&target.name, &target.host),
                (rows, cols),
                self.msg_tx.clone(),
                std::mem::take(&mut self.force_reload),
            )
            .map(|(pgid, ended, pty)| {
                slot.screen = Some(vt100::Parser::new(rows, cols, EXTRA_LINES));
                slot.restart = false;
                (pgid, ended, Some(pty))
            })
        } else {
            spawn_extra(
                &target.cmds[c].command,
                (t, c, run_id),
                (&target.name, &target.host),
                self.msg_tx.clone(),
            )
            .map(|(pgid, ended)| (pgid, ended, None))
        };
        match started {
            Ok((pgid, ended, pty)) => {
                self.extra_run = Some(ExtraRun {
                    t,
                    c,
                    pgid,
                    ended,
                    pty,
                })
            }
            Err(e) => {
                slot.lines = vec![Line::from(format!("cannot run command: {e}"))];
                slot.run_id = 0;
                slot.running = false;
                slot.restart = false;
                slot.finished_at = Some(Instant::now());
            }
        }
    }

    /// The selected target's console (`.0` target, `.1` command), if its
    /// shown command is interactive.
    fn console_cmd(&self) -> Option<(usize, usize)> {
        if !self.has_extras || self.logs_full || self.visible().is_empty() {
            return None;
        }
        let t = self.selected();
        let c = self.extra_c(t);
        self.targets[t].cmds.get(c)?.is_console().then_some((t, c))
    }

    /// Show the target's next extra command (such as the serial console
    /// after the graphical one).
    fn cycle_extra(&mut self, t: usize) {
        let len = self.targets[t].cmds.len();
        if len > 0 {
            self.extra_idx = (self.extra_c(t) + 1) % len;
            self.extra_scroll = 0;
        }
    }

    /// A console pane has the focus, with no dialog in the way. Its reserved
    /// keys (restart, release) work now, even when the command has ended.
    fn console_focus(&self) -> Option<(usize, usize)> {
        if self.focus != Focus::Extra
            || self.confirm.is_some()
            || self.help
            || self.info
            || self.filtering
        {
            return None;
        }
        // A read-only console (a log) never takes the keyboard.
        self.console_cmd()
            .filter(|&(t, c)| !self.targets[t].extra.get(c).is_some_and(|s| s.read_only))
    }

    /// The keyboard goes to the console: its pane has the focus and the
    /// command has not ended (while it starts, keys are dropped rather than
    /// acted on by the dashboard).
    fn console_captured(&self) -> bool {
        self.console_focus().is_some_and(|(t, c)| {
            let target = &self.targets[t];
            let slot = &target.extra[c];
            // A view-only desktop leaves the keyboard to the dashboard.
            target.error.is_none()
                && (slot.running || slot.finished_at.is_none())
                && !(target.cmds[c].vnc.is_some() && self.vnc_view_only(t, c))
        })
    }

    /// An interactive command runs: poll often, its output is echo.
    fn console_live(&self) -> bool {
        self.extra_run.as_ref().is_some_and(|r| r.pty.is_some())
    }

    /// Start the console again, once the old one is gone: it may have to
    /// clean up first (such as deleting a console connection).
    fn restart_console(&mut self) {
        let old = self.extra_run.as_ref().map(|r| r.ended.clone());
        self.stop_extra(false);
        self.restart_wait = old;
        self.force_reload = true;
        if let Some((t, c)) = self.console_cmd() {
            let slot = &mut self.targets[t].extra[c];
            slot.finished_at = None;
            slot.running = false;
            self.extra_scroll = 0;
        }
    }

    /// A key while a console pane has the focus. `false`: not for the
    /// console, the dashboard handles it as usual.
    fn console_key(&mut self, (t, c): (usize, usize), code: KeyCode, mods: KeyModifiers) -> bool {
        if is_console_escape(code, mods) {
            // Back to the dashboard: the target list has the focus.
            self.vnc_release();
            if self.extra_full {
                self.vnc_resized();
            }
            self.focus = Focus::List;
            self.extra_full = false;
            return true;
        }
        if is_console_restart(code, mods) {
            if self.targets[t].cmds[c].vnc.is_some() {
                self.restart_vnc();
            } else {
                self.restart_console();
            }
            return true;
        }
        let vnc = self.targets[t].cmds[c].vnc.is_some();
        // `C` is the dashboard's fullscreen binding, including while a VNC
        // pane owns the keyboard; do not send it to the remote desktop.
        if vnc && code == KeyCode::Char('C') && self.has_extras {
            return false;
        }
        // F6 shows the next view (graphical or serial console) of the target.
        if code == KeyCode::F(6) && self.targets[t].cmds.len() > 1 {
            self.cycle_extra(t);
            return true;
        }
        if vnc {
            // Ctrl+T: the keyboard goes to the desktop, or stays here.
            if code == KeyCode::Char('t') && mods.contains(KeyModifiers::CONTROL) {
                let view_only = self.vnc_view_only(t, c);
                self.targets[t].extra[c].vnc_view_only = Some(!view_only);
                self.vnc_release();
                return true;
            }
            if mods.contains(KeyModifiers::SHIFT)
                && matches!(
                    code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
                )
            {
                return false;
            }
            if !self.console_captured() {
                return false;
            }
            self.vnc_send_key((t, c), code, mods);
            return true;
        }
        let shift = mods.contains(KeyModifiers::SHIFT);
        match code {
            // Shift+arrows keep moving the focus, as everywhere.
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down if shift => {
                return false;
            }
            KeyCode::PageUp | KeyCode::PageDown if shift => {
                let page = usize::from(self.console_size.0.saturating_sub(1)).max(1);
                self.scroll_extra(code == KeyCode::PageUp, page);
                return true;
            }
            _ => {}
        }
        if !self.console_captured() {
            return false;
        }
        let app_cursor = self.targets[t].extra[c]
            .screen
            .as_ref()
            .is_some_and(|p| p.screen().application_cursor());
        let bytes = encode_key(code, mods, app_cursor);
        if !bytes.is_empty() {
            // Typing goes back to the live screen.
            self.extra_scroll = 0;
            if let Some(pty) = self
                .extra_run
                .as_mut()
                .filter(|r| (r.t, r.c) == (t, c))
                .and_then(|r| r.pty.as_mut())
            {
                pty.write(&bytes);
            }
        }
        true
    }

    fn scroll_extra(&mut self, up: bool, by: usize) {
        self.extra_scroll = if up {
            self.extra_scroll.saturating_add(by)
        } else {
            self.extra_scroll.saturating_sub(by)
        };
    }

    fn send(&self, idx: usize, cmd: Cmd) {
        if let Some(tx) = self.targets.get(idx).and_then(|t| t.tx.as_ref()) {
            let _ = tx.send(cmd);
        }
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Report(i, r) => self.targets[i].report = Some(*r),
            Msg::Log(i, line) => self.push_entry(i, Entry::own(line)),
            Msg::Service(i, entry) => self.push_entry(i, entry),
            Msg::History(i, mut entries) => {
                // Older than anything logged this session: merge by time.
                let logs = &mut self.targets[i].logs;
                entries.extend(logs.drain(..));
                entries.sort_by_key(|e| e.at);
                let skip = entries.len().saturating_sub(LOG_LINES);
                logs.extend(entries.into_iter().skip(skip));
            }
            Msg::Busy(i, b) => self.targets[i].busy = b,
            Msg::Done(i, state) => self.targets[i].last_unlock = Some((Instant::now(), state)),
            Msg::Unit(i, unit) => self.targets[i].unit = unit,
            Msg::ExtraLine(i, c, run_id, line) => {
                let shown = self.selected() == i && self.extra_c(i) == c;
                let slot = &mut self.targets[i].extra[c];
                if slot.run_id != run_id {
                    return;
                }
                slot.loading = None;
                if std::mem::take(&mut slot.restart) {
                    slot.lines.clear();
                }
                slot.lines.push(line);
                if slot.lines.len() > EXTRA_LINES + 200 {
                    let extra = slot.lines.len() - EXTRA_LINES;
                    slot.lines.drain(..extra);
                }
                if self.extra_scroll > 0 && shown {
                    self.extra_scroll += 1; // keep the scrolled view in place
                }
            }
            Msg::ConsoleData(i, c, run_id, data) => {
                let shown = self.selected() == i && self.extra_c(i) == c;
                let slot = &mut self.targets[i].extra[c];
                if slot.run_id != run_id {
                    return;
                }
                let scan = crate::progress::scan(&mut slot.progress_tail, &data);
                if let Some(step) = scan.status
                    && let Some((current, _)) = slot.loading.as_mut()
                {
                    *current = step;
                }
                slot.read_only |= scan.read_only;
                // The console is there once it says so, or prints something.
                if scan.ready || scan.data.iter().any(|b| b.is_ascii_graphic()) {
                    slot.loading = None;
                }
                let data = scan.data;
                let Some(parser) = slot.screen.as_mut() else {
                    return;
                };
                parser.process(&data);
                let mut seen = std::mem::take(&mut slot.query_tail);
                seen.extend_from_slice(&data);
                let replies = terminal_replies(&seen, parser.screen().cursor_position());
                slot.query_tail = seen[partial_query(&seen)..].to_vec();
                if self.extra_scroll > 0 && shown && !parser.screen().alternate_screen() {
                    // Keep the scrolled view in place.
                    self.extra_scroll += data.iter().filter(|b| **b == b'\n').count();
                }
                if !replies.is_empty()
                    && let Some(pty) = self
                        .extra_run
                        .as_mut()
                        .filter(|r| (r.t, r.c) == (i, c))
                        .and_then(|r| r.pty.as_mut())
                {
                    pty.write(&replies);
                }
            }
            Msg::ExtraDone(i, c, run_id, code) => {
                let slot = &mut self.targets[i].extra[c];
                if slot.run_id != run_id {
                    return;
                }
                slot.loading = None;
                if std::mem::take(&mut slot.restart) {
                    slot.lines.clear();
                }
                let note = match code {
                    Some(0) => None,
                    Some(n) => Some(format!("[exit {n}]")),
                    None => Some("[killed]".to_string()),
                };
                if let Some(parser) = slot.screen.as_mut() {
                    // On the console's own screen, with the way out.
                    let note = note.unwrap_or_else(|| "[exited]".into());
                    parser.process(format!("\r\n{note} Ctrl+R or F5 restarts\r\n").as_bytes());
                } else if let Some(note) = note {
                    slot.lines.push(Line::from(note));
                }
                slot.running = false;
                slot.finished_at = Some(Instant::now());
                if self
                    .extra_run
                    .as_ref()
                    .is_some_and(|r| (r.t, r.c) == (i, c))
                {
                    self.extra_run = None;
                }
            }
        }
    }

    fn push_entry(&mut self, i: usize, entry: Entry) {
        let logs = &mut self.targets[i].logs;
        logs.push_back(entry);
        while logs.len() > LOG_LINES {
            logs.pop_front();
        }
        if self.log_scroll > 0 && i == self.selected() {
            self.log_scroll += 1; // keep the scrolled view in place
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if std::env::var_os("LSU_VNC_TRACE").is_some()
            && (code == KeyCode::Char('C') || is_console_escape(code, mods))
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            eprintln!(
                "[LSU_VNC_TRACE] {}.{:03} key code={code:?} mods={mods:?} focus_extra={} fullscreen={} console_focus={:?} captured={}",
                now.as_secs(),
                now.subsec_millis(),
                self.focus == Focus::Extra,
                self.extra_full,
                self.console_focus(),
                self.console_captured()
            );
        }
        if let Some((idx, force)) = self.confirm.take() {
            if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.send(idx, Cmd::Unlock { force });
            }
            return;
        }
        if self.help || self.info {
            self.help = false;
            self.info = false;
            return;
        }
        if let Some(console) = self.console_focus()
            && self.console_key(console, code, mods)
        {
            return;
        }
        if self.filtering {
            let n = self.visible().len();
            let pos = self.list.selected().unwrap_or(0);
            match code {
                KeyCode::Esc => self.close_switcher(None),
                KeyCode::Enter => {
                    let chosen = self.visible().get(pos).copied();
                    self.close_switcher(chosen);
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.set_filter();
                }
                KeyCode::Down if n > 0 => self.select((pos + 1) % n),
                KeyCode::Up if n > 0 => self.select((pos + n - 1) % n),
                KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                    self.filter.push(c);
                    self.set_filter();
                }
                _ => {}
            }
            return;
        }
        let n = self.visible().len();
        let pos = self.list.selected().unwrap_or(0).min(n.saturating_sub(1));
        let sel = self.selected();
        let shift = mods.contains(KeyModifiers::SHIFT);
        let dir = match code {
            KeyCode::Left => Some(Dir::Left),
            KeyCode::Right => Some(Dir::Right),
            KeyCode::Up => Some(Dir::Up),
            KeyCode::Down => Some(Dir::Down),
            _ => None,
        };
        match code {
            // Shift+arrows always move the focus; plain arrows act inside
            // the focused pane and move the focus where the pane has no use
            // for them. In a fullscreen console, first restore the dashboard
            // layout so the focus transition is visible and can take effect.
            _ if shift && dir.is_some() => {
                if self.extra_full {
                    self.extra_full = false;
                }
                self.move_focus(dir.expect("checked"));
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('i') if n > 0 => self.info = true,
            KeyCode::Char('l') if n > 0 => {
                self.logs_full = !self.logs_full;
                self.extra_full = false;
                self.focus = Focus::Activity;
            }
            KeyCode::Char('C') if n > 0 && self.has_extras => {
                self.vnc_resized();
                self.extra_full = !self.extra_full;
                self.logs_full = false;
                self.focus = Focus::Extra;
            }
            KeyCode::Esc if self.logs_full || self.extra_full => {
                self.logs_full = false;
                self.extra_full = false;
            }
            KeyCode::Char('/') if !self.targets.is_empty() => self.open_switcher(),
            KeyCode::Char('p')
                if mods.contains(KeyModifiers::CONTROL) && !self.targets.is_empty() =>
            {
                self.open_switcher()
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.set_filter();
            }
            KeyCode::Tab => self.cycle_focus(true),
            KeyCode::BackTab => self.cycle_focus(false),
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('c') | KeyCode::F(6) if self.has_extras && n > 0 => self.cycle_extra(sel),
            KeyCode::Down | KeyCode::Char('j') if n > 0 && self.focus == Focus::List => {
                self.select((pos + 1) % n)
            }
            KeyCode::Up | KeyCode::Char('k') if n > 0 && self.focus == Focus::List => {
                self.select((pos + n - 1) % n)
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Extra => {
                self.scroll_extra(false, 1)
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Extra => {
                self.scroll_extra(true, 1)
            }
            KeyCode::Home if self.focus == Focus::Extra => self.scroll_extra(true, usize::MAX),
            KeyCode::End if self.focus == Focus::Extra => self.extra_scroll = 0,
            KeyCode::Down | KeyCode::Char('j') if self.focus == Focus::Activity => {
                self.scroll_logs(false, 1)
            }
            KeyCode::Up | KeyCode::Char('k') if self.focus == Focus::Activity => {
                self.scroll_logs(true, 1)
            }
            KeyCode::Home if self.focus == Focus::Activity => self.scroll_logs(true, usize::MAX),
            KeyCode::End if self.focus == Focus::Activity => self.log_scroll = 0,
            _ if dir.is_some() => self.move_focus(dir.expect("checked")),
            KeyCode::PageUp if self.focus == Focus::Extra => {
                self.scroll_extra(true, self.hit.extra.height as usize)
            }
            KeyCode::PageDown if self.focus == Focus::Extra => {
                self.scroll_extra(false, self.hit.extra.height as usize)
            }
            KeyCode::PageUp => self.scroll_logs(true, self.hit.logs.height as usize),
            KeyCode::PageDown => self.scroll_logs(false, self.hit.logs.height as usize),
            KeyCode::Char('r') if n > 0 => {
                self.send(sel, Cmd::Probe);
                // Run the extra command again from the start.
                self.stop_extra(false);
                self.restart_vnc();
                let c = self.extra_c(sel);
                if let Some(slot) = self.targets[sel].extra.get_mut(c) {
                    slot.finished_at = None;
                }
            }
            KeyCode::Char('R') => (0..n).for_each(|i| self.send(i, Cmd::Probe)),
            KeyCode::Char('u') if n > 0 && self.targets[sel].tx.is_some() => {
                self.confirm = Some((sel, false));
            }
            KeyCode::Char('F') if n > 0 && self.targets[sel].tx.is_some() => {
                self.confirm = Some((sel, true))
            }
            KeyCode::Char('S') if n > 0 && self.targets[sel].tx.is_some() => {
                self.toggle_service(sel)
            }
            _ => {}
        }
    }

    /// Stop the target's service when it runs, else start it.
    fn toggle_service(&mut self, i: usize) {
        let note = match &self.targets[i].unit {
            Some(u) if u.load == "not-found" => {
                Some("the service is not installed on this host: nothing to start or stop")
            }
            Some(u) => {
                let running = matches!(u.active.as_str(), "active" | "activating");
                self.send(i, Cmd::Service(!running));
                None
            }
            None => Some("the service state is not known yet"),
        };
        if let Some(note) = note {
            self.push_entry(
                i,
                Entry {
                    at: Local::now(),
                    level: Level::Warning,
                    service: false,
                    message: note.into(),
                },
            );
        }
    }

    fn border(&self, pane: Focus) -> Style {
        if self.focus == pane {
            Style::new().cyan().bold()
        } else {
            Style::new()
        }
    }

    /// Focus the neighbouring pane in a direction (the layout decides which
    /// that is).
    fn move_focus(&mut self, dir: Dir) {
        if self.logs_full || self.extra_full {
            return;
        }
        // Top to bottom: the host line, the console, the activity.
        let extra = self.has_extras;
        self.focus = match (self.focus, dir) {
            (Focus::List, Dir::Down) if extra => Focus::Extra,
            (Focus::List, Dir::Down) => Focus::Activity,
            (Focus::Extra, Dir::Up) => Focus::List,
            (Focus::Extra, Dir::Down) => Focus::Activity,
            (Focus::Activity, Dir::Up) if extra && self.portrait => Focus::Extra,
            (Focus::Activity, Dir::Up) => Focus::List,
            // Side by side: the console on the left, the activity on the right.
            (Focus::Extra, Dir::Right) if !self.portrait => Focus::Activity,
            (Focus::Activity, Dir::Left) if extra && !self.portrait => Focus::Extra,
            (f, _) => f,
        };
    }

    fn cycle_focus(&mut self, forward: bool) {
        if self.logs_full || self.extra_full {
            return;
        }
        let mut order = vec![Focus::List];
        if self.has_extras {
            order.push(Focus::Extra);
        }
        order.push(Focus::Activity);
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let len = order.len();
        self.focus = order[if forward {
            (at + 1) % len
        } else {
            (at + len - 1) % len
        }];
    }

    /// Move the dragged border to the pointer.
    fn resize(&mut self, x: u16, y: u16) {
        let body = self.hit.body;
        if body.width == 0 || body.height == 0 {
            return;
        }
        if self.drag == Some(Edge::Extra) {
            // The console comes first (on top, or on the left): its size is
            // where the border is.
            let area = self.hit.split;
            let (at, start, len) = if self.portrait {
                (y, area.y, area.height)
            } else {
                (x, area.x, area.width)
            };
            if len > 0 {
                let size = u32::from(at.saturating_sub(start) + 1);
                self.extra_pct = (size * 100 / u32::from(len)).clamp(15, 90) as u16;
            }
        }
    }

    fn select(&mut self, i: usize) {
        if Some(i) != self.list.selected() {
            self.log_scroll = 0;
            self.extra_scroll = 0;
        }
        self.list.select(Some(i));
    }

    fn scroll_logs(&mut self, up: bool, by: usize) {
        // Clamped to the available history when drawing.
        self.log_scroll = if up {
            self.log_scroll.saturating_add(by)
        } else {
            self.log_scroll.saturating_sub(by)
        };
    }

    fn mouse(&mut self, m: MouseEvent) {
        let (x, y) = (m.column, m.row);
        if self.help || self.info {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.help = false;
                self.info = false;
            }
            return;
        }
        if self.confirm.is_some() {
            if m.kind == MouseEventKind::Down(MouseButton::Left) {
                let (idx, force) = self.confirm.take().expect("checked above");
                if contains(self.hit.yes, x, y) {
                    self.send(idx, Cmd::Unlock { force });
                } else if contains(self.hit.dialog, x, y) && !contains(self.hit.cancel, x, y) {
                    // Clicks on the dialog text neither confirm nor cancel.
                    self.confirm = Some((idx, force));
                }
            }
            return;
        }
        // Over the remote desktop (which has the keyboard), the mouse is its.
        if self.vnc_mouse(m) {
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = contains(self.hit.edge_extra, x, y).then_some(Edge::Extra);
                if self.drag.is_some() {
                    return;
                }
                if self.filtering {
                    if !contains(self.hit.switcher, x, y) {
                        self.close_switcher(None);
                    }
                } else if contains(self.hit.header, x, y) && !self.targets.is_empty() {
                    // The host's name is the way to the switcher.
                    self.focus = Focus::List;
                    self.open_switcher();
                    return;
                } else if contains(self.hit.logs, x, y) {
                    self.focus = Focus::Activity;
                } else if contains(self.hit.extra, x, y) {
                    self.focus = Focus::Extra;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.drag.is_some() => {
                self.resize(x, y);
                return;
            }
            MouseEventKind::Up(MouseButton::Left) => self.drag = None,
            _ => {}
        }
        let vis = self.visible();
        let n = vis.len();
        if n == 0 {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if contains(self.hit.logs, x, y) {
                    self.scroll_logs(up, WHEEL_LINES);
                } else if contains(self.hit.extra, x, y) {
                    self.scroll_extra(up, WHEEL_LINES);
                } else if contains(self.hit.list, x, y) {
                    let pos = self.list.selected().unwrap_or(0).min(n - 1);
                    let next = if up {
                        pos.saturating_sub(1)
                    } else {
                        (pos + 1).min(n - 1)
                    };
                    self.select(next);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let list = self.hit.list;
                if self.filtering && contains(list, x, y) {
                    // One row per host in the switcher; a click switches.
                    let idx = self.list.offset() + usize::from(y - list.y);
                    if idx < n {
                        self.close_switcher(Some(vis[idx]));
                    }
                }
            }
            _ => {}
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(f.area());
        let area = f.area();
        self.hit.extra = Rect::default();
        self.hit.edge_extra = Rect::default();
        // The remote desktop's image is only drawn where a frame puts it.
        self.vnc.area = None;
        self.vnc.placeholders = false;
        // Cells are about twice as tall as wide: below this the side-by-side
        // layout is cramped, so stack the panes instead.
        self.portrait = area.width < 80 || u32::from(area.width) * 2 < u32::from(area.height) * 3;
        // No permanent target list: the console on top, the activity below,
        // and the host switcher (Ctrl+P, /) when it is asked for.
        // A target without a console gives the activity the whole body.
        let own_console = !self.visible().is_empty()
            && !self.targets.is_empty()
            && !self.targets[self.selected()].cmds.is_empty();
        // Wide: the console beside the activity; tall (a phone, a tall tmux
        // pane): the activity below the console.
        let (logs, extra) = if self.has_extras && own_console && !self.logs_full {
            let parts = [
                Constraint::Percentage(self.extra_pct),
                Constraint::Percentage(100 - self.extra_pct),
            ];
            self.hit.split = body;
            if self.portrait {
                let [extra, logs] = Layout::vertical(parts).areas(body);
                self.hit.edge_extra = Rect::new(logs.x, logs.y.saturating_sub(1), logs.width, 2);
                (logs, Some(extra))
            } else {
                let [extra, logs] = Layout::horizontal(parts).areas(body);
                self.hit.edge_extra = Rect::new(logs.x.saturating_sub(1), logs.y, 2, logs.height);
                (logs, Some(extra))
            }
        } else {
            (body, None)
        };
        self.hit.body = body;

        self.hit.header = header;
        f.render_widget(self.header_line(), header);

        if self.targets.is_empty() {
            f.render_widget(
                Paragraph::new(format!(
                    "No targets. Pass environment files, or configure services.luks-ssh-unlock instances ({DEFAULT_CONFIG_DIR}/*.env)."
                ))
                .wrap(Wrap { trim: true })
                .block(pane()),
                body,
            );
        } else {
            let sel = self.selected();
            let any = !self.visible().is_empty();
            self.hit.list = Rect::default();
            if self.logs_full && any {
                self.draw_logs(f, body, sel);
            } else if self.extra_full && any && self.has_extras {
                self.hit.logs = Rect::default();
                self.draw_extra(f, body, sel);
            } else {
                if any {
                    self.draw_logs(f, logs, sel);
                    if let Some(extra) = extra {
                        self.draw_extra(f, extra, sel);
                    }
                } else {
                    self.hit.logs = Rect::default();
                    f.render_widget(
                        Paragraph::new(format!("No target matches “{}”.", self.filter))
                            .block(pane()),
                        logs,
                    );
                }
            }
        }

        let vnc_focus = self
            .console_focus()
            .filter(|&(t, c)| self.targets[t].cmds[c].vnc.is_some());
        let hints = if let Some((t, c)) = vnc_focus
            && self.targets[t].extra[c].running
        {
            let view_only = self.vnc_view_only(t, c);
            let multiple_views = self.targets[t].cmds.len() > 1;
            let choices = match (view_only, multiple_views) {
                (true, true) => [
                    " the desktop is view-only · Ctrl+T give it the keyboard · Ctrl+] back · F6 next view · Ctrl+R/F5 restart",
                    " view-only · Ctrl+T keyboard · Ctrl+] back · F6 next",
                    " Ctrl+] back · Ctrl+T mode",
                    " Ctrl+] back",
                ],
                (false, true) => [
                    " the desktop has the keyboard and mouse · Ctrl+] back · Ctrl+T view-only · F6 next view · Ctrl+R/F5 restart",
                    " desktop keyboard · Ctrl+] back · Ctrl+T view-only · F6 next",
                    " Ctrl+] back · Ctrl+T mode",
                    " Ctrl+] back",
                ],
                (true, false) => [
                    " the desktop is view-only · Ctrl+T give it the keyboard · Ctrl+] back · Ctrl+R/F5 restart",
                    " view-only · Ctrl+T keyboard · Ctrl+] back",
                    " Ctrl+] back · Ctrl+T mode",
                    " Ctrl+] back",
                ],
                (false, false) => [
                    " the desktop has the keyboard and mouse · Ctrl+] back · Ctrl+T view-only · Ctrl+R/F5 restart",
                    " desktop keyboard · Ctrl+] back · Ctrl+T view-only",
                    " Ctrl+] back · Ctrl+T mode",
                    " Ctrl+] back",
                ],
            };
            Line::from(pick(footer.width, &choices).yellow())
        } else if self.console_captured() {
            Line::from(
                pick(
                    footer.width,
                    &[
                        " the console has the keyboard · Ctrl+] back to the dashboard · Ctrl+R/F5 restart · ⇧+arrows move focus · wheel/⇧PgUp scroll",
                        " console keyboard · Ctrl+] release · Ctrl+R/F5 restart · ⇧+arrows focus",
                        " Ctrl+] release · Ctrl+R/F5 restart",
                        " Ctrl+] release",
                    ],
                )
                .yellow(),
            )
        } else if self.extra_full && self.confirm.is_none() {
            Line::from(
                pick(
                    footer.width,
                    &[
                        " ↑↓/PgUp/PgDn/wheel scroll · Home/End oldest/newest · c next command · C/Esc back · q quit",
                        " ↑↓ scroll · c next · C/Esc back · q quit",
                        " C back · q quit",
                    ],
                )
                .dim(),
            )
        } else if self.logs_full && self.confirm.is_none() {
            Line::from(
                pick(
                    footer.width,
                    &[
                        " ↑↓/PgUp/PgDn/wheel scroll · Home/End oldest/newest · l/Esc back · q quit",
                        " ↑↓ scroll · l/Esc back · q quit",
                        " l back · q quit",
                    ],
                )
                .dim(),
            )
        } else if self.filtering {
            Line::from(
                pick(
                    footer.width,
                    &[
                        " switch host: type to match name or host · ↑↓ select · ⏎ switch · Esc cancel",
                        " type to filter · ↑↓ · ⏎ switch · Esc cancel",
                        " ⏎ switch · Esc cancel",
                    ],
                )
                .yellow(),
            )
        } else if self.confirm.is_some() {
            Line::from(
                pick(
                    footer.width,
                    &[
                        " y/click Yes confirms · any other key or click cancels",
                        " y = yes · anything else cancels",
                        " y = yes · else cancel",
                    ],
                )
                .yellow(),
            )
        } else {
            Line::from(
                pick(
                    footer.width,
                    &[
                        " Ctrl+P / switch host · ↑↓ next/previous host · ⇧↑↓/Tab focus pane · drag the border to resize · i details · r/R refresh · u unlock · F force · S service · l full-screen activity · C full-screen console · c next view · ? help · q quit",
                        " Ctrl+P switch host · ↑↓ host · ⇧↑↓/Tab focus · i info · r/R refresh · u unlock · F force · S service · l log · C console · ? help · q quit",
                        " Ctrl+P switch · ↑↓ host · r refresh · u unlock · q quit",
                        " ^P · ↑↓ · r · u · q",
                    ],
                )
                .dim(),
            )
        };
        f.render_widget(hints, footer);

        if self.filtering {
            self.draw_switcher(f);
        }
        if let Some((idx, force)) = self.confirm {
            self.draw_confirm(f, idx, force);
        }
        if self.info && !self.visible().is_empty() {
            self.draw_info(f, self.selected());
        }
        if self.help {
            self.draw_help(f);
        }
    }

    fn draw_help(&self, f: &mut Frame) {
        let keys: &[(&str, &str)] = &[
            (
                "↑ ↓  j k",
                "select a target (scroll in the focused log or console)",
            ),
            ("⇧+arrows  Tab", "move the focus to another pane"),
            (
                "Ctrl+P  /",
                "switch host: type to filter by name or host, ⏎ switches, Esc cancels",
            ),
            ("r  R", "check the selected target / all targets now"),
            ("i", "details of the selected target (dialog)"),
            ("u", "unlock the selected target (asks first)"),
            ("F", "force unlock (skips integrity and host-key checks)"),
            ("S", "stop the target's service, or start it when stopped"),
            ("l", "activity full screen (l or Esc goes back)"),
            ("C", "console (extra command) full screen"),
            ("c", "next console command"),
            (
                "Ctrl+]",
                "interactive console: give the keyboard back to the dashboard",
            ),
            ("Ctrl+R  F5", "interactive console or desktop: restart it"),
            (
                "Ctrl+T",
                "desktop (VNC): send the keyboard and mouse to it, or keep them here (view-only)",
            ),
            (
                "F6",
                "show the next view of the target (graphical or serial console)",
            ),
            ("PgUp PgDn Home End", "scroll the focused log or console"),
            (
                "mouse",
                "click select · double-click unlock · wheel · drag borders",
            ),
            ("?", "this help"),
            ("q  Esc", "quit"),
        ];
        let mut lines: Vec<Line> = keys
            .iter()
            .map(|(k, d)| {
                Line::from(vec![
                    Span::styled(format!("{k:<20}"), Style::new().cyan().bold()),
                    Span::raw(*d),
                ])
            })
            .collect();
        lines.push(Line::default());
        lines.push(Line::from("any key closes this help".dim()));
        let area = centered(f.area(), 84, lines.len() as u16 + 2);
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                Block::bordered()
                    .title(" Keys ")
                    .border_style(Style::new().cyan()),
            ),
            area,
        );
    }

    /// The line on top: the selected host and its state, the fleet at a
    /// glance, and the way to the switcher.
    fn header_line(&self) -> Line<'static> {
        let mut spans = Vec::new();
        if self.visible().is_empty() || self.targets.is_empty() {
            spans.push(" luks-ssh-unlock ".bold().reversed());
        } else {
            let t = &self.targets[self.selected()];
            let (icon, label, color) = summary(t);
            spans.push(Span::styled(format!(" {icon} "), Style::new().fg(color)));
            let name = Span::styled(format!("{} ▾", t.name), Style::new().bold());
            spans.push(if self.focus == Focus::List {
                name.cyan()
            } else {
                name
            });
            spans.push(Span::styled(format!("  {label}"), Style::new().fg(color)));
            if let Some(b) = t.busy {
                spans.push(Span::styled(format!(" · {b}…"), Style::new().fg(color)));
            }
        }
        // The fleet: how many hosts are in each state.
        let mut counts: Vec<(&'static str, Color, usize)> = Vec::new();
        for t in &self.targets {
            let (icon, _, color) = summary(t);
            match counts.iter_mut().find(|(i, _, _)| *i == icon) {
                Some(entry) => entry.2 += 1,
                None => counts.push((icon, color, 1)),
            }
        }
        spans.push(Span::raw("   "));
        for (icon, color, n) in counts {
            spans.push(Span::styled(
                format!("{icon} {n}  "),
                Style::new().fg(color),
            ));
        }
        let inactive = self
            .targets
            .iter()
            .filter(|t| t.unit.as_ref().is_some_and(|u| u.warning().is_some()))
            .count();
        if inactive > 0 {
            spans.push(
                format!("⚠ {inactive} service(s) not running  ")
                    .yellow()
                    .bold(),
            );
        }
        spans.push(
            format!(
                "Ctrl+P switch · {}",
                chrono::Local::now().format("%H:%M:%S")
            )
            .dim(),
        );
        Line::from(spans)
    }

    /// Open the host switcher on the selected host.
    fn open_switcher(&mut self) {
        let current = self.selected();
        self.switch_from = Some(current);
        self.filter.clear();
        self.filtering = true;
        self.list.select(Some(current));
    }

    /// Close the switcher on CHOSEN (a target index), or back on the host it
    /// was opened on.
    fn close_switcher(&mut self, chosen: Option<usize>) {
        let back = self.switch_from.take();
        let target = chosen.or(back).unwrap_or(0);
        self.filtering = false;
        self.filter.clear();
        // Without a filter every host is visible, at its own index.
        self.select(target.min(self.targets.len().saturating_sub(1)));
    }

    /// The host switcher: the hosts that match what is typed, one per line.
    fn draw_switcher(&mut self, f: &mut Frame) {
        let vis = self.visible();
        let items: Vec<ListItem> = vis
            .iter()
            .map(|&i| &self.targets[i])
            .map(|t| {
                let (icon, label, color) = summary(t);
                let mut spans = vec![
                    Span::styled(format!("{icon} "), Style::new().fg(color)),
                    Span::styled(format!("{:<12}", t.name), Style::new().bold()),
                    Span::styled(format!(" {label}"), Style::new().fg(color)),
                ];
                if t.unit.as_ref().is_some_and(|u| u.warning().is_some()) {
                    spans.push(Span::styled(" ⚠ service", Style::new().yellow().bold()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let screen = f.area();
        let height = (items.len().max(1) as u16 + 2).min(screen.height.saturating_sub(2).max(3));
        let area = centered(screen, 60, height);
        let empty = items.is_empty();
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(format!(" switch host · {}▏ ", self.filter))
                    .border_style(Style::new().cyan()),
            )
            .highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌");
        self.hit.switcher = area;
        self.hit.list = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        f.render_widget(Clear, area);
        f.render_stateful_widget(list, area, &mut self.list);
        if empty {
            f.render_widget(Line::from(" no host matches".dim()), self.hit.list);
        }
    }

    /// The details of a target, for the dialog.
    fn details_lines(&self, idx: usize) -> Vec<Line<'static>> {
        let t = &self.targets[idx];
        let mut lines: Vec<Line<'static>> = Vec::new();
        let key_width = if self.portrait { 16 } else { 18 };
        let row = |k: &str, v: String, style: Style| {
            Line::from(vec![
                Span::styled(format!("{k:<key_width$}"), Style::new().dim()),
                Span::styled(v, style),
            ])
        };
        if let Some(w) = t.unit.as_ref().and_then(UnitInfo::warning) {
            lines.push(Line::from(Span::styled(
                format!(
                    "⚠ {w}: this target will not be unlocked automatically (systemctl start luks-ssh-unlock-{}.service)",
                    t.name
                ),
                Style::new().yellow().bold(),
            )));
        }
        if let Some(err) = &t.error {
            lines.push(Line::from(Span::styled(
                format!("Configuration error: {err}"),
                Style::new().red(),
            )));
        } else if let Some(r) = &t.report {
            let (icon, label, color) = summary(t);
            let mut head = vec![Span::styled(
                format!("{icon} {label}"),
                Style::new().fg(color).bold(),
            )];
            // The healthcheck is what tells a booted host from one still
            // waiting, so its result belongs next to the state.
            if r.healthcheck.is_some() {
                let ok = matches!(r.state, TargetState::Unlocked(_));
                head.push(Span::raw("   "));
                head.push(Span::styled(
                    "●",
                    Style::new().fg(if ok { Color::Green } else { Color::Red }),
                ));
                head.push(Span::raw(" healthcheck"));
            }
            lines.push(Line::from(head));
            let detail = match &r.state {
                TargetState::Unlocked(d) | TargetState::Initrd(d) | TargetState::SshFailed(d) => {
                    d.clone()
                }
                TargetState::NotInitrd => "SSH answers, but this is not the initrd".into(),
                TargetState::Unreachable => "nothing answers on SSH".into(),
            };
            // "healthcheck passed" is the indicator above.
            if !matches!(r.state, TargetState::Unlocked(_)) {
                lines.push(Line::from(Span::raw(detail)));
            }
            lines.push(Line::default());
            lines.push(row("Route", r.route.clone(), Style::new().cyan()));
            lines.push(row(
                "LUKS method",
                r.luks_type.into(),
                Style::new().magenta(),
            ));
            lines.push(row(
                "Healthcheck",
                r.healthcheck.clone().unwrap_or("not configured".into()),
                Style::new(),
            ));
            lines.push(row(
                "Initrd checksum",
                match &r.checksum_file {
                    Some(_) if r.signature_required => "verified, signature required".into(),
                    Some(_) => "verified, signature optional".into(),
                    None => "not configured".into(),
                },
                if r.checksum_file.is_some() {
                    Style::new().green()
                } else {
                    Style::new().yellow()
                },
            ));
            lines.push(row(
                "Host keys",
                if r.host_keys_pinned {
                    "pinned".into()
                } else {
                    "not fully pinned".into()
                },
                if r.host_keys_pinned {
                    Style::new().green()
                } else {
                    Style::new().yellow()
                },
            ));
            if let Some(u) = &r.uptime {
                lines.push(row("Uptime", u.clone(), Style::new()));
            }
            if let Some(d) = &r.remote_baseline_date {
                lines.push(row("Remote baseline", d.clone(), Style::new()));
            }
            for n in &r.notes {
                let style = match n.severity {
                    Severity::Warn => Style::new().yellow(),
                    Severity::Error => Style::new().red(),
                };
                lines.push(Line::from(Span::styled(format!("• {}", n.text), style)));
            }
            let mut footer = format!(
                "checked {} ({:.1}s)",
                r.checked_at.format("%H:%M:%S"),
                r.duration_ms as f64 / 1000.0
            );
            if let Some((at, state)) = &t.last_unlock {
                footer.push_str(&format!(
                    " · last unlock attempt {}s ago: {}",
                    at.elapsed().as_secs(),
                    state
                        .map(|s| format!("{s:?}"))
                        .unwrap_or_else(|| "aborted".into())
                ));
            }
            lines.push(Line::from(Span::styled(footer, Style::new().dim())));
        } else {
            lines.push(Line::from(Span::styled(
                format!("Checking {}…", t.host),
                Style::new().dim(),
            )));
        }
        lines
    }

    /// The selected target's details, as a dialog (`i`).
    fn draw_info(&self, f: &mut Frame, idx: usize) {
        let t = &self.targets[idx];
        let lines = self.details_lines(idx);
        let screen = f.area();
        let width = screen.width.min(96);
        let inner = usize::from(width.saturating_sub(2)).max(1);
        let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner)).sum();
        let height = u16::try_from(rows)
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(screen.height);
        let area = centered(screen, width, height);
        let title = match t.busy {
            Some(b) => format!(" {} · {b}… · any key closes ", t.name),
            None => format!(" {} · any key closes ", t.name),
        };
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(title)
                    .border_style(Style::new().cyan()),
            ),
            area,
        );
    }

    fn draw_logs(&mut self, f: &mut Frame, area: Rect, idx: usize) {
        self.hit.logs = area;
        // Right of the console (wide layout) rather than below it.
        let beside = area.x > self.hit.body.x;
        let t = &self.targets[idx];
        let height = area.height.saturating_sub(1) as usize;
        let title = if self.logs_full {
            format!(" Activity · {} · l or Esc closes ", t.name)
        } else {
            " Activity · service journal and this dashboard ".to_string()
        };
        let lines: Vec<Line> = t
            .logs
            .iter()
            .map(|l| {
                let style = match l.level {
                    Level::Debug => Style::new().dim(),
                    Level::Info => Style::new(),
                    Level::Warning => Style::new().yellow(),
                    Level::Error => Style::new().red(),
                };
                Line::from(vec![
                    Span::styled(format!("{} ", l.at.format("%H:%M:%S")), Style::new().dim()),
                    // Magenta for what the service logged, blue for what this
                    // dashboard did, so the two streams are easy to tell apart.
                    if l.service {
                        Span::styled("svc ", Style::new().magenta())
                    } else {
                        Span::styled("tui ", Style::new().light_blue())
                    },
                    Span::styled(l.message.clone(), style),
                ])
            })
            .collect();
        let total = lines.len();
        self.log_scroll = self.log_scroll.min(total.saturating_sub(height));
        let end = total - self.log_scroll;
        let window: Vec<Line> = lines[end.saturating_sub(height)..end].to_vec();
        let title = if self.log_scroll > 0 {
            format!("{title}· ↑{} ", self.log_scroll)
        } else if total == 0 {
            format!("{title}· nothing yet ")
        } else {
            title.to_string()
        };
        f.render_widget(
            Paragraph::new(window).wrap(Wrap { trim: false }).block(
                pane()
                    // Beside the console, a rule keeps the two apart.
                    .borders(if beside {
                        Borders::TOP | Borders::LEFT
                    } else {
                        Borders::TOP
                    })
                    .title(title)
                    .border_style(self.border(Focus::Activity)),
            ),
            area,
        );
    }

    fn draw_extra(&mut self, f: &mut Frame, area: Rect, idx: usize) {
        self.hit.extra = area;
        let t = &self.targets[idx];
        if t.cmds.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from("no command configured for this target".dim())).block(
                    pane()
                        .title(format!(" {} ", t.name))
                        .border_style(self.border(Focus::Extra)),
                ),
                area,
            );
            return;
        }
        let c = self.extra_c(idx);
        let (cmd, slot) = (&t.cmds[c], &t.extra[c]);
        let mut title = format!(" {} · {}", cmd.label, t.name);
        if t.cmds.len() > 1 {
            title.push_str(&format!(" · c next {}/{}", c + 1, t.cmds.len()));
        }
        if slot.read_only {
            title.push_str(" · READ-ONLY");
        }
        if slot.running {
            title.push_str(" · live");
        } else if slot.finished_at.is_some() {
            title.push_str(" · ended");
        } else {
            title.push_str(" · starting");
        }
        if self.extra_scroll > 0 {
            title.push_str(&format!(" · ↑{}", self.extra_scroll));
        }
        title.push(' ');
        if cmd.vnc.is_some() {
            self.draw_vnc_pane(f, area, (idx, c), title);
            return;
        }
        if slot.running
            && !(cmd.interactive && slot.screen.is_some())
            && let Some((step, since)) = &slot.loading
        {
            let block = pane().title(title).border_style(self.border(Focus::Extra));
            let inner = block.inner(area);
            f.render_widget(block, area);
            let lines = viewer::loading_screen(
                "Opening the console",
                step,
                since.elapsed(),
                "still waiting for the console",
            );
            render_loading(f, inner, lines);
            return;
        }
        if cmd.interactive && slot.screen.is_some() {
            self.draw_console(f, area, (idx, c), title);
            return;
        }
        let height = area.height.saturating_sub(1) as usize;
        let total = slot.lines.len();
        self.extra_scroll = self.extra_scroll.min(total.saturating_sub(height));
        let end = total - self.extra_scroll;
        let window: Vec<Line> = slot.lines[end.saturating_sub(height)..end].to_vec();
        f.render_widget(
            Paragraph::new(window)
                .wrap(Wrap { trim: false })
                .block(pane().title(title).border_style(self.border(Focus::Extra))),
            area,
        );
    }

    /// An interactive command's pane: its emulated screen, the cursor while
    /// it has the keyboard, and the window size the child is told about.
    fn draw_console(&mut self, f: &mut Frame, area: Rect, (idx, c): (usize, usize), title: String) {
        let captured = self.console_captured();
        let title_width = title.chars().count();
        let mut block = pane().title(title).border_style(if captured {
            Style::new().yellow().bold()
        } else {
            self.border(Focus::Extra)
        });
        let read_only = self.targets[idx].extra[c].read_only;
        let hint = if read_only {
            Some(" read-only: nothing is sent to it ")
        } else if captured {
            Some(" Ctrl+] back to dashboard · Ctrl+R/F5 restart ")
        } else if self.console_live() {
            Some(" click or ⇧↓ to type ")
        } else {
            None
        };
        if let Some(hint) = hint.filter(|h| hint_fits(area.width, title_width, h)) {
            block = block.title(Line::from(hint).right_aligned());
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        self.console_size = (inner.height, inner.width);
        let scroll = self.extra_scroll;
        let Some(parser) = self.targets[idx].extra[c].screen.as_mut() else {
            return;
        };
        if parser.screen().size() != (inner.height, inner.width) {
            parser.screen_mut().set_size(inner.height, inner.width);
        }
        parser.screen_mut().set_scrollback(scroll);
        let screen = parser.screen();
        self.extra_scroll = screen.scrollback();
        paint_screen(screen, inner, f.buffer_mut());
        let (row, col) = screen.cursor_position();
        if captured && !screen.hide_cursor() && screen.scrollback() == 0 {
            f.set_cursor_position((inner.x + col, inner.y + row));
        }
        if let Some(pty) = self
            .extra_run
            .as_mut()
            .filter(|r| (r.t, r.c) == (idx, c))
            .and_then(|r| r.pty.as_mut())
        {
            pty.resize(inner.height, inner.width);
        }
        let slot = &self.targets[idx].extra[c];
        if slot.running
            && let Some((step, since)) = &slot.loading
        {
            // Over the (still empty) screen, which already has the pane's size.
            f.render_widget(Clear, inner);
            let lines = viewer::loading_screen(
                "Opening the console",
                step,
                since.elapsed(),
                "still waiting for the console",
            );
            render_loading(f, inner, lines);
        }
    }

    fn draw_confirm(&mut self, f: &mut Frame, idx: usize, force: bool) {
        let t = &self.targets[idx];
        let area = centered(f.area(), 64, if force { 11 } else { 9 });
        let mut lines = vec![
            Line::from(format!(
                "Send the LUKS passphrase to {} ({})?",
                t.name, t.host
            )),
            Line::default(),
        ];
        if force {
            lines.push(Line::from(
                "FORCE skips the initrd checksum/signature and SSH host-key checks."
                    .red()
                    .bold(),
            ));
            lines.push(Line::from("Only do this if you know why they fail.".red()));
            lines.push(Line::default());
        } else {
            lines.push(Line::from(
                "The usual checks run first; nothing is sent if they fail.".dim(),
            ));
            lines.push(Line::default());
        }
        lines.push(Line::from("y = yes, any other key = cancel".bold()));
        let inner = Rect::new(
            area.x + 1,
            area.y + area.height.saturating_sub(2),
            area.width.saturating_sub(2),
            1,
        );
        self.hit.dialog = area;
        self.hit.yes = Rect::new(inner.x + 1, inner.y, 9.min(inner.width), 1);
        self.hit.cancel = Rect::new(
            self.hit.yes.x + self.hit.yes.width + 2,
            inner.y,
            10.min(inner.width.saturating_sub(11)),
            1,
        );
        f.render_widget(Clear, area);
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                Block::bordered()
                    .title(if force { " Force unlock " } else { " Unlock " })
                    .border_style(if force {
                        Style::new().red()
                    } else {
                        Style::new().yellow()
                    }),
            ),
            area,
        );
        f.render_widget(
            Span::styled(" [ Yes ] ", Style::new().black().on_green().bold()),
            self.hit.yes,
        );
        f.render_widget(
            Span::styled(" [ Cancel ] ", Style::new().black().on_gray().bold()),
            self.hit.cancel,
        );
    }
}

/// One-line state of a report, for the activity log.
fn describe(r: &Report) -> String {
    match &r.state {
        TargetState::Unlocked(d) => format!("unlocked ({d})"),
        TargetState::Initrd(d) if r.unlockable => format!("waiting for passphrase ({d})"),
        TargetState::Initrd(d) => format!("initrd, verification failed ({d})"),
        TargetState::NotInitrd => "booting / not initrd".into(),
        TargetState::SshFailed(d) => format!("SSH login failed ({d})"),
        TargetState::Unreachable => "unreachable".into(),
    }
}

/// The first text that fits in `width` columns, else the last (shortest).
fn pick<'a>(width: u16, options: &[&'a str]) -> &'a str {
    options
        .iter()
        .copied()
        .find(|o| o.chars().count() <= usize::from(width))
        .unwrap_or_else(|| options[options.len() - 1])
}

fn summary(t: &Target) -> (&'static str, String, Color) {
    if t.error.is_some() {
        return ("✖", "config error".into(), Color::Red);
    }
    let Some(r) = &t.report else {
        return ("…", "checking".into(), Color::DarkGray);
    };
    match &r.state {
        TargetState::Unlocked(_) => ("●", "unlocked".into(), Color::Green),
        TargetState::Initrd(_) if r.unlockable => {
            ("◉", "waiting for passphrase".into(), Color::Yellow)
        }
        TargetState::Initrd(_) => ("✖", "initrd, verification failed".into(), Color::Red),
        TargetState::NotInitrd => ("◌", "booting / not initrd".into(), Color::Yellow),
        TargetState::SshFailed(_) => ("✖", "SSH login failed".into(), Color::Red),
        TargetState::Unreachable => ("○", "unreachable".into(), Color::DarkGray),
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// A right-aligned hint goes on a pane's title line only when it fits beside
/// the title, which must stay readable.
fn hint_fits(width: u16, title: usize, hint: &str) -> bool {
    title + hint.chars().count() + 4 <= usize::from(width)
}

/// The frame of a dashboard pane: a title line on top and nothing else, so
/// that the content gets the width and height. Dialogs keep a full border.
fn pane<'a>() -> Block<'a> {
    Block::new().borders(Borders::TOP)
}

/// Draw a loading screen in the middle of AREA.
fn render_loading(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    let top = area.height.saturating_sub(lines.len() as u16) / 2;
    let area = Rect {
        y: area.y + top,
        height: area.height - top,
        ..area
    };
    f.render_widget(
        Paragraph::new(lines).centered().wrap(Wrap { trim: true }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn terminal_input_hangup_detects_closed_pipe() {
        let mut pipe = [-1; 2];
        // SAFETY: pipe initializes both descriptors in the supplied array.
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        // SAFETY: pipe returned two newly-owned descriptors.
        let reader = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
        // SAFETY: pipe returned two newly-owned descriptors.
        let writer = unsafe { OwnedFd::from_raw_fd(pipe[1]) };

        assert!(!terminal_input_hung_up(reader.as_raw_fd()));
        drop(writer);
        assert!(terminal_input_hung_up(reader.as_raw_fd()));
    }

    #[test]
    fn terminal_input_hangup_detects_closed_pty_master() {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes both descriptors on success.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: openpty returned two newly-owned descriptors.
        let master = unsafe { OwnedFd::from_raw_fd(master) };
        // SAFETY: openpty returned two newly-owned descriptors.
        let slave = unsafe { OwnedFd::from_raw_fd(slave) };

        assert!(!terminal_input_hung_up(slave.as_raw_fd()));
        drop(master);
        assert!(terminal_input_hung_up(slave.as_raw_fd()));
    }

    fn app(n: usize) -> App {
        App {
            targets: (0..n)
                .map(|i| Target {
                    name: format!("host{i}"),
                    host: format!("h{i}.example"),
                    error: None,
                    report: None,
                    busy: None,
                    last_unlock: None,
                    logs: VecDeque::new(),
                    unit: None,
                    tx: None,
                    cmds: Vec::new(),
                    extra: Vec::new(),
                })
                .collect(),
            list: ListState::default().with_selected(Some(0)),
            confirm: None,
            refresh: Duration::from_secs(30),
            quit: false,
            log_scroll: 0,
            portrait: false,
            focus: Focus::List,
            logs_full: false,
            extra_full: false,
            help: false,
            info: false,
            drag: None,
            hit: Hit::default(),
            switch_from: None,
            filter: String::new(),
            filtering: false,
            has_extras: false,
            extra_idx: 0,
            extra_scroll: 0,
            extra_run: None,
            extra_gen: 0,
            console_size: (24, 80),
            restart_wait: None,
            force_reload: false,
            extra_pct: 80,
            vnc: VncState::default(),
            msg_tx: mpsc::channel().0,
        }
    }

    /// Give every target the same extra commands.
    fn set_extras(a: &mut App, cmds: Vec<ExtraCmd>) {
        for t in &mut a.targets {
            t.extra = cmds.iter().map(|_| ExtraOut::default()).collect();
            t.cmds = cmds.clone();
        }
        a.has_extras = !cmds.is_empty();
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        term.backend().to_string()
    }

    #[test]
    fn panes_take_the_full_width_under_the_host_line() {
        let mut a = app(2);
        let out = render(&mut a, 120, 30);
        // No target list: the activity has the whole width, below the line
        // that names the selected host.
        assert_eq!((a.hit.logs.x, a.hit.logs.width), (0, 120));
        assert_eq!(a.hit.header.y, 0);
        assert!(a.hit.logs.y > a.hit.header.y);
        assert_eq!(a.hit.list.width, 0);
        assert!(out.lines().next().unwrap().contains("host0"));
        assert!(out.contains("Ctrl+P"));
    }

    #[test]
    fn phone_shows_the_host_on_top() {
        let mut a = app(2);
        let out = render(&mut a, 45, 40);
        assert!(a.portrait);
        assert!(out.lines().next().unwrap().contains("host0"));
        assert_eq!(a.hit.logs.width, 45);
    }

    #[test]
    fn any_size_renders() {
        for (w, h) in [
            (1, 1),
            (10, 5),
            (30, 8),
            (20, 60),
            (45, 40),
            (80, 24),
            (200, 10),
        ] {
            render(&mut app(5), w, h);
            render(&mut app(0), w, h);
            let mut a = app(3);
            a.confirm = Some((1, true));
            render(&mut a, w, h);
            a.logs_full = true;
            render(&mut a, w, h);
            a.logs_full = false;
            a.help = true;
            render(&mut a, w, h);
            a.help = false;
            a.info = true;
            render(&mut a, w, h);
            a.info = false;
            set_extras(&mut a, vec![ExtraCmd::parse("echo hi")]);
            a.extra_full = true;
            render(&mut a, w, h);
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn drag(a: &mut App, from: (u16, u16), to: (u16, u16)) {
        a.mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            from.0,
            from.1,
        ));
        a.mouse(mouse(MouseEventKind::Drag(MouseButton::Left), to.0, to.1));
        a.mouse(mouse(MouseEventKind::Up(MouseButton::Left), to.0, to.1));
    }

    #[test]
    fn shift_arrows_move_focus_top_to_bottom() {
        let mut a = app(2);
        render(&mut a, 120, 30);
        a.key(KeyCode::Down, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::Activity);
        a.key(KeyCode::Up, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::List);
        a.key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(a.focus == Focus::Activity);
        a.key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::List);
    }

    #[test]
    fn plain_arrows_act_in_the_focused_pane() {
        let mut a = app(3);
        render(&mut a, 120, 30);
        // On the host line, the arrows switch hosts.
        a.key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(a.selected(), 1);
        a.key(KeyCode::Down, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::Activity);
        a.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.log_scroll, 1);
        assert_eq!(a.selected(), 1);
    }

    #[test]
    fn clicking_the_host_line_opens_the_switcher() {
        let mut a = app(3);
        render(&mut a, 120, 30);
        let h = a.hit.header;
        a.mouse(mouse(MouseEventKind::Down(MouseButton::Left), h.x + 3, h.y));
        assert!(a.filtering);
        let out = render(&mut a, 120, 30);
        assert!(out.contains("switch host"));
        // A click on a host switches to it.
        let rows = a.hit.list;
        a.mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            rows.x + 3,
            rows.y + 2,
        ));
        assert!(!a.filtering);
        assert_eq!(a.selected(), 2);
        // A click beside the popup closes it on the same host.
        a.key(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(a.filtering);
        render(&mut a, 120, 30);
        a.mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 20));
        assert!(!a.filtering);
        assert_eq!(a.selected(), 2);
    }

    #[test]
    fn clicking_a_pane_focuses_it() {
        let mut a = app(2);
        render(&mut a, 120, 30);
        let logs = a.hit.logs;
        a.mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            logs.x + 3,
            logs.y + 3,
        ));
        assert!(a.focus == Focus::Activity);
    }

    #[test]
    fn l_toggles_fullscreen_activity() {
        let mut a = app(2);
        render(&mut a, 120, 30);
        a.key(KeyCode::Char('l'), KeyModifiers::NONE);
        render(&mut a, 120, 30);
        assert!(a.logs_full);
        assert_eq!(a.hit.logs.width, 120);
        assert_eq!(a.hit.list.width, 0);
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!a.logs_full && !a.quit);
        a.key(KeyCode::Char('l'), KeyModifiers::NONE);
        a.key(KeyCode::Char('l'), KeyModifiers::NONE);
        assert!(!a.logs_full);
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(a.quit);
    }

    #[test]
    fn the_switcher_filters_switches_and_cancels() {
        let mut a = app(3);
        render(&mut a, 120, 30);
        a.key(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(a.filtering);
        a.key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(a.visible(), vec![2]);
        let out = render(&mut a, 120, 30);
        assert!(out.contains("host2") && !out.contains("host1"));
        a.key(KeyCode::Enter, KeyModifiers::NONE);
        // Switched, and every host is there again.
        assert!(!a.filtering && a.filter.is_empty());
        assert_eq!(a.selected(), 2);
        assert_eq!(a.visible().len(), 3);
        // Esc goes back to the host it was opened on.
        a.key(KeyCode::Char('/'), KeyModifiers::NONE);
        a.key(KeyCode::Char('0'), KeyModifiers::NONE);
        assert_eq!(a.selected(), 0);
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!a.filtering && !a.quit);
        assert_eq!(a.selected(), 2);
        // No match renders without panicking.
        a.key(KeyCode::Char('/'), KeyModifiers::NONE);
        a.key(KeyCode::Char('z'), KeyModifiers::NONE);
        let out = render(&mut a, 120, 30);
        assert!(out.contains("no host matches"));
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(a.visible().len(), 3);
        assert_eq!(a.selected(), 2);
    }

    #[test]
    fn extra_command_parsing_and_pane() {
        let c = ExtraCmd::parse("console=openstack::console-log {host}");
        assert_eq!(
            (c.label.as_str(), c.command.as_str()),
            ("console", "openstack::console-log {host}")
        );
        let c = ExtraCmd::parse("openstack::console-log {host}");
        assert_eq!(c.label, "openstack::console-log");
        let c = ExtraCmd::parse("echo a=b");
        assert_eq!(c.label, "echo");
        let mut a = app(2);
        set_extras(&mut a, vec![c]);
        a.targets[0].extra[0].lines = vec![Line::from("boot ok")];
        let out = render(&mut a, 120, 30);
        assert!(out.contains("boot ok"));
        // Wide: the console on the left, most of the width, the activity
        // beside it.
        assert!(a.hit.logs.x >= a.hit.extra.right());
        assert_eq!(a.hit.extra.y, a.hit.logs.y);
        assert!(a.hit.extra.width > 3 * a.hit.logs.width);
        a.focus = Focus::Activity;
        a.key(KeyCode::Left, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::Extra);
        a.key(KeyCode::Right, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::Activity);
        a.key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(a.focus == Focus::List);
        // Dragging the border between console and activity resizes them.
        let before = a.hit.extra.width;
        let edge = a.hit.edge_extra;
        drag(&mut a, (edge.x, edge.y + 3), (edge.x - 20, edge.y + 3));
        render(&mut a, 120, 30);
        assert!(
            a.hit.extra.width < before,
            "{} < {before}",
            a.hit.extra.width
        );
        // Tall: the activity below the console.
        render(&mut a, 45, 40);
        assert!(a.hit.extra.y < a.hit.logs.y);
        assert_eq!(a.hit.extra.x, a.hit.logs.x);
    }

    #[test]
    fn targets_can_override_extra_commands() {
        let common = vec![
            ExtraCmd::parse("console=openstack {name}"),
            ExtraCmd::parse("dmesg=dmesg"),
        ];
        let own = |specs: &[&str]| specs.iter().map(|s| ExtraCmd::parse(s)).collect();
        let cmds = |v: Vec<ExtraCmd>| -> Vec<(String, String)> {
            v.into_iter().map(|c| (c.label, c.command)).collect()
        };
        assert_eq!(cmds(merge_extras(&common, Vec::new())).len(), 2);
        // Replaced, with the others kept.
        let m = cmds(merge_extras(&common, own(&["console=ipmi {host}"])));
        assert_eq!(m[0], ("console".into(), "ipmi {host}".into()));
        assert_eq!(m.len(), 2);
        // Removed with an empty command; extra ones are added.
        let m = cmds(merge_extras(&common, own(&["console=", "x=echo hi"])));
        assert_eq!(
            m,
            [
                ("dmesg".into(), "dmesg".into()),
                ("x".into(), "echo hi".into())
            ]
        );

        // A target without a command has no console pane: the activity takes
        // the whole body.
        let mut a = app(2);
        set_extras(&mut a, common);
        a.targets[1].cmds.clear();
        a.targets[1].extra.clear();
        a.select(1);
        a.fetch_extra();
        assert!(a.extra_run.is_none());
        render(&mut a, 120, 30);
        assert_eq!(a.hit.extra.height, 0);
        assert_eq!(a.hit.logs.height, 28);
        a.key(KeyCode::Char('c'), KeyModifiers::NONE);
    }

    #[test]
    fn s_stops_and_starts_the_service() {
        let unit = |active: &str| UnitInfo {
            load: "loaded".into(),
            active: active.into(),
            sub: "x".into(),
            enabled: "enabled".into(),
        };
        let (tx, rx) = mpsc::channel();
        let mut a = app(1);
        a.targets[0].tx = Some(tx);
        // Enter does not unlock; `u` does.
        a.key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(a.confirm.is_none());
        a.key(KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(a.confirm.is_some());
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(a.confirm.is_none());
        // Unknown state: a note, nothing sent.
        a.key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(rx.try_recv().is_err());
        assert!(
            a.targets[0]
                .logs
                .back()
                .unwrap()
                .message
                .contains("not known")
        );
        a.targets[0].unit = Some(unit("active"));
        a.key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Service(false))));
        a.targets[0].unit = Some(unit("failed"));
        a.key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Service(true))));
        a.targets[0].unit.as_mut().unwrap().load = "not-found".into();
        a.key(KeyCode::Char('S'), KeyModifiers::NONE);
        assert!(rx.try_recv().is_err());
    }

    fn report(state: TargetState, healthcheck: Option<&str>) -> Report {
        Report {
            hostname: "h".into(),
            route: "root@h:22".into(),
            luks_type: "systemd",
            healthcheck: healthcheck.map(Into::into),
            checksum_file: None,
            signature_required: false,
            host_keys_pinned: true,
            baseline_refreshed: None,
            uptime: None,
            remote_baseline_date: None,
            remote_baseline_at: None,
            state,
            unlockable: false,
            notes: Vec::new(),
            checked_at: Local::now(),
            duration_ms: 100,
        }
    }

    #[test]
    fn list_shows_state_without_healthcheck() {
        let mut a = app(3);
        a.targets[0].report = Some(report(TargetState::Unlocked("up".into()), Some("hc")));
        a.targets[1].report = Some(report(TargetState::Unreachable, Some("hc")));
        a.targets[2].report = Some(report(TargetState::Unlocked("up".into()), None));
        let out = render(&mut a, 120, 30);
        assert!(!out.contains("hc ●"), "{out}");
        // The selected host's state on top, everyone's in the switcher.
        assert!(out.lines().next().unwrap().contains("unlocked"));
        a.key(KeyCode::Char('p'), KeyModifiers::CONTROL);
        let out = render(&mut a, 120, 30);
        assert!(!out.contains("hc ●"), "{out}");
        assert!(out.contains("unlocked") && out.contains("unreachable"));
    }

    #[test]
    fn i_shows_the_details_dialog() {
        let mut a = app(2);
        let out = render(&mut a, 120, 30);
        assert!(!out.contains("Checking"), "details are not in the layout");
        a.key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert!(a.info);
        let out = render(&mut a, 120, 30);
        assert!(out.contains("Checking h0.example"));
        // Any key closes it, and is not acted on.
        a.key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!a.info && !a.quit);
        // In the switcher, `i` is typed into the filter.
        a.key(KeyCode::Char('/'), KeyModifiers::NONE);
        a.key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert!(!a.info && a.filter == "i");
    }

    #[test]
    fn help_and_fullscreen_console() {
        let mut a = app(2);
        set_extras(&mut a, vec![ExtraCmd::parse("echo hi")]);
        a.targets[0].extra[0].lines = vec![Line::from("console line")];
        render(&mut a, 120, 30);
        a.key(KeyCode::Char('?'), KeyModifiers::NONE);
        assert!(a.help);
        assert!(render(&mut a, 120, 30).contains("Keys"));
        // Any key closes it, and is not acted on.
        a.key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!a.help && !a.quit);

        a.key(KeyCode::Char('C'), KeyModifiers::NONE);
        let out = render(&mut a, 120, 30);
        assert!(a.extra_full && a.focus == Focus::Extra);
        assert!(out.contains("console line"));
        assert_eq!(a.hit.extra.width, 120);
        assert_eq!(a.hit.list.width, 0);
        // `l` switches to the activity, Esc goes back, then quits.
        a.key(KeyCode::Char('l'), KeyModifiers::NONE);
        assert!(a.logs_full && !a.extra_full);
        a.key(KeyCode::Char('C'), KeyModifiers::NONE);
        assert!(a.extra_full && !a.logs_full);
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!a.extra_full && !a.quit);
        // Without extra commands `C` does nothing.
        let mut b = app(1);
        b.key(KeyCode::Char('C'), KeyModifiers::NONE);
        assert!(!b.extra_full);
    }

    #[test]
    fn ansi_colors_are_kept() {
        let l = ansi_line("a\x1b[31mred\x1b[0m \x1b[1;38;5;12mblue\x1b[2Jx\x07\ty");
        assert_eq!(l.spans[0].content, "a");
        assert_eq!(l.spans[1].content, "red");
        assert_eq!(l.spans[1].style.fg, Some(Color::Indexed(1)));
        assert_eq!(l.spans[2].style, Style::new());
        assert_eq!(l.spans[3].style.fg, Some(Color::Indexed(12)));
        assert!(l.spans[3].style.add_modifier.contains(Modifier::BOLD));
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "a".to_string() + "red blue" + "x    y");
    }

    /// Run the pending messages of the channel until `done` holds.
    fn pump(a: &mut App, rx: &Receiver<Msg>, done: impl Fn(&App) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(a) {
            assert!(Instant::now() < end, "timed out");
            if let Ok(m) = rx.recv_timeout(Duration::from_millis(50)) {
                a.apply(m);
            }
        }
    }

    #[test]
    fn extra_output_streams_and_stops() {
        let (tx, rx) = mpsc::channel();
        let mut a = app(2);
        a.msg_tx = tx;
        set_extras(
            &mut a,
            vec![ExtraCmd::parse(
                "echo $LUKS_SSH_UNLOCK_NAME {host}; printf '\\033[31mred\\033[0m\\n' >&2; sleep 0.3; echo late; exit 3",
            )],
        );
        a.fetch_extra();
        assert!(a.extra_run.is_some() && a.targets[0].extra[0].running);
        // The first lines arrive while the command still runs.
        pump(&mut a, &rx, |a| a.targets[0].extra[0].lines.len() >= 2);
        assert!(a.targets[0].extra[0].running);
        let first: String = a.targets[0].extra[0].lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(first, "host0 h0.example");
        pump(&mut a, &rx, |a| !a.targets[0].extra[0].running);
        let lines = &a.targets[0].extra[0].lines;
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Indexed(1)));
        let last: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(last, "[exit 3]");
        assert!(a.extra_run.is_none());

        // A command that never ends is killed when another target is selected.
        set_extras(
            &mut a,
            vec![ExtraCmd::parse("while :; do echo x; sleep 0.05; done")],
        );
        a.fetch_extra();
        let pgid = a.extra_run.as_ref().unwrap().pgid;
        pump(&mut a, &rx, |a| a.targets[0].extra[0].lines.len() >= 2);
        a.select(1);
        a.fetch_extra();
        assert_eq!(a.extra_run.as_ref().unwrap().t, 1);
        assert!(!a.targets[0].extra[0].running);
        let end = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(-pgid, 0) } == 0 {
            assert!(Instant::now() < end, "process group still alive");
            thread::sleep(Duration::from_millis(20));
        }
        a.stop_extra(true);
    }

    #[test]
    fn interactive_marker_parses() {
        let c = ExtraCmd::parse("@ssh -t host");
        assert!(c.interactive);
        assert_eq!(
            (c.label.as_str(), c.command.as_str()),
            ("ssh", "ssh -t host")
        );
        let c = ExtraCmd::parse("oci=@  oci-console --x=1");
        assert!(c.interactive);
        assert_eq!(
            (c.label.as_str(), c.command.as_str()),
            ("oci", "oci-console --x=1")
        );
        let c = ExtraCmd::parse("logs=journalctl -f");
        assert!(!c.interactive && c.command == "journalctl -f");
        assert!(!ExtraCmd::parse("echo a@b").interactive);
        // A target's own command replaces the common one with its label.
        let merged = merge_extras(
            &[ExtraCmd::parse("oci=journalctl")],
            vec![ExtraCmd::parse("oci=@console")],
        );
        assert!(merged.len() == 1 && merged[0].interactive && merged[0].command == "console");
    }

    #[test]
    fn keys_are_encoded_like_a_terminal() {
        let enc = |code, mods| encode_key(code, mods, false);
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let alt = KeyModifiers::ALT;
        assert_eq!(enc(KeyCode::Char('a'), none), b"a");
        assert_eq!(enc(KeyCode::Char('A'), KeyModifiers::SHIFT), b"A");
        assert_eq!(enc(KeyCode::Char('é'), none), "é".as_bytes());
        assert_eq!(enc(KeyCode::Char('c'), ctrl), [3]);
        assert_eq!(enc(KeyCode::Char('d'), ctrl), [4]);
        assert_eq!(enc(KeyCode::Char('5'), ctrl), [0x1d]);
        assert_eq!(enc(KeyCode::Char('x'), alt), b"\x1bx");
        assert_eq!(enc(KeyCode::Enter, none), b"\r");
        assert_eq!(enc(KeyCode::Esc, none), b"\x1b");
        assert_eq!(enc(KeyCode::Tab, none), b"\t");
        assert_eq!(enc(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(enc(KeyCode::Backspace, none), [0x7f]);
        assert_eq!(enc(KeyCode::Up, none), b"\x1b[A");
        assert_eq!(encode_key(KeyCode::Up, none, true), b"\x1bOA");
        assert_eq!(enc(KeyCode::Left, ctrl), b"\x1b[1;5D");
        assert_eq!(enc(KeyCode::Home, none), b"\x1b[H");
        assert_eq!(enc(KeyCode::Delete, none), b"\x1b[3~");
        assert_eq!(enc(KeyCode::PageUp, none), b"\x1b[5~");
        assert_eq!(enc(KeyCode::F(1), none), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(5), none), b"\x1b[15~");
        assert_eq!(enc(KeyCode::F(5), ctrl), b"\x1b[15;5~");
        assert_eq!(enc(KeyCode::F(12), none), b"\x1b[24~");
        assert!(enc(KeyCode::CapsLock, none).is_empty());
    }

    #[test]
    fn reserved_keys_are_recognised() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        assert!(is_console_escape(KeyCode::Char(']'), ctrl));
        assert!(is_console_escape(KeyCode::Char('5'), ctrl));
        assert!(!is_console_escape(KeyCode::Char(']'), none));
        assert!(!is_console_escape(KeyCode::Esc, none));
        assert!(is_console_restart(KeyCode::Char('r'), ctrl));
        assert!(is_console_restart(KeyCode::Char('R'), ctrl));
        assert!(is_console_restart(KeyCode::F(5), none));
        assert!(!is_console_restart(KeyCode::Char('r'), none));
        assert!(!is_console_restart(KeyCode::F(5), ctrl));
        assert!(!is_console_restart(KeyCode::F(6), none));
    }

    #[test]
    fn terminal_queries_are_answered() {
        assert_eq!(terminal_replies(b"x\x1b[6ny", (4, 9)), b"\x1b[5;10R");
        assert_eq!(terminal_replies(b"\x1b[5n", (0, 0)), b"\x1b[0n");
        assert_eq!(terminal_replies(b"\x1b[c", (0, 0)), b"\x1b[?1;2c");
        assert!(terminal_replies(b"\x1b[31mred", (0, 0)).is_empty());
        // A query cut in two chunks is kept for the next one.
        assert_eq!(partial_query(b"abc\x1b["), 3);
        assert_eq!(partial_query(b"abc\x1b"), 3);
        assert_eq!(partial_query(b"abc\x1b[6n"), 7);
        assert_eq!(partial_query(b"abc"), 3);
    }

    /// An app whose selected target runs an interactive command, with the
    /// console focused.
    fn console_app(command: &str) -> (App, Receiver<Msg>) {
        let (tx, rx) = mpsc::channel();
        let mut a = app(1);
        a.msg_tx = tx;
        set_extras(&mut a, vec![ExtraCmd::parse(&format!("con=@{command}"))]);
        a.focus = Focus::Extra;
        a.fetch_extra();
        assert!(a.extra_run.as_ref().is_some_and(|r| r.pty.is_some()));
        render(&mut a, 160, 40);
        (a, rx)
    }

    fn screen_text(a: &App) -> String {
        a.targets[0].extra[0]
            .screen
            .as_ref()
            .map(|p| p.screen().contents())
            .unwrap_or_default()
    }

    fn type_text(a: &mut App, text: &str) {
        for ch in text.chars() {
            a.key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    /// Like `pump`, and keeps the dashboard's own housekeeping going.
    fn pump_fetch(a: &mut App, rx: &Receiver<Msg>, done: impl Fn(&App) -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while !done(a) {
            assert!(Instant::now() < end, "timed out");
            if let Ok(m) = rx.recv_timeout(Duration::from_millis(20)) {
                a.apply(m);
            }
            a.fetch_extra();
        }
    }

    fn gone(pgid: i32) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(-pgid, 0) } == 0 {
            if Instant::now() > end {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn console_shows_a_prompt_and_takes_typing() {
        let (mut a, rx) = console_app("printf 'login: '; read u; printf '\\nhello %s\\n' \"$u\"");
        assert!(a.console_captured());
        // The prompt has no newline, and shows at once.
        pump(&mut a, &rx, |a| screen_text(a).contains("login:"));
        assert_eq!(screen_text(&a).trim(), "login:");
        assert!(a.targets[0].extra[0].running);
        let out = render(&mut a, 160, 40);
        assert!(out.contains("login:"), "{out}");
        assert!(out.contains("Ctrl+] back to dashboard"), "{out}");
        assert!(out.contains("Ctrl+R/F5 restart"), "{out}");
        // The keys go to the program, which echoes them.
        type_text(&mut a, "bob");
        pump(&mut a, &rx, |a| screen_text(a).contains("login: bob"));
        a.key(KeyCode::Enter, KeyModifiers::NONE);
        pump(&mut a, &rx, |a| !a.targets[0].extra[0].running);
        let text = screen_text(&a);
        assert!(text.contains("hello bob"), "{text}");
        assert!(text.contains("[exited]"), "{text}");
        // An ended console does not keep the keyboard.
        assert!(!a.console_captured());
        assert!(a.extra_run.is_none());
        assert!(!a.quit);
    }

    #[test]
    fn console_gets_every_key_unchanged() {
        let (mut a, rx) = console_app("stty raw -echo opost; echo RAW; head -c 6 | od -An -tx1");
        pump(&mut a, &rx, |a| screen_text(a).contains("RAW"));
        // `q` quits and Esc leaves the dashboard otherwise; Ctrl+C is the
        // dashboard's quit key in the filter. Here the program gets them all.
        a.key(KeyCode::Char('q'), KeyModifiers::NONE);
        a.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        a.key(KeyCode::Tab, KeyModifiers::NONE);
        a.key(KeyCode::Char('?'), KeyModifiers::NONE);
        a.key(KeyCode::Up, KeyModifiers::NONE);
        pump(&mut a, &rx, |a| !a.targets[0].extra[0].running);
        let text = screen_text(&a);
        assert!(text.contains("71 03 1b 09 3f 1b"), "{text}");
        assert!(!a.quit && !a.help && a.focus == Focus::Extra);
    }

    #[test]
    fn console_follows_the_pane_size() {
        let (mut a, rx) =
            console_app("trap 'stty size' WINCH; stty size; while :; do sleep 0.05; done");
        // Started with the size of the pane when the first screen was drawn.
        let (rows, cols) = a.console_size;
        assert_ne!((rows, cols), (24, 80));
        let want = format!("{rows} {cols}");
        pump(&mut a, &rx, |a| {
            screen_text(a).lines().any(|l| l.trim() == want)
        });
        // A new window size is passed on (SIGWINCH).
        render(&mut a, 100, 30);
        let (rows, cols) = a.console_size;
        assert_ne!((rows, cols), (24, 80));
        let want = format!("{rows} {cols}");
        pump(&mut a, &rx, |a| {
            screen_text(a).lines().any(|l| l.trim() == want)
        });
        let screen = a.targets[0].extra[0].screen.as_ref().unwrap().screen();
        assert_eq!(screen.size(), (rows, cols));
        a.stop_extra(true);
    }

    #[test]
    fn ctrl_bracket_gives_the_keyboard_back() {
        for (code, name) in [(KeyCode::Char(']'), "]"), (KeyCode::Char('5'), "5")] {
            let (mut a, rx) = console_app("cat");
            let pgid = a.extra_run.as_ref().unwrap().pgid;
            assert!(a.console_captured());
            a.key(code, KeyModifiers::CONTROL);
            assert!(
                a.focus == Focus::List && !a.console_captured(),
                "Ctrl+{name}"
            );
            assert!(!a.quit);
            // Nothing was typed into the console (`cat` echoes everything).
            let end = Instant::now() + Duration::from_millis(300);
            while Instant::now() < end {
                if let Ok(m) = rx.recv_timeout(Duration::from_millis(20)) {
                    a.apply(m);
                }
            }
            assert_eq!(screen_text(&a).trim(), "");
            assert!(a.targets[0].extra[0].running);
            // It still runs, and keeps running with the dashboard focused.
            assert!(a.extra_run.is_some());
            let out = render(&mut a, 160, 40);
            assert!(!out.contains("Ctrl+] back to dashboard"), "{out}");
            a.stop_extra(true);
            assert!(gone(pgid));
        }
    }

    #[test]
    fn shift_arrows_and_clicks_leave_the_console_too() {
        let (mut a, _rx) = console_app("cat");
        a.key(KeyCode::Up, KeyModifiers::SHIFT);
        assert!(a.focus != Focus::Extra && !a.console_captured());
        a.stop_extra(true);
    }

    #[test]
    fn ctrl_r_and_f5_restart_the_console() {
        for (code, mods) in [
            (KeyCode::Char('r'), KeyModifiers::CONTROL),
            (KeyCode::F(5), KeyModifiers::NONE),
        ] {
            let (mut a, rx) = console_app("printf 'pid:%s\\n' $$; sleep 30");
            let pid_of = |a: &App| {
                screen_text(a)
                    .split("pid:")
                    .nth(1)
                    .and_then(|s| s.split_whitespace().next())
                    .and_then(|s| s.parse::<i32>().ok())
            };
            pump(&mut a, &rx, |a| pid_of(a).is_some());
            let (old_pid, old_pgid) = (pid_of(&a).unwrap(), a.extra_run.as_ref().unwrap().pgid);
            let old_run = a.targets[0].extra[0].run_id;
            // Not sent to the program: the console starts again instead.
            a.key(code, mods);
            assert!(a.focus == Focus::Extra && !a.quit);
            pump_fetch(&mut a, &rx, |a| {
                pid_of(a).is_some_and(|p| p != old_pid) && a.targets[0].extra[0].running
            });
            assert_ne!(a.targets[0].extra[0].run_id, old_run);
            assert!(gone(old_pgid), "the old console is still running");
            assert!(a.console_captured());
            let new_pgid = a.extra_run.as_ref().unwrap().pgid;
            a.stop_extra(true);
            assert!(gone(new_pgid));
        }
    }

    #[test]
    fn an_ended_console_restarts_with_ctrl_r() {
        let (mut a, rx) = console_app("echo once");
        pump(&mut a, &rx, |a| !a.targets[0].extra[0].running);
        let text = screen_text(&a);
        assert!(text.contains("once") && text.contains("[exited]"), "{text}");
        assert!(text.contains("Ctrl+R or F5 restarts"), "{text}");
        let old_run = a.targets[0].extra[0].run_id;
        // It is not started again on its own.
        for _ in 0..5 {
            a.fetch_extra();
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!a.targets[0].extra[0].running && a.extra_run.is_none());
        // The keys of the console still work, the others are the dashboard's.
        a.key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        pump_fetch(&mut a, &rx, |a| {
            a.targets[0].extra[0].run_id != old_run && a.targets[0].extra[0].run_id != 0
        });
        pump(&mut a, &rx, |a| !a.targets[0].extra[0].running);
        assert!(screen_text(&a).contains("once"));
        assert!(!a.console_captured());
    }

    #[test]
    fn console_programs_get_answers_to_their_queries() {
        let (mut a, rx) =
            console_app("stty raw -echo opost; printf '\\033[6n'; head -c 6 | od -An -tx1");
        pump(&mut a, &rx, |a| !a.targets[0].extra[0].running);
        let text = screen_text(&a);
        assert!(text.contains("1b 5b 31 3b 31 52"), "{text}");
    }

    #[test]
    fn console_scrolls_back_with_shift_page_up() {
        let (mut a, rx) =
            console_app("i=1; while [ $i -le 100 ]; do echo line$i; i=$((i+1)); done; sleep 30");
        pump(&mut a, &rx, |a| screen_text(a).contains("line100"));
        let out = render(&mut a, 160, 40);
        assert!(out.contains("line100") && !out.contains("line1 "), "{out}");
        for _ in 0..10 {
            a.key(KeyCode::PageUp, KeyModifiers::SHIFT);
        }
        let out = render(&mut a, 160, 40);
        assert!(a.extra_scroll > 0);
        assert!(out.contains("line1 ") && !out.contains("line100"), "{out}");
        // Typing goes back to the end.
        a.key(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(a.extra_scroll, 0);
        a.stop_extra(true);
    }

    #[test]
    fn console_footer_names_the_escape() {
        let (mut a, _rx) = console_app("cat");
        let out = render(&mut a, 160, 40);
        assert!(out.contains("Ctrl+] back to the dashboard"), "{out}");
        a.key(KeyCode::Char(']'), KeyModifiers::CONTROL);
        let out = render(&mut a, 160, 40);
        assert!(!out.contains("Ctrl+] back to the dashboard"), "{out}");
        a.key(KeyCode::Char('?'), KeyModifiers::NONE);
        let out = render(&mut a, 160, 40);
        assert!(out.contains("Ctrl+R  F5"), "{out}");
        a.stop_extra(true);
    }

    fn vnc_endpoint(view_only: bool) -> crate::console::VncEndpoint {
        crate::console::VncEndpoint {
            source: crate::console::VncSource::Direct(crate::console::DirectVnc {
                host: "127.0.0.1".into(),
                port: 5900,
                allow_remote: false,
            }),
            password_file: None,
            view_only,
            quality: None,
            compression: None,
            resize_remote: false,
            keymap: None,
        }
    }

    /// A dashboard whose only target has a VNC pane (and a second, plain
    /// view), with terminal graphics ruled out: no session is ever started.
    fn vnc_app(view_only: bool) -> App {
        let mut a = app(1);
        let vnc = ExtraCmd {
            label: "vnc".into(),
            command: "VNC".into(),
            interactive: false,
            vnc: Some(vnc_endpoint(view_only)),
        };
        set_extras(&mut a, vec![vnc, ExtraCmd::parse("other=echo hi")]);
        a.focus = Focus::Extra;
        a.vnc.checked = true;
        a.vnc.problem = Some("no graphics in tests".into());
        a
    }

    #[test]
    fn a_vnc_pane_takes_the_keyboard_like_a_console() {
        let mut a = vnc_app(false);
        render(&mut a, 160, 40);
        assert_eq!(a.vnc_cmd(), Some((0, 0)));
        assert!(a.console_captured());
        // The dashboard does not act on the keys meant for the desktop.
        a.key(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!a.quit);
        // `C` remains the dashboard's fullscreen toggle while VNC owns keys.
        a.key(KeyCode::Char('C'), KeyModifiers::NONE);
        assert!(a.extra_full);
        assert!(a.focus == Focus::Extra);
        a.key(KeyCode::Char('C'), KeyModifiers::NONE);
        assert!(!a.extra_full);
        // Ctrl+] gives the keyboard back.
        a.key(KeyCode::Char(']'), KeyModifiers::CONTROL);
        assert!(a.focus == Focus::List);
        assert!(!a.console_captured());
        // Shift+Up returns from fullscreen to the dashboard and changes focus
        // (the console is on top: up is the host line).
        a.focus = Focus::Extra;
        a.key(KeyCode::Char('C'), KeyModifiers::NONE);
        a.key(KeyCode::Up, KeyModifiers::SHIFT);
        assert!(!a.extra_full);
        assert!(a.focus == Focus::List);
    }

    #[test]
    fn vnc_footer_only_advertises_f6_when_another_view_exists() {
        let mut one_view = vnc_app(false);
        one_view.targets[0].cmds.truncate(1);
        one_view.targets[0].extra.truncate(1);
        one_view.targets[0].extra[0].running = true;
        let text = render(&mut one_view, 160, 40);
        assert!(!text.contains("F6"), "{text}");

        let mut multiple_views = vnc_app(false);
        multiple_views.targets[0].extra[0].running = true;
        let text = render(&mut multiple_views, 160, 40);
        assert!(text.contains("F6 next view"), "{text}");
    }

    #[test]
    fn a_view_only_desktop_leaves_the_keyboard_to_the_dashboard() {
        let mut a = vnc_app(true);
        render(&mut a, 160, 40);
        assert!(a.vnc_view_only(0, 0));
        assert!(!a.console_captured());
        // Ctrl+T hands the keyboard to the desktop, and takes it back.
        a.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(!a.vnc_view_only(0, 0));
        assert!(a.console_captured());
        a.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(a.vnc_view_only(0, 0));
        assert!(!a.console_captured());
        // The reserved keys work either way.
        a.key(KeyCode::Char(']'), KeyModifiers::CONTROL);
        assert!(a.focus == Focus::List);
    }

    #[test]
    fn shift_arrows_still_move_the_focus_out_of_a_vnc_pane() {
        let mut a = vnc_app(false);
        render(&mut a, 160, 40);
        a.key(KeyCode::Up, KeyModifiers::SHIFT);
        assert!(a.focus != Focus::Extra);
    }

    #[test]
    fn f6_cycles_the_views_of_a_target() {
        let mut a = vnc_app(false);
        render(&mut a, 160, 40);
        assert_eq!(a.console_cmd(), Some((0, 0)));
        a.key(KeyCode::F(6), KeyModifiers::NONE);
        assert_eq!(a.extra_c(0), 1);
        // The second view is not a console: F6 is the dashboard's again.
        a.key(KeyCode::F(6), KeyModifiers::NONE);
        assert_eq!(a.extra_c(0), 0);
        assert_eq!(a.vnc_cmd(), Some((0, 0)));
    }

    #[test]
    fn restarting_a_vnc_pane_forgets_how_the_last_session_ended() {
        for restart in [
            (KeyCode::Char('r'), KeyModifiers::CONTROL),
            (KeyCode::F(5), KeyModifiers::NONE),
        ] {
            let mut a = vnc_app(false);
            render(&mut a, 160, 40);
            a.fetch_vnc(0, 0);
            // Graphics are ruled out: the pane says so and nothing connects.
            assert!(a.vnc.run.is_none());
            assert!(a.targets[0].extra[0].finished_at.is_some());
            assert_eq!(a.targets[0].extra[0].vnc_note, "no graphics in tests");
            assert!(!a.console_captured());
            a.key(restart.0, restart.1);
            assert!(a.targets[0].extra[0].finished_at.is_none());
            assert!(a.targets[0].extra[0].vnc_note.is_empty());
            // The terminal is asked again.
            assert!(!a.vnc.checked);
            assert!(a.console_captured());
        }
    }

    #[test]
    fn the_problem_is_shown_in_the_pane() {
        let mut a = vnc_app(false);
        a.fetch_vnc(0, 0);
        let out = render(&mut a, 160, 40);
        assert!(out.contains("no graphics in tests"), "{out}");
        assert!(out.contains("Ctrl+R or F5"), "{out}");
    }

    #[test]
    fn an_unencrypted_endpoint_is_labelled() {
        let mut a = vnc_app(false);
        let local = render(&mut a, 160, 40);
        assert!(!local.contains("UNENCRYPTED"), "{local}");
        for t in &mut a.targets {
            let v = t.cmds[0].vnc.as_mut().unwrap();
            v.source = crate::console::VncSource::Direct(crate::console::DirectVnc {
                host: "192.0.2.10".into(),
                port: 5900,
                allow_remote: true,
            });
        }
        let remote = render(&mut a, 160, 40);
        assert!(remote.contains("UNENCRYPTED"), "{remote}");
    }

    #[test]
    fn vnc_panes_render_at_any_size() {
        for (w, h) in [(1, 1), (10, 5), (30, 8), (45, 40), (80, 24), (200, 10)] {
            let mut a = vnc_app(false);
            render(&mut a, w, h);
            a.fetch_vnc(0, 0);
            render(&mut a, w, h);
            a.extra_full = true;
            render(&mut a, w, h);
            a.help = true;
            render(&mut a, w, h);
        }
    }

    #[test]
    fn the_default_view_comes_first() {
        let cmd = |label: &str| ExtraCmd {
            label: label.into(),
            command: "x".into(),
            interactive: false,
            vnc: None,
        };
        let labels = |v: Vec<ExtraCmd>| v.into_iter().map(|c| c.label).collect::<Vec<_>>();
        let cmds = vec![cmd("console"), cmd("log"), cmd("vnc")];
        assert_eq!(
            labels(default_first(cmds.clone(), Some("vnc"))),
            ["vnc", "console", "log"]
        );
        assert_eq!(
            labels(default_first(cmds.clone(), None)),
            ["console", "log", "vnc"]
        );
        assert_eq!(
            labels(default_first(cmds, Some("missing"))),
            ["console", "log", "vnc"]
        );
    }

    #[test]
    fn discover_reads_the_vnc_file_next_to_the_console_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("web.env"), "HOST=h.example\n").unwrap();
        std::fs::write(
            dir.path().join("web.vnc.json"),
            r#"{"type":"vnc","parameters":{"port":5901,"viewOnly":true}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("db.env"), "HOST=d.example\n").unwrap();
        let sources = discover(&[dir.path().to_path_buf()]).unwrap();
        let web = sources.iter().find(|s| s.name == "web").unwrap();
        let v = web.extras.iter().find_map(|c| c.vnc.as_ref()).unwrap();
        let crate::console::VncSource::Direct(d) = &v.source else {
            panic!("not a direct endpoint")
        };
        assert_eq!(
            (d.host.as_str(), d.port, v.view_only),
            ("127.0.0.1", 5901, true)
        );
        let db = sources.iter().find(|s| s.name == "db").unwrap();
        assert!(db.extras.iter().all(|c| c.vnc.is_none()));
        // A bad file is an error naming the file, not a silent skip.
        std::fs::write(dir.path().join("web.vnc.json"), "{").unwrap();
        let err = format!("{:#}", discover(&[dir.path().to_path_buf()]).err().unwrap());
        assert!(err.contains("web.vnc.json"), "{err}");
    }

    #[test]
    fn footer_hint_fits() {
        assert_eq!(pick(5, &["long text", "short"]), "short");
        assert_eq!(pick(3, &["long text", "short"]), "short");
        assert_eq!(pick(20, &["long text", "short"]), "long text");
    }
}
