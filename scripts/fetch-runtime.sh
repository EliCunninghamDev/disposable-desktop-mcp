#!/usr/bin/env bash
# Fetch the desktop runtime this project needs without root.
#
# Xephyr, Xvfb, and (optionally) sway are separate Arch packages, and openbox
# needs imlib2.  All of their shared-library dependencies are already part of a
# normal desktop install, so extracting the packages into ./.runtime is enough
# to run them.
#
#   ./scripts/fetch-runtime.sh             # install into ./.runtime
#   ./scripts/fetch-runtime.sh --wayland   # also fetch sway + test clients
#   ./scripts/fetch-runtime.sh --dest DIR  # install somewhere else
#
# The server and viewer look for this directory automatically.  You can also
# use a system-wide install (pacman -S xorg-server-xephyr xorg-server-xvfb
# openbox sway grim wtype) and ignore this script entirely.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$REPO/.runtime"
MIRROR="${XEPHYR_MCP_MIRROR:-https://geo.mirror.pkgbuild.com}"

PACKAGES=(
  xorg-server-xephyr
  xorg-server-xvfb
  xorg-server-common
  openbox
  imlib2
)

# Only needed by the Wayland backend and its smoke test.  wlroots is packaged
# under a versioned name; override if Arch moves to a newer ABI.
WAYLAND=false
WLROOTS_PKG="${XEPHYR_MCP_WLROOTS_PKG:-wlroots0.20}"
WAYLAND_PACKAGES=(
  sway
  "$WLROOTS_PKG"
  libliftoff
  grim
  wtype
  swaybg
)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --wayland) WAYLAND=true; shift ;;
    --dest) DEST="$2"; shift 2 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

if $WAYLAND; then
  PACKAGES+=("${WAYLAND_PACKAGES[@]}")
fi

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) REPO_DIR="x86_64" ;;
  *) echo "error: only x86_64 Arch packages are handled (got $ARCH)" >&2; exit 1 ;;
esac

CACHE="$DEST/.packages"
mkdir -p "$DEST" "$CACHE"

echo "mirror:  $MIRROR"
echo "dest:    $DEST"
echo

listing="$(curl -fsSL --retry 3 --max-time 60 "$MIRROR/extra/os/$REPO_DIR/")"

for pkg in "${PACKAGES[@]}"; do
  file="$(grep -oE "${pkg}-[0-9][^\"<]*\.pkg\.tar\.zst" <<<"$listing" | sort -u | tail -n1)"
  if [[ -z "$file" ]]; then
    echo "error: could not find $pkg in $MIRROR/extra/os/$REPO_DIR/" >&2
    exit 1
  fi

  if [[ -f "$CACHE/$file" ]]; then
    echo "cached   $file"
  else
    echo "download $file"
    curl -fL --retry 3 --max-time 300 -o "$CACHE/$file.part" \
      "$MIRROR/extra/os/$REPO_DIR/$file"
    mv "$CACHE/$file.part" "$CACHE/$file"
  fi

  # Packages are filesystem trees (usr/bin/..., usr/lib/...).  Extract them
  # side by side; packages that share directories are merged, which is exactly
  # how a normal install behaves.
  bsdtar -xf "$CACHE/$file" -C "$DEST" \
    --exclude '.BUILDINFO' --exclude '.MTREE' --exclude '.PKGINFO' --exclude '.INSTALL'
done

# bin/ makes the runtime explicit on PATH; the libraries stay in usr/lib and
# are only added to LD_LIBRARY_PATH for bundled helpers that need them.
mkdir -p "$DEST/bin"
for exe in Xephyr Xvfb openbox sway swaymsg grim wtype swaybg; do
  if [[ -x "$DEST/usr/bin/$exe" && ! -e "$DEST/bin/$exe" ]]; then
    ln -s "../usr/bin/$exe" "$DEST/bin/$exe"
  fi
done

echo
for exe in "$DEST/bin/Xephyr" "$DEST/bin/Xvfb" "$DEST/bin/openbox"; do
  if [[ -x "$exe" ]]; then
    echo "ok       $exe"
  else
    echo "missing  $exe" >&2
  fi
done

if $WAYLAND; then
  for exe in "$DEST/bin/sway" "$DEST/bin/swaymsg" "$DEST/bin/grim" \
             "$DEST/bin/wtype"; do
    if [[ -x "$exe" ]]; then
      echo "ok       $exe"
    else
      echo "missing  $exe" >&2
    fi
  done
fi

echo
echo "Runtime ready.  xephyr-mcp finds it automatically; add"
echo "  export PATH=\"$DEST/bin:\$PATH\""
echo "to your shell if you want the binaries on PATH yourself."
