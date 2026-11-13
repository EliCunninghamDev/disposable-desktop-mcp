//! MCP server and live viewer for a private X11 or Wayland desktop.
//!
//! The server owns one nested desktop at a time.  Everything it launches
//! lives inside it: an X server or a sway compositor, a window manager, a
//! private D-Bus session bus, an accessibility bus, and the applications the
//! agent starts.  The host session is never used for any of the input, capture,
//! or window management paths.

pub mod a11y;
pub mod backend;
pub mod clipboard;
pub mod control;
pub mod display;
pub mod env;
pub mod input;
pub mod mcp;
pub mod proc;
pub mod randr;
pub mod screenshot;
pub mod state;
pub mod sway;
pub mod tools;
pub mod trace;
pub mod viewer;
pub mod vk;
pub mod wait;
pub mod wayland;
pub mod windows;
pub mod wl_clipboard;
pub mod wl_input;
pub mod x11;

pub use display::DisplayManager;
