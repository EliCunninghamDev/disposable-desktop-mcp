//! Session state shared with the viewer, plus conservative stale cleanup.

use crate::backend::Capabilities;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Root-window property used by the viewer to discover a nested display when
/// no state file is available.
pub const ROOT_MARKER: &str = "_XEPHYR_MCP";

fn default_backend() -> String {
    "x11".to_string()
}

fn default_control() -> String {
    "agent".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub server_pid: u32,
    #[serde(default)]
    pub session_id: String,
    pub display: String,
    pub display_number: u32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    pub xserver: String,
    pub wm: Option<String>,
    pub runtime_dir: String,
    #[serde(default)]
    pub home: Option<String>,
    #[serde(default)]
    pub persistent_home: bool,
    pub pids: Vec<u32>,
    pub started_at: u64,
    pub version: String,
    /// "x11" for existing sessions; "wayland" once that backend lands.
    #[serde(default = "default_backend")]
    pub backend: String,
    #[serde(default)]
    pub wayland_display: Option<String>,
    #[serde(default)]
    pub swaysock: Option<String>,
    #[serde(default)]
    pub output_name: Option<String>,
    /// XWayland's DISPLAY when the Wayland session enables it.
    #[serde(default)]
    pub xwayland_display: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Capabilities>,
    /// "agent" or "human"; viewer/session arbitration.
    #[serde(default = "default_control")]
    pub control: String,
    #[serde(default)]
    pub control_pid: Option<u32>,
    #[serde(default)]
    pub control_since: Option<u64>,
}

impl State {
    pub fn marker_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn sessions_dir() -> PathBuf {
    crate::env::shared_runtime_dir().join("sessions")
}

pub fn session_path(session_id: &str) -> PathBuf {
    sessions_dir().join(format!("{session_id}.json"))
}

/// Directory used as HOME for a persistent session.  It lives outside the
/// per-session runtime tree so `display_stop` leaves profiles behind.
pub fn session_home(session_id: &str) -> Result<PathBuf> {
    crate::display::validate_session_id(session_id)?;
    Ok(crate::env::shared_runtime_dir()
        .join("homes")
        .join(session_id))
}

fn write_atomic(path: &Path, state: &State) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::env::create_private_dir(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec_pretty(state)?)
        .with_context(|| format!("cannot write {}", temp.display()))?;
    std::fs::rename(&temp, path).with_context(|| format!("cannot publish {}", path.display()))?;
    Ok(())
}

pub fn write(state: &State) -> Result<()> {
    if !state.session_id.is_empty() {
        write_atomic(&session_path(&state.session_id), state)?;
    }
    // current.json is only a convenience for auto-discovery; the per-session
    // file is authoritative and lets several servers coexist.
    write_atomic(&crate::env::state_file(), state)
}

pub fn read_current() -> Option<State> {
    read_from(&crate::env::state_file())
}

pub fn read_session(session_id: &str) -> Option<State> {
    read_from(&session_path(session_id))
}

pub fn read_from(path: &Path) -> Option<State> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn list_sessions() -> Vec<State> {
    let mut states: Vec<State> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(sessions_dir()) {
        for entry in entries.flatten() {
            if let Some(state) = read_from(&entry.path()) {
                states.push(state);
            }
        }
    }
    if let Some(current) = read_current() {
        if !states.iter().any(|s| {
            (!s.session_id.is_empty() && s.session_id == current.session_id)
                || (s.display == current.display && s.server_pid == current.server_pid)
        }) {
            states.push(current);
        }
    }
    states.sort_by_key(|state| std::cmp::Reverse(state.started_at));
    states
}

/// File the viewer writes to ask the owning server to stop a session.  The
/// viewer has no RPC channel to the server, so this is the only request it
/// makes that changes server state.
pub fn stop_request_path(session_id: &str) -> PathBuf {
    sessions_dir().join(format!("{session_id}.stop"))
}

pub fn request_stop(session_id: &str) -> Result<()> {
    if session_id.is_empty() {
        anyhow::bail!("cannot stop a session without an id");
    }
    let path = stop_request_path(session_id);
    if let Some(parent) = path.parent() {
        crate::env::create_private_dir(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&path, b"stop\n").with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Consume a pending stop request.  Returns true once; the file is removed so
/// a restart cannot replay it.
pub fn take_stop_request(session_id: &str) -> bool {
    let path = stop_request_path(session_id);
    if !path.exists() {
        return false;
    }
    let _ = std::fs::remove_file(&path);
    true
}

/// Remove a session's state file.  If current.json pointed at it, point it at
/// the newest remaining live session instead of leaving a dangling display.
pub fn clear(session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    let _ = std::fs::remove_file(session_path(session_id));
    let _ = std::fs::remove_file(stop_request_path(session_id));
    crate::control::clear(session_id);
    let current_is_ours = read_current()
        .map(|state| state.session_id == session_id)
        .unwrap_or(false);
    if !current_is_ours {
        return;
    }
    // list_sessions() includes current.json itself, so the session being
    // cleared must be excluded or it is written straight back and the stale
    // file outlives the display.
    let replacement = list_sessions()
        .into_iter()
        .find(|state| state.session_id != session_id && crate::proc::alive(state.server_pid));
    match replacement {
        Some(state) => {
            let _ = write_atomic(&crate::env::state_file(), &state);
        }
        None => {
            let _ = std::fs::remove_file(crate::env::state_file());
        }
    }
}

fn cmdline(pid: u32) -> String {
    let path = format!("/proc/{pid}/cmdline");
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .split('\0')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" "),
        Err(_) => String::new(),
    }
}

fn environ(pid: u32) -> String {
    let path = format!("/proc/{pid}/environ");
    match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).replace('\0', "\n"),
        Err(_) => String::new(),
    }
}

fn process_is_nested_display(pid: u32, state: &State) -> bool {
    if pid == std::process::id() {
        return false;
    }
    let line = cmdline(pid);
    if line.is_empty() {
        return false;
    }
    // The X server's command line names the display; every other process in
    // the session carries the unique private runtime directory in its env.
    ((line.contains("Xephyr") || line.contains("Xvfb")) && line.contains(&state.display))
        || environ(pid).contains(&state.runtime_dir)
}

/// PIDs whose environment contains `needle` (used with the per-session runtime
/// directory to find processes that escaped their process group).
pub fn pids_with_env(needle: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if environ(pid).contains(needle) {
            out.push(pid);
        }
    }
    out
}

/// Terminate anything still running with this private runtime directory.
pub fn kill_runtime_dir(runtime_dir: &Path) {
    let needle = runtime_dir.display().to_string();
    let pids = pids_with_env(&needle);
    for pid in &pids {
        crate::proc::kill_group(*pid, libc::SIGTERM);
        crate::proc::signal_process(*pid, libc::SIGTERM);
    }
    if !pids.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(180));
    }
    for pid in pids_with_env(&needle) {
        crate::proc::kill_group(pid, libc::SIGKILL);
        crate::proc::signal_process(pid, libc::SIGKILL);
    }
}

/// Tear down everything a session left behind: the processes named by its
/// state, anything else still carrying its private runtime directory, that
/// directory itself, its control claim, and the state file.  The caller must
/// have established that the owning server is gone (or be shutting it down).
pub fn reap_session(state: &State) -> Vec<String> {
    let mut notes = Vec::new();
    let mut groups = Vec::new();
    for pid in &state.pids {
        if process_is_nested_display(*pid, state) {
            groups.push(*pid);
            notes.push(format!("stopped stale {} (pid {pid})", state.display));
        }
    }
    if !groups.is_empty() {
        for pid in &groups {
            crate::proc::kill_group(*pid, libc::SIGTERM);
            crate::proc::signal_process(*pid, libc::SIGTERM);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        for pid in &groups {
            crate::proc::kill_group(*pid, libc::SIGKILL);
            crate::proc::signal_process(*pid, libc::SIGKILL);
        }
    }
    // Catch children that escaped the original process groups.
    if !state.runtime_dir.is_empty() {
        kill_runtime_dir(Path::new(&state.runtime_dir));
        let _ = std::fs::remove_dir_all(&state.runtime_dir);
    }
    clear(&state.session_id);
    notes
}

/// Kill leftovers from a previous server that died without running its
/// cleanup.  Only touches processes whose command line still names the old
/// nested display, so PID reuse cannot hit an unrelated process.
pub fn cleanup_stale() -> Vec<String> {
    let mut notes = Vec::new();
    let mut seen_displays = Vec::new();
    for state in list_sessions() {
        if crate::proc::alive(state.server_pid) {
            continue;
        }
        if !state.display.is_empty() && seen_displays.contains(&state.display) {
            // A live session already owns this display.  Its PIDs may now
            // belong to the new owner, so only shed this dead server's
            // private tree instead of signalling anything.
            if !state.runtime_dir.is_empty() {
                let _ = std::fs::remove_dir_all(&state.runtime_dir);
            }
            clear(&state.session_id);
            continue;
        }
        if !state.display.is_empty() {
            seen_displays.push(state.display.clone());
        }
        notes.extend(reap_session(&state));
    }
    notes.extend(sweep_orphan_requests());
    notes
}

/// Drop stop/control files whose session is gone.  A crashed server or viewer
/// leaves these behind, and nothing else ever reads them.
fn sweep_orphan_requests() -> Vec<String> {
    let mut notes = Vec::new();
    let Ok(entries) = std::fs::read_dir(sessions_dir()) else {
        return notes;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let session_id = name
            .strip_suffix(".control.json")
            .or_else(|| name.strip_suffix(".stop"));
        let Some(session_id) = session_id else {
            continue;
        };
        if session_id.is_empty() || session_path(session_id).exists() {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            notes.push(format!("removed orphan {name}"));
        }
    }
    notes
}

pub fn session_dir(number: u32, server_pid: u32) -> PathBuf {
    crate::env::shared_runtime_dir().join(format!("session-{number}-{server_pid}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_state_files_get_backend_defaults() {
        let value = serde_json::json!({
            "server_pid": 1,
            "display": ":100",
            "display_number": 100,
            "width": 800,
            "height": 600,
            "visible": false,
            "xserver": "Xvfb",
            "wm": "openbox",
            "runtime_dir": "/run/user/1000/xephyr-mcp/session-100-1",
            "pids": [1, 2],
            "started_at": 1,
            "version": "0.1.0"
        });
        let state: State = serde_json::from_value(value).unwrap();
        assert_eq!(state.backend, "x11");
        assert_eq!(state.control, "agent");
        assert!(state.wayland_display.is_none());
        assert!(state.capabilities.is_none());
    }

    #[test]
    fn stop_request_round_trips_once() {
        let id = format!("test-stop-{}", std::process::id());
        assert!(!take_stop_request(&id));
        request_stop(&id).unwrap();
        assert!(stop_request_path(&id).exists());
        assert!(take_stop_request(&id));
        assert!(!take_stop_request(&id));
    }

    #[test]
    fn stop_request_rejects_an_empty_id() {
        assert!(request_stop("").is_err());
    }

    #[test]
    fn wayland_state_round_trips_capabilities() {
        let mut state: State = serde_json::from_value(serde_json::json!({
            "server_pid": 1,
            "display": "wayland-1",
            "display_number": 0,
            "width": 800,
            "height": 600,
            "visible": false,
            "xserver": "sway",
            "wm": null,
            "runtime_dir": "/run/user/1000/xephyr-mcp/session-1-1",
            "pids": [1],
            "started_at": 1,
            "version": "0.1.0"
        }))
        .unwrap();
        state.backend = "wayland".into();
        state.wayland_display = Some("wayland-1".into());
        state.swaysock = Some("/run/user/1000/xephyr-mcp/session-1-1/sway-ipc.1000.1.sock".into());
        state.output_name = Some("HEADLESS-1".into());
        state.capabilities = Some(crate::backend::Capabilities::wayland());
        let encoded = serde_json::to_string(&state).unwrap();
        let decoded: State = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.backend, "wayland");
        assert_eq!(
            decoded.capabilities.unwrap().clipboard,
            crate::backend::ClipboardPath::DataControl
        );
    }
}
