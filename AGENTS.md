# computer-use-hyprland

An MCP server that lets an agent drive a Hyprland desktop: read the
accessibility tree, take screenshots, target windows, and send input.

Read [CONTRIBUTING.md](CONTRIBUTING.md) before changing anything. It holds
every rule this project has: how a commit is written, the verification gate,
what the public contract is and what it is not, how the code is laid out, and
how a release is cut.

## Nothing here can be proven from a build alone

A green `cargo test` says the parsing is right. It says nothing about whether
a window moved, a key arrived, or a screenshot came back, because there is no
compositor in a test and none on a CI runner either.

So a change to window geometry, focus, input synthesis, screenshot capture or
the accessibility tree is verified by running it against a real Hyprland, and
the commit message says what was observed. `cargo install --path .` and then
restart the MCP client: its server process holds the old binary and does not
reload it.

Two traps in a hand-run. Element indices die with their element: an index is
keyed to the AT-SPI identity, so it survives a re-read of the tree and is gone
when the element or its process is, and a call that names a dead one errors
rather than acting on whatever took its place. And one process at a time holds
the input lock — a second server answers `ok=false` naming the holder's pid
rather than fighting it for the pointer. The lock frees 30 seconds after the
holder's last call, so a refusal from a session that has gone quiet clears
itself on a retry.

## A tool description is code

Every `description` in `src/server.rs` is the instruction a model reads before
it calls that tool. A description that promises a capability this build does
not have is a defect of the same kind as a wrong return value, and it fails in
a worse way: the caller believes it and looks for the wrong cause.

`scripts/mcp_safety_check.py` is what pins the surface those descriptions
belong to. Run it after touching anything in `server.rs`.
