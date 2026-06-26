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
  │ lakitu-mcp (MCP) │     │ lakitu (cockpit)│
  │  register /      │     │  live TUI over  │
  │  heartbeat /     │────▶│  the same store │
  │  messages /      │     └─────────────────┘
  │  tasks / personas│              ▲
  └──────────────────┘              │
        │  writes                   │ reads
        ▼                           │
   ~/.claude/lakitu-fleet/  ◀───────┘   (shared store: registry, inboxes,
                                          tasks, personas, presence)
```

- **`lakitu`** — the cockpit TUI (the supervisor's view).
- **`lakitu-mcp`** — the MCP server agents talk to; also runs as an HTTP
  **daemon** (`lakitu-mcp serve`) so a fleet can span machines. On a loopback
  bind, `serve` also hosts a **read-only web cockpit** at
  `http://127.0.0.1:<port>/` — the fleet view + shared tasks in a browser.
- **fleet hooks** — small shell hooks wired into Claude Code's lifecycle that
  report presence, wake idle agents on new mail, inject personas + open tasks at
  session start, and feed the usage chip.
- **the `fleet-coordination` skill** — teaches an agent how to join the fleet.

## Install

```sh
cargo install lakitu lakitu-mcp     # the two binaries
lakitu-mcp install-hooks            # set up the fleet hooks + skill
```

`install-hooks` writes the lifecycle hooks and the `fleet-coordination` skill
into `~/.claude` and wires them into `settings.json` (backing it up first) — no
clone needed, and it's idempotent. (If you'd rather work from a clone, running
`./scripts/install-fleet.sh` does the same thing.)

Bring an agent online (in the repo it works in):

```jsonc
// .mcp.json
{ "mcpServers": { "lakitu-mcp": { "command": "lakitu-mcp" } } }
```
```sh
export LAKITU_FLEET_NAME=aria   # a stable name for this agent
```

…then watch them all:

```sh
lakitu
```

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
- **Multi-machine** — run `lakitu-mcp serve` on a host and point remote agents +
  a remote cockpit at it. See [`deploy/REMOTE.md`](deploy/REMOTE.md).

## Configuration (env)

| Variable | Used by | Meaning |
|---|---|---|
| `LAKITU_FLEET_ROOT` | all | Store location (default `~/.claude/lakitu-fleet`). |
| `LAKITU_FLEET_NAME` | hooks/agents | This agent's stable name. |
| `LAKITU_DEFAULT_OWNER` | lakitu | *Optional* override for the owner prepended to bare repo names. By default the owner is inferred from the registered agents' repos, so this is rarely needed. |
| `LAKITU_DEFAULT_REPO` | lakitu-mcp | `owner/name` default for board tools. |
| `LAKITU_DEFAULT_BOARD` | lakitu-mcp | `owner/projectNumber` board fallback. |
| `LAKITU_FLEET_SERVER` / `LAKITU_FLEET_TOKEN` | remote | Daemon URL + bearer token. |

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at
your option. Unless you state otherwise, any contribution you submit shall be
dual-licensed as above, without additional terms.
