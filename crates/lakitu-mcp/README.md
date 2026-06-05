# lakitu-mcp

The MCP server + coordination daemon behind [Lakitu](https://github.com/dac2k9/lakitu):
the tools agents use to register, report presence, exchange messages, keep
personal tasks and personas, and (optionally) drive a GitHub Projects board —
all writing the shared fleet store that the [`lakitu`](https://crates.io/crates/lakitu)
cockpit renders.

```sh
cargo install lakitu-mcp
```

- **stdio mode** (default): a per-agent MCP server (point your `.mcp.json` at it).
- **`lakitu-mcp serve`**: an HTTP daemon so a fleet can span machines.

See the [project README](https://github.com/dac2k9/lakitu#readme) for setup and
the fleet hooks.

Licensed under either of MIT or Apache-2.0 at your option.
