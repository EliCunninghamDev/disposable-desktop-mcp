//! Generated bindings for the `virtual_keyboard_unstable_v1` protocol.
//!
//! The Rust bindings for this protocol are not part of the published
//! `wayland-protocols` crate, so the XML is vendored under `protocols/` and
//! generated here.  Only the client side is needed.

pub mod virtual_keyboard {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/virtual-keyboard-unstable-v1.xml");
    }

    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/virtual-keyboard-unstable-v1.xml");
}
