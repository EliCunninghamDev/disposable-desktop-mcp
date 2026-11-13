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

/// Read only the last `max_bytes` of a log, then keep the last `max_lines`.
/// Bounded so a large or growing log cannot exhaust memory.
pub fn tail_bounded(path: &std::path::Path, max_lines: usize, max_bytes: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::new();
    file.take(max_bytes).read_to_end(&mut buffer)?;
    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    if start > 0 {
        // Drop the partial first line.
        if let Some(index) = text.find('\n') {
            text.drain(..=index);
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    Ok(lines[start..].join("\n"))
}

#[derive(Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub comm: String,
    pub cmdline: String,
    pub started_ticks: u64,
}

/// Enumerate processes whose environment still points at this session's
/// private runtime directory.  That directory is unique per session, so it is
/// a reliable membership test even for children that forked away from the
/// original process group.
pub fn list_session_processes(runtime_dir: &std::path::Path) -> Vec<ProcessInfo> {
    let needle = runtime_dir.display().to_string();
    let needle = needle.as_bytes();
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if !process_env_contains(pid, needle) {
            continue;
        }
        let Some((ppid, pgid, started_ticks)) = stat_fields(pid) else {
            continue;
        };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
        let cmdline = read_cmdline(pid);
        out.push(ProcessInfo {
            pid,
            ppid,
            pgid,
            comm,
            cmdline,
            started_ticks,
        });
    }
    out.sort_by_key(|process| process.pid);
    out
}

fn process_env_contains(pid: u32, needle: &[u8]) -> bool {
    let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    environ.windows(needle.len()).any(|window| window == needle)
}

fn stat_fields(pid: u32) -> Option<(u32, u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    Some((
        fields.get(1)?.parse().ok()?,
        fields.get(2)?.parse().ok()?,
        fields.get(19)?.parse().ok()?,
    ))
}

fn read_cmdline(pid: u32) -> String {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return String::new();
    };
    let mut parts: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    // Quote arguments containing spaces so the joined command stays readable.
    for part in &mut parts {
        if part.contains(' ') && !part.starts_with('"') {
            *part = format!("\"{part}\"");
        }
    }
    parts.join(" ")
}

pub fn process_belongs_to(pid: u32, runtime_dir: &std::path::Path) -> bool {
    if pid <= 1 || pid == std::process::id() {
        return false;
    }
    let needle = runtime_dir.display().to_string();
    process_env_contains(pid, needle.as_bytes())
}

/// Signal a process owned by this session.  `group` targets the process group
/// when the process is its leader, which is how `launch` starts commands.
pub fn kill_session_process(
    pid: u32,
    runtime_dir: &std::path::Path,
    signal: i32,
    group: bool,
) -> Result<()> {
    if pid <= 1 {
        return Err(anyhow!("refusing to signal pid {pid}"));
    }
    if pid == std::process::id() {
        return Err(anyhow!("refusing to signal the MCP server itself"));
    }
    if !process_belongs_to(pid, runtime_dir) {
        return Err(anyhow!(
            "pid {pid} is not part of this session (its environment does not reference {})",
            runtime_dir.display()
        ));
    }
    let pgid = stat_fields(pid).map(|fields| fields.1).unwrap_or(0);
    if group && pgid == pid {
        if !kill_group(pid, signal) {
            return Err(anyhow!("cannot signal process group {pid}"));
        }
    } else if !signal_process(pid, signal) {
        return Err(anyhow!("cannot signal pid {pid}"));
    }
    Ok(())
}
