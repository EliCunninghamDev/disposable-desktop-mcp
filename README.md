# xephyr-mcp

An MCP server that gives an agent its own private X11 desktop, plus a live
GTK4 viewer for watching it. Everything the agent launches runs inside a nested
Xephyr (visible) or Xvfb (headless) display with its own window manager, D-Bus
session bus, accessibility bus, and runtime directory. Host input, host windows,
and the host session bus are never used.

```
┌──────────────┐   stdio JSON-RPC   ┌──────────────────────────────┐
│ MCP client   │ ─────────────────► │ xephyr-mcp                   │
│ (OpenCode)   │                    │  ├─ Xephyr / Xvfb  :100      │
└──────────────┘                    │  ├─ openbox                  │
                                    │  ├─ private dbus-daemon      │
                                    │  ├─ AT-SPI bus (optional)    │
                                    │  └─ launched applications    │
                                    └──────────────┬───────────────┘
                                                   │ GetImage + XTEST
                                    ┌──────────────▼───────────────┐
                                    │ xephyr-mcp-viewer (GTK4)     │
                                    │  dropdown · Single · BSP     │
                                    └──────────────────────────────┘
```

## Features

- **Nested, disposable desktops.** `display_start` chooses a free display
  number (`:100`–`:250`), starts Xephyr or Xvfb, a window manager, a private
  D-Bus session bus, a private AT-SPI bus, and a fresh `XDG_RUNTIME_DIR` and
  `XAUTHORITY`.
- **Multiple sessions.** Callers can pass `session_id` (`alpha`, `beta`, ...).
  Each server instance/session is tracked independently and the viewer can
  switch or tile between them.
- **Pure-Rust screenshots.** `GetImage` + PNG encoding, with region/window
  crops, optional cursor and coordinate-grid overlays, and downscaling for
  token-efficient full-screen captures.
- **Input through XTEST on the nested server.** Mouse moves/clicks/drags,
  scrolling, typing, and key combos never touch the host pointer.
- **EWMH window management.** List, focus, move/resize, and close windows
  directly over X11.
- **Accessibility via AT-SPI.** Query the tree by name/role, activate by
  accessible name, focus elements, and set editable text.
- **Settling primitives.** `wait_for_idle` and `wait_for` block on frame
  stability, window appearance, process exit, changed regions, or files,
  instead of making agents poll screenshots.
- **Clipboard and process tools.** In-process CLIPBOARD ownership avoids
  per-character typing, and session-scoped process/log tools make failed
  launches diagnosable.
- **Runtime control.** XRandR resizing on Xephyr, EWMH window state changes,
  held mouse/key input, persistent per-session HOME, and an auto-teardown
  watchdog.
- **Trace and marks.** Every tool call is appended to
`$XDG_RUNTIME_DIR/xephyr-mcp/traces/<session_id>.jsonl` with the sanitized
arguments, duration, outcome, and before/after frame hashes for screen-changing
tools. Traces survive `display_stop`; read them with `trace_tail`.

Set `XEPHYR_MCP_TRACE_SCREENSHOTS=1` to also save the before/after frame of each
screen-changing call under `traces/steps/<session>/`. The trace entry then
carries `before_shot` and `after_shot` paths; the newest 400 images per session
are kept.

## Process lifetime

`launch` starts each command in a detached process group. The process keeps
running after the call returns, after its window closes, and after it has
written whatever file it was asked to produce. `launch` returning
`running: true` means the process is still alive; it is not a background job
that the server reaps on the next call. Use `launch_status` to check,
`kill_process all=true` to stop everything you started, and `display_stop` to
tear down the whole desktop.

Launch handles stay queryable: `launch` records every child and a background
reaper stores its exit status, so `launch_status` can report `running`,
`exit_code`, the terminating signal, and uptime long after launch returned.

`persist_home=true` on `display_start` keeps `HOME` and XDG directories under
`$XDG_RUNTIME_DIR/xephyr-mcp/homes/<session_id>` across restarts with the same
session id. Set `XEPHYR_MCP_IDLE_TIMEOUT_SECS` or
`XEPHYR_MCP_MAX_LIFETIME_SECS` to enable the watchdog, which tears down a
desktop when no tool call is active and the limit has elapsed.

The clipboard tools own the X `CLIPBOARD` selection directly (a background
thread answers `SelectionRequest`); the host clipboard is never used. Direct
selection transfer is limited to 1 MiB per set.

`list_processes` matches processes by their private `XDG_RUNTIME_DIR`, and
`kill_process` refuses PIDs that do not belong to the session, so it cannot
signal host processes. `kill_process all=true` is the one-call cleanup for
agents: it sends `SIGTERM` to every launched process group, waits briefly, then
`SIGKILL`s whatever is left. `tail_log` only reads inside the session log directory.

## Wayland backend

`backend="wayland"` starts sway in the private runtime directory. `visible=false`
is a headless compositor (the Xvfb analog); `visible=true` opens a nested window
on the host Wayland session, or on the host X11 session via `WLR_BACKENDS=x11`
(the Xephyr analog). X11 remains the default.

Implemented on this backend:

- sway lifecycle, socket discovery, private D-Bus and AT-SPI, clean
  `display_stop` via the sway IPC `exit` command plus the process-group sweep;
- output capture through `wlr-screencopy-v1` into `wl_shm`, with the existing
  crop/resize/PNG and cursor/grid/marks overlays;
- `list_windows`, `focus_window`, `close_window`, and `window_state` through
  sway IPC; geometry is exact for the compositor rect;
- `move_resize_window` for floating windows, with separate resize/move commands
  and confirmation polling. Tiled windows return an explicit error instead of
  silently reflowing the desktop;
- `resize_display` through `output <name> mode`;
- optional `xwayland=true` for X11-only apps.

Pointer/keyboard injection and the data-control clipboard are the next phase, so
this build reports `pointer: false`, `keyboard: false`, and `clipboard: "none"`
for Wayland sessions and the input/clipboard tools return a capability error.
The viewer is still X11-only. The protocol map, spike results, and acceptance
tests are in [docs/wayland-backend.md](docs/wayland-backend.md).

```json
{ "backend": "wayland", "visible": false, "width": 1024, "height": 768 }
```

To prepare the compositor without root:

```sh
./scripts/fetch-runtime.sh --wayland
./scripts/wayland-smoke.sh
```

The smoke test starts a private headless sway, verifies the output mode,
captures it with `grim`, exercises floating move/resize and `wtype`, then shuts
everything down. It never connects to the host compositor.

## Viewer

```sh
# Follow the current/default session
xephyr-mcp-viewer

# Follow a specific session
xephyr-mcp-viewer --session alpha

# List discoverable sessions and exit
xephyr-mcp-viewer --list

# Open directly in BSP tiling mode
xephyr-mcp-viewer --layout bsp
```

The toolbar contains:

- a **session dropdown** listing every live nested desktop;
- **Single** — show only the selected session;
- **BSP `N`** — tile up to eight live sessions with a balanced binary split.

Selecting a session in BSP mode switches back to Single for that session.
Keyboard shortcuts: `q`/`Esc` quit, `f`/`F11` fullscreen, `t` toggle BSP,
`s` cycle sessions. The viewer is read-only; it never sends input into a nested
desktop.

The window is aspect-ratio-locked in Single mode via `WM_NORMAL_HINTS` when it
runs on X11 (the default when an X display is reachable). Content is always
letterboxed to preserve the nested display's aspect ratio, including on
Wayland-only hosts where the compositor cannot be given an aspect hint. Each
viewer invocation is a separate process and window.

Useful options: `--host-display :N`, `--fps N`, `--scale N`,
`--no-aspect-lock`, `--no-cursor`, `--fullscreen`, `--title`.

### Desktop shortcut

`packaging/xephyr-mcp-viewer.desktop` adds the viewer to KRunner, the Plasma
application menu, and other desktop launchers. It also provides an
"Open in BSP tiling mode" action.

```sh
install -Dm644 packaging/xephyr-mcp-viewer.desktop \
  ~/.local/share/applications/xephyr-mcp-viewer.desktop
update-desktop-database ~/.local/share/applications
kbuildsycoca6        # Plasma 6 (kbuildsycoca5 on Plasma 5)
```

The packaged entry uses `Exec=xephyr-mcp-viewer`, so `~/.cargo/bin` (or wherever
`cargo install` placed it) must be on the desktop session's `PATH`. If it is
not, replace the `Exec=` and `TryExec=` lines with the absolute binary path.

## Isolation model

`display_start` builds a scrubbed environment for every nested process:

- `DISPLAY` points at the new nested server;
- `WAYLAND_DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`, and the host
  runtime directory are removed or replaced;
- `XDG_RUNTIME_DIR` is a fresh directory under `/run/user/<uid>/xephyr-mcp/`;
- a private `dbus-daemon --session` and an AT-SPI bus are started;
- Xephyr/Xvfb, openbox, the bus daemons, and launched applications are placed
  in their own process groups.

`display_stop` terminates the groups, then verifies no processes remain. If the
server dies unexpectedly, the next start finds the stale state file, inspects
`/proc/<pid>/environ` to confirm ownership, and reaps the old runtime.

State lives in `$XDG_RUNTIME_DIR/xephyr-mcp/` (sessions are in `sessions/` and
the current session in `current.json`). Session directories are removed when
their display stops.

## Accessibility notes

AT-SPI provides roles, names, and actions independent of screen coordinates.
GTK3 and many Qt applications report accurate geometry. GTK4 currently reports
`(0,0)` for child widget positions, so nodes include
`"geometry_precise": false`; sizes remain valid and `a11y_click` still invokes
the action. Prefer `a11y_click` by name, or use `screenshot` coordinates when
geometry is imprecise.

The AT-SPI bus is started per session. If PyGObject is unavailable, the a11y
tools return a clear error and the rest of the server keeps working.

## Limitations

- Linux/X11 only. The viewer needs a host X11/XWayland display.
- The viewer is intentionally read-only; input passthrough is not implemented.
- `launch` runs arbitrary shell commands inside the nested desktop. Treat the
  MCP server as trusted local tooling.
- Software rendering (GTK's Cairo renderer) is used automatically on hosts
  without working GL; capture and painting still work.

## Development

```sh
cargo fmt
cargo test
cargo clippy --all-targets
cargo build --release
```

The viewer's capture loop is a self-rescheduling low-priority GLib source: a
slow `GetImage` tick can otherwise keep the main loop busy and starve GTK's
redraw source, which looks like a frozen or flickering window.

## License

MIT. See [LICENSE](LICENSE).
