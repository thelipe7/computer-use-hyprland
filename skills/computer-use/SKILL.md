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

## A clean bench for the application under test

A window that opens on Hyprland is tiled into whatever workspace is visible,
at whatever fraction of it the layout leaves. The same application comes up
947x1024 beside another window and 1904x1024 alone, so anything a test
measures inherits what the user happened to have open. Give it a bench of its
own:

1. **`launch_app`** starts it with the window rules applied at the moment the
   window is mapped: floating, on the first empty workspace, at an exact
   `width`/`height` when the test needs one. Floating is what gives it the
   size the program itself asks for — the size it would open at on a desktop
   that does not tile.
2. **`move_window_to_workspace` with `workspace: "empty"`** does the same for
   an application that is already running. The view follows it, because a
   screenshot captures the visible workspace and the pointer reaches only that
   one.
3. **Put the desk back.** Both tools report `previous_workspace`;
   `focus_workspace` with that id returns the view where it was.

This is for what you launched. A window the user opened is theirs, and moving
it between workspaces rearranges their desk rather than your bench.

Two consequences of the same fact. Floating first is also what makes
`move_window` and `resize_window` work at all — they refuse a tiled window.
And a window that opened tiled does **not** get the program's own size back
when it is floated afterwards: Hyprland gives it a size it remembers from that
window's own history. If the size matters, launch with it or resize to it.

## One selector vocabulary

Every window-targeted tool — `activate_window`, `get_app_state`, `wait_for`,
`screenshot`, `click`, `scroll`, `drag`, `press_key`, `type_text`,
`move_window`, `resize_window`, `set_window_floating` — takes the same nine
selectors, and any of them works on any of those tools:

`window_id`, `pid`, `app_id`, `wm_class`, `title`, and the four that resolve a
terminal: `tty`, `terminal_pid`, `terminal_command`, `terminal_cwd`.

`window_id` is exact and comes from `list_windows`; the rest match. Pass none
and the tool acts on whatever is focused.

**A selector that matches more than one window refuses**, naming each match
with its `window_id`, title and app, rather than driving one of them. `title`
narrows in three passes — the exact title, then the title but for letter case,
then any title containing it — and the first pass with anything in it decides.
So `title: "Sophia"` reaches the window named exactly that even while an
editor two workspaces away carries `sophia` in a project title, and a needle
that only ever appears inside longer titles refuses until `window_id`
disambiguates it.

**`window_title` is not a tenth selector.** It exists on `wait_for` alone and
means the predicate: the substring the focused window's title has to contain
before the wait returns. The selector is always `title`.

## Naming an element

In order of how much they promise:

- **`element_index`**, from `get_app_state` or `wait_for`. It is keyed to the
  element, not to a position in the snapshot, so it survives a re-read and
  keeps meaning the same element until that element goes away.
- **`object_ref` / `element_identifier`**, the AT-SPI identity itself.
- **A semantic selector** — `role`, `name`, `text`, `states` — when it matches
  exactly one node. More than one and the call refuses rather than guessing.
  It matches against the cached tree, so it needs a `get_app_state` or
  `wait_for` in this same server process before it resolves at all.
- **Coordinates** (`x`, `y`) for `click`, `scroll` and `drag`, in desktop
  pixels; `relative: true` reads them as an offset from the target window's
  origin instead.

Every one of those paths reports the desktop point it landed on, the element
an index resolved to, and a warning when the compositor puts the pointer
somewhere other than where the action aimed. Read the point back rather than
assuming it: that line is the difference between a click that worked and a
click that landed on whatever was under the cursor.

`drag` travels in small steps between its two ends rather than jumping, so a
title bar or a reorderable list — anything that reacts to the first movement
while still under the pointer — sees the drag.

Clicking by index beats clicking a pixel: on an element that exposes an AT-SPI
click action, `click` invokes the action and never moves the pointer, so it
does not depend on the window being unobscured. The result says which path ran.

## The four things that look like bugs

**Element indices die with their element.** An index is tied to the AT-SPI
identity it was minted for, which is gone when the view changes or the
application restarts. Then the call errors and says so, rather than acting on
whatever took that place — but it is still an error, so a step that reads and
then acts belongs close together, and after a relaunch the tree is read
again.

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

**A GPUI application puts only labeled nodes on the bus.** A caption sitting
in a plain container is not in the tree at all, so `wait_for(text=...)` times
out on text that is plainly on screen. Read those by `screenshot` with a
`region` instead, and keep the tree for the controls.

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
