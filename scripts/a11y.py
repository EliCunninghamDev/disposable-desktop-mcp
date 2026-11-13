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
            if "=" in key:
                name, value = key[2:].split("=", 1)
                options[name.replace("-", "_")] = value
                index += 1
                continue
            name = key[2:].replace("-", "_")
            if index + 1 < len(argv) and not argv[index + 1].startswith("--"):
                options[name] = argv[index + 1]
                index += 2
            else:
                options[name] = "true"
                index += 1
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


def rect_from(value):
    # GI returns an Atspi.Rect object on current at-spi2-core; older bindings
    # returned a 4/5-tuple.
    if value is None:
        return None
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


def extents(node):
    """Return ([x, y, w, h], coordinate kind).

    GTK4 returns (0, 0) for SCREEN but meaningful window-local coordinates for
    WINDOW, so prefer WINDOW and let absolutize() add the X11 window origin.
    """
    rect = rect_from(safe(lambda: node.get_extents(Atspi.CoordType.WINDOW)))
    if rect:
        return rect, "window"
    rect = rect_from(safe(lambda: node.get_extents(Atspi.CoordType.SCREEN)))
    if rect:
        return rect, "screen"
    return None, None


def states(node):
    state_set = safe(lambda: node.get_state_set())
    if state_set is None:
        return {}
    out = {}
    for name in ("enabled", "focused", "visible", "showing", "sensitive", "checked", "selected"):
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
    rect, coord = extents(node)
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
        "coord": coord,
        "sensitive": flags.get("sensitive", False),
        "checked": flags.get("checked", False),
        "selected": flags.get("selected", False),
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

    root_path = (options.get("path") or "").strip()
    apps = []
    if root_path:
        root_node = resolve_target(desktop, {"path": root_path})
        if root_node is None:
            return nodes, app_count
        app_index = int(root_path.split(".")[0])
        app = safe(lambda: desktop.get_child_at_index(app_index))
        app_name = text(safe(lambda: app.get_name(), "")) if app is not None else ""
        apps = [(root_node, root_path, app_name)]
    else:
        for app_index in range(min(app_count, 128)):
            app = safe(lambda i=app_index: desktop.get_child_at_index(i))
            if app is None:
                continue
            app_name = text(safe(lambda: app.get_name(), ""))
            if app_filter and app_filter not in app_name.lower():
                continue
            apps.append((app, str(app_index), app_name))

    for app, start_path, app_name in apps:
        stack = [(app, start_path, 0, "")]
        while stack:
            if time.monotonic() - started > TIMEOUT_SECONDS or len(nodes) >= max_nodes:
                return nodes, app_count
            node, path, depth, window_name = stack.pop()
            record = node_record(node, path, app_name, depth)
            if record["role"].lower() in ("frame", "dialog", "window", "alert"):
                next_window = record["name"] or window_name
            else:
                next_window = window_name
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
    """Convert window-local accessibility rectangles to screen space.

    The window origin comes from xdotool, which reports the X11 client origin.
    Widgets with SCREEN coordinates are already absolute and are left alone.
    """
    frames = {}
    for node in nodes:
        window = node.get("window") or ""
        if window and node.get("x") is not None and node["role"].lower() in ("frame", "dialog", "window"):
            frames.setdefault(window, node)

    offsets = {}
    for window in frames:
        rect = window_rect(window)
        if rect is not None:
            offsets[window] = (rect[0], rect[1])
    if not offsets:
        return
    for node in nodes:
        if node.get("coord") != "window":
            continue
        offset = offsets.get(node.get("window") or "")
        if offset and node.get("x") is not None:
            node["x"] += offset[0]
            node["y"] += offset[1]


def resolve_target(desktop, record):
    """Re-find a live accessible object from a record's path."""
    app_index_text, *rest = record["path"].split(".")
    app = safe(lambda: desktop.get_child_at_index(int(app_index_text)))
    if app is None:
        return None
    target = app
    for part in rest:
        target = safe(lambda i=int(part): target.get_child_at_index(i))
        if target is None:
            break
    return target


def state_filter(record, wanted):
    """Match a comma-separated list of required boolean states."""
    if not wanted:
        return True
    for name in [part.strip().lower() for part in wanted.split(",") if part.strip()]:
        if name == "checked":
            if not record.get("checked", False):
                return False
        elif not record.get(name, False):
            return False
    return True


def action_filter(record, wanted):
    if not wanted:
        return True
    wanted = wanted.lower()
    return any(wanted in action.lower() for action in record.get("actions", []))


def collect_matches(desktop, options, wanted, role_filter, exact=False):
    """Score accessible nodes by name/role/state and return (matches, app_count)."""
    wanted_lower = (wanted or "").strip().lower()
    wanted_state = (options.get("state") or "").strip()
    wanted_action = (options.get("action") or "").strip()
    chosen = []

    def visit(record):
        if record["role"].lower() in ("application", "frame") and not record["name"]:
            return
        name_lower = record["name"].lower()
        if wanted_lower:
            if exact:
                if name_lower != wanted_lower:
                    return
            elif name_lower != wanted_lower and wanted_lower not in name_lower:
                return
        elif not (role_filter or wanted_state or wanted_action or (options.get("path") or "").strip()):
            return
        if role_filter:
            if not role_matches(record["role"], role_filter):
                return
        if not state_filter(record, wanted_state):
            return
        if not action_filter(record, wanted_action):
            return
        clickish = any(
            any(word in action.lower() for word in ("click", "press", "activate", "toggle"))
            for action in record["actions"]
        )
        role_lower = record["role"].lower()
        score = 0
        if wanted_lower and name_lower == wanted_lower:
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
    chosen.sort(key=lambda item: item[0], reverse=True)
    return chosen, app_count

ROLE_ALIASES = {
    "entry": ("text", "password text", "text field"),
    "textbox": ("text", "password text", "text field"),
    "text field": ("text", "password text"),
    "checkbox": ("check box",),
    "radio": ("radio button",),
}


def role_matches(role, wanted):
    role = (role or "").lower()
    wanted = (wanted or "").lower()
    if not wanted:
        return True
    if wanted in role or role in wanted:
        return True
    return any(alias in role for alias in ROLE_ALIASES.get(wanted, ()))


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
                if not role_matches(record["role"], role_filter):
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

    if command == "click_path":
        path = (options.get("path") or "").strip()
        if not path:
            fail("click_path needs --path")
        target = resolve_target(desktop, {"path": path})
        if target is None:
            emit({
                "ok": True,
                "found": False,
                "did_action": False,
                "path": path,
                "error": "the accessibility node no longer exists",
            })
        record = node_record(target, path, "", 0)
        action_count = safe(lambda: int(target.get_n_actions()), 0) or 0
        names = [text(safe(lambda i=i: target.get_action_name(i), "")) for i in range(action_count)]
        action_index = int(options.get("action", 0) or 0) if "action" in options else None
        if action_index is None:
            action_index = next(
                (
                    i
                    for i, action_name in enumerate(names)
                    if any(
                        word in action_name.lower()
                        for word in ("click", "press", "activate", "toggle")
                    )
                ),
                None,
            )
        did_action = False
        if action_index is not None and 0 <= action_index < action_count:
            did_action = bool(safe(lambda: target.do_action(action_index), False))
        emit({
            "ok": True,
            "found": True,
            "did_action": did_action,
            "action_index": action_index,
            "action_count": action_count,
            "actions": names,
            "method": "atspi",
            "node": record,
        })

    if command == "marks":
        limit = max(1, min(int(options.get("limit", 80) or 80), 200))
        options.setdefault("max_nodes", "1200")
        nodes, app_count = walk(desktop, options, None)
        absolutize(nodes)
        actionable = []
        seen = set()
        for record in nodes:
            role = record["role"].lower()
            if record.get("x") is None or record.get("width") is None:
                continue
            if record["width"] < 4 or record["height"] < 4:
                continue
            if record["x"] < 0 or record["y"] < 0:
                continue
            if record["role"] in ("application", "frame", "dialog", "window", "alert"):
                continue
            if role in ("label", "static text", "caption", "paragraph", "heading"):
                continue
            clickish = any(
                any(word in action.lower() for word in ("click", "press", "activate", "toggle"))
                for action in record["actions"]
            )
            editable = role in ("text", "password text", "text field") or "editable" in role
            if not (clickish or role in ACTIONABLE_ROLES or editable):
                continue
            if not (record["enabled"] or record["sensitive"]):
                continue
            key = (record["x"], record["y"], record["width"], record["height"], record["name"], role)
            if key in seen:
                continue
            seen.add(key)
            actionable.append(record)

        actionable.sort(key=lambda item: (
            (item.get("window") or "").lower(),
            item["y"],
            item["x"],
            item.get("depth", 0),
        ))
        marks = []
        for index, record in enumerate(actionable[:limit], start=1):
            item = dict(record)
            item["mark"] = index
            item["center_x"] = record["x"] + record["width"] // 2
            item["center_y"] = record["y"] + record["height"] // 2
            marks.append(item)
        emit({
            "ok": True,
            "apps": app_count,
            "count": len(marks),
            "total": len(actionable),
            "marks": marks,
        })

    if command == "find":
        wanted = (options.get("name") or "").strip()
        role_filter = (options.get("role") or "").strip().lower()
        if (
            not wanted
            and not role_filter
            and not (options.get("path") or "").strip()
            and not (options.get("state") or "").strip()
            and not (options.get("action") or "").strip()
        ):
            fail("find needs --name, --role, --state, --action, or --path")
        exact = (options.get("exact") or "").lower() in ("1", "true", "yes")
        chosen, app_count = collect_matches(desktop, options, wanted, role_filter, exact)
        limit = max(1, min(int(options.get("limit", 20) or 20), 100))
        nodes = [record for _, record in chosen[:limit]]
        emit({
            "ok": True,
            "found": bool(chosen),
            "query": wanted,
            "role": role_filter or None,
            "state": (options.get("state") or None),
            "action": (options.get("action") or None),
            "apps": app_count,
            "count": len(chosen),
            "nodes": nodes,
        })

    if command == "set_text":
        wanted = (options.get("name") or "").strip()
        role_filter = (options.get("role") or "").strip().lower()
        if not wanted:
            fail("set_text needs --name")
        chosen, app_count = collect_matches(desktop, options, wanted, role_filter)
        if not chosen:
            emit({
                "ok": True,
                "found": False,
                "changed": False,
                "query": wanted,
                "apps": app_count,
                "error": f"no accessible named {wanted!r}",
            })
        index = int(options.get("index", 0) or 0)
        index = max(0, min(index, len(chosen) - 1))
        record = chosen[index][1]
        target = resolve_target(desktop, record)
        value = options.get("text", "")
        editable = bool(safe(lambda: target.is_editable_text(), False)) if target else False
        focused = bool(record.get("focused"))
        if not focused and target is not None:
            focused = bool(safe(lambda: target.grab_focus(), False))
        changed = bool(safe(lambda: target.set_text_contents(value), False)) if target else False
        emit({
            "ok": True,
            "found": True,
            "changed": changed,
            "editable": editable,
            "focused": focused,
            "text": value,
            "node": record,
        })

    if command == "focus":
        wanted = (options.get("name") or "").strip()
        role_filter = (options.get("role") or "").strip().lower()
        if not wanted and not role_filter:
            fail("focus needs --name or --role")
        chosen, app_count = collect_matches(desktop, options, wanted, role_filter)
        if not chosen:
            emit({
                "ok": True,
                "found": False,
                "focused": False,
                "query": wanted,
                "apps": app_count,
                "error": f"no accessible named {wanted!r}",
            })
        index = int(options.get("index", 0) or 0)
        index = max(0, min(index, len(chosen) - 1))
        record = chosen[index][1]
        target = resolve_target(desktop, record)
        focused = bool(record.get("focused"))
        if not focused and target is not None:
            focused = bool(safe(lambda: target.grab_focus(), False))
        if not focused and target is not None:
            count = safe(lambda: int(target.get_n_actions()), 0) or 0
            for action_index in range(count):
                name = text(safe(lambda i=action_index: target.get_action_name(i), ""))
                if "focus" in name.lower():
                    focused = bool(safe(lambda i=action_index: target.do_action(i), False))
                    break
        emit({
            "ok": True,
            "found": True,
            "focused": focused,
            "node": record,
        })

    fail(f"unknown command: {command}")


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as exc:  # pragma: no cover
        fail(f"AT-SPI helper failed: {exc}")
