//! Backend-neutral runtime seam.
//!
//! Tools call [`Backend`] instead of reaching for `DISPLAY` directly.  The X11
//! variant wraps the existing GetImage/EWMH/XTEST/RandR code; the Wayland
//! variant uses sway IPC and wlr-screencopy.  Capabilities are what the session
//! can actually do right now, so a tool can return a precise error instead of
//! failing inside a protocol call.

use crate::display::SessionInfo;
use crate::screenshot::{self, Frame};
use crate::windows::WindowInfo;
use crate::{clipboard, randr, sway, wayland, windows};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    X11,
    Wayland,
}

impl BackendKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X11 => "x11",
            Self::Wayland => "wayland",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveResize {
    Any,
    FloatingOnly,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardPath {
    X11Selection,
    DataControl,
    None,
}

/// What the active backend can actually do.  Keep the fields coarse: a tool
/// either supports an operation or returns a capability error naming the
/// backend, not a partial implementation detail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub move_resize: MoveResize,
    pub resize_display: bool,
    pub capture_window: bool,
    pub clipboard: ClipboardPath,
    pub precise_geometry: bool,
    pub capture_output: bool,
    pub pointer: bool,
    pub keyboard: bool,
    pub window_list: bool,
    pub window_activate: bool,
    pub window_close: bool,
    pub visible_host_window: bool,
}

impl Capabilities {
    /// The current X11 backend: EWMH/GetImage/XTEST, with floating windows
    /// freely placeable and an in-process CLIPBOARD owner.
    pub const fn x11() -> Self {
        Self {
            move_resize: MoveResize::Any,
            resize_display: true,
            capture_window: true,
            clipboard: ClipboardPath::X11Selection,
            precise_geometry: true,
            capture_output: true,
            pointer: true,
            keyboard: true,
            window_list: true,
            window_activate: true,
            window_close: true,
            visible_host_window: true,
        }
    }

    /// A sway session.  Output capture, sway IPC window control, and display
    /// resize are implemented.  Wayland input and data-control clipboard are
    /// the next phase, so they are reported honestly as unavailable.
    pub const fn wayland() -> Self {
        Self {
            move_resize: MoveResize::FloatingOnly,
            resize_display: true,
            capture_window: false,
            clipboard: ClipboardPath::None,
            precise_geometry: false,
            capture_output: true,
            pointer: false,
            keyboard: false,
            window_list: true,
            window_activate: true,
            window_close: true,
            visible_host_window: true,
        }
    }
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::x11()
    }
}

impl Capabilities {
    pub fn for_backend(kind: BackendKind) -> Self {
        match kind {
            BackendKind::X11 => Self::x11(),
            BackendKind::Wayland => Self::wayland(),
        }
    }
}

/// An opaque window handle.  X11 uses the numeric id; Wayland uses the sway
/// container id.  Keep the backend prefix so a handle can never be passed to
/// the wrong implementation by accident.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WinId(String);

impl WinId {
    pub fn x11(id: u32) -> Self {
        Self(format!("x11:0x{id:x}"))
    }

    pub fn wayland(id: impl Into<String>) -> Self {
        Self(format!("wl:{}", id.into()))
    }

    pub fn parse(value: &str) -> Option<Self> {
        if value.starts_with("x11:") || value.starts_with("wl:") {
            Some(Self(value.to_string()))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_x11(&self) -> bool {
        self.0.starts_with("x11:")
    }

    pub fn is_wayland(&self) -> bool {
        self.0.starts_with("wl:")
    }

    pub fn x11_number(&self) -> Option<u32> {
        let text = self.0.strip_prefix("x11:")?;
        let text = text.strip_prefix("0x").unwrap_or(text);
        u32::from_str_radix(text, 16).ok()
    }

    pub fn wayland_number(&self) -> Option<i64> {
        self.0.strip_prefix("wl:")?.parse().ok()
    }

    pub fn to_json(&self) -> Value {
        match self.x11_number() {
            Some(id) => json!(id),
            None => json!(self.0),
        }
    }
}

/// Accept the old bare X11 id and the new backend-prefixed handle.  For a
/// Wayland session a bare number is interpreted as a sway container id.
pub fn parse_window_handle(value: &Value, kind: BackendKind) -> Result<WinId> {
    match value {
        Value::Null => bail!("missing window id"),
        Value::String(text) => {
            if let Some(handle) = WinId::parse(text) {
                return match (kind, handle.is_x11()) {
                    (BackendKind::X11, false) => {
                        bail!(
                            "{} is a wayland handle, but this session is X11",
                            handle.as_str()
                        )
                    }
                    (BackendKind::Wayland, true) => {
                        bail!(
                            "{} is an X11 handle, but this session is Wayland",
                            handle.as_str()
                        )
                    }
                    _ => Ok(handle),
                };
            }
            let text = text.trim();
            if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                let id = u32::from_str_radix(hex, 16)
                    .map_err(|_| anyhow!("invalid window id {text:?}"))?;
                return Ok(match kind {
                    BackendKind::X11 => WinId::x11(id),
                    BackendKind::Wayland => WinId::wayland(id.to_string()),
                });
            }
            let id = text
                .parse::<u32>()
                .map_err(|_| anyhow!("invalid window id {text:?}"))?;
            Ok(match kind {
                BackendKind::X11 => WinId::x11(id),
                BackendKind::Wayland => WinId::wayland(id.to_string()),
            })
        }
        Value::Number(number) => {
            let id = number
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| anyhow!("window id must fit in 32 bits"))?;
            Ok(match kind {
                BackendKind::X11 => WinId::x11(id),
                BackendKind::Wayland => WinId::wayland(id.to_string()),
            })
        }
        _ => bail!("window id must be a number or a backend-prefixed string"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureTarget {
    Output,
    Region(CaptureRect),
    Window(WinId),
}

pub struct CaptureResult {
    pub frame: Frame,
    pub cursor_drawn: bool,
    pub cursor_method: Option<screenshot::CursorMethod>,
}

/// A live session handle.  Cheap to clone; it holds paths and names, not
/// connections.
#[derive(Clone, Debug)]
pub enum Backend {
    X11 {
        display: String,
    },
    Wayland {
        runtime_dir: PathBuf,
        display: String,
        swaysock: PathBuf,
        output: String,
    },
}

impl Backend {
    pub fn from_info(info: &SessionInfo) -> Self {
        match info.backend {
            BackendKind::X11 => Self::X11 {
                display: info.display.clone(),
            },
            BackendKind::Wayland => Self::Wayland {
                runtime_dir: info.runtime_dir.clone(),
                display: info
                    .wayland_display
                    .clone()
                    .unwrap_or_else(|| info.display.clone()),
                swaysock: info
                    .swaysock
                    .clone()
                    .unwrap_or_else(|| info.runtime_dir.join("sway-ipc.sock")),
                output: info.output_name.clone().unwrap_or_default(),
            },
        }
    }

    pub fn kind(&self) -> BackendKind {
        match self {
            Self::X11 { .. } => BackendKind::X11,
            Self::Wayland { .. } => BackendKind::Wayland,
        }
    }

    pub fn capabilities(&self) -> Capabilities {
        Capabilities::for_backend(self.kind())
    }

    pub fn display(&self) -> &str {
        match self {
            Self::X11 { display } => display,
            Self::Wayland { display, .. } => display,
        }
    }

    fn capability_error(&self, feature: &str) -> anyhow::Error {
        anyhow!(
            "{feature} is not available on the {} backend in this build; \
             start a session with backend=x11 for the full X11 surface",
            self.kind().as_str()
        )
    }

    pub fn require_pointer(&self) -> Result<()> {
        if self.capabilities().pointer {
            Ok(())
        } else {
            Err(self.capability_error("pointer input"))
        }
    }

    pub fn require_keyboard(&self) -> Result<()> {
        if self.capabilities().keyboard {
            Ok(())
        } else {
            Err(self.capability_error("keyboard input"))
        }
    }

    pub fn require_clipboard(&self) -> Result<()> {
        if self.capabilities().clipboard == ClipboardPath::None {
            Err(self.capability_error("clipboard"))
        } else {
            Ok(())
        }
    }

    pub fn capture(&self, target: &CaptureTarget, cursor: bool) -> Result<CaptureResult> {
        match (self, target) {
            (Self::X11 { display }, target) => {
                let mut frame = match target {
                    CaptureTarget::Output => screenshot::capture(display)?,
                    CaptureTarget::Region(rect) => {
                        let x = u16::try_from(rect.x)
                            .map_err(|_| anyhow!("region.x must be between 0 and 65535"))?;
                        let y = u16::try_from(rect.y)
                            .map_err(|_| anyhow!("region.y must be between 0 and 65535"))?;
                        let width = u16::try_from(rect.width)
                            .map_err(|_| anyhow!("region.width is too large"))?;
                        let height = u16::try_from(rect.height)
                            .map_err(|_| anyhow!("region.height is too large"))?;
                        screenshot::capture_region(display, x, y, width, height)?
                    }
                    CaptureTarget::Window(handle) => {
                        let id = handle.x11_number().ok_or_else(|| {
                            anyhow!("{} is not an X11 window handle", handle.as_str())
                        })?;
                        screenshot::capture_window(display, id)?
                    }
                };
                let origin = self.origin(target)?.unwrap_or((0, 0));
                let cursor_method = if cursor {
                    screenshot::overlay_cursor(display, &mut frame, origin.0, origin.1)?
                } else {
                    None
                };
                Ok(CaptureResult {
                    cursor_drawn: cursor_method.is_some(),
                    cursor_method,
                    frame,
                })
            }
            (
                Self::Wayland {
                    runtime_dir,
                    display,
                    ..
                },
                target,
            ) => {
                if matches!(target, CaptureTarget::Window(_)) {
                    return Err(self.capability_error("per-window capture"));
                }
                let frame = wayland::capture(runtime_dir, display, Some(self.output()), cursor)?;
                let frame = match target {
                    CaptureTarget::Output => frame,
                    CaptureTarget::Region(rect) => screenshot::crop(
                        &frame,
                        rect.x.max(0) as u32,
                        rect.y.max(0) as u32,
                        rect.width,
                        rect.height,
                    )?,
                    CaptureTarget::Window(_) => unreachable!(),
                };
                Ok(CaptureResult {
                    cursor_drawn: cursor,
                    cursor_method: None,
                    frame,
                })
            }
        }
    }

    fn output(&self) -> &str {
        match self {
            Self::Wayland { output, .. } => output,
            Self::X11 { .. } => "",
        }
    }

    pub fn origin(&self, target: &CaptureTarget) -> Result<Option<(i32, i32)>> {
        match (self, target) {
            (_, CaptureTarget::Output) => Ok(None),
            (_, CaptureTarget::Region(rect)) => Ok(Some((rect.x, rect.y))),
            (Self::X11 { display }, CaptureTarget::Window(handle)) => {
                let id = handle
                    .x11_number()
                    .ok_or_else(|| anyhow!("{} is not an X11 window handle", handle.as_str()))?;
                Ok(Some(screenshot::window_origin(display, id)?))
            }
            (Self::Wayland { .. }, CaptureTarget::Window(_)) => {
                Err(self.capability_error("per-window capture"))
            }
        }
    }

    pub fn windows(&self) -> Result<Vec<WindowInfo>> {
        match self {
            Self::X11 { display } => windows::list(display),
            Self::Wayland { swaysock, .. } => Ok(sway::windows(swaysock)?
                .iter()
                .map(sway::SwayWindow::to_window_info)
                .collect()),
        }
    }

    pub fn focus_window(&self, handle: &WinId) -> Result<Value> {
        match self {
            Self::X11 { display } => {
                let id = handle
                    .x11_number()
                    .ok_or_else(|| anyhow!("{} is not an X11 window handle", handle.as_str()))?;
                windows::focus(display, id)
            }
            Self::Wayland { swaysock, .. } => {
                let id = handle
                    .wayland_number()
                    .ok_or_else(|| anyhow!("{} is not a sway window handle", handle.as_str()))?;
                sway::focus(swaysock, id)
            }
        }
    }

    pub fn move_resize_window(
        &self,
        handle: &WinId,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<Value> {
        match self {
            Self::X11 { display } => {
                let id = handle
                    .x11_number()
                    .ok_or_else(|| anyhow!("{} is not an X11 window handle", handle.as_str()))?;
                let x = i16::try_from(x).map_err(|_| anyhow!("x must fit in 16 bits"))?;
                let y = i16::try_from(y).map_err(|_| anyhow!("y must fit in 16 bits"))?;
                let width = u16::try_from(width).map_err(|_| anyhow!("width is too large"))?;
                let height = u16::try_from(height).map_err(|_| anyhow!("height is too large"))?;
                windows::move_resize(display, id, x, y, width, height)
            }
            Self::Wayland { swaysock, .. } => {
                let id = handle
                    .wayland_number()
                    .ok_or_else(|| anyhow!("{} is not a sway window handle", handle.as_str()))?;
                sway::move_resize(swaysock, id, x, y, width, height)
            }
        }
    }

    pub fn close_window(&self, handle: &WinId, force: bool) -> Result<Value> {
        match self {
            Self::X11 { display } => {
                let id = handle
                    .x11_number()
                    .ok_or_else(|| anyhow!("{} is not an X11 window handle", handle.as_str()))?;
                windows::close(display, id, force)
            }
            Self::Wayland { swaysock, .. } => {
                let id = handle
                    .wayland_number()
                    .ok_or_else(|| anyhow!("{} is not a sway window handle", handle.as_str()))?;
                if force {
                    bail!("force-close is not available on the wayland backend; sway only sends a close request");
                }
                sway::close(swaysock, id)
            }
        }
    }

    pub fn window_action(&self, handle: &WinId, action: &str) -> Result<Value> {
        match self {
            Self::X11 { display } => {
                let id = handle
                    .x11_number()
                    .ok_or_else(|| anyhow!("{} is not an X11 window handle", handle.as_str()))?;
                windows::action(display, id, action)
            }
            Self::Wayland { swaysock, .. } => {
                let id = handle
                    .wayland_number()
                    .ok_or_else(|| anyhow!("{} is not a sway window handle", handle.as_str()))?;
                sway::action(swaysock, id, action)
            }
        }
    }

    pub fn resize_display(&self, width: u32, height: u32, refresh: Option<f32>) -> Result<Value> {
        if !self.capabilities().resize_display {
            return Err(self.capability_error("display resize"));
        }
        match self {
            Self::X11 { display } => {
                let width = u16::try_from(width).map_err(|_| anyhow!("width is too large"))?;
                let height = u16::try_from(height).map_err(|_| anyhow!("height is too large"))?;
                randr::resize(display, width, height, refresh)
            }
            Self::Wayland {
                swaysock, output, ..
            } => {
                let output = if output.is_empty() {
                    let outputs = sway::Sway::new(swaysock.clone()).get_outputs()?;
                    outputs
                        .as_array()
                        .and_then(|list| list.first())
                        .and_then(|item| item.get("name"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("sway did not report an output name"))?
                        .to_string()
                } else {
                    output.clone()
                };
                sway::resize_output(swaysock, &output, width, height)
            }
        }
    }

    pub fn clipboard_get(&self) -> Result<String> {
        self.require_clipboard()?;
        match self {
            Self::X11 { display } => clipboard::get(display),
            Self::Wayland { .. } => Err(self.capability_error("clipboard")),
        }
    }

    pub fn clipboard_set(&self, text: &str) -> Result<usize> {
        self.require_clipboard()?;
        match self {
            Self::X11 { display } => clipboard::set(display, text),
            Self::Wayland { .. } => Err(self.capability_error("clipboard")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_capabilities_are_permissive() {
        let caps = Capabilities::x11();
        assert_eq!(caps.move_resize, MoveResize::Any);
        assert_eq!(caps.clipboard, ClipboardPath::X11Selection);
        assert!(caps.capture_window);
        assert!(caps.precise_geometry);
    }

    #[test]
    fn wayland_capabilities_are_honest() {
        let caps = Capabilities::wayland();
        assert_eq!(caps.move_resize, MoveResize::FloatingOnly);
        assert_eq!(caps.clipboard, ClipboardPath::None);
        assert!(!caps.capture_window);
        assert!(!caps.precise_geometry);
        assert!(!caps.pointer);
        assert!(!caps.keyboard);
        assert!(caps.resize_display);
        assert!(caps.window_list);
    }

    #[test]
    fn capabilities_round_trip() {
        let encoded = serde_json::to_string(&Capabilities::wayland()).unwrap();
        let decoded: Capabilities = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, Capabilities::wayland());
    }

    #[test]
    fn win_ids_keep_their_backend() {
        assert_eq!(WinId::x11(0x400003).as_str(), "x11:0x400003");
        assert_eq!(WinId::wayland("abc").as_str(), "wl:abc");
        assert!(WinId::parse("x11:0x400003").unwrap().is_x11());
        assert!(WinId::parse("wl:abc").unwrap().is_wayland());
        assert!(WinId::parse("400003").is_none());
        assert_eq!(WinId::x11(0x400003).x11_number(), Some(0x400003));
        assert_eq!(WinId::wayland("12").wayland_number(), Some(12));
    }

    #[test]
    fn parses_handles_for_the_active_backend() {
        let x11 = parse_window_handle(&json!(0x400003), BackendKind::X11).unwrap();
        assert_eq!(x11.as_str(), "x11:0x400003");
        let wl = parse_window_handle(&json!("12"), BackendKind::Wayland).unwrap();
        assert_eq!(wl.as_str(), "wl:12");
        assert!(parse_window_handle(&json!("wl:12"), BackendKind::X11).is_err());
        assert!(parse_window_handle(&json!("x11:0x12"), BackendKind::Wayland).is_err());
    }
}
