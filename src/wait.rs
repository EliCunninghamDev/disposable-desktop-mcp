//! Blocking UI conditions: window appearance, process exit, frame stability,
//! and region changes.  These replace screenshot polling in agent loops.

use crate::backend::{Backend, CaptureRect, CaptureTarget};
use crate::display::Session;
use crate::proc;
use crate::screenshot::{self, Frame};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub enum Condition {
    Window {
        title: Option<String>,
        class: Option<String>,
        appear: bool,
    },
    ProcessExit {
        pid: u32,
    },
    ScreenStable {
        region: Option<Region>,
        stable_ms: u64,
    },
    RegionChange {
        region: Region,
    },
    FileExists {
        path: PathBuf,
        present: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug)]
pub struct WaitSpec {
    pub condition: Condition,
    pub timeout: Duration,
    pub poll: Duration,
}

pub async fn run(session: &Session, spec: &WaitSpec) -> Result<Value> {
    let backend = session.backend();
    let started = Instant::now();
    let deadline = started + spec.timeout;
    let poll = spec.poll.max(Duration::from_millis(20));
    let mut polls = 0u64;
    let mut last_error: Option<String> = None;

    // Baseline frame/hash for the stability conditions.
    let mut baseline: Option<u64> = None;
    let mut stable_since: Option<Instant> = None;
    let mut last_hash: Option<u64> = None;

    loop {
        polls += 1;
        let elapsed = started.elapsed();
        match &spec.condition {
            Condition::Window {
                title,
                class,
                appear,
            } => match backend.windows() {
                Ok(list) => {
                    let matches: Vec<Value> = list
                        .iter()
                        .filter(|window| window.mapped)
                        .filter(|window| {
                            let title_match = title.as_ref().is_none_or(|wanted| {
                                window.title.to_lowercase().contains(&wanted.to_lowercase())
                            });
                            let class_match = class.as_ref().is_none_or(|wanted| {
                                let class = window.class.as_deref().unwrap_or("");
                                let instance = window.instance.as_deref().unwrap_or("");
                                class.to_lowercase().contains(&wanted.to_lowercase())
                                    || instance.to_lowercase().contains(&wanted.to_lowercase())
                            });
                            title_match && class_match
                        })
                        .map(|window| window.to_json())
                        .collect();
                    let present = !matches.is_empty();
                    if present == *appear {
                        return Ok(json!({
                            "condition": "window",
                            "satisfied": true,
                            "timed_out": false,
                            "waited_ms": elapsed.as_millis() as u64,
                            "polls": polls,
                            "matched": present,
                            "count": matches.len(),
                            "windows": matches,
                        }));
                    }
                }
                Err(err) => last_error = Some(format!("{err:#}")),
            },
            Condition::ProcessExit { pid } => {
                if process_exited(*pid) {
                    return Ok(json!({
                        "condition": "process_exit",
                        "satisfied": true,
                        "timed_out": false,
                        "waited_ms": elapsed.as_millis() as u64,
                        "polls": polls,
                        "pid": pid,
                        "note": "Use tail_log with the launch log to read the final output.",
                    }));
                }
            }
            Condition::ScreenStable { region, stable_ms } => {
                match capture_hash(&backend, *region) {
                    Ok(hash) => {
                        if last_hash != Some(hash) {
                            last_hash = Some(hash);
                            stable_since = Some(Instant::now());
                        }
                        let stable_for = stable_since
                            .map(|since| since.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        if stable_for >= *stable_ms {
                            return Ok(json!({
                                "condition": "screen_stable",
                                "satisfied": true,
                                "timed_out": false,
                                "waited_ms": elapsed.as_millis() as u64,
                                "polls": polls,
                                "stable_ms": stable_for,
                                "hash": format!("{hash:016x}"),
                                "region": region_json(*region),
                            }));
                        }
                    }
                    Err(err) => last_error = Some(format!("{err:#}")),
                }
            }
            Condition::RegionChange { region } => match capture_hash(&backend, Some(*region)) {
                Ok(hash) => {
                    if baseline.is_none() {
                        baseline = Some(hash);
                    } else if baseline != Some(hash) {
                        return Ok(json!({
                            "condition": "region_change",
                            "satisfied": true,
                            "timed_out": false,
                            "waited_ms": elapsed.as_millis() as u64,
                            "polls": polls,
                            "before_hash": format!("{:016x}", baseline.unwrap_or_default()),
                            "after_hash": format!("{hash:016x}"),
                            "region": region_json(Some(*region)),
                        }));
                    }
                }
                Err(err) => last_error = Some(format!("{err:#}")),
            },
            Condition::FileExists { path, present } => {
                let exists = path.exists();
                if exists == *present {
                    return Ok(json!({
                        "condition": "file_exists",
                        "satisfied": true,
                        "timed_out": false,
                        "waited_ms": elapsed.as_millis() as u64,
                        "polls": polls,
                        "path": path.display().to_string(),
                        "exists": exists,
                    }));
                }
            }
        }

        if Instant::now() >= deadline {
            return Ok(json!({
                "condition": condition_name(&spec.condition),
                "satisfied": false,
                "timed_out": true,
                "waited_ms": elapsed.as_millis() as u64,
                "polls": polls,
                "last_error": last_error,
                "hint": "The condition was not met before timeout_ms. Take a screenshot and inspect the current state, or increase timeout_ms.",
            }));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(poll.min(remaining).max(Duration::from_millis(1))).await;
    }
}

fn condition_name(condition: &Condition) -> &'static str {
    match condition {
        Condition::Window { .. } => "window",
        Condition::ProcessExit { .. } => "process_exit",
        Condition::ScreenStable { .. } => "screen_stable",
        Condition::RegionChange { .. } => "region_change",
        Condition::FileExists { .. } => "file_exists",
    }
}

fn region_json(region: Option<Region>) -> Value {
    match region {
        Some(region) => json!({
            "x": region.x,
            "y": region.y,
            "width": region.width,
            "height": region.height,
        }),
        None => Value::Null,
    }
}

fn capture_hash(backend: &Backend, region: Option<Region>) -> Result<u64> {
    let target = region
        .map(|region| {
            CaptureTarget::Region(CaptureRect {
                x: i32::from(region.x),
                y: i32::from(region.y),
                width: u32::from(region.width),
                height: u32::from(region.height),
            })
        })
        .unwrap_or(CaptureTarget::Output);
    let frame: Frame = backend.capture(&target, false)?.frame;
    Ok(screenshot::hash(&frame))
}

fn process_exited(pid: u32) -> bool {
    if !proc::alive(pid) {
        return true;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .map(|state| state == "Z")
            .unwrap_or(false),
        Err(_) => true,
    }
}

pub fn parse_region(value: &Value) -> Result<Region> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("region must be an object with x, y, width, height"))?;
    let number = |key: &str| -> Result<u16> {
        object
            .get(key)
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(|| anyhow!("region.{key} must be an integer between 0 and 65535"))
    };
    let region = Region {
        x: number("x")?,
        y: number("y")?,
        width: number("width")?,
        height: number("height")?,
    };
    if region.width == 0 || region.height == 0 {
        return Err(anyhow!(
            "region.width and region.height must be greater than zero"
        ));
    }
    Ok(region)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_region() {
        let region = parse_region(&json!({
            "x": 10, "y": 20, "width": 30, "height": 40
        }))
        .unwrap();
        assert_eq!(region.x, 10);
        assert_eq!(region.height, 40);
    }

    #[test]
    fn rejects_empty_region() {
        let error = parse_region(&json!({
            "x": 0, "y": 0, "width": 0, "height": 10
        }))
        .unwrap_err();
        assert!(error.to_string().contains("greater than zero"));
    }
}
