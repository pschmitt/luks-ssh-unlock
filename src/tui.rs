//! Interactive dashboard: every configured target with its live state,
//! details, activity (the service's journal plus its own actions), and one-key (confirmed) unlocks.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
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
/// Minimum time between two checks triggered by new journal lines.
const EVENT_PROBE_GAP: Duration = Duration::from_secs(2);
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
const WHEEL_LINES: usize = 3;
pub const DEFAULT_CONFIG_DIR: &str = "/etc/luks-ssh-unlock";

/// One target to show: a name and the environment describing it.
pub struct Source {
    pub name: String,
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
    Unlock { force: bool },
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
    /// Screen areas of the last frame, for mouse hit-testing.
    hit: Hit,
    last_click: Option<(Instant, usize)>,
}

#[derive(Default)]
struct Hit {
    list: Rect,
    logs: Rect,
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

pub fn run(sources: Vec<Source>, opts: &GlobalOpts, refresh: Duration) -> Result<()> {
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
    drop(tx);

    if targets
        .iter()
        .any(|t| t.error.as_ref().is_some_and(|e| e.contains("not readable")))
    {
        crate::elevate::reexec("some targets need root to read their keys or passphrases");
    }

    let mut app = App {
        targets,
        list: ListState::default().with_selected(Some(0)),
        confirm: None,
        refresh,
        quit: false,
        log_scroll: 0,
        portrait: false,
        hit: Hit::default(),
        last_click: None,
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
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, rx: &Receiver<Msg>) -> Result<()> {
    while !app.quit {
        while let Ok(msg) = rx.try_recv() {
            app.apply(msg);
        }
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
    fn selected(&self) -> usize {
        self.list
            .selected()
            .unwrap_or(0)
            .min(self.targets.len().saturating_sub(1))
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
        let n = self.targets.len();
        let sel = self.selected();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.select((sel + 1) % n),
            KeyCode::Up | KeyCode::Char('k') if n > 0 => self.select((sel + n - 1) % n),
            KeyCode::PageUp => self.scroll_logs(true, self.hit.logs.height as usize),
            KeyCode::PageDown => self.scroll_logs(false, self.hit.logs.height as usize),
            KeyCode::Char('r') => self.send(sel, Cmd::Probe),
            KeyCode::Char('R') => (0..n).for_each(|i| self.send(i, Cmd::Probe)),
            KeyCode::Char('u') | KeyCode::Enter if n > 0 && self.targets[sel].tx.is_some() => {
                self.confirm = Some((sel, false));
            }
            KeyCode::Char('F') if n > 0 && self.targets[sel].tx.is_some() => {
                self.confirm = Some((sel, true))
            }
            _ => {}
        }
    }

    fn select(&mut self, i: usize) {
        if i != self.selected() || self.list.selected().is_none() {
            self.log_scroll = 0;
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
        let n = self.targets.len();
        if n == 0 {
            return;
        }
        match m.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if contains(self.hit.logs, x, y) {
                    self.scroll_logs(up, WHEEL_LINES);
                } else if contains(self.hit.list, x, y) {
                    let sel = self.selected();
                    let next = if up {
                        sel.saturating_sub(1)
                    } else {
                        (sel + 1).min(n - 1)
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
                            if double && self.targets[idx].tx.is_some() {
                                self.last_click = None;
                                self.confirm = Some((idx, false));
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
        // Cells are about twice as tall as wide: below this the side-by-side
        // layout is cramped, so stack the panes instead.
        self.portrait = area.width < 80 || u32::from(area.width) * 2 < u32::from(area.height) * 3;
        let (left, details, logs) = if self.portrait {
            // Two rows per target, but never more than a third of the screen.
            let list_h = (self.targets.len() as u16)
                .saturating_mul(2)
                .saturating_add(2)
                .clamp(4, (body.height / 3).max(4));
            let [l, d, g] = Layout::vertical([
                Constraint::Length(list_h),
                Constraint::Percentage(45),
                Constraint::Min(4),
            ])
            .areas(body);
            (l, d, g)
        } else {
            let [left, right] =
                Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
                    .areas(body);
            let [details, logs] =
                Layout::vertical([Constraint::Length(16), Constraint::Min(3)]).areas(right);
            (left, details, logs)
        };

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
            self.draw_list(f, left);
            let sel = self.selected();
            self.draw_details(f, details, sel);
            self.draw_logs(f, logs, sel);
        }

        let hints = if self.confirm.is_some() {
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
                        " ↑↓/jk/click select · r refresh · R refresh all · u/⏎/double-click unlock · F force · wheel/PgUp/PgDn scroll activity · q quit",
                        " ↑↓ select · r/R refresh · u unlock · F force · PgUp/PgDn scroll · q quit",
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
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .targets
            .iter()
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
            .block(Block::bordered().title(" Targets "))
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
            lines.push(Line::from(Span::styled(
                format!("{icon} {label}"),
                Style::new().fg(color).bold(),
            )));
            let detail = match &r.state {
                TargetState::Unlocked(d) | TargetState::Initrd(d) | TargetState::SshFailed(d) => {
                    d.clone()
                }
                TargetState::NotInitrd => "SSH answers, but this is not the initrd".into(),
                TargetState::Unreachable => "nothing answers on SSH".into(),
            };
            lines.push(Line::from(Span::raw(detail)));
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
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(title)),
            area,
        );
    }

    fn draw_logs(&mut self, f: &mut Frame, area: Rect, idx: usize) {
        self.hit.logs = area;
        let t = &self.targets[idx];
        let height = area.height.saturating_sub(2) as usize;
        let title = " Activity · service journal and this dashboard ";
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
                    Span::styled(
                        format!(
                            "{} {} ",
                            l.at.format("%H:%M:%S"),
                            if l.service { "svc" } else { "tui" }
                        ),
                        Style::new().dim(),
                    ),
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
            Paragraph::new(window)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(title)),
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
                })
                .collect(),
            list: ListState::default().with_selected(Some(0)),
            confirm: None,
            refresh: Duration::from_secs(30),
            quit: false,
            log_scroll: 0,
            portrait: false,
            hit: Hit::default(),
            last_click: None,
        }
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
        assert!(a.hit.logs.x > a.hit.list.x);
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
        }
    }

    #[test]
    fn footer_hint_fits() {
        assert_eq!(pick(5, &["long text", "short"]), "short");
        assert_eq!(pick(3, &["long text", "short"]), "short");
        assert_eq!(pick(20, &["long text", "short"]), "long text");
    }
}
