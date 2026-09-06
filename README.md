<div align="center">

# computer-use-hyprland

An MCP server that lets an agent drive a Hyprland desktop: read the
accessibility tree, take screenshots, target windows, and send input.

[![CI](https://img.shields.io/github/actions/workflow/status/thelipe7/computer-use-hyprland/ci.yml?branch=main&label=CI&style=flat-square)](https://github.com/thelipe7/computer-use-hyprland/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/computer-use-hyprland?style=flat-square)](https://crates.io/crates/computer-use-hyprland)
[![MSRV](https://img.shields.io/badge/rust-1.98.1-blue?style=flat-square)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)

</div>

## What it targets

Hyprland on Wayland, and only that. Not as a limitation to be lifted later —
it is the design. Windows go through `hyprctl` because that is the interface
Hyprland exposes, and input is synthesized through `uinput` because Hyprland's
portal implements no RemoteDesktop interface for anyone to ask.

A server that covered every Linux desktop would offer the intersection of what
all of them can do. This one takes the union of what one can, which is why it
can move a window to an exact pixel, and tell you precisely why a tiled one
refuses.

| Layer | Implementation |
|---|---|
| Windows | `hyprctl` — list, focus, move, resize, float, occlusion, pointer position |
| Accessibility | AT-SPI over the session's a11y bus |
| Screenshots | XDG Desktop Portal `Screenshot`, which needs `grim` on the portal's `PATH` |
| Pointer | A `uinput` absolute pointer device this server creates |
| Literal text | `wtype`, the Wayland virtual-keyboard protocol |
| Keys and chords | `ydotool` through a connectable `ydotoold` socket |

### What has to be there

`doctor` reports each of these, and says which one is missing when input or
capture fails.

- **Hyprland 0.55 or newer.** That is where the Lua dispatchers landed and
  the string ones stopped parsing, and every window action here — focus, move,
  resize, float — is dispatched in the Lua form. `doctor` names the release it
  found and says so when it is too old.
- **`/dev/uinput`, readable and writable by your user.** Both the pointer this
  server creates and `ydotoold` open it. On most distributions that is a
  `uaccess` rule or membership of the `input` group.
- **`ydotoold` running in the user session**, with a socket this process can
  connect to. It sends the keys and chords.
- **`wtype` on `PATH`**, for literal text. Without it, text falls back to
  ydotool's scancodes, which are re-interpreted by the keyboard layout and
  mangle anything that is not plain ASCII.
- **`grim` on the *portal's* `PATH`**, which is not necessarily your shell's.
  Without it the portal answers a screenshot with response 2.

## Install

From the registry:

```bash
cargo install computer-use-hyprland
```

Or from a clone, which is what you want if you are changing it:

```bash
git clone https://github.com/thelipe7/computer-use-hyprland
cd computer-use-hyprland
cargo install --path .
```

The pinned toolchain in `rust-toolchain.toml` installs itself on the first
`cargo` command. An MCP client's server process does not reload the binary, so
restart the client after installing over a running one.

Then ask the machine what it can do:

```bash
computer-use-hyprland doctor
```

An empty `readiness.blockers` means it is ready.

## Point a client at it

`mcp` is the subcommand an MCP client spawns:

```json
{
  "mcpServers": {
    "computer-use-hyprland": {
      "command": "computer-use-hyprland",
      "args": ["mcp"]
    }
  }
}
```

### Or install it as a Claude Code plugin

This repository is also a Claude Code marketplace carrying one plugin. It
registers the server for you and installs a skill that says how to drive it —
the order the tools go in, and the four failures that look like bugs.

```text
/plugin marketplace add thelipe7/computer-use-hyprland
/plugin install computer-use-hyprland@computer-use-hyprland
```

The plugin spawns `computer-use-hyprland mcp`, so install the binary first.

The other subcommands drive the same machinery by hand: `doctor`, `setup`,
`apps`, `state [APP_NAME]`, `screenshot`, `windows`, and `abs-test X Y`, which
clicks that desktop coordinate through the uinput pointer and prints where it
actually landed after clamping — the fastest way to tell a coordinate problem
from an input-backend one.

## Tools

**Reading** — `doctor`, `list_apps`, `list_windows`, `focused_window`,
`get_app_state`, `wait_for`, `screenshot`, `pointer_position`.

**Windows** — `activate_window`, `move_window`, `resize_window`,
`set_window_floating`, `focus_workspace`, `move_window_to_workspace`.

**Starting an application** — `launch_app`, which opens it floating on an
empty workspace, so its geometry does not depend on what else was open.

**Input** — `click`, `drag`, `scroll`, `press_key`, `type_text`,
`perform_action`, `set_value`.

**Setup** — `setup_accessibility`, for when `doctor` says AT-SPI is off.

### Two things that are easy to get wrong

**Element indices die with their element.** An index keeps naming the same
element across re-reads of the tree and stops resolving once that element is
gone, so a stale one errors instead of acting on whatever took its place; call
`get_app_state` or `wait_for` again after a relaunch.

**Hyprland cannot give a tiled window an exact geometry.** `move_window` and
`resize_window` refuse one without dispatching anything: a pixel move is
ignored, and a pixel resize moves the layout split, resizing the neighbors
instead. Call `set_window_floating` with `floating: true`, do the move or
resize, then `floating: false` to put the layout back.

## Environment

| Variable | Effect |
|---|---|
| `COMPUTER_USE_HYPRLAND_ALLOWED_APPS` | Comma-separated `app_id`/`wm_class`/`title` patterns. Input tools refuse a window matching none of them. |
| `COMPUTER_USE_HYPRLAND_FORCE_YDOTOOL_KEYBOARD=1` | Skips `wtype` and sends literal text through ydotool. |
| `COMPUTER_USE_HYPRLAND_DISABLE_ABS_POINTER=1` | Skips the uinput absolute pointer, leaving ydotool for the pointer too. |
| `COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS` | Seconds without a call before a held input lock is given back. `30` unless set; `0` holds it until the process exits. |

## Notes

Only one process may hold the input lock at a time; a second server answers
`ok=false` naming the holder's pid. The lock is a lease: every call renews it,
and it is given back after `COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS` seconds
without one (30 unless set), so a session that stopped driving the desktop
without exiting stops blocking the next one after that long.

Electron applications expose no AT-SPI tree unless launched with
`--force-renderer-accessibility`.

A screenshot denied with response 2 usually means the Hyprland portal started
without `grim` on its `PATH`:

```bash
systemctl --user restart xdg-desktop-portal-hyprland.service xdg-desktop-portal.service
```

`doctor` opens one real connection to the accessibility bus rather than only
reading the properties that say it is configured, and prints the whole error
chain in `accessibility.at_spi_connect` when it fails. The two questions have
different answers: a single peer that refuses an interface query can abort the
connection while every property still reads back `true`.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) holds every rule this project has. Security
reports go through the process in [SECURITY.md](SECURITY.md), not through an
issue.

## License

MIT. See [LICENSE](LICENSE); this is a hard fork of
[agent-sh/computer-use-linux](https://github.com/agent-sh/computer-use-linux),
whose copyright notice is preserved there as the license requires.
