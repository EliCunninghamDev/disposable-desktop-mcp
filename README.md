# disposable-desktop-mcp

An MCP server that gives an agent its own private desktop, plus a live GTK4
viewer for watching it. X11 (the default) uses Xvfb (headless) or a nested
Xephyr window; Wayland uses sway, headless or nested on the host compositor.
`display_start` is headless by default: `visible=true` is opt-in for a window a
person can watch. The backend is chosen per session, and both are driven through
the same tool surface. Both get a private D-Bus session bus, accessibility bus,
and runtime directory. Host input, host windows, and the host session bus are
never used.

```
┌──────────────┐   stdio JSON-RPC   ┌──────────────────────────────────────────────┐
│ MCP client   │ ─────────────────► │ disposable-desktop-mcp                       │
│ (OpenCode)   │                    │  X11:     Xephyr / Xvfb  :100                │
└──────────────┘                    │           openbox · GetImage · XTEST         │
                                    │  Wayland: sway (headless / nested)           │
                                    │           screencopy · virtual input         │
                                    │  both:    private dbus · AT-SPI · apps       │
                                    └──────────────┬───────────────────────────────┘
                                                   │ frames + forwarded input
                                    ┌──────────────▼───────────────────────────────┐
                                    │ disposable-desktop-viewer (GTK4)             │
                                    │  dropdown · Single · BSP                     │
                                    └──────────────────────────────────────────────┘
```

## Features

- **Nested, disposable desktops.** `display_start` picks a free X11 display
  (`:100`–`:250`) and starts Xephyr or Xvfb, openbox, a private D-Bus session
  bus, a private AT-SPI bus, and a fresh `XDG_RUNTIME_DIR` and `XAUTHORITY`; or
  it starts a headless/nested sway with its own runtime, socket, and buses.
- **Multiple sessions.** Callers can pass `session_id` (`alpha`, `beta`, ...).
  Each server instance/session is tracked independently and the viewer can
  switch or tile between them.
- **Pure-Rust capture, both backends.** X11 reads pixels with `GetImage`;
  Wayland uses `wlr-screencopy-v1` for outputs and `ext-image-copy-capture-v1`
  for individual windows. The same PNG pipeline, region/window crops,
  cursor/grid/marks overlays, and downscaling apply to either backend.
  `screenshot window_id=` captures a single window even when another window
  covers it.
- **Input that never reaches the host.** X11 uses XTEST on the nested server;
  Wayland uses compositor virtual pointer and keyboard devices. Mouse
  moves/clicks/drags, scrolling, typing, Unicode text, held keys, and key combos
  all stay inside the session.
- **Window management.** EWMH over X11 and sway IPC over Wayland for listing,
  focusing, moving/resizing, changing state, and closing windows. Wayland can
  move/resize floating windows exactly; tiled windows are rejected rather than
  reflowing the desktop.
- **Accessibility via AT-SPI.** Query the tree by name/role, activate by
  accessible name, focus elements, and set editable text.
- **Settling primitives.** `wait_for_idle` and `wait_for` block on frame
  stability, window appearance, process exit, changed regions, or files,
  instead of making agents poll screenshots.
- **Clipboard and process tools.** X11 owns CLIPBOARD in-process; Wayland serves
  `zwlr_data_control_v1` selections from a background worker. Session-scoped
  process/log tools make failed launches diagnosable.
- **Runtime control.** XRandR resizing on Xephyr and `output mode` on sway, EWMH
  and sway window state changes, held mouse/key input, persistent per-session
  HOME, an auto-teardown watchdog, and explicit agent/human input arbitration
  with `wait_for_control`.
- **Trace and marks.** Every tool call is appended to
`$XDG_RUNTIME_DIR/disposable-desktop-mcp/traces/<session_id>.jsonl` with the sanitized
arguments, duration, outcome, and before/after frame hashes for screen-changing
tools. Traces survive `display_stop`; read them with `trace_tail`.

Set `DISPOSABLE_DESKTOP_MCP_TRACE_SCREENSHOTS=1` to also save the before/after frame of each
screen-changing call under `traces/steps/<session>/`. The trace entry then
carries `before_shot` and `after_shot` paths; the newest 400 images per session
are kept.

## Backends at a glance

| | X11 (default) | Wayland |
| --- | --- | --- |
| Nested visible desktop | Xephyr | sway, nested on the host Wayland or X11 session |
| Headless desktop | Xvfb | sway, headless (`WLR_BACKENDS=headless`) |
| Window manager | openbox | sway |
| Output capture | `GetImage` | `wlr-screencopy-v1` |
| Window capture | `GetImage` on the window | `ext-image-copy-capture-v1` |
| Pointer/keyboard | XTEST | `zwlr_virtual_pointer_v1`, `zwp_virtual_keyboard_v1` |
| Windows | EWMH | sway IPC |
| Clipboard | X `CLIPBOARD` owner | `zwlr_data_control_v1` |
| Resize display | XRandR (Xephyr only) | `output <name> mode` |
| Move/resize windows | any window | floating windows only |
| Cursor in window capture | yes (XFixes) | no (`cursor_drawn: false`) |

Capabilities are reported per session by `display_start`, so a tool can check
support instead of guessing from the backend name. Host input can still reach a
visible Xephyr window directly, so prefer a headless X11 session when that
matters; a nested sway session parks its host devices on a `locked` seat.

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
`$XDG_RUNTIME_DIR/disposable-desktop-mcp/homes/<session_id>` across restarts with the same
session id. Set `DISPOSABLE_DESKTOP_MCP_IDLE_TIMEOUT_SECS` or
`DISPOSABLE_DESKTOP_MCP_MAX_LIFETIME_SECS` to enable the watchdog, which tears down a
desktop when no tool call is active and the limit has elapsed.

The clipboard tools own the selection directly: on X11 a background thread
answers `SelectionRequest` for `CLIPBOARD`, and on Wayland a
`zwlr_data_control_v1` worker serves `send` requests while the session owns the
selection. The host clipboard is never read or written. X11 selection transfer
is limited to 1 MiB per `clipboard_set`.

`list_processes` matches processes by their private `XDG_RUNTIME_DIR`, and
`kill_process` refuses PIDs that do not belong to the session, so it cannot
signal host processes. `kill_process all=true` is the one-call cleanup for
agents: it sends `SIGTERM` to every launched process group, waits briefly, then
`SIGKILL`s whatever is left. `tail_log` only reads inside the session log directory.

## Wayland backend

`backend="wayland"` starts sway in the private runtime directory. Headless is
the default (`visible=false`), which is the Xvfb analog. `visible=true` opens a
nested window on the host Wayland session, or on the host X11 session via
`WLR_BACKENDS=x11` (the Xephyr analog), if a person needs to watch. X11 remains
the default backend.

Implemented on this backend:

- sway lifecycle, socket discovery, private D-Bus and AT-SPI, clean
  `display_stop` via the sway IPC `exit` command plus the process-group sweep;
- output capture through `wlr-screencopy-v1` into `wl_shm`, with the existing
  crop/resize/PNG and cursor/grid/marks overlays;
- per-window capture through `ext-foreign-toplevel-list-v1`,
  `ext-foreign-toplevel-image-capture-source-v1`, and
  `ext-image-copy-capture-v1`, so an occluded window is captured without the
  windows above it and the image matches the sway rect;
- `list_windows`, `focus_window`, `close_window`, and `window_state` through
  sway IPC; geometry is exact for the compositor rect;
- `move_resize_window` for floating windows, with separate resize/move commands
  and confirmation polling. Tiled windows return an explicit error instead of
  silently reflowing the desktop;
- `resize_display` through `output <name> mode`;
- pointer and keyboard injection through `zwlr_virtual_pointer_v1` and
  `zwp_virtual_keyboard_v1`, including key holds and Unicode text via a
  generated per-chunk keymap. No ydotool or `/dev/uinput` is involved;
- clipboard get/set through `zwlr_data_control_v1`, with a worker thread that
  keeps serving `send` requests while the session owns the selection;
- optional `xwayland=true` for X11-only apps; this is off by default because it
  creates a host-global `/tmp/.X11-unix` socket.

XWayland clients join the clipboard only after an X11 view has been focused at
least once; that is a sway/wlroots bridge limitation, not a server policy. The
viewer can watch Wayland sessions, but it is still an X11/XWayland client and
needs a host X display. The protocol map, spike results, and acceptance tests
are in [docs/wayland-backend.md](docs/wayland-backend.md).

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
disposable-desktop-viewer

# Follow a specific session
disposable-desktop-viewer --session alpha

# List discoverable sessions and exit
disposable-desktop-viewer --list

# Open directly in BSP tiling mode
disposable-desktop-viewer --layout bsp
```

The toolbar contains:

- a **session dropdown** listing every live nested desktop;
- **Single** — show only the selected session;
- **BSP `N`** — tile up to eight live sessions with a balanced binary split.

Selecting a session in BSP mode switches back to Single for that session.
While the agent owns control, the keyboard shortcuts are `q`/`Esc` quit,
`f`/`F11` fullscreen, `t` toggle BSP, and `s` cycle sessions. Click the image
once so the canvas has keyboard focus if a shortcut does not respond.

The viewer is read-only by default. Press `c` or the **Control** toggle to claim
human control: mouse, wheel, and keyboard events are then forwarded to the
selected session (including `Esc`), the title shows `[HUMAN]`, and the MCP
server refuses agent input until you press `Ctrl+Shift+C`, click **Control**
again, or close the viewer. While human control is active every key goes to the
session, so press `Ctrl+Shift+C` to release and get the shortcuts above back.
`Ctrl+Shift+Q` closes the viewer while human control is active. Claims are tied
to the viewer PID, so a crashed viewer cannot lock an agent out.

The viewer attaches to any running session, headless or visible, so a headless
desktop can be shown on demand without restarting it. Starting with
`visible=true` is only needed when the nested server window itself should exist
on the host from the start.

Wayland sessions (including headless ones) are captured through the output
screencopy path and receive forwarded input through the virtual pointer and
keyboard. A nested Wayland session parks its host input devices on a dummy seat
while the agent owns it and moves them back only during human control.

The window is aspect-ratio-locked in Single mode via `WM_NORMAL_HINTS` when it
runs on X11 (the default when an X display is reachable). Content is always
letterboxed to preserve the nested display's aspect ratio, including on
Wayland-only hosts where the compositor cannot be given an aspect hint. Each
viewer invocation is a separate process and window.

Useful options: `--host-display :N`, `--fps N`, `--scale N`,
`--no-aspect-lock`, `--no-cursor`, `--fullscreen`, `--title`.

### Desktop shortcut

`packaging/disposable-desktop-viewer.desktop` adds the viewer to KRunner, the Plasma
application menu, and other desktop launchers. It also provides an
"Open in BSP tiling mode" action.

```sh
install -Dm644 packaging/disposable-desktop-viewer.desktop \
  ~/.local/share/applications/disposable-desktop-viewer.desktop
update-desktop-database ~/.local/share/applications
kbuildsycoca6        # Plasma 6 (kbuildsycoca5 on Plasma 5)
```

The packaged entry uses `Exec=disposable-desktop-viewer`, so `~/.cargo/bin` (or wherever
`cargo install` placed it) must be on the desktop session's `PATH`. If it is
not, replace the `Exec=` and `TryExec=` lines with the absolute binary path.

## Isolation model

`display_start` builds a scrubbed environment for every nested process:

- `DISPLAY` points at the new nested X server; on Wayland it is absent unless
  `xwayland=true`;
- `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR` point at the private compositor for
  `backend="wayland"`; host `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`, and the
  host runtime directory are removed or replaced;
- `XDG_RUNTIME_DIR` is a fresh directory under `/run/user/<uid>/disposable-desktop-mcp/`;
- a private `dbus-daemon --session` and an AT-SPI bus are started;
- Xephyr/Xvfb and openbox, or sway, plus the bus daemons and launched
  applications, are placed in their own process groups.

`display_stop` terminates the groups, then verifies no processes remain. If the
server dies unexpectedly, the next start finds the stale state file, inspects
`/proc/<pid>/environ` to confirm ownership, and reaps the old runtime.

State lives in `$XDG_RUNTIME_DIR/disposable-desktop-mcp/` (sessions are in `sessions/` and
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

- Linux only. The X11 backend needs Xephyr or Xvfb; the Wayland backend needs
  sway and the wlroots protocols listed above. The viewer still needs a host
  X11/XWayland display.
- The viewer forwards input only after explicit human control. Headless is the
  default, but if a session is started with `visible=true`, host input can reach
  a visible Xephyr window directly. A nested Wayland session can park its host
  devices on a `locked` seat instead.
- Wayland windows are moved/resized only while floating; tiled windows return an
  error. Per-window capture never includes the compositor cursor.
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

The build links against `libxkbcommon` (for the Wayland keymap). Install the
development package if it is missing, for example `libxkbcommon-dev` on Debian
or Ubuntu. The compositor itself comes from the system or from
`./scripts/fetch-runtime.sh --wayland`.

The viewer's capture loop is a self-rescheduling low-priority GLib source: a
slow `GetImage` tick can otherwise keep the main loop busy and starve GTK's
redraw source, which looks like a frozen or flickering window.

## License

MIT. See [LICENSE](LICENSE).
