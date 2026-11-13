//! EWMH window listing, focus, move/resize and close.

use crate::x11;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConfigureWindowAux, ConnectionExt as _, EventMask, MapState, Window,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;

#[derive(Clone, Debug)]
pub struct WindowInfo {
    pub id: Window,
    pub title: String,
    pub instance: Option<String>,
    pub class: Option<String>,
    pub pid: Option<u32>,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub mapped: bool,
    pub focused: bool,
    pub desktop: Option<u32>,
    pub frame: Option<(u32, u32, u32, u32)>,
    pub stack: usize,
}

impl WindowInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "id_hex": format!("0x{:x}", self.id),
            "title": self.title,
            "class": self.class,
            "instance": self.instance,
            "pid": self.pid,
            "x": self.x,
            "y": self.y,
            "width": self.width,
            "height": self.height,
            "mapped": self.mapped,
            "focused": self.focused,
            "desktop": self.desktop,
            "frame_extents": self.frame.map(|(l, r, t, b)| json!({"left": l, "right": r, "top": t, "bottom": b})),
            "stack": self.stack,
        })
    }
}

struct Atoms {
    client_list: Atom,
    client_list_stacking: Atom,
    active_window: Atom,
    net_wm_name: Atom,
    utf8: Atom,
    wm_name: Atom,
    wm_class: Atom,
    wm_pid: Atom,
    wm_desktop: Atom,
    frame_extents: Atom,
    moveresize: Atom,
    wm_protocols: Atom,
    wm_delete: Atom,
    wm_state: Atom,
    wm_change_state: Atom,
    net_wm_state: Atom,
    state_maximized_vert: Atom,
    state_maximized_horz: Atom,
    state_fullscreen: Atom,
    state_above: Atom,
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Self> {
        Ok(Self {
            client_list: x11::atom(conn, "_NET_CLIENT_LIST")?,
            client_list_stacking: x11::atom(conn, "_NET_CLIENT_LIST_STACKING")?,
            active_window: x11::atom(conn, "_NET_ACTIVE_WINDOW")?,
            net_wm_name: x11::atom(conn, "_NET_WM_NAME")?,
            utf8: x11::atom(conn, "UTF8_STRING")?,
            wm_name: x11::atom(conn, "WM_NAME")?,
            wm_class: x11::atom(conn, "WM_CLASS")?,
            wm_pid: x11::atom(conn, "_NET_WM_PID")?,
            wm_desktop: x11::atom(conn, "_NET_WM_DESKTOP")?,
            frame_extents: x11::atom(conn, "_NET_FRAME_EXTENTS")?,
            moveresize: x11::atom(conn, "_NET_MOVERESIZE_WINDOW")?,
            wm_protocols: x11::atom(conn, "WM_PROTOCOLS")?,
            wm_delete: x11::atom(conn, "WM_DELETE_WINDOW")?,
            wm_state: x11::atom(conn, "WM_STATE")?,
            wm_change_state: x11::atom(conn, "WM_CHANGE_STATE")?,
            net_wm_state: x11::atom(conn, "_NET_WM_STATE")?,
            state_maximized_vert: x11::atom(conn, "_NET_WM_STATE_MAXIMIZED_VERT")?,
            state_maximized_horz: x11::atom(conn, "_NET_WM_STATE_MAXIMIZED_HORZ")?,
            state_fullscreen: x11::atom(conn, "_NET_WM_STATE_FULLSCREEN")?,
            state_above: x11::atom(conn, "_NET_WM_STATE_ABOVE")?,
        })
    }
}

pub fn list(display: &str) -> Result<Vec<WindowInfo>> {
    let (conn, screen_num) = x11::connect(display)?;
    let root = x11::root(&conn, screen_num);
    let atoms = Atoms::new(&conn)?;

    // Prefer the stacking list when the window manager provides it; it is
    // bottom-to-top, the same order as _NET_CLIENT_LIST.
    let clients = x11::get_u32s(
        &conn,
        root,
        atoms.client_list_stacking,
        AtomEnum::WINDOW.into(),
    )?
    .filter(|list| !list.is_empty())
    .or(x11::get_u32s(
        &conn,
        root,
        atoms.client_list,
        AtomEnum::WINDOW.into(),
    )?)
    .unwrap_or_default();
    let clients = if clients.is_empty() {
        fallback_clients(&conn, root, &atoms)?
    } else {
        clients
    };

    let active = x11::get_u32s(&conn, root, atoms.active_window, AtomEnum::WINDOW.into())?
        .and_then(|ids| ids.first().copied())
        .or_else(|| {
            conn.get_input_focus()
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| reply.focus)
        });

    let mut out: Vec<WindowInfo> = Vec::new();
    for id in clients {
        if id == root {
            continue;
        }
        let Ok(attr_cookie) = conn.get_window_attributes(id) else {
            continue;
        };
        let Ok(attr) = attr_cookie.reply() else {
            continue;
        };
        if attr.class != WindowClass::INPUT_OUTPUT || attr.override_redirect {
            continue;
        }

        let title = x11::get_string(&conn, id, atoms.net_wm_name, atoms.utf8)?
            .or(x11::get_string(
                &conn,
                id,
                atoms.wm_name,
                AtomEnum::STRING.into(),
            )?)
            .map(|text| sanitize(&text))
            .unwrap_or_default();

        let (instance, class) = wm_class(&conn, id, atoms.wm_class)?;
        if title.is_empty() && class.is_none() {
            // Toolkit helper windows and unmapped filler.
            continue;
        }

        let Ok(geom_cookie) = conn.get_geometry(id) else {
            continue;
        };
        let Ok(geom) = geom_cookie.reply() else {
            continue;
        };
        if geom.width <= 1 || geom.height <= 1 {
            continue;
        }
        let (x, y) = match conn.translate_coordinates(id, root, 0, 0) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => (reply.dst_x, reply.dst_y),
                Err(_) => continue,
            },
            Err(_) => continue,
        };

        let pid = x11::get_u32s(&conn, id, atoms.wm_pid, AtomEnum::CARDINAL.into())?
            .and_then(|ids| ids.first().copied());
        let desktop = x11::get_u32s(&conn, id, atoms.wm_desktop, AtomEnum::CARDINAL.into())?
            .and_then(|ids| ids.first().copied())
            .filter(|desktop| *desktop != 0xFFFF_FFFF);
        let frame = x11::get_u32s(&conn, id, atoms.frame_extents, AtomEnum::CARDINAL.into())?
            .filter(|values| values.len() >= 4)
            .map(|values| (values[0], values[1], values[2], values[3]));

        out.push(WindowInfo {
            id,
            title,
            instance,
            class,
            pid,
            x,
            y,
            width: geom.width,
            height: geom.height,
            mapped: attr.map_state == MapState::VIEWABLE,
            focused: active == Some(id),
            desktop,
            frame,
            stack: 0,
        });
    }

    // _NET_CLIENT_LIST is bottom-to-top; present the topmost window first.
    if !out.is_empty() {
        out.reverse();
        for (index, window) in out.iter_mut().enumerate() {
            window.stack = index;
        }
    }
    Ok(out)
}

fn fallback_clients(conn: &RustConnection, root: Window, atoms: &Atoms) -> Result<Vec<u32>> {
    let tree = conn.query_tree(root)?.reply()?;
    let mut out = Vec::new();
    for id in tree.children {
        let Ok(cookie) = conn.get_window_attributes(id) else {
            continue;
        };
        let Ok(attr) = cookie.reply() else {
            continue;
        };
        if attr.class != WindowClass::INPUT_OUTPUT
            || attr.override_redirect
            || attr.map_state != MapState::VIEWABLE
        {
            continue;
        }
        let has_state = x11::get_u32s(conn, id, atoms.wm_state, AtomEnum::ANY.into())?
            .map(|values| !values.is_empty())
            .unwrap_or(false);
        let title = x11::get_string(conn, id, atoms.net_wm_name, atoms.utf8)?.or(x11::get_string(
            conn,
            id,
            atoms.wm_name,
            AtomEnum::STRING.into(),
        )?);
        let (_, class) = wm_class(conn, id, atoms.wm_class)?;
        if has_state || title.is_some() || class.is_some() {
            out.push(id);
        }
    }
    Ok(out)
}

fn wm_class(
    conn: &RustConnection,
    window: Window,
    atom: Atom,
) -> Result<(Option<String>, Option<String>)> {
    let Some(reply) = x11::property(conn, window, atom, AtomEnum::STRING.into(), 64)? else {
        return Ok((None, None));
    };
    let parts: Vec<&[u8]> = reply.value.split(|byte| *byte == 0).collect();
    let to_string = |bytes: Option<&&[u8]>| {
        bytes
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
    };
    Ok((to_string(parts.first()), to_string(parts.get(1))))
}

fn sanitize(text: &str) -> String {
    text.replace(['\n', '\r', '\t'], " ").trim().to_string()
}

fn active_id(conn: &RustConnection, root: Window, atoms: &Atoms) -> Option<Window> {
    x11::get_u32s(conn, root, atoms.active_window, AtomEnum::WINDOW.into())
        .ok()
        .flatten()
        .and_then(|ids| ids.first().copied())
        .or_else(|| {
            conn.get_input_focus()
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|reply| reply.focus)
        })
}

pub fn focus(display: &str, id: Window) -> Result<Value> {
    let (conn, screen_num) = x11::connect(display)?;
    let root = x11::root(&conn, screen_num);
    let atoms = Atoms::new(&conn)?;

    let before = active_id(&conn, root, &atoms);
    // source indication 2 = pager/tool, timestamp 0 = current time.
    x11::send_client_message(
        &conn,
        root,
        id,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        atoms.active_window,
        [2, 0, 0, 0, 0],
    )?;

    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        if active_id(&conn, root, &atoms) == Some(id) {
            return Ok(json!({"id": id, "focused": true, "previous": before}));
        }
        std::thread::sleep(Duration::from_millis(40));
    }

    // No EWMH window manager, or it ignored the request.
    conn.set_input_focus(x11rb::protocol::xproto::InputFocus::PARENT, id, 0u32)?
        .check()?;
    conn.configure_window(
        id,
        &ConfigureWindowAux::new().stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
    )?
    .check()?;
    conn.flush()?;
    let focused = active_id(&conn, root, &atoms) == Some(id)
        || conn
            .get_input_focus()?
            .reply()
            .map(|reply| reply.focus == id)
            .unwrap_or(false);
    Ok(json!({"id": id, "focused": focused, "previous": before, "fallback": true}))
}

pub fn move_resize(
    display: &str,
    id: Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
) -> Result<Value> {
    let (conn, screen_num) = x11::connect(display)?;
    let root = x11::root(&conn, screen_num);
    let atoms = Atoms::new(&conn)?;

    if width == 0 || height == 0 {
        return Err(anyhow!("width and height must be greater than zero"));
    }

    // WMs disagree about whether the coordinates in _NET_MOVERESIZE_WINDOW
    // name the client or its frame.  Ask, measure the client, and correct by
    // the observed delta.  Two correction rounds handle decorated windows.
    let flags: u32 = 1 | 2 | 4 | 8;
    let gravity: u32 = 10;
    let mut request_x = x;
    let mut request_y = y;
    let target = (x, y, width, height);

    for attempt in 0..3 {
        let data = [
            gravity | (flags << 8) | (2 << 12),
            request_x as u32,
            request_y as u32,
            width as u32,
            height as u32,
        ];
        x11::send_client_message(
            &conn,
            root,
            id,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            atoms.moveresize,
            data,
        )?;
        std::thread::sleep(Duration::from_millis(180));

        if let Ok((actual_x, actual_y, actual_w, actual_h)) = x11::window_rect(&conn, id) {
            if (actual_x, actual_y, actual_w, actual_h) == target {
                return Ok(geometry_json(&conn, id, false));
            }
            request_x += x - actual_x;
            request_y += y - actual_y;
            if attempt == 2 {
                break;
            }
        }
    }

    // No EWMH window manager, or it refused to move the client.  Ask X
    // directly; a manager may still override this, which the final geometry
    // reports honestly.
    conn.configure_window(
        id,
        &ConfigureWindowAux::new()
            .x(x as i32)
            .y(y as i32)
            .width(width as u32)
            .height(height as u32),
    )?
    .check()?;
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(120));
    Ok(geometry_json(&conn, id, true))
}

fn geometry_json(conn: &RustConnection, id: Window, fallback: bool) -> Value {
    match x11::window_rect(conn, id) {
        Ok((x, y, width, height)) => json!({
            "id": id,
            "x": x,
            "y": y,
            "width": width,
            "height": height,
            "fallback": fallback,
        }),
        Err(err) => json!({"id": id, "error": err.to_string(), "fallback": fallback}),
    }
}

/// Apply a window-management action through EWMH.  `action=state` only
/// reports the current state.
pub fn action(display: &str, id: Window, action: &str) -> Result<Value> {
    let (conn, screen_num) = x11::connect(display)?;
    let root = x11::root(&conn, screen_num);
    let atoms = Atoms::new(&conn)?;

    // Verify the window still exists before sending anything.
    let _ = conn
        .get_window_attributes(id)?
        .reply()
        .context("window does not exist")?;

    let normalized = action.trim().to_ascii_lowercase().replace('-', "_");
    let mut applied = normalized.clone();
    let mut minimize = false;

    match normalized.as_str() {
        "minimize" | "minimise" | "iconify" => {
            minimize = true;
            let data = [3u32, 0, 0, 0, 0]; // IconicState
            x11::send_client_message(
                &conn,
                root,
                id,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                atoms.wm_change_state,
                data,
            )?;
        }
        "maximize" | "maximise" => {
            set_state(
                &conn,
                root,
                id,
                &atoms,
                1,
                atoms.state_maximized_vert,
                atoms.state_maximized_horz,
            )?;
        }
        "unmaximize" | "unmaximise" => {
            set_state(
                &conn,
                root,
                id,
                &atoms,
                0,
                atoms.state_maximized_vert,
                atoms.state_maximized_horz,
            )?;
        }
        "restore" => {
            set_state(
                &conn,
                root,
                id,
                &atoms,
                0,
                atoms.state_maximized_vert,
                atoms.state_maximized_horz,
            )?;
            set_state(
                &conn,
                root,
                id,
                &atoms,
                0,
                atoms.state_fullscreen,
                x11rb::NONE,
            )?;
            conn.map_window(id)?;
        }
        "fullscreen" | "full_screen" => {
            set_state(
                &conn,
                root,
                id,
                &atoms,
                1,
                atoms.state_fullscreen,
                x11rb::NONE,
            )?;
        }
        "unfullscreen" | "exit_fullscreen" => {
            set_state(
                &conn,
                root,
                id,
                &atoms,
                0,
                atoms.state_fullscreen,
                x11rb::NONE,
            )?;
        }
        "raise" | "above" => {
            conn.configure_window(
                id,
                &ConfigureWindowAux::new().stack_mode(x11rb::protocol::xproto::StackMode::ABOVE),
            )?
            .check()?;
        }
        "lower" | "below" => {
            conn.configure_window(
                id,
                &ConfigureWindowAux::new().stack_mode(x11rb::protocol::xproto::StackMode::BELOW),
            )?
            .check()?;
        }
        "set_always_on_top" | "always_on_top" | "always_above" => {
            set_state(&conn, root, id, &atoms, 1, atoms.state_above, x11rb::NONE)?;
            applied = "set_always_on_top".into();
        }
        "clear_always_on_top" | "unset_always_on_top" | "always_on_top_off" => {
            set_state(&conn, root, id, &atoms, 0, atoms.state_above, x11rb::NONE)?;
            applied = "clear_always_on_top".into();
        }
        "state" | "query" => {
            applied = "state".into();
        }
        other => {
            return Err(anyhow!(
                "unknown window action {other:?}; use minimize, maximize, unmaximize, restore, \
                 fullscreen, unfullscreen, raise, lower, set_always_on_top, \
                 clear_always_on_top, or state"
            ))
        }
    }
    conn.flush()?;

    // Give the manager time to apply the request; minimization is async.
    let deadline = Instant::now() + Duration::from_millis(if minimize { 1200 } else { 350 });
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let minimized = conn
            .get_window_attributes(id)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map(|attr| attr.map_state != MapState::VIEWABLE)
            .unwrap_or(false);
        if !minimize || minimized || Instant::now() >= deadline {
            break;
        }
    }

    let names = state_names(&conn, id, &atoms)?;
    let minimized = conn
        .get_window_attributes(id)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|attr| attr.map_state != MapState::VIEWABLE)
        .unwrap_or(false);
    let geometry = x11::window_rect(&conn, id).ok();
    let focused = active_id(&conn, root, &atoms) == Some(id);

    Ok(json!({
        "id": id,
        "action": applied,
        "requested": action,
        "states": names,
        "minimized": minimized,
        "fullscreen": names.iter().any(|name| name == "_NET_WM_STATE_FULLSCREEN"),
        "maximized": names.iter().any(|name| name == "_NET_WM_STATE_MAXIMIZED_VERT")
            && names.iter().any(|name| name == "_NET_WM_STATE_MAXIMIZED_HORZ"),
        "always_on_top": names.iter().any(|name| name == "_NET_WM_STATE_ABOVE"),
        "focused": focused,
        "geometry": geometry.map(|(x, y, width, height)| json!({
            "x": x, "y": y, "width": width, "height": height
        })),
    }))
}

fn set_state(
    conn: &RustConnection,
    root: Window,
    id: Window,
    atoms: &Atoms,
    operation: u32,
    first: Atom,
    second: Atom,
) -> Result<()> {
    x11::send_client_message(
        conn,
        root,
        id,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        atoms.net_wm_state,
        [operation, first, second, 1, 0],
    )?;
    Ok(())
}

fn state_names(conn: &RustConnection, id: Window, atoms: &Atoms) -> Result<Vec<String>> {
    let values =
        x11::get_u32s(conn, id, atoms.net_wm_state, AtomEnum::ATOM.into())?.unwrap_or_default();
    let mut names = Vec::new();
    for atom in values {
        if let Ok(cookie) = conn.get_atom_name(atom) {
            if let Ok(reply) = cookie.reply() {
                names.push(String::from_utf8_lossy(&reply.name).into_owned());
            }
        }
    }
    Ok(names)
}

pub fn close(display: &str, id: Window, force: bool) -> Result<Value> {
    let (conn, screen_num) = x11::connect(display)?;
    let _ = screen_num;
    let atoms = Atoms::new(&conn)?;

    let protocols =
        x11::get_u32s(&conn, id, atoms.wm_protocols, AtomEnum::ATOM.into())?.unwrap_or_default();
    let supports_delete = protocols.contains(&atoms.wm_delete);

    let mut requested = false;
    if supports_delete {
        x11::send_client_message(
            &conn,
            id,
            id,
            EventMask::NO_EVENT,
            atoms.wm_protocols,
            [atoms.wm_delete, 0, 0, 0, 0],
        )?;
        requested = true;
    }

    let deadline = Instant::now() + Duration::from_millis(1500);
    let mut closed = false;
    while Instant::now() < deadline {
        match conn.get_window_attributes(id) {
            Ok(cookie) => match cookie.reply() {
                Ok(attr) => {
                    if attr.map_state != MapState::VIEWABLE {
                        closed = true;
                        break;
                    }
                }
                Err(_) => {
                    closed = true;
                    break;
                }
            },
            Err(_) => {
                closed = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut killed = false;
    if !closed && force {
        conn.kill_client(id)?.check()?;
        conn.flush()?;
        killed = true;
        closed = true;
    }

    Ok(json!({
        "id": id,
        "delete_requested": requested,
        "closed": closed,
        "killed": killed,
        "force_used": !requested && closed,
    }))
}
