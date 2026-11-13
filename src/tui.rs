//! Interactive dashboard: every configured target with its live state,
//! details, activity (the service's journal plus its own actions), and one-key (confirmed) unlocks.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::ctx::{Ctx, Mode};
use crate::daemon::State;
use crate::log::{Level, LogLine, Logger};
use crate::settings::{GlobalOpts, Settings, parse_env_file};
use crate::status::{Report, Severity, TargetState};
use chrono::{DateTime, Local, TimeZone};

const LOG_LINES: usize = 500;
const EXTRA_LINES: usize = 2000;
/// How long an extra command gets to clean up after SIGTERM before SIGKILL.
const EXTRA_GRACE: Duration = Duration::from_secs(5);
/// Minimum time between two checks triggered by new journal lines.
const EVENT_PROBE_GAP: Duration = Duration::from_secs(2);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WHEEL_LINES: usize = 3;
pub const DEFAULT_CONFIG_DIR: &str = "/etc/luks-ssh-unlock";

/// An extra command whose output is shown in an optional fourth pane.
#[derive(Clone)]
pub struct ExtraCmd {
    pub label: String,
    pub command: String,
}

impl ExtraCmd {
    /// `[LABEL=]COMMAND`. Without a label, the command's first word is used.
    pub fn parse(spec: &str) -> Self {
        if let Some((label, command)) = spec.split_once('=')
            && !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
        {
            return Self {
                label: label.into(),
                command: command.trim().into(),
            };
        }
        Self {
            label: spec.split_whitespace().next().unwrap_or(spec).into(),
            command: spec.trim().into(),
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

/// Start an extra command for a target and stream its output (stdout and
/// stderr) to the dashboard line by line. `{host}` and `{name}` are replaced
/// by the quoted values, and also exported as `LUKS_SSH_UNLOCK_HOST` and
/// `LUKS_SSH_UNLOCK_NAME`. Returns its process group and a flag that is set
/// once it has been reaped.
fn spawn_extra(
    command: &str,
    (t, c, run_id): (usize, usize, u64),
    (name, host): (&str, &str),
    tx: Sender<Msg>,
) -> std::io::Result<(i32, Arc<AtomicBool>)> {
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
  exec sudo -n -H -u "$2" -- env "LUKS_SSH_UNLOCK_NAME=$3" "LUKS_SSH_UNLOCK_HOST=$4" sh -c "$1"
fi
LUKS_SSH_UNLOCK_NAME=$3 LUKS_SSH_UNLOCK_HOST=$4
export LUKS_SSH_UNLOCK_NAME LUKS_SSH_UNLOCK_HOST
exec sh -c "$1""#,
        "sh",
        &script,
        &user,
        name,
        host,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .process_group(0);
    // Do not leave the command running when the dashboard dies.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
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

/// One target to show: a name and the environment describing it.
pub struct Source {
    pub name: String,
    /// This target's own extra commands (`<name>.tui-commands`).
    pub extras: Vec<ExtraCmd>,
    pub env: Option<std::collections::HashMap<String, String>>,
}

/// Environment files given on the command line, or every `*.env` in the
/// NixOS module's directory.
pub fn discover(files: &[PathBuf]) -> Result<Vec<Source>> {
    let paths: Vec<PathBuf> = if files.is_empty() {
        let mut found: Vec<PathBuf> = match fs::read_dir(DEFAULT_CONFIG_DIR) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "env"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(
                    anyhow::Error::new(e).context(format!("cannot read {DEFAULT_CONFIG_DIR}"))
                );
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
            let text =
                fs::read_to_string(p).with_context(|| format!("cannot read {}", p.display()))?;
            Ok(Source {
                extras: read_extras(&p.with_extension("tui-commands")),
                name: p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| p.display().to_string()),
                env: Some(parse_env_file(&text)),
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
    Details,
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
    /// Between the target list and the details.
    List,
    /// Between the details and the activity.
    Details,
    /// Between the activity and the extra pane.
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
    drag: Option<Edge>,
    /// Pane sizes set by dragging the borders (otherwise sized to their
    /// content): landscape list width and height of the top row; portrait
    /// list height; and the portrait details height in percent.
    land_list_w: Option<u16>,
    land_top_h: Option<u16>,
    port_list_h: Option<u16>,
    port_details_pct: u16,
    /// Screen areas of the last frame, for mouse hit-testing.
    hit: Hit,
    last_click: Option<(Instant, usize)>,
    /// Target filter (`/`): typed text, and whether it is being edited.
    filter: String,
    filtering: bool,
    /// Optional extra commands, shown in a fourth pane; `c` switches.
    has_extras: bool,
    extra_idx: usize,
    extra_scroll: usize,
    extra_run: Option<ExtraRun>,
    extra_gen: u64,
    /// Height of the extra pane, in percent of the space it shares with the activity.
    extra_pct: u16,
    msg_tx: Sender<Msg>,
}

#[derive(Default)]
struct Hit {
    body: Rect,
    list: Rect,
    details: Rect,
    logs: Rect,
    extra: Rect,
    /// The activity and the extra pane together, and the border between them.
    split: Rect,
    edge_extra: Rect,
    edge_list: Rect,
    edge_details: Rect,
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

pub fn run(
    sources: Vec<Source>,
    opts: &GlobalOpts,
    refresh: Duration,
    extras: Vec<ExtraCmd>,
    filter: String,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let mut targets = Vec::new();
    for (idx, src) in sources.into_iter().enumerate() {
        let settings = match &src.env {
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
        let host = settings
            .as_ref()
            .map(|s| s.hostname.clone())
            .unwrap_or_default();
        let cmds = merge_extras(&extras, src.extras.clone());
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
        match settings.and_then(|mut s| {
            s.once = true;
            Ctx::new(s, logger, Mode::Status)
        }) {
            Ok(ctx) => {
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
            Err(e) => {
                let mut msg = format!("{e:#}");
                if msg.contains("not readable") && unsafe { libc::geteuid() } != 0 {
                    msg.push_str(" (run as root?)");
                }
                target.error = Some(msg);
            }
        }
        targets.push(target);
    }
    if targets
        .iter()
        .any(|t| t.error.as_ref().is_some_and(|e| e.contains("not readable")))
    {
        crate::elevate::reexec();
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
        drag: None,
        land_list_w: None,
        land_top_h: None,
        port_list_h: None,
        port_details_pct: 45,
        hit: Hit::default(),
        last_click: None,
        filter,
        filtering: false,
        has_extras,
        extra_idx: 0,
        extra_scroll: 0,
        extra_run: None,
        extra_gen: 0,
        extra_pct: 50,
        msg_tx: tx,
    };
    let mut terminal = ratatui::init();
    // ratatui's panic hook restores the screen but knows nothing of the mouse.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = event_loop(&mut terminal, &mut app, &rx);
    app.stop_extra(true);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, rx: &Receiver<Msg>) -> Result<()> {
    while !app.quit {
        while let Ok(msg) = rx.try_recv() {
            app.apply(msg);
        }
        app.fetch_extra();
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(150))? {
            match event::read()? {
                TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    app.key(key.code, key.modifiers)
                }
                TermEvent::Mouse(m) => app.mouse(m),
                _ => {}
            }
        }
    }
    Ok(())
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
        let Some((t, c)) = want else { return };
        let refresh = self.refresh;
        let target = &mut self.targets[t];
        if target.error.is_some() {
            return;
        }
        let slot = &mut target.extra[c];
        if slot.running || slot.finished_at.is_some_and(|a| a.elapsed() < refresh) {
            return;
        }
        self.extra_gen += 1;
        let run_id = self.extra_gen;
        slot.run_id = run_id;
        slot.running = true;
        slot.restart = true;
        slot.finished_at = None;
        match spawn_extra(
            &target.cmds[c].command,
            (t, c, run_id),
            (&target.name, &target.host),
            self.msg_tx.clone(),
        ) {
            Ok((pgid, ended)) => self.extra_run = Some(ExtraRun { t, c, pgid, ended }),
            Err(e) => {
                slot.lines = vec![Line::from(format!("cannot run command: {e}"))];
                slot.run_id = 0;
                slot.running = false;
                slot.restart = false;
                slot.finished_at = Some(Instant::now());
            }
        }
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
            Msg::ExtraDone(i, c, run_id, code) => {
                let slot = &mut self.targets[i].extra[c];
                if slot.run_id != run_id {
                    return;
                }
                if std::mem::take(&mut slot.restart) {
                    slot.lines.clear();
                }
                match code {
                    Some(0) => {}
                    Some(n) => slot.lines.push(Line::from(format!("[exit {n}]"))),
                    None => slot.lines.push(Line::from("[killed]")),
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
        if let Some((idx, force)) = self.confirm.take() {
            if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.send(idx, Cmd::Unlock { force });
            }
            return;
        }
        if self.help {
            self.help = false;
            return;
        }
        if self.filtering {
            let n = self.visible().len();
            let pos = self.list.selected().unwrap_or(0);
            match code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filtering = false;
                    self.set_filter();
                }
                KeyCode::Enter => self.filtering = false,
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
            // for them.
            _ if shift && dir.is_some() => self.move_focus(dir.expect("checked")),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('l') if n > 0 => {
                self.logs_full = !self.logs_full;
                self.extra_full = false;
                self.focus = Focus::Activity;
            }
            KeyCode::Char('C') if n > 0 && self.has_extras => {
                self.extra_full = !self.extra_full;
                self.logs_full = false;
                self.focus = Focus::Extra;
            }
            KeyCode::Esc if self.logs_full || self.extra_full => {
                self.logs_full = false;
                self.extra_full = false;
            }
            KeyCode::Char('/') if !self.targets.is_empty() => self.filtering = true,
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter.clear();
                self.set_filter();
            }
            KeyCode::Tab => self.cycle_focus(true),
            KeyCode::BackTab => self.cycle_focus(false),
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Char('c') if self.has_extras && n > 0 => {
                let len = self.targets[sel].cmds.len();
                if len > 0 {
                    self.extra_idx = (self.extra_c(sel) + 1) % len;
                    self.extra_scroll = 0;
                }
            }
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
                let c = self.extra_c(sel);
                if let Some(slot) = self.targets[sel].extra.get_mut(c) {
                    slot.finished_at = None;
                }
            }
            KeyCode::Char('R') => (0..n).for_each(|i| self.send(i, Cmd::Probe)),
            KeyCode::Char('u') | KeyCode::Enter if n > 0 && self.targets[sel].tx.is_some() => {
                self.confirm = Some((sel, false));
            }
            KeyCode::Char('F') if n > 0 && self.targets[sel].tx.is_some() => {
                self.confirm = Some((sel, true))
            }
            KeyCode::Char('s') if n > 0 && self.targets[sel].tx.is_some() => {
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
        let extra = self.has_extras;
        self.focus = match (self.portrait, self.focus, dir) {
            (false, Focus::List, Dir::Right) => Focus::Details,
            (false, Focus::Details | Focus::Activity | Focus::Extra, Dir::Left) => Focus::List,
            (true, Focus::Details, Dir::Up) => Focus::List,
            (true, Focus::List, Dir::Down) => Focus::Details,
            (_, Focus::Details, Dir::Down) => Focus::Activity,
            (_, Focus::Activity, Dir::Up) => Focus::Details,
            (_, Focus::Activity, Dir::Down) if extra => Focus::Extra,
            (_, Focus::Extra, Dir::Up) => Focus::Activity,
            (_, f, _) => f,
        };
    }

    fn cycle_focus(&mut self, forward: bool) {
        if self.logs_full || self.extra_full {
            return;
        }
        let mut order = vec![Focus::List, Focus::Details, Focus::Activity];
        if self.has_extras {
            order.push(Focus::Extra);
        }
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
        match (self.portrait, self.drag) {
            (false, Some(Edge::List)) => {
                let w = x.saturating_sub(body.x) + 1;
                self.land_list_w = Some(w.clamp(15, body.width.saturating_sub(20).max(15)));
            }
            (false, Some(Edge::Details)) => {
                let h = y.saturating_sub(body.y) + 1;
                self.land_top_h = Some(h.clamp(6, body.height.saturating_sub(4).max(6)));
            }
            (true, Some(Edge::List)) => {
                let h = y.saturating_sub(body.y) + 1;
                self.port_list_h = Some(h.clamp(4, body.height.saturating_sub(8).max(4)));
            }
            (true, Some(Edge::Details)) => {
                let h = (u32::from(y) + 1).saturating_sub(u32::from(self.hit.details.y));
                self.port_details_pct = (h * 100 / u32::from(body.height)).clamp(15, 70) as u16;
            }
            (_, Some(Edge::Extra)) => {
                let area = self.hit.split;
                if area.height > 0 {
                    let h = u32::from(area.bottom().saturating_sub(y));
                    self.extra_pct = (h * 100 / u32::from(area.height)).clamp(15, 85) as u16;
                }
            }
            _ => {}
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
        if self.help {
            if matches!(m.kind, MouseEventKind::Down(_)) {
                self.help = false;
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
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.drag = if contains(self.hit.edge_list, x, y) {
                    Some(Edge::List)
                } else if contains(self.hit.edge_details, x, y) {
                    Some(Edge::Details)
                } else if contains(self.hit.edge_extra, x, y) {
                    Some(Edge::Extra)
                } else {
                    None
                };
                if self.drag.is_some() {
                    return;
                }
                if contains(self.hit.list, x, y) {
                    self.focus = Focus::List;
                } else if contains(self.hit.details, x, y) {
                    self.focus = Focus::Details;
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
                if contains(list, x, y) {
                    // Two text rows per item, inside the border.
                    if y > list.y && y + 1 < list.y + list.height {
                        let idx = self.list.offset() + usize::from(y - list.y - 1) / 2;
                        if idx < n {
                            self.select(idx);
                            let double = self
                                .last_click
                                .is_some_and(|(at, i)| i == idx && at.elapsed() < DOUBLE_CLICK);
                            self.last_click = Some((Instant::now(), idx));
                            if double && self.targets[vis[idx]].tx.is_some() {
                                self.last_click = None;
                                self.confirm = Some((vis[idx], false));
                            }
                        }
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
        self.hit.edge_list = Rect::default();
        self.hit.edge_details = Rect::default();
        self.hit.extra = Rect::default();
        self.hit.edge_extra = Rect::default();
        // Cells are about twice as tall as wide: below this the side-by-side
        // layout is cramped, so stack the panes instead.
        self.portrait = area.width < 80 || u32::from(area.width) * 2 < u32::from(area.height) * 3;
        let (left, details, logs) = if self.portrait {
            // Two rows per target, but never more than a third of the screen
            // unless dragged there.
            let auto = (self.targets.len() as u16)
                .saturating_mul(2)
                .saturating_add(2)
                .clamp(4, (body.height / 3).max(4));
            let list_h = self
                .port_list_h
                .map_or(auto, |h| h.clamp(4, body.height.saturating_sub(8).max(4)));
            let [l, d, g] = Layout::vertical([
                Constraint::Length(list_h),
                Constraint::Percentage(self.port_details_pct),
                Constraint::Min(4),
            ])
            .areas(body);
            self.hit.edge_list = Rect::new(body.x, l.bottom().saturating_sub(1), body.width, 2);
            self.hit.edge_details = Rect::new(body.x, d.bottom().saturating_sub(1), body.width, 2);
            (l, d, g)
        } else {
            // The list is as wide as its content needs, next to the details;
            // the activity gets the full width below.
            let content_w = self
                .targets
                .iter()
                .map(|t| {
                    let warn = t.unit.as_ref().is_some_and(|u| u.warning().is_some());
                    let title = 2 + t.name.chars().count() + if warn { 10 } else { 0 };
                    title.max(3 + summary(t).1.chars().count())
                })
                .max()
                .unwrap_or(0) as u16
                + 4; // borders and the highlight symbol
            let list_w = self
                .land_list_w
                .map_or(content_w.clamp(24, (body.width * 2 / 5).max(24)), |w| {
                    w.clamp(15, body.width.saturating_sub(20).max(15))
                });
            let top_h = self
                .land_top_h
                .map_or(14, |h| h.clamp(6, body.height.saturating_sub(4).max(6)))
                .min(body.height.saturating_sub(3))
                .max(3);
            let [left, right] =
                Layout::horizontal([Constraint::Length(list_w), Constraint::Min(10)]).areas(body);
            let [details, logs] =
                Layout::vertical([Constraint::Length(top_h), Constraint::Min(3)]).areas(right);
            self.hit.edge_list = Rect::new(left.right().saturating_sub(1), body.y, 2, body.height);
            self.hit.edge_details =
                Rect::new(right.x, details.bottom().saturating_sub(1), right.width, 2);
            (left, details, logs)
        };
        let (logs, extra) = if !self.has_extras || self.logs_full {
            (logs, None)
        } else {
            let [a, b] = Layout::vertical([
                Constraint::Percentage(100 - self.extra_pct),
                Constraint::Percentage(self.extra_pct),
            ])
            .areas(logs);
            self.hit.split = logs;
            self.hit.edge_extra = Rect::new(a.x, a.bottom().saturating_sub(1), a.width, 2);
            (a, Some(b))
        };
        self.hit.body = body;
        self.hit.details = details;

        let inactive = self
            .targets
            .iter()
            .filter(|t| t.unit.as_ref().is_some_and(|u| u.warning().is_some()))
            .count();
        f.render_widget(
            Line::from(vec![
                " luks-ssh-unlock ".bold().reversed(),
                if self.portrait {
                    format!(
                        " {} · {}",
                        self.targets.len(),
                        chrono::Local::now().format("%H:%M:%S")
                    )
                } else {
                    format!(
                        "  {} target(s) · refresh every {}s · {}",
                        self.targets.len(),
                        self.refresh.as_secs(),
                        chrono::Local::now().format("%H:%M:%S")
                    )
                }
                .dim(),
                if inactive > 0 {
                    format!(
                        "  ⚠ {inactive} {}",
                        if self.portrait {
                            "svc down"
                        } else {
                            "service(s) not running"
                        }
                    )
                    .yellow()
                    .bold()
                } else {
                    Span::raw("")
                },
            ]),
            header,
        );

        if self.targets.is_empty() {
            f.render_widget(
                Paragraph::new(format!(
                    "No targets. Pass environment files, or configure services.luks-ssh-unlock instances ({DEFAULT_CONFIG_DIR}/*.env)."
                ))
                .wrap(Wrap { trim: true })
                .block(Block::bordered()),
                body,
            );
        } else {
            let sel = self.selected();
            let any = !self.visible().is_empty();
            if self.logs_full && any {
                self.hit.list = Rect::default();
                self.hit.details = Rect::default();
                self.draw_logs(f, body, sel);
            } else if self.extra_full && any && self.has_extras {
                self.hit.list = Rect::default();
                self.hit.details = Rect::default();
                self.hit.logs = Rect::default();
                self.draw_extra(f, body, sel);
            } else {
                self.draw_list(f, left);
                if any {
                    self.draw_details(f, details, sel);
                    self.draw_logs(f, logs, sel);
                    if let Some(extra) = extra {
                        self.draw_extra(f, extra, sel);
                    }
                } else {
                    self.hit.logs = Rect::default();
                    f.render_widget(
                        Paragraph::new(format!("No target matches “{}”.", self.filter))
                            .block(Block::bordered()),
                        details,
                    );
                }
            }
        }

        let hints = if self.extra_full && self.confirm.is_none() {
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
                        " filter: type to match name or host · ↑↓ select · ⏎ keep filter · Esc clear",
                        " type to filter · ⏎ keep · Esc clear",
                        " ⏎ keep · Esc clear",
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
                        " ↑↓/jk/click select · ⇧←↑↓→/Tab focus pane · drag borders resize · / filter · r/R refresh · u/⏎/double-click unlock · F force · l full-screen activity · C full-screen console · c next command · ? help · q quit",
                        " ↑↓ select · / filter · ⇧arrows/Tab focus · r/R refresh · u unlock · F force · s service · l log · C console · ? help · q quit",
                        " ↑↓ select · r refresh · u unlock · q quit",
                        " ↑↓ · r · u · q",
                    ],
                )
                .dim(),
            )
        };
        f.render_widget(hints, footer);

        if let Some((idx, force)) = self.confirm {
            self.draw_confirm(f, idx, force);
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
            ("/", "filter targets by name or host (⏎ keeps, Esc clears)"),
            ("r  R", "check the selected target / all targets now"),
            ("u  ⏎", "unlock the selected target (asks first)"),
            ("F", "force unlock (skips integrity and host-key checks)"),
            ("s", "stop the target's service, or start it when stopped"),
            ("l", "activity full screen (l or Esc goes back)"),
            ("C", "console (extra command) full screen"),
            ("c", "next console command"),
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

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let vis = self.visible();
        if self.list.selected().is_some_and(|p| p >= vis.len()) {
            self.list.select(Some(vis.len().saturating_sub(1)));
        }
        let items: Vec<ListItem> = vis
            .iter()
            .map(|&i| &self.targets[i])
            .map(|t| {
                let (icon, label, color) = summary(t);
                let mut title = vec![
                    Span::styled(format!("{icon} "), Style::new().fg(color)),
                    Span::styled(t.name.clone(), Style::new().bold()),
                ];
                if t.unit.as_ref().is_some_and(|u| u.warning().is_some()) {
                    title.push(Span::styled(" ⚠ service", Style::new().yellow().bold()));
                }
                ListItem::new(vec![
                    Line::from(title),
                    Line::from(Span::styled(format!("   {label}"), Style::new().fg(color))),
                ])
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::bordered()
                    .title(if self.filtering {
                        format!(" Targets · /{}▏ ", self.filter)
                    } else if self.filter.is_empty() {
                        " Targets ".to_string()
                    } else {
                        format!(" Targets · /{} ", self.filter)
                    })
                    .border_style(self.border(Focus::List)),
            )
            .highlight_style(
                Style::new()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▌");
        self.hit.list = area;
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_details(&self, f: &mut Frame, area: Rect, idx: usize) {
        let t = &self.targets[idx];
        let mut lines: Vec<Line> = Vec::new();
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
        let title = match t.busy {
            Some(b) => format!(" {} · {b}… ", t.name),
            None => format!(" {} ", t.name),
        };
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(title)
                    .border_style(self.border(Focus::Details)),
            ),
            area,
        );
    }

    fn draw_logs(&mut self, f: &mut Frame, area: Rect, idx: usize) {
        self.hit.logs = area;
        let t = &self.targets[idx];
        let height = area.height.saturating_sub(2) as usize;
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
                Block::bordered()
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
                    Block::bordered()
                        .title(format!(" {} ", t.name))
                        .border_style(self.border(Focus::Extra)),
                ),
                area,
            );
            return;
        }
        let c = self.extra_c(idx);
        let (cmd, slot) = (&t.cmds[c], &t.extra[c]);
        let height = area.height.saturating_sub(2) as usize;
        let total = slot.lines.len();
        self.extra_scroll = self.extra_scroll.min(total.saturating_sub(height));
        let end = total - self.extra_scroll;
        let window: Vec<Line> = slot.lines[end.saturating_sub(height)..end].to_vec();
        let mut title = format!(" {} · {}", cmd.label, t.name);
        if t.cmds.len() > 1 {
            title.push_str(&format!(" · c next {}/{}", c + 1, t.cmds.len()));
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
        f.render_widget(
            Paragraph::new(window).wrap(Wrap { trim: false }).block(
                Block::bordered()
                    .title(title)
                    .border_style(self.border(Focus::Extra)),
            ),
            area,
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
            drag: None,
            land_list_w: None,
            land_top_h: None,
            port_list_h: None,
            port_details_pct: 45,
            hit: Hit::default(),
            last_click: None,
            filter: String::new(),
            filtering: false,
            has_extras: false,
            extra_idx: 0,
            extra_scroll: 0,
            extra_run: None,
            extra_gen: 0,
            extra_pct: 50,
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
    fn wide_terminal_is_side_by_side() {
        let mut a = app(2);
        render(&mut a, 120, 30);
        assert!(!a.portrait);
        // The list on the left over the full height; details above the
        // activity on the right.
        assert!(a.hit.details.x > a.hit.list.x);
        assert!(a.hit.logs.y > a.hit.details.y);
        assert_eq!(a.hit.logs.x, a.hit.details.x);
        assert_eq!(a.hit.logs.right(), 120);
        assert!(a.hit.list.height > a.hit.details.height);
        // The list is as wide as its content, not a third of the screen.
        assert!(a.hit.list.width < 40, "{}", a.hit.list.width);
    }

    #[test]
    fn phone_is_stacked() {
        let mut a = app(2);
        let out = render(&mut a, 45, 40);
        assert!(a.portrait);
        assert_eq!(a.hit.list.x, a.hit.logs.x);
        assert!(a.hit.logs.y > a.hit.list.y);
        assert!(out.contains("host0"));
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
    fn shift_arrows_move_focus_by_layout() {
        let mut a = app(2);
        render(&mut a, 120, 30);
        a.key(KeyCode::Right, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::Details);
        a.key(KeyCode::Down, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::Activity);
        a.key(KeyCode::Left, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::List);
        a.key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(a.focus == Focus::Details);
        a.key(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::List);

        let mut p = app(2);
        render(&mut p, 45, 40);
        p.key(KeyCode::Down, KeyModifiers::SHIFT);
        assert!(p.focus == Focus::Details);
        p.key(KeyCode::Down, KeyModifiers::SHIFT);
        assert!(p.focus == Focus::Activity);
        p.key(KeyCode::Up, KeyModifiers::SHIFT);
        assert!(p.focus == Focus::Details);
    }

    #[test]
    fn plain_arrows_act_in_the_focused_pane() {
        let mut a = app(3);
        render(&mut a, 120, 30);
        a.key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(a.selected(), 1);
        a.key(KeyCode::Right, KeyModifiers::NONE);
        assert!(a.focus == Focus::Details);
        a.key(KeyCode::Down, KeyModifiers::NONE);
        assert!(a.focus == Focus::Activity);
        a.key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.log_scroll, 1);
        assert_eq!(a.selected(), 1);
    }

    #[test]
    fn dragging_borders_resizes_landscape() {
        let mut a = app(2);
        render(&mut a, 120, 30);
        let before = a.hit.list.width;
        let edge = a.hit.edge_list;
        drag(&mut a, (edge.x, 10), (60, 10));
        render(&mut a, 120, 30);
        assert!(a.hit.list.width > before, "{} > {before}", a.hit.list.width);

        let edge = a.hit.edge_details;
        drag(&mut a, (edge.x + 5, edge.y), (edge.x + 5, 20));
        render(&mut a, 120, 30);
        assert_eq!(a.hit.details.height, 20);
    }

    #[test]
    fn dragging_borders_resizes_portrait() {
        let mut a = app(2);
        render(&mut a, 45, 40);
        let edge = a.hit.edge_list;
        drag(&mut a, (10, edge.y), (10, 15));
        render(&mut a, 45, 40);
        // The border follows the pointer: rows 1 (below the header) to 15.
        assert_eq!(a.hit.list.height, 15);
        let edge = a.hit.edge_details;
        drag(&mut a, (10, edge.y), (10, 30));
        render(&mut a, 45, 40);
        assert!(a.hit.details.height > 14);
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
    fn slash_filters_targets() {
        let mut a = app(3);
        render(&mut a, 120, 30);
        a.key(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(a.filtering);
        a.key(KeyCode::Char('2'), KeyModifiers::NONE);
        assert_eq!(a.visible(), vec![2]);
        assert_eq!(a.selected(), 2);
        let out = render(&mut a, 120, 30);
        assert!(out.contains("host2") && !out.contains("host1"));
        a.key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!a.filtering && a.filter == "2");
        // Esc clears the filter before it quits.
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(a.filter.is_empty() && !a.quit);
        assert_eq!(a.visible().len(), 3);
        // No match renders without panicking.
        a.key(KeyCode::Char('/'), KeyModifiers::NONE);
        a.key(KeyCode::Char('z'), KeyModifiers::NONE);
        let out = render(&mut a, 120, 30);
        assert!(out.contains("No target matches"));
        a.key(KeyCode::Char('u'), KeyModifiers::NONE);
        a.key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(a.visible().len(), 3);
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
        assert!(a.hit.extra.y >= a.hit.logs.bottom());
        assert_eq!(a.hit.extra.x, a.hit.logs.x);
        a.focus = Focus::Activity;
        a.key(KeyCode::Down, KeyModifiers::SHIFT);
        assert!(a.focus == Focus::Extra);
        a.key(KeyCode::Tab, KeyModifiers::NONE);
        assert!(a.focus == Focus::List);
        // Dragging the border between activity and extra pane resizes it.
        let before = a.hit.extra.height;
        let edge = a.hit.edge_extra;
        drag(&mut a, (edge.x + 3, edge.y), (edge.x + 3, edge.y - 5));
        render(&mut a, 120, 30);
        assert!(
            a.hit.extra.height > before,
            "{} > {before}",
            a.hit.extra.height
        );
        render(&mut a, 45, 40);
        assert!(a.hit.extra.y > a.hit.logs.y);
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

        // The pane tells when the selected target has no command.
        let mut a = app(2);
        set_extras(&mut a, common);
        a.targets[1].cmds.clear();
        a.targets[1].extra.clear();
        a.select(1);
        a.fetch_extra();
        assert!(a.extra_run.is_none());
        assert!(render(&mut a, 120, 30).contains("no command configured"));
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
        // Unknown state: a note, nothing sent.
        a.key(KeyCode::Char('s'), KeyModifiers::NONE);
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
        a.key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Service(false))));
        a.targets[0].unit = Some(unit("failed"));
        a.key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(matches!(rx.try_recv(), Ok(Cmd::Service(true))));
        a.targets[0].unit.as_mut().unwrap().load = "not-found".into();
        a.key(KeyCode::Char('s'), KeyModifiers::NONE);
        assert!(rx.try_recv().is_err());
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
    fn footer_hint_fits() {
        assert_eq!(pick(5, &["long text", "short"]), "short");
        assert_eq!(pick(3, &["long text", "short"]), "short");
        assert_eq!(pick(20, &["long text", "short"]), "long text");
    }
}
