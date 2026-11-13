# Wayland backend

Status: design, validated against sway 1.12 + wlroots 0.20 (2026-11-13).

The X11 backend stays the default. This document describes the seam that lets a
Wayland session exist beside it, the protocol mapping, and the parts of the
original sketch that the compositor does not actually support.

## Why a backend seam

Every tool currently reaches the display through a display string:

- `screenshot::capture(&display)`
- `windows::list(&display)`
- `input::*` (xdotool on `DISPLAY`)
- `clipboard::{get,set}(&display)`
- `randr::resize(&display, ...)`

Those are X11 implementation details leaking into `tools.rs`. The seam should
move them behind a per-session object, keep the MCP tool names and schemas
unchanged, and make weak backends return a capability error instead of failing
inside `x11rb`.

## Trait

The sketched trait is close. Two refinements:

1. `start`/`stop` belong to a launcher; the runtime object should not own
   lifecycle. `DisplayManager` already serializes start/stop and writes state.
2. Capture/input/windows are blocking. Keep them sync and call them from
   `spawn_blocking`, as `tools.rs` does today, instead of making every method
   `async` and holding a runtime across wayland event dispatch.

```rust
pub trait Backend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn capabilities(&self) -> Capabilities;
    fn child_env(&self, h: &Handle, base: &[(String, String)]) -> Vec<(String, String)>;
    fn capture(&self, h: &Handle, target: CaptureTarget) -> Result<Frame>;
    fn pointer(&self, h: &Handle, ev: PointerEvent) -> Result<()>;
    fn keyboard(&self, h: &Handle, ev: KeyEvent) -> Result<()>;
    fn windows(&self, h: &Handle) -> Result<Vec<WindowInfo>>;
    fn window_op(&self, h: &Handle, id: &WinId, op: WindowOp) -> Result<()>;
    fn resize(&self, h: &Handle, width: u32, height: u32) -> Result<()>;
    fn clipboard_get(&self, h: &Handle) -> Result<String>;
    fn clipboard_set(&self, h: &Handle, text: &str) -> Result<usize>;
    fn markers(&self, h: &Handle) -> Vec<Marker>;
}

pub trait Launcher: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn probe(&self) -> Result<Probe>;                 // tools, protocols, host surface
    async fn start(&self, spec: &DisplaySpec) -> Result<Started>;
    async fn stop(&self, started: &Started) -> Result<()>;
}
```

`WinId` should be opaque (`x11:0x400003` / `wl:<toplevel-uuid>`), not `u32`.
Keep accepting bare X11 ids for compatibility and echo the backend in responses.

```rust
pub struct Capabilities {
    pub capture_output: bool,
    pub capture_window: bool,
    pub pointer: bool,
    pub keyboard: bool,
    pub clipboard: bool,
    pub window_list: bool,
    pub window_activate: bool,
    pub window_close: bool,
    pub move_resize: MoveResize,      // Full | FloatingOnly | None
    pub resize_display: bool,
    pub precise_client_geometry: bool,
    pub visible_host_window: bool,
}
```

## Session model

Wayland sessions use the same isolation as X11: private
`XDG_RUNTIME_DIR`, private D-Bus, private AT-SPI, private `HOME` when requested,
and a compositor process group that `display_stop` reaps.

Differences:

| Concern | X11 | Wayland |
| --- | --- | --- |
| Headless | Xvfb | `sway` with `WLR_BACKENDS=headless`, `WLR_RENDERER=pixman` |
| Nested window | Xephyr | `sway` with `WLR_BACKENDS=wayland` (host Wayland) or `WLR_BACKENDS=x11` (host X11) |
| Socket | `/tmp/.X11-unix/X<n>` | `$XDG_RUNTIME_DIR/wayland-<n>` |
| Input | XTEST | `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1` |
| Window list | EWMH | `wlr-foreign-toplevel-management` + sway IPC |
| Geometry | EWMH/`GetGeometry` | sway IPC `get_tree` rects |
| Resize | RandR | `output <name> mode <w>x<h>` |
| Capture | `GetImage` | `wlr-screencopy` (or `ext-image-copy-capture`) |
| Clipboard | in-process selection owner | `ext/wlr-data-control` |

XWayland is optional. It gives X11-only test apps a `DISPLAY`, but input still
goes through the Wayland virtual devices, so XTEST is not used for Wayland
sessions even when XWayland is on.

## Verified protocol support

`wayland-info` against sway 1.12/wlroots 0.20 advertises:

```
ext_image_copy_capture_manager_v1
ext_foreign_toplevel_list_v1
ext_foreign_toplevel_image_capture_source_manager_v1
ext_data_control_manager_v1
zwlr_screencopy_manager_v1
zwlr_foreign_toplevel_manager_v1
zwlr_virtual_pointer_manager_v1
zwp_virtual_keyboard_manager_v1
zwlr_output_manager_v1
```

So the mapping is feasible. Prefer `wlr-screencopy` for the first Rust
implementation: it is simpler (shm buffer + `copy`/`ready`) and is in
`wayland-protocols-wlr`. Keep `ext-image-copy-capture` for a second pass and
for per-window capture via `ext_foreign_toplevel_image_capture_source`.

Nested mode was also verified under both host types: `WLR_BACKENDS=wayland`
brings up `WL-1`, and `WLR_BACKENDS=x11` on `DISPLAY=:0` brings up `X11-1`.
Both honored `output * mode 900x700` and captured with `grim`.

## Corrections to the sketch

These are measured, not theoretical.

### Sway refuses `wayland-0`

`sway/server.c` starts its socket loop at 1 ("Avoid using wayland-0 as display
socket"). Setting `WAYLAND_DISPLAY=wayland-0` is ignored; a fresh private
runtime directory yields `wayland-1`. Do not hardcode `wayland-0`; discover the
socket after start (and record it in state). labwc/libwayland would use
`wayland-0`, so the field must be data, not a constant.

### Nested mode needs the host socket path

The nested compositor has a private `XDG_RUNTIME_DIR`, but the host Wayland
socket is in the host runtime directory. libwayland accepts an absolute
`WAYLAND_DISPLAY`, so start sway with:

```
XDG_RUNTIME_DIR=<private>
WAYLAND_DISPLAY=/run/user/1000/wayland-1
WLR_BACKENDS=wayland
```

Sway then sets `WAYLAND_DISPLAY=wayland-1` for its children (inside the private
directory). Verified: the nested output comes up as `WL-1` at 1280x720.

If the host has no Wayland compositor, use `WLR_BACKENDS=x11` with the host
`DISPLAY`/`XAUTHORITY` instead. `visible=true` should pick the host surface type
from the environment and error clearly when neither exists. Host-surface mode
also accepts `WLR_RENDERER=gles2`; keep `pixman` for headless determinism.

### Output size must be set explicitly

Headless defaults to 1280x720. Put `output * mode <w>x<h>` in the generated
config before start; verified 1024x768. Nested mode also honors
`output WL-1 mode 800x600`, and the host window followed. `resize_display`
therefore works on Wayland without RandR.

### Move/resize only applies to floating windows

Tiled clients are arranged by the compositor and cannot be placed at arbitrary
coordinates. A tiled alacritty was 1280x720 at 0,0. After `floating enable`,
`move position 100 80` and `resize set 400 300` produced exactly those values.

Consequences:

- `Capabilities::move_resize` is `FloatingOnly` for sway.
- `move_resize_window` must either (a) require `floating=true`, or (b) opt into
  floating first and report that it changed layout. Default to an explicit
  error for tiled windows rather than silently reflowing the desktop.
- Issue `resize set` and `move position` as separate IPC commands. In one
  command list, sway computed the position against the new size: asking for
  `move position 100 80` with a 400x300 resize yielded `(300, 230)`. Separate
  commands with a short settle gave `(100, 80)`. `move absolute position`
  also works.

### Window list has two halves

`wlr-foreign-toplevel-management` gives app_id, title, state, activate, close.
It does not give geometry. Use sway IPC `get_tree` for rect, pid, floating,
workspace. Match by `(app_id, pid)`; titles are not unique.

`precise_client_geometry` is false: the compositor rect is exact, but clients
cannot report screen-absolute widget coordinates. AT-SPI under Wayland remains
window-relative/unreliable for GTK4, exactly as it is on X11 today; the
compositor origin from sway must be added to the a11y fallback.

### Clipboard is not covered by the X11 selection owner

Sway advertises both `ext_data_control_manager_v1` and
`zwlr_data_control_manager_v1`. Implement data-control for Wayland sessions.
With XWayland enabled, X11 and Wayland clipboards are separate unless bridged;
decide which one is authoritative (data-control) and document the split. This
is the one tool group missing from the original mapping.

### Per-window capture is possible, but not via grim

`grim` only captures outputs. `ext_foreign_toplevel_image_capture_source` is
present, so `screenshot window=...` can capture a single toplevel without
overlap problems. Until that is implemented, either crop the output using the
sway rect (wrong when occluded) or return `capture_window: false`.

### Input passthrough and the nested window

A nested sway/Xephyr window is a real host window and receives host input
directly. The server cannot intercept that path. Therefore:

- In **headless** mode, the viewer is the only input path, so a control lock can
  genuinely arbitrate: the viewer forwards events only in `human` mode, and the
  server refuses input tools in `human` mode.
- In **nested** mode, the host window keeps accepting input regardless of the
  lock. The lock only prevents the viewer from injecting and the agent from
  calling tools; a human at the host window is still unarbitrated. If real
  arbitration is required, use headless + viewer, or accept the same advisory
  model Xephyr has today.

Do not use ydotool/uinput in either mode. uinput enters the host input stack and
breaks the isolation invariant. The virtual protocols are scoped to the private
compositor.

### Cursor capture

`wlr-screencopy` can include the compositor cursor on the output copy; verify
whether sway 1.12 honors the overlay-cursor flag under the pixman renderer. If
not, draw the cursor from the last known pointer position (the server already
tracks it) instead of XFixes. Do not assume `GetCursorImage` has an analog.

### Stale cleanup

`state::process_is_nested_display` currently recognizes only `Xephyr`/`Xvfb`.
The environment sweep still catches sway children, but add `sway`/`labwc` to the
command-line check so the state file path is accurate after a crash.
`display_stop` should try `swaymsg exit` first for graceful client teardown,
then TERM/KILL the group and sweep the runtime directory.

## Launch environment

Wayland sessions:

```
XDG_SESSION_TYPE=wayland
WAYLAND_DISPLAY=<nested socket name>
XDG_RUNTIME_DIR=<private>
GDK_BACKEND=wayland,x11
QT_QPA_PLATFORM=wayland;xcb
SDL_VIDEODRIVER=wayland,x11
MOZ_ENABLE_WAYLAND=1
CLUTTER_BACKEND=wayland
GTK_A11Y=atspi
QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1
DISPLAY=<nested XWayland display>   # only when xwayland=true
```

Remove host `WAYLAND_DISPLAY`, `DISPLAY`, `SWAYSOCK`, `HYPRLAND_INSTANCE_SIGNATURE`,
and `I3SOCK` before applying these.

## State and viewer

Add to `State` (serde defaults preserve old files):

```rust
backend: String,              // "x11" | "wayland"
wayland_display: Option<String>,
sway_socket: Option<String>,
control: String,              // "agent" | "human"
control_pid: Option<u32>,
control_since: Option<u64>,
```

Keep `display` for the human-readable name (`:100` or `wayland-1`). The viewer
uses the state file, not the X11 root marker. Also write a marker file next to
the socket (`$XDG_RUNTIME_DIR/xephyr-mcp-session.json`) for discovery when the
shared state directory is unavailable. Aspect-lock hints stay best-effort.

## Phases

1. **Seam.** Move capture/input/windows/clipboard/resize behind `Backend`;
   X11 implementation is a mechanical wrapper. Add `capabilities()` and
   `backend` to session JSON. No behavior change; existing tests and the 35-tool
   catalog stay identical.
2. **Wayland lifecycle + capture.** Sway discovery/runtime, private bus and
   AT-SPI, generated config, `output * mode`, socket discovery, state/marker,
   `display_stop` via `swaymsg exit` + group sweep. Screencopy capture with the
   existing fit/resize/PNG code. `display_start backend=wayland` (headless
   first), `resize_display` via output mode.
3. **Wayland windows and input.** Sway IPC `get_tree`, foreign-toplevel list,
   activate/close, floating-only move/resize, virtual pointer and keyboard,
   data-control clipboard. Add the `xwayland` option and launch env.
4. **Viewer + control.** Screencopy client, state-based discovery, `c` toggle,
   `control` in state, server-side input refusal, `wait_for_control`, no trace
   screenshots during human control, optional blank/redacted capture. Document
   the nested-window limitation.
5. **Polish.** Nested mode on both host types, per-window capture via
   `ext-foreign-toplevel-image-capture-source`, labwc support or an explicit
   capability error, packaging, docs, smoke tests in CI.

## Acceptance tests

- Headless Wayland: start 1024x768, launch a GTK app, capture, click changes
  the frame hash, type text, list the window, move/resize it while floating,
  resize the display, `display_stop` leaves no processes or sockets.
- Nested Wayland: host window appears at the requested size, resize follows,
  human input reaches the nested app, `display_stop` reaps everything.
- Isolation: child env has no host `WAYLAND_DISPLAY`/`DISPLAY`/`SWAYSOCK`;
  no `/dev/uinput` access; private bus and AT-SPI only.
- XWayland: X11-only app gets the nested `DISPLAY`; killing the session removes
  Xwayland and its socket.
- Clipboard: Wayland-native roundtrip; XWayland roundtrip; no host clipboard
  read/write.
- A11y: tree/find/action on a GTK app; coordinate fallback adds the compositor
  origin and reports `geometry_precise: false`.
- Regression: all existing X11 tools and response shapes unchanged.
