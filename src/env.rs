//! Locating tools and building the scrubbed environment for nested processes.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A resolved executable plus any library/data directories it needs.
#[derive(Clone, Debug)]
pub struct Tool {
    pub path: PathBuf,
    /// Extra environment entries (LD_LIBRARY_PATH, XDG_DATA_DIRS, ...) needed
    /// when the tool comes from the extracted `.runtime` tree.
    pub extra: Vec<(String, String)>,
}

impl Tool {
    pub fn label(&self) -> String {
        self.path.display().to_string()
    }
}

/// A private runtime tree created by `scripts/fetch-runtime.sh`.
pub fn runtime_root() -> Option<PathBuf> {
    let looks_right = |p: &Path| {
        ["usr/bin/Xephyr", "bin/Xephyr", "usr/bin/Xvfb", "bin/Xvfb"]
            .iter()
            .any(|rel| is_executable(&p.join(rel)))
    };

    if let Some(value) = env::var_os("XEPHYR_MCP_RUNTIME") {
        let path = PathBuf::from(value);
        if looks_right(&path) {
            return Some(path);
        }
    }

    if let Ok(exe) = env::current_exe() {
        let mut dir = exe.parent();
        for _ in 0..5 {
            let Some(current) = dir else { break };
            let candidate = current.join(".runtime");
            if looks_right(&candidate) {
                return Some(candidate);
            }
            dir = current.parent();
        }
    }

    if let Ok(cwd) = env::current_dir() {
        let candidate = cwd.join(".runtime");
        if looks_right(&candidate) {
            return Some(candidate);
        }
    }

    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        for candidate in [
            home.join(".local/opt/xephyr-mcp"),
            home.join(".local/share/xephyr-mcp"),
        ] {
            if looks_right(&candidate) {
                return Some(candidate);
            }
        }
    }

    None
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Resolve one executable.  Installed tools win; the bundled runtime is the
/// fallback so a rootless checkout still works.
pub fn find_tool(name: &str, runtime: Option<&Path>) -> Option<Tool> {
    if let Some(path) = find_in_path(name) {
        return Some(Tool {
            path,
            extra: Vec::new(),
        });
    }

    let runtime = runtime?;
    for rel in [format!("usr/bin/{name}"), format!("bin/{name}")] {
        let candidate = runtime.join(rel);
        if is_executable(&candidate) {
            let mut extra = Vec::new();
            let lib = runtime.join("usr/lib");
            if lib.is_dir() {
                extra.push(("LD_LIBRARY_PATH".to_string(), lib.display().to_string()));
            }
            let data = runtime.join("usr/share");
            if data.is_dir() {
                extra.push((
                    "XDG_DATA_DIRS".to_string(),
                    join_paths(&data, "/usr/local/share:/usr/share"),
                ));
            }
            let config = runtime.join("etc/xdg");
            if config.is_dir() {
                extra.push((
                    "XDG_CONFIG_DIRS".to_string(),
                    join_paths(&config, "/etc/xdg"),
                ));
            }
            return Some(Tool {
                path: candidate,
                extra,
            });
        }
    }
    None
}

fn join_paths(first: &Path, rest: &str) -> String {
    format!("{}:{}", first.display(), rest)
}

pub fn find_in_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable(&path).then_some(path);
    }

    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Resolve `at-spi2-registryd`, which Arch installs under /usr/lib rather than
/// on PATH.
pub fn find_registryd() -> Option<PathBuf> {
    if let Some(path) = find_in_path("at-spi2-registryd") {
        return Some(path);
    }
    for candidate in [
        "/usr/lib/at-spi2-registryd",
        "/usr/libexec/at-spi2-registryd",
        "/usr/local/libexec/at-spi2-registryd",
    ] {
        let path = PathBuf::from(candidate);
        if is_executable(&path) {
            return Some(path);
        }
    }
    None
}

/// Variables we deliberately inherit from the host.
const INHERIT: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LANGUAGE", "TZ",
];

/// Environment shared by every process we spawn.  This deliberately contains
/// no DISPLAY, no WAYLAND_DISPLAY, no XAUTHORITY, and no D-Bus address: callers
/// add the nested values explicitly.
pub fn base_environment() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();

    for key in INHERIT {
        if let Some(value) = env::var_os(key) {
            out.push((key.to_string(), value.to_string_lossy().into_owned()));
        }
    }

    // Locale categories are numerous and harmless to inherit.
    for (key, value) in env::vars_os() {
        let key = key.to_string_lossy();
        if key.starts_with("LC_") {
            out.push((key.into_owned(), value.to_string_lossy().into_owned()));
        }
    }

    if !out.iter().any(|(k, _)| k == "PATH") {
        out.push(("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()));
    }
    if !out.iter().any(|(k, _)| k == "HOME") {
        if let Some(home) = env::var_os("HOME") {
            out.push(("HOME".into(), home.to_string_lossy().into_owned()));
        }
    }

    out
}

pub fn env_map(entries: &[(String, String)]) -> HashMap<String, String> {
    entries.iter().cloned().collect()
}

/// Host display used by Xephyr to put its window on the real desktop.
pub fn host_display() -> Option<String> {
    env::var("DISPLAY").ok().filter(|s| !s.trim().is_empty())
}

pub fn host_xauthority() -> Option<OsString> {
    env::var_os("XAUTHORITY").filter(|s| !s.is_empty())
}

/// Runtime directory for state shared between the server and viewer.
/// Falls back to a per-uid temp directory when XDG_RUNTIME_DIR is unset.
pub fn shared_runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir).join("xephyr-mcp");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/xephyr-mcp-{uid}"))
}

pub fn state_file() -> PathBuf {
    shared_runtime_dir().join("current.json")
}

/// A per-session directory with mode 0700.
pub fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}
