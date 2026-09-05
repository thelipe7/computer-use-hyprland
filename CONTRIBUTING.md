# Contributing

Use GitHub Discussions for support and early ideas, and Issues for
reproducible defects or agreed work.

This file holds every rule this project has. Nothing about how to contribute
lives in a README, a wiki page or a comment thread.

## Scope

This is a Hyprland server. Not "Linux, currently tested on Hyprland" — the
target is Hyprland on Wayland, and the design follows from it: windows are
driven by `hyprctl` because that is the interface Hyprland exposes, and input
is synthesized through `uinput` because Hyprland's portal implements no
RemoteDesktop interface for anyone to ask.

A generic Linux server is a different program. It abstracts over compositors
that disagree about what a window even is, and every capability it offers is
the intersection of what all of them can do. This one takes the union of what
one compositor can do, which is why it can move a window to an exact pixel,
name the layout split that refuses to, and tell a caller which tool clears the
refusal.

So a change that only makes sense on another desktop does not belong here,
and neither does an abstraction whose only purpose is to leave room for one.

## Git

Pull requests merge by rebase. Every commit lands on the default branch
exactly as written, so every rule below is about the individual commit, not
the pull request.

### Commits

- **Conventional Commits.** `<type>(scope): <description>`, imperative mood.
  The types are `build`, `chore`, `ci`, `docs`, `feat`, `fix`, `perf`,
  `refactor`, `revert`, `style` and `test`. CI checks every commit and the
  pull request title.
- **A breaking change says so with `!` before the colon.** A break that does
  not say so ships as a patch. [Compatibility](#compatibility) says what
  counts as one here, and it is not what it would be in a library.
- **One change per commit.** A subject that needs an "and" is two commits.
  This is a rule about the story the history tells, not about the state of the
  tree at each step: CI runs on the branch tip, so an intermediate commit that
  does not build on its own is not worth rewriting history to fix.
- **The reasoning goes in the commit body**, not only in the pull request. The
  body is what survives the merge.
- **Sign off every commit** with `git commit -s`. The `Signed-off-by` line
  certifies the [Developer Certificate of
  Origin](https://developercertificate.org/), and CI checks it commit by
  commit against each commit's own author.
- **Hard-wrap commit body lines at 72 characters.**

### Pull requests

Keep each pull request focused, and add or update a test at the behavior seam
being changed.

`unsafe` here is `uinput` ioctls, `flock`, and the `libc` calls around process
groups — foreign calls, every one. New `unsafe` needs a `SAFETY` comment
saying what is being promised, and the promise has to be one a reader can
check against the code around it. `env::set_var` is the one to be careful
with: edition 2024 marks it `unsafe` because writing a variable while another
thread reads one is undefined behavior, and this process spawns blocking
threads.

Miri is deliberately absent, and the reason is what this `unsafe` is rather
than how much of it there is. A foreign call is the thing Miri cannot see
inside, and it cannot spawn a process either. It earns its place the day
something here holds `unsafe` over raw pointers or aliasing.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo test --locked --doc
taplo fmt --check
typos
cargo deny --locked check
cargo build --locked && python3 scripts/mcp_safety_check.py --binary target/debug/computer-use-hyprland
```

Four of those need a tool the toolchain does not carry:

```bash
cargo install cargo-deny taplo-cli typos-cli
```

`taplo` formats the TOML nothing else formats, and this project is mostly
TOML at the edges: the lint policy, the profiles, the dependency policy.
`typos` reads prose and identifiers, where a misspelling compiles and so
survives every other check.

The last line is the contract check. It spawns the binary, does the MCP
handshake and asserts the exact tool set, that `run_shell` stays opt-in, that
every window-targeted tool exposes the same nine selectors, and that the
exported JSON schemas are well-formed. Add a tool and its name goes in
`EXPECTED_TOOLS`; that is the point, not an obstacle.

### What CI cannot check

A runner has no Hyprland, no AT-SPI bus and no `uinput`. So CI proves the
server builds, lints, and still speaks the tool surface it promises, and
nothing about whether it can drive a desktop.

Anything that can only be answered by a running compositor is answered by
hand, against one, and the commit message says what was observed. That means
a change to window geometry, focus, input synthesis, screenshot capture or
the accessibility tree. Install and restart before believing a result — the
MCP client's server process does not reload the binary:

```bash
cargo install --path .
```

Two things make a hand-run confusing if you do not know them. Element indices
die when the target application restarts, so `get_app_state` or `wait_for` has
to run again before an index is used against a relaunched process. And only
one process at a time may hold the input lock, so a second server answers
`ok=false` naming the holder's pid rather than fighting it for the pointer.

## Dependencies

`Cargo.lock` is committed and every command above holds it fixed, so a change
that moves a version moves the lockfile in the same commit. That is what makes
a red build mean this code broke rather than somebody else's release.

`deny.toml` lists the licenses this project has accepted, and every one of
them is permissive. Adding an entry is a line somebody writes and a reviewer
reads. Nothing is taken from git, and `[sources]` says so.

## Toolchains

`rust-toolchain.toml` pins the compiler this project is developed with, and
`rustup` installs it on the first `cargo` command. `rust-version` in
`Cargo.toml` states the floor, and it is that same pinned toolchain, the only
one anything here is built or tested on. Moving the pin is two edits in one
commit.

## Layout

One crate, one binary, and a library beside it that exists so the code can be
tested: `run_cli_from_env` is the whole public surface, and everything else is
`pub(crate)`.

| File | What it owns |
|---|---|
| `server.rs` | The MCP surface: every tool, its parameters, its result |
| `atspi_tree.rs` | Reading the accessibility tree and invoking its actions |
| `windowing/` | Windows: the `hyprctl` backend, target resolution, types |
| `screenshot.rs` | Portal capture, and encoding a payload to fit its bounds |
| `abs_pointer.rs` | The `uinput` absolute pointer this server creates |
| `ydotool.rs` | Keys and chords through a connectable `ydotoold` socket |
| `diagnostics.rs` | `doctor`, `setup`, and the desktop environment hydration |
| `terminal.rs` | The tty and process metadata a terminal window carries |
| `command_runner.rs` | Running a subprocess under a deadline and an output cap |
| `session_lock.rs` | The machine-wide input lock |
| `cli.rs` | The subcommands, including `mcp`, which is what a client runs |

`server.rs` is eight thousand lines and that is a fact about it rather than a
target: a tool's parameters, its handler and its tests sit together, and
splitting them by mechanism would put the three halves of one tool in three
files.

### The code

A tool's `description` is not documentation a human skims. It is the
instruction a model reads before every call, so it says what the tool does,
what it needs, and what refuses it — in that order, naming the tool that
clears a refusal where one exists. A description that promises a capability
this build does not have is a bug of the same kind as a wrong return value.

An error message names what failed, what was tried, and what the caller can do
next. "Window 0x… is tiled, so Hyprland cannot resize it to an exact geometry"
tells the caller more than "resize failed", and the sentence after it, naming
`set_window_floating`, is the part that ends the loop.

`Cargo.toml` and the `version` literal in the `tool_handler` attribute in
`server.rs` must match. The macro only accepts a string literal, so it cannot
read `CARGO_PKG_VERSION`, and the contract check compares the two.

## Compatibility

The public surface is not a Rust API. It is the MCP contract: the set of tool
names, the parameter names and shapes each accepts, the shape of what each
returns, the environment variables, and the name of the binary an MCP client
spawns. Those are what somebody's configuration depends on, and a change to
any of them is a break that says so with `!`.

A tool description is not part of the contract, and neither is an error
message. Both are meant to improve.

**Until the first release on the registry**, in-place breakage is the rule:
rename it, update every call site, and drop the old name. There is no
deprecated alias and no compatibility shim. This window is the whole advantage
of fixing a surface before anybody depends on it.

**From the first release onward**, an added tool or an added optional
parameter is a `feat`; a renamed or removed one is a `feat!` or a `refactor!`,
and the release notes say what a client has to change.

## Releasing

A release is a thing somebody sits down and does. Four steps, in this order:

1. Bump `version` in `Cargo.toml` and the `version` literal in the
   `tool_handler` attribute in `src/server.rs`. The contract check fails if
   they disagree.
2. Run the whole of [Verification](#verification), plus
   `cargo publish --dry-run` and `cargo package --list`, and read the file
   list: the package should carry the sources, the manifest, the license and
   the README, and nothing else.
3. Tag the commit and push the tag.
4. `cargo publish`.

Publishing is irreversible. A version can be yanked, which stops new
dependents from resolving it, and cannot be deleted or replaced.
