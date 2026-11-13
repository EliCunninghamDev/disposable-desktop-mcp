//! Pointer and keyboard input via xdotool's XTEST path.
//!
//! XTEST is intentionally used without `--window`: the events are delivered to
//! the nested X server that owns the focused window, never to the host.

use crate::display::Session;
use crate::env::Tool;
use crate::proc;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(20);

fn command(session: &Session, tool: &Tool) -> Command {
    let mut cmd = Command::new(&tool.path);
    cmd.env_clear();
    for (key, value) in session.nested_env() {
        cmd.env(key, value);
    }
    cmd
}

async fn run(session: &Session, tool: &Tool, action: &str, args: &[String]) -> Result<Value> {
    let mut cmd = command(session, tool);
    cmd.args(args);
    let output = proc::run_output(cmd, TIMEOUT).await?;
    if !output.ok() {
        let detail = output
            .stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .or_else(|| {
                output
                    .stdout
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
            })
            .unwrap_or("xdotool failed");
        return Err(anyhow!("{detail}"));
    }
    Ok(json!({
        "ok": true,
        "action": action,
        "display": session.info.display,
        "stdout": output.stdout.trim(),
    }))
}

pub fn button_number(button: &str) -> Result<u32> {
    match button.to_ascii_lowercase().as_str() {
        "" | "left" | "1" => Ok(1),
        "middle" | "2" => Ok(2),
        "right" | "3" => Ok(3),
        "back" | "4" => Ok(4),
        "forward" | "5" => Ok(5),
        other => other
            .parse::<u32>()
            .ok()
            .filter(|number| (1..=9).contains(number))
            .ok_or_else(|| anyhow!("unknown mouse button: {other}")),
    }
}

pub async fn click(
    session: &Session,
    tool: &Tool,
    x: i32,
    y: i32,
    button: u32,
    count: u32,
) -> Result<Value> {
    let mut args = vec![
        "mousemove".to_string(),
        x.to_string(),
        y.to_string(),
        "click".to_string(),
    ];
    if count > 1 {
        args.push("--repeat".into());
        args.push(count.to_string());
    }
    args.push(button.to_string());
    run(session, tool, "click", &args).await
}

pub async fn move_to(session: &Session, tool: &Tool, x: i32, y: i32) -> Result<Value> {
    let args = vec!["mousemove".to_string(), x.to_string(), y.to_string()];
    run(session, tool, "move", &args).await
}

pub async fn drag(
    session: &Session,
    tool: &Tool,
    from: (i32, i32),
    to: (i32, i32),
    button: u32,
    duration_ms: u32,
) -> Result<Value> {
    let steps = 8u32;
    let delay = (duration_ms.max(40) as f64 / steps as f64 / 1000.0).min(0.15);
    let mut args = vec![
        "mousemove".to_string(),
        from.0.to_string(),
        from.1.to_string(),
        "mousedown".to_string(),
        button.to_string(),
    ];
    for step in 1..=steps {
        let x = from.0 + (to.0 - from.0) * step as i32 / steps as i32;
        let y = from.1 + (to.1 - from.1) * step as i32 / steps as i32;
        args.push("mousemove".into());
        args.push(x.to_string());
        args.push(y.to_string());
        args.push("sleep".into());
        args.push(format!("{delay:.3}"));
    }
    args.push("mouseup".into());
    args.push(button.to_string());
    run(session, tool, "drag", &args).await
}

pub async fn scroll(
    session: &Session,
    tool: &Tool,
    at: Option<(i32, i32)>,
    up: bool,
    amount: u32,
) -> Result<Value> {
    let amount = amount.clamp(1, 100);
    let mut args = Vec::new();
    if let Some((x, y)) = at {
        args.push("mousemove".into());
        args.push(x.to_string());
        args.push(y.to_string());
    }
    args.push("click".into());
    args.push("--repeat".into());
    args.push(amount.to_string());
    args.push(if up { "4" } else { "5" }.to_string());
    run(session, tool, "scroll", &args).await
}

pub async fn type_text(session: &Session, tool: &Tool, text: &str, delay_ms: u32) -> Result<Value> {
    if text.is_empty() {
        return Err(anyhow!("text must not be empty"));
    }
    let args = vec![
        "type".to_string(),
        "--delay".to_string(),
        delay_ms.min(1000).to_string(),
        "--clearmodifiers".to_string(),
        "--".to_string(),
        text.to_string(),
    ];
    run(session, tool, "type", &args).await
}

pub async fn key(session: &Session, tool: &Tool, combo: &str, delay_ms: u32) -> Result<Value> {
    if combo.trim().is_empty() {
        return Err(anyhow!("key combo must not be empty"));
    }
    let args = vec![
        "key".to_string(),
        "--clearmodifiers".to_string(),
        "--delay".to_string(),
        delay_ms.min(1000).to_string(),
        combo.to_string(),
    ];
    run(session, tool, "key", &args).await
}
