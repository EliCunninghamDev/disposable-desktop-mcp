# Agent guidance

This repository is an MCP server plus a human-facing desktop viewer.

- Start desktops headless (`visible=false`, the default). Do not pass
  `visible=true` and do not launch `disposable-desktop-viewer` unless the user
  explicitly asks to watch or interact with a session.
- Verification must not open windows on the host desktop. Use headless sessions
  and a throwaway display if a GUI host is required, then clean up with
  `display_stop` or `kill_process all=true`.
- A running headless session can be shown to a person by attaching the viewer;
  it does not need to be restarted as `visible=true`.
