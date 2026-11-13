//! Backend-neutral capability and identity types.
//!
//! The X11 and Wayland implementations live outside this module for now; these
//! types are the part tools and state files share.  Capabilities let a tool
//! return a precise error on a backend that cannot do something, instead of
//! failing inside an X11 or Wayland call.

use serde::{Deserialize, Serialize};

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

    /// A sway session.  Geometry is exact for the compositor rect, not for
    /// client-relative widget coordinates; window capture needs the
    /// ext-foreign-toplevel source and is not wired yet.
    pub const fn wayland() -> Self {
        Self {
            move_resize: MoveResize::FloatingOnly,
            resize_display: true,
            capture_window: false,
            clipboard: ClipboardPath::DataControl,
            precise_geometry: false,
            capture_output: true,
            pointer: true,
            keyboard: true,
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

/// An opaque window handle.  X11 uses the numeric id; Wayland uses the
/// compositor's toplevel identifier.  Keep the backend prefix so a handle can
/// never be passed to the wrong implementation by accident.
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
        assert_eq!(caps.clipboard, ClipboardPath::DataControl);
        assert!(!caps.capture_window);
        assert!(!caps.precise_geometry);
        assert!(caps.resize_display);
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
    }
}
