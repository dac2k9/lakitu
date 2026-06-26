# One `lakitu` binary — design

Status: proposed (Dac-approved direction, 2026-06-26). Owner: lakitu (binary + lib split + install-hooks). Packaging/release: toad. Reviewer: protoman.

## The problem
Lakitu ships two binaries — `lakitu` (the TUI cockpit) and `lakitu-mcp` (the MCP server + coordination daemon). Multiple requests have asked to combine them. Two binaries means two things to install and keep version-synced, two crates.io publishes, and an MCP config (`"command": "lakitu-mcp"`) under a different name from the cockpit. One binary is simpler to install, version, and document.

## The idea
A single `lakitu` binary with subcommands. The cockpit stays the default (no args); the server/daemon/installer move under verbs:

| Command | Today | Behavior |
|---|---|---|
| `lakitu` (+ flags) | `lakitu` | TUI cockpit (unchanged) |
| `lakitu mcp` | `lakitu-mcp` | stdio MCP (Claude Code's per-agent transport) |
| `lakitu serve` | `lakitu-mcp serve` | HTTP daemon (MCP-over-HTTP + REST) |
| `lakitu install-hooks` | `lakitu-mcp install-hooks` | materialize hooks + skill into `~/.claude` |

## Why it's clean today
- Both are **binary-only** crates (modules are private `mod`s in `main.rs`; no `lib.rs`).
- **No module-name collisions**: `lakitu` has app/ui/store/client/event/gh/log/remote/work; `lakitu-mcp` has server/fleet/persona/daemon/rest/install/wire. Disjoint.
- `lakitu-mcp` already dispatches subcommands (`serve`, `install-hooks`, default = stdio) — we're extending that, not inventing it.

## Crate organization — recommended: lib + binary, not a full merge
Turn `lakitu-mcp` into a **library** (add `lib.rs` exposing `run_stdio()`, `serve()`, `install::run()` and the types the daemon/web need public). The `lakitu` crate keeps the **binary**, adds `lakitu-mcp = { path = "../lakitu-mcp" }`, and dispatches the subcommands to it.

- One binary (`lakitu`); `lakitu-mcp` becomes a lib dependency.
- Least churn — both codebases stay intact (toad's web work stays in the `lakitu-mcp` lib).
- Note: **"one binary" ≠ "one crate."** This still publishes two crates to crates.io (a `lakitu` bin + a `lakitu-mcp` lib). Collapsing to a single crate is possible (modules don't collide) but is a much bigger move and a harder rebase against toad's branch — deferred unless we also want a single crates.io entry.

## Breaking change + migration
`lakitu mcp` replaces the `lakitu-mcp` command, so every fleet config (`~/.claude.json`, `.mcp.json`) with `"command": "lakitu-mcp"` breaks on upgrade. To not break the running fleet:
1. Ship a thin **`lakitu-mcp` shim** (a tiny second bin that execs `lakitu mcp`, with a deprecation note to stderr) for one release.
2. Update `install-hooks` to emit the new `{ "command": "lakitu", "args": ["mcp"] }` config.
3. Drop the shim the following release.

The project already does graceful renames (the `GENBOT_*` → `LAKITU_*` env back-compat), so this fits the pattern. → ships as **0.5.0** (breaking).

## Smaller seams
- **Deps union**: the combined binary links both dep sets (ratatui/image + rmcp/axum) → a larger binary, and the `mcp` mode carries the TUI deps. Acceptable for a dev tool; optional cargo features to slim a build are future work.
- **Error type**: `lakitu` uses `color-eyre`, `lakitu-mcp` uses `anyhow`. The dispatcher converts at the boundary (each subcommand keeps its own).
- **tracing**: one init; the stdio `mcp` mode must keep stdout clean for the JSON-RPC wire (tracing → side-channel file, as today).

## Phasing & owners
1. lib-ify `lakitu-mcp` (`lib.rs` + pub the entry points) — **lakitu**
2. `lakitu` binary: clap subcommand dispatch (`mcp` / `serve` / `install-hooks` + default TUI) — **lakitu**
3. `lakitu-mcp` deprecation shim + `install-hooks` emits the new config — **lakitu**
4. README / docs + crates.io packaging update — **toad**
5. Cut **0.5.0** with migration notes — **toad**

**Sequencing:** land this *after* toad's `feat-web-ui` merges — it restructures `crates/lakitu-mcp`, where that branch lives, so doing it first forces a brutal rebase.

## Open decisions (for review)
*Decided 2026-06-26: keep `serve` for the daemon verb (not `daemon`) — least migration churn, matches the current command.*

- **One crate too?** lib + bin (recommended) vs fully merge into a single crate (one crates.io entry, bigger move).
- **Shim:** one-release `lakitu-mcp` shim (recommended) vs hard cutover with loud release notes.
- **Default with no args:** TUI (recommended, current behavior) vs print help.
