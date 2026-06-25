# Lakitu-wrapped GitHub ops — design

Status: proposed (Dac-requested, 2026-06-25). Owner: lakitu (MCP tools / store / snapshot + TUI). Web: toad. Reviewer: protoman.

## The problem
Agents call `gh` directly to open PRs/issues. Those mutations never touch the fleet store, so the cockpit (TUI + web) can't show them — Dac missed PRs #12–#16 because a raw-`gh` PR is invisible there. The cockpit surfaces a PR only when it's *recorded* (linked to a shared task, or attached to a task). Three costs:
1. **Invisible work** — the supervisor can't see/steer/merge what isn't recorded.
2. **Store drift** — the store stops reflecting reality.
3. **No single control point** for GitHub mutations (auth, repo normalization, templates).

## The idea
Route the GH mutations that *should be tracked* through Lakitu. A wrapped tool both performs the GitHub action **and** records it in the store in one shot — so "open a PR" *means* "it shows in the cockpit." Visibility by construction, zero agent discipline required.

## Scope — wrap the tracked ops, NOT all of gh
Wrap only the few mutating ops worth tracking; leave gh's long tail (reviews, releases, `api`, `workflow`, …) to raw `gh`.
- **P1:** `open_pr`, `file_issue` (generalizes the existing `file_followup_issue`).
- **P2 (optional):** `merge_pr`, `comment_pr`.

Rationale: wrapping all of gh is a maintenance burden, and every MCP tool adds schema to the **fixed context re-read every turn** — the ~96%-of-billed cost Gate 0 found. Deferred-tool loading keeps these off the hot path, and a minimal surface keeps the added fixed cost near zero.

## The tools (P1)
```
open_pr(repo, title, body, base?, head?, draft?, shared_task?, fixes_issue?)
  → gh pr create …                       (the GitHub action)
  → emit pr_opened event; add to the opener's open-PRs in the snapshot;
    link to shared_task if given (else stands as the agent's own open PR)
  ⇒ { number, url }

file_issue(repo, title, body, labels?, shared_task?)
  → gh issue create …
  → emit event; optional shared-task link / board card
  ⇒ { number, url }
```
The opener is auto-credited; the same `{repo, number}` normalization as the link-dedupe fix is applied so refs never double up.

## Visibility surface
Add a per-agent **open-PRs** list to the snapshot (a field on `AgentSummary`, or a top-level `agent_prs`), populated at creation by the wrapped tools and reconciled by the existing sweep (merged/closed → dropped, reusing the cached ref-state from #11). The cockpit renders "agent X — open PRs: …" in the TUI + web. This is the **record-at-creation** version of the queued sweep-into-snapshot fix (`7ebbf5`) and supersedes it.

## "Paved path", not a wall
We can't *hard-block* `gh` — agents have Bash and can shell out; truly preventing it needs PATH-stripping / sandboxing (heavy, brittle). So:
- The wrapped tools are the **paved path** (and the docs/skill point agents at them).
- A `settings.json` permission **deny-rule** on raw `gh pr create` / `gh issue create` nudges agents to the wrapped tools (prompt-on-use). The auto-mode classifier already gates gh writes — same spirit.
- **Backstop:** `sweep_agent_prs` already flags open PRs not linked to a shared task (#9); extend it to flag *any* PR not opened via the wrapper, surfaced in the cockpit as "untracked" — so a raw-gh PR that slips through is still caught.

## Bonus: single control point
The wrapper centralizes things agents shouldn't each reinvent: repo normalization, PR-body templates, and notably **gh account selection** — the `dac2k9` vs `johan-larsson-fossid` switching done by hand today could move into the wrapper, keyed by the target repo.

## Phasing & owners
1. `open_pr` + `file_issue` + record-on-create + the snapshot open-PRs surface — **lakitu** (MCP + store/snapshot).
2. TUI render of agent open-PRs — **lakitu**; web `/tasks` (+ agent-PR) render — **toad**.
3. Deny-raw-gh permission nudge + `sweep_agent_prs` "untracked" backstop — **lakitu**.
4. (P2) `merge_pr` / `comment`, gh-account centralization, templates.

protoman reviews each PR.

## Open decisions (for review)
- **Enforcement:** deny-rule nudge (recommended) vs. paved-path-by-convention only.
- **Surface:** per-agent open-PRs in the snapshot (recommended) vs. task-attachment-only.
- **P1 ops:** PR + issue together, or PR first?
- **gh-account centralization:** fold into the wrapper now, or keep separate (it touches auth)?
- **Untracked PRs:** flag-and-surface (recommended) vs. ignore.
- **Token budget:** confirm the deferred-tool schema cost is acceptable with dr-mario's Gate-0 numbers.
