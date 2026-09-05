# computer-use-hyprland

An MCP server that lets an agent drive this desktop: read the accessibility
tree, take screenshots, target windows, and send input.

This is a hard fork of [agent-sh/computer-use-linux][upstream], narrowed to one
environment — **Hyprland on Wayland** — because that is the only one the
maintainer runs and the only one the code can honestly claim to work on. The
GNOME, KWin, COSMIC, i3 and X11 backends were removed rather than shipped
untested.

[upstream]: https://github.com/agent-sh/computer-use-linux

## What it runs on

| Layer | Implementation |
|---|---|
| Windows | `hyprctl` (list, focus, move, resize, float, occlusion, pointer position) |
| Accessibility | AT-SPI over the session's a11y bus |
| Screenshots | XDG Desktop Portal `Screenshot` (needs `grim` on the portal's `PATH`) |
| Pointer | A `uinput` absolute pointer device this server creates |
| Literal text | `wtype`, the Wayland virtual-keyboard protocol |
| Keys and chords | `ydotool` through a connectable `ydotoold` socket |

There is no RemoteDesktop portal on Hyprland, and this build does not look for
one. Everything goes through `uinput`.

## Install

```bash
cargo install --path . --force
```

The running server does not reload the binary, so restart the MCP client
session after reinstalling.

Then check the machine:

```bash
computer-use-hyprland doctor
```

`readiness.blockers` empty means it is ready. `mcp` is the subcommand an MCP
client launches; `setup`, `apps`, `state`, `screenshot` and `windows` are for
poking at the same machinery by hand. `abs-test X Y` clicks that desktop
coordinate through the uinput pointer and prints where it actually landed
after clamping, which is the fastest way to tell a coordinate problem from an
input-backend problem.

## Tools

**Reading:** `doctor`, `list_apps`, `list_windows`, `focused_window`,
`get_app_state`, `wait_for`, `screenshot`, `pointer_position`.

**Windows:** `activate_window`, `move_window`, `resize_window`,
`set_window_floating`.

**Input:** `click`, `drag`, `scroll`, `press_key`, `type_text`,
`perform_action`, `set_value`.

**Setup:** `setup_accessibility`, only when `doctor` says AT-SPI is off.

`run_shell` is registered only when `COMPUTER_USE_LINUX_ENABLE_SHELL=1`.

### Two things that are easy to get wrong

**Element indices die when the app restarts.** Call `get_app_state` or
`wait_for` again before using an index against a relaunched process.

**Hyprland cannot give a tiled window an exact geometry.** `move_window` and
`resize_window` refuse one without dispatching anything: a pixel move is
ignored, and a pixel resize moves the layout split, resizing the neighbours
instead. Call `set_window_floating` with `floating: true`, do the move or
resize, then `floating: false` to put the layout back.

## Environment

| Variable | Effect |
|---|---|
| `COMPUTER_USE_LINUX_ENABLE_SHELL=1` | Registers `run_shell`. Off by default; the command is not sandboxed. |
| `COMPUTER_USE_LINUX_ALLOWED_APPS` | Comma-separated `app_id`/`wm_class`/`title` patterns. Input tools refuse windows matching none of them. |
| `COMPUTER_USE_LINUX_SCREENSHOT_BACKEND` | Pins the screenshot backend instead of probing. |
| `COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD=1` | Skips `wtype` and sends literal text through ydotool. |
| `CU_DISABLE_ABS_POINTER=1` | Skips the uinput absolute pointer, leaving ydotool for the pointer too. |

## Notes

Only one process may hold the input lock at a time; a second server answers
`ok=false` naming the holder's pid.

Electron apps expose no AT-SPI tree unless launched with
`--force-renderer-accessibility`.

A screenshot denied with response 2 usually means the Hyprland portal started
without `grim` on its `PATH`:

```bash
systemctl --user restart xdg-desktop-portal-hyprland.service xdg-desktop-portal.service
```

## Licence

MIT. See [LICENSE](LICENSE); the upstream copyright notice is preserved there
as the licence requires.
