# Contributing

Thanks for your interest! Lakitu is a Cargo workspace:

- `crates/lakitu` — the cockpit TUI
- `crates/lakitu-mcp` — the MCP server + HTTP daemon, with the fleet hooks and
  the `fleet-coordination` skill vendored under `assets/`

## Dev loop

```sh
cargo test --workspace
cargo build --workspace
cargo run -p lakitu          # the cockpit
```

## Before opening a PR

- `cargo fmt --all`
- `cargo clippy --workspace`
- add or adjust tests for any behavior change (the store/UI logic is well
  covered — keep it that way)

## License

By contributing, you agree that your contributions will be dual-licensed under
**MIT OR Apache-2.0**, without any additional terms.
