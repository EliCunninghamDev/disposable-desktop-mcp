//! Sway process helpers: generated config, socket discovery, IPC, and the
//! window operations that sway can express.
//!
//! Capture lives in `wayland.rs`; this module only speaks the sway IPC JSON
//! protocol.  Sway has no readiness fd, so discovery is a glob in the private
//! runtime directory plus a `get_version` round trip.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const GET_OUTPUTS: u32 = 3;
const GET_TREE: u32 = 4;
const GET_VERSION: u32 = 7;
const GET_INPUTS: u32 = 100;

/// Resolve the IPC socket from a private runtime directory.  Returns
/// `(wayland socket name, sway IPC path)`.
pub fn discover_sockets(runtime_dir: &Path) -> (Option<String>, Option<PathBuf>) {
    let mut wayland = None;
    let mut ipc = None;
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return (wayland, ipc);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("wayland-") && !name.ends_with(".lock") {
            wayland = Some(name);
        } else if name.starts_with("sway-ipc.") && name.ends_with(".sock") {
            ipc = Some(entry.path());
        }
    }
    (wayland, ipc)
}

/// Poll until sway has published both sockets and answers `get_version`.
pub async fn wait_ready(
    runtime_dir: &Path,
    pid: u32,
    log: &Path,
    timeout: Duration,
) -> Result<(String, PathBuf)> {
    let deadline = Instant::now() + timeout;
    loop {
        if !crate::proc::alive(pid) {
            bail!("sway exited during startup{}", log_hint(log));
        }
        let (wayland, ipc) = discover_sockets(runtime_dir);
        if let (Some(wayland), Some(ipc)) = (wayland, ipc) {
            if ipc.exists() && Sway::new(ipc.clone()).get_version().is_ok() {
                return Ok((wayland, ipc));
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for sway on {} (wayland {:?}){}",
                runtime_dir.display(),
                discover_sockets(runtime_dir).0,
                log_hint(log)
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The compositor output name and size reported by sway.
pub async fn wait_output(
    ipc: &Path,
    _width: u32,
    _height: u32,
    timeout: Duration,
) -> Result<(String, u32, u32)> {
    let deadline = Instant::now() + timeout;
    loop {
        let outputs = Sway::new(ipc.to_path_buf()).get_outputs()?;
        if let Some(output) = outputs.as_array().and_then(|list| list.first()) {
            let name = output.get("name").and_then(Value::as_str).unwrap_or("");
            let rect = output.get("rect").cloned().unwrap_or_default();
            let w = rect.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
            let h = rect.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
            if !name.is_empty() && w > 0 && h > 0 {
                return Ok((name.to_string(), w, h));
            }
        }
        if Instant::now() >= deadline {
            bail!("sway did not report an output within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn config(runtime_dir: &Path, width: u32, height: u32, xwayland: bool) -> Result<PathBuf> {
    let dir = runtime_dir.join("sway");
    crate::env::create_private_dir(&dir)?;
    let path = dir.join("config");
    let mut text = String::new();
    text.push_str(&format!("output * mode {width}x{height}\n"));
    text.push_str("default_border none\n");
    // Match the openbox model the agent already expects: real positions and
    // no reflow when one window is moved.  Dialogues are floating anyway, and
    // `class` covers XWayland clients when that is enabled.
    text.push_str("for_window [app_id=\".*\"] floating enable\n");
    text.push_str("for_window [class=\".*\"] floating enable\n");
    text.push_str("seat seat0 fallback true\n");
    // Host input devices are parked here while the agent owns the session.
    text.push_str("seat locked fallback false\n");
    if xwayland {
        // `force` starts Xwayland immediately instead of waiting for an X11
        // client, so the DISPLAY is available when launch returns.
        text.push_str("xwayland force\n");
    } else {
        text.push_str("xwayland disable\n");
    }
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(path)
}

fn log_hint(log: &Path) -> String {
    let tail = crate::proc::tail(log, 5);
    if tail.trim().is_empty() {
        String::new()
    } else {
        format!(" (log: {})", tail.trim())
    }
}

#[derive(Clone, Debug)]
pub struct Sway {
    pub socket: PathBuf,
}

impl Sway {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    fn request(&self, kind: u32, payload: &[u8]) -> Result<Vec<u8>> {
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("cannot connect to sway IPC at {}", self.socket.display()))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.write_all(MAGIC)?;
        stream.write_all(&(payload.len() as u32).to_le_bytes())?;
        stream.write_all(&kind.to_le_bytes())?;
        stream.write_all(payload)?;
        stream.flush()?;

        let mut header = [0u8; 14];
        stream.read_exact(&mut header)?;
        if &header[..6] != MAGIC {
            bail!("sway IPC returned an invalid header");
        }
        let length = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
        if length > 32 * 1024 * 1024 {
            bail!("sway IPC reply is implausibly large ({length} bytes)");
        }
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body)?;
        Ok(body)
    }

    fn json(&self, kind: u32, payload: &[u8]) -> Result<Value> {
        let body = self.request(kind, payload)?;
        serde_json::from_slice(&body)
            .with_context(|| format!("cannot parse sway IPC reply from {}", self.socket.display()))
    }

    pub fn get_version(&self) -> Result<Value> {
        self.json(GET_VERSION, b"")
    }

    pub fn get_tree(&self) -> Result<Value> {
        self.json(GET_TREE, b"")
    }

    pub fn get_outputs(&self) -> Result<Value> {
        self.json(GET_OUTPUTS, b"")
    }

    pub fn get_inputs(&self) -> Result<Value> {
        self.json(GET_INPUTS, b"")
    }

    /// Run an IPC command and fail when sway reports an unsuccessful node.
    pub fn run(&self, command: &str) -> Result<Value> {
        let reply = self.json(RUN_COMMAND, command.as_bytes())?;
        if let Some(items) = reply.as_array() {
            for item in items {
                if item.get("success").and_then(Value::as_bool) == Some(false) {
                    let error = item
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("sway rejected the command");
                    bail!("{error}");
                }
            }
        }
        Ok(reply)
    }

    fn run_ok(&self, command: &str) -> Result<()> {
        self.run(command).map(|_| ())
    }
}

#[derive(Clone, Debug)]
pub struct SwayWindow {
    pub id: i64,
    pub title: String,
    pub app_id: Option<String>,
    pub class: Option<String>,
    pub instance: Option<String>,
    pub pid: Option<u32>,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub focused: bool,
    pub floating: bool,
    pub visible: bool,
    pub workspace: Option<String>,
    pub stack: usize,
}

impl SwayWindow {
    pub fn handle(&self) -> crate::backend::WinId {
        crate::backend::WinId::wayland(self.id.to_string())
    }

    pub fn to_window_info(&self) -> crate::windows::WindowInfo {
        crate::windows::WindowInfo::wayland(
            self.handle(),
            self.title.clone(),
            self.app_id.clone(),
            // Wayland-native clients have an app_id and no WM_CLASS.  Mirror it
            // into class/instance so wait_for class= and existing filters work.
            self.class.clone().or_else(|| self.app_id.clone()),
            self.instance.clone().or_else(|| self.app_id.clone()),
            self.pid,
            self.x,
            self.y,
            self.width,
            self.height,
            self.focused,
            self.visible,
            self.workspace.clone(),
            self.stack,
        )
    }
}

/// Every toplevel in the tree, in tree order.  Containers without an
/// `app_id`/`window_properties` are workspaces, outputs, and split nodes.
pub fn windows(socket: &Path) -> Result<Vec<SwayWindow>> {
    let tree = Sway::new(socket.to_path_buf()).get_tree()?;
    let mut out = Vec::new();
    let mut stack = 0usize;
    walk(&tree, None, &mut stack, &mut out);
    Ok(out)
}

fn walk(node: &Value, workspace: Option<&str>, stack: &mut usize, out: &mut Vec<SwayWindow>) {
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
    let workspace = if node_type == "workspace" {
        node.get("name").and_then(Value::as_str)
    } else {
        workspace
    };

    let properties = node.get("window_properties");
    let app_id = node
        .get("app_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let class = properties
        .and_then(|value| value.get("class"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let instance = properties
        .and_then(|value| value.get("instance"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let is_toplevel = node_type == "con" || node_type == "floating_con";

    if is_toplevel && (app_id.is_some() || class.is_some()) {
        let id = node.get("id").and_then(Value::as_i64).unwrap_or(0);
        let rect = node.get("rect").cloned().unwrap_or_default();
        let number = |key: &str| rect.get(key).and_then(Value::as_i64).unwrap_or(0) as i32;
        let floating = node
            .get("floating")
            .and_then(Value::as_str)
            .map(|value| value != "auto_off" && value != "user_off")
            .unwrap_or(false);
        out.push(SwayWindow {
            id,
            title: node
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            app_id,
            class,
            instance,
            pid: node
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok()),
            x: number("x"),
            y: number("y"),
            width: number("width"),
            height: number("height"),
            focused: node
                .get("focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            floating,
            visible: node.get("visible").and_then(Value::as_bool).unwrap_or(true),
            workspace: workspace.map(str::to_string),
            stack: *stack,
        });
        *stack += 1;
    }

    for child in node
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            node.get("floating_nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
    {
        walk(child, workspace, stack, out);
    }
}

pub fn find_window(socket: &Path, id: i64) -> Result<SwayWindow> {
    windows(socket)?
        .into_iter()
        .find(|window| window.id == id)
        .ok_or_else(|| anyhow!("window wl:{id} is no longer present"))
}

pub fn focus(socket: &Path, id: i64) -> Result<Value> {
    let sway = Sway::new(socket.to_path_buf());
    sway.run_ok(&format!("[con_id={id}] focus"))?;
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        if let Ok(window) = find_window(socket, id) {
            if window.focused {
                return Ok(json!({
                    "id": window.handle().as_str(),
                    "focused": true,
                    "title": window.title,
                }));
            }
        }
        if Instant::now() >= deadline {
            return Ok(json!({
                "id": crate::backend::WinId::wayland(id.to_string()).as_str(),
                "focused": false,
                "note": "sway accepted the focus command but the window did not report focus",
            }));
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

pub fn close(socket: &Path, id: i64) -> Result<Value> {
    if let Ok(window) = find_window(socket, id) {
        Sway::new(socket.to_path_buf()).run_ok(&format!("[con_id={id}] kill"))?;
        return Ok(json!({
            "id": window.handle().as_str(),
            "closed": true,
            "title": window.title,
            "note": "sway sent the close request; the client may still refuse to exit",
        }));
    }
    Ok(json!({
        "id": crate::backend::WinId::wayland(id.to_string()).as_str(),
        "closed": false,
        "note": "window was already gone",
    }))
}

pub fn move_resize(
    socket: &Path,
    id: i64,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<Value> {
    if width == 0 || height == 0 {
        bail!("width and height must be greater than zero");
    }
    let before = find_window(socket, id)?;
    if !before.floating {
        bail!(
            "window wl:{id} is tiled; sway only allows arbitrary geometry on floating windows. \
             Move it with window_state action=float, or use backend=x11 for a tiling-free desktop."
        );
    }
    let sway = Sway::new(socket.to_path_buf());
    sway.run_ok(&format!("[con_id={id}] resize set {width} {height}"))?;
    let resized = wait_rect(socket, id, |window| {
        (window.width as u32, window.height as u32) == (width, height)
    });
    // Resize and move are deliberately separate commands.  In one command
    // list sway computes the position against the new size.
    sway.run_ok(&format!("[con_id={id}] move position {x} {y}"))?;
    let moved = wait_rect(socket, id, |window| (window.x, window.y) == (x, y));
    let window = find_window(socket, id)?;
    Ok(json!({
        "id": window.handle().as_str(),
        "x": window.x,
        "y": window.y,
        "width": window.width,
        "height": window.height,
        "requested": { "x": x, "y": y, "width": width, "height": height },
        "size_matched": resized,
        "position_matched": moved,
        "floating": window.floating,
        "note": if resized && moved { Value::Null } else { json!(
            "the client or compositor clamped the requested geometry; the result fields report the actual rect"
        )},
    }))
}

fn wait_rect(socket: &Path, id: i64, done: impl Fn(&SwayWindow) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_millis(700);
    loop {
        if let Ok(window) = find_window(socket, id) {
            if done(&window) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub fn action(socket: &Path, id: i64, action: &str) -> Result<Value> {
    let window = find_window(socket, id)?;
    let sway = Sway::new(socket.to_path_buf());
    match action {
        "state" => Ok(json!({
            "id": window.handle().as_str(),
            "title": window.title,
            "floating": window.floating,
            "focused": window.focused,
            "visible": window.visible,
            "workspace": window.workspace,
            "geometry": { "x": window.x, "y": window.y, "width": window.width, "height": window.height },
        })),
        "float" | "set_floating" => {
            sway.run_ok(&format!("[con_id={id}] floating enable"))?;
            Ok(json!({"id": window.handle().as_str(), "floating": true}))
        }
        "unfloat" | "set_tiling" => {
            sway.run_ok(&format!("[con_id={id}] floating disable"))?;
            Ok(json!({"id": window.handle().as_str(), "floating": false}))
        }
        "fullscreen" => {
            sway.run_ok(&format!("[con_id={id}] fullscreen enable"))?;
            Ok(json!({"id": window.handle().as_str(), "fullscreen": true}))
        }
        "unfullscreen" => {
            sway.run_ok(&format!("[con_id={id}] fullscreen disable"))?;
            Ok(json!({"id": window.handle().as_str(), "fullscreen": false}))
        }
        other => bail!(
            "window_state {other:?} is not supported on the wayland backend; \
             use state, float, unfloat, fullscreen, or unfullscreen"
        ),
    }
}

/// Host devices consumed by the nested compositor.  Virtual devices created by
/// this server are named `wlr_virtual_*` and stay on seat0.
pub fn host_input_ids(socket: &Path) -> Result<Vec<String>> {
    let inputs = Sway::new(socket.to_path_buf()).get_inputs()?;
    let list = inputs
        .as_array()
        .ok_or_else(|| anyhow!("sway returned a non-list for get_inputs"))?;
    Ok(list
        .iter()
        .filter_map(|item| {
            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
            let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
            if name.starts_with("wlr_virtual_") || !matches!(kind, "pointer" | "keyboard" | "touch")
            {
                return None;
            }
            item.get("identifier")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect())
}

const HOST_INPUT_BEGIN: &str = "# xephyr-mcp host input begin";
const HOST_INPUT_END: &str = "# xephyr-mcp host input end";

/// Move every host input device to `seat0` (human) or the dummy `locked` seat
/// (agent).  Sway's runtime `seat attach` is additive and is forgotten on
/// reload, so the attachment is written into the generated config and the
/// config is reloaded; that is what actually removes the device from the other
/// seat.
pub fn set_host_input(socket: &Path, config: &Path, enabled: bool) -> Result<usize> {
    let mut text = std::fs::read_to_string(config)
        .with_context(|| format!("cannot read {}", config.display()))?;
    if let Some(begin) = text.find(HOST_INPUT_BEGIN) {
        if let Some(end) = text[begin..].find(HOST_INPUT_END) {
            let end = begin + end + HOST_INPUT_END.len();
            text.replace_range(begin..end, "");
        }
    }
    let ids = host_input_ids(socket)?;
    if !ids.is_empty() {
        let seat = if enabled { "seat0" } else { "locked" };
        text.push_str(HOST_INPUT_BEGIN);
        text.push_str(&format!("\nseat {seat} {{\n"));
        for id in &ids {
            text.push_str(&format!("    attach {id}\n"));
        }
        text.push_str("}\n");
        text.push_str(HOST_INPUT_END);
        text.push('\n');
    }
    std::fs::write(config, text).with_context(|| format!("cannot write {}", config.display()))?;
    Sway::new(socket.to_path_buf()).run("reload")?;
    Ok(ids.len())
}

/// Wait for the Wayland backend to publish its host devices, then park them on
/// the dummy seat.  Headless sessions have none and return 0 without reloading.
pub async fn lock_host_input(socket: &Path, config: &Path, timeout: Duration) -> Result<usize> {
    let deadline = Instant::now() + timeout;
    loop {
        if !host_input_ids(socket)?.is_empty() {
            return set_host_input(socket, config, false);
        }
        if Instant::now() >= deadline {
            return Ok(0);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Current size of one output, or of the first output when `output` is None.
pub fn output_size(socket: &Path, output: Option<&str>) -> Result<(u32, u32)> {
    let outputs = Sway::new(socket.to_path_buf()).get_outputs()?;
    let list = outputs
        .as_array()
        .ok_or_else(|| anyhow!("sway returned a non-list for get_outputs"))?;
    let item = match output {
        Some(name) => list
            .iter()
            .find(|item| item.get("name").and_then(Value::as_str) == Some(name))
            .ok_or_else(|| anyhow!("sway no longer reports output {name:?}"))?,
        None => list
            .first()
            .ok_or_else(|| anyhow!("sway reports no outputs"))?,
    };
    let rect = item.get("rect").cloned().unwrap_or_default();
    let width = rect.get("width").and_then(Value::as_u64).unwrap_or(0) as u32;
    let height = rect.get("height").and_then(Value::as_u64).unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        bail!("sway reports an empty output rect");
    }
    Ok((width, height))
}

pub fn resize_output(socket: &Path, output: &str, width: u32, height: u32) -> Result<Value> {
    if width < 64 || height < 64 {
        bail!("width and height must be at least 64 pixels");
    }
    let sway = Sway::new(socket.to_path_buf());
    sway.run_ok(&format!("output {output} mode {width}x{height}"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut actual = (0u32, 0u32);
    loop {
        if let Ok(outputs) = sway.get_outputs() {
            if let Some(item) = outputs.as_array().and_then(|list| {
                list.iter()
                    .find(|item| item.get("name").and_then(Value::as_str) == Some(output))
            }) {
                let rect = item.get("rect").cloned().unwrap_or_default();
                actual = (
                    rect.get("width").and_then(Value::as_u64).unwrap_or(0) as u32,
                    rect.get("height").and_then(Value::as_u64).unwrap_or(0) as u32,
                );
                if actual == (width, height) {
                    break;
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "sway accepted the output mode but {output} stayed {}x{} (wanted {width}x{height})",
                actual.0,
                actual.1
            );
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Ok(json!({
        "display": output,
        "width": actual.0,
        "height": actual.1,
        "mode": format!("{width}x{height}"),
        "output": output,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn discovers_wayland_and_ipc_sockets() {
        let dir = std::env::temp_dir().join(format!("sway-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("wayland-1"), "").unwrap();
        std::fs::write(dir.join("wayland-1.lock"), "").unwrap();
        std::fs::write(dir.join("sway-ipc.1000.1.sock"), "").unwrap();
        let (wayland, ipc) = discover_sockets(&dir);
        assert_eq!(wayland.as_deref(), Some("wayland-1"));
        assert_eq!(ipc, Some(dir.join("sway-ipc.1000.1.sock")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_native_and_xwayland_toplevels() {
        let tree = json!({
            "type": "root",
            "nodes": [{
                "type": "output",
                "nodes": [{
                    "type": "workspace",
                    "name": "2",
                    "nodes": [{
                        "type": "floating_con",
                        "id": 7,
                        "name": "Alacritty",
                        "app_id": "foot",
                        "pid": 42,
                        "rect": {"x": 10, "y": 20, "width": 300, "height": 200},
                        "focused": true,
                        "floating": "user_on",
                        "visible": true
                    }],
                    "floating_nodes": [{
                        "type": "floating_con",
                        "id": 8,
                        "name": "xmessage",
                        "app_id": null,
                        "pid": 43,
                        "rect": {"x": 40, "y": 50, "width": 100, "height": 60},
                        "focused": false,
                        "floating": "user_on",
                        "visible": true,
                        "window_properties": {"class": "Xmessage", "instance": "xmessage"}
                    }]
                }]
            }]
        });
        let mut out = Vec::new();
        let mut stack = 0;
        walk(&tree, None, &mut stack, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].handle().as_str(), "wl:7");
        assert_eq!(out[0].workspace.as_deref(), Some("2"));
        assert_eq!(out[0].app_id.as_deref(), Some("foot"));
        let info = out[0].to_window_info();
        assert_eq!(info.class.as_deref(), Some("foot"));
        assert_eq!(out[1].class.as_deref(), Some("Xmessage"));
        assert!(out[1].floating);
    }
}
