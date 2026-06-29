# Multi-agent coordination — design / contract

This is the shared contract between three pieces:

1. **`lakitu`** (this repo) — the TUI. Read-only viewer of the store.
2. **`lakitu-mcp`** (`~/src/lakitu-mcp`) — the write-side the agents
   call (`register_agent`, `heartbeat`, `send_message`, `read_inbox`,
   `list_agents`).
3. **The skills** (`board-issue-loop`, `pr-review-fixup`) — tell each agent to
   register on start, heartbeat at step boundaries, and check its inbox.

Everything coordinates through a shared directory. No daemon, no sockets — the
same file-as-IPC philosophy the existing `agent-actions.log` tailer already
uses. Ephemeral Claude Code agents come and go; files survive restarts and are
trivially inspectable.

## Store layout

The store root resolves to `$XDG_STATE_HOME/lakitu/fleet` (default
`~/.local/state/lakitu/fleet`), per the XDG Base Directory spec — this is
*state*, not config or cache. An existing pre-XDG `~/.claude/lakitu-fleet` keeps
being used when it's present (no auto-migration), and `$LAKITU_FLEET_ROOT`
overrides everything. All resolution lives in `src/paths.rs`.

```
<store-root>/                # $XDG_STATE_HOME/lakitu/fleet (legacy ~/.claude/lakitu-fleet)
  agents/
    <name>.json              # registry — written once at register time
    <name>.heartbeat.json    # presence — rewritten on every heartbeat
  inbox/
    <name>/
      <ts>-<id>.json         # one unread message per file
      read/
        <ts>-<id>.json       # messages the recipient has consumed
```

`<name>` is a stable, human-friendly agent name (kebab-case, e.g. `vscode-bot`).
`<ts>` is `YYYYMMDDTHHMMSS`; `<id>` is a short opaque token for uniqueness.

## File schemas

### `agents/<name>.json` — registry
```json
{
  "name": "vscode-bot",
  "kind": "agent",
  "repo": "acme/web",
  "board": "acme/14",
  "description": "VS Code extension; ask me to wire UI/commands against new MCP tools.",
  "path": "/Users/you/src/web",
  "started": "2026-05-29T10:00:00+02:00"
}
```
`path` is the agent's local checkout dir, recorded at registration so the
optional inbox-waker (`<store-root>/waker.sh`, toggled from the cockpit with
`w`) can relaunch a *stopped* agent there when mail arrives.
`kind` is `agent` (default, omittable) or `human` — the supervisor running the
cockpit registers as a `human` "client" (written by the TUI, not the MCP). The
cockpit also keeps the human's chosen name in `<store-root>/me`. Humans
render at the top of the roster, never go stale, and have no work-state.
`repo` and `board` are free-form labels (#7). `board` convention:
`<owner>/<projectNumber>`. `description` is a stable, self-authored capability
blurb — *what this agent is for and what peers can ask it to do* — distinct from
the transient `task` in the heartbeat. It's the field a peer reads (via
`list_agents`) to decide who to message. Optional; absent on agents that
registered without one.

### `agents/<name>.heartbeat.json` — presence (#3)
```json
{
  "ts": "2026-05-29T10:05:00+02:00",
  "state": "working",
  "task": "issue #90: fix watcher leak"
}
```
`state` ∈ `idle` | `working` | `blocked`. `task` is a free-form one-liner
(optional). Agents declare their own state explicitly — the TUI does not infer
it from the log.

**Staleness:** if `now - ts > STALE_AFTER` (default **15 min**) the TUI shows the
agent as *stale* (dimmed) regardless of declared `state`. Claude Code agents
have no background ticker — a heartbeat is "last activity + declared state", so
the window is lenient. A missing heartbeat file = state `unknown`.

### `inbox/<name>/<ts>-<id>.json` — message (#5, #6)
```json
{
  "id": "0f3a9c",
  "time": "2026-05-29T10:06:00+02:00",
  "from": "vscode-bot",
  "title": "Need fetch_match_ranges schema",
  "body": "Can you expose X so I can wire the VS Code side? It can be a feature request."
}
```
A message is addressed by *which directory it lands in* (`inbox/<to>/`). Reading
moves it to `inbox/<to>/read/` (so unread = top-level files). The TUI shows
unread prominently and read below.

## Responsibilities

| Concern | Writer | Reader |
| --- | --- | --- |
| Registry (`agents/<name>.json`) | `register_agent` | TUI agents pane |
| Presence (`*.heartbeat.json`) | `heartbeat` | TUI agents pane |
| Send a message | `send_message` (writes to recipient's inbox) | — |
| Read inbox | `read_inbox` (moves to `read/`) | TUI inbox view |
| Peer awareness (#4) | — | `list_agents` (agents) + TUI |

The existing `agent-actions.log` is unchanged — it stays the per-issue audit
trail that drives the work-items pane. Presence/identity/messaging live in this
separate store so the log format and `work.rs` aggregation are untouched.
