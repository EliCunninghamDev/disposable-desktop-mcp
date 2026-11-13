//! Tool definitions and handlers.

use crate::display::{DisplayManager, Session};
use crate::env::Tool;
use crate::mcp::CallResult;
use crate::{a11y, clipboard, input, proc, randr, screenshot, trace, wait, windows, x11};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

type ScreenshotOutput = (
    Vec<u8>,
    u32,
    u32,
    u32,
    u32,
    Option<(i32, i32)>,
    Option<screenshot::CursorMethod>,
);

pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "display_start",
            "Start a private, disposable nested X desktop for GUI testing, screenshots, mouse/keyboard automation, and window management. Call this before any other xephyr-mcp tool. visible=true launches Xephyr (a watchable window) on the host X11/XWayland display; visible=false launches headless Xvfb. Also starts a window manager and private D-Bus/AT-SPI buses so launched apps cannot reach the host session bus. Each server process owns one display: call display_stop before starting another, or run separate server instances with distinct session_id values for parallel desktops. Returns display, pids, dbus_address, runtime/log/home dirs, trace path, and accessibility status.",
            schema(
                json!({
                    "width": { "type": "integer", "minimum": 64, "maximum": 8192, "description": "Screen width in pixels (default 1280)" },
                    "height": { "type": "integer", "minimum": 64, "maximum": 8192, "description": "Screen height in pixels (default 800)" },
                    "visible": { "type": "boolean", "description": "Xephyr window (true, default) or headless Xvfb (false)" },
                    "session_id": { "type": "string", "description": "Optional caller-supplied identifier (1-64 letters/digits/-_.). Used by the viewer and to keep multiple server instances apart. Generated if omitted." },
                    "persist_home": { "type": "boolean", "description": "Keep HOME and XDG directories under the state dir keyed by session_id, so app profiles survive display_stop/display_start (default false)" }
                }),
                &[],
            ),
        ),
        tool(
            "display_stop",
            "Stop the nested display and kill every process group it started: applications, window manager, X server, accessibility registry, and private D-Bus session. Safe to call when nothing is running.",
            schema(json!({}), &[]),
        ),
        tool(
            "launch",
            "Launch an application or shell command inside the nested display. The binary comes from the host filesystem, but DISPLAY, XAUTHORITY, XDG_RUNTIME_DIR, and the session/accessibility buses point at the private desktop; WAYLAND_DISPLAY is removed. Uses 'sh -c', so pipelines and quoting work. Returns the PID and log file path. Use wait_ms to detect immediate crashes and read the log for startup errors.",
            schema(
                json!({
                    "command": { "type": "string", "description": "Shell command to run, e.g. 'zenity --info --text hello' or 'xterm -e top'" },
                    "wait_ms": { "type": "integer", "minimum": 0, "maximum": 10000, "description": "How long to watch for an immediate exit (default 250)" }
                }),
                &["command"],
            ),
        ),
        tool(
            "launch_status",
            "Report the current state of processes started with launch in this session: running, exit_code, terminating signal, uptime, and log path. With pid, report just that launch; with include_exited=false, omit finished ones. Exit status is captured by a background reaper, so it remains available after launch returns.",
            schema(
                json!({
                    "pid": { "type": "integer", "minimum": 1, "description": "Only report this pid (must have been started by launch)" },
                    "include_exited": { "type": "boolean", "description": "Include launches that have finished (default true)" }
                }),
                &[],
            ),
        ),
        tool(
            "screenshot",
            "Capture the nested desktop as a PNG. Without options this captures the full root window downscaled to max_width/max_height; set region or window_id to crop at native resolution, and cursor/grid/marks to overlay the pointer, a 100 px grid, or numbered accessibility targets. Marks also records the mapping so click(mark=N) can target an element. The text block reports display/image dimensions, crop origin, scale, and the mark list.",
            schema(
                json!({
                    "max_width": { "type": "integer", "minimum": 64, "maximum": 4096, "description": "Maximum returned image width (default 1280; ignored with native=true)" },
                    "max_height": { "type": "integer", "minimum": 64, "maximum": 4096, "description": "Maximum returned image height (default 800; ignored with native=true)" },
                    "region": {
                        "type": "object",
                        "description": "Crop to this display-pixel rectangle before scaling",
                        "properties": {
                            "x": { "type": "integer", "minimum": 0 },
                            "y": { "type": "integer", "minimum": 0 },
                            "width": { "type": "integer", "minimum": 1 },
                            "height": { "type": "integer", "minimum": 1 }
                        },
                        "required": ["x", "y", "width", "height"],
                        "additionalProperties": false
                    },
                    "window_id": { "description": "Capture this window instead of the root (id from list_windows)", "oneOf": [{ "type": "integer" }, { "type": "string" }] },
                    "cursor": { "type": "boolean", "description": "Overlay the nested pointer using XFixes (default false)" },
                    "grid": { "type": "boolean", "description": "Overlay a 100 px coordinate grid (default false)" },
                    "marks": { "type": "boolean", "description": "Draw numbered boxes around actionable accessibility elements and record their centers for click(mark=N) (default false)" },
                    "mark_limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Maximum numbered elements when marks=true (default 60)" },
                    "native": { "type": "boolean", "description": "Skip downscaling and return full-resolution pixels (default false)" }
                }),
                &[],
            ),
        ),
        tool(
            "wait_for",
            "Block until a UI or process condition is met instead of polling screenshots. Conditions: window (title/class appears or disappears), process_exit (pid), screen_stable (frame hash unchanged for stable_ms), region_change (pixels in region changed), file_exists (path appears/disappears). Returns satisfied/timed_out, elapsed time, polls, and condition details.",
            schema(
                json!({
                    "condition": { "type": "string", "enum": ["window", "process_exit", "screen_stable", "region_change", "file_exists"], "description": "What to wait for" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 120000, "description": "Give up after this long (default 10000)" },
                    "poll_ms": { "type": "integer", "minimum": 20, "maximum": 5000, "description": "Polling interval (default 100)" },
                    "title": { "type": "string", "description": "window: case-insensitive title substring" },
                    "class": { "type": "string", "description": "window: case-insensitive WM_CLASS/instance substring" },
                    "appear": { "type": "boolean", "description": "window: true waits for a match, false waits for it to disappear (default true)" },
                    "pid": { "type": "integer", "description": "process_exit: process id to watch" },
                    "stable_ms": { "type": "integer", "minimum": 0, "maximum": 60000, "description": "screen_stable: required unchanged duration (default 500)" },
                    "region": {
                        "type": "object",
                        "description": "screen_stable/region_change: restrict captures to this rectangle",
                        "properties": {
                            "x": { "type": "integer", "minimum": 0 },
                            "y": { "type": "integer", "minimum": 0 },
                            "width": { "type": "integer", "minimum": 1 },
                            "height": { "type": "integer", "minimum": 1 }
                        },
                        "required": ["x", "y", "width", "height"],
                        "additionalProperties": false
                    },
                    "path": { "type": "string", "description": "file_exists: path to watch" },
                    "present": { "type": "boolean", "description": "file_exists: true waits for existence, false for removal (default true)" }
                }),
                &["condition"],
            ),
        ),
        tool(
            "wait_for_idle",
            "Wait until the screen stops changing, then return. This is the default settle step after launching an app, opening a menu, or submitting a form; prefer it over repeated screenshots. Scoping region to the changed area avoids waiting on clocks or animated backgrounds.",
            schema(
                json!({
                    "stable_ms": { "type": "integer", "minimum": 0, "maximum": 60000, "description": "Required unchanged duration (default 500)" },
                    "timeout_ms": { "type": "integer", "minimum": 1, "maximum": 120000, "description": "Give up after this long (default 10000)" },
                    "poll_ms": { "type": "integer", "minimum": 20, "maximum": 5000, "description": "Polling interval (default 100)" },
                    "region": {
                        "type": "object",
                        "description": "Only watch this rectangle instead of the whole screen",
                        "properties": {
                            "x": { "type": "integer", "minimum": 0 },
                            "y": { "type": "integer", "minimum": 0 },
                            "width": { "type": "integer", "minimum": 1 },
                            "height": { "type": "integer", "minimum": 1 }
                        },
                        "required": ["x", "y", "width", "height"],
                        "additionalProperties": false
                    }
                }),
                &[],
            ),
        ),
        tool(
            "click",
            "Move the nested pointer and click. Pass x/y in display pixels, or mark from the last screenshot with marks=true. count>1 gives double/triple clicks; modifiers holds ctrl/shift/alt/super for the click. Uses XTEST on the nested server; the host cursor is untouched. Call focus_window first when the target window is not focused.",
            schema(
                json!({
                    "x": { "type": "integer", "description": "Display X coordinate (omit when using mark)" },
                    "y": { "type": "integer", "description": "Display Y coordinate (omit when using mark)" },
                    "mark": { "type": "integer", "minimum": 1, "maximum": 999, "description": "Target the center of this numbered element from the last marks screenshot" },
                    "button": { "description": "left (default), middle, right, or a button number", "oneOf": [{ "type": "string" }, { "type": "integer" }] },
                    "count": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Click count (default 1)" },
                    "modifiers": { "type": "array", "items": { "type": "string", "enum": ["ctrl", "control", "shift", "alt", "super", "meta"] }, "description": "Modifiers held during the click" }
                }),
                &[],
            ),
        ),
        tool(
            "mouse_down",
            "Press and hold a mouse button, optionally moving to x/y first. Pair with mouse_up for drag-selection, canvas drawing, or games where a single drag call is too coarse.",
            schema(
                json!({
                    "x": { "type": "integer", "description": "Optional display X to move to first" },
                    "y": { "type": "integer", "description": "Optional display Y to move to first" },
                    "button": { "description": "left (default), middle, right, or a button number", "oneOf": [{ "type": "string" }, { "type": "integer" }] }
                }),
                &[],
            ),
        ),
        tool(
            "mouse_up",
            "Release a mouse button, optionally after moving to x/y. Completes a mouse_down sequence.",
            schema(
                json!({
                    "x": { "type": "integer", "description": "Optional display X to move to first" },
                    "y": { "type": "integer", "description": "Optional display Y to move to first" },
                    "button": { "description": "left (default), middle, right, or a button number", "oneOf": [{ "type": "string" }, { "type": "integer" }] }
                }),
                &[],
            ),
        ),
        tool(
            "key_down",
            "Press and hold a key or modifier (e.g. 'ctrl', 'shift', 'a'). Pair with key_up for modifier-held clicks or multi-step shortcuts. Use key for complete combos.",
            schema(
                json!({
                    "key": { "type": "string", "description": "Key or modifier to hold" }
                }),
                &["key"],
            ),
        ),
        tool(
            "key_up",
            "Release a key or modifier previously pressed with key_down.",
            schema(
                json!({
                    "key": { "type": "string", "description": "Key or modifier to release" }
                }),
                &["key"],
            ),
        ),
        tool(
            "move",
            "Move the nested pointer to (x, y) in display pixels without clicking.",
            schema(
                json!({
                    "x": { "type": "integer", "description": "Display X coordinate" },
                    "y": { "type": "integer", "description": "Display Y coordinate" }
                }),
                &["x", "y"],
            ),
        ),
        tool(
            "drag",
            "Press a mouse button at (x1, y1), move through intermediate points to (x2, y2), and release. Coordinates are display pixels.",
            schema(
                json!({
                    "x1": { "type": "integer", "description": "Start X" },
                    "y1": { "type": "integer", "description": "Start Y" },
                    "x2": { "type": "integer", "description": "End X" },
                    "y2": { "type": "integer", "description": "End Y" },
                    "button": { "description": "left (default), middle, right, or a button number", "oneOf": [{ "type": "string" }, { "type": "integer" }] },
                    "duration_ms": { "type": "integer", "minimum": 0, "maximum": 10000, "description": "Drag duration (default 350)" }
                }),
                &["x1", "y1", "x2", "y2"],
            ),
        ),
        tool(
            "scroll",
            "Scroll the wheel at the current pointer position or at (x, y). Positive amount scrolls down by default; set direction to 'up' for up.",
            schema(
                json!({
                    "amount": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Wheel clicks (default 3)" },
                    "direction": { "type": "string", "enum": ["up", "down"], "description": "Scroll direction (default down)" },
                    "x": { "type": "integer", "description": "Optional X to move to first" },
                    "y": { "type": "integer", "description": "Optional Y to move to first" }
                }),
                &[],
            ),
        ),
        tool(
            "type",
            "Type text into the focused window using XTEST. Supports Unicode and newlines; this is not a key-combo API (use key for shortcuts).",
            schema(
                json!({
                    "text": { "type": "string", "description": "Text to type" },
                    "delay_ms": { "type": "integer", "minimum": 0, "maximum": 1000, "description": "Per-character delay (default 12)" }
                }),
                &["text"],
            ),
        ),
        tool(
            "key",
            "Send a key combination to the focused window, e.g. 'Return', 'ctrl+l', 'alt+Tab', 'ctrl+shift+t'. Uses XTEST and clears held modifiers first.",
            schema(
                json!({
                    "combo": { "type": "string", "description": "xdotool key combo" },
                    "delay_ms": { "type": "integer", "minimum": 0, "maximum": 1000, "description": "Delay between keys (default 12)" }
                }),
                &["combo"],
            ),
        ),
        tool(
            "clipboard_get",
            "Read the nested desktop's CLIPBOARD selection as text. Use this after ctrl+c or a Copy action; it is faster and more reliable than selecting text with the pointer.",
            schema(json!({}), &[]),
        ),
        tool(
            "clipboard_set",
            "Put text on the nested desktop's CLIPBOARD selection. Much more reliable than type for long or Unicode text, and it avoids per-character XTEST latency. Paste with key ctrl+v.",
            schema(
                json!({
                    "text": { "type": "string", "description": "Text to place on the clipboard" }
                }),
                &["text"],
            ),
        ),
        tool(
            "list_windows",
            "List managed top-level windows, topmost first, with ids, titles, class, pid, absolute display geometry, frame extents, mapped/focused flags, and stack order.",
            schema(json!({}), &[]),
        ),
        tool(
            "focus_window",
            "Activate a window by id using EWMH _NET_ACTIVE_WINDOW, with an X11 input-focus fallback when no window manager is present. Get ids from list_windows.",
            schema(
                json!({
                    "id": { "description": "Window id (decimal or 0x hex)", "oneOf": [{ "type": "integer" }, { "type": "string" }] }
                }),
                &["id"],
            ),
        ),
        tool(
            "move_resize_window",
            "Move and resize a window. Uses EWMH _NET_MOVERESIZE_WINDOW first and falls back to ConfigureWindow if the manager ignores it. Coordinates are the client's top-left in display pixels.",
            schema(
                json!({
                    "id": { "description": "Window id (decimal or 0x hex)", "oneOf": [{ "type": "integer" }, { "type": "string" }] },
                    "x": { "type": "integer" },
                    "y": { "type": "integer" },
                    "width": { "type": "integer", "minimum": 1, "maximum": 32767 },
                    "height": { "type": "integer", "minimum": 1, "maximum": 32767 }
                }),
                &["id", "x", "y", "width", "height"],
            ),
        ),
        tool(
            "close_window",
            "Ask a window to close via WM_DELETE_WINDOW. Returns closed:false if the application declined; set force=true to XKillClient it.",
            schema(
                json!({
                    "id": { "description": "Window id (decimal or 0x hex)", "oneOf": [{ "type": "integer" }, { "type": "string" }] },
                    "force": { "type": "boolean", "description": "Kill the client if it refuses to close (default false)" }
                }),
                &["id"],
            ),
        ),
        tool(
            "window_state",
            "Change a window's EWMH state: minimize, maximize, unmaximize, restore, fullscreen, unfullscreen, raise, lower, set_always_on_top, or clear_always_on_top. Pass action=state to read the current state without changing anything. Returns the applied action, final state names, geometry, and mapped/focused flags.",
            schema(
                json!({
                    "id": { "description": "Window id (decimal or 0x hex)", "oneOf": [{ "type": "integer" }, { "type": "string" }] },
                    "action": { "type": "string", "enum": ["minimize", "maximize", "unmaximize", "restore", "fullscreen", "unfullscreen", "raise", "lower", "set_always_on_top", "clear_always_on_top", "state"], "description": "State change to apply, or state to query" }
                }),
                &["id", "action"],
            ),
        ),
        tool(
            "resize_display",
            "Resize the nested desktop at runtime with RandR. Xephyr offers common modes (1600x1200, 1400x1050, 1280x1024, 1024x768, 800x600, 640x480, ...); Xvfb only offers its startup size. Changes the root geometry immediately and updates the viewer/state file.",
            schema(
                json!({
                    "width": { "type": "integer", "minimum": 64, "maximum": 8192, "description": "New display width" },
                    "height": { "type": "integer", "minimum": 64, "maximum": 8192, "description": "New display height" },
                    "refresh": { "type": "number", "description": "Preferred refresh rate when several modes match (optional)" }
                }),
                &["width", "height"],
            ),
        ),
        tool(
            "trace_tail",
            "Read the append-only JSONL tool trace for a session (or the newest trace when session_id is omitted). Each line records tool, sanitized arguments, duration, ok/error, and before/after frame hashes for screen-changing tools. Traces survive display_stop.",
            schema(
                json!({
                    "session_id": { "type": "string", "description": "Session trace to read (default: newest trace file)" },
                    "lines": { "type": "integer", "minimum": 1, "maximum": 2000, "description": "Trailing lines (default 100)" }
                }),
                &[],
            ),
        ),
        tool(
            "list_processes",
            "List processes belonging to the nested desktop (matched by their private XDG_RUNTIME_DIR), with pid, ppid, pgid, command line, and whether the process was started by launch. Use this to find a PID for kill_process or wait_for process_exit.",
            schema(json!({}), &[]),
        ),
        tool(
            "kill_process",
            "Signal a process owned by the nested desktop. Only PIDs whose environment references this session's private runtime directory are accepted, so host processes cannot be targeted. signal defaults to TERM; set group=true for a launched command's whole process group.",
            schema(
                json!({
                    "pid": { "type": "integer", "minimum": 2, "description": "PID from list_processes or launch" },
                    "signal": { "type": "string", "enum": ["TERM", "KILL", "INT", "HUP", "QUIT"], "description": "Signal name (default TERM)" },
                    "group": { "type": "boolean", "description": "Signal the process group when pid is its leader (default false)" }
                }),
                &["pid"],
            ),
        ),
        tool(
            "tail_log",
            "Read the tail of a launch or session log inside the nested desktop's log directory. Pass a path returned by launch, or a file name relative to the log directory. Bounded to the last lines/bytes so large logs stay cheap.",
            schema(
                json!({
                    "file": { "type": "string", "description": "Log path from launch, or a file name in the session log directory" },
                    "lines": { "type": "integer", "minimum": 1, "maximum": 2000, "description": "Number of trailing lines (default 100)" }
                }),
                &["file"],
            ),
        ),
        tool(
            "a11y_find",
            "Query the AT-SPI tree by name, role, state, and/or action and return only matching nodes (with actions, states, and geometry). Cheaper than a full a11y_tree dump on large applications. Use path to search inside a subtree, and the returned index with a11y_click, a11y_focus, or a11y_set_text.",
            schema(
                json!({
                    "name": { "type": "string", "description": "Accessible name substring (or exact match with exact=true)" },
                    "role": { "type": "string", "description": "Role filter, e.g. 'push button', 'entry', 'menu item'" },
                    "state": { "type": "string", "description": "Comma-separated required states: enabled, focused, visible, showing, sensitive, checked, selected" },
                    "action": { "type": "string", "description": "Action-name substring, e.g. 'click', 'activate', 'toggle'" },
                    "path": { "type": "string", "description": "Only search this node's subtree (path from a11y_tree/a11y_find)" },
                    "exact": { "type": "boolean", "description": "Require an exact name match (default false)" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Maximum nodes to return (default 20)" },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 64, "description": "Tree depth (default 12)" },
                    "max_nodes": { "type": "integer", "minimum": 1, "maximum": 4000, "description": "Node budget (default 400)" }
                }),
                &[],
            ),
        ),
        tool(
            "a11y_set_text",
            "Set the text of an editable accessible field by name or tree path using the AT-SPI EditableText interface. Path targeting is reliable for GTK4 widgets. Avoids focus and per-character typing, and works for long or Unicode input. Returns changed/editable and the matched node.",
            schema(
                json!({
                    "name": { "type": "string", "description": "Accessible name or label of the field" },
                    "path": { "type": "string", "description": "Tree path from a11y_tree/a11y_find; bypasses name matching and geometry" },
                    "text": { "type": "string", "description": "New field contents" },
                    "role": { "type": "string", "description": "Optional role filter, e.g. 'entry'" },
                    "index": { "type": "integer", "minimum": 0, "maximum": 200, "description": "Pick the Nth best match (default 0)" }
                }),
                &["text"],
            ),
        ),
        tool(
            "a11y_focus",
            "Give keyboard focus to an accessible element by name, role, or tree path. Path targeting is reliable for GTK4 widgets. Use before key/type when clicking should not change selection or activate the control.",
            schema(
                json!({
                    "name": { "type": "string", "description": "Accessible name substring" },
                    "path": { "type": "string", "description": "Tree path from a11y_tree/a11y_find; bypasses name matching and geometry" },
                    "role": { "type": "string", "description": "Role filter, e.g. 'entry', 'text'" },
                    "index": { "type": "integer", "minimum": 0, "maximum": 200, "description": "Pick the Nth best match (default 0)" }
                }),
                &[],
            ),
        ),
        tool(
            "a11y_tree",
            "Read the AT-SPI accessibility tree of the nested desktop: roles, names, actions, states, and screen rectangles. Much cheaper and more precise than screenshots for forms, buttons and menus. Pass path to dump only one subtree instead of the whole desktop. Requires GTK/Qt accessibility to be enabled (it is for launched apps) and PyGObject.",
            schema(
                json!({
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 64, "description": "Tree depth (default 12)" },
                    "max_nodes": { "type": "integer", "minimum": 1, "maximum": 4000, "description": "Node budget (default 400)" },
                    "app": { "type": "string", "description": "Only include applications whose name contains this string" },
                    "path": { "type": "string", "description": "Dump this node's subtree (path from a11y_tree/a11y_find)" }
                }),
                &[],
            ),
        ),
        tool(
            "diagnose",
            "Report which dependencies xephyr-mcp found and their versions: Xephyr, Xvfb, window manager, xdotool, dbus, AT-SPI, PyGObject, runtime tree, host display, and active sessions. Use this first when a tool fails for environmental reasons.",
            schema(json!({}), &[]),
        ),
        tool(
            "a11y_click",
            "Activate an accessible element by name or by tree path. Invokes its click/press/activate/toggle action; if it has none, clicks its center with XTEST on non-GTK4 toolkits. Path targeting is the reliable option for GTK4 widgets, whose child positions are unreliable. Returns the matched node and how it was activated.",
            schema(
                json!({
                    "name": { "type": "string", "description": "Accessible name (button label, menu item, field label, ...)" },
                    "path": { "type": "string", "description": "Tree path from a11y_tree/a11y_find; bypasses name matching and geometry" },
                    "role": { "type": "string", "description": "Optional role filter, e.g. 'push button', 'menu item', 'check box'" },
                    "index": { "type": "integer", "minimum": 0, "maximum": 200, "description": "Pick the Nth best match (default 0)" }
                }),
                &[],
            ),
        ),
    ]
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": schema,
    })
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

pub async fn call(manager: &DisplayManager, name: &str, args: &Value) -> CallResult {
    manager.begin_call();
    let started = std::time::Instant::now();
    let before_session = manager.session().await;
    let hashed = trace::should_hash(name);
    let with_png = hashed && trace::screenshots_enabled();
    let before = if hashed {
        capture_state(before_session.as_ref(), with_png).await
    } else {
        None
    };

    let outcome = dispatch(manager, name, args).await;
    let after_session = manager.session().await;
    let after = if hashed {
        capture_state(after_session.as_ref(), with_png).await
    } else {
        None
    };
    let result = match outcome {
        Ok(result) => result,
        Err(err) => CallResult::error(format!("{err:#}")),
    };

    // Prefer the post-call session so display_start is recorded under the new
    // session id; fall back to the pre-call one for display_stop.
    let session = after_session.as_ref().or(before_session.as_ref());
    let session_id = session.map(|session| session.info.session_id.as_str());
    let display = session.map(|session| session.info.display.as_str());
    let error = result
        .is_error
        .then(|| result.summary().unwrap_or("tool error"));
    let mut entry = trace::entry(
        name,
        args,
        session_id,
        display,
        started.elapsed().as_millis(),
        !result.is_error,
        error,
        before.as_ref().map(|capture| capture.hash.as_str()),
        after.as_ref().map(|capture| capture.hash.as_str()),
    );
    if with_png {
        let dir = trace::steps_dir(session_id);
        attach_step(&mut entry, &dir, name, "before", before.as_ref());
        attach_step(&mut entry, &dir, name, "after", after.as_ref());
    }
    let _ = trace::append(&trace::path_for(session_id), entry);

    manager.end_call();
    result
}

struct Capture {
    hash: String,
    png: Option<Vec<u8>>,
}

async fn capture_state(session: Option<&Arc<Session>>, with_png: bool) -> Option<Capture> {
    let display = session?.info.display.clone();
    tokio::task::spawn_blocking(move || {
        screenshot::capture(&display).ok().map(|frame| {
            let hash = format!("{:016x}", screenshot::hash(&frame));
            let png = with_png
                .then(|| screenshot::encode_png(frame.width, frame.height, &frame.rgba).ok())
                .flatten();
            Capture { hash, png }
        })
    })
    .await
    .ok()
    .flatten()
}

fn attach_step(
    entry: &mut Value,
    dir: &std::path::Path,
    tool: &str,
    phase: &str,
    capture: Option<&Capture>,
) {
    let Some(png) = capture.and_then(|capture| capture.png.as_ref()) else {
        return;
    };
    if let Ok(path) = trace::write_step(dir, tool, phase, png) {
        entry[format!("{phase}_shot")] = json!(path.display().to_string());
    }
}

async fn dispatch(manager: &DisplayManager, name: &str, args: &Value) -> Result<CallResult> {
    match name {
        "display_start" => {
            let width = opt_u64(args, "width").unwrap_or(1280) as u32;
            let height = opt_u64(args, "height").unwrap_or(800) as u32;
            let visible = opt_bool(args, "visible").unwrap_or(true);
            let session_id = opt_str(args, "session_id").map(str::to_string);
            let persist_home = opt_bool(args, "persist_home").unwrap_or(false);
            let report = manager
                .start(width, height, visible, session_id, persist_home)
                .await?;
            Ok(CallResult::text(&report.to_json()))
        }
        "display_stop" => {
            let report = manager.stop().await?;
            Ok(CallResult::text(&report.to_json()))
        }
        "launch" => {
            let session = active(manager).await?;
            let command = required_str(args, "command")?;
            let wait_ms = opt_u64(args, "wait_ms").unwrap_or(250);
            let result = launch_process(&session, command, wait_ms).await?;
            Ok(CallResult::text(&result))
        }
        "launch_status" => {
            let session = active(manager).await?;
            let wanted = opt_u64(args, "pid").map(|value| value as u32);
            let include_exited = opt_bool(args, "include_exited").unwrap_or(true);
            let launches = session.launched();
            if let Some(pid) = wanted {
                if !launches.iter().any(|item| item.pid == pid) {
                    return Err(anyhow!(
                        "pid {pid} was not started by launch in this session; use list_processes for other processes"
                    ));
                }
            }
            let mut items: Vec<Value> = Vec::new();
            for item in launches {
                if wanted.map(|pid| pid != item.pid).unwrap_or(false) {
                    continue;
                }
                let exit = session.exit_info(item.pid);
                let running = exit.is_none() && session.running(item.pid);
                if !include_exited && !running {
                    continue;
                }
                items.push(json!({
                    "pid": item.pid,
                    "command": item.command,
                    "log": item.log.display().to_string(),
                    "running": running,
                    "exit_code": exit.as_ref().and_then(|info| info.code),
                    "signal": exit.as_ref().and_then(|info| info.signal),
                    "exit_after_ms": exit.as_ref().map(|info| info.after_ms),
                    "uptime_ms": item.started_at.elapsed().as_millis() as u64,
                }));
            }
            Ok(CallResult::text(&json!({
                "display": session.info.display,
                "count": items.len(),
                "launches": items,
            })))
        }
        "screenshot" => {
            let session = active(manager).await?;
            let region = args.get("region").map(wait::parse_region).transpose()?;
            let window_id = match args.get("window_id") {
                Some(Value::Null) | None => None,
                Some(value) => Some(x11::parse_window_id(value)?),
            };
            let cursor = opt_bool(args, "cursor").unwrap_or(false);
            let grid = opt_bool(args, "grid").unwrap_or(false);
            let marks_requested = opt_bool(args, "marks").unwrap_or(false);
            let mark_limit = opt_u64(args, "mark_limit").unwrap_or(60).clamp(1, 200) as u32;
            // Crops default to native resolution: downscaling a small region
            // is exactly what destroys text legibility.
            let native =
                opt_bool(args, "native").unwrap_or(region.is_some() || window_id.is_some());
            let max_width = opt_u64(args, "max_width").unwrap_or(1280).clamp(64, 4096) as u32;
            let max_height = opt_u64(args, "max_height").unwrap_or(800).clamp(64, 4096) as u32;
            let display = session.info.display.clone();
            let display_width = session.width();
            let display_height = session.height();
            let kind = if window_id.is_some() {
                "window"
            } else if region.is_some() {
                "region"
            } else {
                "root"
            };

            let (marks_value, mark_list) = if marks_requested {
                let (value, list) = a11y::marks(
                    &session,
                    manager.tools.python.as_ref(),
                    mark_limit,
                    14,
                    1600,
                )
                .await?;
                (Some(value), list)
            } else {
                (None, Vec::new())
            };
            let mark_boxes: Vec<(u32, i32, i32, u32, u32)> = mark_list
                .iter()
                .map(|mark| (mark.mark, mark.x, mark.y, mark.width, mark.height))
                .collect();

            let (png, source_width, source_height, width, height, origin, cursor_method) =
                tokio::task::spawn_blocking(move || -> Result<ScreenshotOutput> {
                    let mut frame = if let Some(window) = window_id {
                        screenshot::capture_window(&display, window)?
                    } else if let Some(region) = region {
                        screenshot::capture_region(
                            &display,
                            region.x,
                            region.y,
                            region.width,
                            region.height,
                        )?
                    } else {
                        screenshot::capture(&display)?
                    };
                    let origin = if let Some(window) = window_id {
                        Some(screenshot::window_origin(&display, window)?)
                    } else {
                        region.map(|region| (region.x as i32, region.y as i32))
                    };
                    let mut cursor_method = None;
                    if cursor {
                        let (ox, oy) = origin.unwrap_or((0, 0));
                        cursor_method = screenshot::overlay_cursor(&display, &mut frame, ox, oy)
                            .ok()
                            .flatten();
                    }
                    if grid {
                        let spacing = if frame.width < 500 { 50 } else { 100 };
                        screenshot::overlay_grid(&mut frame, spacing);
                    }
                    if !mark_boxes.is_empty() {
                        let (ox, oy) = origin.unwrap_or((0, 0));
                        screenshot::draw_marks(&mut frame, &mark_boxes, ox, oy);
                    }
                    let source_width = frame.width;
                    let source_height = frame.height;
                    let (width, height) = if native {
                        (frame.width, frame.height)
                    } else {
                        screenshot::fit(frame.width, frame.height, max_width, max_height)
                    };
                    let scaled = screenshot::resize(&frame, width, height);
                    let png = screenshot::encode_png(width, height, &scaled)?;
                    Ok((
                        png,
                        source_width,
                        source_height,
                        width,
                        height,
                        origin,
                        cursor_method,
                    ))
                })
                .await
                .context("screenshot task failed")??;

            if marks_requested {
                session.set_marks(mark_list);
            }
            let origin_x = origin.map(|(x, _)| x).unwrap_or(0);
            let origin_y = origin.map(|(_, y)| y).unwrap_or(0);
            let metadata = json!({
                "display": session.info.display,
                "capture": kind,
                "window_id": window_id,
                "region": origin.map(|(x, y)| json!({
                    "x": x,
                    "y": y,
                    "width": source_width,
                    "height": source_height,
                })),
                "display_width": display_width,
                "display_height": display_height,
                "source_width": source_width,
                "source_height": source_height,
                "origin_x": origin_x,
                "origin_y": origin_y,
                "image_width": width,
                "image_height": height,
                "scale": if source_width == 0 { 1.0 } else { width as f64 / source_width as f64 },
                "cursor_overlay": cursor,
                "cursor_drawn": cursor_method.is_some(),
                "cursor_method": cursor_method.map(|method| format!("{method:?}").to_lowercase()),
                "grid_overlay": grid,
                "marks_requested": marks_requested,
                "mark_count": mark_list_count(&marks_value),
                "marks": marks_value
                    .as_ref()
                    .and_then(|value| value.get("marks"))
                    .cloned()
                    .unwrap_or_else(|| json!([])),
                "marks_geometry_note": marks_value
                    .as_ref()
                    .and_then(|value| value.get("geometry_note"))
                    .cloned()
                    .unwrap_or(Value::Null),
                "coordinate_space": "display",
                "note": "Input coordinates are display pixels. For coordinates measured on this image, divide by scale and add origin_x/origin_y.",
            });
            Ok(CallResult::image(png, "image/png", &metadata))
        }
        "click" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let button = button_arg(args)?;
            let count = opt_u64(args, "count").unwrap_or(1).clamp(1, 20) as u32;
            let modifiers = args
                .get("modifiers")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let target = opt_u64(args, "mark")
                .map(|mark| {
                    session
                        .marks()
                        .into_iter()
                        .find(|item| u64::from(item.mark) == mark)
                        .ok_or_else(|| {
                            anyhow!(
                                "mark {mark} is not in the last set-of-marks screenshot; call screenshot with marks=true first"
                            )
                        })
                })
                .transpose()?;

            if let Some(mark) = &target {
                // GTK4 reports unreliable child positions, so prefer semantic
                // accessibility actions for marked elements.
                let role = mark.role.to_ascii_lowercase();
                let text_role =
                    role.contains("text") || role.contains("entry") || role.contains("password");
                let actionable = mark.actions.iter().any(|action| {
                    let action = action.to_ascii_lowercase();
                    let words: &[&str] = if text_role {
                        &["click", "press", "toggle"]
                    } else {
                        &["click", "press", "activate", "toggle"]
                    };
                    words.iter().any(|word| action.contains(word))
                });
                let plain_left = button == 1 && count == 1 && modifiers.is_empty();
                if plain_left {
                    if let Some(python) = manager.tools.python.as_ref() {
                        // Clicking an entry means "focus it", not "activate
                        // the default button".
                        if text_role && !mark.name.is_empty() {
                            let mut result = a11y::focus(
                                &session,
                                Some(python),
                                Some(&mark.name),
                                None,
                                Some(&mark.role),
                                0,
                            )
                            .await?;
                            if result.get("focused").and_then(Value::as_bool) == Some(true) {
                                result["mark"] = json!(mark.mark);
                                result["via"] = json!("mark/focus");
                                return Ok(CallResult::text(&result));
                            }
                        }
                        if actionable && !mark.path.is_empty() {
                            let mut result =
                                a11y::click_path(&session, Some(python), &mark.path).await?;
                            if result.get("did_action").and_then(Value::as_bool) == Some(true) {
                                result["mark"] = json!(mark.mark);
                                result["via"] = json!("mark/a11y");
                                return Ok(CallResult::text(&result));
                            }
                        }
                    }
                }
            }

            let (x, y) = match &target {
                Some(mark) => mark.center(),
                None => (
                    required_i64(args, "x")? as i32,
                    required_i64(args, "y")? as i32,
                ),
            };
            let mut result = input::click(&session, &tool, x, y, button, count, &modifiers).await?;
            if let Some(mark) = &target {
                result["mark"] = json!(mark.mark);
                result["via"] = json!("mark/xtest");
            }
            Ok(CallResult::text(&result))
        }
        "mouse_down" | "mouse_up" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let button = button_arg(args)?;
            let at = match (opt_i64(args, "x"), opt_i64(args, "y")) {
                (Some(x), Some(y)) => Some((x as i32, y as i32)),
                (None, None) => None,
                _ => return Err(anyhow!("mouse_down/mouse_up need both x and y, or neither")),
            };
            Ok(CallResult::text(
                &input::mouse_button(&session, &tool, at, button, name == "mouse_down").await?,
            ))
        }
        "key_down" | "key_up" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let combo = required_str(args, "key")?;
            Ok(CallResult::text(
                &input::key_hold(&session, &tool, combo, name == "key_down").await?,
            ))
        }
        "move" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let x = required_i64(args, "x")? as i32;
            let y = required_i64(args, "y")? as i32;
            Ok(CallResult::text(
                &input::move_to(&session, &tool, x, y).await?,
            ))
        }
        "drag" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let from = (
                required_i64(args, "x1")? as i32,
                required_i64(args, "y1")? as i32,
            );
            let to = (
                required_i64(args, "x2")? as i32,
                required_i64(args, "y2")? as i32,
            );
            let button = button_arg(args)?;
            let duration = opt_u64(args, "duration_ms").unwrap_or(350).min(10000) as u32;
            Ok(CallResult::text(
                &input::drag(&session, &tool, from, to, button, duration).await?,
            ))
        }
        "scroll" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let amount = opt_u64(args, "amount").unwrap_or(3).clamp(1, 100) as u32;
            let direction = opt_str(args, "direction").unwrap_or("down");
            let up = match direction.to_ascii_lowercase().as_str() {
                "up" => true,
                "down" => false,
                other => return Err(anyhow!("direction must be 'up' or 'down', got {other:?}")),
            };
            let at = match (opt_i64(args, "x"), opt_i64(args, "y")) {
                (Some(x), Some(y)) => Some((x as i32, y as i32)),
                _ => None,
            };
            Ok(CallResult::text(
                &input::scroll(&session, &tool, at, up, amount).await?,
            ))
        }
        "type" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let text = required_str(args, "text")?;
            let delay = opt_u64(args, "delay_ms").unwrap_or(12).min(1000) as u32;
            Ok(CallResult::text(
                &input::type_text(&session, &tool, text, delay).await?,
            ))
        }
        "key" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let combo = required_str(args, "combo")?;
            let delay = opt_u64(args, "delay_ms").unwrap_or(12).min(1000) as u32;
            Ok(CallResult::text(
                &input::key(&session, &tool, combo, delay).await?,
            ))
        }
        "wait_for" => {
            let session = active(manager).await?;
            let condition = required_str(args, "condition")?;
            let timeout = Duration::from_millis(
                opt_u64(args, "timeout_ms")
                    .unwrap_or(10_000)
                    .clamp(1, 120_000),
            );
            let poll =
                Duration::from_millis(opt_u64(args, "poll_ms").unwrap_or(100).clamp(20, 5000));
            let spec = wait::WaitSpec {
                condition: parse_wait_condition(condition, args)?,
                timeout,
                poll,
            };
            Ok(CallResult::text(&wait::run(&session, &spec).await?))
        }
        "wait_for_idle" => {
            let session = active(manager).await?;
            let timeout = Duration::from_millis(
                opt_u64(args, "timeout_ms")
                    .unwrap_or(10_000)
                    .clamp(1, 120_000),
            );
            let poll =
                Duration::from_millis(opt_u64(args, "poll_ms").unwrap_or(100).clamp(20, 5000));
            let region = args.get("region").map(wait::parse_region).transpose()?;
            let spec = wait::WaitSpec {
                condition: wait::Condition::ScreenStable {
                    region,
                    stable_ms: opt_u64(args, "stable_ms").unwrap_or(500).min(60_000),
                },
                timeout,
                poll,
            };
            Ok(CallResult::text(&wait::run(&session, &spec).await?))
        }
        "clipboard_get" => {
            let session = active(manager).await?;
            let display = session.info.display.clone();
            let text = tokio::task::spawn_blocking(move || clipboard::get(&display))
                .await
                .context("clipboard read task failed")??;
            Ok(CallResult::text(&json!({
                "display": session.info.display,
                "text": text,
                "length": text.chars().count(),
            })))
        }
        "clipboard_set" => {
            let session = active(manager).await?;
            let text = required_str(args, "text")?.to_string();
            let display = session.info.display.clone();
            let length = text.chars().count();
            tokio::task::spawn_blocking(move || clipboard::set(&display, &text))
                .await
                .context("clipboard write task failed")??;
            Ok(CallResult::text(&json!({
                "display": session.info.display,
                "ok": true,
                "length": length,
                "note": "Selection ownership is served by a background thread for as long as the MCP server runs.",
            })))
        }
        "list_processes" => {
            let session = active(manager).await?;
            let runtime_dir = session.info.runtime_dir.clone();
            let launched = session
                .launched
                .lock()
                .map(|list| {
                    list.iter()
                        .map(|item| {
                            (
                                item.pid,
                                json!({
                                    "command": item.command,
                                    "log": item.log.display().to_string(),
                                    "uptime_ms": item.started_at.elapsed().as_millis() as u64,
                                }),
                            )
                        })
                        .collect::<std::collections::HashMap<_, _>>()
                })
                .unwrap_or_default();
            let processes =
                tokio::task::spawn_blocking(move || proc::list_session_processes(&runtime_dir))
                    .await
                    .context("process listing task failed")?;
            let items: Vec<Value> = processes
                .iter()
                .map(|process| {
                    let launch = launched.get(&process.pid);
                    json!({
                        "pid": process.pid,
                        "ppid": process.ppid,
                        "pgid": process.pgid,
                        "comm": process.comm,
                        "cmdline": process.cmdline,
                        "launched": launch.is_some(),
                        "launch": launch,
                        "started_ticks": process.started_ticks,
                    })
                })
                .collect();
            Ok(CallResult::text(&json!({
                "display": session.info.display,
                "count": items.len(),
                "processes": items,
            })))
        }
        "kill_process" => {
            let session = active(manager).await?;
            let pid = opt_u64(args, "pid").ok_or_else(|| anyhow!("missing pid"))? as u32;
            let signal = match opt_str(args, "signal")
                .unwrap_or("TERM")
                .to_ascii_uppercase()
                .as_str()
            {
                "TERM" => libc::SIGTERM,
                "KILL" => libc::SIGKILL,
                "INT" => libc::SIGINT,
                "HUP" => libc::SIGHUP,
                "QUIT" => libc::SIGQUIT,
                other => return Err(anyhow!("unsupported signal {other:?}")),
            };
            let group = opt_bool(args, "group").unwrap_or(false);
            let runtime_dir = session.info.runtime_dir.clone();
            tokio::task::spawn_blocking(move || {
                proc::kill_session_process(pid, &runtime_dir, signal, group)
            })
            .await
            .context("kill task failed")??;
            Ok(CallResult::text(&json!({
                "display": session.info.display,
                "pid": pid,
                "signal": opt_str(args, "signal").unwrap_or("TERM"),
                "group": group,
                "ok": true,
            })))
        }
        "tail_log" => {
            let session = active(manager).await?;
            let file = required_str(args, "file")?;
            let lines = opt_u64(args, "lines").unwrap_or(100).clamp(1, 2000) as usize;
            let requested = std::path::PathBuf::from(file);
            let path = if requested.is_absolute() {
                requested
            } else {
                session.info.log_dir.join(requested)
            };
            let log_dir = session.info.log_dir.clone();
            let runtime_dir = session.info.runtime_dir.clone();
            let resolved = path
                .canonicalize()
                .with_context(|| format!("cannot resolve log path {}", path.display()))?;
            let canonical_log = log_dir.canonicalize().unwrap_or(log_dir);
            let canonical_runtime = runtime_dir.canonicalize().unwrap_or(runtime_dir);
            if !resolved.starts_with(&canonical_log) && !resolved.starts_with(&canonical_runtime) {
                return Err(anyhow!(
                    "log path must be inside the session log directory ({})",
                    canonical_log.display()
                ));
            }
            let resolved_for_read = resolved.clone();
            let text = tokio::task::spawn_blocking(move || {
                proc::tail_bounded(&resolved_for_read, lines, 512 * 1024)
            })
            .await
            .context("log read task failed")??;
            Ok(CallResult::text(&json!({
                "display": session.info.display,
                "file": resolved.display().to_string(),
                "requested_lines": lines,
                "lines": text.lines().count(),
                "text": text,
            })))
        }
        "a11y_find" => {
            let session = active(manager).await?;
            let name = opt_str(args, "name");
            let role = opt_str(args, "role");
            let state = opt_str(args, "state");
            let action = opt_str(args, "action");
            let path = opt_str(args, "path");
            if name.is_none()
                && role.is_none()
                && state.is_none()
                && action.is_none()
                && path.is_none()
            {
                return Err(anyhow!(
                    "a11y_find needs at least one of 'name', 'role', 'state', 'action', or 'path'"
                ));
            }
            let result = a11y::find(
                &session,
                manager.tools.python.as_ref(),
                name,
                role,
                state,
                action,
                path,
                opt_bool(args, "exact").unwrap_or(false),
                opt_u64(args, "limit").unwrap_or(20) as u32,
                opt_u64(args, "max_depth").unwrap_or(12) as u32,
                opt_u64(args, "max_nodes").unwrap_or(400) as u32,
            )
            .await?;
            Ok(CallResult::text(&result))
        }
        "a11y_set_text" => {
            let session = active(manager).await?;
            let name = opt_str(args, "name");
            let path = opt_str(args, "path");
            if name.is_none() && path.is_none() {
                return Err(anyhow!("a11y_set_text needs 'name' or 'path'"));
            }
            let text = required_str(args, "text")?;
            let result = a11y::set_text(
                &session,
                manager.tools.python.as_ref(),
                name,
                path,
                opt_str(args, "role"),
                opt_u64(args, "index").unwrap_or(0) as u32,
                text,
            )
            .await?;
            Ok(CallResult::text(&result))
        }
        "a11y_focus" => {
            let session = active(manager).await?;
            let name = opt_str(args, "name");
            let path = opt_str(args, "path");
            let role = opt_str(args, "role");
            if name.is_none() && path.is_none() && role.is_none() {
                return Err(anyhow!("a11y_focus needs 'name', 'path', or 'role'"));
            }
            let result = a11y::focus(
                &session,
                manager.tools.python.as_ref(),
                name,
                path,
                role,
                opt_u64(args, "index").unwrap_or(0) as u32,
            )
            .await?;
            Ok(CallResult::text(&result))
        }
        "diagnose" => {
            let report = diagnose(manager).await;
            Ok(CallResult::text(&report))
        }
        "list_windows" => {
            let session = active(manager).await?;
            let display = session.info.display.clone();
            let list = tokio::task::spawn_blocking(move || windows::list(&display))
                .await
                .context("window listing task failed")??;
            let windows: Vec<Value> = list.iter().map(|window| window.to_json()).collect();
            Ok(CallResult::text(&json!({
                "display": session.info.display,
                "count": windows.len(),
                "windows": windows,
            })))
        }
        "focus_window" => {
            let session = active(manager).await?;
            let id = x11::parse_window_id(args.get("id").unwrap_or(&Value::Null))?;
            let display = session.info.display.clone();
            let result = tokio::task::spawn_blocking(move || windows::focus(&display, id))
                .await
                .context("focus task failed")??;
            Ok(CallResult::text(&result))
        }
        "move_resize_window" => {
            let session = active(manager).await?;
            let id = x11::parse_window_id(args.get("id").unwrap_or(&Value::Null))?;
            let x = clamp_i16(required_i64(args, "x")?)?;
            let y = clamp_i16(required_i64(args, "y")?)?;
            let width = clamp_u16(required_i64(args, "width")?)?;
            let height = clamp_u16(required_i64(args, "height")?)?;
            if width == 0 || height == 0 {
                return Err(anyhow!("width and height must be greater than zero"));
            }
            let display = session.info.display.clone();
            let result = tokio::task::spawn_blocking(move || {
                windows::move_resize(&display, id, x, y, width, height)
            })
            .await
            .context("move/resize task failed")??;
            Ok(CallResult::text(&result))
        }
        "close_window" => {
            let session = active(manager).await?;
            let id = x11::parse_window_id(args.get("id").unwrap_or(&Value::Null))?;
            let force = opt_bool(args, "force").unwrap_or(false);
            let display = session.info.display.clone();
            let result = tokio::task::spawn_blocking(move || windows::close(&display, id, force))
                .await
                .context("close task failed")??;
            Ok(CallResult::text(&result))
        }
        "window_state" => {
            let session = active(manager).await?;
            let id = x11::parse_window_id(args.get("id").unwrap_or(&Value::Null))?;
            let action = required_str(args, "action")?.to_string();
            let display = session.info.display.clone();
            let result =
                tokio::task::spawn_blocking(move || windows::action(&display, id, &action))
                    .await
                    .context("window state task failed")??;
            Ok(CallResult::text(&result))
        }
        "resize_display" => {
            let session = active(manager).await?;
            let width = opt_u64(args, "width")
                .ok_or_else(|| anyhow!("missing width"))?
                .clamp(64, 8192) as u16;
            let height = opt_u64(args, "height")
                .ok_or_else(|| anyhow!("missing height"))?
                .clamp(64, 8192) as u16;
            let refresh = args
                .get("refresh")
                .and_then(Value::as_f64)
                .map(|v| v as f32);
            let display = session.info.display.clone();
            let result = tokio::task::spawn_blocking(move || {
                randr::resize(&display, width, height, refresh)
            })
            .await
            .context("resize task failed")??;
            session
                .set_size(u32::from(width), u32::from(height))
                .context("display resized but the state file could not be updated")?;
            Ok(CallResult::text(&result))
        }
        "trace_tail" => {
            let session_id = match opt_str(args, "session_id") {
                Some(id) => Some(id.to_string()),
                None => manager
                    .session()
                    .await
                    .map(|session| session.info.session_id.clone()),
            };
            let lines = opt_u64(args, "lines").unwrap_or(100).clamp(1, 2000) as usize;
            let (file, text) =
                tokio::task::spawn_blocking(move || trace::read_tail(session_id.as_deref(), lines))
                    .await
                    .context("trace read task failed")??;
            Ok(CallResult::text(&json!({
                "file": file.display().to_string(),
                "requested_lines": lines,
                "lines": text.lines().count(),
                "text": text,
            })))
        }
        "a11y_tree" => {
            let session = active(manager).await?;
            let max_depth = opt_u64(args, "max_depth").unwrap_or(12) as u32;
            let max_nodes = opt_u64(args, "max_nodes").unwrap_or(400) as u32;
            let app = opt_str(args, "app");
            let path = opt_str(args, "path");
            let result = a11y::tree(
                &session,
                manager.tools.python.as_ref(),
                max_depth,
                max_nodes,
                app,
                path,
            )
            .await?;
            Ok(CallResult::text(&result))
        }
        "a11y_click" => {
            let session = active(manager).await?;
            let name = opt_str(args, "name");
            let role = opt_str(args, "role");
            let path = opt_str(args, "path");
            if name.is_none() && path.is_none() {
                return Err(anyhow!("a11y_click needs 'name' or 'path'"));
            }
            let index = opt_u64(args, "index").unwrap_or(0) as u32;
            let result = a11y::click(
                &session,
                manager.tools.python.as_ref(),
                manager.tools.xdotool.as_ref(),
                name,
                role,
                path,
                index,
            )
            .await?;
            Ok(CallResult::text(&result))
        }
        other => Err(anyhow!(
            "unknown tool: {other}. Call tools/list for the current tool names."
        )),
    }
}

async fn active(manager: &DisplayManager) -> Result<Arc<Session>> {
    manager
        .session()
        .await
        .ok_or_else(|| anyhow!("no nested display is running; call display_start first"))
}

fn xdotool(manager: &DisplayManager) -> Result<crate::env::Tool> {
    manager
        .tools
        .xdotool
        .clone()
        .ok_or_else(|| anyhow!("xdotool was not found; install it to use input tools"))
}

async fn launch_process(session: &Arc<Session>, command: &str, wait_ms: u64) -> Result<Value> {
    let command = command.trim();
    if command.is_empty() {
        return Err(anyhow!("command must not be empty"));
    }

    let seq = session.next_launch_seq();
    let log = session.info.log_dir.join(format!("launch-{seq}.log"));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .with_context(|| format!("cannot open {}", log.display()))?;
    let file2 = file.try_clone()?;

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd.env_clear();
    for (key, value) in session.nested_env() {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(file));
    cmd.stderr(Stdio::from(file2));
    crate::proc::detach(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot launch {command:?} on {}", session.info.display))?;
    let pid = child.id().unwrap_or(0);
    let started_at = std::time::Instant::now();

    tokio::time::sleep(Duration::from_millis(wait_ms.min(10_000))).await;
    let status = child.try_wait().context("cannot poll launched process")?;

    session.record(crate::display::Launched {
        pid,
        command: command.to_string(),
        log: log.clone(),
        started_at,
    });

    let (running, exit_code, output_tail) = match status {
        None => {
            // Reap in the background so `launch_status` can report the exit
            // status long after this call has returned.
            let session = session.clone();
            tokio::spawn(async move {
                if let Ok(status) = child.wait().await {
                    use std::os::unix::process::ExitStatusExt;
                    session.record_exit(
                        pid,
                        status.code(),
                        status.signal(),
                        started_at.elapsed().as_millis() as u64,
                    );
                }
            });
            (true, None, None)
        }
        Some(status) => {
            use std::os::unix::process::ExitStatusExt;
            session.record_exit(
                pid,
                status.code(),
                status.signal(),
                started_at.elapsed().as_millis() as u64,
            );
            let tail = crate::proc::tail(&log, 8);
            (
                false,
                status.code(),
                (!tail.trim().is_empty()).then_some(tail),
            )
        }
    };

    Ok(json!({
        "display": session.info.display,
        "pid": pid,
        "pgid": pid,
        "running": running,
        "exit_code": exit_code,
        "log": log.display().to_string(),
        "output_tail": output_tail,
        "status_tool": "launch_status",
    }))
}

fn parse_wait_condition(name: &str, args: &Value) -> Result<wait::Condition> {
    match name {
        "window" => {
            let title = opt_str(args, "title").map(str::to_string);
            let class = opt_str(args, "class").map(str::to_string);
            if title.is_none() && class.is_none() {
                return Err(anyhow!(
                    "wait_for window needs 'title' and/or 'class' to match"
                ));
            }
            Ok(wait::Condition::Window {
                title,
                class,
                appear: opt_bool(args, "appear").unwrap_or(true),
            })
        }
        "process_exit" => {
            let pid = opt_u64(args, "pid")
                .filter(|pid| *pid > 1)
                .ok_or_else(|| anyhow!("wait_for process_exit needs a pid greater than 1"))?;
            Ok(wait::Condition::ProcessExit { pid: pid as u32 })
        }
        "screen_stable" => Ok(wait::Condition::ScreenStable {
            region: args.get("region").map(wait::parse_region).transpose()?,
            stable_ms: opt_u64(args, "stable_ms").unwrap_or(500).min(60_000),
        }),
        "region_change" => {
            let region = args
                .get("region")
                .ok_or_else(|| anyhow!("wait_for region_change needs a 'region' object"))?;
            Ok(wait::Condition::RegionChange {
                region: wait::parse_region(region)?,
            })
        }
        "file_exists" => Ok(wait::Condition::FileExists {
            path: std::path::PathBuf::from(required_str(args, "path")?),
            present: opt_bool(args, "present").unwrap_or(true),
        }),
        other => Err(anyhow!(
            "unknown wait condition {other:?}; use window, process_exit, screen_stable, region_change, or file_exists"
        )),
    }
}

async fn diagnose(manager: &DisplayManager) -> Value {
    let mut tools: Vec<Value> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    let mut add = |name: &str, path: &Option<Tool>| match path {
        Some(tool) => tools.push(json!({
            "name": name,
            "available": true,
            "path": tool.path.display().to_string(),
        })),
        None => {
            missing.push(name.to_string());
            tools.push(json!({
                "name": name,
                "available": false,
            }));
        }
    };
    add("Xephyr", &manager.tools.xephyr);
    add("Xvfb", &manager.tools.xvfb);
    add("window_manager", &manager.tools.wm);
    add("xdotool", &manager.tools.xdotool);
    add("python3", &manager.tools.python);
    add("gdbus", &manager.tools.gdbus);
    add("dbus-launch", &manager.tools.dbus_launch);

    let version_targets: Vec<(&str, Tool, Vec<&str>)> = [
        (
            "window_manager",
            manager.tools.wm.clone(),
            vec!["--version"],
        ),
        ("xdotool", manager.tools.xdotool.clone(), vec!["--version"]),
        ("python3", manager.tools.python.clone(), vec!["--version"]),
        ("gdbus", manager.tools.gdbus.clone(), vec!["--version"]),
        (
            "dbus-launch",
            manager.tools.dbus_launch.clone(),
            vec!["--version"],
        ),
    ]
    .into_iter()
    .filter_map(|(name, tool, args)| tool.map(|tool| (name, tool, args)))
    .collect();

    for (name, tool, args) in version_targets {
        let version = command_version(&tool.path, &tool.extra, &args).await;
        if let Some(item) = tools
            .iter_mut()
            .find(|item| item["name"].as_str() == Some(name))
        {
            item["version"] = json!(version);
        }
    }

    let gi = match manager.tools.python.as_ref() {
        Some(python) => {
            command_ok(
                &python.path,
                &python.extra,
                &["-c", "import gi; gi.require_version('Atspi', '2.0')"],
            )
            .await
        }
        None => false,
    };

    let session = manager.session().await;
    let active = session.as_ref().map(|session| {
        json!({
            "session_id": session.info.session_id,
            "display": session.info.display,
            "display_reachable": x11::connect(&session.info.display).is_ok(),
            "width": session.width(),
            "height": session.height(),
            "visible": session.info.visible,
            "a11y_ready": session.info.a11y_ready,
            "runtime_dir": session.info.runtime_dir.display().to_string(),
            "log_dir": session.info.log_dir.display().to_string(),
            "home": session.info.home.as_ref().map(|path| path.display().to_string()),
            "persistent_home": session.info.persistent_home,
            "trace": trace::session_path(&session.info.session_id).display().to_string(),
            "launched": session.launch_count(),
        })
    });

    json!({
        "ok": missing.is_empty(),
        "host_display": crate::env::host_display(),
        "runtime_root": manager
            .tools
            .runtime
            .as_ref()
            .map(|path| path.display().to_string()),
        "state_dir": crate::env::shared_runtime_dir().display().to_string(),
        "trace_dir": trace::traces_dir().display().to_string(),
        "trace_screenshots": trace::screenshots_enabled(),
        "homes_dir": crate::env::shared_runtime_dir().join("homes").display().to_string(),
        "idle_timeout_secs": manager.idle_timeout.map(|value| value.as_secs()),
        "max_lifetime_secs": manager.max_lifetime.map(|value| value.as_secs()),
        "python_atspi": gi,
        "active_session": active,
        "sessions_on_host": crate::state::list_sessions().len(),
        "tools": tools,
        "missing": missing,
        "notes": [
            "Xephyr/Xvfb are found in the bundled runtime tree even when they are absent from the host PATH.",
            "X servers do not implement -version; their path identifies the bundled build.",
            "The nested desktop is X11-only; the host may be Wayland through XWayland.",
            "AT-SPI requires PyGObject in the python3 environment (python_atspi).",
            "XEPHYR_MCP_IDLE_TIMEOUT_SECS and XEPHYR_MCP_MAX_LIFETIME_SECS enable the auto-teardown watchdog.",
            "Set persist_home=true to keep app profiles across display_stop/display_start with the same session_id.",
            "Set XEPHYR_MCP_TRACE_SCREENSHOTS=1 to save before/after PNGs next to the JSONL trace (newest 400 kept per session).",
        ],
    })
}

async fn command_version(
    path: &std::path::Path,
    extra: &[(String, String)],
    args: &[&str],
) -> Option<String> {
    let mut command = tokio::process::Command::new(path);
    command.args(args);
    command.env_clear();
    command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    command.env("HOME", std::env::var_os("HOME").unwrap_or_default());
    for (key, value) in extra {
        command.env(key, value);
    }
    let output = tokio::time::timeout(Duration::from_secs(3), command.output())
        .await
        .ok()?
        .ok()?;
    let text = if output.stdout.is_empty() {
        String::from_utf8_lossy(&output.stderr).into_owned()
    } else {
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        text
    };
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(line.chars().take(160).collect())
}

async fn command_ok(path: &std::path::Path, extra: &[(String, String)], args: &[&str]) -> bool {
    let mut command = tokio::process::Command::new(path);
    command.args(args);
    command.env_clear();
    command.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    command.env("HOME", std::env::var_os("HOME").unwrap_or_default());
    for (key, value) in extra {
        command.env(key, value);
    }
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

fn mark_list_count(value: &Option<Value>) -> usize {
    value
        .as_ref()
        .and_then(|value| value.get("marks"))
        .and_then(Value::as_array)
        .map(|items| items.len())
        .unwrap_or(0)
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("missing required string argument '{key}'"))
}

fn opt_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn opt_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64).or_else(|| {
        args.get(key)
            .and_then(Value::as_i64)
            .and_then(|v| u64::try_from(v).ok())
    })
}

fn opt_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn required_i64(args: &Value, key: &str) -> Result<i64> {
    opt_i64(args, key).ok_or_else(|| anyhow!("missing required integer argument '{key}'"))
}

fn button_arg(args: &Value) -> Result<u32> {
    match args.get("button") {
        None | Some(Value::Null) => Ok(1),
        Some(Value::Number(number)) => number
            .as_u64()
            .filter(|button| (1..=9).contains(button))
            .map(|button| button as u32)
            .ok_or_else(|| anyhow!("button must be 1-9")),
        Some(Value::String(text)) => input::button_number(text),
        Some(_) => Err(anyhow!("button must be a string or number")),
    }
}

fn clamp_i16(value: i64) -> Result<i16> {
    i16::try_from(value).map_err(|_| anyhow!("coordinate {value} is outside the X11 range"))
}

fn clamp_u16(value: i64) -> Result<u16> {
    u16::try_from(value).map_err(|_| anyhow!("dimension {value} is outside the X11 range"))
}
