# Contributing

This is a single-maintainer fork. There is no PR process; what follows is the
gate a change has to pass before it lands.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
python3 scripts/mcp_safety_check.py --binary target/debug/computer-use-hyprland
```

The safety check spawns the binary, does the MCP handshake, and asserts the
exact tool set, that `run_shell` stays opt-in, and that the exported JSON
schemas are well-formed. Add a tool, and its name goes in `EXPECTED_TOOLS`.

## Scope

The supported environment is Hyprland on Wayland. A change that only makes
sense on another desktop does not belong here — the other backends were
removed because nobody could test them.

## Commits

Conventional Commits subject line, an imperative body that says why rather
than what, and `git commit -s`.

## Versions

`Cargo.toml` and the `version` literal in the `tool_handler` attribute in
`src/server.rs` must match; the macro only accepts a string literal, so it
cannot read `CARGO_PKG_VERSION`.
