//! The dashboard's in-process VNC viewer: a session on its own thread, and the
//! small pieces the dashboard needs around it (key translation, the safety
//! checks for the terminal context).
//!
//! The session talks to the dashboard through two bounded channels, so that a
//! busy or stalled remote desktop can neither grow memory without limit nor
//! slow the interface down:
//!
//! * **desktop to dashboard**: a queue that merges pixel updates covered by a
//!   newer one, and makes the session thread wait once too many bytes are
//!   pending (the dashboard takes events out of it without ever blocking);
//! * **dashboard to desktop**: a fixed-size channel written with `try_send`;
//!   when it is full a key press or pointer move is dropped, but a release
//!   (of a key or a button) never is: it is parked and sent as soon as there
//!   is room, and the session ends instead if that does not come, so nothing
//!   stays held on the remote desktop.
//!
//! Dropping a [`Session`] asks the thread to stop and returns at once; the
//! thread closes the connection in the background.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

use crate::console::{OciConsole, VncEndpoint, VncSource};
use crate::oci::vnc::{Cancel as OciCancel, VncSession as OciSession};
use crate::openstack::Kind as OsKind;
use crate::vnc::keymap::{self, Layout};
use crate::vnc::{Backend, ConnectError, Input, Key, Options, Rect, ScreenLayout, Update, Vnc};

/// Pending pixel data above which the session thread waits for the dashboard.
const QUEUE_LIMIT: usize = 32 << 20;
/// A decoded JPEG, like every rectangle, may not need more than this.
pub const PIXEL_LIMIT: usize = 64 << 20;
/// Cost charged for an event that carries no pixels.
const EVENT_COST: usize = 64;
/// Pending events looked at when merging a new rectangle into the queue.
const MERGE_WINDOW: usize = 64;
/// Pending events (of any kind) above which the session thread waits for the
/// dashboard. Status and layout events are merged into one each, and the
/// end of the session always fits, so a server cannot grow the queue.
const QUEUE_EVENTS: usize = 1024;
/// Keystrokes and pointer events waiting to be sent to the remote desktop.
const INPUT_QUEUE: usize = 512;
/// Key and button releases that did not fit in the input queue. Past this
/// the session ends: a release must never be lost.
const PENDING_RELEASES: usize = 64;
/// Shortest time between two incremental update requests.
const REFRESH_GAP: Duration = Duration::from_millis(20);
/// Silence after which a request is sent in case an update was lost.
const WATCHDOG: Duration = Duration::from_secs(2);
/// Time given to the backend to close its connection.
const CLOSE_GRACE: Duration = Duration::from_secs(2);
/// Longest message shown from the remote or the system.
const MESSAGE_MAX: usize = 160;

/// What the remote desktop did, as far as the dashboard cares.
pub enum Event {
    /// The remote framebuffer has this size.
    Resolution(u16, u16),
    Bgra(Rect, Vec<u8>),
    /// A decoded JPEG: three bytes per pixel, in RGB order.
    Rgb(Rect, Vec<u8>),
    Copy {
        dst: Rect,
        from: (u16, u16),
    },
    Layout(ScreenLayout),
    /// The server takes keys by scancode.
    Scancodes,
    /// Progress, for the status line.
    Status(String),
    /// The session is over, with the reason when it was a failure.
    Ended(Option<String>),
}

impl Event {
    fn cost(&self) -> usize {
        match self {
            Event::Bgra(_, data) | Event::Rgb(_, data) => data.len() + EVENT_COST,
            _ => EVENT_COST,
        }
    }

    fn pixels(&self) -> Option<Rect> {
        match self {
            Event::Bgra(r, _) | Event::Rgb(r, _) => Some(*r),
            _ => None,
        }
    }
}

fn covers(outer: Rect, inner: Rect) -> bool {
    let (ox, oy) = (u32::from(outer.x), u32::from(outer.y));
    let (ix, iy) = (u32::from(inner.x), u32::from(inner.y));
    ix >= ox
        && iy >= oy
        && ix + u32::from(inner.width) <= ox + u32::from(outer.width)
        && iy + u32::from(inner.height) <= oy + u32::from(outer.height)
}

/// Events waiting for the dashboard, with the bytes they hold.
#[derive(Default)]
struct Queue {
    events: VecDeque<Event>,
    bytes: usize,
}

impl Queue {
    fn push(&mut self, event: Event) {
        match &event {
            // Only the latest of these is of any use.
            Event::Layout(..) => self.events.retain(|e| !matches!(e, Event::Layout(..))),
            Event::Status(..) => self.events.retain(|e| !matches!(e, Event::Status(..))),
            // A new size supersedes every pixel that came before it.
            Event::Resolution(..) => {
                self.events.retain(|e| {
                    !matches!(e, Event::Resolution(..) | Event::Copy { .. }) && e.pixels().is_none()
                });
            }
            // Pixels that a newer rectangle paints over are never seen. A copy
            // reads the framebuffer, so nothing before one may be dropped.
            _ => {
                if let Some(rect) = event.pixels() {
                    let mut i = self.events.len();
                    let stop = i.saturating_sub(MERGE_WINDOW);
                    while i > stop {
                        i -= 1;
                        match &self.events[i] {
                            Event::Resolution(..) | Event::Copy { .. } => break,
                            e if e.pixels().is_some_and(|old| covers(rect, old)) => {
                                self.events.remove(i);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        self.events.push_back(event);
        self.bytes = self.events.iter().map(Event::cost).sum();
    }

    fn drain(&mut self, max_events: usize, max_bytes: usize) -> Vec<Event> {
        let mut out = Vec::new();
        let mut taken = 0;
        while out.len() < max_events {
            let Some(next) = self.events.front() else {
                break;
            };
            let cost = next.cost();
            if !out.is_empty() && taken + cost > max_bytes {
                break;
            }
            taken += cost;
            out.extend(self.events.pop_front());
        }
        self.bytes = self.events.iter().map(Event::cost).sum();
        out
    }
}

struct Shared {
    queue: Mutex<Queue>,
    drained: Notify,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queue an event, waiting while the dashboard is too far behind.
    async fn push(&self, event: Event) {
        let mut event = Some(event);
        loop {
            {
                let mut queue = self.lock();
                let always = event.as_ref().is_some_and(|e| {
                    matches!(e, Event::Ended(..) | Event::Status(..) | Event::Layout(..))
                });
                if always || (queue.bytes < QUEUE_LIMIT && queue.events.len() < QUEUE_EVENTS) {
                    queue.push(event.take().expect("pushed once"));
                    return;
                }
            }
            // One producer (the session task) ever waits here, and `drain`
            // wakes it with `notify_one`, which keeps a permit when nobody is
            // waiting yet: a drain between the check above and this await is
            // not lost.
            self.drained.notified().await;
        }
    }

    fn status(&self, text: impl Into<String>) -> Event {
        Event::Status(text.into())
    }
}

struct Stop {
    flag: AtomicBool,
    notify: Notify,
}

impl Stop {
    fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    async fn wait(&self) {
        while !self.flag.load(Ordering::SeqCst) {
            self.notify.notified().await;
        }
    }
}

/// A connection to one VNC endpoint, running on its own thread.
pub struct Session {
    shared: Arc<Shared>,
    input: mpsc::Sender<Input>,
    stop: Arc<Stop>,
    /// Releases that did not fit in the input queue, sent as soon as it has
    /// room. Until then nothing else is sent, so the order is kept.
    pending: Mutex<Vec<Input>>,
}

impl Session {
    /// Start connecting. Never blocks; failures arrive as [`Event::Ended`].
    pub fn start(endpoint: VncEndpoint) -> Self {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            drained: Notify::new(),
        });
        let stop = Arc::new(Stop {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let (input, rx) = mpsc::channel(INPUT_QUEUE);
        let thread = {
            let (shared, stop) = (shared.clone(), stop.clone());
            std::thread::Builder::new()
                .name("lsu-vnc".into())
                .spawn(move || thread_main(endpoint, shared, rx, stop))
        };
        if let Err(e) = thread {
            shared.lock().push(Event::Ended(Some(clean(
                &format!("cannot start the VNC session: {e}"),
                None,
            ))));
        }
        Self {
            shared,
            input,
            stop,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Events that arrived since the last call: at most `max_events`, and
    /// about `max_bytes` of pixels.
    pub fn drain(&self, max_events: usize, max_bytes: usize) -> Vec<Event> {
        self.flush_pending();
        let events = self.shared.lock().drain(max_events, max_bytes);
        if !events.is_empty() {
            self.shared.drained.notify_one();
        }
        events
    }

    /// Send something to the remote desktop; `false` when it was dropped
    /// because the session is not keeping up. Releases go through
    /// [`Session::release`].
    pub fn send(&self, input: Input) -> bool {
        self.flush_pending();
        if !self.pending_lock().is_empty() {
            return false;
        }
        self.input.try_send(input).is_ok()
    }

    /// Send a key release or the release of a pointer button. It is never
    /// dropped: if the queue is full it waits for room, and if that does not
    /// come the session is ended (a held key or button on the remote
    /// desktop is worse than a reconnect).
    pub fn release(&self, input: Input) {
        self.flush_pending();
        let mut pending = self.pending_lock();
        if pending.is_empty() && self.input.try_send(input.clone()).is_ok() {
            return;
        }
        // A later pointer position supersedes an earlier one.
        if matches!(input, Input::Pointer(..)) {
            pending.retain(|i| !matches!(i, Input::Pointer(..)));
        }
        if pending.len() >= PENDING_RELEASES {
            drop(pending);
            self.abort("the keyboard could not be sent: the connection is too slow");
            return;
        }
        pending.push(input);
    }

    /// Send a key, as key presses and releases.
    pub fn send_keys(&self, keys: &[Key]) {
        for key in keys {
            if key.down {
                self.send(Input::Key(*key));
            } else {
                self.release(Input::Key(*key));
            }
        }
    }

    fn pending_lock(&self) -> std::sync::MutexGuard<'_, Vec<Input>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Send the releases that were waiting for room, in order.
    fn flush_pending(&self) {
        let mut pending = self.pending_lock();
        while let Some(first) = pending.first() {
            if self.input.try_send(first.clone()).is_err() {
                break;
            }
            pending.remove(0);
        }
    }

    /// End the session at once, with a reason for the pane.
    fn abort(&self, reason: &str) {
        self.shared
            .lock()
            .push(Event::Ended(Some(reason.to_string())));
        self.stop.trigger();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.trigger();
        self.shared.drained.notify_one();
    }
}

fn thread_main(
    endpoint: VncEndpoint,
    shared: Arc<Shared>,
    rx: mpsc::Receiver<Input>,
    stop: Arc<Stop>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            shared.lock().push(Event::Ended(Some(clean(
                &format!("cannot start the VNC session: {e}"),
                None,
            ))));
            return;
        }
    };
    runtime.block_on(run(endpoint, &shared, rx, &stop));
}

async fn run(endpoint: VncEndpoint, shared: &Shared, mut rx: mpsc::Receiver<Input>, stop: &Stop) {
    // The password is read here, not by the dashboard, so that a restart reads
    // the file again, and so that it is never kept anywhere else.
    let password = match crate::console::read_vnc_password(&endpoint) {
        Ok(p) => p,
        Err(e) => return finish(shared, Some(format!("{e:#}")), None).await,
    };
    let secret = password.as_deref().map(String::as_str);
    let options = Options {
        quality: endpoint.quality,
        compression: endpoint.compression,
        local_cursor: false,
        clipboard: false,
    };
    // How the session ended, when the transport knows better than the
    // protocol does (a WebSocket close, a failed handshake).
    let mut transport_outcome = None;
    // The OCI tunnel lives as long as the viewer; it is dropped (which ends
    // the tunnel and deletes the console connection) after the backend closed.
    let mut oci_session = OciHold::default();
    let connect = async {
        shared
            .push(shared.status(format!("connecting to {}", endpoint.address())))
            .await;
        match &endpoint.source {
            VncSource::Direct(direct) => {
                let addr = direct.resolve().await.map_err(|e| format!("{e:#}"))?;
                let vnc = Vnc::new(addr, password.as_deref().cloned(), options);
                vnc.connect().await.map_err(connect_error)
            }
            VncSource::OpenStack(console) => {
                // A fresh token every time; the client and the WebSocket
                // handshake are cancelled by dropping this future.
                let established = crate::openstack::open(&console.options(), OsKind::Vnc)
                    .await
                    .map_err(|e| e.to_string())?;
                transport_outcome = Some(established.outcome.clone());
                let vnc = Vnc::new(String::new(), password.as_deref().cloned(), options);
                vnc.connect_stream(established.stream)
                    .await
                    .map_err(connect_error)
            }
            VncSource::Oci(console) => {
                let session = open_oci(console).await?;
                shared.push(shared.status("console tunnel is up")).await;
                let session = oci_session.insert(session);
                // The console client: it takes the service's one slot.
                let stream = session.connect().map_err(|e| format!("{e}"))?;
                stream.set_nonblocking(true).map_err(|e| format!("{e}"))?;
                let stream =
                    tokio::net::UnixStream::from_std(stream).map_err(|e| format!("{e}"))?;
                let vnc = Vnc::new(String::new(), password.as_deref().cloned(), options);
                match vnc.connect_stream(stream).await {
                    Ok(b) => Ok(b),
                    Err(e @ ConnectError::NeedsPassword) => Err(connect_error(e)),
                    Err(e) => Err(oci_reason(session, connect_error(e)).await),
                }
            }
        }
    };
    let backend = tokio::select! {
        () = stop.wait() => return,
        b = connect => b,
    };
    let backend = match backend {
        Ok(b) => b,
        Err(e) => return finish(shared, Some(e), secret).await,
    };
    shared.push(shared.status("connected")).await;
    let outcome = tokio::select! {
        () = stop.wait() => None,
        r = pump(&backend, shared, &mut rx) => Some(r),
    };
    let _ = tokio::time::timeout(CLOSE_GRACE, backend.close()).await;
    if let Some(mut reason) = outcome {
        // The proxy's own account of why it hung up beats "connection lost".
        if let Some(Err(why)) = transport_outcome.as_ref().and_then(|o| o.get()) {
            reason = Some(why);
        }
        if let (Some(session), Some(r)) = (oci_session.get(), reason.take()) {
            reason = Some(oci_reason(session, r).await);
        }
        // Free the console slot before the pane says it is over.
        oci_session.release();
        finish(shared, reason, secret).await;
    }
}

/// OCI sessions that are open or still being taken down.
static OCI_LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How long [`wait_oci_closed`] waits for a session's tunnel directory to go.
const OCI_CLOSE_WAIT: Duration = Duration::from_secs(3);

/// The viewer's OCI session. Releasing it (explicitly, or when the viewer
/// thread ends by any path) ends the tunnel and removes its socket directory
/// before returning, within [`OCI_CLOSE_WAIT`]; the connection itself is
/// deleted by the helper, in the background.
#[derive(Default)]
struct OciHold(Option<OciSession>);

impl OciHold {
    fn insert(&mut self, session: OciSession) -> &mut OciSession {
        OCI_LIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.0.insert(session)
    }

    fn get(&self) -> Option<&OciSession> {
        self.0.as_ref()
    }

    fn release(&mut self) {
        let Some(session) = self.0.take() else {
            return;
        };
        let dir = session
            .socket_path()
            .and_then(|p| p.parent())
            .map(std::path::Path::to_path_buf);
        // Stops the tunnel, then has the connection deleted, on a thread of
        // its own; the directory goes with the tunnel.
        drop(session);
        if let Some(dir) = dir {
            let end = std::time::Instant::now() + OCI_CLOSE_WAIT;
            while dir.exists() && std::time::Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        OCI_LIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for OciHold {
    fn drop(&mut self) {
        self.release();
    }
}

/// At quit: wait (a few seconds at most) until the viewer threads have taken
/// their OCI tunnels down, so no socket directory is left behind.
pub fn wait_oci_closed(limit: Duration) {
    let end = std::time::Instant::now() + limit;
    while OCI_LIVE.load(std::sync::atomic::Ordering::SeqCst) > 0 && std::time::Instant::now() < end
    {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Open the OCI console on a thread of its own (it blocks for tens of
/// seconds, running the `oci` client). Dropping this future cancels it, and
/// whatever it had created is cleaned up.
async fn open_oci(console: &OciConsole) -> Result<OciSession, String> {
    struct CancelOnDrop(OciCancel);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let cancel = OciCancel::new();
    let _guard = CancelOnDrop(cancel.clone());
    let options = console.options();
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("lsu-oci-open".into())
        .spawn(move || {
            // A receiver that is gone drops the session, which cleans up.
            let _ = tx.send(crate::oci::vnc::open(&options, &cancel));
        })
        .map_err(|e| format!("cannot start the OCI console: {e}"))?;
    match rx.await {
        Ok(Ok(session)) => Ok(session),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(_) => Err("the OCI console helper stopped unexpectedly".into()),
    }
}

/// Why an OCI console session failed: the category of what `ssh` complained
/// about when there is one (never its text), else the given reason.
async fn oci_reason(session: &OciSession, reason: String) -> String {
    // The tunnel's complaint is read by another thread; let it arrive.
    tokio::time::sleep(Duration::from_millis(300)).await;
    match session.diagnosis() {
        Some(d) => d.message().to_string(),
        None => reason,
    }
}

fn connect_error(e: ConnectError) -> String {
    match e {
        ConnectError::NeedsPassword => {
            "the server asks for a password (set passwordFile)".to_string()
        }
        ConnectError::Failed(e) => format!("{e:#}"),
    }
}

async fn finish(shared: &Shared, reason: Option<String>, secret: Option<&str>) {
    let reason = reason.map(|r| clean(&r, secret));
    shared.push(Event::Ended(reason)).await;
}

/// Run the connection until it fails; the reason is `None` for a clean end.
async fn pump(
    backend: &impl Backend,
    shared: &Shared,
    rx: &mut mpsc::Receiver<Input>,
) -> Option<String> {
    let mut pushing = false;
    let mut refresh_at: Option<Instant> = None;
    let mut idle_at = Instant::now() + WATCHDOG;
    let mut idle_full = true;
    let mut resolution: Option<(u16, u16)> = None;
    loop {
        let timer = if pushing {
            None
        } else {
            Some(refresh_at.unwrap_or(idle_at))
        };
        tokio::select! {
            input = rx.recv() => match input {
                Some(input) => {
                    if backend.send(input).await.is_err() {
                        return Some("the connection was lost".into());
                    }
                }
                // The dashboard let go of the session.
                None => return None,
            },
            update = backend.recv() => {
                idle_at = Instant::now() + WATCHDOG;
                idle_full = true;
                match update {
                    Err(e) => return Some(format!("{e:#}")),
                    Ok(Update::Error(e)) => return Some(e),
                    Ok(Update::Resolution(s)) => {
                        shared.push(Event::Resolution(s.width, s.height)).await;
                        // After the desktop changed size what the dashboard holds
                        // is undefined (the pixels it had are dropped): ask for
                        // the whole picture, not just what changes next.
                        let size = (s.width, s.height);
                        let changed = resolution.is_some_and(|old| old != size);
                        resolution = Some(size);
                        if changed
                            && backend.send(Input::Refresh { incremental: false }).await.is_err()
                        {
                            return Some("the connection was lost".into());
                        }
                    }
                    Ok(Update::Bgra(r, data)) => shared.push(Event::Bgra(r, data)).await,
                    Ok(Update::Jpeg(r, data)) => {
                        match decode_jpeg(r, &data) {
                            Some((rect, rgb)) => shared.push(Event::Rgb(rect, rgb)).await,
                            // Say so (it replaces any earlier note): a desktop
                            // that only sends tiles that are refused would
                            // otherwise just stay blank.
                            None => {
                                shared
                                    .push(Event::Status(
                                        "a JPEG tile from the desktop could not be decoded".into(),
                                    ))
                                    .await
                            }
                        }
                    }
                    Ok(Update::Copy { dst, from }) => shared.push(Event::Copy { dst, from }).await,
                    Ok(Update::Layout(layout)) => shared.push(Event::Layout(layout)).await,
                    Ok(Update::Scancodes) => shared.push(Event::Scancodes).await,
                    Ok(Update::FrameEnd) => refresh_at = Some(Instant::now() + REFRESH_GAP),
                    Ok(Update::Pushing(p)) => pushing = p,
                    // Clipboard, cursor shape, lock keys, bell and latency
                    // probes are not used by the dashboard.
                    Ok(_) => {}
                }
            }
            () = async {
                match timer {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            } => {
                let incremental = refresh_at.take().is_some() || !idle_full;
                idle_full = false;
                idle_at = Instant::now() + Duration::from_secs(5);
                if backend.send(Input::Refresh { incremental }).await.is_err() {
                    return Some("the connection was lost".into());
                }
            }
        }
    }
}

/// A message safe to show: no secret, no control characters (a remote could
/// send escape sequences), and not too long.
fn clean(message: &str, secret: Option<&str>) -> String {
    let mut text = match secret {
        Some(s) if !s.is_empty() => message.replace(s, "***"),
        _ => message.to_string(),
    };
    text = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MESSAGE_MAX)
        .collect();
    text
}

/// Width and height from the first start-of-frame marker of a JPEG, without
/// decoding anything. `None` for anything that is not a plausible JPEG.
fn jpeg_size(data: &[u8]) -> Option<(u16, u16)> {
    if data.get(..2)? != [0xff, 0xd8] {
        return None;
    }
    let mut i = 2;
    while i + 4 <= data.len() {
        if data[i] != 0xff {
            return None;
        }
        let marker = data[i + 1];
        match marker {
            // Fill bytes.
            0xff => i += 1,
            // Markers without a payload.
            0x01 | 0xd0..=0xd7 => i += 2,
            // Start of frame (not DHT, JPG or DAC): precision, height, width.
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                let frame = data.get(i + 4..i + 9)?;
                let height = u16::from_be_bytes([frame[1], frame[2]]);
                let width = u16::from_be_bytes([frame[3], frame[4]]);
                return (width > 0 && height > 0).then_some((width, height));
            }
            // Start of scan or end of image before any frame.
            0xd9 | 0xda => return None,
            _ => {
                let len = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
                if len < 2 {
                    return None;
                }
                i += 2 + len;
            }
        }
    }
    None
}

/// May a JPEG of this size be decoded into a rectangle of the advertised
/// size? The picture may not exceed the rectangle, and no rectangle may need
/// more than [`PIXEL_LIMIT`] bytes, whatever the server claims.
fn jpeg_fits(image: (u16, u16), advertised: Rect) -> bool {
    let bytes = |w: u16, h: u16| usize::from(w) * usize::from(h) * 3;
    image.0 <= advertised.width
        && image.1 <= advertised.height
        && bytes(advertised.width, advertised.height) <= PIXEL_LIMIT
        && bytes(image.0, image.1) <= PIXEL_LIMIT
}

/// Decode a JPEG rectangle to RGB. The size is checked against the advertised
/// rectangle and the limit before anything is allocated, and a picture that is
/// malformed is dropped, not a reason to stop the session.
fn decode_jpeg(advertised: Rect, data: &[u8]) -> Option<(Rect, Vec<u8>)> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let size = jpeg_size(data)?;
    if !jpeg_fits(size, advertised) {
        return None;
    }
    // The decoder is third-party code on data from the network: a panic in it
    // must not take the session down.
    catch_unwind(AssertUnwindSafe(|| {
        let options = DecoderOptions::default()
            .jpeg_set_out_colorspace(ColorSpace::RGB)
            .set_max_width(usize::from(advertised.width))
            .set_max_height(usize::from(advertised.height));
        let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
        decoder.decode_headers().ok()?;
        let info = decoder.info()?;
        if (info.width, info.height) != size {
            return None;
        }
        let len = decoder.output_buffer_size()?;
        if len != usize::from(size.0) * usize::from(size.1) * 3 || len > PIXEL_LIMIT {
            return None;
        }
        let mut rgb = vec![0u8; len];
        decoder.decode_into(&mut rgb).ok()?;
        Some((
            Rect {
                x: advertised.x,
                y: advertised.y,
                width: size.0,
                height: size.1,
            },
            rgb,
        ))
    }))
    .ok()
    .flatten()
}

// --- keys ---------------------------------------------------------------

const SHIFT_L: u32 = 0xffe1;
const CONTROL_L: u32 = 0xffe3;
const ALT_L: u32 = 0xffe9;

/// The keysym of a typed character: Latin-1 as it is, the rest as a Unicode
/// keysym (`0x01000000` plus the code point).
fn char_keysym(c: char) -> u32 {
    match u32::from(c) {
        cp @ (0x20..=0x7e | 0xa0..=0xff) => cp,
        0x7f => 0xffff,
        cp if cp < 0x20 => 0,
        cp => 0x0100_0000 + cp,
    }
}

fn named_keysym(code: KeyCode) -> u32 {
    match code {
        KeyCode::Enter => 0xff0d,
        KeyCode::Tab | KeyCode::BackTab => 0xff09,
        KeyCode::Backspace => 0xff08,
        KeyCode::Esc => 0xff1b,
        KeyCode::Up => 0xff52,
        KeyCode::Down => 0xff54,
        KeyCode::Left => 0xff51,
        KeyCode::Right => 0xff53,
        KeyCode::Home => 0xff50,
        KeyCode::End => 0xff57,
        KeyCode::PageUp => 0xff55,
        KeyCode::PageDown => 0xff56,
        KeyCode::Insert => 0xff63,
        KeyCode::Delete => 0xffff,
        KeyCode::F(n @ 1..=24) => 0xffbd + u32::from(n),
        _ => 0,
    }
}

const ISO_LEVEL3_SHIFT: u32 = 0xfe03;
const SPACE: u32 = 0x20;

/// XT scancodes (QEMU numbering) of the modifiers and the space bar.
const SC_CONTROL_L: u16 = 0x1d;
const SC_SHIFT_L: u16 = 0x2a;
const SC_ALT_L: u16 = 0x38;
const SC_ALTGR: u16 = 0xb8;
const SC_SPACE: u16 = 0x39;

/// The scancode of a key that types no character.
fn named_scancode(code: KeyCode) -> Option<u16> {
    Some(match code {
        KeyCode::Enter => 0x1c,
        KeyCode::Tab | KeyCode::BackTab => 0x0f,
        KeyCode::Backspace => 0x0e,
        KeyCode::Esc => 0x01,
        KeyCode::Up => 0xc8,
        KeyCode::Down => 0xd0,
        KeyCode::Left => 0xcb,
        KeyCode::Right => 0xcd,
        KeyCode::Home => 0xc7,
        KeyCode::End => 0xcf,
        KeyCode::PageUp => 0xc9,
        KeyCode::PageDown => 0xd1,
        KeyCode::Insert => 0xd2,
        KeyCode::Delete => 0xd3,
        KeyCode::F(n @ 1..=10) => 0x3a + u16::from(n),
        KeyCode::F(11) => 0x57,
        KeyCode::F(12) => 0x58,
        _ => return None,
    })
}

/// The key events for one key press: modifiers down, the key down and up,
/// modifiers up. Empty for a key that has no keysym.
///
/// With a guest `layout` (the server takes scancodes), the events carry the
/// scancodes of the keys that type the character in that layout, with Shift
/// or AltGr as it needs them, so the guest gets the right character whatever
/// keyboard layout the server assumes. A key the layout does not have goes by
/// keysym alone, as without a layout.
pub fn key_events(code: KeyCode, mods: KeyModifiers, layout: Option<Layout>) -> Vec<Key> {
    layout
        .and_then(|layout| scancode_events(code, mods, layout))
        .unwrap_or_else(|| keysym_events(code, mods))
}

fn scancode_events(code: KeyCode, mods: KeyModifiers, layout: Layout) -> Option<Vec<Key>> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let (keysym, stroke) = match code {
        KeyCode::Char(c) => {
            // Ctrl+letter is the letter's key; a shift that came with it is
            // the user's own (Ctrl+Shift+C).
            let c = if ctrl && c.is_ascii_alphabetic() {
                c.to_ascii_lowercase()
            } else {
                c
            };
            let mut stroke = keymap::stroke(layout, c)?;
            stroke.shift |= ctrl && mods.contains(KeyModifiers::SHIFT);
            (char_keysym(c), stroke)
        }
        other => (
            named_keysym(other),
            keymap::Stroke {
                scancode: named_scancode(other)?,
                shift: mods.contains(KeyModifiers::SHIFT) || other == KeyCode::BackTab,
                altgr: false,
                dead: false,
            },
        ),
    };
    if keysym == 0 {
        return None;
    }
    let key = |keysym, scancode, down| Key {
        keysym,
        down,
        scancode: Some(scancode),
    };
    let held: Vec<(u32, u16)> = [
        (ctrl, CONTROL_L, SC_CONTROL_L),
        (alt, ALT_L, SC_ALT_L),
        (stroke.shift, SHIFT_L, SC_SHIFT_L),
        (stroke.altgr, ISO_LEVEL3_SHIFT, SC_ALTGR),
    ]
    .into_iter()
    .filter(|(on, ..)| *on)
    .map(|(_, sym, sc)| (sym, sc))
    .collect();
    let mut out: Vec<Key> = held.iter().map(|&(sym, sc)| key(sym, sc, true)).collect();
    out.push(key(keysym, stroke.scancode, true));
    out.push(key(keysym, stroke.scancode, false));
    out.extend(held.iter().rev().map(|&(sym, sc)| key(sym, sc, false)));
    if stroke.dead {
        // The guest waits for the next key; a space gives the character.
        out.push(key(SPACE, SC_SPACE, true));
        out.push(key(SPACE, SC_SPACE, false));
    }
    Some(out)
}

fn keysym_events(code: KeyCode, mods: KeyModifiers) -> Vec<Key> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let mut shift = mods.contains(KeyModifiers::SHIFT) || code == KeyCode::BackTab;
    let keysym = match code {
        KeyCode::Char(c) => {
            // A typed character already carries its shift; the modifier is
            // only sent on its own with Ctrl or Alt, where it changes what the
            // remote does.
            shift = shift && (ctrl || alt);
            let c = if ctrl && (shift || c.is_ascii_uppercase()) {
                c.to_ascii_lowercase()
            } else {
                c
            };
            char_keysym(c)
        }
        other => named_keysym(other),
    };
    if keysym == 0 {
        return Vec::new();
    }
    let mut held = Vec::new();
    if ctrl {
        held.push(CONTROL_L);
    }
    if alt {
        held.push(ALT_L);
    }
    if shift {
        held.push(SHIFT_L);
    }
    let mut out: Vec<Key> = held
        .iter()
        .map(|&keysym| Key {
            keysym,
            down: true,
            scancode: None,
        })
        .collect();
    out.push(Key {
        keysym,
        down: true,
        scancode: None,
    });
    out.push(Key {
        keysym,
        down: false,
        scancode: None,
    });
    out.extend(held.iter().rev().map(|&keysym| Key {
        keysym,
        down: false,
        scancode: None,
    }));
    out
}

// --- terminal context -----------------------------------------------------

/// Is a tmux context something a process running as root may use? The
/// terminal graphics code asks tmux (`tmux display-message`, `show-option`)
/// through `TMUX` and `TMUX_PANE`; as root, those variables come from the
/// user who started sudo, so the socket must belong to that user (or to
/// root), and the pane must look like a pane id.
pub fn tmux_context_trusted(
    tmux: &str,
    pane: Option<&str>,
    socket_owner: Option<u32>,
    sudo_uid: Option<u32>,
) -> bool {
    let socket = tmux.split(',').next().unwrap_or_default();
    if !socket.starts_with('/') || socket.contains('\0') {
        return false;
    }
    let pane_ok = pane.is_none_or(|p| {
        p.strip_prefix('%')
            .is_some_and(|n| !n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit()))
    });
    pane_ok && socket_owner.is_some_and(|uid| uid == 0 || Some(uid) == sudo_uid)
}

/// Why this process should not draw graphics, or `None` when it may. Only
/// stricter than usual while elevated.
pub fn graphics_context_problem() -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let tmux = std::env::var("TMUX").ok().filter(|t| !t.is_empty())?;
    let sudo_uid = std::env::var("SUDO_UID").ok().and_then(|u| u.parse().ok());
    let socket = tmux.split(',').next().unwrap_or_default();
    let owner = std::fs::metadata(socket).ok().map(|m| m.uid());
    let pane = std::env::var("TMUX_PANE").ok();
    (!tmux_context_trusted(&tmux, pane.as_deref(), owner, sudo_uid))
        .then(|| "the tmux session is not owned by the user who started sudo".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    fn pixels(e: &Event) -> Option<Rect> {
        e.pixels()
    }

    fn scancodes(keys: &[Key]) -> Vec<(u16, bool)> {
        keys.iter()
            .map(|k| (k.scancode.expect("scancode"), k.down))
            .collect()
    }

    #[test]
    fn with_a_guest_layout_keys_go_by_scancode() {
        let de = Some(Layout::De);
        // QWERTZ: z is the key that is y on a US keyboard.
        let z = key_events(KeyCode::Char('z'), KeyModifiers::NONE, de);
        assert_eq!(scancodes(&z), [(0x15, true), (0x15, false)]);
        assert_eq!(z[0].keysym, 0x7a);
        // An upper-case letter holds Shift around the key.
        let y = key_events(KeyCode::Char('Y'), KeyModifiers::SHIFT, de);
        assert_eq!(
            scancodes(&y),
            [(0x2a, true), (0x2c, true), (0x2c, false), (0x2a, false)]
        );
        // @ is AltGr+Q on a German keyboard.
        let at = key_events(KeyCode::Char('@'), KeyModifiers::NONE, de);
        assert_eq!(
            scancodes(&at),
            [(0xb8, true), (0x10, true), (0x10, false), (0xb8, false)]
        );
        // Ctrl+C is Ctrl and the C key.
        let c = key_events(KeyCode::Char('c'), KeyModifiers::CONTROL, de);
        assert_eq!(
            scancodes(&c),
            [(0x1d, true), (0x2e, true), (0x2e, false), (0x1d, false)]
        );
        // A dead key is followed by a space, which gives the character.
        let caret = key_events(KeyCode::Char('^'), KeyModifiers::NONE, de);
        assert_eq!(
            scancodes(&caret),
            [(0x29, true), (0x29, false), (0x39, true), (0x39, false)]
        );
        // Named keys have their own scancodes.
        let up = key_events(KeyCode::Up, KeyModifiers::NONE, de);
        assert_eq!(scancodes(&up), [(0xc8, true), (0xc8, false)]);
        let us = key_events(KeyCode::Char('z'), KeyModifiers::NONE, Some(Layout::Us));
        assert_eq!(scancodes(&us), [(0x2c, true), (0x2c, false)]);
    }

    #[test]
    fn keys_a_layout_lacks_and_servers_without_scancodes_use_keysyms() {
        let e = key_events(KeyCode::Char('é'), KeyModifiers::NONE, Some(Layout::De));
        assert!(e.iter().all(|k| k.scancode.is_none()));
        assert_eq!(e[0].keysym, 0xe9);
        let z = key_events(KeyCode::Char('z'), KeyModifiers::NONE, None);
        assert!(z.iter().all(|k| k.scancode.is_none()));
    }

    #[test]
    fn typed_characters_are_sent_as_their_keysym() {
        let keys = key_events(KeyCode::Char('a'), KeyModifiers::NONE, None);
        assert_eq!(
            keys,
            vec![
                Key {
                    keysym: 0x61,
                    down: true,
                    scancode: None
                },
                Key {
                    keysym: 0x61,
                    down: false,
                    scancode: None
                }
            ]
        );
        // The shift of a typed character is already in the character.
        let keys = key_events(KeyCode::Char('A'), KeyModifiers::SHIFT, None);
        assert_eq!(
            keys.iter().map(|k| k.keysym).collect::<Vec<_>>(),
            [0x41, 0x41]
        );
        // Latin-1 as it is, anything else as a Unicode keysym.
        assert_eq!(
            key_events(KeyCode::Char('é'), KeyModifiers::NONE, None)[0].keysym,
            0xe9
        );
        assert_eq!(
            key_events(KeyCode::Char('€'), KeyModifiers::NONE, None)[0].keysym,
            0x0100_20ac
        );
    }

    #[test]
    fn modifiers_wrap_the_key() {
        let keys = key_events(KeyCode::Char('C'), KeyModifiers::CONTROL, None);
        let seq: Vec<(u32, bool)> = keys.iter().map(|k| (k.keysym, k.down)).collect();
        assert_eq!(
            seq,
            [(0xffe3, true), (0x63, true), (0x63, false), (0xffe3, false)]
        );
        let keys = key_events(
            KeyCode::Delete,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            None,
        );
        let seq: Vec<(u32, bool)> = keys.iter().map(|k| (k.keysym, k.down)).collect();
        assert_eq!(
            seq,
            [
                (0xffe3, true),
                (0xffe9, true),
                (0xffff, true),
                (0xffff, false),
                (0xffe9, false),
                (0xffe3, false)
            ]
        );
        // Shift is sent for keys that are not characters, and with Ctrl.
        let keys = key_events(KeyCode::BackTab, KeyModifiers::SHIFT, None);
        assert_eq!(keys.first().map(|k| k.keysym), Some(0xffe1));
        assert_eq!(keys[1].keysym, 0xff09);
        let keys = key_events(
            KeyCode::Char('T'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            None,
        );
        assert_eq!(keys.iter().filter(|k| k.keysym == 0xffe1).count(), 2);
        assert!(keys.iter().any(|k| k.keysym == 0x74));
    }

    #[test]
    fn named_keys_have_keysyms() {
        for (code, sym) in [
            (KeyCode::Enter, 0xff0d),
            (KeyCode::Esc, 0xff1b),
            (KeyCode::Backspace, 0xff08),
            (KeyCode::Up, 0xff52),
            (KeyCode::PageDown, 0xff56),
            (KeyCode::F(1), 0xffbe),
            (KeyCode::F(12), 0xffc9),
        ] {
            assert_eq!(
                key_events(code, KeyModifiers::NONE, None)[0].keysym,
                sym,
                "{code:?}"
            );
        }
        assert!(key_events(KeyCode::Null, KeyModifiers::NONE, None).is_empty());
        assert!(key_events(KeyCode::F(99), KeyModifiers::NONE, None).is_empty());
        assert!(key_events(KeyCode::Char('\u{1}'), KeyModifiers::NONE, None).is_empty());
    }

    #[test]
    fn newer_rectangles_replace_covered_ones() {
        let mut q = Queue::default();
        q.push(Event::Resolution(100, 100));
        q.push(Event::Bgra(rect(10, 10, 5, 5), vec![0; 100]));
        q.push(Event::Bgra(rect(0, 0, 50, 50), vec![0; 10_000]));
        let rects: Vec<_> = q.events.iter().filter_map(pixels).collect();
        assert_eq!(rects, [rect(0, 0, 50, 50)]);
        assert_eq!(q.bytes, 2 * EVENT_COST + 10_000);
        // A copy reads what is there: older pixels stay.
        q.push(Event::Copy {
            dst: rect(0, 0, 1, 1),
            from: (2, 2),
        });
        q.push(Event::Bgra(rect(0, 0, 60, 60), vec![0; 14_400]));
        assert_eq!(q.events.iter().filter_map(pixels).count(), 2);
        // A new size drops all pending pixels.
        q.push(Event::Resolution(200, 200));
        assert_eq!(q.events.len(), 1);
    }

    #[test]
    fn the_queue_hands_out_bounded_batches() {
        let mut q = Queue::default();
        for i in 0..10u16 {
            q.push(Event::Bgra(rect(i * 20, 0, 10, 10), vec![0; 400]));
        }
        let first = q.drain(100, 1000);
        // At least one event, then not past the byte budget.
        assert!(!first.is_empty() && first.len() < 10);
        let rest = q.drain(100, usize::MAX);
        assert_eq!(first.len() + rest.len(), 10);
        assert_eq!(q.bytes, 0);
        // Never an empty batch for a big event.
        q.push(Event::Bgra(rect(0, 0, 100, 100), vec![0; 40_000]));
        assert_eq!(q.drain(10, 10).len(), 1);
    }

    fn shared() -> Shared {
        Shared {
            queue: Mutex::new(Queue::default()),
            drained: Notify::new(),
        }
    }

    fn layout(w: u16) -> ScreenLayout {
        ScreenLayout {
            screen: (w, 10).into(),
            screens: Vec::new(),
            reason: 0,
            status: crate::vnc::ResizeStatus::Success,
        }
    }

    #[tokio::test]
    async fn a_full_queue_makes_the_session_wait_for_the_dashboard() {
        let shared = shared();
        shared.lock().bytes = QUEUE_LIMIT;
        let blocked = tokio::time::timeout(
            Duration::from_millis(50),
            shared.push(Event::Bgra(rect(0, 0, 1, 1), vec![0; 4])),
        )
        .await;
        assert!(blocked.is_err(), "the push did not wait");
        assert!(shared.lock().events.is_empty());
        shared.lock().bytes = 0;
        shared.push(Event::Bgra(rect(0, 0, 1, 1), vec![0; 4])).await;
        assert_eq!(shared.lock().events.len(), 1);
    }

    /// `drain` may land between the producer's capacity check and its await:
    /// the permit `notify_one` keeps wakes it anyway.
    #[tokio::test]
    async fn a_drain_before_the_await_still_wakes_the_producer() {
        let shared = shared();
        shared.lock().bytes = QUEUE_LIMIT;
        // The wake-up comes first (nobody is waiting yet)...
        shared.lock().bytes = 0;
        shared.drained.notify_one();
        // ...and a producer that then finds the queue full still gets it.
        shared.lock().bytes = QUEUE_LIMIT;
        let push = shared.push(Event::Bgra(rect(0, 0, 1, 1), vec![0; 4]));
        tokio::pin!(push);
        // First poll: full, consumes the stored permit, loops, parks again.
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut push)
                .await
                .is_err()
        );
        shared.lock().bytes = 0;
        shared.drained.notify_one();
        tokio::time::timeout(Duration::from_secs(2), push)
            .await
            .expect("the producer stayed blocked");
    }

    #[tokio::test]
    async fn the_queue_holds_a_bounded_number_of_events_whatever_the_server_sends() {
        let shared = shared();
        // Copies cannot be merged (each reads the framebuffer): the session
        // waits instead of letting them pile up.
        for i in 0..QUEUE_EVENTS {
            shared
                .push(Event::Copy {
                    dst: rect(0, 0, 1, 1),
                    from: (i as u16, 0),
                })
                .await;
        }
        let blocked = tokio::time::timeout(
            Duration::from_millis(50),
            shared.push(Event::Copy {
                dst: rect(0, 0, 1, 1),
                from: (0, 0),
            }),
        )
        .await;
        assert!(blocked.is_err(), "a copy was queued past the limit");
        // Status and layout keep one slot each (the latest), and the end of
        // the session always fits.
        for w in 0..100 {
            shared.push(Event::Layout(layout(w))).await;
            shared.push(Event::Status(format!("s{w}"))).await;
        }
        shared.push(Event::Ended(None)).await;
        {
            let q = shared.lock();
            assert_eq!(q.events.len(), QUEUE_EVENTS + 3);
            assert_eq!(
                q.events
                    .iter()
                    .filter(|e| matches!(e, Event::Layout(l) if l.screen.width == 99))
                    .count(),
                1
            );
            assert_eq!(
                q.events
                    .iter()
                    .filter(|e| matches!(e, Event::Status(s) if s == "s99"))
                    .count(),
                1
            );
        }
        // The dashboard takes events out, and the session goes on.
        let batch = shared.lock().drain(10, usize::MAX);
        assert_eq!(batch.len(), 10);
        shared
            .push(Event::Copy {
                dst: rect(0, 0, 1, 1),
                from: (1, 1),
            })
            .await;
    }

    fn endpoint_at(port: u16) -> VncEndpoint {
        VncEndpoint {
            source: crate::console::VncSource::Direct(crate::console::DirectVnc {
                host: "127.0.0.1".into(),
                port,
                allow_remote: false,
            }),
            password_file: None,
            view_only: false,
            quality: None,
            compression: None,
            resize_remote: false,
            keymap: None,
        }
    }

    /// A session whose input queue nobody empties: the sender is dropped on
    /// the receiving side only when the thread ends, so build the pieces by
    /// hand.
    fn stalled_session(capacity: usize) -> (Session, mpsc::Receiver<Input>) {
        let (input, rx) = mpsc::channel(capacity);
        let session = Session {
            shared: Arc::new(shared()),
            input,
            stop: Arc::new(Stop {
                flag: AtomicBool::new(false),
                notify: Notify::new(),
            }),
            pending: Mutex::new(Vec::new()),
        };
        (session, rx)
    }

    #[test]
    fn releases_are_never_dropped_when_the_input_queue_is_full() {
        let (session, mut rx) = stalled_session(2);
        let down = |keysym| Key {
            keysym,
            down: true,
            scancode: None,
        };
        let up = |keysym| Key {
            keysym,
            down: false,
            scancode: None,
        };
        assert!(session.send(Input::Key(down(1))));
        assert!(session.send(Input::Key(down(2))));
        // Full: presses are dropped, the release is kept.
        assert!(!session.send(Input::Key(down(3))));
        session.release(Input::Key(up(1)));
        assert!(
            !session.send(Input::Key(down(4))),
            "ordering: nothing passes a pending release"
        );
        // Room again: the release goes out before anything new.
        assert!(matches!(
            rx.try_recv().unwrap(),
            Input::Key(Key {
                keysym: 1,
                down: true,
                scancode: None
            })
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Input::Key(Key {
                keysym: 2,
                down: true,
                scancode: None
            })
        ));
        assert!(session.send(Input::Key(down(5))));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Input::Key(Key {
                keysym: 1,
                down: false,
                scancode: None
            })
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            Input::Key(Key {
                keysym: 5,
                down: true,
                scancode: None
            })
        ));
        assert!(session.pending_lock().is_empty());
    }

    #[test]
    fn a_session_that_cannot_take_releases_ends_instead_of_leaving_keys_held() {
        let (session, _rx) = stalled_session(1);
        assert!(session.send(Input::Key(Key {
            keysym: 1,
            down: true,
            scancode: None
        })));
        for i in 0..=PENDING_RELEASES {
            session.release(Input::Key(Key {
                keysym: 100 + i as u32,
                down: false,
                scancode: None,
            }));
        }
        assert!(session.stop.flag.load(Ordering::SeqCst));
        let events = session.shared.lock().drain(10, usize::MAX);
        assert!(matches!(events.last(), Some(Event::Ended(Some(_)))));
    }

    #[test]
    fn only_the_latest_pointer_release_is_kept() {
        let (session, _rx) = stalled_session(1);
        assert!(session.send(Input::Key(Key {
            keysym: 1,
            down: true,
            scancode: None
        })));
        for x in 0..10 {
            session.release(Input::Pointer(crate::vnc::Pointer {
                x,
                y: 0,
                buttons: 0,
            }));
        }
        let pending = session.pending_lock();
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            pending[0],
            Input::Pointer(crate::vnc::Pointer { x: 9, .. })
        ));
    }

    /// Serve one RFB 3.8 session without authentication: a raw update after
    /// the first update request, then collect the key events the client sends.
    fn fake_server(listener: std::net::TcpListener) -> std::sync::mpsc::Receiver<(bool, u32)> {
        use std::io::{Read, Write};
        let (keys_tx, keys_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut s, _)) = listener.accept() else {
                return;
            };
            s.set_read_timeout(Some(Duration::from_secs(10))).ok();
            let skip = |s: &mut std::net::TcpStream, n: usize| {
                let mut buf = vec![0u8; n];
                s.read_exact(&mut buf).map(|()| buf)
            };
            let run = |s: &mut std::net::TcpStream| -> std::io::Result<()> {
                s.write_all(b"RFB 003.008\n")?;
                skip(s, 12)?;
                s.write_all(&[1, 1])?;
                skip(s, 1)?;
                s.write_all(&0u32.to_be_bytes())?;
                skip(s, 1)?;
                let mut init = Vec::new();
                init.extend(8u16.to_be_bytes());
                init.extend(4u16.to_be_bytes());
                // 32 bits per pixel, depth 24, little endian, true colour.
                init.extend([32, 24, 0, 1]);
                init.extend(255u16.to_be_bytes());
                init.extend(255u16.to_be_bytes());
                init.extend(255u16.to_be_bytes());
                init.extend([16, 8, 0, 0, 0, 0]);
                init.extend(4u32.to_be_bytes());
                init.extend(b"test");
                s.write_all(&init)?;
                let mut sent = false;
                loop {
                    let kind = skip(s, 1)?[0];
                    match kind {
                        0 => {
                            skip(s, 19)?;
                        }
                        2 => {
                            let head = skip(s, 3)?;
                            let n = usize::from(u16::from_be_bytes([head[1], head[2]]));
                            skip(s, 4 * n)?;
                        }
                        3 => {
                            skip(s, 9)?;
                            if !sent {
                                sent = true;
                                let mut update = vec![0, 0, 0, 1];
                                update.extend(0u16.to_be_bytes());
                                update.extend(0u16.to_be_bytes());
                                update.extend(8u16.to_be_bytes());
                                update.extend(4u16.to_be_bytes());
                                update.extend(0i32.to_be_bytes());
                                update.extend(std::iter::repeat_n(0x80u8, 8 * 4 * 4));
                                s.write_all(&update)?;
                            }
                        }
                        4 => {
                            let k = skip(s, 7)?;
                            let _ = keys_tx
                                .send((k[0] != 0, u32::from_be_bytes([k[3], k[4], k[5], k[6]])));
                        }
                        5 => {
                            skip(s, 5)?;
                        }
                        6 => {
                            let head = skip(s, 7)?;
                            let n = u32::from_be_bytes([head[3], head[4], head[5], head[6]]);
                            skip(s, n as usize)?;
                        }
                        _ => return Ok(()),
                    }
                }
            };
            let _ = run(&mut s);
        });
        keys_rx
    }

    #[test]
    fn a_loopback_session_shows_the_first_frame_and_sends_keys() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let keys = fake_server(listener);
        let session = Session::start(endpoint_at(port));
        let end = std::time::Instant::now() + Duration::from_secs(10);
        let mut frame = None;
        let mut size = None;
        while frame.is_none() && std::time::Instant::now() < end {
            for event in session.drain(16, usize::MAX) {
                match event {
                    Event::Resolution(w, h) => size = Some((w, h)),
                    Event::Bgra(r, data) => frame = Some((r, data.len())),
                    Event::Ended(reason) => panic!("the session ended: {reason:?}"),
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (r, len) = frame.expect("no frame arrived");
        assert_eq!(r, rect(0, 0, 8, 4));
        assert_eq!(len, 8 * 4 * 4);
        assert!(size.is_none() || size == Some((8, 4)));
        // A key press reaches the server as a press and a release.
        session.send_keys(&key_events(KeyCode::Char('a'), KeyModifiers::NONE, None));
        assert_eq!(keys.recv_timeout(Duration::from_secs(10)), Ok((true, 0x61)));
        assert_eq!(
            keys.recv_timeout(Duration::from_secs(10)),
            Ok((false, 0x61))
        );
        let start = std::time::Instant::now();
        drop(session);
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn messages_are_cleaned() {
        assert_eq!(clean("a\u{1b}[31mb\nc", None), "a [31mb c");
        assert_eq!(
            clean("bad password hunter2", Some("hunter2")),
            "bad password ***"
        );
        assert_eq!(clean(&"x".repeat(1000), None).len(), MESSAGE_MAX);
    }

    /// A JPEG header: SOI, then a frame header of the given size.
    fn jpeg_header(w: u16, h: u16) -> Vec<u8> {
        let mut v = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00];
        v.extend([0xff, 0xc0, 0x00, 0x0b, 8]);
        v.extend(h.to_be_bytes());
        v.extend(w.to_be_bytes());
        v.extend([1, 1, 0x11, 0]);
        v
    }

    #[test]
    fn jpeg_sizes_come_from_the_frame_header() {
        assert_eq!(jpeg_size(&jpeg_header(640, 480)), Some((640, 480)));
        assert_eq!(jpeg_size(&jpeg_header(0, 480)), None);
        assert_eq!(jpeg_size(&jpeg_header(640, 0)), None);
        assert_eq!(jpeg_size(b"not a jpeg"), None);
        assert_eq!(jpeg_size(&jpeg_header(10, 10)[..12]), None);
        assert_eq!(jpeg_size(&[0xff, 0xd8, 0xff, 0xda, 0, 2]), None);
    }

    #[test]
    fn jpeg_dimensions_are_checked_before_decoding() {
        let r = rect(5, 6, 100, 50);
        assert!(jpeg_fits((100, 50), r));
        assert!(jpeg_fits((10, 10), r));
        assert!(!jpeg_fits((101, 50), r));
        assert!(!jpeg_fits((100, 51), r));
        // A huge advertised rectangle does not make a huge allocation.
        assert!(!jpeg_fits((10, 10), rect(0, 0, u16::MAX, u16::MAX)));
        assert!(decode_jpeg(r, &jpeg_header(200, 200)).is_none());
        assert!(decode_jpeg(r, &jpeg_header(0, 0)).is_none());
    }

    #[test]
    fn malformed_jpegs_are_errors_not_panics() {
        let r = rect(0, 0, 64, 64);
        let mut truncated = jpeg_header(32, 32);
        truncated.extend([0xff, 0xda, 0x00, 0x08, 1, 1, 0, 0, 0x3f, 0, 0x12, 0x34]);
        assert!(decode_jpeg(r, &truncated).is_none());
        assert!(decode_jpeg(r, &[]).is_none());
        assert!(decode_jpeg(r, &[0xff; 64]).is_none());
        let mut noise = vec![0xff, 0xd8];
        noise.extend((0..2000u32).map(|i| (i * 31 % 251) as u8));
        assert!(decode_jpeg(r, &noise).is_none());
    }

    #[test]
    fn root_trusts_only_the_sudo_users_tmux() {
        let t = "/tmp/tmux-1000/default,1234,0";
        assert!(tmux_context_trusted(t, Some("%3"), Some(1000), Some(1000)));
        assert!(tmux_context_trusted(t, None, Some(0), Some(1000)));
        // Another user's server, a missing socket, or no sudo user.
        assert!(!tmux_context_trusted(t, Some("%3"), Some(1001), Some(1000)));
        assert!(!tmux_context_trusted(t, Some("%3"), None, Some(1000)));
        assert!(!tmux_context_trusted(t, Some("%3"), Some(1000), None));
        // A relative socket path, or a pane that is not a pane id.
        assert!(!tmux_context_trusted(
            "tmux/default,1,0",
            None,
            Some(0),
            None
        ));
        assert!(!tmux_context_trusted(
            t,
            Some("%3;rm"),
            Some(1000),
            Some(1000)
        ));
        assert!(!tmux_context_trusted(t, Some("3"), Some(1000), Some(1000)));
        assert!(!tmux_context_trusted(t, Some("%"), Some(1000), Some(1000)));
    }

    #[test]
    fn a_session_that_cannot_connect_ends_and_drops_quickly() {
        // A loopback port nobody listens on.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let endpoint = VncEndpoint {
            source: crate::console::VncSource::Direct(crate::console::DirectVnc {
                host: "127.0.0.1".into(),
                port,
                allow_remote: false,
            }),
            password_file: None,
            view_only: false,
            quality: None,
            compression: None,
            resize_remote: false,
            keymap: None,
        };
        let session = Session::start(endpoint);
        let end = std::time::Instant::now() + Duration::from_secs(10);
        let mut reason = None;
        'wait: while std::time::Instant::now() < end {
            for event in session.drain(16, usize::MAX) {
                if let Event::Ended(r) = event {
                    reason = Some(r);
                    break 'wait;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(matches!(reason, Some(Some(_))), "{reason:?}");
        let start = std::time::Instant::now();
        drop(session);
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn dropping_a_session_stops_a_pending_connection() {
        // A listener that never answers the RFB handshake.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = VncEndpoint {
            source: crate::console::VncSource::Direct(crate::console::DirectVnc {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
                allow_remote: false,
            }),
            password_file: None,
            view_only: false,
            quality: None,
            compression: None,
            resize_remote: false,
            keymap: None,
        };
        let session = Session::start(endpoint);
        std::thread::sleep(Duration::from_millis(100));
        let start = std::time::Instant::now();
        drop(session);
        assert!(start.elapsed() < Duration::from_millis(200));
        // The connection is closed by the session thread soon after.
        listener.set_nonblocking(false).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 64];
        let closed = loop {
            match std::io::Read::read(&mut stream, &mut buf) {
                Ok(0) => break true,
                Ok(_) => continue,
                Err(_) => break false,
            }
        };
        assert!(closed, "the session did not close its connection");
    }

    #[test]
    fn oci_open_failure_is_reported_without_a_panic() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let console = OciConsole {
            instance: "web".into(),
            // No such key: refused before anything is created at the provider.
            identity: Some("/nonexistent/lsu-test/id_rsa".into()),
            config_file: None,
            cli_rc_file: None,
            profile: None,
            compartment_id: None,
            keep: false,
            keep_others: false,
        };
        let err = match rt.block_on(open_oci(&console)) {
            Ok(_) => panic!("opened a console without a key"),
            Err(e) => e,
        };
        assert!(!err.is_empty());
    }
}
