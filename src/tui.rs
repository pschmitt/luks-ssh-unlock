//! Interactive dashboard: every configured target with its live state,
//! details, logs, and one-key (confirmed) unlocks.

use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
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

const LOG_LINES: usize = 500;
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
        let mut found: Vec<PathBuf> = fs::read_dir(DEFAULT_CONFIG_DIR)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|e| e == "env"))
                    .collect()
            })
            .unwrap_or_default();
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
    Journal,
}

enum Msg {
    Report(usize, Box<Report>),
    Log(usize, LogLine),
    Busy(usize, Option<&'static str>),
    Done(usize, Option<State>),
    Journal(usize, Vec<String>),
}

struct Target {
    name: String,
    host: String,
    error: Option<String>,
    report: Option<Report>,
    busy: Option<&'static str>,
    last_unlock: Option<(Instant, Option<State>)>,
    logs: VecDeque<LogLine>,
    journal: Option<Vec<String>>,
    tx: Option<Sender<Cmd>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Logs,
    Journal,
}

struct App {
    targets: Vec<Target>,
    list: ListState,
    confirm: Option<(usize, bool)>,
    pane: Pane,
    refresh: Duration,
    quit: bool,
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
    loop {
        let cmd = match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(cmd) => cmd,
            Err(RecvTimeoutError::Timeout) => Cmd::Probe,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        match cmd {
            Cmd::Probe => {
                let _ = tx.send(Msg::Busy(idx, Some("checking")));
                ctx.mode = Mode::Status;
                match ctx.status(false) {
                    Ok(report) => {
                        let _ = tx.send(Msg::Report(idx, Box::new(report)));
                    }
                    Err(e) => ctx.log.error(format!("Status check failed: {e:#}")),
                }
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
            Cmd::Journal => {
                let lines = Command::new("journalctl")
                    .args(["--no-pager", "--output=short-iso", "-n", "200", "-u", &unit])
                    .output()
                    .map(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .lines()
                            .map(str::to_string)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|e| vec![format!("journalctl failed: {e}")]);
                let _ = tx.send(Msg::Journal(idx, lines));
            }
        }
        let _ = tx.send(Msg::Busy(idx, None));
    }
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
            journal: None,
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
                thread::spawn(move || worker(idx, ctx, unit, ctx_rx, tx, refresh));
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

    let mut app = App {
        targets,
        list: ListState::default().with_selected(Some(0)),
        confirm: None,
        pane: Pane::Logs,
        refresh,
        quit: false,
    };
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &mut app, &rx);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App, rx: &Receiver<Msg>) -> Result<()> {
    while !app.quit {
        while let Ok(msg) = rx.try_recv() {
            app.apply(msg);
        }
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(150))?
            && let TermEvent::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.key(key.code, key.modifiers);
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
            Msg::Log(i, line) => {
                let logs = &mut self.targets[i].logs;
                logs.push_back(line);
                while logs.len() > LOG_LINES {
                    logs.pop_front();
                }
            }
            Msg::Busy(i, b) => self.targets[i].busy = b,
            Msg::Done(i, state) => self.targets[i].last_unlock = Some((Instant::now(), state)),
            Msg::Journal(i, lines) => self.targets[i].journal = Some(lines),
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
            KeyCode::Down | KeyCode::Char('j') if n > 0 => self.list.select(Some((sel + 1) % n)),
            KeyCode::Up | KeyCode::Char('k') if n > 0 => self.list.select(Some((sel + n - 1) % n)),
            KeyCode::Char('r') => self.send(sel, Cmd::Probe),
            KeyCode::Char('R') => (0..n).for_each(|i| self.send(i, Cmd::Probe)),
            KeyCode::Char('u') | KeyCode::Enter if n > 0 && self.targets[sel].tx.is_some() => {
                self.confirm = Some((sel, false));
            }
            KeyCode::Char('F') if n > 0 && self.targets[sel].tx.is_some() => {
                self.confirm = Some((sel, true))
            }
            KeyCode::Char('l') | KeyCode::Tab => {
                self.pane = match self.pane {
                    Pane::Logs => {
                        self.send(sel, Cmd::Journal);
                        Pane::Journal
                    }
                    Pane::Journal => Pane::Logs,
                };
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
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
                .areas(body);
        let [details, logs] =
            Layout::vertical([Constraint::Length(14), Constraint::Min(3)]).areas(right);

        f.render_widget(
            Line::from(vec![
                " luks-ssh-unlock ".bold().reversed(),
                format!(
                    "  {} target(s) · refresh every {}s · {}",
                    self.targets.len(),
                    self.refresh.as_secs(),
                    chrono::Local::now().format("%H:%M:%S")
                )
                .dim(),
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
            Line::from(" y confirm · any other key cancels".yellow())
        } else {
            Line::from(
                " ↑↓/jk select · r refresh · R refresh all · u/⏎ unlock · F force unlock · l logs/journal · q quit"
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
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(format!("{icon} "), Style::new().fg(color)),
                        Span::styled(t.name.clone(), Style::new().bold()),
                    ]),
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
        f.render_stateful_widget(list, area, &mut self.list);
    }

    fn draw_details(&self, f: &mut Frame, area: Rect, idx: usize) {
        let t = &self.targets[idx];
        let mut lines: Vec<Line> = Vec::new();
        let row = |k: &str, v: String, style: Style| {
            Line::from(vec![
                Span::styled(format!("{k:<18}"), Style::new().dim()),
                Span::styled(v, style),
            ])
        };
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

    fn draw_logs(&self, f: &mut Frame, area: Rect, idx: usize) {
        let t = &self.targets[idx];
        let height = area.height.saturating_sub(2) as usize;
        let (title, lines): (&str, Vec<Line>) = match self.pane {
            Pane::Logs => (
                " Activity (this session) ",
                t.logs
                    .iter()
                    .rev()
                    .take(height)
                    .rev()
                    .map(|l| {
                        let style = match l.level {
                            Level::Debug => Style::new().dim(),
                            Level::Info => Style::new(),
                            Level::Warning => Style::new().yellow(),
                            Level::Error => Style::new().red(),
                        };
                        Line::from(vec![
                            Span::styled(
                                format!("{} ", l.at.format("%H:%M:%S")),
                                Style::new().dim(),
                            ),
                            Span::styled(l.message.clone(), style),
                        ])
                    })
                    .collect(),
            ),
            Pane::Journal => (
                " Service journal ",
                match &t.journal {
                    Some(j) => j
                        .iter()
                        .rev()
                        .take(height)
                        .rev()
                        .map(|l| Line::from(l.clone()))
                        .collect(),
                    None => vec![Line::from("loading…".dim())],
                },
            ),
        };
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(title)),
            area,
        );
    }

    fn draw_confirm(&self, f: &mut Frame, idx: usize, force: bool) {
        let t = &self.targets[idx];
        let area = centered(f.area(), 64, if force { 9 } else { 7 });
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
    }
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
