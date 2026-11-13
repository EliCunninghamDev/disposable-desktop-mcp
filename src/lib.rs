//! MCP server and live viewer for a nested Xephyr/Xvfb desktop.
//!
//! The server owns one nested X display at a time.  Everything it launches
//! lives in that display: the X server, a window manager, a private D-Bus
//! session bus, an accessibility bus, and the applications the agent starts.
//! The host session is never used for any of the input, capture, or window
//! management paths.

pub mod a11y;
pub mod display;
pub mod env;
pub mod input;
pub mod mcp;
pub mod proc;
pub mod screenshot;
pub mod state;
pub mod tools;
pub mod viewer;
pub mod windows;
pub mod x11;

pub use display::DisplayManager;
