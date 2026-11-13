//! Wayland clipboard through `zwlr-data-control-v1`.
//!
//! Data-control is the compositor-scoped clipboard API: it replaces X11
//! selection ownership with a `data_source` and a per-seat `data_device`, and
//! never touches the host clipboard.
//!
//! Reading is a one-shot connection.  Writing keeps a small worker thread per
//! session alive, because the tool that sets the selection must keep serving
//! `send` requests for as long as it owns the clipboard.  The worker exits when
//! the selection is replaced, the session stops, or the compositor goes away.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixStream, UnixStream as StopStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

const READ_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_BYTES: usize = 8 * 1024 * 1024;

/// MIME types we offer.  XWayland-aware clients negotiate one of these.
const OFFER_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

fn preferred_mime(mimes: &[String]) -> Option<&str> {
    for wanted in [
        "text/plain;charset=utf-8",
        "text/plain;charset=UTF-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
        "TEXT",
    ] {
        if let Some(found) = mimes.iter().find(|mime| mime.as_str() == wanted) {
            return Some(found.as_str());
        }
    }
    mimes.first().map(String::as_str)
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error()).context("pipe2 failed");
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn write_all(fd: &OwnedFd, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let written = unsafe {
            libc::write(
                fd.as_raw_fd(),
                bytes.as_ptr().cast(),
                bytes.len() as libc::size_t,
            )
        };
        if written < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

fn read_text(fd: &OwnedFd, timeout: Duration) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for the clipboard owner to send data");
        }
        let mut pollfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, remaining.as_millis() as i32) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("clipboard poll failed");
        }
        if ready == 0 {
            bail!("timed out waiting for the clipboard owner to send data");
        }
        let read = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if read < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err).context("clipboard read failed");
        }
        if read == 0 {
            break;
        }
        out.extend_from_slice(&buf[..read as usize]);
        if out.len() > MAX_BYTES {
            bail!("clipboard data is larger than {MAX_BYTES} bytes");
        }
    }
    String::from_utf8(out).map_err(|_| anyhow!("the clipboard is not valid UTF-8"))
}

fn connect(socket: &Path) -> Result<Connection> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("cannot connect to Wayland socket {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Connection::from_socket(stream)
        .with_context(|| format!("cannot initialize Wayland client on {}", socket.display()))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[derive(Default)]
struct GetState {
    offer: Option<ZwlrDataControlOfferV1>,
    mimes: Vec<String>,
    selection: Option<ZwlrDataControlOfferV1>,
    /// A selection event was received, including the "nothing selected" form.
    reported: bool,
    finished: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for GetState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for GetState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for GetState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for GetState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.mimes.clear();
                state.offer = Some(id);
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.selection = id;
                state.reported = true;
            }
            zwlr_data_control_device_v1::Event::Finished => state.finished = true,
            _ => {}
        }
    }

    wayland_client::event_created_child!(GetState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for GetState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.mimes.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for GetState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlSourceV1,
        _event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

pub fn get(runtime_dir: &Path, display: &str) -> Result<String> {
    let socket = runtime_dir.join(display);
    let conn = connect(&socket)?;
    let (globals, mut queue) =
        registry_queue_init::<GetState>(&conn).context("cannot read the compositor global list")?;
    let qh = queue.handle();
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(|err| anyhow!("data-control is not available: {err}"))?;
    let seat: wl_seat::WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|err| anyhow!("the compositor did not advertise a wl_seat: {err}"))?;

    let mut state = GetState::default();
    let device = manager.get_data_device(&seat, &qh, ());
    conn.flush()
        .context("cannot flush the data device request")?;

    let deadline = Instant::now() + READ_TIMEOUT;
    while !state.reported && !state.finished {
        if Instant::now() >= deadline {
            bail!("the compositor did not report a clipboard selection");
        }
        queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch failed while reading the clipboard")?;
    }
    if state.finished {
        bail!("the compositor closed the data device before reporting a selection");
    }
    let offer = state
        .selection
        .as_ref()
        .ok_or_else(|| anyhow!("the clipboard is empty"))?;
    let mime = preferred_mime(&state.mimes)
        .ok_or_else(|| anyhow!("the clipboard offers no text MIME type"))?
        .to_string();

    let (read_fd, write_fd) = pipe()?;
    offer.receive(mime, write_fd.as_fd());
    conn.flush()
        .context("cannot flush the clipboard receive request")?;
    drop(write_fd);
    let text = read_text(&read_fd, READ_TIMEOUT)?;

    offer.destroy();
    device.destroy();
    Ok(text)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SetState {
    source: Option<ZwlrDataControlSourceV1>,
    device: Option<ZwlrDataControlDeviceV1>,
    text: String,
    cancelled: bool,
    finished: bool,
    serve_errors: Vec<String>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for SetState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for SetState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for SetState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlManagerV1,
        _event: wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for SetState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_device_v1::Event::Finished = event {
            state.finished = true;
        }
    }

    wayland_client::event_created_child!(SetState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for SetState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrDataControlOfferV1,
        _event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for SetState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if let Err(err) = write_all(&fd, state.text.as_bytes()) {
                    state
                        .serve_errors
                        .push(format!("cannot write {mime_type}: {err}"));
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => state.cancelled = true,
            _ => {}
        }
    }
}

fn stop_requested(stop: &StopStream) -> bool {
    let mut pollfd = libc::pollfd {
        fd: stop.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pollfd, 1, 0) > 0 }
}

fn serve(
    socket: &Path,
    text: &str,
    stop: StopStream,
    ready: mpsc::Sender<Result<(), String>>,
) -> Result<()> {
    let conn = match connect(socket) {
        Ok(conn) => conn,
        Err(err) => {
            let _ = ready.send(Err(format!("{err:#}")));
            return Err(err);
        }
    };
    let (globals, mut queue) = match registry_queue_init::<SetState>(&conn) {
        Ok(value) => value,
        Err(err) => {
            let _ = ready.send(Err(format!("{err:#}")));
            return Err(err).context("cannot read the compositor global list");
        }
    };
    let qh = queue.handle();
    let manager: ZwlrDataControlManagerV1 = match globals.bind(&qh, 1..=2, ()) {
        Ok(manager) => manager,
        Err(err) => {
            let message = format!("data-control is not available: {err}");
            let _ = ready.send(Err(message.clone()));
            bail!("{message}");
        }
    };
    let seat: wl_seat::WlSeat = match globals.bind(&qh, 1..=9, ()) {
        Ok(seat) => seat,
        Err(err) => {
            let message = format!("the compositor did not advertise a wl_seat: {err}");
            let _ = ready.send(Err(message.clone()));
            bail!("{message}");
        }
    };

    let mut state = SetState {
        text: text.to_string(),
        ..SetState::default()
    };
    let device = manager.get_data_device(&seat, &qh, ());
    let source = manager.create_data_source(&qh, ());
    for mime in OFFER_MIMES {
        source.offer((*mime).to_string());
    }
    device.set_selection(Some(&source));
    state.device = Some(device);
    state.source = Some(source);
    conn.flush()
        .context("cannot flush the clipboard selection")?;
    if let Err(err) = queue.roundtrip(&mut state) {
        let _ = ready.send(Err(format!("{err:#}")));
        return Err(err).context("the compositor rejected the clipboard selection");
    }
    if state.finished {
        let message =
            "the compositor closed the data device while setting the selection".to_string();
        let _ = ready.send(Err(message.clone()));
        bail!("{message}");
    }
    let _ = ready.send(Ok(()));

    let loop_result = (|| -> Result<()> {
        loop {
            queue
                .dispatch_pending(&mut state)
                .context("Wayland dispatch failed while serving the clipboard")?;
            if !state.serve_errors.is_empty() {
                bail!("{}", state.serve_errors.join("; "));
            }
            if state.finished {
                bail!("the compositor closed the data device");
            }
            if stop_requested(&stop) {
                break;
            }
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let mut fds = [
                libc::pollfd {
                    fd: guard.connection_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stop.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if ready < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err).context("clipboard poll failed");
            }
            if fds[1].revents != 0 {
                break;
            }
            if fds[0].revents != 0 {
                guard
                    .read()
                    .context("cannot read clipboard events from the compositor")?;
            }
        }
        Ok(())
    })();

    if let Some(device) = state.device.take() {
        device.destroy();
    }
    loop_result
}

struct Worker {
    /// Dropping this end closes the stop pipe and wakes the worker out of
    /// `poll`, which is how a replaced selection releases its source.
    _stop: StopStream,
}

static WORKERS: OnceLock<Mutex<HashMap<PathBuf, Worker>>> = OnceLock::new();

fn workers() -> &'static Mutex<HashMap<PathBuf, Worker>> {
    WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Put `text` on the session's clipboard.  The previous worker, if any, is
/// stopped by closing its stop pipe.
pub fn set(runtime_dir: &Path, display: &str, text: &str) -> Result<usize> {
    let socket = runtime_dir.join(display);
    let (stop_read, stop_write) =
        UnixStream::pair().context("cannot create the clipboard stop pipe")?;
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread_socket = socket.clone();
    let thread_text = text.to_string();
    std::thread::Builder::new()
        .name("disposable-desktop-mcp-clipboard".into())
        .spawn(move || {
            let _ = serve(&thread_socket, &thread_text, stop_read, ready_tx);
        })
        .context("cannot start the clipboard worker")?;

    match ready_rx.recv_timeout(Duration::from_secs(6)) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => bail!("{err}"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("timed out setting the Wayland clipboard")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("the clipboard worker exited before the selection was set")
        }
    }

    let mut guard = workers()
        .lock()
        .map_err(|_| anyhow!("the clipboard registry is poisoned"))?;
    // Dropping the old worker closes its stop pipe, which wakes it out of poll.
    guard.insert(socket, Worker { _stop: stop_write });
    Ok(text.chars().count())
}

/// Release the selection when the session stops.
pub fn disconnect(runtime_dir: &Path, display: &str) {
    let socket = runtime_dir.join(display);
    if let Ok(mut guard) = workers().lock() {
        guard.remove(&socket);
    }
}

pub fn status(runtime_dir: &Path, display: &str) -> Value {
    let socket = runtime_dir.join(display);
    let owned = workers()
        .lock()
        .map(|guard| guard.contains_key(&socket))
        .unwrap_or(false);
    json!({ "owner": owned })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_utf8_mime() {
        let mimes = vec!["text/plain".to_string(), "UTF8_STRING".to_string()];
        assert_eq!(preferred_mime(&mimes), Some("UTF8_STRING"));
    }

    #[test]
    fn falls_back_to_any_offered_mime() {
        let mimes = vec!["application/x-custom".to_string()];
        assert_eq!(preferred_mime(&mimes), Some("application/x-custom"));
    }

    #[test]
    fn empty_offer_has_no_mime() {
        assert_eq!(preferred_mime(&[]), None);
    }
}
