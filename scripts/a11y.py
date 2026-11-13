#!/usr/bin/env python3
"""AT-SPI helper for xephyr-mcp.

Reads the accessibility tree, or activates an accessible by name, and prints a
single JSON object on stdout.  Warnings from GTK/AT-SPI go to stderr and are
ignored by the server.
"""

import json
import re
import shutil
import subprocess
import sys
import time

TIMEOUT_SECONDS = 15.0


def emit(obj):
    sys.stdout.write(json.dumps(obj))
    sys.stdout.write("\n")
    sys.stdout.flush()
    raise SystemExit(0)


def fail(message):
    emit({"ok": False, "error": str(message)})


try:
    import gi

    gi.require_version("Atspi", "2.0")
    from gi.repository import Atspi
except Exception as exc:  # pragma: no cover - environment-dependent
    fail(f"AT-SPI introspection unavailable: {exc}")

try:
    Atspi.set_timeout(2500, 2500)
except Exception:
    pass


def parse(argv):
    options = {}
    index = 0
    while index < len(argv):
        key = argv[index]
        if key.startswith("--"):
            name = key[2:].replace("-", "_")
            value = argv[index + 1] if index + 1 < len(argv) else ""
            options[name] = value
            index += 2
        else:
            index += 1
    return options


def safe(call, default=None):
    try:
        return call()
    except Exception:
        return default


def text(value):
    if value is None:
        return ""
    return str(value)


def extents(node):
    """Return [x, y, w, h] in screen coordinates, or None."""
    value = safe(lambda: node.get_extents(Atspi.CoordType.SCREEN))
    if value is None:
        return None
    # GI returns an Atspi.Rect object on current at-spi2-core; older bindings
    # returned a 4/5-tuple.
    for attr in ("x", "y", "width", "height"):
        if hasattr(value, attr):
            rect = [int(getattr(value, attr)) for attr in ("x", "y", "width", "height")]
            return rect if rect[2] > 0 and rect[3] > 0 else None
    if isinstance(value, (tuple, list)):
        if len(value) == 4:
            return [int(v) for v in value]
        if len(value) == 5:
            if not value[0]:
                return None
            return [int(v) for v in value[1:]]
    return None


def states(node):
    state_set = safe(lambda: node.get_state_set())
    if state_set is None:
        return {}
    out = {}
    for name in ("enabled", "focused", "visible", "showing", "sensitive"):
        constant = getattr(Atspi.StateType, name.upper(), None)
        out[name] = bool(constant is not None and safe(lambda: state_set.contains(constant), False))
    return out


def actions(node):
    count = safe(lambda: int(node.get_n_actions()), 0) or 0
    names = []
    for index in range(min(count, 8)):
        name = safe(lambda i=index: node.get_action_name(i), "")
        names.append(text(name))
    return names


def node_record(node, path, app_name, depth):
    flags = states(node)
    names = actions(node)
    rect = extents(node)
    toolkit = text(safe(lambda: node.get_toolkit_name(), ""))
    toolkit_version = text(safe(lambda: node.get_toolkit_version(), ""))
    role = text(safe(lambda: node.get_role_name(), ""))
    # GTK4 does not expose ENABLED, but SENSITIVE carries the useful meaning.
    enabled = flags.get("enabled", False) or flags.get("sensitive", False)
    # GTK4's AT-SPI backend reports (0, 0) for every non-toplevel widget, so
    # sizes are usable but positions are not.
    gtk4 = toolkit_version.startswith("4.") or toolkit == "GTK"
    precise = (not gtk4) or role.lower() in ("frame", "dialog", "window", "alert", "application")
    record = {
        "path": path,
        "app": app_name,
        "role": role,
        "name": text(safe(lambda: node.get_name(), "")),
        "description": text(safe(lambda: node.get_description(), "")),
        "toolkit": toolkit,
        "toolkit_version": toolkit_version,
        "geometry_precise": precise,
        "depth": depth,
        "window": "",
        "actions": names,
        "enabled": enabled,
        "focused": flags.get("focused", False),
        "visible": flags.get("visible", False),
        "showing": flags.get("showing", False),
        "sensitive": flags.get("sensitive", False),
    }
    if rect:
        record["x"], record["y"], record["width"], record["height"] = rect
    else:
        record["x"] = record["y"] = record["width"] = record["height"] = None
    return record


def children(node):
    count = safe(lambda: int(node.get_child_count()), 0) or 0
    # AT-SPI occasionally reports absurd counts while an app is starting.
    for index in range(min(count, 512)):
        child = safe(lambda i=index: node.get_child_at_index(i))
        if child is not None:
            yield child


def walk(desktop, options, visitor):
    started = time.monotonic()
    max_depth = int(options.get("max_depth", 12) or 12)
    max_nodes = int(options.get("max_nodes", 400) or 400)
    app_filter = (options.get("app") or "").strip().lower()
    app_count = getattr(desktop, "get_child_count", lambda: 0)()
    nodes = []

    for app_index in range(min(app_count, 128)):
        app = safe(lambda i=app_index: desktop.get_child_at_index(i))
        if app is None:
            continue
        app_name = text(safe(lambda: app.get_name(), ""))
        if app_filter and app_filter not in app_name.lower():
            continue
        stack = [(app, str(app_index), 0, "")]
        while stack:
            if time.monotonic() - started > TIMEOUT_SECONDS or len(nodes) >= max_nodes:
                return nodes, app_count
            node, path, depth, window_name = stack.pop()
            record = node_record(node, path, app_name, depth)
            next_window = window_name
            if depth >= 1 and record["role"].lower() in ("frame", "dialog", "window", "alert"):
                next_window = record["name"] or window_name
            record["window"] = next_window
            keep = bool(record["name"] or record["actions"] or record["focused"])
            keep = keep or record["role"] in ("application", "frame", "dialog", "window", "alert")
            if keep:
                if visitor is not None:
                    visitor(record)
                nodes.append(record)
            if depth >= max_depth:
                continue
            kids = list(children(node))
            for offset in range(len(kids) - 1, -1, -1):
                stack.append((kids[offset], f"{path}.{offset}", depth + 1, next_window))
    return nodes, app_count


def window_rect(name):
    """Ask the nested X server for the client window matching an accessible frame."""
    exe = shutil.which("xdotool")
    if not exe or not name:
        return None
    try:
        result = subprocess.run(
            [exe, "search", "--onlyvisible", "--name", "^" + re.escape(name) + "$",
             "getwindowgeometry", "--shell"],
            capture_output=True, text=True, timeout=3,
        )
    except Exception:
        return None
    best = None
    current = {}
    for line in result.stdout.splitlines() + [""]:
        line = line.strip()
        if not line:
            if {"X", "Y", "WIDTH", "HEIGHT"}.issubset(current):
                rect = (int(current["X"]), int(current["Y"]), int(current["WIDTH"]), int(current["HEIGHT"]))
                if rect[2] > 1 and rect[3] > 1 and best is None:
                    best = rect
            current = {}
            continue
        if "=" in line:
            key, value = line.split("=", 1)
            current[key] = value
    return best


def absolutize(nodes):
    """GTK4 reports window-local rectangles; convert them to screen space.

    GTK3 and Qt report screen coordinates already, so only apps whose frame
    rectangle is at (0, 0) with the X11 client's exact size are shifted.
    """
    frames = {}
    for node in nodes:
        window = node.get("window") or ""
        if window and node.get("x") is not None and node["role"].lower() in ("frame", "dialog", "window"):
            frames.setdefault(window, node)

    offsets = {}
    for window, frame in frames.items():
        rect = window_rect(window)
        if rect is None:
            continue
        wx, wy, ww, wh = rect
        local = (
            abs(frame["x"]) <= 1
            and abs(frame["y"]) <= 1
            and abs((frame.get("width") or 0) - ww) <= 2
            and abs((frame.get("height") or 0) - wh) <= 2
        )
        if local:
            offsets[window] = (wx, wy)

    if not offsets:
        return
    for node in nodes:
        offset = offsets.get(node.get("window") or "")
        if offset and node.get("x") is not None:
            node["x"] += offset[0]
            node["y"] += offset[1]


ACTIONABLE_ROLES = {
    "button", "push button", "toggle button", "menu item", "check box",
    "radio button", "tab", "link", "combo box", "entry", "text", "spin button",
    "slider", "menu", "list item", "tree item",
}


def main():
    if len(sys.argv) < 2:
        fail("usage: a11y.py tree|click [options]")
    command = sys.argv[1]
    options = parse(sys.argv[2:])

    desktop = safe(lambda: Atspi.get_desktop(0))
    if desktop is None:
        fail("AT-SPI desktop is not available (is the accessibility bus running?)")

    if command == "tree":
        nodes, app_count = walk(desktop, options, None)
        absolutize(nodes)
        emit({"ok": True, "apps": app_count, "count": len(nodes), "nodes": nodes})

    if command == "click":
        wanted = (options.get("name") or "").strip()
        if not wanted:
            fail("click needs --name")
        wanted_lower = wanted.lower()
        role_filter = (options.get("role") or "").strip().lower()
        chosen = []

        def visit(record):
            if record["role"].lower() in ("application", "frame") and not record["name"]:
                return
            name_lower = record["name"].lower()
            if not name_lower:
                return
            if name_lower != wanted_lower and wanted_lower not in name_lower:
                return
            if role_filter:
                role = record["role"].lower()
                if role_filter not in role and role not in role_filter:
                    return
            clickish = any(
                any(word in action.lower() for word in ("click", "press", "activate", "toggle"))
                for action in record["actions"]
            )
            role_lower = record["role"].lower()
            score = 0
            if name_lower == wanted_lower:
                score += 120
            if clickish:
                score += 90
            if role_lower in ACTIONABLE_ROLES:
                score += 60
            if role_lower in ("label", "static text", "text"):
                score -= 40
            if record["actions"]:
                score += 10
            if record["enabled"]:
                score += 8
            if record["showing"] or record["visible"]:
                score += 4
            if record["width"] and record["height"]:
                score += 6
            score += record["depth"]
            chosen.append((score, record))

        nodes, app_count = walk(desktop, options, visit)
        absolutize(nodes)
        if not chosen:
            emit({
                "ok": True,
                "found": False,
                "query": wanted,
                "apps": app_count,
                "error": f"no accessible named {wanted!r}",
            })

        chosen.sort(key=lambda item: item[0], reverse=True)
        index = int(options.get("index", 0) or 0)
        index = max(0, min(index, len(chosen) - 1))
        record = chosen[index][1]

        # Re-find the node by path so we can call do_action on the live object.
        target = None
        app_index_text, *rest = record["path"].split(".")
        app = safe(lambda: desktop.get_child_at_index(int(app_index_text)))
        if app is not None:
            target = app
            for part in rest:
                target = safe(lambda i=int(part): target.get_child_at_index(i))
                if target is None:
                    break
        if target is None:
            emit({"ok": True, "found": True, "did_action": False, "node": record})

        action_index = int(options.get("action", 0) or 0)
        action_count = safe(lambda: int(target.get_n_actions()), 0) or 0
        did_action = False
        role_lower = record["role"].lower()
        if action_count > 0:
            names = [safe(lambda i=i: target.get_action_name(i), "") or "" for i in range(action_count)]
            preferred = None
            if "action" not in options:
                for i, action_name in enumerate(names):
                    lowered = str(action_name).lower()
                    if any(word in lowered for word in ("click", "press", "activate", "toggle")):
                        preferred = i
                        break
            if preferred is not None:
                action_index = preferred
            elif role_lower not in ACTIONABLE_ROLES:
                # A label with clipboard actions: do not invoke clipboard.copy.
                action_index = -1
            action_index = max(0, min(action_index, action_count - 1)) if action_index >= 0 else -1
            if action_index >= 0:
                did_action = bool(safe(lambda: target.do_action(action_index), False))

        emit({
            "ok": True,
            "found": True,
            "did_action": did_action,
            "action_index": action_index if action_count and action_index >= 0 else None,
            "action_count": action_count,
            "node": record,
            "candidates": [item[1]["path"] for item in chosen[:8]],
        })

    fail(f"unknown command: {command}")


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as exc:  # pragma: no cover
        fail(f"AT-SPI helper failed: {exc}")
