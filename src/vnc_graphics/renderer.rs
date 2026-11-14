//! Synchronous framebuffer-to-Kitty renderer for a bounded terminal pane.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use super::PaneRect;
use super::detect as terminal;
use super::framebuffer::{Framebuffer, Rect as FbRect};
use super::kitty::{self, Placement};
use super::transport;

// Spread virtual IDs across distinct xterm color-cube values and delay reuse,
// so tmux cannot collapse adjacent IDs while stale placeholder cells remain.
const IMAGE_IDS: [u32; 8] = [
    0x5f0000, 0x5f005f, 0x5f0087, 0x5f00af, 0x5f00d7, 0x5f00ff, 0x5f5f00, 0x5f5f5f,
];
const CELL_PX: (u32, u32) = (8, 16);
const MAX_FRAMEBUFFER_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_FRAME_PIXELS: u64 = 1024 * 1024;
#[allow(dead_code)] // Used by the public terminal-cell geometry setter.
const MAX_CELL_PIXELS: u32 = 32;
const MIN_DRAW_INTERVAL: Duration = Duration::from_millis(100);

fn terminal_cell_pixel_size() -> Option<(u32, u32)> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: ioctl initializes the supplied winsize when it succeeds.
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: TIOCGWINSZ initialized the structure on success.
    let size = unsafe { size.assume_init() };
    cell_pixel_size_from_winsize(size)
}

fn cell_pixel_size_from_winsize(size: libc::winsize) -> Option<(u32, u32)> {
    let (cols, rows, width, height) = (
        u32::from(size.ws_col),
        u32::from(size.ws_row),
        u32::from(size.ws_xpixel),
        u32::from(size.ws_ypixel),
    );
    if cols == 0 || rows == 0 || width == 0 || height == 0 {
        return None;
    }
    Some((
        (width / cols).clamp(1, MAX_CELL_PIXELS),
        (height / rows).clamp(1, MAX_CELL_PIXELS),
    ))
}
const MAX_OUTPUT_BATCH_BYTES: usize = 64 * 1024;
const TMUX_OVERLAY_Z: i32 = -1;

fn inactive_image_id(active: Option<u32>) -> u32 {
    let next = active
        .and_then(|active| IMAGE_IDS.iter().position(|image_id| *image_id == active))
        .map_or(0, |index| (index + 1) % IMAGE_IDS.len());
    IMAGE_IDS[next]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Kitty,
}

/// Detect likely Kitty-protocol terminals. This is an environment-based hint,
/// not a protocol probe; callers may still handle a terminal that rejects APCs.
///
/// Outside tmux the terminal's own environment decides. Inside tmux the
/// terminal of the attached client does (its name, its `XTVERSION` answer, its
/// process environment), and `allow-passthrough` (`on` or `all`) must be set
/// for the pane; discovery is asynchronous there and an initial call may
/// return `None` (see [`unsupported_reason`]). `LUKS_SSH_UNLOCK_GRAPHICS`
/// (`kitty` or `off`) overrides the recognition of the terminal.
pub fn detect() -> Option<Transport> {
    let forced = match terminal::override_from_env() {
        terminal::Override::Off => return None,
        terminal::Override::Force => true,
        terminal::Override::Auto => false,
    };
    if transport::in_tmux() {
        transport::tmux_graphics_supported(forced).then_some(Transport::Kitty)
    } else {
        (forced || terminal::classify_process_env().is_some()).then_some(Transport::Kitty)
    }
}

/// Why [`detect`] says no, ready to show. `None`: the reason is not known
/// (yet), which only happens in tmux until its first answer is in.
pub fn unsupported_reason() -> Option<String> {
    let forced = match terminal::override_from_env() {
        terminal::Override::Off => {
            return Some(format!(
                "Pictures are turned off ({}=off).",
                terminal::OVERRIDE_ENV
            ));
        }
        terminal::Override::Force => true,
        terminal::Override::Auto => false,
    };
    if transport::in_tmux() {
        transport::tmux_unsupported_reason(forced)
    } else if forced || terminal::classify_process_env().is_some() {
        None
    } else {
        Some(terminal::outside_reason(
            std::env::var("TERM").ok().as_deref().unwrap_or(""),
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
    fn framebuffer(self) -> FbRect {
        FbRect::new(
            self.x.into(),
            self.y.into(),
            self.width.into(),
            self.height.into(),
        )
    }
    fn from_framebuffer(rect: FbRect) -> Self {
        Self::new(
            rect.x as u16,
            rect.y as u16,
            rect.width as u16,
            rect.height as u16,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceholderCell {
    pub col: u16,
    pub row: u16,
    pub foreground: (u8, u8, u8),
    pub glyph: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placeholders {
    pub cells: Vec<PlaceholderCell>,
}

pub struct Renderer {
    _transport: Transport,
    framebuffer: Framebuffer,
    dirty: bool,
    pane: Option<PaneRect>,
    tmux_origin: Option<(u16, u16)>,
    cell_px: (u32, u32),
    displayed: Option<(u16, u16, u16, u16)>,
    active_image_id: Option<u32>,
    // Virtual placeholders are redrawn asynchronously by tmux, separately from
    // passthrough APCs. Direct overlays keep each image switch in one APC stream.
    tmux_direct: bool,
    uploaded: bool,
    last_draw: Option<Instant>,
    virtual_image: Option<(u32, u32, u16, u16, u16, u16)>,
    pending: Option<PendingTransfer>,
}

struct PendingTransfer {
    bytes: Vec<u8>,
    offset: usize,
    pane: PaneRect,
    origin: (u16, u16),
    preserve_cursor: bool,
    image_id: u32,
    displayed: (u16, u16, u16, u16),
}

impl Renderer {
    pub fn new(transport: Transport) -> Self {
        Self {
            _transport: transport,
            framebuffer: Framebuffer::new(0, 0),
            dirty: true,
            pane: None,
            tmux_origin: None,
            cell_px: terminal_cell_pixel_size().unwrap_or(CELL_PX),
            displayed: None,
            active_image_id: None,
            // Explicit Kitty placements bypass tmux's pane clipping and z-order.
            // Keep tmux on virtual placeholders, which are ordinary pane cells.
            tmux_direct: false,
            uploaded: false,
            last_draw: None,
            virtual_image: None,
            pending: None,
        }
    }

    pub fn framebuffer_size(&self) -> (u32, u32) {
        (self.framebuffer.width(), self.framebuffer.height())
    }

    #[allow(dead_code)] // Exposed for integrations that query/set terminal geometry.
    pub fn cell_pixel_size(&self) -> (u32, u32) {
        self.cell_px
    }

    /// Supply terminal cell geometry when the caller already queried it.
    #[allow(dead_code)] // Exposed for integrations that query/set terminal geometry.
    pub fn set_cell_pixel_size(&mut self, width: u32, height: u32) {
        let next = (
            width.clamp(1, MAX_CELL_PIXELS),
            height.clamp(1, MAX_CELL_PIXELS),
        );
        if self.cell_px != next {
            self.cell_px = next;
            self.dirty = true;
        }
    }

    /// Resize to a bounded source image. Oversized server dimensions are scaled
    /// down with aspect ratio preserved before allocation (maximum 64 MiB BGRA).
    pub fn resize_framebuffer(&mut self, width: u32, height: u32) {
        let (width, height) = bounded_size(width, height);
        if self.framebuffer_size() != (width, height) {
            self.framebuffer.resize(width, height);
            self.dirty = true;
            self.uploaded = false;
        }
    }

    pub fn apply_bgra(&mut self, rect: Rect, bgra: &[u8]) -> Option<Rect> {
        let damage = self.framebuffer.apply_bgra(rect.framebuffer(), bgra)?;
        self.dirty = true;
        self.uploaded = false;
        Some(Rect::from_framebuffer(damage))
    }

    pub fn apply_rgb(&mut self, rect: Rect, rgb: &[u8]) -> Option<Rect> {
        let damage = self.framebuffer.apply_rgb(rect.framebuffer(), rgb)?;
        self.dirty = true;
        self.uploaded = false;
        Some(Rect::from_framebuffer(damage))
    }

    /// Copy a remote framebuffer rectangle; `from` is its source (x,y).
    pub fn apply_copy(&mut self, dst: Rect, from: (u16, u16)) -> Option<Rect> {
        let dst_x = u32::from(dst.x);
        let dst_y = u32::from(dst.y);
        let from_x = u32::from(from.0);
        let from_y = u32::from(from.1);
        let w = u32::from(dst.width)
            .min(self.framebuffer.width().saturating_sub(dst_x))
            .min(self.framebuffer.width().saturating_sub(from_x));
        let h = u32::from(dst.height)
            .min(self.framebuffer.height().saturating_sub(dst_y))
            .min(self.framebuffer.height().saturating_sub(from_y));
        if w == 0 || h == 0 {
            return None;
        }
        let row_bytes = w as usize * 4;
        let dst_fb = FbRect::new(dst_x, dst_y, w, h);
        if dst_y > from_y {
            for row in (0..h).rev() {
                self.copy_row(dst_fb, (from_x, from_y), row, row_bytes);
            }
        } else {
            for row in 0..h {
                self.copy_row(dst_fb, (from_x, from_y), row, row_bytes);
            }
        }
        self.dirty = true;
        self.uploaded = false;
        Some(Rect::new(dst_x as u16, dst_y as u16, w as u16, h as u16))
    }

    fn copy_row(&mut self, dst: FbRect, from: (u32, u32), row: u32, bytes: usize) {
        let stride = self.framebuffer.width() as usize * 4;
        let src = (from.1 as usize + row as usize) * stride + from.0 as usize * 4;
        let target = (dst.y as usize + row as usize) * stride + dst.x as usize * 4;
        self.framebuffer
            .pixels_mut_copy_within(src..src + bytes, target);
    }

    /// Outer pixel size for a pane's full cell rectangle, for guest resize requests.
    pub fn pane_pixels(&self, pane: PaneRect) -> Option<(u16, u16)> {
        if pane.cols == 0 || pane.rows == 0 {
            return None;
        }
        Some((
            u32::from(pane.cols)
                .saturating_mul(self.cell_px.0)
                .min(u16::MAX as u32) as u16,
            u32::from(pane.rows)
                .saturating_mul(self.cell_px.1)
                .min(u16::MAX as u32) as u16,
        ))
    }

    /// Whether the picture is shown through virtual placeholders (tmux cells)
    /// rather than an explicit placement.
    pub fn has_virtual_image(&self) -> bool {
        self.virtual_image.is_some()
    }

    /// Render and upload the picture again at the next draw, for example after
    /// the cell geometry changed. What is on screen stays until it is replaced.
    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    /// Use explicit Kitty placements when virtual placeholders cannot reliably
    /// follow a rapidly changing fullscreen framebuffer in tmux.
    pub fn set_tmux_direct(&mut self, direct: bool) {
        if self.tmux_direct != direct {
            self.tmux_direct = direct;
            self.dirty = true;
            self.uploaded = false;
        }
    }

    fn placeholder_image_id(&self) -> u32 {
        if let Some(active) = self.active_image_id {
            return active;
        }
        if let Some(pending) = &self.pending {
            return pending.image_id;
        }
        inactive_image_id(None)
    }

    /// Describe the pane-local Unicode cells that must be painted for a tmux
    /// virtual placement. Coordinates are absolute terminal cells.
    pub fn placeholder_cells(&mut self, pane: PaneRect) -> Option<Placeholders> {
        if !transport::in_tmux()
            || self.tmux_direct
            || self.framebuffer.width() == 0
            || self.framebuffer.height() == 0
            || pane.cols == 0
            || pane.rows == 0
        {
            self.set_virtual_image(None);
            return None;
        }
        let (fit_cols, fit_rows) = fit_cells(self.framebuffer_size(), pane, self.cell_px);
        if fit_cols == 0 || fit_rows == 0 {
            self.set_virtual_image(None);
            return None;
        }
        let display_w = u32::from(fit_cols).saturating_mul(self.cell_px.0);
        let display_h = u32::from(fit_rows).saturating_mul(self.cell_px.1);
        let (mut width, mut height) = bounded_transfer_size(display_w, display_h);
        width = width.min(self.cell_px.0 * 128).max(1);
        height = height.min(self.cell_px.1 * 128).max(1);
        let cols = width.div_ceil(self.cell_px.0) as u16;
        let rows = height.div_ceil(self.cell_px.1) as u16;
        let origin_col = pane.cols.saturating_sub(cols) / 2;
        let origin_row = pane.rows.saturating_sub(rows) / 2;
        let mut cells = Vec::with_capacity(usize::from(cols) * usize::from(rows));
        let image_id = self.placeholder_image_id();
        let foreground = kitty::placeholder_foreground(image_id);
        for row in 0..rows {
            for col in 0..cols {
                let glyph = kitty::placeholder_glyph(image_id, row, col)?;
                cells.push(PlaceholderCell {
                    col: pane.left.saturating_add(origin_col).saturating_add(col),
                    row: pane.top.saturating_add(origin_row).saturating_add(row),
                    foreground,
                    glyph,
                });
            }
        }
        self.pane = Some(pane);
        self.set_virtual_image(Some((width, height, cols, rows, origin_col, origin_row)));
        Some(Placeholders { cells })
    }

    fn set_virtual_image(&mut self, next: Option<(u32, u32, u16, u16, u16, u16)>) {
        // Direct placements track `displayed` themselves: the per-frame call
        // that says "no virtual image" must not make `draw` think the picture
        // is gone, or it re-places the image on every event-loop pass.
        if next.is_some() || self.virtual_image.is_some() {
            self.displayed = next.map(|(_, _, cols, rows, origin_col, origin_row)| {
                (origin_col, origin_row, cols, rows)
            });
        }
        if self.virtual_image != next {
            self.virtual_image = next;
            self.dirty = true;
            self.uploaded = false;
        }
    }

    /// Transmit dirty image data after the caller has painted placeholder cells.
    /// Returns true while a transfer is queued or rate-limited.
    pub fn transmit<W: Write>(&mut self, out: &mut W) -> io::Result<bool> {
        if self.pending.is_some() {
            return self.emit_pending(out);
        }
        let Some(pane) = self.pane else {
            return Ok(false);
        };
        let Some((width, height, cols, rows, origin_col, origin_row)) = self.virtual_image else {
            return Ok(false);
        };
        if transport::viewers_changed() {
            self.dirty = true;
        }
        if !self.dirty && self.uploaded {
            return Ok(false);
        }
        if self.dirty
            && self
                .last_draw
                .is_some_and(|at| at.elapsed() < MIN_DRAW_INTERVAL)
        {
            return Ok(true);
        }
        let source = aspect_fit_source(self.framebuffer_size(), width, height);
        let mut rgb = Vec::new();
        resample(&self.framebuffer, source, width, height, &mut rgb);
        let image_id = inactive_image_id(self.active_image_id);
        let mut bytes = Vec::new();
        kitty::encode_virtual_rgb(&mut bytes, image_id, width, height, self.cell_px, &rgb);
        self.pending = Some(PendingTransfer {
            bytes,
            offset: 0,
            pane,
            origin: transport::pane_origin().unwrap_or((0, 0)),
            preserve_cursor: false,
            image_id,
            displayed: (origin_col, origin_row, cols, rows),
        });
        self.dirty = false;
        self.last_draw = Some(Instant::now());
        self.emit_pending(out)
    }

    fn discard_pending<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        let Some(pending) = self.pending.take() else {
            return Ok(());
        };
        let mut bytes = Vec::new();
        kitty::delete_image(&mut bytes, pending.image_id);
        transport::write_at_origin(out, &bytes, pending.pane, pending.origin)?;
        self.dirty = true;
        self.uploaded = self.active_image_id.is_some();
        Ok(())
    }

    fn emit_pending<W: Write>(&mut self, out: &mut W) -> io::Result<bool> {
        self.emit_pending_batch(out)
    }

    fn emit_pending_batch<W: Write>(&mut self, out: &mut W) -> io::Result<bool> {
        let Some(pending) = self.pending.as_mut() else {
            return Ok(false);
        };
        let end = next_batch_end(&pending.bytes, pending.offset, MAX_OUTPUT_BATCH_BYTES);
        let mut batch = Vec::with_capacity(end - pending.offset + 32);
        batch.extend_from_slice(b"\x1b[?2026h");
        if pending.preserve_cursor {
            batch.extend_from_slice(b"\x1b7");
        }
        batch.extend_from_slice(&pending.bytes[pending.offset..end]);
        if pending.preserve_cursor {
            batch.extend_from_slice(b"\x1b8");
        }
        batch.extend_from_slice(b"\x1b[?2026l");
        transport::write_at_origin(out, &batch, pending.pane, pending.origin)?;
        pending.offset = end;
        if pending.offset == pending.bytes.len() {
            let completed = self.pending.take().expect("pending transfer exists");
            self.active_image_id = Some(completed.image_id);
            self.displayed = Some(completed.displayed);
            self.uploaded = !self.dirty;
        }
        Ok(self.pending.is_some() || self.dirty)
    }

    /// Draw the current image. Returns true while a transfer is queued or rate-limited.
    pub fn draw<W: Write>(&mut self, out: &mut W, pane: PaneRect) -> io::Result<bool> {
        let origin = if self.tmux_direct {
            let Some(origin) = transport::pane_origin() else {
                return Ok(true);
            };
            origin
        } else {
            (0, 0)
        };
        let pane_changed = self.pane != Some(pane);
        let origin_changed = self.tmux_origin.is_some_and(|old| old != origin);
        if self.pending.as_ref().is_some_and(|pending| {
            self.tmux_direct && (pending.pane != pane || pending.origin != origin)
        }) {
            self.discard_pending(out)?;
        } else if self.pending.is_some() {
            return self.emit_pending(out);
        }
        if self.framebuffer.width() == 0
            || self.framebuffer.height() == 0
            || pane.cols == 0
            || pane.rows == 0
        {
            self.pane = Some(pane);
            self.tmux_origin = Some(origin);
            return Ok(false);
        }
        if !transport::pane_is_active() {
            self.pane = Some(pane);
            self.tmux_origin = Some(origin);
            self.hide(out)?;
            return Ok(false);
        }
        // The old placement stays until its replacement goes up in the same
        // synchronized batch (or, for a new upload, until the new image is
        // placed): deleting it first would blank the pane in between.
        let replace_placement = (pane_changed || origin_changed) && self.displayed.is_some();
        self.tmux_origin = Some(origin);
        if !self.dirty && !pane_changed && !origin_changed && self.displayed.is_some() {
            return Ok(false);
        }
        if self.dirty
            && !pane_changed
            && !origin_changed
            && self
                .last_draw
                .is_some_and(|at| at.elapsed() < MIN_DRAW_INTERVAL)
        {
            return Ok(true);
        }
        let mut buf = Vec::new();
        let (cols, rows) = fit_cells(self.framebuffer_size(), pane, self.cell_px);
        if cols == 0 || rows == 0 {
            return Ok(false);
        }
        let (display_w, display_h) = (
            u32::from(cols).saturating_mul(self.cell_px.0),
            u32::from(rows).saturating_mul(self.cell_px.1),
        );
        let (width, height) = bounded_transfer_size(display_w, display_h);
        let origin_col = (pane.cols.saturating_sub(cols)) / 2;
        let origin_row = (pane.rows.saturating_sub(rows)) / 2;
        if self.uploaded && !self.dirty {
            let image_id = self.active_image_id.unwrap_or(IMAGE_IDS[0]);
            if replace_placement {
                kitty::delete_placement(&mut buf, image_id);
            }
            if self.tmux_direct {
                kitty::place_existing_at_z(
                    &mut buf,
                    image_id,
                    origin_col,
                    origin_row,
                    cols,
                    rows,
                    TMUX_OVERLAY_Z,
                );
            } else {
                kitty::place_existing(&mut buf, image_id, origin_col, origin_row, cols, rows);
            }
            self.displayed = Some((origin_col, origin_row, cols, rows));
            let mut batch = Vec::with_capacity(buf.len() + 20);
            batch.extend_from_slice(b"\x1b[?2026h\x1b7");
            batch.extend_from_slice(&buf);
            batch.extend_from_slice(b"\x1b8\x1b[?2026l");
            transport::write_at_origin(out, &batch, pane, origin)?;
            self.pane = Some(pane);
            self.last_draw = Some(Instant::now());
            return Ok(false);
        }
        let source = aspect_fit_source(self.framebuffer_size(), width, height);
        let mut rgb = Vec::new();
        resample(&self.framebuffer, source, width, height, &mut rgb);
        let image_id = inactive_image_id(self.active_image_id);
        let placement = Placement {
            image_id,
            col: origin_col,
            row: origin_row,
            width_px: width,
            height_px: height,
            cell_cols: cols,
            cell_rows: rows,
        };
        if self.tmux_direct {
            kitty::place_rgb_at_z(&mut buf, placement, &rgb, TMUX_OVERLAY_Z);
        } else {
            kitty::place_rgb(&mut buf, placement, &rgb);
        }
        if let Some(old_image_id) = self.active_image_id {
            kitty::delete_image(&mut buf, old_image_id);
        }
        self.pending = Some(PendingTransfer {
            bytes: buf,
            offset: 0,
            pane,
            origin,
            preserve_cursor: true,
            image_id,
            displayed: (origin_col, origin_row, cols, rows),
        });
        self.dirty = false;
        self.pane = Some(pane);
        self.last_draw = Some(Instant::now());
        self.emit_pending(out)
    }

    /// Remove visible placements but retain framebuffer contents for redraw.
    pub fn hide<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.discard_pending(out)?;
        let was_virtual = self.virtual_image.take().is_some();
        if self.displayed.take().is_some()
            && (!transport::in_tmux() || !was_virtual)
            && let Some(image_id) = self.active_image_id
        {
            let mut bytes = Vec::new();
            kitty::delete_placement(&mut bytes, image_id);
            transport::write_at_origin(
                out,
                &bytes,
                self.pane.unwrap_or(PaneRect::new(0, 0, 0, 0)),
                self.tmux_origin.unwrap_or((0, 0)),
            )?;
        }
        Ok(())
    }

    /// Delete this renderer's Kitty image; safe to call repeatedly.
    pub fn teardown<W: Write>(&mut self, out: &mut W) -> io::Result<()> {
        self.pending = None;
        self.displayed = None;
        self.active_image_id = None;
        self.dirty = true;
        self.uploaded = false;
        let mut bytes = Vec::new();
        for image_id in IMAGE_IDS {
            kitty::delete_image(&mut bytes, image_id);
        }
        self.virtual_image = None;
        transport::write_at_origin(
            out,
            &bytes,
            self.pane.unwrap_or(PaneRect::new(0, 0, 0, 0)),
            self.tmux_origin.unwrap_or((0, 0)),
        )
    }

    /// Map a pane-relative terminal cell to source pixels. Sub-cell offsets are
    /// optional and measured in terminal pixels.
    pub fn map_pointer(&self, col: u16, row: u16) -> Option<(u16, u16)> {
        let pane = self.pane?;
        let (left, top, cols, rows) = self.displayed?;
        let local_col = col.checked_sub(pane.left)?;
        let local_row = row.checked_sub(pane.top)?;
        if local_col < left
            || local_row < top
            || local_col >= left.saturating_add(cols)
            || local_row >= top.saturating_add(rows)
        {
            return None;
        }
        let (sw, sh) = self.framebuffer_size();
        let display_w = u32::from(cols) * self.cell_px.0;
        let display_h = u32::from(rows) * self.cell_px.1;
        let dx = (u32::from(local_col - left) * self.cell_px.0 + self.cell_px.0 / 2)
            .min(display_w.saturating_sub(1));
        let dy = (u32::from(local_row - top) * self.cell_px.1 + self.cell_px.1 / 2)
            .min(display_h.saturating_sub(1));
        Some((
            (u64::from(dx) * u64::from(sw) / u64::from(display_w.max(1)))
                .min(u64::from(sw.saturating_sub(1))) as u16,
            (u64::from(dy) * u64::from(sh) / u64::from(display_h.max(1)))
                .min(u64::from(sh.saturating_sub(1))) as u16,
        ))
    }
}

fn next_batch_end(bytes: &[u8], offset: usize, limit: usize) -> usize {
    let mut end = offset;
    while end < bytes.len() {
        let Some(terminator) = bytes[end..].windows(2).position(|pair| pair == b"\x1b\\") else {
            return bytes.len();
        };
        let packet_end = end + terminator + 2;
        if packet_end - offset > limit && end > offset {
            break;
        }
        end = packet_end;
        if end - offset >= limit {
            break;
        }
    }
    end
}

fn bounded_size(width: u32, height: u32) -> (u32, u32) {
    bounded_dimensions(width, height, MAX_FRAMEBUFFER_PIXELS)
}

fn bounded_transfer_size(width: u32, height: u32) -> (u32, u32) {
    bounded_dimensions(width, height, MAX_FRAME_PIXELS)
}

fn bounded_dimensions(width: u32, height: u32, max_pixels: u64) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (width, height);
    }
    let pixels = u64::from(width) * u64::from(height);
    if pixels <= max_pixels {
        return (width, height);
    }
    let scale = (max_pixels as f64 / pixels as f64).sqrt();
    let mut w = ((f64::from(width) * scale).floor() as u32).max(1);
    let mut h = ((f64::from(height) * scale).floor() as u32).max(1);
    while u64::from(w) * u64::from(h) > max_pixels {
        if w >= h && w > 1 {
            w /= 2;
        } else if h > 1 {
            h /= 2;
        } else {
            break;
        }
    }
    (w, h)
}

fn fit_cells(size: (u32, u32), pane: PaneRect, cell: (u32, u32)) -> (u16, u16) {
    let (pw, ph) = (
        u32::from(pane.cols).saturating_mul(cell.0),
        u32::from(pane.rows).saturating_mul(cell.1),
    );
    let (_, _, w, h) = aspect_fit_source(size, pw, ph);
    (
        (w / cell.0).min(pane.cols as u32) as u16,
        (h / cell.1).min(pane.rows as u32) as u16,
    )
}

fn aspect_fit_source(source: (u32, u32), bounds_w: u32, bounds_h: u32) -> (u32, u32, u32, u32) {
    if source.0 == 0 || source.1 == 0 || bounds_w == 0 || bounds_h == 0 {
        return (0, 0, 0, 0);
    }
    let scale =
        (f64::from(bounds_w) / f64::from(source.0)).min(f64::from(bounds_h) / f64::from(source.1));
    let w = (f64::from(source.0) * scale).round().max(1.0) as u32;
    let h = (f64::from(source.1) * scale).round().max(1.0) as u32;
    (0, 0, w.min(bounds_w), h.min(bounds_h))
}

fn resample(
    fb: &Framebuffer,
    _source: (u32, u32, u32, u32),
    width: u32,
    height: u32,
    out: &mut Vec<u8>,
) {
    let (sw, sh) = (fb.width(), fb.height());
    if sw == 0 || sh == 0 || width == 0 || height == 0 {
        return;
    }
    let data = fb.pixels();
    let x_weights = (0..width)
        .map(|x| axis_weights(sw, width, x))
        .collect::<Vec<_>>();
    let y_weights = (0..height)
        .map(|y| axis_weights(sh, height, y))
        .collect::<Vec<_>>();
    out.reserve(width as usize * height as usize * 3);
    for yw in &y_weights {
        for xw in &x_weights {
            let mut rgb = [0.0f32; 3];
            for &(sy, wy) in yw {
                for &(sx, wx) in xw {
                    let i = (sy as usize * sw as usize + sx as usize) * 4;
                    let weight = wx * wy;
                    rgb[0] += f32::from(data[i + 2]) * weight;
                    rgb[1] += f32::from(data[i + 1]) * weight;
                    rgb[2] += f32::from(data[i]) * weight;
                }
            }
            out.extend(rgb.map(|v| v.round().clamp(0.0, 255.0) as u8));
        }
    }
}

/// Area-average when reducing dimensions to retain detail without aliasing;
/// pixel-center linear interpolation when enlarging.
fn axis_weights(source: u32, target: u32, index: u32) -> Vec<(u32, f32)> {
    if target < source {
        let start = index as f64 * f64::from(source) / f64::from(target);
        let end = (index + 1) as f64 * f64::from(source) / f64::from(target);
        let span = end - start;
        let first = start.floor() as u32;
        let last = end.ceil() as u32;
        (first..last)
            .map(|i| {
                let overlap = end.min(f64::from(i + 1)) - start.max(f64::from(i));
                (i, (overlap / span) as f32)
            })
            .collect()
    } else {
        let pos = ((f64::from(index) + 0.5) * f64::from(source) / f64::from(target) - 0.5)
            .clamp(0.0, f64::from(source - 1));
        let low = pos.floor() as u32;
        let high = (low + 1).min(source - 1);
        if low == high {
            vec![(low, 1.0)]
        } else {
            let mix = (pos - f64::from(low)) as f32;
            vec![(low, 1.0 - mix), (high, mix)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pixels that do not compress, so that transfers stay multi-batch.
    fn noise(len: usize, mut seed: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect()
    }

    fn test_renderer() -> Renderer {
        let mut renderer = Renderer::new(Transport::Kitty);
        renderer.set_cell_pixel_size(8, 16);
        renderer
    }

    /// Cost of one console frame through the tmux (virtual placement) path:
    /// `cargo test --release -- --ignored --nocapture frame_cost`.
    #[test]
    #[ignore = "timing report, not a check"]
    fn frame_cost_report() {
        for &(sw, sh, cols, rows) in &[
            (720u32, 400u32, 200u16, 50u16),
            (1024, 768, 200, 50),
            (1280, 800, 120, 40),
        ] {
            let mut renderer = test_renderer();
            renderer.resize_framebuffer(sw, sh);
            // Black console with a few lines of grey text-like noise.
            let mut rgb = vec![0u8; (sw * sh * 3) as usize];
            for line in 0..20u32 {
                for x in 0..(sw * 2 / 3) {
                    if (x * 7 + line * 13) % 5 < 2 {
                        for y in 0..10 {
                            let i = (((line * 18 + y) * sw + x) * 3) as usize;
                            rgb[i..i + 3].copy_from_slice(&[0xaa, 0xaa, 0xaa]);
                        }
                    }
                }
            }
            renderer.apply_rgb(Rect::new(0, 0, sw as u16, sh as u16), &rgb);
            let pane = PaneRect::new(0, 0, cols, rows);
            let (fc, fr) = fit_cells(renderer.framebuffer_size(), pane, renderer.cell_px);
            let (w, h) = bounded_transfer_size(u32::from(fc) * 8, u32::from(fr) * 16);
            let (w, h) = (w.min(8 * 128), h.min(16 * 128));
            renderer.pane = Some(pane);
            renderer.virtual_image =
                Some((w, h, w.div_ceil(8) as u16, h.div_ceil(16) as u16, 0, 0));
            let mut times = Vec::new();
            let mut total = 0usize;
            let mut batches = 0usize;
            for _ in 0..5 {
                renderer.dirty = true;
                renderer.last_draw = None;
                let start = Instant::now();
                let mut out = Vec::new();
                renderer.transmit(&mut out).unwrap();
                batches += 1;
                while renderer.pending.is_some() {
                    renderer.transmit(&mut out).unwrap();
                    batches += 1;
                }
                times.push(start.elapsed());
                total += out.len();
            }
            eprintln!(
                "FRAME_COST src={sw}x{sh} img={w}x{h} bytes/frame={} batches/frame={} cpu/frame={:?}",
                total / 5,
                batches / 5,
                times.iter().sum::<Duration>() / 5
            );
        }
    }

    #[test]
    fn terminal_cell_size_uses_pixel_dimensions_and_rejects_incomplete_geometry() {
        let size = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 900,
            ws_ypixel: 600,
        };
        assert_eq!(cell_pixel_size_from_winsize(size), Some((9, 20)));
        assert_eq!(
            cell_pixel_size_from_winsize(libc::winsize { ws_row: 0, ..size }),
            None
        );
        assert_eq!(
            cell_pixel_size_from_winsize(libc::winsize {
                ws_xpixel: 0,
                ..size
            }),
            None
        );
    }

    #[test]
    fn resampling_averages_downscaled_pixels_and_interpolates_enlarged_pixels() {
        let mut source = Framebuffer::new(3, 1);
        source.apply_rgb(FbRect::new(0, 0, 3, 1), &[0, 0, 0, 0, 0, 0, 255, 255, 255]);
        let mut reduced = Vec::new();
        resample(&source, (0, 0, 3, 1), 1, 1, &mut reduced);
        assert_eq!(reduced, [85, 85, 85]);

        let mut source = Framebuffer::new(2, 1);
        source.apply_rgb(FbRect::new(0, 0, 2, 1), &[0, 0, 0, 255, 255, 255]);
        let mut enlarged = Vec::new();
        resample(&source, (0, 0, 2, 1), 3, 1, &mut enlarged);
        assert_eq!(enlarged, [0, 0, 0, 128, 128, 128, 255, 255, 255]);
    }

    #[test]
    fn virtual_image_ids_do_not_repeat_until_the_ring_is_exhausted() {
        let ids = (0..IMAGE_IDS.len())
            .scan(None, |active, _| {
                let next = inactive_image_id(*active);
                *active = Some(next);
                Some(next)
            })
            .collect::<Vec<_>>();

        assert_eq!(ids, IMAGE_IDS);
        assert_eq!(inactive_image_id(ids.last().copied()), IMAGE_IDS[0]);
        assert_eq!(IMAGE_IDS[0] & 0x00ff_ffff, 0x5f0000);
        assert_eq!(IMAGE_IDS[1] & 0x00ff_ffff, 0x5f005f);
    }

    #[test]
    fn virtual_placeholders_keep_the_active_image_during_upload() {
        let mut renderer = test_renderer();
        renderer.active_image_id = Some(IMAGE_IDS[0]);
        renderer.dirty = true;
        renderer.uploaded = false;
        assert_eq!(renderer.placeholder_image_id(), IMAGE_IDS[0]);

        renderer.pending = Some(PendingTransfer {
            bytes: Vec::new(),
            offset: 0,
            pane: PaneRect::new(0, 0, 1, 1),
            origin: (0, 0),
            preserve_cursor: false,
            image_id: IMAGE_IDS[2],
            displayed: (0, 0, 1, 1),
        });
        assert_eq!(renderer.placeholder_image_id(), IMAGE_IDS[0]);

        renderer.active_image_id = Some(IMAGE_IDS[2]);
        renderer.pending = None;
        renderer.dirty = false;
        renderer.uploaded = true;
        assert_eq!(renderer.placeholder_image_id(), IMAGE_IDS[2]);
    }

    #[test]
    fn aspect_fit_keeps_ratio_and_centers_in_pane() {
        let mut renderer = test_renderer();
        renderer.resize_framebuffer(640, 480);
        renderer.apply_rgb(Rect::new(0, 0, 640, 480), &vec![0x33; 640 * 480 * 3]);
        let pane = PaneRect::new(3, 2, 20, 10);
        let mut out = Vec::new();
        renderer.draw(&mut out, pane).unwrap();
        assert!(!out.is_empty());
        assert!(renderer.map_pointer(3, 2).is_none());
    }

    #[test]
    fn virtual_placeholder_bounds_are_pointer_mappable() {
        let mut renderer = test_renderer();
        renderer.resize_framebuffer(16, 8);
        renderer.pane = Some(PaneRect::new(4, 2, 10, 5));
        renderer.set_virtual_image(Some((16, 8, 2, 1, 4, 2)));

        assert_eq!(renderer.map_pointer(8, 4), Some((4, 4)));
        assert_eq!(renderer.map_pointer(9, 4), Some((12, 4)));
        assert_eq!(renderer.map_pointer(7, 4), None);
    }

    #[test]
    fn idle_virtual_resize_invalidates_the_previous_upload() {
        let mut renderer = test_renderer();
        renderer.virtual_image = Some((800, 600, 100, 38, 0, 0));
        renderer.uploaded = true;
        renderer.dirty = false;

        renderer.set_virtual_image(Some((640, 480, 80, 30, 0, 0)));

        assert!(renderer.dirty);
        assert!(!renderer.uploaded);
    }

    #[test]
    fn tmux_direct_overlay_avoids_virtual_placeholder_id_reuse() {
        let mut renderer = test_renderer();
        renderer.set_tmux_direct(true);
        renderer.resize_framebuffer(16, 8);
        renderer.apply_rgb(Rect::new(0, 0, 16, 8), &[0x5a; 16 * 8 * 3]);
        let pane = PaneRect::new(4, 1, 12, 6);
        let mut out = Vec::new();

        assert!(renderer.placeholder_cells(pane).is_none());
        assert!(!renderer.draw(&mut out, pane).unwrap());
        assert!(
            out.windows(b"a=p,q=2,z=-1".len())
                .any(|w| w == b"a=p,q=2,z=-1")
        );
        assert!(out.windows(b"c=12,r=3".len()).any(|w| w == b"c=12,r=3"));
        assert!(!out.windows(b"a=T,U=1".len()).any(|w| w == b"a=T,U=1"));
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[0]));

        let resized_pane = PaneRect::new(10, 3, 6, 4);
        out.clear();
        assert!(!renderer.draw(&mut out, resized_pane).unwrap());
        let sync_begin = out
            .windows(b"\x1b[?2026h".len())
            .position(|bytes| bytes == b"\x1b[?2026h")
            .unwrap();
        let old_placement = out
            .windows(b"a=d,d=i,i=6225920,p=1".len())
            .position(|bytes| bytes == b"a=d,d=i,i=6225920,p=1")
            .unwrap();
        let new_placement = out
            .windows(b"a=p,q=2,z=-1".len())
            .position(|bytes| bytes == b"a=p,q=2,z=-1")
            .unwrap();
        let sync_end = out
            .windows(b"\x1b[?2026l".len())
            .position(|bytes| bytes == b"\x1b[?2026l")
            .unwrap();
        assert!(
            sync_begin < old_placement && old_placement < new_placement && new_placement < sync_end,
            "old and new placements must switch within one synchronized frame"
        );
        let expected_cursor = b"\x1b[5;11H".as_slice();
        assert!(
            out.windows(expected_cursor.len())
                .any(|w| w == expected_cursor),
            "resized placement missing: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(out.windows(b"c=6,r=1".len()).any(|w| w == b"c=6,r=1"));
        assert!(
            out.windows(b"a=p,q=2,z=-1".len())
                .any(|w| w == b"a=p,q=2,z=-1")
        );

        renderer.apply_rgb(Rect::new(0, 0, 16, 8), &[0x33; 16 * 8 * 3]);
        renderer.last_draw = None;
        out.clear();
        assert!(!renderer.draw(&mut out, pane).unwrap());
        let place = out
            .windows(b"a=p,q=2,z=-1".len())
            .position(|w| w == b"a=p,q=2,z=-1")
            .unwrap();
        let old_delete = out
            .windows(b"a=d,d=I,i=6225920".len())
            .position(|w| w == b"a=d,d=I,i=6225920")
            .unwrap();
        assert!(
            place < old_delete,
            "new placement must precede deleting the old image"
        );
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[1]));
    }

    #[test]
    fn direct_overlay_replaces_its_placement_after_the_pane_origin_moves() {
        let mut renderer = test_renderer();
        renderer.tmux_direct = true;
        renderer.resize_framebuffer(16, 8);
        let pane = PaneRect::new(4, 1, 12, 6);
        let mut out = Vec::new();
        renderer.draw(&mut out, pane).unwrap();
        assert!(renderer.displayed.is_some());

        renderer.tmux_origin = Some((7, 3));
        out.clear();
        assert!(!renderer.draw(&mut out, pane).unwrap());

        let deletion = out
            .windows(b"a=d,d=i,i=6225920,p=1".len())
            .position(|bytes| bytes == b"a=d,d=i,i=6225920,p=1")
            .unwrap();
        let placement = out
            .windows(b"a=p,q=2,z=-1".len())
            .position(|bytes| bytes == b"a=p,q=2,z=-1")
            .unwrap();
        assert!(deletion < placement);
    }

    #[test]
    fn changed_pane_discards_an_incomplete_transfer_before_restarting() {
        let mut renderer = test_renderer();
        renderer.tmux_direct = true;
        renderer.resize_framebuffer(512, 384);
        renderer.apply_rgb(Rect::new(0, 0, 512, 384), &noise(512 * 384 * 3, 1));
        let pane = PaneRect::new(0, 0, 128, 48);
        let moved = PaneRect::new(8, 4, 120, 44);
        let mut out = Vec::new();
        assert!(renderer.draw(&mut out, pane).unwrap());
        assert!(renderer.pending.is_some());

        out.clear();
        assert!(renderer.draw(&mut out, moved).unwrap());

        assert_eq!(renderer.pending.as_ref().map(|p| p.pane), Some(moved));
        let deletion = out
            .windows(b"a=d,d=I,i=6225920".len())
            .position(|bytes| bytes == b"a=d,d=I,i=6225920")
            .unwrap();
        let upload = out
            .windows(b"a=t,q=2,f=24,o=z,i=6225920".len())
            .position(|bytes| bytes == b"a=t,q=2,f=24,o=z,i=6225920")
            .unwrap();
        assert!(deletion < upload);
    }

    #[test]
    fn direct_transfer_is_double_buffered_and_bounded() {
        let mut renderer = test_renderer();
        renderer.resize_framebuffer(512, 384);
        renderer.apply_rgb(Rect::new(0, 0, 512, 384), &noise(512 * 384 * 3, 1));
        let pane = PaneRect::new(0, 0, 128, 48);
        let mut out = Vec::new();
        let mut more = renderer.draw(&mut out, pane).unwrap();
        assert!(more);
        assert!(out.len() <= MAX_OUTPUT_BATCH_BYTES + 128);
        assert_eq!(renderer.active_image_id, None);
        let mut batches = 1;
        while more {
            out.clear();
            more = renderer.draw(&mut out, pane).unwrap();
            assert!(out.len() <= MAX_OUTPUT_BATCH_BYTES + 128);
            batches += 1;
        }
        assert!(batches > 1);
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[0]));

        renderer.apply_rgb(Rect::new(0, 0, 512, 384), &noise(512 * 384 * 3, 2));
        renderer.last_draw = None;
        out.clear();
        let mut more = renderer.draw(&mut out, pane).unwrap();
        assert!(more);
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[0]));
        assert!(
            out.windows(b"a=t,q=2,f=24,o=z,i=6226015".len())
                .any(|bytes| { bytes == b"a=t,q=2,f=24,o=z,i=6226015" })
        );
        while more {
            out.clear();
            more = renderer.draw(&mut out, pane).unwrap();
            assert!(out.len() <= MAX_OUTPUT_BATCH_BYTES + 128);
        }
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[1]));
        assert!(
            out.windows(b"a=d,d=I,i=6225920".len())
                .any(|bytes| { bytes == b"a=d,d=I,i=6225920" })
        );
    }

    #[test]
    fn multipart_transfer_batches_end_on_apc_boundaries_and_stay_bounded() {
        let rgb = noise(256 * 128 * 3, 3);
        let mut encoded = Vec::new();
        kitty::encode_virtual_rgb(&mut encoded, IMAGE_IDS[0], 256, 128, CELL_PX, &rgb);

        let mut offset = 0;
        let mut batches = 0;
        while offset < encoded.len() {
            let end = next_batch_end(&encoded, offset, MAX_OUTPUT_BATCH_BYTES);
            assert!(end > offset);
            assert!(end - offset <= MAX_OUTPUT_BATCH_BYTES);
            assert!(encoded[offset..end].ends_with(b"\x1b\\"));
            offset = end;
            batches += 1;
        }
        assert!(batches > 1);
    }

    #[test]
    fn virtual_frame_upload_keeps_placeholders_on_active_image_until_complete() {
        let mut renderer = test_renderer();
        renderer.resize_framebuffer(256, 128);
        renderer.apply_rgb(Rect::new(0, 0, 256, 128), &noise(256 * 128 * 3, 4));
        renderer.pane = Some(PaneRect::new(4, 2, 32, 8));
        renderer.virtual_image = Some((256, 128, 32, 8, 0, 0));
        renderer.active_image_id = Some(IMAGE_IDS[0]);
        renderer.uploaded = true;
        assert_eq!(renderer.placeholder_image_id(), IMAGE_IDS[0]);

        let mut out = Vec::new();
        let mut more = renderer.transmit(&mut out).unwrap();
        assert!(more);
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[0]));
        assert_eq!(renderer.placeholder_image_id(), IMAGE_IDS[0]);
        assert!(
            out.windows(b"i=6226015".len())
                .any(|bytes| bytes == b"i=6226015")
        );
        while more {
            out.clear();
            more = renderer.transmit(&mut out).unwrap();
        }
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[1]));
        assert_eq!(renderer.placeholder_image_id(), IMAGE_IDS[1]);
        assert!(renderer.uploaded);
    }

    #[test]
    fn tmux_virtual_upload_limits_synchronized_output_to_each_batch() {
        let mut renderer = test_renderer();
        let pixels = vec![0x5a; 512 * 128 * 3];
        let mut bytes = Vec::new();
        let (cols, rows) =
            kitty::encode_virtual_rgb(&mut bytes, IMAGE_IDS[1], 512, 128, CELL_PX, &pixels);
        renderer.pending = Some(PendingTransfer {
            bytes,
            offset: 0,
            pane: PaneRect::new(0, 0, 64, 8),
            origin: (0, 0),
            preserve_cursor: false,
            image_id: IMAGE_IDS[1],
            displayed: (0, 0, cols, rows),
        });

        let mut out = Vec::new();
        while renderer.pending.is_some() {
            out.clear();
            renderer.emit_pending_batch(&mut out).unwrap();
            assert!(out.starts_with(b"\x1b[?2026h"));
            assert!(out.ends_with(b"\x1b[?2026l"));
        }

        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[1]));
    }

    #[test]
    fn source_and_direct_transfer_allocations_are_capped() {
        let source = bounded_size(u32::MAX, u32::MAX);
        assert!(u64::from(source.0) * u64::from(source.1) <= MAX_FRAMEBUFFER_PIXELS);
        let panoramic = bounded_size(u32::MAX, 1);
        assert!(u64::from(panoramic.0) * u64::from(panoramic.1) <= MAX_FRAMEBUFFER_PIXELS);
        let transfer = bounded_transfer_size(u32::MAX, u32::MAX);
        assert_eq!(bounded_transfer_size(1024, 768), (1024, 768));
        assert!(u64::from(transfer.0) * u64::from(transfer.1) <= MAX_FRAME_PIXELS);
        assert!(transfer.0 as usize * transfer.1 as usize * 3 <= 3 * 1024 * 1024);
    }

    #[test]
    fn dirty_frames_coalesce_until_rate_limit_allows_draw() {
        let mut renderer = test_renderer();
        renderer.resize_framebuffer(1, 1);
        renderer.apply_rgb(Rect::new(0, 0, 1, 1), &[1, 2, 3]);
        let pane = PaneRect::new(0, 0, 2, 2);
        renderer.pane = Some(pane);
        renderer.last_draw = Some(Instant::now());
        let mut out = Vec::new();
        assert!(renderer.draw(&mut out, pane).unwrap());
        assert!(out.is_empty());
        assert!(renderer.dirty);
    }

    #[test]
    fn per_frame_placeholder_query_does_not_re_place_a_direct_image() {
        let mut renderer = test_renderer();
        renderer.resize_framebuffer(4, 4);
        renderer.apply_rgb(Rect::new(0, 0, 4, 4), &[1; 48]);
        let pane = PaneRect::new(0, 0, 10, 5);
        let mut out = Vec::new();
        while renderer.draw(&mut out, pane).unwrap() {}
        for _ in 0..3 {
            // What the dashboard does on every frame before drawing the image.
            assert!(renderer.placeholder_cells(pane).is_none());
            out.clear();
            renderer.draw(&mut out, pane).unwrap();
            assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));
        }
        assert!(renderer.map_pointer(1, 1).is_some());
    }

    #[test]
    fn invalidated_direct_image_is_replaced_without_a_blank_interval() {
        let mut renderer = test_renderer();
        renderer.resize_framebuffer(64, 32);
        renderer.apply_rgb(Rect::new(0, 0, 64, 32), &noise(64 * 32 * 3, 5));
        let pane = PaneRect::new(0, 0, 16, 4);
        let mut out = Vec::new();
        while renderer.draw(&mut out, pane).unwrap() {}
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[0]));

        // The terminal was resized: a new, larger pane.
        renderer.invalidate();
        renderer.last_draw = None;
        let wider = PaneRect::new(0, 0, 24, 6);
        out.clear();
        while renderer.draw(&mut out, wider).unwrap() {}
        let text = String::from_utf8_lossy(&out);
        // The replacement is uploaded and placed before the old image goes,
        // and the old one is never deleted on its own beforehand.
        let upload = text.find("i=6226015").expect("replacement uploaded");
        let delete_old = text.find("a=d,d=I,i=6225920").expect("old image deleted");
        assert!(upload < delete_old);
        assert!(!text.contains("a=d,d=i,i=6225920"));
        assert_eq!(renderer.active_image_id, Some(IMAGE_IDS[1]));
        assert!(!renderer.has_virtual_image());
    }

    #[test]
    fn draw_is_idempotent_and_hide_keeps_framebuffer() {
        let mut renderer = test_renderer();
        renderer.resize_framebuffer(4, 4);
        renderer.apply_rgb(Rect::new(0, 0, 4, 4), &[1; 48]);
        let pane = PaneRect::new(0, 0, 10, 5);
        let mut out = Vec::new();
        renderer.draw(&mut out, pane).unwrap();
        out.clear();
        renderer.draw(&mut out, pane).unwrap();
        assert!(out.is_empty());
        renderer.hide(&mut out).unwrap();
        assert_eq!(renderer.framebuffer_size(), (4, 4));
        assert!(!out.is_empty());
        out.clear();
        assert!(!renderer.draw(&mut out, pane).unwrap());
        assert!(out.windows(5).any(|bytes| bytes == b"a=p,q"));
        out.clear();
        renderer.teardown(&mut out).unwrap();
        assert!(
            out.windows(b"a=d,d=I,i=6225920".len())
                .any(|bytes| bytes == b"a=d,d=I,i=6225920")
        );
        assert!(
            out.windows(b"a=d,d=I,i=6226015".len())
                .any(|bytes| bytes == b"a=d,d=I,i=6226015")
        );
        assert!(!out.windows(7).any(|bytes| bytes == b"a=d,d=A"));
    }
}
