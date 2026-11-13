use anyhow::Result;
use disposable_desktop_mcp::{mcp, DisplayManager};
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};

#[tokio::main]
async fn main() -> Result<()> {
    let manager = Arc::new(DisplayManager::new());
    eprintln!(
        "[disposable-desktop-mcp] tools: {}",
        serde_json::to_string(&manager.tools.describe()).unwrap_or_default()
    );

    let cleanup = manager.clone();
    tokio::spawn(async move {
        let mut term = signal(SignalKind::terminate()).ok();
        let mut hup = signal(SignalKind::hangup()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = async {
                match term.as_mut() {
                    Some(sig) => { sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {},
            _ = async {
                match hup.as_mut() {
                    Some(sig) => { sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {},
        }
        let _ = cleanup.stop().await;
        std::process::exit(0);
    });

    let result = mcp::serve(manager.clone()).await;
    if let Err(err) = manager.stop().await {
        eprintln!("[disposable-desktop-mcp] cleanup failed: {err:#}");
    }
    result
}
