//! Live viewer for one or more xephyr-mcp displays.
//!
//! The viewer is a small GTK4 application.  It captures each nested root window
//! with GetImage, uploads the frame as a texture, and paints one pane (single
//! mode) or a balanced BSP tiling of every live session.  GTK does the
//! compositing, double buffering, input handling and native session dropdown,
//! so the window does not flicker and the controls behave like normal widgets.
//!
//! The viewer is read-only until a person claims control with `c`: it then
//! forwards mouse, wheel, and keyboard events to the selected session and the
//! server refuses agent input until control is released.

use anyhow::{anyhow, bail, Result};
use glib::translate::IntoGlib;
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{cairo, gdk, glib};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode, Window};
use x11rb::protocol::xtest;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::backend::{Backend, CaptureTarget};
use crate::control::{self, Owner};
use crate::state::{self, State};
use crate::{screenshot, wl_input, x11};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const TOOLBAR_H: i32 = 38;
const MAX_PANES: usize = 8;
const REFRESH: Duration = Duration::from_secs(2);
const APP_ID: &str = "dev.xephyr.mcp.Viewer";

const COL_BG: (f32, f32, f32) = (0.055, 0.058, 0.068);
const COL_PANE: (f32, f32, f32) = (0.035, 0.037, 0.043);
const COL_BORDER: (f32, f32, f32) = (0.29, 0.30, 0.33);
const COL_TEXT: (f32, f32, f32) = (0.91, 0.92, 0.93);
const COL_DIM: (f32, f32, f32) = (0.66, 0.68, 0.71);
const COL_ERROR: (f32, f32, f32) = (0.95, 0.45, 0.42);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Layout {
    Single,
    Bsp,
}

#[derive(Clone, Copy, Debug)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn inset(&self, amount: i32) -> Rect {
        Rect {
            x: self.x + amount,
            y: self.y + amount,
            w: (self.w - amount * 2).max(1),
            h: (self.h - amount * 2).max(1),
        }
    }
}

#[derive(Clone, Debug)]
struct Target {
    display: String,
    width: u16,
    height: u16,
    session_id: Option<String>,
    started_at: u64,
    backend: Backend,
}

impl Target {
    /// Wayland sessions all use `wayland-1` in their private runtime dirs, so
    /// the session id is the identity; X11 falls back to the display name.
    fn key(&self) -> String {
        self.session_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| self.display.clone())
    }

    fn label(&self) -> String {
        self.session_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| self.display.clone())
    }

    fn menu_label(&self) -> String {
        format!(
            "{}  -  {}  {}x{}  {}",
            self.label(),
            self.display,
            self.width,
            self.height,
            self.backend.kind().as_str()
        )
    }
}

#[derive(Debug, Clone)]
struct Options {
    display: Option<String>,
    session: Option<String>,
    list_sessions: bool,
    host_display: Option<String>,
    title: Option<String>,
    fps: u32,
    scale: Option<f64>,
    cursor: bool,
    aspect_lock: bool,
    state_file: std::path::PathBuf,
    fullscreen: bool,
    layout: Layout,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            display: None,
            session: None,
            list_sessions: false,
            host_display: None,
            title: None,
            fps: 15,
            scale: None,
            cursor: true,
            aspect_lock: true,
            state_file: crate::env::state_file(),
            fullscreen: false,
            layout: Layout::Single,
        }
    }
}

fn usage() -> &'static str {
    "xephyr-mcp-viewer - live view of xephyr-mcp nested displays

USAGE:
    xephyr-mcp-viewer [OPTIONS]

OPTIONS:
    --display :N          Nested display to watch
    --session ID          Select a session started with a caller-supplied session_id
    --list                List discoverable sessions and exit
    --layout single|bsp   Single session (default) or tile all live sessions
    --bsp                 Shorthand for --layout bsp
    --host-display :N     Display for the viewer window (defaults to $DISPLAY)
    --state-file PATH     Session state file (default: $XDG_RUNTIME_DIR/xephyr-mcp/current.json)
    --title TEXT          Window title
    --fps N               Capture rate, 1-60 (default 15; capped at 10 in BSP)
    --scale N             Window size as a fraction of native pixels (default: fit 90% of host screen)
    --fullscreen          Start fullscreen
    --no-cursor           Do not draw the nested pointer
    --no-aspect-lock      Do not lock the window to the display aspect ratio
    -h, --help            Show this help

The toolbar has a session dropdown, Single/BSP buttons, and a Control toggle.
Keys: c claim human control, Ctrl+Shift+C release it (the Control button also
toggles), q quit, f / F11 fullscreen, t toggle BSP, s cycle sessions.  While
human control is active every other key, including Esc, is forwarded to the
session; Ctrl+Shift+Q closes the viewer.  Input is forwarded only in Single
mode.

The viewer prefers the X11 backend when an X display is available so that the
window aspect hint and window management behave consistently; set GDK_BACKEND
to override."
}

fn parse_options() -> Result<Option<Options>> {
    let mut options = Options::default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let mut next = |name: &str| -> Result<String> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| anyhow!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--display" => options.display = Some(next("--display")?),
            "--session" => options.session = Some(next("--session")?),
            "--list" => options.list_sessions = true,
            "--layout" => {
                options.layout = match next("--layout")?.to_ascii_lowercase().as_str() {
                    "single" => Layout::Single,
                    "bsp" | "tile" | "tiling" => Layout::Bsp,
                    other => bail!("unknown layout {other:?}; use single or bsp"),
                }
            }
            "--bsp" => options.layout = Layout::Bsp,
            "--host-display" => options.host_display = Some(next("--host-display")?),
            "--state-file" => options.state_file = std::path::PathBuf::from(next("--state-file")?),
            "--title" => options.title = Some(next("--title")?),
            "--fps" => {
                options.fps = next("--fps")?
                    .parse::<u32>()
                    .map_err(|_| anyhow!("--fps must be a number"))?
                    .clamp(1, 60);
            }
            "--scale" => {
                options.scale = Some(
                    next("--scale")?
                        .parse::<f64>()
                        .map_err(|_| anyhow!("--scale must be a number"))?
                        .clamp(0.1, 4.0),
                );
            }
            "--fullscreen" => options.fullscreen = true,
            "--no-cursor" => options.cursor = false,
            "--no-aspect-lock" => options.aspect_lock = false,
            other => bail!("unknown argument {other:?}\n\n{}", usage()),
        }
        index += 1;
    }
    Ok(Some(options))
}

fn target_from_state(state: &State) -> Option<Target> {
    let backend = Backend::from_state(state);
    let (width, height) = match &backend {
        Backend::X11 { display } => {
            let (conn, screen_num) = x11::connect(display).ok()?;
            x11::screen_size(&conn, screen_num)
        }
        Backend::Wayland {
            swaysock, output, ..
        } => {
            let expected = (!output.is_empty()).then_some(output.as_str());
            match crate::sway::output_size(swaysock, expected) {
                Ok((width, height)) => (
                    width.min(u16::MAX as u32) as u16,
                    height.min(u16::MAX as u32) as u16,
                ),
                Err(_) if state.width > 0 && state.height > 0 => {
                    (state.width as u16, state.height as u16)
                }
                Err(_) => return None,
            }
        }
    };
    Some(Target {
        display: state.display.clone(),
        width,
        height,
        session_id: Some(state.session_id.clone()).filter(|id| !id.is_empty()),
        started_at: state.started_at,
        backend,
    })
}

fn scan_displays(session: Option<&str>) -> Vec<Target> {
    let mut targets = Vec::new();
    for number in 100u16..=250 {
        if !std::path::Path::new(&format!("/tmp/.X11-unix/X{number}")).exists() {
            continue;
        }
        let display = format!(":{number}");
        let Ok((conn, screen_num)) = x11::connect(&display) else {
            continue;
        };
        let Some(text) = screenshot::session_marker(&conn, screen_num) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<State>(&text) else {
            continue;
        };
        if let Some(wanted) = session {
            if state.session_id != wanted {
                continue;
            }
        }
        if let Some(target) = target_from_state(&state) {
            targets.push(target);
        }
    }
    targets
}

fn discover_sessions() -> Vec<Target> {
    let mut targets: Vec<Target> = Vec::new();
    for state in state::list_sessions() {
        if let Some(target) = target_from_state(&state) {
            targets.push(target);
        }
    }
    for target in scan_displays(None) {
        if !targets.iter().any(|item| item.key() == target.key()) {
            targets.push(target);
        }
    }
    targets.sort_by_key(|target| std::cmp::Reverse(target.started_at));
    targets.dedup_by(|a, b| a.key() == b.key());
    targets
}

fn discover_target(options: &Options) -> Result<Target> {
    if let Some(display) = &options.display {
        let (conn, screen_num) = x11::connect(display)?;
        let (width, height) = x11::screen_size(&conn, screen_num);
        let state = screenshot::session_marker(&conn, screen_num)
            .and_then(|text| serde_json::from_str::<State>(&text).ok());
        return Ok(Target {
            display: display.clone(),
            width,
            height,
            session_id: state
                .as_ref()
                .map(|state| state.session_id.clone())
                .filter(|id| !id.is_empty()),
            started_at: state.map(|state| state.started_at).unwrap_or(0),
            backend: Backend::X11 {
                display: display.clone(),
            },
        });
    }

    if let Some(session) = &options.session {
        if let Some(state) = state::read_session(session) {
            if let Some(target) = target_from_state(&state) {
                return Ok(target);
            }
        }
        if let Some(target) = scan_displays(Some(session)).into_iter().next() {
            return Ok(target);
        }
        bail!("session {session:?} was not found; use --list to see active sessions");
    }

    if options.state_file != crate::env::state_file() {
        if let Some(state) = state::read_from(&options.state_file) {
            if let Some(target) = target_from_state(&state) {
                return Ok(target);
            }
        }
    }

    if let Some(state) = state::read_current() {
        if let Some(target) = target_from_state(&state) {
            return Ok(target);
        }
    }

    if let Some(target) = discover_sessions().into_iter().next() {
        return Ok(target);
    }

    bail!(
        "no xephyr-mcp display found. Start one with display_start, pass --display :N or \
         --session ID, or use --list. (state file: {})",
        options.state_file.display()
    )
}

/// X11 capture plus XTEST input for a pane.  Wayland panes use the library
/// Backend instead of a long-lived X connection.
struct X11View {
    conn: RustConnection,
    screen: usize,
    root: Window,
    keymap: HashMap<u32, u8>,
    mods: u32,
}

impl X11View {
    fn new(conn: RustConnection, screen: usize) -> Self {
        let root = x11::root(&conn, screen);
        let mut keymap = HashMap::new();
        let setup = conn.setup();
        let min = setup.min_keycode;
        let max = setup.max_keycode;
        if max >= min {
            if let Some(reply) = conn
                .get_keyboard_mapping(min, max - min + 1)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
            {
                let per = reply.keysyms_per_keycode.max(1) as usize;
                for (index, chunk) in reply.keysyms.chunks(per).enumerate() {
                    for sym in chunk {
                        if *sym != 0 {
                            keymap.entry(*sym).or_insert((min as usize + index) as u8);
                        }
                    }
                }
            }
        }
        Self {
            conn,
            screen,
            root,
            keymap,
            mods: 0,
        }
    }

    fn pointer(&self) -> Option<(i16, i16)> {
        self.conn
            .query_pointer(self.root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .filter(|reply| reply.same_screen)
            .map(|reply| (reply.root_x, reply.root_y))
    }

    fn motion(&self, x: i32, y: i32) {
        let _ = xtest::fake_input(
            &self.conn,
            6, // MotionNotify
            0,
            0,
            self.root,
            x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            0,
        )
        .map(|cookie| cookie.check());
    }

    fn button(&self, button: u32, down: bool) {
        let _ = xtest::fake_input(
            &self.conn,
            if down { 4 } else { 5 }, // ButtonPress / ButtonRelease
            button.min(255) as u8,
            0,
            self.root,
            0,
            0,
            0,
        )
        .map(|cookie| cookie.check());
    }

    fn raw_key(&self, keycode: u8, down: bool) {
        let _ = xtest::fake_input(
            &self.conn,
            if down { 2 } else { 3 }, // KeyPress / KeyRelease
            keycode,
            0,
            self.root,
            0,
            0,
            0,
        )
        .map(|cookie| cookie.check());
    }

    fn sync_mods(&mut self, modifiers: wl_input::Modifiers) {
        let desired = (if modifiers.shift { 1 } else { 0 })
            | (if modifiers.ctrl { 4 } else { 0 })
            | (if modifiers.alt { 8 } else { 0 })
            | (if modifiers.super_key { 64 } else { 0 });
        for (bit, keycode) in [(1u32, 50u8), (4, 37), (8, 64), (64, 133)] {
            let had = self.mods & bit != 0;
            let want = desired & bit != 0;
            if want && !had {
                self.raw_key(keycode, true);
            } else if had && !want {
                self.raw_key(keycode, false);
            }
        }
        self.mods = desired;
    }

    fn key(&mut self, keysym: u32, modifiers: wl_input::Modifiers, down: bool) {
        self.sync_mods(modifiers);
        if let Some(keycode) = self.keymap.get(&keysym).copied() {
            self.raw_key(keycode, down);
        }
    }

    fn scroll(&mut self, up: bool, amount: u32) {
        let button = if up { 4 } else { 5 };
        for _ in 0..amount.clamp(1, 100) {
            self.button(button, true);
            self.button(button, false);
        }
    }
}

struct Pane {
    target: Target,
    x11: Option<X11View>,
    texture: Option<cairo::ImageSurface>,
    pointer: Option<(i16, i16)>,
    error: Option<String>,
    /// Screen rectangle of the painted frame, used to map viewer input back
    /// into nested display coordinates.
    last_inner: Option<Rect>,
}

struct Viewer {
    window: gtk::ApplicationWindow,
    canvas: gtk::DrawingArea,
    dropdown: gtk::DropDown,
    single_button: gtk::ToggleButton,
    bsp_button: gtk::ToggleButton,
    control_button: gtk::ToggleButton,
    sessions: Vec<Target>,
    labels: Vec<String>,
    panes: Vec<Pane>,
    selected: String,
    layout: Layout,
    aspect_lock: bool,
    cursor: bool,
    fullscreen: bool,
    pinned: Option<Target>,
    syncing: bool,
    fps: u32,
    control: Owner,
    /// True only when this viewer's PID owns the human claim.
    control_owned: bool,
    last_pointer: Option<(f64, f64)>,
}

pub fn run() -> Result<()> {
    let Some(options) = parse_options()? else {
        println!("{}", usage());
        return Ok(());
    };

    if options.list_sessions {
        let sessions: Vec<serde_json::Value> = state::list_sessions()
            .into_iter()
            .map(|state| {
                serde_json::json!({
                    "session_id": state.session_id,
                    "display": state.display,
                    "width": state.width,
                    "height": state.height,
                    "visible": state.visible,
                    "server_pid": state.server_pid,
                    "alive": x11::connect(&state.display).is_ok(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(());
    }

    if let Some(display) = &options.host_display {
        std::env::set_var("DISPLAY", display);
    }
    // Prefer X11 when it is actually reachable: it keeps the aspect hint and
    // window management deterministic on XWayland, and matches the rest of the
    // tooling.  Wayland-only sessions fall back to the default GDK backend.
    if std::env::var_os("GDK_BACKEND").is_none() && x11rb::connect(None).is_ok() {
        std::env::set_var("GDK_BACKEND", "x11");
    }

    let first = match discover_target(&options) {
        Ok(target) => Some(target),
        Err(err) => {
            if options.display.is_some() || options.session.is_some() {
                return Err(err);
            }
            eprintln!("[xephyr-mcp-viewer] {err}");
            None
        }
    };
    if let Some(target) = &first {
        eprintln!(
            "[xephyr-mcp-viewer] {} showing {} ({}x{})",
            VERSION, target.display, target.width, target.height
        );
    }

    let app = gtk::Application::builder()
        .application_id(APP_ID)
        // Each viewer invocation is its own process/window.  Without this GTK
        // forwards activation to an existing instance, which opens a second
        // window in that process and makes the new process exit immediately.
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let options_for_ui = options.clone();
    let first_for_ui = first.clone();
    app.connect_activate(move |app| {
        if let Err(err) = build_ui(app, &options_for_ui, first_for_ui.clone()) {
            eprintln!("[xephyr-mcp-viewer] {err:#}");
        }
    });
    // Args are parsed above; keep GApplication from rejecting our options.
    app.run_with_args::<&str>(&[]);
    Ok(())
}

fn build_ui(app: &gtk::Application, options: &Options, first: Option<Target>) -> Result<()> {
    install_css();

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(
            options
                .title
                .clone()
                .unwrap_or_else(|| "xephyr mcp viewer".into()),
        )
        .build();
    window.set_resizable(true);

    let (default_width, content_height) = match &first {
        Some(target) => initial_size(target, options.scale),
        None => (900, 560),
    };
    window.set_default_size(default_width, content_height + TOOLBAR_H);

    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    toolbar.add_css_class("viewer-toolbar");
    toolbar.set_margin_top(4);
    toolbar.set_margin_bottom(4);
    toolbar.set_margin_start(8);
    toolbar.set_margin_end(8);

    let dropdown = gtk::DropDown::from_strings(&["no sessions"]);
    dropdown.set_hexpand(true);
    dropdown.set_size_request(240, -1);
    dropdown.set_tooltip_text(Some("Select a session"));
    toolbar.append(&dropdown);

    let single_button = gtk::ToggleButton::with_label("Single");
    single_button.set_tooltip_text(Some("Show only the selected session"));
    let bsp_button = gtk::ToggleButton::with_label("BSP");
    bsp_button.set_tooltip_text(Some("Tile every live session (up to 8)"));
    single_button.set_group(Some(&bsp_button));
    toolbar.append(&single_button);
    toolbar.append(&bsp_button);

    let control_button = gtk::ToggleButton::with_label("Control");
    control_button.set_tooltip_text(Some(
        "Take human control and forward mouse/keyboard to the session (c)",
    ));
    toolbar.append(&control_button);

    let canvas = gtk::DrawingArea::new();
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    canvas.set_draw_func(|_, _, _, _| {});
    canvas.add_css_class("viewer-canvas");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&toolbar);
    root.append(&canvas);
    window.set_child(Some(&root));

    let selected = first
        .as_ref()
        .map(|target| target.key())
        .unwrap_or_default();
    let viewer = Rc::new(RefCell::new(Viewer {
        window: window.clone(),
        canvas: canvas.clone(),
        dropdown: dropdown.clone(),
        single_button: single_button.clone(),
        bsp_button: bsp_button.clone(),
        control_button: control_button.clone(),
        sessions: Vec::new(),
        labels: Vec::new(),
        panes: Vec::new(),
        selected,
        layout: options.layout,
        aspect_lock: options.aspect_lock,
        cursor: options.cursor,
        fullscreen: options.fullscreen,
        pinned: first,
        syncing: false,
        fps: options.fps,
        control: Owner::Agent,
        control_owned: false,
        last_pointer: None,
    }));

    {
        let viewer = viewer.clone();
        canvas.set_focusable(true);
        canvas.set_draw_func(move |_area, context, width, height| {
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                viewer.draw(context, width, height);
            }
        });
    }

    {
        let viewer = viewer.clone();
        dropdown.connect_selected_notify(move |dropdown| {
            let Ok(mut viewer) = viewer.try_borrow_mut() else {
                return;
            };
            if viewer.syncing {
                return;
            }
            let index = dropdown.selected();
            if index == gtk::INVALID_LIST_POSITION {
                return;
            }
            let Some(target) = viewer.sessions.get(index as usize).cloned() else {
                return;
            };
            if target.key() == viewer.selected {
                return;
            }
            if viewer.control_owned {
                let _ = viewer.set_human(false);
            }
            viewer.selected = target.key();
            viewer.layout = Layout::Single;
            viewer.sync_panes();
            viewer.sync_buttons();
            viewer.update_title();
            viewer.apply_aspect_hint();
            viewer.canvas.queue_draw();
        });
    }

    {
        let viewer = viewer.clone();
        single_button.connect_toggled(move |button| {
            if !button.is_active() {
                return;
            }
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                if !viewer.syncing {
                    viewer.set_layout(Layout::Single);
                }
            }
        });
    }
    {
        let viewer = viewer.clone();
        bsp_button.connect_toggled(move |button| {
            if !button.is_active() {
                return;
            }
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                if !viewer.syncing {
                    viewer.set_layout(Layout::Bsp);
                }
            }
        });
    }

    {
        let key = gtk::EventControllerKey::new();
        // The viewer must see keys before a focused toolbar widget can consume
        // Return or a shortcut chord.
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let released_viewer = viewer.clone();
        let viewer = viewer.clone();
        key.connect_key_pressed(move |_, keyval, _, modifiers| {
            let Ok(mut viewer) = viewer.try_borrow_mut() else {
                return glib::Propagation::Proceed;
            };
            if viewer.control == Owner::Human {
                let chord = modifiers
                    .contains(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK);
                if chord && matches!(keyval, gdk::Key::c | gdk::Key::C) {
                    if viewer.control_owned {
                        if let Err(err) = viewer.set_human(false) {
                            eprintln!("[xephyr-mcp-viewer] release control: {err:#}");
                        }
                    }
                    return glib::Propagation::Stop;
                }
                if chord && matches!(keyval, gdk::Key::q | gdk::Key::Q) {
                    viewer.window.close();
                    return glib::Propagation::Stop;
                }
                if !viewer.control_owned {
                    return glib::Propagation::Proceed;
                }
                return if viewer.forward_key(keyval.into_glib(), modifiers, true) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                };
            }
            if modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
                return glib::Propagation::Proceed;
            }
            let stop = match keyval {
                gdk::Key::c | gdk::Key::C => {
                    if let Err(err) = viewer.set_human(true) {
                        eprintln!("[xephyr-mcp-viewer] claim control: {err:#}");
                    }
                    viewer.sync_buttons();
                    true
                }
                gdk::Key::q | gdk::Key::Q | gdk::Key::Escape => {
                    viewer.window.close();
                    true
                }
                gdk::Key::f | gdk::Key::F | gdk::Key::F11 => {
                    let enabled = !viewer.fullscreen;
                    viewer.set_fullscreen(enabled);
                    true
                }
                gdk::Key::t | gdk::Key::T => {
                    let layout = if viewer.layout == Layout::Bsp {
                        Layout::Single
                    } else {
                        Layout::Bsp
                    };
                    viewer.set_layout(layout);
                    true
                }
                gdk::Key::s | gdk::Key::S => {
                    viewer.cycle_session();
                    true
                }
                _ => false,
            };
            if stop {
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        key.connect_key_released(move |_, keyval, _, modifiers| {
            if let Ok(mut viewer) = released_viewer.try_borrow_mut() {
                if viewer.control == Owner::Human {
                    viewer.forward_key(keyval.into_glib(), modifiers, false);
                }
            }
        });
        window.add_controller(key);
    }

    {
        let viewer = viewer.clone();
        control_button.connect_toggled(move |button| {
            let Ok(mut viewer) = viewer.try_borrow_mut() else {
                return;
            };
            if viewer.syncing {
                return;
            }
            let want = button.is_active();
            if want == viewer.control_owned {
                return;
            }
            if let Err(err) = viewer.set_human(want) {
                eprintln!("[xephyr-mcp-viewer] control: {err:#}");
            }
            viewer.sync_buttons();
        });
    }

    {
        let motion = gtk::EventControllerMotion::new();
        let viewer = viewer.clone();
        motion.connect_motion(move |_, x, y| {
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                viewer.last_pointer = Some((x, y));
                viewer.forward_motion(x, y);
            }
        });
        canvas.add_controller(motion);
    }

    {
        let click = gtk::GestureClick::new();
        click.set_button(0);
        let released_viewer = viewer.clone();
        let viewer = viewer.clone();
        click.connect_pressed(move |gesture, _, x, y| {
            if let Some(widget) = gesture.widget() {
                widget.grab_focus();
            }
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                viewer.last_pointer = Some((x, y));
                viewer.forward_button(gesture.current_button(), x, y, true);
            }
        });
        click.connect_released(move |gesture, _, x, y| {
            if let Ok(mut viewer) = released_viewer.try_borrow_mut() {
                viewer.forward_button(gesture.current_button(), x, y, false);
            }
        });
        canvas.add_controller(click);
    }

    {
        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        let viewer = viewer.clone();
        scroll.connect_scroll(move |_, _dx, dy| {
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                viewer.forward_scroll(dy);
            }
            glib::Propagation::Stop
        });
        canvas.add_controller(scroll);
    }

    {
        let viewer = viewer.clone();
        window.connect_close_request(move |_| {
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                viewer.release_control();
            }
            glib::Propagation::Proceed
        });
    }

    schedule_capture(viewer.clone());
    {
        let viewer = viewer.clone();
        glib::timeout_add_local(REFRESH, move || {
            if let Ok(mut viewer) = viewer.try_borrow_mut() {
                viewer.refresh_sessions();
            }
            glib::ControlFlow::Continue
        });
    }

    {
        let mut viewer = viewer.borrow_mut();
        viewer.refresh_sessions();
        viewer.sync_buttons();
        viewer.update_title();
    }

    window.present();
    if options.fullscreen {
        window.fullscreen();
    }
    {
        let viewer = viewer.clone();
        window.connect_map(move |_| {
            if let Ok(viewer) = viewer.try_borrow() {
                viewer.apply_aspect_hint();
            }
        });
    }
    Ok(())
}

fn initial_size(target: &Target, scale: Option<f64>) -> (i32, i32) {
    let monitor = gdk::Display::default()
        .and_then(|display| display.monitors().item(0))
        .and_then(|monitor| monitor.downcast::<gdk::Monitor>().ok())
        .map(|monitor| monitor.geometry());
    let (monitor_width, monitor_height) = monitor
        .map(|geometry| (geometry.width() as f64, geometry.height() as f64))
        .unwrap_or((1920.0, 1080.0));

    let available_height = (monitor_height * 0.85 - TOOLBAR_H as f64).max(200.0);
    let auto = ((monitor_width * 0.90) / target.width as f64)
        .min(available_height / target.height as f64)
        .min(1.0);
    let factor = scale
        .unwrap_or(auto)
        .max(320.0 / target.width as f64)
        .max(200.0 / target.height as f64);
    (
        ((target.width as f64 * factor).round() as i32).max(1),
        ((target.height as f64 * factor).round() as i32).max(1),
    )
}

fn install_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_data(
        ".viewer-toolbar { background: #22242a; }\
         .viewer-canvas { background: #0e0f12; }",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

impl Viewer {
    fn sync_control(&mut self) {
        let state = self
            .current_target()
            .and_then(|target| target.session_id.as_ref())
            .filter(|id| !id.is_empty())
            .map(|id| control::effective(id))
            .unwrap_or_default();
        self.control = state.owner;
        self.control_owned = state.owner == Owner::Human && state.pid == std::process::id();
    }

    /// Claim or release human input control.  The server reads the same file
    /// and refuses agent input while the claim is live.
    fn set_human(&mut self, enabled: bool) -> Result<()> {
        let target = self
            .current_target()
            .cloned()
            .ok_or_else(|| anyhow!("no session is selected"))?;
        let session_id = target
            .session_id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "this session has no id; start it with display_start session_id=... to use viewer control"
                )
            })?;
        let pid = std::process::id();
        let state = if enabled {
            control::claim(&session_id, Owner::Human, pid)?
        } else {
            control::release(&session_id, pid)?
        };
        if let Err(err) = target.backend.set_host_input(enabled) {
            eprintln!("[xephyr-mcp-viewer] host input: {err:#}");
        }
        self.control = state.owner;
        self.control_owned = state.owner == Owner::Human && state.pid == pid;
        self.syncing = true;
        self.control_button.set_active(self.control_owned);
        self.syncing = false;
        self.update_title();
        Ok(())
    }

    fn release_control(&mut self) {
        if self.control_owned {
            let _ = self.set_human(false);
        }
    }

    /// Map a canvas point through the letterboxed frame into nested pixels.
    fn nested_point(&self, x: f64, y: f64) -> Option<(i32, i32)> {
        if self.layout != Layout::Single {
            return None;
        }
        let pane = self.panes.first()?;
        let inner = pane.last_inner?;
        if x < inner.x as f64
            || y < inner.y as f64
            || x >= (inner.x + inner.w) as f64
            || y >= (inner.y + inner.h) as f64
        {
            return None;
        }
        let width = pane.target.width.max(1) as f64;
        let height = pane.target.height.max(1) as f64;
        let nx = ((x - inner.x as f64) * width / inner.w.max(1) as f64).round() as i32;
        let ny = ((y - inner.y as f64) * height / inner.h.max(1) as f64).round() as i32;
        Some((
            nx.clamp(0, width as i32 - 1),
            ny.clamp(0, height as i32 - 1),
        ))
    }

    fn forward_motion(&mut self, x: f64, y: f64) {
        if !self.control_owned {
            return;
        }
        let Some((nx, ny)) = self.nested_point(x, y) else {
            return;
        };
        let Some(pane) = self.panes.first_mut() else {
            return;
        };
        match &pane.target.backend {
            Backend::X11 { .. } => {
                if let Some(view) = pane.x11.as_mut() {
                    view.motion(nx, ny);
                }
            }
            Backend::Wayland { .. } => {
                let backend = pane.target.backend.clone();
                let _ = backend.pointer_move(nx, ny);
            }
        }
    }

    fn forward_button(&mut self, button: u32, x: f64, y: f64, down: bool) {
        if !self.control_owned || button == 0 {
            return;
        }
        let at = self.nested_point(x, y);
        let Some(pane) = self.panes.first_mut() else {
            return;
        };
        match &pane.target.backend {
            Backend::X11 { .. } => {
                if let Some(view) = pane.x11.as_mut() {
                    if let Some((nx, ny)) = at {
                        view.motion(nx, ny);
                    }
                    view.button(button, down);
                }
            }
            Backend::Wayland { .. } => {
                let backend = pane.target.backend.clone();
                let _ = backend.pointer_button(at, button, down);
            }
        }
    }

    fn forward_scroll(&mut self, dy: f64) {
        if !self.control_owned || dy == 0.0 {
            return;
        }
        let up = dy < 0.0;
        let amount = dy.abs().round().clamp(1.0, 20.0) as u32;
        let at = self.last_pointer.and_then(|(x, y)| self.nested_point(x, y));
        let Some(pane) = self.panes.first_mut() else {
            return;
        };
        match &pane.target.backend {
            Backend::X11 { .. } => {
                if let Some(view) = pane.x11.as_mut() {
                    view.scroll(up, amount);
                }
            }
            Backend::Wayland { .. } => {
                let backend = pane.target.backend.clone();
                let _ = backend.pointer_scroll(at, up, amount);
            }
        }
    }

    fn forward_key(&mut self, keyval: u32, modifiers: gdk::ModifierType, pressed: bool) -> bool {
        if !self.control_owned || self.layout != Layout::Single {
            return false;
        }
        let mods = wl_input::Modifiers {
            shift: modifiers.contains(gdk::ModifierType::SHIFT_MASK),
            ctrl: modifiers.contains(gdk::ModifierType::CONTROL_MASK),
            alt: modifiers.contains(gdk::ModifierType::ALT_MASK),
            super_key: modifiers.contains(gdk::ModifierType::SUPER_MASK),
        };
        // Modifier keys are represented by `mods`, not sent as ordinary keys.
        let modifier_key = matches!(
            keyval,
            0xffe1..=0xffe6 | 0xffe7..=0xffec | 0xff7f
        );
        let Some(pane) = self.panes.first_mut() else {
            return false;
        };
        match &pane.target.backend {
            Backend::X11 { .. } => {
                let Some(view) = pane.x11.as_mut() else {
                    return false;
                };
                if modifier_key {
                    view.sync_mods(mods);
                } else {
                    view.key(keyval, mods, pressed);
                }
                true
            }
            Backend::Wayland { .. } => {
                if modifier_key {
                    return true;
                }
                let backend = pane.target.backend.clone();
                let _ = backend.key_sym(keyval, mods, pressed);
                true
            }
        }
    }

    fn tick(&mut self) {
        if !self.canvas.is_mapped() {
            return;
        }
        for pane in self.panes.iter_mut() {
            let captured = match &pane.target.backend {
                Backend::X11 { .. } => match pane.x11.as_ref() {
                    Some(view) => screenshot::capture_from(&view.conn, view.screen)
                        .map(|frame| (frame, view.pointer())),
                    None => continue,
                },
                Backend::Wayland { .. } => pane
                    .target
                    .backend
                    .capture(&CaptureTarget::Output, true)
                    .map(|capture| (capture.frame, None)),
            };
            match captured {
                Ok((frame, pointer)) => {
                    // Keep the native-resolution frame and let Cairo scale it
                    // into the pane when painting.  Software resizing here made
                    // each tick expensive enough to starve GTK's redraw source.
                    if frame.width != pane.target.width as u32
                        || frame.height != pane.target.height as u32
                    {
                        pane.target.width = frame.width.min(u16::MAX as u32) as u16;
                        pane.target.height = frame.height.min(u16::MAX as u32) as u16;
                    }
                    pane.texture =
                        image_surface(&frame.rgba, frame.width as i32, frame.height as i32);
                    pane.pointer = pointer;
                    pane.error = None;
                }
                Err(err) => pane.error = Some(short_error(&err)),
            }
        }
        self.canvas.queue_draw();
    }

    fn refresh_sessions(&mut self) {
        let mut sessions = discover_sessions();
        if let Some(pinned) = &self.pinned {
            if !sessions.iter().any(|target| target.key() == pinned.key()) {
                sessions.push(pinned.clone());
            }
        }
        sessions.sort_by_key(|target| std::cmp::Reverse(target.started_at));
        sessions.dedup_by(|a, b| a.key() == b.key());
        self.sessions = sessions;

        if !self
            .sessions
            .iter()
            .any(|target| target.key() == self.selected)
        {
            if let Some(first) = self.sessions.first() {
                self.selected = first.key();
            }
        }
        self.sync_control();
        self.sync_dropdown();
        self.sync_panes();
        self.sync_buttons();
        self.update_title();
        self.apply_aspect_hint();
        self.canvas.queue_draw();
    }

    fn sync_dropdown(&mut self) {
        let labels: Vec<String> = if self.sessions.is_empty() {
            vec!["no sessions".to_string()]
        } else {
            self.sessions
                .iter()
                .map(|target| target.menu_label())
                .collect()
        };
        let selected = self
            .sessions
            .iter()
            .position(|target| target.key() == self.selected)
            .map(|index| index as u32)
            .unwrap_or(gtk::INVALID_LIST_POSITION);

        self.syncing = true;
        if labels != self.labels {
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let model = gtk::StringList::new(&refs);
            self.dropdown.set_model(Some(&model));
            self.labels = labels;
        }
        self.dropdown.set_selected(selected);
        self.dropdown.set_sensitive(!self.sessions.is_empty());
        self.syncing = false;
    }

    fn sync_panes(&mut self) {
        let desired: Vec<Target> = match self.layout {
            Layout::Single => self
                .sessions
                .iter()
                .find(|target| target.key() == self.selected)
                .or(self.pinned.as_ref())
                .cloned()
                .into_iter()
                .collect(),
            Layout::Bsp => self.sessions.iter().take(MAX_PANES).cloned().collect(),
        };

        let mut panes = Vec::with_capacity(desired.len());
        for target in desired {
            if let Some(index) = self
                .panes
                .iter()
                .position(|pane| pane.target.key() == target.key())
            {
                let mut pane = self.panes.remove(index);
                pane.target = target;
                panes.push(pane);
            } else {
                let x11 = match &target.backend {
                    Backend::X11 { display } => {
                        let Ok((conn, screen)) = x11::connect(display) else {
                            continue;
                        };
                        Some(X11View::new(conn, screen))
                    }
                    Backend::Wayland { .. } => None,
                };
                panes.push(Pane {
                    target,
                    x11,
                    texture: None,
                    pointer: None,
                    error: None,
                    last_inner: None,
                });
            }
        }
        self.panes = panes;
    }

    fn sync_buttons(&mut self) {
        self.syncing = true;
        self.single_button.set_active(self.layout == Layout::Single);
        self.bsp_button.set_active(self.layout == Layout::Bsp);
        self.control_button.set_active(self.control_owned);
        let tile_count = self.sessions.len().min(MAX_PANES);
        self.bsp_button.set_label(&if tile_count == 0 {
            "BSP".to_string()
        } else {
            format!("BSP {tile_count}")
        });
        if self.sessions.len() > MAX_PANES && self.layout == Layout::Bsp {
            self.bsp_button.set_tooltip_text(Some(&format!(
                "Showing {} of {} live sessions",
                MAX_PANES,
                self.sessions.len()
            )));
        } else {
            self.bsp_button
                .set_tooltip_text(Some("Tile every live session (up to 8)"));
        }
        self.syncing = false;
    }

    fn set_layout(&mut self, layout: Layout) {
        if self.layout != layout {
            self.layout = layout;
            self.sync_panes();
            self.sync_buttons();
            self.update_title();
            self.apply_aspect_hint();
            self.canvas.queue_draw();
        } else {
            self.sync_buttons();
        }
    }

    fn cycle_session(&mut self) {
        if self.sessions.is_empty() {
            return;
        }
        let index = self
            .sessions
            .iter()
            .position(|target| target.key() == self.selected)
            .unwrap_or(0);
        let next = (index + 1) % self.sessions.len();
        if self.control_owned {
            let _ = self.set_human(false);
        }
        self.selected = self.sessions[next].key();
        self.layout = Layout::Single;
        self.sync_control();
        self.sync_dropdown();
        self.sync_panes();
        self.sync_buttons();
        self.update_title();
        self.apply_aspect_hint();
        self.canvas.queue_draw();
    }

    fn set_fullscreen(&mut self, enabled: bool) {
        self.fullscreen = enabled;
        if enabled {
            self.window.fullscreen();
        } else {
            self.window.unfullscreen();
        }
        self.apply_aspect_hint();
    }

    fn current_target(&self) -> Option<&Target> {
        self.sessions
            .iter()
            .find(|target| target.key() == self.selected)
            .or(self.sessions.first())
            .or(self.pinned.as_ref())
    }

    fn display_dims(&self) -> (u16, u16) {
        self.current_target()
            .map(|target| (target.width, target.height))
            .unwrap_or((1024, 640))
    }

    fn update_title(&mut self) {
        let label = self
            .current_target()
            .map(|target| target.label())
            .unwrap_or_else(|| "no sessions".to_string());
        let (width, height) = self.display_dims();
        let suffix = match self.layout {
            Layout::Single => String::new(),
            Layout::Bsp => format!(" [bsp {}]", self.panes.len()),
        };
        let control = if self.control == Owner::Human {
            " [HUMAN]"
        } else {
            ""
        };
        self.window.set_title(Some(&format!(
            "xephyr mcp viewer - {label} ({width}x{height}){suffix}{control}"
        )));
    }

    fn layout_rects(&self, width: i32, height: i32) -> Vec<Rect> {
        let area = Rect {
            x: 0,
            y: 0,
            w: width,
            h: height,
        };
        match self.layout {
            Layout::Single => vec![area],
            Layout::Bsp => bsp_rects(area, self.panes.len().max(1)),
        }
    }

    fn draw(&mut self, context: &cairo::Context, width: i32, height: i32) {
        set_source(context, COL_BG);
        context.rectangle(0.0, 0.0, width as f64, height as f64);
        let _ = context.fill();

        if self.panes.is_empty() {
            let text = "No sessions - start one with display_start";
            let (text_width, text_height) = text_size(context, text);
            draw_text(
                context,
                text,
                (width as f64 - text_width) / 2.0,
                (height as f64 - text_height) / 2.0,
                COL_DIM,
            );
            return;
        }

        let leaves = self.layout_rects(width, height);
        let bsp = self.layout == Layout::Bsp;
        let cursor = self.cursor;
        for (index, pane) in self.panes.iter_mut().enumerate() {
            pane.last_inner = None;
            let leaf = leaves.get(index).copied().unwrap_or(Rect {
                x: 0,
                y: 0,
                w: width,
                h: height,
            });
            let rect = if bsp { leaf.inset(2) } else { leaf };
            if rect.w <= 0 || rect.h <= 0 {
                continue;
            }
            let _ = context.save();
            context.rectangle(rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
            context.clip();
            set_source(context, COL_PANE);
            context.rectangle(rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
            let _ = context.fill();

            let mut inner_rect = None;
            if let Some(surface) = &pane.texture {
                let inner = fit_rect(rect, surface.width() as u32, surface.height() as u32);
                let _ = context.save();
                context.translate(inner.x as f64, inner.y as f64);
                context.scale(
                    inner.w as f64 / surface.width().max(1) as f64,
                    inner.h as f64 / surface.height().max(1) as f64,
                );
                let _ = context.set_source_surface(surface, 0.0, 0.0);
                context.source().set_filter(cairo::Filter::Bilinear);
                let _ = context.paint();
                let _ = context.restore();

                if cursor && !bsp {
                    if let Some((x, y)) = pane.pointer {
                        let px = inner.x as f64
                            + x as f64 * inner.w as f64 / pane.target.width.max(1) as f64;
                        let py = inner.y as f64
                            + y as f64 * inner.h as f64 / pane.target.height.max(1) as f64;
                        draw_crosshair(context, px, py);
                    }
                }
                inner_rect = Some(inner);
            } else if let Some(error) = &pane.error {
                draw_text(
                    context,
                    &format!("capture failed: {error}"),
                    rect.x as f64 + 8.0,
                    rect.y as f64 + 8.0,
                    COL_ERROR,
                );
            }
            pane.last_inner = inner_rect;
            let _ = context.restore();

            if bsp {
                draw_outline(context, leaf);
                let label = format!("{}  {}", pane.target.label(), pane.target.display);
                let (label_width, label_height) = text_size(context, &label);
                set_source(context, (0.04, 0.04, 0.05));
                context.rectangle(
                    leaf.x as f64 + 4.0,
                    leaf.y as f64 + 4.0,
                    label_width + 10.0,
                    label_height + 4.0,
                );
                let _ = context.fill();
                draw_text(
                    context,
                    &label,
                    leaf.x as f64 + 9.0,
                    leaf.y as f64 + 6.0,
                    COL_DIM,
                );
            }
        }

        if bsp && self.sessions.len() > MAX_PANES {
            let note = format!("showing {} of {} sessions", MAX_PANES, self.sessions.len());
            draw_text(context, &note, 8.0, height as f64 - 20.0, COL_TEXT);
        }
    }

    fn apply_aspect_hint(&self) {
        let enabled = self.aspect_lock && self.layout == Layout::Single && !self.fullscreen;
        let (display_width, display_height) = self.display_dims();
        let Some(surface) = self.window.surface() else {
            return;
        };
        let Ok(surface) = surface.downcast::<gdk4_x11::X11Surface>() else {
            // Not an X11 surface (Wayland): the content is still letterboxed by
            // the fit logic, but the compositor cannot be given an aspect hint.
            return;
        };
        let xid = surface.xid() as u32;
        let Ok((conn, _)) = x11rb::connect(None) else {
            return;
        };

        const P_MIN_SIZE: u32 = 1 << 4;
        const P_ASPECT: u32 = 1 << 7;
        let mut hints = [0u32; 18];
        if enabled && display_width > 0 && display_height > 0 {
            hints[0] = P_MIN_SIZE | P_ASPECT;
            hints[5] = 320;
            hints[6] = 240;
            hints[11] = display_width as u32;
            hints[12] = display_height.max(1) as u32;
            hints[13] = display_width as u32;
            hints[14] = display_height.max(1) as u32;
        }
        let _ = conn.change_property32(
            PropMode::REPLACE,
            xid,
            AtomEnum::WM_NORMAL_HINTS,
            AtomEnum::WM_SIZE_HINTS,
            &hints,
        );
        let _ = conn.flush();
    }
}

fn capture_interval(viewer: &Viewer) -> Duration {
    if viewer.layout == Layout::Bsp {
        Duration::from_millis(100)
    } else {
        Duration::from_millis((1000 / viewer.fps.clamp(1, 60) as u64).max(10))
    }
}

/// Capture on a self-rescheduling low-priority source.  A fixed 15 fps timer
/// with a slow capture makes the timeout perpetually ready, which can starve
/// GTK's redraw source entirely.  Rescheduling after each tick guarantees the
/// main loop gets idle time to paint, and LOW priority lets painting win.
fn schedule_capture(viewer: Rc<RefCell<Viewer>>) {
    let interval = viewer
        .try_borrow()
        .map(|viewer| capture_interval(&viewer))
        .unwrap_or_else(|_| Duration::from_millis(66));
    glib::timeout_add_local_full(interval, glib::Priority::LOW, move || {
        if let Ok(mut viewer) = viewer.try_borrow_mut() {
            viewer.tick();
        }
        schedule_capture(viewer.clone());
        glib::ControlFlow::Break
    });
}

fn bsp_rects(area: Rect, count: usize) -> Vec<Rect> {
    if area.w <= 0 || area.h <= 0 {
        return vec![area; count];
    }
    fn recurse(area: Rect, count: usize, out: &mut Vec<Rect>) {
        if count == 0 {
            return;
        }
        if count == 1 {
            out.push(area);
            return;
        }
        let first = count.div_ceil(2);
        let second = count - first;
        if area.w >= area.h {
            let cut = (area.w * first as i32 / count as i32).clamp(1, area.w - 1);
            recurse(Rect { w: cut, ..area }, first, out);
            recurse(
                Rect {
                    x: area.x + cut,
                    w: area.w - cut,
                    ..area
                },
                second,
                out,
            );
        } else {
            let cut = (area.h * first as i32 / count as i32).clamp(1, area.h - 1);
            recurse(Rect { h: cut, ..area }, first, out);
            recurse(
                Rect {
                    y: area.y + cut,
                    h: area.h - cut,
                    ..area
                },
                second,
                out,
            );
        }
    }
    let mut out = Vec::with_capacity(count);
    recurse(area, count, &mut out);
    out
}

fn fit_rect(rect: Rect, width: u32, height: u32) -> Rect {
    let width = width.max(1) as f64;
    let height = height.max(1) as f64;
    let scale = (rect.w as f64 / width).min(rect.h as f64 / height);
    let fit_width = ((width * scale).round() as i32).clamp(1, rect.w.max(1));
    let fit_height = ((height * scale).round() as i32).clamp(1, rect.h.max(1));
    Rect {
        x: rect.x + (rect.w - fit_width) / 2,
        y: rect.y + (rect.h - fit_height) / 2,
        w: fit_width,
        h: fit_height,
    }
}

fn set_source(context: &cairo::Context, (red, green, blue): (f32, f32, f32)) {
    context.set_source_rgb(red as f64, green as f64, blue as f64);
}

fn prepare_font(context: &cairo::Context) {
    context.select_font_face(
        "monospace",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Normal,
    );
    context.set_font_size(11.0);
}

fn text_size(context: &cairo::Context, text: &str) -> (f64, f64) {
    let _ = context.save();
    prepare_font(context);
    let size = context
        .text_extents(text)
        .map(|extents| (extents.width(), extents.height()))
        .unwrap_or((text.chars().count() as f64 * 7.0, 13.0));
    let _ = context.restore();
    size
}

fn draw_text(context: &cairo::Context, text: &str, x: f64, y: f64, color: (f32, f32, f32)) {
    let _ = context.save();
    set_source(context, color);
    prepare_font(context);
    let height = context
        .text_extents(text)
        .map(|extents| extents.height())
        .unwrap_or(13.0);
    context.move_to(x, y + height);
    let _ = context.show_text(text);
    let _ = context.restore();
}

fn draw_outline(context: &cairo::Context, rect: Rect) {
    let _ = context.save();
    set_source(context, COL_BORDER);
    context.set_line_width(1.0);
    context.rectangle(
        rect.x as f64 + 0.5,
        rect.y as f64 + 0.5,
        rect.w as f64 - 1.0,
        rect.h as f64 - 1.0,
    );
    let _ = context.stroke();
    let _ = context.restore();
}

fn draw_crosshair(context: &cairo::Context, x: f64, y: f64) {
    let _ = context.save();
    context.set_line_width(1.0);
    context.set_source_rgba(0.0, 0.0, 0.0, 0.75);
    context.move_to(x - 8.0, y);
    context.line_to(x + 8.0, y);
    context.move_to(x, y - 8.0);
    context.line_to(x, y + 8.0);
    let _ = context.stroke();

    context.set_source_rgba(1.0, 1.0, 1.0, 0.95);
    context.move_to(x - 6.0, y);
    context.line_to(x + 6.0, y);
    context.move_to(x, y - 6.0);
    context.line_to(x, y + 6.0);
    let _ = context.stroke();

    context.set_source_rgb(1.0, 0.25, 0.25);
    context.rectangle(x - 1.0, y - 1.0, 3.0, 3.0);
    let _ = context.fill();
    let _ = context.restore();
}

fn image_surface(rgba: &[u8], width: i32, height: i32) -> Option<cairo::ImageSurface> {
    let width = width.max(1);
    let height = height.max(1);
    let expected = width as usize * height as usize * 4;
    if rgba.len() < expected {
        return None;
    }
    let mut bgra = Vec::with_capacity(expected);
    for pixel in rgba.chunks_exact(4) {
        // Cairo ARGB32 is premultiplied native-endian; opaque input keeps alpha
        // at 255 and only needs the red/blue channels swapped on little-endian.
        bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    cairo::ImageSurface::create_for_data(bgra, cairo::Format::ARgb32, width, height, width * 4).ok()
}

fn short_error(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    text.lines()
        .next()
        .unwrap_or("capture failed")
        .chars()
        .take(120)
        .collect()
}
