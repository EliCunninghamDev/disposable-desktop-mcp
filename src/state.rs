//! Session state shared with the viewer, plus conservative stale cleanup.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Root-window property used by the viewer to discover a nested display when
/// no state file is available.
pub const ROOT_MARKER: &str = "_XEPHYR_MCP";

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
    pub pids: Vec<u32>,
    pub started_at: u64,
    pub version: String,
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

/// Remove a session's state file.  If current.json pointed at it, point it at
/// the newest remaining live session instead of leaving a dangling display.
pub fn clear(session_id: &str) {
    let _ = std::fs::remove_file(session_path(session_id));
    let current_is_ours = read_current()
        .map(|state| state.session_id == session_id)
        .unwrap_or(false);
    if !current_is_ours {
        return;
    }
    let replacement = list_sessions()
        .into_iter()
        .find(|state| crate::proc::alive(state.server_pid));
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
        if seen_displays.contains(&state.display) {
            clear(&state.session_id);
            continue;
        }
        seen_displays.push(state.display.clone());

        let mut groups = Vec::new();
        for pid in &state.pids {
            if process_is_nested_display(*pid, &state) {
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
        kill_runtime_dir(Path::new(&state.runtime_dir));
        clear(&state.session_id);
    }
    notes
}

pub fn session_dir(number: u32, server_pid: u32) -> PathBuf {
    crate::env::shared_runtime_dir().join(format!("session-{number}-{server_pid}"))
}
