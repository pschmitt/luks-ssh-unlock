//! The dashboard's side of the built-in VNC viewer: starting and stopping the
//! session with the selection, turning its events into pixels, drawing the
//! image in the console pane, and forwarding the keyboard and the mouse.
//!
//! The network side lives in [`super::vnc`] and runs on its own thread; the
//! picture is drawn by [`crate::vnc_graphics`] with the terminal's graphics
//! protocol, after the text of each frame.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::vnc::{Event, Session, graphics_context_problem};
use super::{
    App, Focus, VNC_BYTES, VNC_EVENTS, VNC_RESIZE_DELAY, VNC_RESIZE_MIN, VNC_TMUX_PROBE, VncRun,
    VncState, contains,
};
use crate::vnc::{Input, Pointer};
use crate::vnc_graphics::renderer::Rect as ImageRect;
use crate::vnc_graphics::{self, PaneRect, Renderer};

/// Shown when the terminal cannot display the picture and nothing more
/// specific is known (outside tmux the reason is always known).
const NO_GRAPHICS: &str = "This terminal cannot show the remote desktop: it needs the Kitty graphics protocol (kitty, WezTerm, Ghostty or Konsole). Inside tmux, also enable `allow-passthrough`.";

/// Shown when tmux did not answer in time: it is not installed, or the
/// session is not reachable from here.
const NO_TMUX_ANSWER: &str = "Could not ask tmux about the terminal (is `tmux` on PATH, and is the session reachable?). Press Ctrl+R to try again, or set LUKS_SSH_UNLOCK_GRAPHICS=kitty if the terminal behind tmux shows images.";

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn image_rect(r: crate::vnc::Rect) -> ImageRect {
    ImageRect::new(r.x, r.y, r.width, r.height)
}

impl App {
    /// The selected target's VNC pane (`.0` target, `.1` command), when the
    /// command that is shown is a VNC endpoint.
    #[cfg(test)]
    pub(super) fn vnc_cmd(&self) -> Option<(usize, usize)> {
        let (t, c) = self.console_cmd()?;
        self.targets[t].cmds[c].vnc.is_some().then_some((t, c))
    }

    /// Does the keyboard stay with the dashboard for this endpoint? The
    /// keyboard toggle (`Ctrl+T`) wins over the configured default.
    pub(super) fn vnc_view_only(&self, t: usize, c: usize) -> bool {
        self.targets[t].extra[c].vnc_view_only.unwrap_or_else(|| {
            self.targets[t].cmds[c]
                .vnc
                .as_ref()
                .is_some_and(|v| v.view_only)
        })
    }

    /// The session of this pane, once its first picture is in.
    fn vnc_ready(&self, t: usize, c: usize) -> Option<&VncRun> {
        self.vnc
            .run
            .as_ref()
            .filter(|r| (r.t, r.c) == (t, c) && r.framed)
    }

    /// How long the event loop may wait for a key.
    pub(super) fn poll_ms(&self) -> u64 {
        if self.vnc.more {
            5
        } else if self.vnc.run.is_some() {
            20
        } else if self.console_live() {
            15
        } else {
            150
        }
    }

    /// Let go of the session: it stops by itself, without the dashboard
    /// waiting for it, and the picture is taken off the screen.
    pub(super) fn stop_vnc(&mut self) {
        if let Some(run) = self.vnc.run.take() {
            let slot = &mut self.targets[run.t].extra[run.c];
            slot.run_id = 0;
            slot.running = false;
            slot.finished_at = None;
            // `run` drops here, which stops the session.
        }
        self.vnc.retire();
    }

    /// Start the viewer again (`Ctrl+R`, `F5`, `r`): terminal graphics are
    /// looked for once more, and the connection is made anew.
    pub(super) fn restart_vnc(&mut self) {
        self.stop_vnc();
        self.force_reload = true;
        self.vnc.checked = false;
        self.vnc.probing = None;
        // The user may have just set `allow-passthrough` or moved the tmux
        // session to another terminal: ask tmux again.
        vnc_graphics::transport::recheck();
        if self.has_extras && !self.visible().is_empty() {
            let t = self.selected();
            let c = self.extra_c(t);
            if self.targets[t].cmds.get(c).is_some_and(|x| x.vnc.is_some()) {
                let slot = &mut self.targets[t].extra[c];
                slot.finished_at = None;
                slot.running = false;
                slot.vnc_note.clear();
            }
        }
    }

    /// Start the selected target's VNC session, unless one is running or
    /// the last one ended (that stays as it is until a restart).
    pub(super) fn fetch_vnc(&mut self, t: usize, c: usize) {
        {
            let slot = &self.targets[t].extra[c];
            if slot.running || slot.finished_at.is_some() {
                return;
            }
        }
        if !self.vnc.checked {
            self.vnc.transport = None;
            self.vnc.problem = graphics_context_problem();
            if self.vnc.problem.is_none() {
                // Right after a restart the old answer may predate the user's
                // change of `allow-passthrough`: wait for the new one.
                let in_tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
                let since = *self.vnc.probing.get_or_insert_with(Instant::now);
                if in_tmux
                    && !running_as_root()
                    && vnc_graphics::transport::recheck_pending()
                    && since.elapsed() < VNC_TMUX_PROBE
                {
                    return;
                }
                self.vnc.transport = vnc_graphics::detect();
                if self.vnc.transport.is_none() {
                    // A known reason is shown at once. Inside tmux the answer
                    // comes from a background thread, a moment after the
                    // question (never as root: it does not ask there).
                    let reason = vnc_graphics::unsupported_reason();
                    let asking = in_tmux && !running_as_root();
                    if reason.is_none() && asking && since.elapsed() < VNC_TMUX_PROBE {
                        return;
                    }
                    self.vnc.problem = Some(reason.unwrap_or_else(|| {
                        if asking { NO_TMUX_ANSWER } else { NO_GRAPHICS }.to_string()
                    }));
                }
            }
            self.vnc.checked = true;
        }
        if let Some(problem) = self.vnc.problem.clone() {
            // Do not connect to a desktop that cannot be shown.
            let slot = &mut self.targets[t].extra[c];
            slot.finished_at = Some(Instant::now());
            slot.vnc_note = problem;
            return;
        }
        let (Some(transport), Some(mut endpoint)) =
            (self.vnc.transport, self.targets[t].cmds[c].vnc.clone())
        else {
            return;
        };
        // A restart (Ctrl+R) makes room whatever the configuration says.
        if std::mem::take(&mut self.force_reload)
            && let crate::console::VncSource::Oci(oci) = &mut endpoint.source
        {
            oci.keep_others = false;
        }
        self.extra_gen += 1;
        let slot = &mut self.targets[t].extra[c];
        slot.run_id = self.extra_gen;
        slot.running = true;
        slot.restart = false;
        slot.finished_at = None;
        slot.vnc_note = endpoint.first_step();
        self.vnc.renderer = Some(Renderer::new(transport));
        let keymap = endpoint
            .keymap
            .unwrap_or_else(crate::vnc::keymap::Layout::detect);
        self.vnc.run = Some(VncRun {
            t,
            c,
            keymap,
            scancodes: false,
            started: Instant::now(),
            session: Session::start(endpoint),
            framed: false,
            layout: None,
            buttons: 0,
            pointer: (0, 0),
            resize: None,
            resized: None,
        });
    }

    /// Take what the session has produced since the last turn of the loop.
    pub(super) fn pump_vnc(&mut self) {
        let VncState { run, renderer, .. } = &mut self.vnc;
        let Some(run) = run.as_mut() else { return };
        let (t, c) = (run.t, run.c);
        let mut ended = None;
        let mut note = None;
        let mut dropped = false;
        for event in run.session.drain(VNC_EVENTS, VNC_BYTES) {
            match event {
                Event::Resolution(w, h) => {
                    if let Some(r) = renderer.as_mut() {
                        r.resize_framebuffer(u32::from(w), u32::from(h));
                    }
                }
                // The pane counts as showing a picture only once pixels have
                // been taken into the framebuffer: an update that arrives
                // before the desktop's size, or lies outside it, is dropped
                // and must not make an empty pane look "live".
                Event::Bgra(rect, data) => {
                    if let Some(r) = renderer.as_mut() {
                        let taken = r.apply_bgra(image_rect(rect), &data).is_some();
                        run.framed |= taken;
                        dropped |= !taken;
                    }
                }
                Event::Rgb(rect, data) => {
                    if let Some(r) = renderer.as_mut() {
                        let taken = r.apply_rgb(image_rect(rect), &data).is_some();
                        run.framed |= taken;
                        dropped |= !taken;
                    }
                }
                Event::Copy { dst, from } => {
                    if let Some(r) = renderer.as_mut() {
                        r.apply_copy(image_rect(dst), from);
                    }
                }
                Event::Layout(layout) => run.layout = Some(layout),
                Event::Scancodes => {
                    if std::env::var_os("LSU_VNC_TRACE").is_some() {
                        eprintln!("[LSU_VNC_TRACE] the server takes scancodes");
                    }
                    run.scancodes = true;
                }
                Event::Status(s) => note = Some(s),
                Event::Ended(reason) => {
                    ended = Some(reason);
                    break;
                }
            }
        }
        // The remote desktop is asked to follow the pane once the pane has
        // held still for a moment (only with `resizeRemote`).
        if ended.is_none()
            && let Some((want, since)) = run.resize
            && since.elapsed() >= VNC_RESIZE_DELAY
            && let Some(layout) = &run.layout
            && let [screen] = layout.screens.as_slice()
        {
            let mut screen = *screen;
            screen.width = want.0;
            screen.height = want.1;
            if run.session.send(Input::Resize {
                width: want.0,
                height: want.1,
                screens: vec![screen],
            }) {
                run.resized = Some(want);
                run.resize = None;
            }
        }
        let slot = &mut self.targets[t].extra[c];
        if note.is_none() && dropped && !run.framed {
            note = Some(
                "the desktop sent pixels that do not fit its reported size; waiting for a usable picture"
                    .to_string(),
            );
        }
        if let Some(note) = note {
            slot.vnc_note = note;
        }
        if let Some(reason) = ended {
            slot.running = false;
            slot.run_id = 0;
            slot.finished_at = Some(Instant::now());
            slot.vnc_note = reason.unwrap_or_else(|| "the connection was closed".to_string());
            self.vnc.run = None;
            self.vnc.retire();
        }
    }

    /// Remember the size the remote desktop should take, when the pane's
    /// size changes and `resizeRemote` is set.
    fn want_remote_size(&mut self, pane: PaneRect) {
        let Some((t, c)) = self.vnc.run.as_ref().map(|r| (r.t, r.c)) else {
            return;
        };
        let enabled = self.targets[t].cmds[c]
            .vnc
            .as_ref()
            .is_some_and(|v| v.resize_remote)
            && !self.vnc_view_only(t, c);
        if !enabled {
            return;
        }
        let Some(want) = self
            .vnc
            .renderer
            .as_ref()
            .and_then(|r| r.pane_pixels(pane))
            .filter(|(w, h)| *w >= VNC_RESIZE_MIN && *h >= VNC_RESIZE_MIN)
        else {
            return;
        };
        let Some(run) = self.vnc.run.as_mut() else {
            return;
        };
        if run.resized == Some(want) {
            run.resize = None;
        } else if run.resize.is_none_or(|(w, _)| w != want) {
            run.resize = Some((want, Instant::now()));
        }
    }

    /// Use direct Kitty placements for every tmux pane. Virtual placeholders
    /// rely on a separate tmux redraw and can lag behind live framebuffer swaps.
    fn paint_placeholders(&mut self, buf: &mut Buffer, pane: PaneRect) -> bool {
        let direct = self.extra_full || vnc_graphics::transport::in_tmux();
        let Some(renderer) = self.vnc.renderer.as_mut() else {
            return false;
        };
        renderer.set_tmux_direct(direct);
        let Some(placeholders) = renderer.placeholder_cells(pane) else {
            return false;
        };
        for cell in placeholders.cells {
            if let Some(target) = buf.cell_mut((cell.col, cell.row)) {
                let (r, g, b) = cell.foreground;
                target.set_symbol(&cell.glyph);
                target.set_fg(Color::Rgb(r, g, b));
            }
        }
        true
    }

    /// A VNC pane: the picture once there is one (drawn after the frame; the
    /// pane's inside stays empty for it), otherwise what is going on.
    pub(super) fn draw_vnc_pane(
        &mut self,
        f: &mut Frame,
        area: Rect,
        (idx, c): (usize, usize),
        title: String,
    ) {
        let slot = &self.targets[idx].extra[c];
        let cmd = &self.targets[idx].cmds[c];
        let running = slot.running;
        let view_only = self.vnc_view_only(idx, c);
        let captured = self.console_captured();
        let remote = cmd.vnc.as_ref().is_some_and(|v| v.unencrypted_remote());
        let mut spans = vec![Span::raw(title.trim_end().to_string())];
        if running && view_only {
            spans.push(Span::raw(" · view-only"));
        }
        if remote {
            spans.push(Span::raw(" · "));
            spans.push(Span::styled("UNENCRYPTED", Style::new().red().bold()));
        }
        spans.push(Span::raw(" "));
        let title_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let mut block = super::pane()
            .title(Line::from(spans))
            .border_style(if captured {
                Style::new().yellow().bold()
            } else {
                self.border(Focus::Extra)
            });
        let hint = if captured {
            Some(if self.targets[idx].cmds.len() > 1 {
                " Ctrl+] back to dashboard · Ctrl+T view-only · Ctrl+R/F5 restart · F6 next view "
            } else {
                " Ctrl+] back to dashboard · Ctrl+T view-only · Ctrl+R/F5 restart "
            })
        } else if running && view_only {
            Some(" Ctrl+T sends the keyboard to the desktop ")
        } else if running {
            Some(" click or ⇧↓ to control ")
        } else {
            None
        };
        // Never over the title (it may carry a warning such as UNENCRYPTED).
        if let Some(hint) = hint.filter(|h| super::hint_fits(area.width, title_width, h)) {
            block = block.title(Line::from(hint).right_aligned());
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        if running && self.vnc_ready(idx, c).is_some() && self.vnc.renderer.is_some() {
            // Under a dialog the picture goes away; the cells stay empty.
            if self.confirm.is_none() && !self.help && !self.info {
                let pane = PaneRect::new(inner.x, inner.y, inner.width, inner.height);
                self.vnc.area = Some(inner);
                self.vnc.placeholders = self.paint_placeholders(f.buffer_mut(), pane);
                self.want_remote_size(pane);
            }
            return;
        }
        let slot = &self.targets[idx].extra[c];
        let mut lines = Vec::new();
        if running {
            let note = if slot.vnc_note.is_empty() {
                "connecting…"
            } else {
                &slot.vnc_note
            };
            let elapsed = self
                .vnc
                .run
                .as_ref()
                .filter(|r| (r.t, r.c) == (idx, c))
                .map_or(Duration::ZERO, |r| r.started.elapsed());
            let oci = cmd
                .vnc
                .as_ref()
                .is_some_and(|v| matches!(v.source, crate::console::VncSource::Oci(_)));
            let hint = if oci {
                "OCI sets a console connection up first: this can take a minute"
            } else {
                "still waiting for the desktop's first picture"
            };
            let loading = loading_screen("Opening the graphical console", note, elapsed, hint);
            super::render_loading(f, inner, loading);
            return;
        } else if slot.finished_at.is_some() {
            lines.push(Line::from(slot.vnc_note.clone().red()));
            lines.push(Line::default());
            let mut hint = String::from("Ctrl+R or F5 starts the viewer again");
            if self.targets[idx].cmds.len() > 1 {
                hint.push_str("; c or F6 shows the next view (such as the serial console)");
            }
            lines.push(Line::from(hint.dim()));
        } else {
            lines.push(Line::from("starting…".dim()));
        }
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    /// Put the picture on the screen (or take it off), after the frame's
    /// text. Failures to write are the terminal's, and show up in the next
    /// frame.
    pub(super) fn draw_vnc_image(&mut self) {
        let mut out = io::stdout().lock();
        if let Some(mut old) = self.vnc.retired.take() {
            let _ = old.teardown(&mut out);
        }
        let area = self.vnc.area;
        let placeholders = self.vnc.placeholders;
        let Some(renderer) = self.vnc.renderer.as_mut() else {
            let _ = out.flush();
            return;
        };
        let more = match area {
            Some(area) => {
                let pane = PaneRect::new(area.x, area.y, area.width, area.height);
                if placeholders {
                    renderer.transmit(&mut out)
                } else {
                    renderer.draw(&mut out, pane)
                }
            }
            None => renderer.hide(&mut out).map(|()| false),
        };
        self.vnc.more = more.unwrap_or(false);
        let _ = out.flush();
    }

    /// The terminal layout changed. Virtual placements are discarded so the
    /// next frame starts with fresh Kitty ids at the new cell geometry. A
    /// direct placement stays up until its rescaled replacement is placed:
    /// deleting it first left the pane blank for the whole upload.
    pub(super) fn vnc_resized(&mut self) {
        if let Some(renderer) = self.vnc.renderer.as_mut() {
            if renderer.has_virtual_image() {
                let _ = renderer.teardown(&mut io::stdout().lock());
            } else {
                renderer.invalidate();
            }
        }
    }

    /// Quit: the session ends and every image is deleted from the terminal.
    pub(super) fn shutdown_vnc(&mut self) {
        self.stop_vnc();
        if !self.rage_quit {
            super::vnc::wait_oci_closed(std::time::Duration::from_secs(4));
        }
        let mut out = io::stdout().lock();
        if let Some(mut old) = self.vnc.retired.take() {
            let _ = old.teardown(&mut out);
        }
        let _ = out.flush();
    }

    /// Let go of the buttons the remote desktop holds down.
    pub(super) fn vnc_release(&mut self) {
        if let Some(run) = self.vnc.run.as_mut()
            && run.buttons != 0
        {
            let (x, y) = run.pointer;
            run.session
                .release(Input::Pointer(Pointer { x, y, buttons: 0 }));
            run.buttons = 0;
        }
    }

    /// Send a key to the remote desktop (dropped until its first picture:
    /// the keys would otherwise be replayed into whatever shows up).
    pub(super) fn vnc_send_key(
        &mut self,
        (t, c): (usize, usize),
        code: ratatui::crossterm::event::KeyCode,
        mods: ratatui::crossterm::event::KeyModifiers,
    ) {
        if let Some(run) = self.vnc_ready(t, c) {
            let layout = run.scancodes.then_some(run.keymap);
            run.session
                .send_keys(&super::vnc::key_events(code, mods, layout));
        }
    }

    /// A mouse event over the picture. `true`: it was for the remote desktop
    /// (or swallowed), the dashboard has nothing to do with it. The click
    /// that gives the pane the focus is the dashboard's.
    pub(super) fn vnc_mouse(&mut self, m: MouseEvent) -> bool {
        if !self.console_captured() {
            // The keyboard went elsewhere: what is held down is let go of.
            self.vnc_release();
            return false;
        }
        let Some(area) = self.vnc.area else {
            return false;
        };
        let (x, y) = (m.column, m.row);
        let held = self.vnc.run.as_ref().is_some_and(|r| r.buttons != 0);
        let releasing = held && matches!(m.kind, MouseEventKind::Up(_) | MouseEventKind::Drag(_));
        if !(contains(area, x, y) || releasing) {
            return false;
        }
        let VncState { run, renderer, .. } = &mut self.vnc;
        let (Some(run), Some(renderer)) = (run.as_mut(), renderer.as_ref()) else {
            return true;
        };
        if !run.framed {
            return true;
        }
        let bit = |b: MouseButton| match b {
            MouseButton::Left => 1u8,
            MouseButton::Middle => 2,
            MouseButton::Right => 4,
        };
        let mut buttons = run.buttons;
        let mut wheel = 0u8;
        match m.kind {
            MouseEventKind::Down(b) => buttons |= bit(b),
            MouseEventKind::Up(b) => buttons &= !bit(b),
            MouseEventKind::Drag(_) | MouseEventKind::Moved => {}
            MouseEventKind::ScrollUp => wheel = 8,
            MouseEventKind::ScrollDown => wheel = 16,
            _ => return true,
        }
        let pos = match renderer.map_pointer(x, y) {
            Some(p) => p,
            // A button that was pressed is released wherever the mouse is.
            None if matches!(m.kind, MouseEventKind::Up(_)) => run.pointer,
            // Outside the picture (the pane is bigger than it).
            None => return true,
        };
        if wheel == 0 && pos == run.pointer && buttons == run.buttons {
            return true;
        }
        let pointer = Input::Pointer(Pointer {
            x: pos.0,
            y: pos.1,
            buttons,
        });
        if wheel != 0 {
            // A wheel click is a button press and release.
            run.session.send(Input::Pointer(Pointer {
                x: pos.0,
                y: pos.1,
                buttons: buttons | wheel,
            }));
            run.session.release(pointer);
        } else if run.buttons & !buttons != 0 {
            // A button that is let go of must reach the remote desktop.
            run.session.release(pointer);
        } else {
            run.session.send(pointer);
        }
        run.buttons = buttons;
        run.pointer = pos;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vnc_graphics::Transport;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> ImageRect {
        ImageRect::new(x, y, w, h)
    }

    /// `pump_vnc` calls the pane "framed" only when the renderer takes the
    /// pixels. These are the cases it relies on.
    #[test]
    fn pixels_before_the_desktop_size_are_not_taken() {
        let mut r = Renderer::new(Transport::Kitty);
        let pixels = vec![0x80; 4 * 4 * 4];
        assert!(r.apply_bgra(rect(0, 0, 4, 4), &pixels).is_none());
        r.resize_framebuffer(8, 8);
        assert!(r.apply_bgra(rect(0, 0, 4, 4), &pixels).is_some());
    }

    #[test]
    fn pixels_outside_the_desktop_or_too_short_are_not_taken() {
        let mut r = Renderer::new(Transport::Kitty);
        r.resize_framebuffer(8, 8);
        let pixels = vec![0x80; 4 * 4 * 4];
        assert!(r.apply_bgra(rect(100, 100, 4, 4), &pixels).is_none());
        assert!(r.apply_bgra(rect(0, 0, 4, 4), &pixels[..8]).is_none());
        assert!(r.apply_rgb(rect(0, 0, 4, 4), &pixels[..8]).is_none());
        assert!(r.apply_rgb(rect(0, 0, 2, 2), &pixels[..12]).is_some());
    }
}

/// What a console pane shows until its console is there: what is happening,
/// for how long, and a spinner, so that a slow start does not look stuck.
/// HINT is added once it takes long.
pub(super) fn loading_screen(
    header: &str,
    step: &str,
    elapsed: Duration,
    hint: &str,
) -> Vec<Line<'static>> {
    const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let frame = SPINNER[(elapsed.as_millis() / 100) as usize % SPINNER.len()];
    let secs = elapsed.as_secs();
    let mut lines = vec![
        Line::from(vec![
            Span::styled(frame.to_string(), Style::new().cyan().bold()),
            Span::raw("  "),
            Span::styled(header.to_string(), Style::new().bold()),
        ]),
        Line::default(),
        Line::from(step.to_string()),
        Line::from(format!("{secs} s").dim()),
    ];
    if secs >= 10 {
        lines.push(Line::default());
        lines.push(Line::from(hint.to_string().dim()));
        lines.push(Line::from("Ctrl+R starts over".dim()));
    }
    lines
}

#[cfg(test)]
mod loading_tests {
    use super::*;

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_loading_screen_says_what_is_going_on_and_for_how_long() {
        let early = text(&loading_screen(
            "Opening",
            "step x",
            Duration::from_secs(3),
            "hint",
        ));
        assert!(early.contains("Opening") && early.contains("step x"));
        assert!(early.contains("3 s"));
        assert!(!early.contains("hint"));
        let late = text(&loading_screen(
            "Opening",
            "step",
            Duration::from_secs(25),
            "hint",
        ));
        assert!(late.contains("hint") && late.contains("Ctrl+R"));
        // The spinner moves.
        let a = text(&loading_screen("w", "s", Duration::from_millis(0), "h"));
        let b = text(&loading_screen("w", "s", Duration::from_millis(100), "h"));
        assert_ne!(a.lines().next(), b.lines().next());
    }
}
