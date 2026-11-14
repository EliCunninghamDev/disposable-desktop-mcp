# Wayland backend

Status: phase 3 implemented and validated against sway 1.12 + wlroots 0.20
(2026-11-13). The `Backend` seam, headless/nested sway lifecycle, screencopy
capture, sway-IPC window control, floating move/resize, output-mode resize,
virtual pointer/keyboard injection, and the data-control clipboard are live.

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
pub enum MoveResize { Any, FloatingOnly, None }
pub enum ClipboardPath { X11Selection, DataControl, None }
```

`WinId` should be opaque (`x11:0x400003` / `wl:<toplevel-uuid>`), not `u32`.
Keep accepting bare X11 ids for compatibility and echo the backend in responses.

```rust
pub struct Capabilities {
    pub move_resize: MoveResize,          // Any | FloatingOnly | None
    pub resize_display: bool,             // true on sway headless and nested
    pub capture_window: bool,             // true: foreign-toplevel + image-copy-capture
    pub clipboard: ClipboardPath,         // X11Selection | DataControl | None
    pub precise_geometry: bool,
    // Extra, for clearer tool errors and UI hints:
    pub capture_output: bool,
    pub pointer: bool,
    pub keyboard: bool,
    pub window_list: bool,
    pub window_activate: bool,
    pub window_close: bool,
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
| Capture | `GetImage` | outputs: `wlr-screencopy`; windows: `ext-image-copy-capture` |
| Clipboard | in-process selection owner | `ext/wlr-data-control` |

XWayland is optional and defaults to **disabled**. It creates a socket in
`/tmp/.X11-unix`, a host-global namespace, which weakens the isolation story;
enable it explicitly for X11-only test apps. Input still goes through the
Wayland virtual devices, so XTEST is not used for Wayland sessions even when
XWayland is on.

Xwayland's DISPLAY is not visible in `/proc/<sway-pid>/environ`: sway calls
`setenv` after start, and `/proc` still reports the initial environment. The
server asks sway to run a one-line helper (`exec sh -c 'printf %s "$DISPLAY"
> <runtime>/xwayland-display'`) and reads that file. This avoids guessing from
the host-global socket namespace. Verified with `xmessage`: the app received
`DISPLAY=:2`, appeared in `list_windows`, and its Xwayland exited with
`display_stop`.

### Generated sway config

```text
output * mode 1280x900
default_border none
for_window [app_id=".*"] floating enable
for_window [class=".*"] floating enable
xwayland disable
```

Floating-by-default matches the openbox model the agent already expects: real
positions, no reflow of other windows, and `move_resize_window` works on most
windows. The cost is that tiling is no longer the default. Dialogs are floating
anyway; without these rules most windows are tiled and cannot be placed. The
`class` rule covers XWayland clients when that is enabled. `default_border none`
also makes the compositor rect equal the client rect, which the a11y origin
calculation relies on.

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

So the mapping is feasible. Outputs use `wlr-screencopy` (shm buffer +
`copy`/`ready`). Windows use `ext-foreign-toplevel-list-v1` to resolve sway's
`foreign_toplevel_identifier`, `ext_foreign_toplevel_image_capture_source_v1`
to make a source for that handle, and `ext-image-copy-capture-v1` to copy it
into `wl_shm`.

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

- `Capabilities::move_resize` is `FloatingOnly` for sway. With the generated
  `for_window` rules most windows are floating; a genuinely tiled window still
  gets an explicit `not_supported` error rather than silently reflowing the
  desktop.
- Confirm instead of sleeping. `resize set` is only real after the client
  commits the new size, so poll `get_tree` (or watch the window event) until
  the rect matches or a timeout expires. Then send `move position`, and return
  both the requested and the actual rect. If the client clamps to a minimum
  size, the caller sees `requested` and `result` side by side.
- Issue `resize set` and `move position` as separate IPC commands. In one
  command list, sway computed the position against the new size: asking for
  `move position 100 80` with a 400x300 resize yielded `(300, 230)`. Separate
  commands with confirmation gave `(100, 80)`. `move absolute position` also
  works.

### Window list has two halves

`wlr-foreign-toplevel-management` gives app_id, title, state, activate, close.
It does not give geometry. Use sway IPC `get_tree` for rect, pid, floating,
workspace. Match by `(app_id, pid)`; titles are not unique.

`precise_client_geometry` is false: the compositor rect is exact, but clients
cannot report screen-absolute widget coordinates. AT-SPI under Wayland remains
window-relative/unreliable for GTK4, exactly as it is on X11 today. Request
extents window-relative and add `rect.x + window_rect.x` from sway's tree, not
just `rect.x`, because `rect` includes decorations. With `default_border none`
the two are equal, which is another reason to set it.

### Clipboard is not covered by the X11 selection owner

Sway advertises both `ext_data_control_manager_v1` and
`zwlr_data_control_manager_v1`. Implement data-control for Wayland sessions.
With XWayland enabled, data-control is authoritative; the X11 selection is
bridged only under the focus condition below.

The XWayland bridge was tested directly (sway 1.12/wlroots 0.20, xclip +
wl-copy, `xwayland force`):

- With no X11 window focused, the log says
  `not handling selection events: no seat assigned to xwayland`, and neither
  direction works. A headless session with no X11 app therefore has no bridge.
- With an X11 view focused (`xmessage`), both directions work:
  `wl-copy` → `xclip -o` returned the Wayland text, and `xclip -i` → `wl-paste`
  returned the X11 text.

The reason is in sway's `seat.c`: `wlr_xwayland_set_seat()` is called when an
X11 view gains focus, not at XWayland startup. Until that happens wlroots'
xwm has no seat and ignores selections. If `xwayland=true`, either warm the
bridge by focusing an X11 window once, or document that XWayland clipboard
interop starts with the first focused X11 app. Default `xwayland` to false, as
proposed; that keeps the clipboard path purely Wayland.

### Per-window capture

`grim` only captures outputs; `screenshot window_id=` uses the ext protocols
instead. `ext-foreign-toplevel-list-v1` is bound, its `stop`/`finished` handshake
collects the initial toplevel batch, and sway's `foreign_toplevel_identifier`
from `get_tree` selects the handle. The capture source then targets that handle,
so an overlapping window does not appear in the image.

The source returns the client surface, which for client-side-decorated toolkits
(GTK, Qt) includes symmetric shadow margins: a 300x263 zenity window captures as
344x307 with 22 px on each side. `list_windows` reports the window geometry, so
the backend center-crops the frame back to that geometry before returning it.
The origin therefore stays `(window.x, window.y)`, and marks/click coordinates
line up with the image. Clients without shadows (alacritty, XWayland) need no
crop. Cursors are not composited into window captures; the backend draws the
vendored Posy arrow from the last virtual-pointer position when it falls inside
the window.

### Input passthrough and the nested window

A nested sway/Xephyr window is a real host window and receives host input
directly. The server cannot intercept that path. The first idea — use
`swaymsg input <identifier> events disabled` — does **not** work: sway's
`input events` command returns early for any device that is not libinput
(`wlr_input_device_is_libinput()`), and the wayland-backend devices do not even
expose `send_events` in `get_inputs`. The command reports success and changes
nothing.

There is a working mechanism: move the backend-fed devices to a dummy seat.

```
swaymsg seat locked attach 0:0:wayland-pointer-seat0
swaymsg seat locked attach 0:0:wayland-keyboard-seat0
```

Measured in a two-layer setup (headless/nested A, nested B):

- Before: `seat0` had the pointer and keyboard; `get_inputs` reported
  `0:0:wayland-{pointer,keyboard}-seat0`.
- After both attaches: `get_seats` showed `locked` with both devices and
  `seat0` with `capabilities: 0` and `devices: []`.
- The virtual keyboard still typed into the focused client on `seat0`
  (`wtype` against B kept working), so the agent path is unaffected.
- Release is the inverse: `seat seat0 attach <identifier>`.

This is structural rather than advisory: once the host devices belong to a seat
with no focused output/client, their events cannot reach `seat0` clients. The
two-layer test could not observe host-fed key delivery in the first place (the
nested window did not accept the parent's virtual keyboard), so the block
itself was not observed end-to-end; the device-to-seat removal was. Verify with
a real host pointer/keyboard before calling it a hard guarantee.

Consequences:

- In **nested** mode the lock can be enforced by moving the wayland-backend
  devices to `locked` and back, with a re-scan for hotplugged identifiers.
  The viewer is not required for human input in this mode.
- In **headless** mode there are no backend-fed devices; the viewer is the only
  input path, so the viewer-side lock remains the mechanism.
- Still never use ydotool/uinput. uinput enters the host input stack and breaks
  the isolation invariant.
- The lock is advisory against a hostile same-user process, not a security
  boundary.

### Cursor capture

`wlr-screencopy` can include the compositor cursor on the output copy, and
`screenshot` asks for it by default; pass `cursor=false` to omit the marker.
Under the pixman renderer (notably headless sway) that flag is ignored, and
per-window capture never includes the compositor cursor, so the backend tracks
the last position sent through `zwlr_virtual_pointer_v1` and draws the vendored
Posy arrow there. The marker is absent until the pointer has been moved by
`move`, `click`, `mouse_down`/`mouse_up`, `drag`, or `scroll`.

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

## State and discovery

Treat the socket name as recorded data. Add to `State` (serde defaults preserve
old files):

```rust
backend: String,                    // "x11" | "wayland"
wayland_display: Option<String>,    // relative name, e.g. "wayland-1"
swaysock: Option<String>,           // absolute sway IPC socket
output_name: Option<String>,        // HEADLESS-1 / WL-1 / X11-1
capabilities: Option<serde_json::Value>,
control: String,                    // "agent" | "human"
control_pid: Option<u32>,
control_since: Option<u64>,
```

Keep `display` for the human-readable name (`:100` or `wayland-1`). The viewer
uses the state file, not the X11 root marker. Also write a marker file next to
the socket (`$XDG_RUNTIME_DIR/xephyr-mcp-session.json`) with the same JSON, for
discovery when the shared state directory is unavailable. Aspect-lock hints
stay best-effort.

Discovery is a glob in the private runtime directory: `wayland-*` (excluding
`.lock`) and `sway-ipc.*.sock`. Poll until (a) the Wayland socket accepts a
connection and (b) `get_version` answers over IPC. Sway has no readiness fd and
there is no `-displayfd` analog to build. Because the runtime directory is
private, there is no race with another session.

Child environment: the server sets `WAYLAND_DISPLAY=<recorded name>` with the
private `XDG_RUNTIME_DIR` and keeps its `env_clear()` model. Launch apps from
the server, not via `sway exec`, so the absolute host socket path and host
`DISPLAY` never reach app environments. Only the sway process itself sees the
host path (and host `DISPLAY`/`XAUTHORITY` for `WLR_BACKENDS=x11`).

## Control arbitration

Ownership lives in `$XDG_RUNTIME_DIR/xephyr-mcp/sessions/<id>.control.json`:

```json
{ "owner": "human", "pid": 1234, "since": 1794573677 }
```

The server reads it before every pointer, keyboard, or accessibility action and
returns an error naming the controlling PID and time. `wait_for_control` polls
the same file, so an agent can block until a viewer releases the session instead
of retrying. A claim whose PID is no longer alive is treated as `agent` and the
file is reset, which makes a crashed viewer harmless.

The viewer claims `human` for its own PID when `c`/Control is activated and
releases it on `Ctrl+Shift+C`, a second Control press, session switch, or window
close. It forwards mouse, wheel, and keyboard events only while the claim is
live; `Esc` and ordinary `c` presses are forwarded to the session rather than
being consumed as viewer shortcuts. Trace PNGs are
skipped while a person owns the session; frame hashes are still recorded.

For nested Wayland, host input devices (`wayland-pointer-seat0`,
`wayland-keyboard-seat0`) are parked on a `locked` seat while the agent works.
Sway's runtime `seat attach` is additive and is not persisted, so the viewer
rewrites the generated config's seat block and runs `swaymsg reload` when
control changes; the reload is what removes the devices from the other seat.
Headless sessions have no host devices and rely on viewer forwarding.

## Phases

1. **Seam.** ✅ `Backend` in `src/backend.rs` dispatches capture, windows,
   resize, clipboard, and capability errors. X11 is a mechanical wrapper;
   existing tests and the tool catalog are unchanged.
2. **Wayland lifecycle + capture.** ✅ Sway discovery/runtime, private bus and
   AT-SPI, generated config, `output * mode`, socket discovery, state/marker,
   `display_stop` via `swaymsg exit` + group sweep. Screencopy capture reuses
   the existing fit/resize/PNG code. `display_start backend=wayland` (headless
   and nested), `resize_display` via output mode, optional XWayland.
3. **Wayland windows and input.** ✅ Windows via sway IPC `get_tree`,
   activate/close, floating-only move/resize with confirmation polling. ✅
   Pointer and keyboard through `zwlr_virtual_pointer_v1` and
   `zwp_virtual_keyboard_v1`; Unicode text uses a generated keysym keymap per
   chunk, and `key_down`/`key_up` keep compositor state across calls. ✅
   Clipboard get/set through `zwlr_data_control_v1`; a per-session worker
   serves `send` requests and exits when the selection is replaced or the
   session stops.
4. **Viewer + control.** ✅ The GTK viewer reads `State` for both backends and
   captures Wayland outputs through the same screencopy client. ✅ `c`/the
   Control toggle claim human control in `<session>.control.json`; the server
   refuses pointer, keyboard, and accessibility actions and skips trace PNGs
   while a person owns the session. ✅ `wait_for_control` blocks until the
   wanted owner appears, and a claim whose PID is gone is treated as stale. ✅
   The viewer forwards mouse, wheel, and keyboard input (XTEST for X11,
   virtual devices for Wayland). Nested Wayland sessions park host input on a
   `locked` seat while the agent works, then rewrite the seat attachments and
   reload sway when human control starts; headless sessions use viewer
   forwarding. **Remaining:** optional blank/redacted capture.
5. **Polish.** ✅ Per-window capture via
   `ext-foreign-toplevel-image-capture-source` + `ext-image-copy-capture`,
   including overlap-free capture and geometry cropping. **Remaining:** labwc
   support (or an explicit capability error), packaging, and smoke tests in
   CI. Nested mode is already validated on both host surfaces.

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
- Clipboard: Wayland-native roundtrip; XWayland roundtrip after an X11 view is
  focused; no host clipboard read/write. Confirm that a headless session with
  no X11 window has no XWayland bridge (expected).
- Control lock: nested — attach the wayland devices to `locked`, verify host
  input stops and virtual input still works; detach and verify host input
  resumes. Headless — viewer forwards only in `human` mode and the server
  refuses input tools.
- A11y: tree/find/action on a GTK app; coordinate fallback adds the compositor
  origin and reports `geometry_precise: false`.
- Per-window capture: `screenshot window_id=<wl:id>` matches the sway rect
  exactly for native CSD and XWayland clients, excludes an overlapping window,
  keeps marks aligned, and returns a clear error after the window closes.
- Regression: all existing X11 tools and response shapes unchanged.
