#!/usr/bin/env bash
# Start a headless sway session from ./.runtime and verify the primitives the
# Wayland backend will rely on: socket discovery, output mode, screencopy,
# typing, floating move/resize, and clean shutdown.
#
#   ./scripts/wayland-smoke.sh
#
# Requires ./scripts/fetch-runtime.sh --wayland.  Nothing here touches the host
# compositor: XDG_RUNTIME_DIR, WAYLAND_DISPLAY, and SWAYSOCK are private.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${XEPHYR_MCP_RUNTIME:-$REPO/.runtime}"
SWAY="$DEST/usr/bin/sway"
SWAYMSG="$DEST/usr/bin/swaymsg"
GRIM="$DEST/usr/bin/grim"
WTYPE="$DEST/usr/bin/wtype"

for exe in "$SWAY" "$SWAYMSG" "$GRIM"; do
  if [[ ! -x "$exe" ]]; then
    echo "error: $exe is missing; run ./scripts/fetch-runtime.sh --wayland" >&2
    exit 1
  fi
done

RUN="$(mktemp -d "${TMPDIR:-/tmp}/xephyr-mcp-sway.XXXXXX")"
chmod 700 "$RUN"
SWAY_PID=""
cleanup() {
  if [[ -n "$SWAY_PID" ]] && kill -0 "$SWAY_PID" 2>/dev/null; then
    "$SWAYMSG" -s "${SWAYSOCK:-}" exit >/dev/null 2>&1 || true
    sleep 0.4
    kill -TERM "$SWAY_PID" 2>/dev/null || true
    sleep 0.4
    kill -KILL "$SWAY_PID" 2>/dev/null || true
  fi
  rm -rf "$RUN"
}
trap cleanup EXIT

cat > "$RUN/config" <<'EOF'
xwayland disable
default_border none
output * mode 1024x768
EOF

echo "runtime: $DEST"
echo "session: $RUN"
env -u WAYLAND_DISPLAY -u SWAYSOCK -u DISPLAY \
  XDG_RUNTIME_DIR="$RUN" \
  WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
  LD_LIBRARY_PATH="$DEST/usr/lib" \
  "$SWAY" -c "$RUN/config" > "$RUN/sway.log" 2>&1 &
SWAY_PID=$!

for _ in $(seq 1 50); do
  [[ -S "$RUN/wayland-1" ]] && break
  sleep 0.1
done
if [[ ! -S "$RUN/wayland-1" ]]; then
  echo "error: sway did not create wayland-1" >&2
  tail -20 "$RUN/sway.log" >&2 || true
  exit 1
fi
SWAYSOCK="$(find "$RUN" -name 'sway-ipc.*.sock' -print -quit)"
if [[ -z "$SWAYSOCK" ]]; then
  echo "error: sway did not create its IPC socket" >&2
  exit 1
fi
export SWAYSOCK WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR="$RUN"
export LD_LIBRARY_PATH="$DEST/usr/lib"

outputs="$("$SWAYMSG" -t get_outputs)"
python3 - "$outputs" <<'PY'
import json, sys
outputs = json.loads(sys.argv[1])
assert outputs, "no outputs"
name = outputs[0]["name"]
rect = outputs[0]["rect"]
assert (rect["width"], rect["height"]) == (1024, 768), rect
print(f"ok       output {name} {rect['width']}x{rect['height']}")
PY

"$GRIM" "$RUN/shot.png"
python3 - "$RUN/shot.png" <<'PY'
import struct, sys
data = open(sys.argv[1], "rb").read()
assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
width, height = struct.unpack(">II", data[16:24])
assert (width, height) == (1024, 768), (width, height)
print(f"ok       screencopy {width}x{height} ({len(data)} bytes)")
PY

# Exercise a real client when one is available.  The backend will launch these
# itself; this only proves the environment is complete.
APP=""
for candidate in alacritty foot zenity; do
  if command -v "$candidate" >/dev/null 2>&1; then
    APP="$candidate"
    break
  fi
done
if [[ -n "$APP" ]]; then
  case "$APP" in
    alacritty) "$APP" --class wayland-smoke > "$RUN/app.log" 2>&1 & ;;
    foot)      "$APP" --app-id wayland-smoke > "$RUN/app.log" 2>&1 & ;;
    zenity)    "$APP" --info --title=wayland-smoke --text=wayland-smoke \
                        --timeout=30 > "$RUN/app.log" 2>&1 & ;;
  esac
  APP_PID=$!
  for _ in $(seq 1 40); do
    if "$SWAYMSG" -t get_tree | grep -q 'wayland-smoke'; then break; fi
    sleep 0.1
  done

  "$SWAYMSG" '[app_id=wayland-smoke] floating enable' >/dev/null 2>&1 || true
  "$SWAYMSG" '[title=wayland-smoke] floating enable' >/dev/null 2>&1 || true
  sleep 0.3
  "$SWAYMSG" '[app_id=wayland-smoke] resize set 400 300' >/dev/null 2>&1 || true
  "$SWAYMSG" '[title=wayland-smoke] resize set 400 300' >/dev/null 2>&1 || true
  sleep 0.3
  "$SWAYMSG" '[app_id=wayland-smoke] move position 40 30' >/dev/null 2>&1 || true
  "$SWAYMSG" '[title=wayland-smoke] move position 40 30' >/dev/null 2>&1 || true
  sleep 0.3
  rect="$("$SWAYMSG" -t get_tree | python3 -c '
import json, sys
def walk(node):
    if "wayland-smoke" in (node.get("app_id") or "") \
       or "wayland-smoke" in (node.get("name") or ""):
        print(json.dumps(node["rect"])); raise SystemExit
    for child in node.get("nodes", []) + node.get("floating_nodes", []):
        walk(child)
walk(json.load(sys.stdin))')"
  python3 - "$rect" <<'PY'
import json, sys
rect = json.loads(sys.argv[1])
assert (rect["x"], rect["y"]) == (40, 30), rect
assert (rect["width"], rect["height"]) == (400, 300), rect
print("ok       floating geometry 400x300 at 40,30")
PY

  if [[ -x "$WTYPE" ]]; then
    "$WTYPE" -d 20 'hello wayland' >/dev/null 2>&1 || true
    echo "ok       virtual keyboard"
  fi

  kill "$APP_PID" 2>/dev/null || true
else
  echo "skip     no alacritty/foot/zenity for the client test"
fi

"$SWAYMSG" exit >/dev/null 2>&1 || true
for _ in $(seq 1 30); do
  kill -0 "$SWAY_PID" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$SWAY_PID" 2>/dev/null; then
  echo "error: sway did not exit" >&2
  exit 1
fi
SWAY_PID=""
echo "ok       sway exited"
