//! Child-process helpers: new process groups, group shutdown, and bounded
//! command execution.

use anyhow::{anyhow, Context, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Start the child as a session/process-group leader so the whole subtree can
/// be signalled later.  The returned PID is also the process-group ID.
pub fn detach(cmd: &mut Command) {
    // SAFETY: setsid() is async-signal-safe and only runs in the forked child
    // between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.kill_on_drop(false);
}

pub fn kill_group(pid: u32, signal: i32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // Negative PID addresses the process group whose ID equals `pid`.
    unsafe { libc::kill(-(pid as i32), signal) == 0 }
}

pub fn signal_process(pid: u32, signal: i32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    unsafe { libc::kill(pid as i32, signal) == 0 }
}

pub fn alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Politely terminate every process group, then force what is left.
pub async fn terminate_groups(pids: &[u32]) {
    let mut groups: Vec<u32> = pids.iter().copied().filter(|p| *p > 0).collect();
    groups.sort_unstable();
    groups.dedup();
    if groups.is_empty() {
        return;
    }

    for pid in &groups {
        kill_group(*pid, libc::SIGTERM);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
    loop {
        let remaining: Vec<u32> = groups.iter().copied().filter(|p| alive(*p)).collect();
        if remaining.is_empty() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    for pid in groups {
        kill_group(pid, libc::SIGKILL);
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
}

pub struct CommandOutput {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn ok(&self) -> bool {
        self.status.success()
    }
}

/// Run a command with a deadline and collect its output.
pub async fn run_output(mut cmd: Command, timeout: Duration) -> Result<CommandOutput> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    detach(&mut cmd);

    let child = cmd.spawn().context("failed to spawn command")?;
    let pid = child.id().unwrap_or(0);

    let waited = tokio::time::timeout(timeout, child.wait_with_output()).await;
    match waited {
        Ok(Ok(output)) => Ok(CommandOutput {
            status: output.status,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        Ok(Err(err)) => Err(anyhow!("failed to wait for command: {err}")),
        Err(_) => {
            kill_group(pid, libc::SIGTERM);
            tokio::time::sleep(Duration::from_millis(100)).await;
            kill_group(pid, libc::SIGKILL);
            Err(anyhow!("command timed out after {timeout:?}"))
        }
    }
}

/// Read the tail of a log file for error reporting.
pub fn tail(path: &std::path::Path, max_lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}
