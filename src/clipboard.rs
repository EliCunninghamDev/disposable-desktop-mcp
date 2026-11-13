//! Clipboard access through the X CLIPBOARD selection.
//!
//! Reading issues a `ConvertSelection` request and waits for `SelectionNotify`.
//! Writing creates a small owner window and keeps an event loop alive in a
//! background thread so any application can request the contents later.  Both
//! paths connect directly to the nested display; the host clipboard is never
//! involved.

use crate::x11;
use anyhow::{anyhow, Context, Result};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;

const TIMEOUT: Duration = Duration::from_secs(3);
const MAX_PROPERTY_UNITS: u32 = 1 << 20;

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    text: Atom,
    property: Atom,
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Self> {
        Ok(Self {
            clipboard: x11::atom(conn, "CLIPBOARD")?,
            targets: x11::atom(conn, "TARGETS")?,
            utf8: x11::atom(conn, "UTF8_STRING")?,
            text: x11::atom(conn, "TEXT")?,
            property: x11::atom(conn, "DISPOSABLE_DESKTOP_MCP_CLIPBOARD")?,
        })
    }
}

pub fn get(display: &str) -> Result<String> {
    let (conn, screen_num) = x11rb::connect(Some(display))
        .with_context(|| format!("cannot connect to nested display {display}"))?;
    let atoms = Atoms::new(&conn)?;
    let owner = conn
        .get_selection_owner(atoms.clipboard)?
        .reply()
        .context("cannot query the CLIPBOARD owner")?
        .owner;
    if owner == x11rb::NONE {
        return Ok(String::new());
    }

    let window = create_window(&conn, screen_num)?;
    let result = (|| {
        for target in [atoms.utf8, atoms.text, AtomEnum::STRING.into()] {
            if let Some(bytes) = request(
                &conn,
                window,
                atoms.clipboard,
                target,
                atoms.property,
                TIMEOUT,
            )? {
                return Ok(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
        Ok(String::new())
    })();
    let _ = conn.destroy_window(window);
    let _ = conn.flush();
    result
}

/// Take ownership of CLIPBOARD and serve requests on a background thread.
/// A later `clipboard_set` replaces this owner, at which point the old thread
/// observes `SelectionClear` and exits.
pub fn set(display: &str, text: &str) -> Result<usize> {
    if text.len() > 1 << 20 {
        return Err(anyhow!(
            "clipboard text is {} bytes; the direct selection limit is 1 MiB",
            text.len()
        ));
    }
    let (conn, screen_num) = x11rb::connect(Some(display))
        .with_context(|| format!("cannot connect to nested display {display}"))?;
    let atoms = Atoms::new(&conn)?;
    let window = create_window(&conn, screen_num)?;

    conn.set_selection_owner(window, atoms.clipboard, x11rb::CURRENT_TIME)?;
    conn.flush()?;
    let owner = conn
        .get_selection_owner(atoms.clipboard)?
        .reply()
        .context("cannot verify the CLIPBOARD owner")?
        .owner;
    if owner != window {
        let _ = conn.destroy_window(window);
        return Err(anyhow!(
            "another client took ownership of CLIPBOARD before the set completed"
        ));
    }

    let bytes = text.as_bytes().to_vec();
    let served = bytes.len();
    std::thread::Builder::new()
        .name("disposable-desktop-clipboard".into())
        .spawn(move || serve(conn, window, atoms, bytes))
        .context("cannot start the clipboard owner thread")?;
    Ok(served)
}

fn create_window(conn: &RustConnection, screen_num: usize) -> Result<u32> {
    let screen = &conn.setup().roots[screen_num];
    let window = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    Ok(window)
}

fn request(
    conn: &RustConnection,
    window: u32,
    selection: Atom,
    target: Atom,
    property: Atom,
    timeout: Duration,
) -> Result<Option<Vec<u8>>> {
    conn.convert_selection(window, selection, target, property, x11rb::CURRENT_TIME)?;
    conn.flush()?;

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match conn.poll_for_event()? {
            Some(x11rb::protocol::Event::SelectionNotify(event))
                if event.requestor == window && event.selection == selection =>
            {
                if event.property == x11rb::NONE {
                    return Ok(None);
                }
                let reply = conn
                    .get_property(
                        true,
                        window,
                        event.property,
                        AtomEnum::ANY,
                        0,
                        MAX_PROPERTY_UNITS,
                    )?
                    .reply()?;
                return Ok(Some(reply.value));
            }
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    Err(anyhow!("timed out waiting for the clipboard owner"))
}

fn serve(conn: RustConnection, window: u32, atoms: Atoms, bytes: Vec<u8>) {
    while let Ok(event) = conn.wait_for_event() {
        match event {
            x11rb::protocol::Event::SelectionRequest(request) => {
                let property = if request.property == x11rb::NONE {
                    request.target
                } else {
                    request.property
                };
                let (type_, format, data, units) = if request.target == atoms.targets {
                    let targets = [
                        atoms.utf8,
                        atoms.text,
                        AtomEnum::STRING.into(),
                        atoms.targets,
                    ];
                    let mut encoded = Vec::with_capacity(targets.len() * 4);
                    for target in targets {
                        encoded.extend_from_slice(&target.to_le_bytes());
                    }
                    (AtomEnum::ATOM.into(), 32, encoded, targets.len() as u32)
                } else if request.target == atoms.utf8 || request.target == atoms.text {
                    (atoms.utf8, 8, bytes.clone(), bytes.len() as u32)
                } else if request.target == Atom::from(AtomEnum::STRING) {
                    (
                        AtomEnum::STRING.into(),
                        8,
                        bytes.clone(),
                        bytes.len() as u32,
                    )
                } else {
                    (x11rb::NONE, 8, Vec::new(), 0)
                };

                let success = if data.is_empty() {
                    false
                } else {
                    conn.change_property(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        type_,
                        format,
                        units,
                        &data,
                    )
                    .is_ok()
                };
                let _ = conn.send_event(
                    false,
                    request.requestor,
                    EventMask::NO_EVENT,
                    SelectionNotifyEvent {
                        response_type: x11rb::protocol::xproto::SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: request.time,
                        requestor: request.requestor,
                        selection: request.selection,
                        target: request.target,
                        property: if success { property } else { x11rb::NONE },
                    },
                );
                let _ = conn.flush();
            }
            x11rb::protocol::Event::SelectionClear(clear) if clear.selection == atoms.clipboard => {
                break;
            }
            _ => {}
        }
    }
    let _ = conn.destroy_window(window);
    let _ = conn.flush();
}
