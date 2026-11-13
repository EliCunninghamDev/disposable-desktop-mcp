//! Output capture through `wlr-screencopy-v1` into `wl_shm`.
//!
//! The compositor is ours, so no portal or PipeWire is involved.  Only the
//! output copy is implemented; per-window capture needs
//! `ext-foreign-toplevel-image-capture-source` and is reported as unsupported
//! for now.

use crate::screenshot::Frame;
use anyhow::{anyhow, bail, Context, Result};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle};
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
        let size = (stride as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| anyhow!("screencopy buffer size overflow"))?;
        if size == 0 {
            bail!("screencopy buffer is empty");
        }

        let name = std::ffi::CString::new("xephyr-mcp-shm")?;
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
        self.mapping = Some(Mapping {
            ptr: ptr.cast(),
            len: size,
        });

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
        self.buffer_info = Some((width, height, stride));
        Ok(buffer)
    }
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
    let needed = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("screencopy buffer size overflow"))?;
    if mapping.len < needed {
        bail!(
            "screencopy buffer is short: {} bytes, expected {needed}",
            mapping.len
        );
    }
    let bytes = unsafe { std::slice::from_raw_parts(mapping.ptr, needed) };
    let format = state
        .buffer_format
        .ok_or_else(|| anyhow!("compositor never reported a buffer format"))?;

    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let row_index = if state.y_invert {
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
                other => bail!("unsupported wl_shm format for screencopy: {other:?}"),
            }
        }
    }

    Ok(Frame {
        width,
        height,
        rgba,
    })
}
