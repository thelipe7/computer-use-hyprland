---
name: computer-use
description: >
  Use when driving a desktop application on Hyprland through the
  computer-use-hyprland MCP server — reading an accessibility tree, focusing a
  window, clicking, typing, pressing a chord, dragging, scrolling, taking a
  screenshot, or moving a window. Covers the order the tools go in, the one
  selector vocabulary they share, and the four failures that look like bugs:
  dead element indices, the machine-wide input lock, a tiled window refusing an
  exact geometry, and a portal that cannot screenshot.
---

# Driving a Hyprland desktop

The client lists what the server exposes. This says how the tools fit together
and where the surprises are.

## The loop

1. **`doctor` when anything looks off.** It reports the compositor, the
   portal, the accessibility bus and each input backend. `readiness.blockers`
   empty means the machine is ready, and a failure with something in it is
   that, not the tool.
2. **Find the window.** `list_windows` or `focused_window` to see what is
   there, `activate_window` to focus one. Targeted input refuses to run when
   focus cannot be verified, so this is not an optional step.
3. **Read before acting.** `get_app_state` returns the accessibility tree with
   an `element_index` on every node, which is how the other tools name an
   element. Pass `pid` or `window_id`: an untargeted tree is anchored to a
   window only when exactly one matches, and element coordinates are offsets
   from that window's origin.
4. **Act**: `click`, `type_text`, `press_key`, `drag`, `scroll`,
   `perform_action`, `set_value`.
5. **Verify with `wait_for`,** which blocks until a predicate holds, up to
   `timeout_ms`, and returns the tree as it is when it does. Use it instead of
   sleeping: a sleep either wastes the time or is too short.

## One selector vocabulary

Every window-targeted tool — `activate_window`, `get_app_state`, `wait_for`,
`screenshot`, `click`, `scroll`, `drag`, `press_key`, `type_text`,
`move_window`, `resize_window`, `set_window_floating` — takes the same nine
selectors, and any of them works on any of those tools:

`window_id`, `pid`, `app_id`, `wm_class`, `title`, and the four that resolve a
terminal: `tty`, `terminal_pid`, `terminal_command`, `terminal_cwd`.

`window_id` is exact and comes from `list_windows`; the rest match. Pass none
and the tool acts on whatever is focused.

**`window_title` is not a tenth selector.** It exists on `wait_for` alone and
means the predicate: the substring the focused window's title has to contain
before the wait returns. The selector is always `title`.

## Naming an element

In order of how much they promise:

- **`element_index`**, from the most recent `get_app_state` or `wait_for`.
- **`object_ref` / `element_identifier`**, which survive a re-read of the tree.
- **A semantic selector** — `role`, `name`, `text`, `states` — when it matches
  exactly one node. More than one and the call refuses rather than guessing.
- **Coordinates** (`x`, `y`) for `click`, `scroll` and `drag`, in desktop
  pixels; `relative: true` reads them as an offset from the target window's
  origin instead.

Clicking by index beats clicking a pixel: on an element that exposes an AT-SPI
click action, `click` invokes the action and never moves the pointer, so it
does not depend on the window being unobscured. The result says which path ran.

## The four things that look like bugs

**Element indices die when the application restarts.** They are positions in
one snapshot of one process's tree. After a relaunch or a crash, call
`get_app_state` or `wait_for` again. An index used across a restart does not
error — it points at whatever now sits in that position.

**One process at a time holds the input lock.** It is machine-wide, because
two servers driving one desktop would interleave their pointer and key events.
The second answers `ok=false` naming the holder's pid and saying when the lock
frees: 30 seconds after the holder's last call, unless that server's
`COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS` says otherwise. Wait that long and retry
once. A second refusal means the other session is still driving; report it
rather than work around it.

**Hyprland cannot give a tiled window an exact geometry.** `move_window` and
`resize_window` refuse a tiled window before dispatching anything: a pixel move
is ignored outright, and a pixel resize moves the layout split, resizing the
neighbors instead of the target. The refusal names the way out —
`set_window_floating` with `floating: true`, the move or resize, then
`floating: false` to put the layout back.

**A screenshot denied with response 2** is the Hyprland portal started without
`grim` on its `PATH`, not a permission problem. It is fixed by restarting
`xdg-desktop-portal-hyprland.service` and `xdg-desktop-portal.service`.

## Typing and reading text

`type_text` sends literal text through `wtype`, which speaks the Wayland
virtual-keyboard protocol rather than pressing scancodes, so an accented
character arrives as itself. `press_key` sends named keys and chords.

**Prefer `set_value` over selecting and typing.** It uses the AT-SPI Value or
EditableText interface and falls back to the keyboard — GrabFocus, Ctrl+A,
type — only when the element exposes neither; the result says which ran.
Applications built on GPUI expose no EditableText, so their fields always take
the fallback.

**A clipboard chord destroys the user's clipboard.** `ctrl+c` and `ctrl+v`
work, and whatever a person had copied is gone. Reach for `set_value` or
`type_text` unless the clipboard is the point.

To read text back, use the tree: a text node carries `content`, `caret_offset`
and `selections`, and `selection_error` when the selection could not be read at
all, which is a different answer from nothing being selected.

## Screenshots

`screenshot` returns a bounded image by default, because an unbounded desktop
capture is a large payload for what is usually a small question. Ask for more
deliberately: `max_width` / `max_height` raise the ceiling, `max_bytes` the
byte cap, `region` crops, and `format: "jpeg"` with `quality` suits
photographic content. Pass a window selector to capture one window, and
`raise_window` when it may be behind another.

`get_app_state` takes `include_screenshot` when both the tree and the picture
are wanted in one call.

## Applications that expose no tree

Electron applications expose no AT-SPI tree unless they were launched with
`--force-renderer-accessibility`. An empty `get_app_state` on an Electron
window is that, not a failure to read. Say so rather than falling back to
clicking blind pixels.
