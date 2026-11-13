//! Owns the nested display: X server, window manager, private D-Bus session,
//! accessibility bus, launched applications, and shutdown.

use crate::env::{self, Tool};
use crate::proc;
use crate::state::{self, State};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use x11rb::protocol::xproto::ConnectionExt as _;

#[derive(Clone, Debug)]
pub struct Tools {
    pub runtime: Option<PathBuf>,
    pub xephyr: Option<Tool>,
    pub xvfb: Option<Tool>,
    pub wm: Option<Tool>,
    pub xdotool: Option<Tool>,
    pub python: Option<Tool>,
    pub gdbus: Option<Tool>,
    pub registryd: Option<PathBuf>,
    pub dbus_launch: Option<Tool>,
}

impl Tools {
    pub fn discover() -> Tools {
        let runtime = env::runtime_root();
        let rt = runtime.as_deref();
        let wm = [
            "openbox",
            "fluxbox",
            "jwm",
            "twm",
            "matchbox-window-manager",
        ]
        .iter()
        .find_map(|name| env::find_tool(name, rt));
        let xephyr = env::find_tool("Xephyr", rt);
        let xvfb = env::find_tool("Xvfb", rt);
        let xdotool = env::find_tool("xdotool", rt);
        let python = env::find_tool("python3", rt);
        let gdbus = env::find_tool("gdbus", rt);
        let registryd = env::find_registryd();
        let dbus_launch = env::find_tool("dbus-launch", rt);
        Tools {
            runtime,
            xephyr,
            xvfb,
            wm,
            xdotool,
            python,
            gdbus,
            registryd,
            dbus_launch,
        }
    }

    pub fn wm_name(&self) -> Option<String> {
        self.wm.as_ref().and_then(|tool| {
            tool.path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
    }

    pub fn describe(&self) -> Value {
        json!({
            "xephyr": self.xephyr.as_ref().map(Tool::label),
            "xvfb": self.xvfb.as_ref().map(Tool::label),
            "window_manager": self.wm.as_ref().map(Tool::label),
            "xdotool": self.xdotool.as_ref().map(Tool::label),
            "python3": self.python.as_ref().map(Tool::label),
            "dbus_launch": self.dbus_launch.as_ref().map(Tool::label),
            "at_spi_registryd": self.registryd.as_ref().map(|p| p.display().to_string()),
            "bundled_runtime": self.runtime.as_ref().map(|p| p.display().to_string()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub session_id: String,
    pub number: u32,
    pub display: String,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    pub xserver: String,
    pub wm: Option<String>,
    pub dbus_address: String,
    pub dbus_pid: u32,
    pub dbus_group: u32,
    pub xserver_pid: u32,
    pub wm_pid: Option<u32>,
    pub a11y_registry_pid: Option<u32>,
    pub a11y_ready: bool,
    pub runtime_dir: PathBuf,
    pub xauthority: PathBuf,
    pub log_dir: PathBuf,
    pub home: Option<PathBuf>,
    pub persistent_home: bool,
    pub started_at: u64,
}

impl SessionInfo {
    pub fn pids(&self) -> Vec<u32> {
        let mut pids = vec![self.xserver_pid, self.dbus_pid, self.dbus_group];
        if let Some(pid) = self.wm_pid {
            pids.push(pid);
        }
        if let Some(pid) = self.a11y_registry_pid {
            pids.push(pid);
        }
        pids.retain(|pid| *pid > 0);
        pids
    }

    pub fn to_json(&self) -> Value {
        json!({
            "session_id": self.session_id,
            "display": self.display,
            "display_number": self.number,
            "width": self.width,
            "height": self.height,
            "visible": self.visible,
            "xserver": self.xserver,
            "window_manager": self.wm,
            "dbus_address": self.dbus_address,
            "pids": {
                "xserver": self.xserver_pid,
                "window_manager": self.wm_pid,
                "dbus": self.dbus_pid,
                "dbus_group": self.dbus_group,
                "a11y_registry": self.a11y_registry_pid,
            },
            "accessibility": self.a11y_ready,
            "runtime_dir": self.runtime_dir.display().to_string(),
            "log_dir": self.log_dir.display().to_string(),
            "home": self.home.as_ref().map(|path| path.display().to_string()),
            "persistent_home": self.persistent_home,
            "trace": crate::trace::session_path(&self.session_id).display().to_string(),
            "state_file": env::state_file().display().to_string(),
            "started_at": self.started_at,
        })
    }

    fn state(&self) -> State {
        State {
            server_pid: std::process::id(),
            session_id: self.session_id.clone(),
            display: self.display.clone(),
            display_number: self.number,
            width: self.width,
            height: self.height,
            visible: self.visible,
            xserver: self.xserver.clone(),
            wm: self.wm.clone(),
            runtime_dir: self.runtime_dir.display().to_string(),
            home: self.home.as_ref().map(|path| path.display().to_string()),
            persistent_home: self.persistent_home,
            pids: self.pids(),
            started_at: self.started_at,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Launched {
    pub pid: u32,
    pub command: String,
    pub log: PathBuf,
    pub started_at: Instant,
}

/// Recorded exit status for a launched child.  A background task reaps each
/// child, so the status stays queryable after `launch` has returned.
#[derive(Clone, Debug)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub after_ms: u64,
}

pub struct Session {
    pub info: SessionInfo,
    pub launched: Mutex<Vec<Launched>>,
    /// Current display size.  RandR resizing changes this after start.
    size: Mutex<(u32, u32)>,
    /// Numbered elements from the most recent set-of-marks screenshot.
    marks: Mutex<Vec<crate::a11y::Mark>>,
    /// Exit statuses keyed by launch pid.
    exits: Mutex<std::collections::HashMap<u32, ExitInfo>>,
    launch_seq: AtomicU64,
}

impl Session {
    /// Explicit environment for every child on the nested display.  Only
    /// whitelisted host variables survive, and nothing points at the host
    /// compositor, D-Bus session, or X server.
    pub fn nested_env(&self) -> Vec<(String, String)> {
        let mut entries = env::base_environment();
        let mut push = |key: &str, value: String| entries.push((key.to_string(), value));

        push("DISPLAY", self.info.display.clone());
        push("DBUS_SESSION_BUS_ADDRESS", self.info.dbus_address.clone());
        push(
            "XDG_RUNTIME_DIR",
            self.info.runtime_dir.display().to_string(),
        );
        push("XAUTHORITY", self.info.xauthority.display().to_string());
        // Never let an application fall back to the host compositor.
        push("XDG_SESSION_TYPE", "x11".into());
        push("GDK_BACKEND", "x11".into());
        push("QT_QPA_PLATFORM", "xcb".into());
        push("CLUTTER_BACKEND", "x11".into());
        push("SDL_VIDEODRIVER", "x11".into());
        push("MOZ_ENABLE_WAYLAND", "0".into());
        // Accessibility backends for GTK4 and Qt.
        push("GTK_A11Y", "atspi".into());
        push("QT_LINUX_ACCESSIBILITY_ALWAYS_ON", "1".into());
        push("TERM", "xterm-256color".into());
        if let Some(home) = &self.info.home {
            for (key, path) in [
                ("HOME", home.clone()),
                ("XDG_CONFIG_HOME", home.join(".config")),
                ("XDG_DATA_HOME", home.join(".local/share")),
                ("XDG_STATE_HOME", home.join(".local/state")),
                ("XDG_CACHE_HOME", home.join(".cache")),
            ] {
                let _ = std::fs::create_dir_all(&path);
                push(key, path.display().to_string());
            }
        }
        entries
    }

    pub fn width(&self) -> u32 {
        self.size
            .lock()
            .map(|size| size.0)
            .unwrap_or(self.info.width)
    }

    pub fn height(&self) -> u32 {
        self.size
            .lock()
            .map(|size| size.1)
            .unwrap_or(self.info.height)
    }

    /// Record a RandR resize and refresh the state file the viewer watches.
    pub fn set_size(&self, width: u32, height: u32) -> Result<()> {
        if let Ok(mut size) = self.size.lock() {
            *size = (width, height);
        }
        let mut info = self.info.clone();
        info.width = width;
        info.height = height;
        state::write(&info.state())?;
        let _ = write_root_marker(&info);
        Ok(())
    }

    pub fn set_marks(&self, marks: Vec<crate::a11y::Mark>) {
        if let Ok(mut current) = self.marks.lock() {
            *current = marks;
        }
    }

    pub fn marks(&self) -> Vec<crate::a11y::Mark> {
        self.marks
            .lock()
            .map(|marks| marks.clone())
            .unwrap_or_default()
    }

    pub fn launch_count(&self) -> usize {
        self.launched.lock().map(|v| v.len()).unwrap_or(0)
    }

    pub fn next_launch_seq(&self) -> u64 {
        self.launch_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn record(&self, launched: Launched) {
        if let Ok(mut list) = self.launched.lock() {
            list.push(launched);
        }
    }

    /// Store the status of a reaped child.  Later calls to `launch_status`
    /// read this instead of waiting on the process again.
    pub fn record_exit(&self, pid: u32, code: Option<i32>, signal: Option<i32>, after_ms: u64) {
        if let Ok(mut exits) = self.exits.lock() {
            exits.insert(
                pid,
                ExitInfo {
                    code,
                    signal,
                    after_ms,
                },
            );
        }
    }

    pub fn exit_info(&self, pid: u32) -> Option<ExitInfo> {
        self.exits.lock().ok()?.get(&pid).cloned()
    }

    /// True while a launch is alive and its exit status has not been reaped.
    pub fn running(&self, pid: u32) -> bool {
        self.exit_info(pid).is_none() && crate::proc::alive(pid)
    }

    pub fn launched(&self) -> Vec<Launched> {
        self.launched
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default()
    }
}

pub struct StartReport {
    pub info: SessionInfo,
    pub warnings: Vec<String>,
}

impl StartReport {
    pub fn to_json(&self) -> Value {
        let mut value = self.info.to_json();
        value["warnings"] = json!(self.warnings);
        value
    }
}

pub struct StopReport {
    pub stopped: bool,
    pub display: Option<String>,
    pub processes: Vec<u32>,
    pub warnings: Vec<String>,
}

impl StopReport {
    pub fn to_json(&self) -> Value {
        json!({
            "stopped": self.stopped,
            "display": self.display,
            "signalled": self.processes,
            "warnings": self.warnings,
        })
    }
}

pub struct DisplayManager {
    inner: tokio::sync::Mutex<Option<Arc<Session>>>,
    pub tools: Tools,
    /// Unix seconds of the last tool call, used by the idle watchdog.
    last_activity: AtomicU64,
    /// Calls currently executing; the watchdog must not stop a busy desktop.
    active_calls: AtomicU64,
    pub idle_timeout: Option<Duration>,
    pub max_lifetime: Option<Duration>,
}

impl DisplayManager {
    pub fn new() -> Self {
        let seconds = |key: &str| -> Option<Duration> {
            std::env::var(key)
                .ok()
                .and_then(|value| value.trim().parse::<u64>().ok())
                .filter(|value| *value > 0)
                .map(Duration::from_secs)
        };
        Self {
            inner: tokio::sync::Mutex::new(None),
            tools: Tools::discover(),
            last_activity: AtomicU64::new(state::now()),
            active_calls: AtomicU64::new(0),
            idle_timeout: seconds("XEPHYR_MCP_IDLE_TIMEOUT_SECS"),
            max_lifetime: seconds("XEPHYR_MCP_MAX_LIFETIME_SECS"),
        }
    }

    pub fn begin_call(&self) {
        self.active_calls.fetch_add(1, Ordering::Relaxed);
        self.touch();
    }

    pub fn end_call(&self) {
        self.touch();
        self.active_calls.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn touch(&self) {
        self.last_activity.store(state::now(), Ordering::Relaxed);
    }

    /// True when the idle timeout or maximum lifetime has elapsed and no tool
    /// call is in flight.
    pub fn should_auto_stop(&self, running_for: Duration) -> Option<&'static str> {
        if self.active_calls.load(Ordering::Relaxed) > 0 {
            return None;
        }
        if let Some(limit) = self.idle_timeout {
            let idle = Duration::from_secs(
                state::now().saturating_sub(self.last_activity.load(Ordering::Relaxed)),
            );
            if idle >= limit {
                return Some("idle timeout");
            }
        }
        if let Some(limit) = self.max_lifetime {
            if running_for >= limit {
                return Some("maximum lifetime");
            }
        }
        None
    }

    pub async fn session(&self) -> Option<Arc<Session>> {
        self.inner.lock().await.clone()
    }

    pub async fn start(
        &self,
        width: u32,
        height: u32,
        visible: bool,
        session_id: Option<String>,
        persist_home: bool,
    ) -> Result<StartReport> {
        if !(64..=8192).contains(&width) || !(64..=8192).contains(&height) {
            bail!("width and height must be between 64 and 8192 pixels");
        }
        let session_id = match session_id {
            Some(id) => {
                validate_session_id(&id)?;
                if let Some(existing) = state::read_session(&id) {
                    if crate::proc::alive(existing.server_pid) {
                        bail!(
                            "session id {id:?} is already in use by display {}",
                            existing.display
                        );
                    }
                }
                id
            }
            None => generate_session_id(),
        };

        let mut guard = self.inner.lock().await;
        if let Some(session) = guard.as_ref() {
            bail!(
                "a nested display is already running at {} ({}x{}); call display_stop first",
                session.info.display,
                session.width(),
                session.height()
            );
        }

        let stale = state::cleanup_stale();
        let mut warnings: Vec<String> = stale;

        let server_tool = if visible {
            if env::host_display().is_none() {
                bail!(
                    "visible=true needs an X11 display for Xephyr's window, but DISPLAY is unset. \
                     Run display_start from a desktop session, or use visible=false for Xvfb."
                );
            }
            self.tools
                .xephyr
                .clone()
                .ok_or_else(|| anyhow!(missing_xephyr_hint()))?
        } else {
            self.tools
                .xvfb
                .clone()
                .ok_or_else(|| anyhow!(missing_xvfb_hint()))?
        };
        let xserver_name = file_name(&server_tool.path);

        if self.tools.wm.is_none() {
            warnings.push(
                "no window manager found (openbox/fluxbox/jwm/twm); windows will not be decorated \
                 and focus activation may be limited"
                    .to_string(),
            );
        }
        if self.tools.dbus_launch.is_none() {
            bail!("dbus-launch was not found; install the dbus package");
        }
        if self.tools.xdotool.is_none() {
            warnings
                .push("xdotool was not found; input tools will fail until it is installed".into());
        }

        let server_pid = std::process::id();
        let mut last_error: Option<anyhow::Error> = None;
        let mut attempts = 0u32;

        for number in 100u32..=250 {
            if display_in_use(number) {
                continue;
            }
            if attempts >= 6 {
                break;
            }
            attempts += 1;
            match self
                .start_once(
                    number,
                    width,
                    height,
                    visible,
                    &server_tool,
                    &xserver_name,
                    server_pid,
                    &session_id,
                    persist_home,
                    &mut warnings,
                )
                .await
            {
                Ok(info) => {
                    state::write(&info.state())?;
                    write_root_marker(&info)?;
                    let session = Arc::new(Session {
                        info: info.clone(),
                        launched: Mutex::new(Vec::new()),
                        size: Mutex::new((info.width, info.height)),
                        marks: Mutex::new(Vec::new()),
                        exits: Mutex::new(std::collections::HashMap::new()),
                        launch_seq: AtomicU64::new(0),
                    });
                    *guard = Some(session);
                    return Ok(StartReport { info, warnings });
                }
                Err(err) => {
                    last_error = Some(err);
                    // Try the next display number; a lock file can be stale or
                    // another client may have raced us to the socket.
                    continue;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("could not find a free X display number")))
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_once(
        &self,
        number: u32,
        width: u32,
        height: u32,
        visible: bool,
        server_tool: &Tool,
        xserver_name: &str,
        server_pid: u32,
        session_id: &str,
        persist_home: bool,
        warnings: &mut Vec<String>,
    ) -> Result<SessionInfo> {
        let display = format!(":{number}");
        let runtime_dir = state::session_dir(number, server_pid);
        env::create_private_dir(&runtime_dir)
            .with_context(|| format!("cannot create {}", runtime_dir.display()))?;
        let log_dir = runtime_dir.join("logs");
        env::create_private_dir(&log_dir)?;

        let home = if persist_home {
            let home = state::session_home(session_id)?;
            env::create_private_dir(&home)?;
            for child in [".config", ".local/share", ".local/state", ".cache"] {
                let _ = std::fs::create_dir_all(home.join(child));
            }
            Some(home)
        } else {
            None
        };

        let xauthority = runtime_dir.join("Xauthority");
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&xauthority)
            .with_context(|| format!("cannot create {}", xauthority.display()))?;

        // A private session bus is what keeps notifications, portals and
        // single-instance applications out of the real session.
        let bus = match start_private_bus(&runtime_dir).await {
            Ok(bus) => bus,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&runtime_dir);
                return Err(err);
            }
        };

        let xlog = log_dir.join("xserver.log");
        let mut cmd = Command::new(&server_tool.path);
        cmd.env_clear();
        if visible {
            cmd.arg(&display)
                .arg("-screen")
                .arg(format!("{width}x{height}"))
                .arg("-ac")
                .arg("-br")
                .arg("-noreset")
                .arg("-nolisten")
                .arg("tcp")
                .arg("-dpi")
                .arg("96")
                .arg("-title")
                .arg(format!("xephyr-mcp {display} ({width}x{height})"));
            if let Some(host) = env::host_display() {
                cmd.env("DISPLAY", host);
            }
            if let Some(auth) = env::host_xauthority() {
                cmd.env("XAUTHORITY", auth);
            }
        } else {
            cmd.arg(&display)
                .arg("-screen")
                .arg("0")
                .arg(format!("{width}x{height}x24"))
                .arg("-ac")
                .arg("-noreset")
                .arg("-nolisten")
                .arg("tcp");
        }
        apply_env(&mut cmd, &env::base_environment());
        apply_env(&mut cmd, &server_tool.extra);
        let xserver_pid = match spawn_logged(&mut cmd, &xlog) {
            Ok(pid) => pid,
            Err(err) => {
                proc::kill_group(bus.group_pid, libc::SIGTERM);
                proc::signal_process(bus.pid, libc::SIGTERM);
                let _ = std::fs::remove_dir_all(&runtime_dir);
                return Err(err).with_context(|| {
                    format!(
                        "cannot start {}: {}",
                        server_tool.path.display(),
                        proc::tail(&xlog, 5)
                    )
                });
            }
        };

        if let Err(err) =
            wait_for_display(&display, xserver_pid, &xlog, Duration::from_secs(8)).await
        {
            proc::kill_group(xserver_pid, libc::SIGTERM);
            proc::kill_group(bus.group_pid, libc::SIGTERM);
            proc::signal_process(bus.pid, libc::SIGTERM);
            let _ = std::fs::remove_dir_all(&runtime_dir);
            return Err(err.context(format!(
                "{} did not start on {display}",
                server_tool.path.display()
            )));
        }

        let mut info = SessionInfo {
            session_id: session_id.to_string(),
            number,
            display: display.clone(),
            width,
            height,
            visible,
            xserver: xserver_name.to_string(),
            wm: self.tools.wm_name(),
            dbus_address: bus.address,
            dbus_pid: bus.pid,
            dbus_group: bus.group_pid,
            xserver_pid,
            wm_pid: None,
            a11y_registry_pid: None,
            a11y_ready: false,
            runtime_dir: runtime_dir.clone(),
            xauthority: xauthority.clone(),
            log_dir: log_dir.clone(),
            home,
            persistent_home: persist_home,
            started_at: state::now(),
        };

        // Window manager.
        if let Some(wm) = &self.tools.wm {
            let wmlog = log_dir.join("window-manager.log");
            let mut cmd = Command::new(&wm.path);
            cmd.env_clear();
            if wm.path.file_name().and_then(|n| n.to_str()) == Some("openbox") {
                cmd.arg("--sm-disable");
            }
            let mut wm_env = info_env(&info, &runtime_dir, &xauthority);
            wm_env.extend(wm.extra.clone());
            apply_env(&mut cmd, &wm_env);
            match spawn_logged(&mut cmd, &wmlog) {
                Ok(pid) => {
                    info.wm_pid = Some(pid);
                    if !wait_for_wm(&display, pid, &wmlog, Duration::from_millis(2500)).await {
                        warnings.push(format!(
                            "{} did not advertise EWMH support; focus/window tools will use X11 fallbacks{}",
                            file_name(&wm.path),
                            log_hint(&wmlog)
                        ));
                    }
                }
                Err(err) => {
                    info.wm = None;
                    warnings.push(format!(
                        "could not start window manager {}: {err}{}",
                        wm.path.display(),
                        log_hint(&wmlog)
                    ));
                }
            }
        }

        // Accessibility bus.  Setting org.a11y.Status=true on the private
        // session bus starts at-spi-bus-launcher there; the launcher's socket
        // lives under our private XDG_RUNTIME_DIR.  The registry is started
        // explicitly because D-Bus activation can collide with the host's
        // systemd user unit.
        let a11y = init_accessibility(&info, &runtime_dir, &xauthority).await;
        info.a11y_ready = a11y.ready;
        info.a11y_registry_pid = a11y.registry_pid;
        warnings.extend(a11y.warnings);

        Ok(info)
    }

    pub async fn stop(&self) -> Result<StopReport> {
        let mut guard = self.inner.lock().await;
        let Some(session) = guard.take() else {
            return Ok(StopReport {
                stopped: false,
                display: None,
                processes: Vec::new(),
                warnings: vec!["no nested display is running".into()],
            });
        };

        let info = &session.info;
        let launched: Vec<u32> = session
            .launched
            .lock()
            .map(|list| list.iter().map(|p| p.pid).collect())
            .unwrap_or_default();

        let mut pids = info.pids();
        pids.extend(launched);
        pids.retain(|pid| *pid > 0);

        // Kill every group at once, then the individual bus PID in case the
        // daemon left its original process group.
        proc::terminate_groups(&pids).await;
        for pid in &pids {
            proc::signal_process(*pid, libc::SIGTERM);
        }
        proc::signal_process(info.dbus_pid, libc::SIGTERM);

        // Launched applications can fork or detach; the private runtime
        // directory in their environment is a reliable last sweep.
        state::kill_runtime_dir(&info.runtime_dir);

        state::clear(&info.session_id);
        let _ = std::fs::remove_dir_all(&info.runtime_dir);

        Ok(StopReport {
            stopped: true,
            display: Some(info.display.clone()),
            processes: pids,
            warnings: Vec::new(),
        })
    }
}

impl Default for DisplayManager {
    fn default() -> Self {
        Self::new()
    }
}

struct BusInfo {
    address: String,
    pid: u32,
    group_pid: u32,
}

async fn start_private_bus(runtime_dir: &Path) -> Result<BusInfo> {
    let dbus_launch = env::find_tool("dbus-launch", env::runtime_root().as_deref())
        .ok_or_else(|| anyhow!("dbus-launch was not found; install the dbus package"))?;

    let mut cmd = Command::new(&dbus_launch.path);
    cmd.env_clear();
    cmd.arg("--sh-syntax");
    let mut entries = env::base_environment();
    entries.push(("XDG_RUNTIME_DIR".into(), runtime_dir.display().to_string()));
    apply_env(&mut cmd, &entries);
    proc::detach(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let child = cmd.spawn().context("cannot run dbus-launch")?;
    let group_pid = child.id().unwrap_or(0);
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("dbus-launch timed out"))?
        .context("dbus-launch failed")?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("dbus-launch failed: {}", err.trim());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let mut address = None;
    let mut pid = None;
    for line in text.lines() {
        let line = line.trim().trim_end_matches(';');
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().trim_matches('\'');
            match key {
                "DBUS_SESSION_BUS_ADDRESS" => address = Some(value.to_string()),
                "DBUS_SESSION_BUS_PID" => pid = value.parse::<u32>().ok(),
                _ => {}
            }
        }
    }

    let address = address.ok_or_else(|| {
        anyhow!("dbus-launch did not report DBUS_SESSION_BUS_ADDRESS (output: {text:?})")
    })?;
    let pid = pid
        .or(Some(group_pid))
        .filter(|p| *p > 0)
        .ok_or_else(|| anyhow!("dbus-launch returned no bus pid"))?;

    // The daemon keeps the process group of the dbus-launch process; record
    // that group as well so cleanup does not depend on daemon behaviour.
    Ok(BusInfo {
        address,
        pid,
        group_pid,
    })
}

fn display_in_use(number: u32) -> bool {
    Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
        || Path::new(&format!("/tmp/.X{number}-lock")).exists()
}

async fn wait_for_display(display: &str, pid: u32, log: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !proc::alive(pid) {
            bail!("X server exited early{}", log_hint(log));
        }
        if crate::x11::connect(display).is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
    bail!(
        "timed out waiting for {display} to accept connections{}",
        log_hint(log)
    )
}

async fn wait_for_wm(display: &str, pid: u32, log: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let Ok((conn, screen_num)) = crate::x11::connect(display) else {
        return false;
    };
    let Ok(atom) = crate::x11::atom(&conn, "_NET_SUPPORTING_WM_CHECK") else {
        return false;
    };
    let root = crate::x11::root(&conn, screen_num);
    while Instant::now() < deadline {
        if !proc::alive(pid) {
            return false;
        }
        if let Ok(Ok(reply)) = conn
            .get_property(
                false,
                root,
                atom,
                x11rb::protocol::xproto::AtomEnum::WINDOW,
                0,
                1,
            )
            .map(|cookie| cookie.reply())
        {
            if reply.value_len > 0 {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let _ = log;
    false
}

struct A11yInit {
    ready: bool,
    registry_pid: Option<u32>,
    warnings: Vec<String>,
}

async fn init_accessibility(info: &SessionInfo, runtime_dir: &Path, xauthority: &Path) -> A11yInit {
    let mut warnings = Vec::new();
    let nested = info_env(info, runtime_dir, xauthority);
    let Some(gdbus) = env::find_tool("gdbus", env::runtime_root().as_deref()) else {
        warnings.push("gdbus not found; AT-SPI status could not be enabled".into());
        return A11yInit {
            ready: false,
            registry_pid: None,
            warnings,
        };
    };

    let base = |method: &[&str]| {
        let mut cmd = Command::new(&gdbus.path);
        cmd.env_clear();
        cmd.arg("call")
            .arg("--address")
            .arg(&info.dbus_address)
            .arg("--dest")
            .arg("org.a11y.Bus")
            .arg("--object-path")
            .arg("/org/a11y/bus")
            .arg("--method");
        for arg in method {
            cmd.arg(arg);
        }
        cmd
    };

    let mut address_ok = false;
    let mut cmd = base(&["org.a11y.Bus.GetAddress"]);
    apply_env(&mut cmd, &nested);
    match proc::run_output(cmd, Duration::from_secs(5)).await {
        Ok(out) if out.ok() => address_ok = true,
        Ok(out) => warnings.push(format!(
            "could not start the accessibility bus: {}",
            first_nonempty(&out.stderr).unwrap_or_else(|| out.stdout.trim().to_string())
        )),
        Err(err) => warnings.push(format!("accessibility bus probe failed: {err}")),
    }

    let mut enabled_ok = false;
    if address_ok {
        let mut cmd = base(&[
            "org.freedesktop.DBus.Properties.Set",
            "org.a11y.Status",
            "IsEnabled",
            "<true>",
        ]);
        apply_env(&mut cmd, &nested);
        match proc::run_output(cmd, Duration::from_secs(5)).await {
            Ok(out) if out.ok() => enabled_ok = true,
            Ok(out) => warnings.push(format!(
                "could not enable AT-SPI: {}",
                first_nonempty(&out.stderr).unwrap_or_else(|| out.stdout.trim().to_string())
            )),
            Err(err) => warnings.push(format!("AT-SPI enable failed: {err}")),
        }
    }

    let mut registry_pid = None;
    if enabled_ok {
        if let Some(registryd) = &env::find_registryd() {
            let log = info.log_dir.join("a11y-registry.log");
            let mut cmd = Command::new(registryd);
            cmd.env_clear();
            apply_env(&mut cmd, &nested);
            match spawn_logged(&mut cmd, &log) {
                Ok(pid) => {
                    tokio::time::sleep(Duration::from_millis(180)).await;
                    if proc::alive(pid) {
                        registry_pid = Some(pid);
                    } else {
                        // A registry may already have been started through
                        // D-Bus activation; that is not fatal.
                        let tail = proc::tail(&log, 3);
                        if !tail.is_empty() {
                            warnings.push(format!("AT-SPI registry note: {tail}"));
                        }
                    }
                }
                Err(err) => warnings.push(format!("could not start at-spi2-registryd: {err}")),
            }
        } else {
            warnings.push(
                "at-spi2-registryd was not found; a11y tools may not see applications".into(),
            );
        }
    }

    A11yInit {
        ready: enabled_ok,
        registry_pid,
        warnings,
    }
}

fn info_env(info: &SessionInfo, runtime_dir: &Path, xauthority: &Path) -> Vec<(String, String)> {
    let mut entries = env::base_environment();
    if let Some(home) = &info.home {
        entries.push(("HOME".into(), home.display().to_string()));
        entries.push((
            "XDG_CONFIG_HOME".into(),
            home.join(".config").display().to_string(),
        ));
        entries.push((
            "XDG_DATA_HOME".into(),
            home.join(".local/share").display().to_string(),
        ));
        entries.push((
            "XDG_STATE_HOME".into(),
            home.join(".local/state").display().to_string(),
        ));
        entries.push((
            "XDG_CACHE_HOME".into(),
            home.join(".cache").display().to_string(),
        ));
    }
    entries.push(("DISPLAY".into(), info.display.clone()));
    entries.push(("DBUS_SESSION_BUS_ADDRESS".into(), info.dbus_address.clone()));
    entries.push(("XDG_RUNTIME_DIR".into(), runtime_dir.display().to_string()));
    entries.push(("XAUTHORITY".into(), xauthority.display().to_string()));
    entries.push(("XDG_SESSION_TYPE".into(), "x11".into()));
    entries.push(("GDK_BACKEND".into(), "x11".into()));
    entries.push(("QT_QPA_PLATFORM".into(), "xcb".into()));
    entries.push(("GTK_A11Y".into(), "atspi".into()));
    entries.push(("TERM".into(), "xterm-256color".into()));
    entries
}

fn apply_env(cmd: &mut Command, entries: &[(String, String)]) {
    for (key, value) in entries {
        cmd.env(key, value);
    }
}

fn spawn_logged(cmd: &mut Command, log: &Path) -> Result<u32> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
        .with_context(|| format!("cannot open log {}", log.display()))?;
    let file2 = file.try_clone()?;
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::from(file));
    cmd.stderr(std::process::Stdio::from(file2));
    proc::detach(cmd);
    let child = cmd.spawn()?;
    Ok(child.id().unwrap_or(0))
}

fn write_root_marker(info: &SessionInfo) -> Result<()> {
    let (conn, screen_num) = crate::x11::connect(&info.display)?;
    let atom = crate::x11::atom(&conn, state::ROOT_MARKER)?;
    let text = info.state().marker_json();
    crate::x11::set_string_property(
        &conn,
        crate::x11::root(&conn, screen_num),
        atom,
        x11rb::protocol::xproto::AtomEnum::STRING,
        &text,
    )?;
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn log_hint(log: &Path) -> String {
    let tail = proc::tail(log, 4);
    if tail.trim().is_empty() {
        String::new()
    } else {
        format!(" (log: {})", tail.trim())
    }
}

fn first_nonempty(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

fn missing_xephyr_hint() -> String {
    "Xephyr was not found. Install xorg-server-xephyr, or run \
     ./scripts/fetch-runtime.sh to install a private copy."
        .into()
}

fn missing_xvfb_hint() -> String {
    "Xvfb was not found. Install xorg-server-xvfb, or run \
     ./scripts/fetch-runtime.sh to install a private copy."
        .into()
}

pub fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 64 {
        bail!("session_id must be 1-64 characters");
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("session_id may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

fn generate_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "{:x}{:04x}",
        nanos & 0xffff_ffff,
        std::process::id() & 0xffff
    )
}
