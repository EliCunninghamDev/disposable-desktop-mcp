//! Minimal MCP server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! Implements the surface this server needs: initialize, tools/list,
//! tools/call, ping and notification handling.  All diagnostics go to stderr;
//! stdout is reserved for protocol messages.

use crate::tools;
use crate::DisplayManager;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub enum Content {
    Text(String),
    Image { data: String, mime_type: String },
}

impl Content {
    fn to_json(&self) -> Value {
        match self {
            Content::Text(text) => json!({ "type": "text", "text": text }),
            Content::Image { data, mime_type } => {
                json!({ "type": "image", "data": data, "mimeType": mime_type })
            }
        }
    }
}

pub struct CallResult {
    pub content: Vec<Content>,
    pub is_error: bool,
}

impl CallResult {
    pub fn text(value: &Value) -> Self {
        let text = match value {
            Value::String(text) => text.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        };
        Self {
            content: vec![Content::Text(text)],
            is_error: false,
        }
    }

    pub fn image(png: Vec<u8>, mime_type: &str, metadata: &Value) -> Self {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(png);
        Self {
            content: vec![
                Content::Text(match metadata {
                    Value::String(text) => text.clone(),
                    other => serde_json::to_string_pretty(other).unwrap_or_default(),
                }),
                Content::Image {
                    data,
                    mime_type: mime_type.to_string(),
                },
            ],
            is_error: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text(format!("Error: {}", message.into()))],
            is_error: true,
        }
    }

    pub fn to_json(&self) -> Value {
        let content: Vec<Value> = self.content.iter().map(Content::to_json).collect();
        let mut out = json!({ "content": content });
        if self.is_error {
            out["isError"] = Value::Bool(true);
        }
        out
    }
}

pub async fn serve(manager: Arc<DisplayManager>) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();

    eprintln!("[xephyr-mcp] server ready (v{})", env!("CARGO_PKG_VERSION"));

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(msg) => msg,
            Err(err) => {
                eprintln!("[xephyr-mcp] malformed JSON: {err}");
                continue;
            }
        };
        if let Some(response) = handle(&manager, &msg).await {
            let mut encoded = serde_json::to_string(&response)?;
            encoded.push('\n');
            out.write_all(encoded.as_bytes()).await?;
            out.flush().await?;
        }
    }

    eprintln!("[xephyr-mcp] stdin closed, shutting down");
    Ok(())
}

async fn handle(manager: &DisplayManager, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str)?;

    // Notifications never get a response.
    id.as_ref()?;

    let outcome: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(initialize(msg)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools::definitions() })),
        "tools/call" => call_tool(manager, msg).await,
        "shutdown" => Ok(json!({})),
        "logging/setLevel" => Ok(json!({})),
        other => Err((-32601, format!("method not found: {other}"))),
    };

    let id = id?;
    Some(match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        }),
    })
}

async fn call_tool(manager: &DisplayManager, msg: &Value) -> Result<Value, (i64, String)> {
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((-32602, "tools/call requires a tool name".to_string()))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let args = if args.is_object() { args } else { json!({}) };
    Ok(tools::call(manager, name, &args).await.to_json())
}

fn initialize(msg: &Value) -> Value {
    let requested = msg
        .get("params")
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or("2025-06-18");

    json!({
        "protocolVersion": requested,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "xephyr-mcp",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Drive a disposable nested X desktop. Call display_start (visible=true for Xephyr, visible=false for headless Xvfb), then launch applications, then screenshot/list_windows. Input coordinates are nested-display pixels; screenshots may be downscaled (see the scale field). display_stop kills the X server, window manager, private D-Bus, and every launched process group."
    })
}
