//! Live viewer for one or more xephyr-mcp displays.
//!
//! The viewer is a small GTK4 application.  It captures each nested root window
//! with GetImage, uploads the frame as a texture, and paints one pane (single
//! mode) or a balanced BSP tiling of every live session.  GTK does the
//! compositing, double buffering, input handling and native session dropdown,
//! so the window does not flicker and the controls behave like normal widgets.
//!
//! The viewer is read-only: it never sends input to a nested display.

use anyhow::{anyhow, bail, Result};
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{cairo, gdk, glib};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::state::{self, State};
use crate::{screenshot, x11};

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
}

impl Target {
    fn label(&self) -> String {
        self.session_id
            .clone()
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| self.display.clone())
    }

    fn menu_label(&self) -> String {
        format!(
            "{}  -  {}  {}x{}",
            self.label(),
            self.display,
            self.width,
            self.height
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

The toolbar has a session dropdown and Single/BSP buttons.  Keys: q / Esc quit,
f / F11 fullscreen, t toggle BSP, s cycle sessions.  The viewer is read-only; it
never sends input to a nested display.

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
    let (conn, screen_num) = x11::connect(&state.display).ok()?;
    let (width, height) = x11::screen_size(&conn, screen_num);
    Some(Target {
        display: state.display.clone(),
        width,
        height,
        session_id: Some(state.session_id.clone()).filter(|id| !id.is_empty()),
        started_at: state.started_at,
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
        if !targets.iter().any(|item| item.display == target.display) {
            targets.push(target);
        }
    }
    targets.sort_by_key(|target| std::cmp::Reverse(target.started_at));
    targets.dedup_by(|a, b| a.display == b.display);
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

struct Pane {
    target: Target,
    conn: RustConnection,
    screen: usize,
    root: x11rb::protocol::xproto::Window,
    texture: Option<cairo::ImageSurface>,
    pointer: Option<(i16, i16)>,
    error: Option<String>,
}

struct Viewer {
    window: gtk::ApplicationWindow,
    canvas: gtk::DrawingArea,
    dropdown: gtk::DropDown,
    single_button: gtk::ToggleButton,
    bsp_button: gtk::ToggleButton,
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
        .map(|target| target.display.clone())
        .unwrap_or_default();
    let viewer = Rc::new(RefCell::new(Viewer {
        window: window.clone(),
        canvas: canvas.clone(),
        dropdown: dropdown.clone(),
        single_button: single_button.clone(),
        bsp_button: bsp_button.clone(),
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
    }));

    {
        let viewer = viewer.clone();
        canvas.set_draw_func(move |_area, context, width, height| {
            if let Ok(viewer) = viewer.try_borrow() {
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
            if target.display == viewer.selected {
                return;
            }
            viewer.selected = target.display;
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
        let viewer = viewer.clone();
        key.connect_key_pressed(move |_, keyval, _, modifiers| {
            if modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
                return glib::Propagation::Proceed;
            }
            let Ok(mut viewer) = viewer.try_borrow_mut() else {
                return glib::Propagation::Proceed;
            };
            let stop = match keyval {
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
        window.add_controller(key);
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
    fn tick(&mut self) {
        if !self.canvas.is_mapped() {
            return;
        }
        for pane in self.panes.iter_mut() {
            match screenshot::capture_from(&pane.conn, pane.screen) {
                Ok(frame) => {
                    // Keep the native-resolution frame and let Cairo scale it
                    // into the pane when painting.  Software resizing here made
                    // each tick expensive enough to starve GTK's redraw source.
                    pane.texture =
                        image_surface(&frame.rgba, frame.width as i32, frame.height as i32);
                    pane.pointer = pane
                        .conn
                        .query_pointer(pane.root)
                        .ok()
                        .and_then(|cookie| cookie.reply().ok())
                        .filter(|reply| reply.same_screen)
                        .map(|reply| (reply.root_x, reply.root_y));
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
            if !sessions
                .iter()
                .any(|target| target.display == pinned.display)
            {
                if let Ok((conn, screen_num)) = x11::connect(&pinned.display) {
                    let (width, height) = x11::screen_size(&conn, screen_num);
                    sessions.push(Target {
                        display: pinned.display.clone(),
                        width,
                        height,
                        session_id: pinned.session_id.clone(),
                        started_at: pinned.started_at,
                    });
                }
            }
        }
        sessions.sort_by_key(|target| std::cmp::Reverse(target.started_at));
        sessions.dedup_by(|a, b| a.display == b.display);
        self.sessions = sessions;

        if !self
            .sessions
            .iter()
            .any(|target| target.display == self.selected)
        {
            if let Some(first) = self.sessions.first() {
                self.selected = first.display.clone();
            }
        }
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
            .position(|target| target.display == self.selected)
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
                .find(|target| target.display == self.selected)
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
                .position(|pane| pane.target.display == target.display)
            {
                let mut pane = self.panes.remove(index);
                pane.target = target;
                panes.push(pane);
            } else if let Ok((conn, screen)) = x11::connect(&target.display) {
                let root = x11::root(&conn, screen);
                panes.push(Pane {
                    target,
                    conn,
                    screen,
                    root,
                    texture: None,
                    pointer: None,
                    error: None,
                });
            }
        }
        self.panes = panes;
    }

    fn sync_buttons(&mut self) {
        self.syncing = true;
        self.single_button.set_active(self.layout == Layout::Single);
        self.bsp_button.set_active(self.layout == Layout::Bsp);
        self.bsp_button
            .set_label(&format!("BSP {}", self.panes.len().max(1)));
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
            .position(|target| target.display == self.selected)
            .unwrap_or(0);
        let next = (index + 1) % self.sessions.len();
        self.selected = self.sessions[next].display.clone();
        self.layout = Layout::Single;
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
            .find(|target| target.display == self.selected)
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
        self.window.set_title(Some(&format!(
            "xephyr mcp viewer - {label} ({width}x{height}){suffix}"
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

    fn draw(&self, context: &cairo::Context, width: i32, height: i32) {
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
        for (index, pane) in self.panes.iter().enumerate() {
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

                if self.cursor && !bsp {
                    if let Some((x, y)) = pane.pointer {
                        let px = inner.x as f64
                            + x as f64 * inner.w as f64 / pane.target.width.max(1) as f64;
                        let py = inner.y as f64
                            + y as f64 * inner.h as f64 / pane.target.height.max(1) as f64;
                        draw_crosshair(context, px, py);
                    }
                }
            } else if let Some(error) = &pane.error {
                draw_text(
                    context,
                    &format!("capture failed: {error}"),
                    rect.x as f64 + 8.0,
                    rect.y as f64 + 8.0,
                    COL_ERROR,
                );
            }
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
