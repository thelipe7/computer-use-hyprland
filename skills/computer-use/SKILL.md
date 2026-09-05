---
name: computer-use
description: >
  Use when driving a desktop application on Hyprland through the
  computer-use-hyprland MCP server — reading an accessibility tree, finding and
  focusing a window, clicking, typing, pressing a chord, dragging, scrolling,
  taking a screenshot, or moving and resizing a window. Covers the order the
  tools go in, the one window-selector vocabulary they share, and the four
  failures that look like bugs and are not: dead element indices, the
  machine-wide input lock, a tiled window refusing an exact geometry, and a
  portal that cannot screenshot.
---

# Driving a Hyprland desktop

The server is `computer-use-hyprland`. Every tool it exposes is listed by the
client; this says how they fit together and where the surprises are.

## The loop

1. **`doctor`, once per session, when anything looks off.** It reports the
   compositor, the portal, the accessibility bus and each input backend.
   `readiness.blockers` empty means the machine is ready. Everything below
   assumes it is; a failure with a non-empty `blockers` is that, not the tool.
2. **Find the window.** `list_windows` or `focused_window` to see what is
   there, `activate_window` to focus one. Targeted input refuses to run if
   focus cannot be verified, so this step is not optional.
3. **Read before acting.** `get_app_state` returns the accessibility tree with
   an `element_index` on each node. That index is how everything else names an
   element.
4. **Act**: `click`, `type_text`, `press_key`, `drag`, `scroll`,
   `perform_action`, `set_value`.
5. **Verify.** `wait_for` blocks until a predicate holds, up to `timeout_ms`.
   Use it instead of sleeping — a sleep either wastes the time or is too short,
   and `wait_for` returns the tree as it is when the predicate holds.

## One selector vocabulary

Every window-targeted tool — `activate_window`, `get_app_state`, `wait_for`,
`screenshot`, `click`, `scroll`, `drag`, `press_key`, `type_text`,
`move_window`, `resize_window`, `set_window_floating` — takes the same nine
selectors, and any of them may be used on any of those tools:

`window_id`, `pid`, `app_id`, `wm_class`, `title`, and the four that resolve a
terminal: `tty`, `terminal_pid`, `terminal_command`, `terminal_cwd`.

`window_id` is exact and comes from `list_windows`; the rest match. Pass none
of them and the tool acts on whatever is focused.

**`window_title` is not a tenth selector.** It exists on `wait_for` alone and
means something else: the predicate, the substring the focused window's title
has to contain before the wait returns. The selector is always `title`.

## Naming an element

In order of how much they promise:

- **`element_index`**, from the most recent `get_app_state` or `wait_for`.
- **`object_ref` / `element_identifier`**, which survive a re-read of the tree.
- **A semantic selector** — `role`, `name`, `text`, `states` — when it matches
  exactly one node. More than one and the call refuses rather than guessing.
- **Coordinates** (`x`, `y`), for `click`, `scroll` and `drag`, in desktop
  pixels. `relative: true` reads them as an offset from the target window's
  origin instead. Screenshot metadata carries the scale to convert with.

`click` on an element that exposes an AT-SPI click action invokes the action
first and only falls back to the pointer; the result says which happened. That
is why clicking by index is more reliable than clicking a pixel — it does not
depend on the window being unobscured.

## The four things that look like bugs

**Element indices die when the application restarts.** They are positions in
one snapshot of one process's tree. After a relaunch, a crash, or anything
that replaces the process, call `get_app_state` or `wait_for` again before
using an index. An index used across a restart does not error — it points at
whatever now occupies that position.

**One process at a time holds the input lock.** It is machine-wide: two
servers driving one desktop would interleave their pointer and key events, so
the second answers `ok=false` naming the holder's pid. That is a refusal, not
a failure to work around. Stop and say who holds it.

**Hyprland cannot give a tiled window an exact geometry.** `move_window` and
`resize_window` refuse a tiled window before dispatching anything: a pixel move
is ignored outright, and a pixel resize moves the layout split, resizing the
neighbors instead of the target. The refusal names the way out —
`set_window_floating` with `floating: true`, then the move or resize, then
`floating: false` to put the layout back.

**A screenshot denied with response 2** is usually the Hyprland portal started
without `grim` on its `PATH`, not a permission problem:

```bash
systemctl --user restart xdg-desktop-portal-hyprland.service xdg-desktop-portal.service
```

## Text and typing

`type_text` sends literal text through `wtype`, which is layout-safe: it
speaks the Wayland virtual-keyboard protocol rather than pressing scancodes,
so an accented character or a symbol arrives as itself. `press_key` sends named
keys and chords through `ydotool`.

**Prefer `set_value` over selecting-and-typing** when a field is settable. It
uses the AT-SPI Value or EditableText interface, and only falls back to the
keyboard — GrabFocus, Ctrl+A, type — when the element exposes neither. The
result says which path ran. Applications built on GPUI and accesskit expose no
EditableText, so their text fields always take the fallback.

**A clipboard chord overwrites the user's clipboard.** `ctrl+c` and `ctrl+v`
work, and they destroy whatever a person had copied. Reach for `set_value` or
`type_text` instead unless the clipboard is the point.

## Screenshots

`screenshot` returns a bounded image by default, because an unbounded desktop
capture is a large payload for what is usually a small question. Ask for more
deliberately: `max_width` / `max_height` to raise the ceiling, `max_bytes` to
raise the byte cap, `region` to crop, `format: "jpeg"` with `quality` when the
content is photographic. Pass a window selector to capture one window, and
`raise_window` when it may be behind another.

`get_app_state` takes `include_screenshot` when both the tree and the picture
are wanted in one call.

## Reading text out of an element

The tree carries a node's name and text. What it does not carry is the
selection — where the caret is, and what is highlighted. That comes from the
AT-SPI `Text` interface on the a11y bus, which is a different bus from the
session one:

```bash
gdbus call --address "unix:path=$XDG_RUNTIME_DIR/at-spi/bus_0" \
  --dest <name> --object-path <path> \
  --method org.a11y.atspi.Text.GetSelection 0
```

`busctl --user` cannot see it. The address is the one `doctor` reports.

## Debugging by hand

`computer-use-hyprland` is also a command. `doctor`, `windows`, `apps`,
`state [APP_NAME]` and `screenshot` run the same machinery outside MCP, and
`abs-test X Y` clicks a desktop coordinate through the uinput pointer and
prints where it actually landed after clamping — the fastest way to tell a
coordinate problem from an input-backend one.

One trap when reaching for `hyprctl` directly: on a Hyprland configured in
Lua, `hyprctl dispatch movewindowpixel …` is rejected, because the Lua config
wraps every dispatch argument and only the `hl.dsp.*` table forms parse. The
server tries both and needs no help; a hand-run does.

## Applications that expose no tree

Electron applications expose no AT-SPI tree unless they were launched with
`--force-renderer-accessibility`. An empty `get_app_state` on an Electron
window is that, not a failure to read. Say so rather than falling back to
clicking blind pixels.
