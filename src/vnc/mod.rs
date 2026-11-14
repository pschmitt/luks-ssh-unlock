//! Native asynchronous RFB/VNC client and backend.
//!
//! The protocol implementation is vendored from desktui 0.4.0 and vnc-rs 0.5.3;
//! see the license and third-party notices in this directory.

#[allow(dead_code)]
pub mod api;
pub mod backend;
pub mod keymap;
pub mod rfb;

#[cfg(test)]
mod loopback_tests;

#[cfg(test)]
pub use api::ResizeStatus;
pub use api::{Backend, ConnectError, Input, Key, Pointer, Rect, ScreenLayout, Update};
pub use backend::{Options, Vnc};
