//! Append-only JSONL trace of tool calls.
//!
//! Every call is recorded with its arguments, duration, outcome, and - for
//! tools that can change the screen - a before/after frame hash.  The file is
//! deliberately outside the session runtime directory so it survives
//! `display_stop` and can be inspected afterwards.

use crate::env;
use crate::state;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn traces_dir() -> PathBuf {
    env::shared_runtime_dir().join("traces")
}

pub fn session_path(session_id: &str) -> PathBuf {
    traces_dir().join(format!("{session_id}.jsonl"))
}

fn server_path() -> PathBuf {
    traces_dir().join(format!("server-{}.jsonl", std::process::id()))
}

/// Resolve the trace file for a call.  Prefer the active session's id so all
/// calls from one desktop land in one file, even before/after start.
pub fn path_for(session_id: Option<&str>) -> PathBuf {
    match session_id {
        Some(id) if !id.is_empty() => session_path(id),
        _ => server_path(),
    }
}

/// Append one trace record.  Tracing must never break a tool call, so all
/// errors are swallowed by callers; this function only reports them.
pub fn append(path: &Path, entry: Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        env::create_private_dir(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    serde_json::to_writer(&mut file, &entry)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

/// Trim arguments before they reach the trace: long strings (typed text,
/// clipboard contents) are truncated and image payloads never appear here.
pub fn sanitize_args(args: &Value) -> Value {
    fn limit(value: &Value, depth: usize) -> Value {
        if depth > 4 {
            return json!("<deep>");
        }
        match value {
            Value::String(text) => {
                if text.chars().count() > 512 {
                    let head: String = text.chars().take(512).collect();
                    json!(format!("{head}… ({} chars)", text.chars().count()))
                } else {
                    Value::String(text.clone())
                }
            }
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| limit(item, depth + 1)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(key, _)| !matches!(key.as_str(), "data" | "image" | "png"))
                    .map(|(key, value)| (key.clone(), limit(value, depth + 1)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    limit(args, 0)
}

/// Build the standard trace entry.
#[allow(clippy::too_many_arguments)]
pub fn entry(
    tool: &str,
    args: &Value,
    session_id: Option<&str>,
    display: Option<&str>,
    duration_ms: u128,
    ok: bool,
    error: Option<&str>,
    before_hash: Option<&str>,
    after_hash: Option<&str>,
) -> Value {
    let mut record = json!({
        "ts": state::now(),
        "tool": tool,
        "args": sanitize_args(args),
        "session_id": session_id,
        "display": display,
        "duration_ms": duration_ms,
        "ok": ok,
    });
    if let Some(error) = error {
        record["error"] = json!(error.chars().take(2048).collect::<String>());
    }
    if let Some(hash) = before_hash {
        record["before_hash"] = json!(hash);
    }
    if let Some(hash) = after_hash {
        record["after_hash"] = json!(hash);
        if let Some(before) = before_hash {
            record["changed"] = json!(before != hash);
        }
    }
    record
}

/// Tools whose call can plausibly change the frame.  Only these pay the cost
/// of two extra captures per call.
pub const HASHED_TOOLS: &[&str] = &[
    "click",
    "move",
    "drag",
    "scroll",
    "type",
    "key",
    "mouse_down",
    "mouse_up",
    "key_down",
    "key_up",
    "launch",
    "focus_window",
    "move_resize_window",
    "close_window",
    "window_state",
    "resize_display",
    "a11y_click",
    "a11y_set_text",
    "a11y_focus",
    "clipboard_set",
];

pub fn should_hash(tool: &str) -> bool {
    HASHED_TOOLS.contains(&tool)
}

/// Read a bounded trace tail.  The path is constrained to the trace directory
/// so a caller cannot use this to read arbitrary files.
pub fn read_tail(session_id: Option<&str>, lines: usize) -> Result<(PathBuf, String)> {
    let path = match session_id {
        Some(id) => {
            crate::display::validate_session_id(id)?;
            session_path(id)
        }
        None => newest_trace().ok_or_else(|| anyhow!("no trace files exist yet"))?,
    };
    let canonical_dir = traces_dir().canonicalize().unwrap_or_else(|_| traces_dir());
    let canonical = path
        .canonicalize()
        .with_context(|| format!("trace file {} does not exist", path.display()))?;
    if !canonical.starts_with(&canonical_dir) {
        return Err(anyhow!("trace path is outside the trace directory"));
    }
    let text = crate::proc::tail_bounded(&canonical, lines.clamp(1, 2000), 512 * 1024)?;
    Ok((canonical, text))
}

fn newest_trace() -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(traces_dir()).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        if best
            .as_ref()
            .map(|(time, _)| modified > *time)
            .unwrap_or(true)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_truncates_long_strings() {
        let text = "x".repeat(900);
        let value = sanitize_args(&json!({"text": text, "n": 3}));
        let kept = value["text"].as_str().unwrap();
        assert!(kept.contains("900 chars"));
        assert!(kept.chars().count() < 600);
        assert_eq!(value["n"], json!(3));
    }

    #[test]
    fn sanitize_drops_payload_fields() {
        let value = sanitize_args(&json!({"data": "AAAA", "image": {"png": "x"}, "x": 1}));
        assert!(value.get("data").is_none());
        assert!(value.get("image").is_none());
        assert_eq!(value["x"], json!(1));
    }

    #[test]
    fn hash_tool_list_is_stable() {
        assert!(should_hash("click"));
        assert!(should_hash("type"));
        assert!(!should_hash("screenshot"));
        assert!(!should_hash("diagnose"));
    }
}
