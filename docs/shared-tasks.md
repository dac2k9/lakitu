# Shared tasks + auto-sync — design

Status: proposed (Dac-approved direction, 2026-06-22). Owner: lakitu (model/MCP/reconcile) + toad (web). Reviewer: protoman.

## The three asks
1. Tasks shared across a **team** or the whole **fleet** (not just one agent).
2. A **web view** of a shared task: involved clients, related PRs, and a Start→Goal **timeline**.
3. Stop relying on agents to hand-update Lakitu — today a PR gets opened and the client forgets to reflect it.

## Keep the layers distinct
- **Per-agent `Task`** (`tasks/<name>.json`) — a *private* scratchpad reminder. **Unchanged.** (Its doc comment is explicit: issues are the shared/reviewable unit; a task is the agent's own note.)
- **GitHub issue** — the durable, shared, reviewable unit of work. **Unchanged.**
- **NEW — `SharedTask`** — a team/fleet-scoped *goal* that **groups** issues + PRs across agents, with participants and a timeline. It *references* the units above; it does not duplicate them. (Example: "Release 0.3.1" was a fleet SharedTask spanning lakitu/toad/protoman/kirby + the keepalive PR.)

## `SharedTask` schema  — `tasks/shared/<id>.json`
```
{ id, title, goal?,
  scope: "team" | "fleet",   team?: "<owner/projectNumber>",
  owner, participants: [ "<agent>", ... ],
  issues: [ { repo: "<owner/name>", number } ],   // flat, each pinned to {repo, number}
  prs:    [ { repo: "<owner/name>", number } ],
  state: "open" | "active" | "blocked" | "in-review" | "done",
  timeline: [ { state, ts, by } ],   // append-only transitions
  created, updated }
```

## #3, the enforcement win — observe GitHub, don't nag
The robust fix is **not** forcing agents to update Lakitu; it's deriving state from reality. Extend the existing reconcile/sweep: each pass, for every SharedTask's linked issues, find PRs that close them (`Fixes/Closes #N`) → auto-link the PR, add its author as a participant, map the PR/issue state → `SharedTask.state`, and append a timeline transition. A SharedTask then **self-populates and self-advances** from GitHub — an agent forgetting to update Lakitu stops mattering.
- Backstop (P3): `sweep_agent_prs` **flags any open PR not linked to a shared task** (⚠ + a suggestion to add `Fixes #N` or `link_shared_task`). Chosen over a Stop-hook nudge — the Stop hooks are deliberately zero-token, and detecting an unlinked PR would need a gh call per idle; the sweep already holds the open PRs + bodies, so flagging there is accurate and ~free.
- Daemon-era: a GitHub **webhook** on PR events → real-time updates instead of polling.

## Web view (toad) — P4
`/tasks`: a card per SharedTask → participants (avatars) → linked PRs (live status pills) → a Start→Goal **timeline** plotting the transitions + PRs. Read-only SSR + htmx, rendered from the snapshot. lakitu exposes SharedTasks in the snapshot DTO; toad renders them.

## MCP tools (lakitu-mcp)
`create_shared_task(owner, title, goal?, scope, team?)` · `link_shared_task(id, kind, repo, number)` · `join_shared_task(id, name)` · `advance_shared_task(id, state, by)` · `list_shared_tasks(name?, include_done?)`; the snapshot DTO gains a `shared_tasks` array. A dedicated `list_shared_tasks` (rather than overloading `read_tasks`) keeps private vs shared cleanly separated. Per-agent task tools (`add_task`/`read_tasks`/`complete_task`/`drop_task`) are untouched.

## Phasing & owners
1. `SharedTask` model + store IO + MCP tools + snapshot DTO — **lakitu** (lakitu-mcp)
2. Auto-sync reconcile (extend the sweep) — **lakitu**
3. Unlinked-PR flag in `sweep_agent_prs` — **lakitu** (chosen over a Stop-hook nudge; see #3)
4. Web `/tasks` view — **toad** (+ lakitu on the snapshot DTO); coordinate once 1–3 land

protoman reviews each PR.

## Open decisions (for review)
- **Name + shape:** separate `SharedTask` entity (recommended — cleaner) vs. extending `Task` with scope/participants/timeline. Name: SharedTask / Initiative / Goal?
- **Scope tiers:** just team + fleet, or also a personal "goal" tier above the scratchpad tasks?
- **Participant rule:** auto (anyone with a linked PR/issue) + explicit join — confirm that's the right default.
