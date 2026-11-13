//! Optional AT-SPI semantic access through a small embedded Python helper.
//!
//! Python + PyGObject are used because the AT-SPI typelib is already present on
//! most desktops; this keeps the Rust server free of a large accessibility
//! binding.  The helper talks to the private accessibility bus created with the
//! nested display.

use crate::display::Session;
use crate::env::Tool;
use crate::input;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const HELPER: &str = include_str!("../scripts/a11y.py");
const TIMEOUT: Duration = Duration::from_secs(25);

async fn run_helper(session: &Session, python: &Tool, args: &[String]) -> Result<Value> {
    let mut cmd = Command::new(&python.path);
    cmd.arg("-");
    cmd.args(args);
    cmd.env_clear();
    for (key, value) in session.nested_env() {
        cmd.env(key, value);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().context("cannot run python3 for AT-SPI")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(HELPER.as_bytes())
            .await
            .context("cannot send AT-SPI helper script")?;
        stdin.shutdown().await.ok();
    }

    let output = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("AT-SPI helper timed out after {TIMEOUT:?}"))?
        .context("AT-SPI helper failed")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim()).map_err(|_| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("no output");
        anyhow!("AT-SPI helper returned no JSON ({detail})")
    })?;

    if parsed.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(anyhow!(
            "{}",
            parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("AT-SPI helper failed")
        ));
    }
    Ok(parsed)
}

pub async fn tree(
    session: &Session,
    python: Option<&Tool>,
    max_depth: u32,
    max_nodes: u32,
    app: Option<&str>,
) -> Result<Value> {
    let python =
        python.ok_or_else(|| anyhow!("python3 with PyGObject is required for a11y_tree"))?;
    let mut args = vec![
        "tree".to_string(),
        "--max-depth".to_string(),
        max_depth.clamp(1, 64).to_string(),
        "--max-nodes".to_string(),
        max_nodes.clamp(1, 4000).to_string(),
    ];
    if let Some(app) = app {
        args.push("--app".into());
        args.push(app.to_string());
    }
    let mut value = run_helper(session, python, &args).await?;
    value["display"] = json!(session.info.display);
    if !session.info.a11y_ready {
        value["warning"] = json!(
            "AT-SPI was not confirmed enabled for this display; if no applications appear, \
             restart the display with a working at-spi2-registryd"
        );
    }
    Ok(value)
}

pub async fn click(
    session: &Session,
    python: Option<&Tool>,
    xdotool: Option<&Tool>,
    name: &str,
    role: Option<&str>,
    index: u32,
) -> Result<Value> {
    let python =
        python.ok_or_else(|| anyhow!("python3 with PyGObject is required for a11y_click"))?;
    let mut args = vec![
        "click".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--index".to_string(),
        index.to_string(),
    ];
    if let Some(role) = role {
        args.push("--role".into());
        args.push(role.to_string());
    }

    let mut value = run_helper(session, python, &args).await?;
    let found = value.get("found").and_then(Value::as_bool).unwrap_or(false);
    let did_action = value
        .get("did_action")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let precise = value
        .get("node")
        .and_then(|node| node.get("geometry_precise"))
        .and_then(Value::as_bool)
        .unwrap_or(true);

    if found && !did_action && !precise {
        value["note"] = json!(
            "This toolkit (GTK4) reports sizes but not positions for child widgets, so a              coordinate click would be unreliable. Use an accessible with an action, or click              coordinates taken from a screenshot."
        );
    }

    if found && !did_action && precise {
        // Fall back to a real XTEST click at the accessible's center.  This is
        // what makes text labels, list rows and other non-actionable nodes
        // still usable.
        let node = value.get("node").cloned().unwrap_or_default();
        let x = node.get("x").and_then(Value::as_i64);
        let y = node.get("y").and_then(Value::as_i64);
        let w = node.get("width").and_then(Value::as_i64);
        let h = node.get("height").and_then(Value::as_i64);
        if let (Some(x), Some(y), Some(w), Some(h)) = (x, y, w, h) {
            if w > 0 && h > 0 {
                let tool = xdotool.ok_or_else(|| {
                    anyhow!("xdotool is required to click an accessible without an action")
                })?;
                let cx = (x + w / 2) as i32;
                let cy = (y + h / 2) as i32;
                input::click(session, tool, cx, cy, 1, 1).await?;
                value["did_action"] = json!(true);
                value["clicked_at"] = json!({"x": cx, "y": cy});
                value["method"] = json!("xtest");
            }
        }
    }

    value["display"] = json!(session.info.display);
    Ok(value)
}

#[allow(clippy::too_many_arguments)]
pub async fn find(
    session: &Session,
    python: Option<&Tool>,
    name: Option<&str>,
    role: Option<&str>,
    exact: bool,
    limit: u32,
    max_depth: u32,
    max_nodes: u32,
) -> Result<Value> {
    let python =
        python.ok_or_else(|| anyhow!("python3 with PyGObject is required for a11y_find"))?;
    let mut args = vec![
        "find".to_string(),
        "--max-depth".to_string(),
        max_depth.clamp(1, 64).to_string(),
        "--max-nodes".to_string(),
        max_nodes.clamp(1, 4000).to_string(),
        "--limit".to_string(),
        limit.clamp(1, 100).to_string(),
    ];
    if let Some(name) = name {
        args.push("--name".into());
        args.push(name.to_string());
    }
    if let Some(role) = role {
        args.push("--role".into());
        args.push(role.to_string());
    }
    if exact {
        args.push("--exact".into());
    }
    let mut value = run_helper(session, python, &args).await?;
    value["display"] = json!(session.info.display);
    if let Some(warning) = a11y_warning(session) {
        value["warning"] = warning;
    }
    Ok(value)
}

pub async fn set_text(
    session: &Session,
    python: Option<&Tool>,
    name: &str,
    role: Option<&str>,
    index: u32,
    text: &str,
) -> Result<Value> {
    let python =
        python.ok_or_else(|| anyhow!("python3 with PyGObject is required for a11y_set_text"))?;
    let mut args = vec![
        "set_text".to_string(),
        "--name".to_string(),
        name.to_string(),
        "--index".to_string(),
        index.to_string(),
        format!("--text={text}"),
    ];
    if let Some(role) = role {
        args.push("--role".into());
        args.push(role.to_string());
    }
    let mut value = run_helper(session, python, &args).await?;
    value["display"] = json!(session.info.display);
    if let Some(warning) = a11y_warning(session) {
        value["warning"] = warning;
    }
    Ok(value)
}

pub async fn focus(
    session: &Session,
    python: Option<&Tool>,
    name: Option<&str>,
    role: Option<&str>,
    index: u32,
) -> Result<Value> {
    let python =
        python.ok_or_else(|| anyhow!("python3 with PyGObject is required for a11y_focus"))?;
    let mut args = vec![
        "focus".to_string(),
        "--index".to_string(),
        index.to_string(),
    ];
    if let Some(name) = name {
        args.push("--name".into());
        args.push(name.to_string());
    }
    if let Some(role) = role {
        args.push("--role".into());
        args.push(role.to_string());
    }
    let mut value = run_helper(session, python, &args).await?;
    value["display"] = json!(session.info.display);
    if let Some(warning) = a11y_warning(session) {
        value["warning"] = warning;
    }
    Ok(value)
}

fn a11y_warning(session: &Session) -> Option<Value> {
    if session.info.a11y_ready {
        None
    } else {
        Some(json!(
            "AT-SPI was not confirmed enabled for this display; if no applications appear, \
             restart the display with a working at-spi2-registryd"
        ))
    }
}
