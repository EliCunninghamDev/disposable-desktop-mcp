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
- **Pure-Rust screenshots.** `GetImage` + PNG encoding, downscaled by default
  to keep image tokens small.
- **Input through XTEST on the nested server.** Mouse moves/clicks/drags,
  scrolling, typing, and key combos never touch the host pointer.
- **EWMH window management.** List, focus, move/resize, and close windows
  directly over X11.
- **Accessibility via AT-SPI.** Walk the accessibility tree and click by
  accessible name as a fallback when pixel coordinates are awkward.
- **Live viewer.** A GTK4 window with a native session dropdown, Single/BSP
  layout buttons, aspect-ratio locking, and double-buffered painting.
- **Cleanup.** Child process groups, the bus daemons, and the display are
  terminated on `display_stop` or server exit. Stale state is recovered after a
  crash (`SIGKILL`) on the next start.

## Requirements

- Linux with X11 or XWayland (the viewer needs a host display).
- Rust 1.80+.
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

The server speaks MCP over stdio.

## Tools

| Tool | Purpose |
| --- | --- |
| `display_start` | Start the nested desktop. Parameters: `width`, `height`, `visible` (Xephyr vs Xvfb), `session_id`. |
| `display_stop` | Stop the display and every child process it owns. |
| `launch` | Run a shell command inside the nested display (`command`, `wait_ms`). |
| `screenshot` | Capture the nested root window as PNG (`max_width`, `max_height`). Returns the scale used. |
| `click` | Click at display coordinates (`x`, `y`, `button`, `count`). |
| `move` | Move the nested pointer. |
| `drag` | Drag from `x1,y1` to `x2,y2` (`button`, `duration_ms`). |
| `scroll` | Wheel scroll (`direction`, `amount`, optional `x`,`y`). |
| `type` | Type text (`text`, `delay_ms`). |
| `key` | Send an xdotool key combo (`combo`), e.g. `ctrl+l`. |
| `list_windows` | List EWMH windows with geometry, class, title, pid, and stacking. |
| `focus_window` | Activate a window by `id`. |
| `move_resize_window` | Move/resize a window (`id`, `x`, `y`, `width`, `height`). |
| `close_window` | Ask a window to close (`id`, `force`). |
| `a11y_tree` | Dump the AT-SPI tree (`max_depth`, `max_nodes`, `app`). |
| `a11y_click` | Click an accessible element by `name`, optional `role`/`index`. |

Coordinates are always in the nested display's pixel space.

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
cargo clippy --all-targets
cargo build --release
```

The viewer's capture loop is a self-rescheduling low-priority GLib source: a
slow `GetImage` tick can otherwise keep the main loop busy and starve GTK's
redraw source, which looks like a frozen or flickering window.

## License

MIT. See [LICENSE](LICENSE).
