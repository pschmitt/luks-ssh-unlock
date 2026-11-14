//! Self-contained Kitty graphics output for embedding a remote framebuffer in a pane.
//!
//! This module deliberately does not participate in the application's module tree yet;
//! callers can opt in without coupling it to the VNC protocol or terminal setup.
//!
//! Portions of the Kitty encoder and framebuffer are adapted from desktui 0.4.0
//! by Mish Ushakov, Copyright (c) 2026 Mish Ushakov, MIT licensed. The tmux
//! passthrough and pane-origin translation are adapted from the LSU desktui patch.

pub mod detect;
pub mod framebuffer;
pub mod kitty;
pub mod renderer;
pub mod transport;

pub use renderer::{Renderer, Transport, detect, unsupported_reason};

/// Drawable pane bounds in terminal cells. `left` and `top` are zero-based
/// within the process's terminal; tmux output adds the pane's outer origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneRect {
    pub left: u16,
    pub top: u16,
    pub cols: u16,
    pub rows: u16,
}

impl PaneRect {
    pub const fn new(left: u16, top: u16, cols: u16, rows: u16) -> Self {
        Self {
            left,
            top,
            cols,
            rows,
        }
    }
}
