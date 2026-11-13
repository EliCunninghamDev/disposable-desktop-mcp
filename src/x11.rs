//! Small x11rb helpers shared by screenshots, window management and the viewer.

use anyhow::{anyhow, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, ClientMessageEvent, ConnectionExt as _, GetPropertyReply, PropMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub fn connect(display: &str) -> Result<(RustConnection, usize)> {
    x11rb::connect(Some(display)).with_context(|| format!("cannot connect to X display {display}"))
}

pub fn atom(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

pub fn root(conn: &RustConnection, screen_num: usize) -> Window {
    conn.setup().roots[screen_num].root
}

pub fn screen_size(conn: &RustConnection, screen_num: usize) -> (u16, u16) {
    let screen = &conn.setup().roots[screen_num];
    (screen.width_in_pixels, screen.height_in_pixels)
}

/// Fetch a property without pre-decoding it.
pub fn property(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    type_: Atom,
    length: u32,
) -> Result<Option<GetPropertyReply>> {
    let reply = conn
        .get_property(false, window, property, type_, 0, length)
        .with_context(|| format!("get_property failed for 0x{window:x}"))?
        .reply()?;
    // A missing property comes back with type NONE.
    if reply.type_ == 0 || reply.value_len == 0 {
        return Ok(None);
    }
    Ok(Some(reply))
}

pub fn get_u32s(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    type_: Atom,
) -> Result<Option<Vec<u32>>> {
    let Some(reply) = self::property(conn, window, property, type_, 1024)? else {
        return Ok(None);
    };
    if reply.format != 32 {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(reply.value.len() / 4);
    for chunk in reply.value.chunks_exact(4) {
        out.push(u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(Some(out))
}

pub fn get_string(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    type_: Atom,
) -> Result<Option<String>> {
    let Some(reply) = self::property(conn, window, property, type_, 4096)? else {
        return Ok(None);
    };
    if reply.format != 8 {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&reply.value).into_owned();
    let text = text.trim_end_matches('\0').to_string();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

pub fn set_string_property(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    type_: impl Into<Atom>,
    value: &str,
) -> Result<()> {
    conn.change_property8(PropMode::REPLACE, window, property, type_, value.as_bytes())?
        .check()?;
    Ok(())
}

pub fn set_u32_property(
    conn: &RustConnection,
    window: Window,
    property: Atom,
    type_: impl Into<Atom>,
    values: &[u32],
) -> Result<()> {
    conn.change_property32(PropMode::REPLACE, window, property, type_, values)?
        .check()?;
    Ok(())
}

/// Absolute geometry of a client window (decoration excluded).
pub fn window_rect(conn: &RustConnection, win: Window) -> Result<(i16, i16, u16, u16)> {
    let root = conn.setup().roots[0].root;
    let geom = conn.get_geometry(win)?.reply()?;
    let translated = conn.translate_coordinates(win, root, 0, 0)?.reply()?;
    Ok((translated.dst_x, translated.dst_y, geom.width, geom.height))
}

/// Send an EWMH client message.  `destination` is normally the root window;
/// `window` is the client the message is about (event.window).
pub fn send_client_message(
    conn: &RustConnection,
    destination: Window,
    window: Window,
    event_mask: x11rb::protocol::xproto::EventMask,
    type_: Atom,
    data: [u32; 5],
) -> Result<()> {
    let event = ClientMessageEvent::new(32, window, type_, data);
    conn.send_event(false, destination, event_mask, event)?
        .check()?;
    conn.flush()?;
    Ok(())
}

/// Parse a window id supplied by a model or human: decimal, 0x-prefixed hex,
/// or a bare hex string.
pub fn parse_window_id(value: &serde_json::Value) -> Result<u32> {
    let text = match value {
        serde_json::Value::Number(n) => n.as_u64().map(|n| n.to_string()).unwrap_or_default(),
        serde_json::Value::String(s) => s.trim().to_string(),
        _ => String::new(),
    };
    if text.is_empty() {
        return Err(anyhow!(
            "window id must be a number or a hexadecimal string"
        ));
    }
    let parsed = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16)
    } else if let Ok(dec) = text.parse::<u32>() {
        Ok(dec)
    } else {
        u32::from_str_radix(&text, 16)
    };
    parsed.map_err(|_| anyhow!("invalid window id: {text}"))
}

pub fn window_exists(conn: &RustConnection, win: Window) -> bool {
    match conn.get_window_attributes(win) {
        Ok(cookie) => cookie.reply().is_ok(),
        Err(_) => false,
    }
}
