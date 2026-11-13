//! Screen capture through compositor protocols into `wl_shm`.
//!
//! Outputs use `wlr-screencopy-v1`.  Windows use
//! `ext-foreign-toplevel-list-v1` to resolve sway's toplevel identifier,
//! `ext-foreign-toplevel-image-capture-source-v1` to make a source for it, and
//! `ext-image-copy-capture-v1` to copy it into shared memory.  The compositor
//! is ours, so no portal or PipeWire is involved.

use crate::screenshot::Frame;
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

struct Mapping {
    ptr: *mut u8,
    len: usize,
}

unsafe impl Send for Mapping {}

impl Drop for Mapping {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            unsafe {
                libc::munmap(self.ptr.cast(), self.len);
            }
        }
    }
}

#[derive(Default)]
struct CaptureState {
    output_name: Option<String>,
    shm: Option<wl_shm::WlShm>,
    manager: Option<ZwlrScreencopyManagerV1>,
    buffer_format: Option<wl_shm::Format>,
    buffer_info: Option<(u32, u32, u32)>,
    buffer: Option<wl_buffer::WlBuffer>,
    mapping: Option<Mapping>,
    y_invert: bool,
    ready: bool,
    failed: Option<String>,
    error: Option<String>,
}

impl CaptureState {
    fn create_buffer(
        &mut self,
        format: wl_shm::Format,
        width: u32,
        height: u32,
        stride: u32,
        qh: &QueueHandle<CaptureState>,
    ) -> Result<wl_buffer::WlBuffer> {
        let shm = self
            .shm
            .as_ref()
            .ok_or_else(|| anyhow!("compositor did not advertise wl_shm"))?;
        let (buffer, mapping) = create_shm_buffer(shm, format, width, height, stride, qh)?;
        self.mapping = Some(mapping);
        self.buffer_info = Some((width, height, stride));
        Ok(buffer)
    }
}

fn create_shm_buffer<D>(
    shm: &wl_shm::WlShm,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
    qh: &QueueHandle<D>,
) -> Result<(wl_buffer::WlBuffer, Mapping)>
where
    D: Dispatch<wl_shm_pool::WlShmPool, ()> + Dispatch<wl_buffer::WlBuffer, ()> + 'static,
{
    let size = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("capture buffer size overflow"))?;
    if size == 0 {
        bail!("capture buffer is empty");
    }

    let name = c"xephyr-mcp-shm";
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("memfd_create failed");
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::ftruncate(fd.as_raw_fd(), size as libc::off_t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("ftruncate failed");
    }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error()).context("mmap failed");
    }
    let mapping = Mapping {
        ptr: ptr.cast(),
        len: size,
    };

    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride as i32,
        format,
        qh,
        (),
    );
    pool.destroy();
    Ok((buffer, mapping))
}

fn frame_from_mapping(
    mapping: &Mapping,
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    y_invert: bool,
) -> Result<Frame> {
    let needed = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("capture buffer size overflow"))?;
    if mapping.len < needed {
        bail!(
            "capture buffer is short: {} bytes, expected {needed}",
            mapping.len
        );
    }
    let bytes = unsafe { std::slice::from_raw_parts(mapping.ptr, needed) };
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let row_index = if y_invert {
            height as usize - 1 - row
        } else {
            row
        };
        let start = row_index * stride as usize;
        let line = &bytes[start..start + width as usize * 4];
        for px in line.chunks_exact(4) {
            let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
            match format {
                wl_shm::Format::Argb8888 => {
                    // Premultiplied alpha: undo it so the PNG is not dark.
                    if a == 0 || a == 255 {
                        rgba.extend_from_slice(&[r, g, b, 255]);
                    } else {
                        let un = |component: u8| {
                            ((component as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
                        };
                        rgba.extend_from_slice(&[un(r), un(g), un(b), 255]);
                    }
                }
                wl_shm::Format::Xrgb8888 => rgba.extend_from_slice(&[r, g, b, 255]),
                other => bail!("unsupported wl_shm format for capture: {other:?}"),
            }
        }
    }
    Ok(Frame {
        width,
        height,
        rgba,
    })
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for CaptureState {
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

impl Dispatch<wl_shm::WlShm, ()> for CaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for CaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, ()> for CaptureState {
    fn event(
        state: &mut Self,
        _proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.output_name = Some(name);
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for CaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for CaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: zwlr_screencopy_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let format = match format.into_result() {
                    Ok(format) => format,
                    Err(_) => {
                        state.error = Some(format!(
                            "compositor offered an unknown wl_shm format: {format:?}"
                        ));
                        return;
                    }
                };
                state.buffer_format = Some(format);
                match state.create_buffer(format, width, height, stride, qh) {
                    Ok(buffer) => {
                        proxy.copy(&buffer);
                        state.buffer = Some(buffer);
                    }
                    Err(err) => state.error = Some(format!("{err:#}")),
                }
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.y_invert = flags
                    .into_result()
                    .map(|value| value.contains(zwlr_screencopy_frame_v1::Flags::YInvert))
                    .unwrap_or(false);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => state.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.failed = Some("compositor reported a failed screencopy".into());
            }
            _ => {}
        }
    }
}

/// Millisecond timestamp for protocol requests.  The base is undefined, but
/// every request from this process must share one clock.
pub fn now_ms() -> u32 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as u32
}

/// Capture the whole output.  `cursor` asks the compositor to composite its
/// pointer into the frame, which is the Wayland equivalent of the XFixes
/// overlay.
pub fn capture(
    runtime_dir: &Path,
    display: &str,
    expected_output: Option<&str>,
    cursor: bool,
) -> Result<Frame> {
    let socket = runtime_dir.join(display);
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("cannot connect to Wayland socket {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let conn = Connection::from_socket(stream)
        .with_context(|| format!("cannot initialize Wayland client on {}", socket.display()))?;

    let (globals, mut queue) = registry_queue_init::<CaptureState>(&conn)
        .context("cannot read the compositor global list")?;
    let qh = queue.handle();
    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|err| anyhow!("wl_shm is not available: {err}"))?;
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .map_err(|err| anyhow!("wlr-screencopy is not available: {err}"))?;
    let output: wl_output::WlOutput = globals
        .bind(&qh, 4..=4, ())
        .or_else(|_| globals.bind(&qh, 1..=3, ()))
        .map_err(|err| anyhow!("the compositor did not advertise a wl_output: {err}"))?;

    let mut state = CaptureState {
        shm: Some(shm),
        manager: Some(manager),
        ..CaptureState::default()
    };
    // The output name arrives in a `name` event after binding.
    queue
        .roundtrip(&mut state)
        .context("failed while reading output information")?;
    if let (Some(expected), Some(actual)) = (expected_output, state.output_name.as_deref()) {
        if expected != actual {
            bail!("expected output {expected}, but the compositor reported {actual}");
        }
    }

    let manager = state
        .manager
        .as_ref()
        .ok_or_else(|| anyhow!("screencopy manager disappeared"))?;
    let frame = manager.capture_output(cursor as i32, &output, &qh, ());
    conn.flush()
        .context("cannot flush the screencopy request")?;

    let deadline = Instant::now() + Duration::from_secs(8);
    while !state.ready && state.failed.is_none() && state.error.is_none() {
        if Instant::now() >= deadline {
            bail!("timed out waiting for the compositor to copy the output");
        }
        queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch failed during screencopy")?;
    }
    frame.destroy();
    output.release();

    if let Some(error) = state.error.take() {
        bail!("{error}");
    }
    if let Some(failed) = state.failed.take() {
        bail!("{failed}");
    }

    let (width, height, stride) = state
        .buffer_info
        .ok_or_else(|| anyhow!("compositor never described the screencopy buffer"))?;
    let mapping = state
        .mapping
        .as_ref()
        .ok_or_else(|| anyhow!("screencopy buffer was not mapped"))?;
    let format = state
        .buffer_format
        .ok_or_else(|| anyhow!("compositor never reported a buffer format"))?;
    frame_from_mapping(mapping, width, height, stride, format, state.y_invert)
}

// ---------------------------------------------------------------------------
// Per-window capture through ext-image-copy-capture
// ---------------------------------------------------------------------------

use std::sync::{Arc, Mutex};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

#[derive(Default, Clone)]
struct ToplevelInfo {
    identifier: Option<String>,
    app_id: Option<String>,
    title: Option<String>,
    done: bool,
    closed: bool,
}

type SharedToplevel = Arc<Mutex<ToplevelInfo>>;

#[derive(Default)]
struct WindowCaptureState {
    shm: Option<wl_shm::WlShm>,
    copy_manager: Option<ExtImageCopyCaptureManagerV1>,
    source_manager: Option<ExtForeignToplevelImageCaptureSourceManagerV1>,
    /// Kept alive while the initial toplevel list is collected.
    _list: Option<ExtForeignToplevelListV1>,
    toplevels: Vec<(ExtForeignToplevelHandleV1, SharedToplevel)>,
    list_finished: bool,
    source: Option<ExtImageCaptureSourceV1>,
    session: Option<ExtImageCopyCaptureSessionV1>,
    frame: Option<ExtImageCopyCaptureFrameV1>,
    buffer_size: Option<(u32, u32)>,
    buffer_format: Option<wl_shm::Format>,
    constraints_done: bool,
    buffer: Option<wl_buffer::WlBuffer>,
    mapping: Option<Mapping>,
    transform: Option<wl_output::Transform>,
    ready: bool,
    failed: Option<String>,
    stopped: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WindowCaptureState {
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

impl Dispatch<wl_shm::WlShm, ()> for WindowCaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for WindowCaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for WindowCaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for WindowCaptureState {
    fn event(
        state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                // The user data is created by event_created_child below; read
                // it back from the proxy.
                let shared = toplevel.data::<SharedToplevel>().cloned();
                if let Some(shared) = shared {
                    state.toplevels.push((toplevel, shared));
                }
            }
            ext_foreign_toplevel_list_v1::Event::Finished => state.list_finished = true,
            _ => {}
        }
    }

    wayland_client::event_created_child!(WindowCaptureState, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, SharedToplevel::default()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, SharedToplevel> for WindowCaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        data: &SharedToplevel,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let mut info = match data.lock() {
            Ok(info) => info,
            Err(_) => return,
        };
        match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => {
                info.identifier = Some(identifier);
            }
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                info.app_id = Some(app_id);
            }
            ext_foreign_toplevel_handle_v1::Event::Title { title } => {
                info.title = Some(title);
            }
            ext_foreign_toplevel_handle_v1::Event::Done => info.done = true,
            ext_foreign_toplevel_handle_v1::Event::Closed => info.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for WindowCaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtForeignToplevelImageCaptureSourceManagerV1,
        _event: wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for WindowCaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCaptureSourceV1,
        _event: wayland_protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for WindowCaptureState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for WindowCaptureState {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                state.buffer_size = Some((width, height));
            }
            ext_image_copy_capture_session_v1::Event::ShmFormat { format } => {
                if let Ok(format) = format.into_result() {
                    state.buffer_format = Some(format);
                }
            }
            ext_image_copy_capture_session_v1::Event::Done => state.constraints_done = true,
            ext_image_copy_capture_session_v1::Event::Stopped => state.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for WindowCaptureState {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Transform { transform } => {
                state.transform = transform.into_result().ok();
            }
            ext_image_copy_capture_frame_v1::Event::Ready => state.ready = true,
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                state.failed = Some(format!("compositor failed the window capture: {reason:?}"));
            }
            _ => {}
        }
    }
}

/// Capture one toplevel by its foreign-toplevel identifier.  `app_id` and
/// `title` are fallbacks for compositors that do not expose identifiers.
pub fn capture_toplevel(
    runtime_dir: &Path,
    display: &str,
    identifier: Option<&str>,
    app_id: Option<&str>,
    title: Option<&str>,
) -> Result<Frame> {
    let socket = runtime_dir.join(display);
    let stream = UnixStream::connect(&socket)
        .with_context(|| format!("cannot connect to Wayland socket {}", socket.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let conn = Connection::from_socket(stream)
        .with_context(|| format!("cannot initialize Wayland client on {}", socket.display()))?;
    let (globals, mut queue) = registry_queue_init::<WindowCaptureState>(&conn)
        .context("cannot read the compositor global list")?;
    let qh = queue.handle();

    let shm: wl_shm::WlShm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|err| anyhow!("wl_shm is not available: {err}"))?;
    let list: ExtForeignToplevelListV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|err| anyhow!("ext-foreign-toplevel-list is not available: {err}"))?;
    let source_manager: ExtForeignToplevelImageCaptureSourceManagerV1 =
        globals.bind(&qh, 1..=1, ()).map_err(|err| {
            anyhow!("ext-foreign-toplevel-image-capture-source is not available: {err}")
        })?;
    let copy_manager: ExtImageCopyCaptureManagerV1 = globals
        .bind(&qh, 1..=1, ())
        .map_err(|err| anyhow!("ext-image-copy-capture is not available: {err}"))?;

    let mut state = WindowCaptureState {
        shm: Some(shm),
        _list: Some(list),
        source_manager: Some(source_manager),
        copy_manager: Some(copy_manager),
        ..WindowCaptureState::default()
    };
    // Binding the list sends every existing toplevel and its properties
    // immediately; one roundtrip collects that initial batch.  `finished` is
    // only sent after a `stop` request.
    conn.flush()
        .context("cannot flush the toplevel list request")?;
    queue
        .roundtrip(&mut state)
        .context("Wayland dispatch failed while listing toplevels")?;
    if let Some(list) = state._list.take() {
        list.stop();
    }
    conn.flush()
        .context("cannot flush the toplevel list stop request")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.list_finished {
        if Instant::now() >= deadline {
            bail!("timed out waiting for the compositor to finish listing toplevels");
        }
        queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch failed while finishing the toplevel list")?;
    }

    let chosen = match_toplevel(&state, identifier, app_id, title)?;
    let source = state
        .source_manager
        .as_ref()
        .ok_or_else(|| anyhow!("the capture source manager disappeared"))?
        .create_source(&chosen, &qh, ());
    let session = state
        .copy_manager
        .as_ref()
        .ok_or_else(|| anyhow!("the copy capture manager disappeared"))?
        .create_session(
            &source,
            wayland_protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::Options::empty(),
            &qh,
            (),
        );
    state.source = Some(source);
    state.session = Some(session.clone());
    conn.flush()
        .context("cannot flush the capture session request")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while !state.constraints_done {
        if state.stopped {
            bail!("the compositor stopped the window capture session");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for window capture buffer constraints");
        }
        queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch failed while waiting for capture constraints")?;
    }
    let (width, height) = state
        .buffer_size
        .ok_or_else(|| anyhow!("the compositor never reported a capture buffer size"))?;
    let format = state
        .buffer_format
        .ok_or_else(|| anyhow!("the compositor offered no shm format for window capture"))?;
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("window capture width is too large"))?;
    let shm = state
        .shm
        .as_ref()
        .ok_or_else(|| anyhow!("compositor did not advertise wl_shm"))?;
    let (buffer, mapping) = create_shm_buffer(shm, format, width, height, stride, &qh)?;
    let frame = session.create_frame(&qh, ());
    frame.attach_buffer(&buffer);
    frame.damage_buffer(0, 0, width as i32, height as i32);
    frame.capture();
    state.buffer = Some(buffer);
    state.mapping = Some(mapping);
    state.frame = Some(frame.clone());
    conn.flush()
        .context("cannot flush the window capture request")?;

    let deadline = Instant::now() + Duration::from_secs(8);
    while !state.ready && state.failed.is_none() && !state.stopped {
        if Instant::now() >= deadline {
            bail!("timed out waiting for the compositor to copy the toplevel");
        }
        queue
            .blocking_dispatch(&mut state)
            .context("Wayland dispatch failed during window capture")?;
    }
    frame.destroy();
    if let Some(failed) = state.failed.take() {
        bail!("{failed}");
    }
    if state.stopped {
        bail!("the compositor stopped the window capture session");
    }
    match state.transform {
        None | Some(wl_output::Transform::Normal) => {}
        Some(other) => bail!("the compositor returned a transformed window image ({other:?})"),
    }
    let mapping = state
        .mapping
        .as_ref()
        .ok_or_else(|| anyhow!("window capture buffer was not mapped"))?;
    frame_from_mapping(mapping, width, height, stride, format, false)
}

fn match_toplevel(
    state: &WindowCaptureState,
    identifier: Option<&str>,
    app_id: Option<&str>,
    title: Option<&str>,
) -> Result<ExtForeignToplevelHandleV1> {
    let mut fallback = None;
    let mut available = Vec::new();
    for (handle, shared) in &state.toplevels {
        let Ok(info) = shared.lock() else {
            continue;
        };
        if info.closed {
            continue;
        }
        available.push(format!(
            "{:?}/{:?}",
            info.identifier.as_deref().unwrap_or("-"),
            info.app_id.as_deref().unwrap_or("-")
        ));
        if let (Some(wanted), Some(found)) = (identifier, info.identifier.as_deref()) {
            if wanted == found {
                return Ok(handle.clone());
            }
        }
        let app_matches = app_id
            .zip(info.app_id.as_deref())
            .map(|(wanted, found)| wanted == found)
            .unwrap_or(false);
        let title_matches = title
            .zip(info.title.as_deref())
            .map(|(wanted, found)| wanted == found)
            .unwrap_or(false);
        if app_matches && (title.is_none() || title_matches) {
            fallback = Some(handle.clone());
        }
    }
    fallback.ok_or_else(|| {
        anyhow!(
            "no foreign toplevel matched identifier {:?}, app_id {:?}, title {:?}; available: {}",
            identifier,
            app_id,
            title,
            available.join(", ")
        )
    })
}
