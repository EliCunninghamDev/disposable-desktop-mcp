//! MCP server built on the official `rmcp` SDK.
//!
//! The transport, handshake, JSON-RPC framing, cancellation, and protocol
//! version negotiation are handled by `rmcp`.  Tool definitions and dispatch
//! still live in [`crate::tools`]; this module only adapts between the JSON
//! values used there and rmcp's typed model.

use crate::tools;
use crate::DisplayManager;
use anyhow::Result;
use rmcp::{
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer},
    transport::stdio,
    ErrorData as McpError, ServerHandler,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// Agent-facing guidance returned in the MCP `initialize` response.
pub const INSTRUCTIONS: &str = "Use xephyr-mcp for GUI and desktop testing on a private, disposable X desktop: screenshots, mouse/keyboard input, window management, and accessibility-driven UI automation. Nothing here touches the host desktop, host cursor, or host session bus.\n\nTypical flow: display_start -> launch -> screenshot/list_windows -> click/type/key -> display_stop. Call display_start first; if a tool says no display is running, start one. Use visible=false for headless Xvfb or visible=true for a watchable Xephyr window. Screenshots may be downscaled: use the returned scale to convert image coordinates back to display pixels for input. Prefer a11y_find/a11y_click over pixels for forms, buttons, and menus.\n\nAfter an action that changes the UI, call wait_for_idle before the next screenshot or click instead of polling. Use wait_for to block on a window title/class, a process exit, a screen-stable state, a changed region, or a file. Use screenshot region/window_id for legible native-resolution crops, clipboard_set/clipboard_get for long or Unicode text, and list_processes/tail_log when an app fails to start. Call diagnose first when an environmental tool is missing. For repeated clicks use count; hold ctrl/shift/alt with modifiers; use mouse_down/mouse_up and key_down/key_up for state that spans calls. window_state changes minimize/maximize/fullscreen/always-on-top; resize_display works on Xephyr. screenshot marks=true numbers actionable elements and click(mark=N) activates them through accessibility when possible. Every call is traced; use trace_tail to review what happened. display_start persist_home=true keeps app profiles, and XEPHYR_MCP_IDLE_TIMEOUT_SECS enables auto-teardown.\n\nEach server process owns one display and can run several sessions via session_id. A host-side viewer can watch them: 'xephyr-mcp-viewer --session ID', or '--layout bsp' to tile every live session. Call display_stop when finished; it reaps every launched process group, the window manager, the X server, and the private D-Bus/AT-SPI buses.";

pub enum Content {
    Text(String),
    Image { data: String, mime_type: String },
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
            content: vec![Content::Text(message.into())],
            is_error: true,
        }
    }

    /// First text block, used for trace error messages.
    pub fn summary(&self) -> Option<&str> {
        self.content.iter().find_map(|item| match item {
            Content::Text(text) => Some(text.as_str()),
            Content::Image { .. } => None,
        })
    }

    /// Convert to rmcp's typed tool result.  Tool-level failures stay in the
    /// result (`isError`) so the caller sees the message, matching MCP's
    /// guidance for errors raised while executing a valid tool call.
    pub fn into_mcp(self) -> CallToolResult {
        let content: Vec<ContentBlock> = self
            .content
            .into_iter()
            .map(|item| match item {
                Content::Text(text) => ContentBlock::text(text),
                Content::Image { data, mime_type } => ContentBlock::image(data, mime_type),
            })
            .collect();
        if self.is_error {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        }
    }
}

#[derive(Clone)]
pub struct XephyrServer {
    manager: Arc<DisplayManager>,
}

impl XephyrServer {
    pub fn new(manager: Arc<DisplayManager>) -> Self {
        Self { manager }
    }
}

impl ServerHandler for XephyrServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("xephyr-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(
            tools::definitions()
                .into_iter()
                .map(tool_from_json)
                .collect(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        Ok(tools::call(&self.manager, &request.name, &args)
            .await
            .into_mcp()
            .into())
    }
}

/// Adapt the JSON tool definitions in `tools.rs` to rmcp's typed `Tool`.
fn tool_from_json(value: Value) -> Tool {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let description = value
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let schema = value
        .get("inputSchema")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    Tool::new(name, description, Arc::new(schema))
}

pub async fn serve(manager: Arc<DisplayManager>) -> Result<()> {
    eprintln!("[xephyr-mcp] server ready (v{})", env!("CARGO_PKG_VERSION"));
    let watchdog = tokio::spawn(watchdog(manager.clone()));
    let running = rmcp::serve_server(XephyrServer::new(manager), stdio())
        .await
        .map_err(|err| anyhow::anyhow!("MCP initialization failed: {err}"))?;
    let outcome = running.waiting().await;
    watchdog.abort();
    outcome.map_err(|err| anyhow::anyhow!("MCP server task failed: {err}"))?;
    eprintln!("[xephyr-mcp] stdin closed, shutting down");
    Ok(())
}

/// Stop a desktop left running by a dead or hung client.  Stdio EOF covers the
/// normal case; this covers a client that stays connected but never sends
/// display_stop, or a session that exceeds its configured lifetime.
async fn watchdog(manager: Arc<DisplayManager>) {
    if manager.idle_timeout.is_none() && manager.max_lifetime.is_none() {
        return;
    }
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        let Some(session) = manager.session().await else {
            continue;
        };
        let running_for = std::time::Duration::from_secs(
            crate::state::now().saturating_sub(session.info.started_at),
        );
        if let Some(reason) = manager.should_auto_stop(running_for) {
            eprintln!(
                "[xephyr-mcp] watchdog: stopping {} ({reason})",
                session.info.display
            );
            let path = crate::trace::path_for(Some(&session.info.session_id));
            let entry = crate::trace::entry(
                "watchdog",
                &json!({ "reason": reason }),
                Some(&session.info.session_id),
                Some(&session.info.display),
                0,
                true,
                None,
                None,
                None,
            );
            let _ = crate::trace::append(&path, entry);
            if let Err(err) = manager.stop().await {
                eprintln!("[xephyr-mcp] watchdog stop failed: {err:#}");
            }
        }
    }
}
