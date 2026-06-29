# Lakitu

[![CI](https://github.com/dac2k9/lakitu/actions/workflows/ci.yml/badge.svg)](https://github.com/dac2k9/lakitu/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/lakitu.svg)](https://crates.io/crates/lakitu)
[![license](https://img.shields.io/crates/l/lakitu.svg)](#license)

**A live cockpit for a fleet of coordinating [Claude Code](https://claude.com/claude-code) agents.**

When you run several Claude Code agents in parallel — across repos, even across
machines — Lakitu is the screen you keep open: a TUI that shows every agent's
presence, what it's working on, its inbox, its tasks, and the work flowing
through your GitHub board. The agents coordinate through a small MCP server and
a shared on-disk store; the cockpit renders it live.

<!-- TODO: drop a screenshot/gif here -->

> The name is an homage: Lakitu is the cloud-riding cameraman who keeps the lens
> on the whole stage. That's the job — watch the fleet from above.

## How it fits together

```
   agents (Claude Code)          you
  ┌──────────────────┐     ┌─────────────────┐
  │ lakitu mcp (MCP) │     │ lakitu (cockpit)│
  │  register /      │     │  live TUI over  │
  │  heartbeat /     │────▶│  the same store │
  │  messages /      │     └─────────────────┘
  │  tasks / personas│              ▲
  └──────────────────┘              │
        │  writes                   │ reads
        ▼                           │
   $XDG_STATE_HOME/lakitu/  ◀───────┘   (shared store: registry, inboxes,
        fleet/                            tasks, personas, presence;
                                          legacy: ~/.claude/lakitu-fleet)
```

One binary, `lakitu`, with subcommands:

- **`lakitu`** (no args) — the cockpit TUI (the supervisor's view).
- **`lakitu mcp`** — the stdio MCP server agents talk to.
- **`lakitu serve`** — the same coordination service as an HTTP **daemon** so a
  fleet can span machines. On a loopback bind, `serve` also hosts a **read-only
  web cockpit** at `http://127.0.0.1:<port>/` — the fleet view + shared tasks in
  a browser.
- **fleet hooks** — small shell hooks wired into Claude Code's lifecycle that
  report presence, wake idle agents on new mail, inject personas + open tasks at
  session start, and feed the usage chip.
- **the `fleet-coordination` skill** — teaches an agent how to join the fleet.

## Install

```sh
cargo install lakitu          # one binary: the cockpit + `mcp`/`serve` subcommands
lakitu install-hooks          # set up the fleet hooks + skill
```

`install-hooks` writes the lifecycle hooks and the `fleet-coordination` skill
into `~/.claude` and wires them into `settings.json` (backing it up first) — no
clone needed, and it's idempotent. (If you'd rather work from a clone, running
`./scripts/install-fleet.sh` does the same thing.)

Bring an agent online (in the repo it works in):

```jsonc
// .mcp.json
{ "mcpServers": { "lakitu-mcp": { "command": "lakitu", "args": ["mcp"] } } }
```
```sh
export LAKITU_FLEET_NAME=aria   # a stable name for this agent
```

…then watch them all:

```sh
lakitu
```

## Upgrading from 0.4.x

Lakitu used to ship two binaries (`lakitu` + `lakitu-mcp`); 0.5.0 merges them into
one. To upgrade safely:

1. **Install the new binary first:** `cargo install lakitu` — it replaces the
   separate `lakitu-mcp` crate (now deprecated).
2. **Then** flip MCP configs from `"command": "lakitu-mcp"` to
   `{ "command": "lakitu", "args": ["mcp"] }`. **Order matters** — flipping
   before the new binary is installed fails. A `lakitu-mcp` shim ships for one
   release, so existing configs keep working until you flip.

See the [v0.5.0 release notes](https://github.com/dac2k9/lakitu/releases/tag/v0.5.0)
for the full migration.

## What you get

- **Presence at a glance** — a spinner for working, ⚠ for "needs you", ◐ for
  waiting-on-a-peer, dimmed when stale.
- **Per-agent inboxes** — agents message each other and you; reply, delete, or
  turn a message into a task right from the cockpit.
- **Tasks** — a private, compaction-surviving to-do list per agent (and yours),
  optionally hung off a PR.
- **Personas** — each agent keeps a self-authored identity + private notes on
  teammates that reload every session.
- **GitHub board automation** (optional) — MCP tools to move cards, set/clear
  blockers, and sweep agent PRs against a Projects v2 board. Configure the
  default repo/board with `LAKITU_DEFAULT_REPO` / `LAKITU_DEFAULT_BOARD`, or pass
  `repo=` per call.
- **Multi-machine** — run `lakitu serve` on a host and point remote agents +
  a remote cockpit at it. See [`deploy/REMOTE.md`](deploy/REMOTE.md).

## Configuration (env)

| Variable | Used by | Meaning |
|---|---|---|
| `LAKITU_FLEET_ROOT` | all | Store location. Default: `$XDG_STATE_HOME/lakitu/fleet` (i.e. `~/.local/state/lakitu/fleet`), falling back to the legacy `~/.claude/lakitu-fleet` when it already exists. |
| `XDG_STATE_HOME` | all | Base for the default store + logs (per the XDG Base Directory spec; defaults to `~/.local/state`). |
| `LAKITU_FLEET_NAME` | hooks/agents | This agent's stable name. |
| `LAKITU_DEFAULT_OWNER` | lakitu | *Optional* override for the owner prepended to bare repo names. By default the owner is inferred from the registered agents' repos, so this is rarely needed. |
| `LAKITU_DEFAULT_REPO` | lakitu mcp/serve | `owner/name` default for board tools. |
| `LAKITU_DEFAULT_BOARD` | lakitu mcp/serve | `owner/projectNumber` board fallback. |
| `LAKITU_FLEET_SERVER` / `LAKITU_FLEET_TOKEN` | remote | Daemon URL + bearer token. |

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option. Unless you state otherwise, any contribution you submit shall be
dual-licensed as above, without additional terms.
