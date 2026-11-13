//! Tool definitions and handlers.

use crate::display::{DisplayManager, Session};
use crate::mcp::CallResult;
use crate::{a11y, input, screenshot, windows, x11};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

pub fn definitions() -> Vec<Value> {
    vec![
        tool(
            "display_start",
            "Start the private desktop. Call this before any other xephyr-mcp tool. visible=true launches Xephyr (a watchable window) on the host X11/XWayland display; visible=false launches headless Xvfb. Also starts a window manager and private D-Bus/AT-SPI buses so launched apps cannot reach the host session bus. Each server process owns one display: call display_stop before starting another, or run separate server instances with distinct session_id values for parallel desktops. Returns display, pids, dbus_address, runtime/log dirs, and accessibility status.",
            schema(
                json!({
                    "width": { "type": "integer", "minimum": 64, "maximum": 8192, "description": "Screen width in pixels (default 1280)" },
                    "height": { "type": "integer", "minimum": 64, "maximum": 8192, "description": "Screen height in pixels (default 800)" },
                    "visible": { "type": "boolean", "description": "Xephyr window (true, default) or headless Xvfb (false)" },
                    "session_id": { "type": "string", "description": "Optional caller-supplied identifier (1-64 letters/digits/-_.). Used by the viewer and to keep multiple server instances apart. Generated if omitted." }
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
            "screenshot",
            "Capture the nested root window and return a PNG image. The image is downscaled to fit max_width/max_height (default 1280x800); the text block reports both image and display dimensions plus scale. Input tools use DISPLAY pixel coordinates, so convert image coordinates with display = image / scale.",
            schema(
                json!({
                    "max_width": { "type": "integer", "minimum": 64, "maximum": 4096, "description": "Maximum returned image width (default 1280)" },
                    "max_height": { "type": "integer", "minimum": 64, "maximum": 4096, "description": "Maximum returned image height (default 800)" }
                }),
                &[],
            ),
        ),
        tool(
            "click",
            "Move the nested pointer to (x, y) in display pixels and click. Uses XTEST on the nested X server; the host cursor is untouched. Call focus_window first when the target window is not focused.",
            schema(
                json!({
                    "x": { "type": "integer", "description": "Display X coordinate" },
                    "y": { "type": "integer", "description": "Display Y coordinate" },
                    "button": { "description": "left (default), middle, right, or a button number", "oneOf": [{ "type": "string" }, { "type": "integer" }] },
                    "count": { "type": "integer", "minimum": 1, "maximum": 20, "description": "Click count (default 1)" }
                }),
                &["x", "y"],
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
            "a11y_tree",
            "Read the AT-SPI accessibility tree of the nested desktop: roles, names, actions, states, and screen rectangles. Much cheaper and more precise than screenshots for forms, buttons and menus. Requires GTK/Qt accessibility to be enabled (it is for launched apps) and PyGObject.",
            schema(
                json!({
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 64, "description": "Tree depth (default 12)" },
                    "max_nodes": { "type": "integer", "minimum": 1, "maximum": 4000, "description": "Node budget (default 400)" },
                    "app": { "type": "string", "description": "Only include applications whose name contains this string" }
                }),
                &[],
            ),
        ),
        tool(
            "a11y_click",
            "Find an accessible by name (case-insensitive; exact matches win), optionally filtered by role, and invoke its first action. If the node has no action, click its center with XTEST. Returns the matched node and how it was activated.",
            schema(
                json!({
                    "name": { "type": "string", "description": "Accessible name (button label, menu item, field label, ...)" },
                    "role": { "type": "string", "description": "Optional role filter, e.g. 'push button', 'menu item', 'check box'" },
                    "index": { "type": "integer", "minimum": 0, "maximum": 200, "description": "Pick the Nth best match (default 0)" }
                }),
                &["name"],
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
    match dispatch(manager, name, args).await {
        Ok(result) => result,
        Err(err) => CallResult::error(format!("{err:#}")),
    }
}

async fn dispatch(manager: &DisplayManager, name: &str, args: &Value) -> Result<CallResult> {
    match name {
        "display_start" => {
            let width = opt_u64(args, "width").unwrap_or(1280) as u32;
            let height = opt_u64(args, "height").unwrap_or(800) as u32;
            let visible = opt_bool(args, "visible").unwrap_or(true);
            let session_id = opt_str(args, "session_id").map(str::to_string);
            let report = manager.start(width, height, visible, session_id).await?;
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
        "screenshot" => {
            let session = active(manager).await?;
            let max_width = opt_u64(args, "max_width").unwrap_or(1280).clamp(64, 4096) as u32;
            let max_height = opt_u64(args, "max_height").unwrap_or(800).clamp(64, 4096) as u32;
            let display = session.info.display.clone();
            let result = tokio::task::spawn_blocking(move || {
                screenshot::capture_png(&display, max_width, max_height)
            })
            .await
            .context("screenshot task failed")??;
            let metadata = json!({
                "display": session.info.display,
                "display_width": result.source_width,
                "display_height": result.source_height,
                "image_width": result.width,
                "image_height": result.height,
                "scale": result.scale(),
                "coordinate_space": "display",
                "note": "Input coordinates are display pixels. For coordinates measured on this image, divide by scale.",
            });
            Ok(CallResult::image(result.png, "image/png", &metadata))
        }
        "click" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let x = required_i64(args, "x")? as i32;
            let y = required_i64(args, "y")? as i32;
            let button = button_arg(args)?;
            let count = opt_u64(args, "count").unwrap_or(1).clamp(1, 20) as u32;
            Ok(CallResult::text(&input::click(&session, &tool, x, y, button, count).await?))
        }
        "move" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let x = required_i64(args, "x")? as i32;
            let y = required_i64(args, "y")? as i32;
            Ok(CallResult::text(&input::move_to(&session, &tool, x, y).await?))
        }
        "drag" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let from = (required_i64(args, "x1")? as i32, required_i64(args, "y1")? as i32);
            let to = (required_i64(args, "x2")? as i32, required_i64(args, "y2")? as i32);
            let button = button_arg(args)?;
            let duration = opt_u64(args, "duration_ms").unwrap_or(350).min(10000) as u32;
            Ok(CallResult::text(&input::drag(&session, &tool, from, to, button, duration).await?))
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
            Ok(CallResult::text(&input::scroll(&session, &tool, at, up, amount).await?))
        }
        "type" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let text = required_str(args, "text")?;
            let delay = opt_u64(args, "delay_ms").unwrap_or(12).min(1000) as u32;
            Ok(CallResult::text(&input::type_text(&session, &tool, text, delay).await?))
        }
        "key" => {
            let session = active(manager).await?;
            let tool = xdotool(manager)?;
            let combo = required_str(args, "combo")?;
            let delay = opt_u64(args, "delay_ms").unwrap_or(12).min(1000) as u32;
            Ok(CallResult::text(&input::key(&session, &tool, combo, delay).await?))
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
        "a11y_tree" => {
            let session = active(manager).await?;
            let max_depth = opt_u64(args, "max_depth").unwrap_or(12) as u32;
            let max_nodes = opt_u64(args, "max_nodes").unwrap_or(400) as u32;
            let app = opt_str(args, "app");
            let result = a11y::tree(&session, manager.tools.python.as_ref(), max_depth, max_nodes, app).await?;
            Ok(CallResult::text(&result))
        }
        "a11y_click" => {
            let session = active(manager).await?;
            let name = required_str(args, "name")?;
            let role = opt_str(args, "role");
            let index = opt_u64(args, "index").unwrap_or(0) as u32;
            let result = a11y::click(
                &session,
                manager.tools.python.as_ref(),
                manager.tools.xdotool.as_ref(),
                name,
                role,
                index,
            )
            .await?;
            Ok(CallResult::text(&result))
        }
        other => Err(anyhow!(
            "unknown tool: {other}. Available tools: display_start, display_stop, launch, screenshot, click, move, drag, scroll, type, key, list_windows, focus_window, move_resize_window, close_window, a11y_tree, a11y_click"
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

async fn launch_process(session: &Session, command: &str, wait_ms: u64) -> Result<Value> {
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

    tokio::time::sleep(Duration::from_millis(wait_ms.min(10_000))).await;
    let status = child.try_wait().context("cannot poll launched process")?;

    let (running, exit_code, output_tail) = match status {
        None => (true, None, None),
        Some(status) => {
            let tail = crate::proc::tail(&log, 8);
            (
                false,
                status.code(),
                (!tail.trim().is_empty()).then_some(tail),
            )
        }
    };

    if running {
        session.record(crate::display::Launched {
            pid,
            command: command.to_string(),
            log: log.clone(),
            started_at: std::time::Instant::now(),
        });
    }

    Ok(json!({
        "display": session.info.display,
        "pid": pid,
        "pgid": pid,
        "running": running,
        "exit_code": exit_code,
        "log": log.display().to_string(),
        "output_tail": output_tail,
    }))
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
