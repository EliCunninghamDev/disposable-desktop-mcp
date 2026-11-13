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
- **Trace and marks.** Every tool call is appended to a JSONL trace with
  before/after frame hashes, and screenshots can draw numbered accessibility
  targets for reliable `click(mark=N)` routing.
- **Live viewer.** A GTK4 window with a native session dropdown, Single/BSP
  layout buttons, aspect-ratio locking, and double-buffered painting.
- **Cleanup.** Child process groups, the bus daemons, and the display are
  terminated on `display_stop` or server exit. Stale state is recovered after a
  crash (`SIGKILL`) on the next start.

## Requirements

- Linux with X11 or XWayland (the viewer needs a host display).
- Rust 1.88+ (the MCP layer uses the official `rmcp` SDK).
- GTK 4 development files for building the viewer (`gtk4.pc`; package names
  vary: `gtk4`, `libgtk-4-dev`, `gtk4-devel`).
- `xdotool` for input.
- Optional: `python3` with PyGObject (`python-gobject` / `python3-gi`) for the
  AT-SPI tools.

The nested X servers, openbox, and their libraries are fetched at runtime, so
the host does not need Xephyr or Xvfb installed.

## Build

```sh
./scripts/fetch-runtime.sh   # extracts Xephyr/Xvfb/openbox into ./.runtime
cargo build --release
```

`XEPHYR_MCP_RUNTIME` can point at an existing runtime tree instead of
`./.runtime`.

## MCP configuration

Add the server to your MCP client. For OpenCode:

```json
{
  "mcp": {
    "servers": {
      "xephyr-mcp": {
        "type": "local",
        "command": ["/home/eli/code/xephyr-mcp/target/release/xephyr-mcp"]
      }
    }
  }
}
```

The server speaks MCP over stdio using the official
[`rmcp`](https://crates.io/crates/rmcp) Rust SDK, which handles framing,
protocol-version negotiation, cancellation, and the typed MCP model. Tool
definitions and handlers are in `src/tools.rs`; `src/mcp.rs` is the adapter.

## Tools

| Tool | Purpose |
| --- | --- |
| `display_start` | Start the nested desktop. Parameters: `width`, `height`, `visible` (Xephyr vs Xvfb), `session_id`, `persist_home`. |
| `display_stop` | Stop the display and every child process it owns. |
| `launch` | Run a shell command inside the nested display (`command`, `wait_ms`). |
| `screenshot` | Capture PNG (`max_width`, `max_height`, `region`, `window_id`, `cursor`, `grid`, `marks`, `mark_limit`, `native`). |
| `wait_for` | Block on `window`, `process_exit`, `screen_stable`, `region_change`, or `file_exists`. |
| `wait_for_idle` | Block until the screen (or a region) stops changing. |
| `click` | Click at display coordinates or a screenshot mark (`x`, `y`, `mark`, `button`, `count`, `modifiers`). |
| `mouse_down` / `mouse_up` | Hold or release a mouse button (optional `x`/`y`). |
| `key_down` / `key_up` | Hold or release a key or modifier. |
| `move` | Move the nested pointer. |
| `drag` | Drag from `x1,y1` to `x2,y2` (`button`, `duration_ms`). |
| `scroll` | Wheel scroll (`direction`, `amount`, optional `x`,`y`). |
| `type` | Type text (`text`, `delay_ms`). |
| `key` | Send an xdotool key combo (`combo`), e.g. `ctrl+l`. |
| `clipboard_get` | Read the nested CLIPBOARD selection as text. |
| `clipboard_set` | Put text on the nested CLIPBOARD selection. |
| `list_windows` | List EWMH windows with geometry, class, title, pid, and stacking. |
| `focus_window` | Activate a window by `id`. |
| `move_resize_window` | Move/resize a window (`id`, `x`, `y`, `width`, `height`). |
| `close_window` | Ask a window to close (`id`, `force`). |
| `window_state` | Minimize, maximize, restore, fullscreen, raise/lower, always-on-top, or query state (`id`, `action`). |
| `resize_display` | Resize the nested desktop with RandR (`width`, `height`, optional `refresh`). |
| `trace_tail` | Read the JSONL tool trace (`session_id`, `lines`). |
| `list_processes` | List processes owned by the session (pid, ppid, pgid, command). |
| `kill_process` | Signal a session-owned process (`pid`, `signal`, `group`). |
| `tail_log` | Read the tail of a launch or session log (`file`, `lines`). |
| `a11y_tree` | Dump the AT-SPI tree, or one subtree with `path` (`max_depth`, `max_nodes`, `app`). |
| `a11y_find` | Query accessible nodes by `name`, `role`, `state`, `action`, and `path` without dumping the tree. |
| `a11y_click` | Activate an accessible by `name` or `path`, optional `role`/`index`. |
| `a11y_set_text` | Set an editable field's contents via the EditableText interface. |
| `a11y_focus` | Give keyboard focus to an accessible element. |
| `diagnose` | Report dependency paths/versions, host display, and active sessions. |

Coordinates are always in the nested display's pixel space.

Screenshots report `source_width`/`source_height`, `image_width`/`image_height`,
`origin_x`/`origin_y`, and `scale`. To convert a pixel measured on the returned
image back to display coordinates: `display = image / scale + origin`. Crops
(`region`, `window_id`) default to native resolution so small text stays legible.

`wait_for` and `wait_for_idle` replace screenshot polling:

```json
{"condition": "window", "title": "Save As", "timeout_ms": 5000}
{"condition": "process_exit", "pid": 12345}
{"condition": "region_change", "region": {"x": 0, "y": 0, "width": 400, "height": 300}}
{"condition": "screen_stable", "stable_ms": 400, "region": {"x": 0, "y": 0, "width": 900, "height": 600}}
{"condition": "file_exists", "path": "/tmp/export.csv"}
```

A timeout is returned as a successful result with `"timed_out": true` and
`"satisfied": false`, so an agent can inspect state or retry. Settling on a
region avoids waiting forever on clocks and animated backgrounds.

## Phase 2 controls

`click` accepts `count` for double/triple clicks and `modifiers` (for example
`["ctrl","shift"]`) held for the click. `mouse_down`/`mouse_up` and
`key_down`/`key_up` keep state across calls for drag-selection and games.
`window_state` applies EWMH state changes; `resize_display` switches Xephyr to
one of its built-in modes (Xvfb only offers its startup size and returns a clear
error).

With `screenshot marks=true`, actionable accessibility elements are numbered
and their centers are recorded. `click mark=N` targets that element; when it has
an accessibility action (buttons, menu items, check boxes), the action is
invoked directly, so it does not depend on pixel geometry. GTK4 still reports
unreliable child coordinates, so the drawn boxes are approximate there, but the
mark mapping and action routing are reliable. Use `a11y_click path=...` for the
same reason.

Every tool call is appended to
`$XDG_RUNTIME_DIR/xephyr-mcp/traces/<session_id>.jsonl` with the sanitized
arguments, duration, outcome, and before/after frame hashes for screen-changing
tools. Traces survive `display_stop`; read them with `trace_tail`. Set
`persist_home=true` on `display_start` to keep `HOME` and XDG directories under
`$XDG_RUNTIME_DIR/xephyr-mcp/homes/<session_id>` across restarts with the same
session id. Set `XEPHYR_MCP_IDLE_TIMEOUT_SECS` or
`XEPHYR_MCP_MAX_LIFETIME_SECS` to enable the watchdog, which tears down a
desktop when no tool call is active and the limit has elapsed.

The clipboard tools own the X `CLIPBOARD` selection directly (a background
thread answers `SelectionRequest`); the host clipboard is never used. Direct
selection transfer is limited to 1 MiB per set.

`list_processes` matches processes by their private `XDG_RUNTIME_DIR`, and
`kill_process` refuses PIDs that do not belong to the session, so it cannot
signal host processes. `tail_log` only reads inside the session log directory.

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
