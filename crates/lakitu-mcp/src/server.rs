//! Tool definitions + handlers for the lakitu-mcp MCP.
//!
//! Phase-1 surface: `emit_event`, `move_card`, `set_blocker`,
//! `clear_blocker`. These collapse the deterministic bash incantations
//! from `board-issue-loop` / `pr-review-fixup` into single-call
//! primitives, so the skill markdown can describe *decisions* without
//! re-deriving the mechanics each time.
//!
//! ## State guards in the MCP, not the skill
//!
//! `set_blocker` refuses CLOSED issues outright (skill used to rely on
//! the LLM remembering to check first; today's bug shows that doesn't
//! survive context). The OPEN-state guard is here for everyone.
//!
//! ## Idempotency
//!
//! `set_blocker` is a no-op if the same blocker was logged for the same
//! issue+reason within the last 24 hours. The `<!-- agent-blocker-filed -->`
//! HTML marker keeps the @-mention comment from double-firing.
//!
//! ## Caching
//!
//! Project / field / option IDs are stable per-project; we look them up
//! once on first call and reuse. The cache lives in memory only — a
//! restart re-fetches. Stale-ID failure mode is loud (404 from `gh
//! project item-edit`) rather than silent.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::fleet;
use crate::persona;

// Repo / board defaults come from the environment so the tools aren't tied to
// any one project. Every GitHub-hitting tool takes an optional per-call `repo`;
// when it's omitted we fall back to these:
//   LAKITU_DEFAULT_REPO   "owner/name"          — the default repo
//   LAKITU_DEFAULT_BOARD  "owner/projectNumber" — board fallback when a repo
//                                                 has no linked Project v2
// Unset → pass `repo=` per call; the board falls back to the repo's own owner
// and project #1.
fn default_repo_slug() -> String {
    std::env::var("LAKITU_DEFAULT_REPO").unwrap_or_default()
}

/// Owner from `LAKITU_DEFAULT_REPO`, if set (used to expand a bare repo name).
fn default_owner() -> Option<String> {
    default_repo_slug()
        .split_once('/')
        .map(|(o, _)| o.to_string())
        .filter(|o| !o.is_empty())
}

/// Board to use when a repo has no linked Project v2: `LAKITU_DEFAULT_BOARD`
/// ("owner/number") if set, else the repo's own owner with project #1.
fn board_fallback(repo_slug: &str) -> BoardCoords {
    if let Some((owner, num)) = std::env::var("LAKITU_DEFAULT_BOARD").ok().and_then(|v| {
        v.split_once('/').and_then(|(o, n)| {
            n.parse::<u32>()
                .ok()
                .filter(|_| !o.is_empty())
                .map(|n| (o.to_string(), n))
        })
    }) {
        return BoardCoords { owner, number: num };
    }
    let owner = repo_slug
        .split_once('/')
        .map(|(o, _)| o)
        .unwrap_or("")
        .to_string();
    BoardCoords { owner, number: 1 }
}

/// Resolve a caller-supplied `repo` into the full `owner/repo` slug
/// `gh` wants on its `--repo` flag. Three input shapes:
/// - `None`                  → `LAKITU_DEFAULT_REPO` (empty if unset)
/// - `Some("api")`            → `<default owner>/api` (else bare, if no owner)
/// - `Some("acme/foo")`       → as-is (cross-owner if ever needed)
fn normalize_repo_slug(opt: Option<&str>) -> String {
    match opt {
        None => default_repo_slug(),
        Some(s) if s.contains('/') => s.to_string(),
        Some(s) => match default_owner() {
            Some(owner) => format!("{owner}/{s}"),
            None => s.to_string(),
        },
    }
}

/// Just the repo name (no owner) — for the audit-log `repo` column.
/// Tracks `normalize_repo_slug` so both are derived from the same input.
fn normalize_repo_name(opt: Option<&str>) -> String {
    match opt {
        None => default_repo_slug()
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string(),
        Some(s) => s.rsplit('/').next().unwrap_or(s).to_string(),
    }
}

/// Branch-name prefixes the agent uses. The sweep filters to these so we
/// don't act on (or even list) PRs the agent didn't open.
/// Conventional commit-style prefixes the agent uses when branching.
/// Matches the `pr-review-fixup` skill's §Scope filter. The `issue-<N>-`
/// segment from the board-issue-loop convention is NOT required here —
/// sister repos like api use branch slugs without an issue
/// number (e.g. `feat/matchedby-remote-ranges`), and they're still
/// agent-authored. The footer check (`AGENT_FOOTER`) gates against
/// human-opened PRs that happen to use these prefixes.
const AGENT_BRANCH_PREFIXES: &[&str] = &["fix/", "feat/", "chore/", "docs/", "test/", "refactor/"];

/// Literal footer the agent appends to every PR body it opens (via the
/// loop's PR template). Paired with the branch-prefix check, both must
/// match for a PR to count as agent-authored.
const AGENT_FOOTER: &str = "🤖 Generated with [Claude Code]";

/// Which GitHub Project (owner login + number) a repo's board lives on.
/// Resolved per-repo by `discover_board`, falling back to the v0.1 default.
#[derive(Debug, Clone)]
struct BoardCoords {
    owner: String,
    number: u32,
}

/// Auto-discover the Project v2 linked to `repo_slug` (`owner/name`): take the
/// first linked project. Falls back to the hardcoded default (acme/#14)
/// when nothing is linked or the lookup fails, so existing single-board setups
/// keep working unchanged.
async fn discover_board(repo_slug: &str) -> BoardCoords {
    let fallback = board_fallback(repo_slug);
    let Some((owner, name)) = repo_slug.split_once('/') else {
        return fallback;
    };
    let query = "query($o:String!,$n:String!){repository(owner:$o,name:$n){projectsV2(first:10){nodes{number owner{__typename ... on Organization{login} ... on User{login}}}}}}";
    let out = run_gh(&[
        "api",
        "graphql",
        "-f",
        &format!("query={query}"),
        "-f",
        &format!("o={owner}"),
        "-f",
        &format!("n={name}"),
    ])
    .await;
    let Ok(json) = out else { return fallback };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else {
        return fallback;
    };
    let node = v["data"]["repository"]["projectsV2"]["nodes"]
        .as_array()
        .and_then(|a| a.first());
    match node.and_then(|n| n["number"].as_u64()) {
        Some(number) => BoardCoords {
            owner: node
                .and_then(|n| n["owner"]["login"].as_str())
                .unwrap_or(owner)
                .to_string(),
            number: number as u32,
        },
        None => fallback,
    }
}

/// Cached IDs that are stable per-project. Re-fetched on first use per board.
#[derive(Debug, Default, Clone)]
struct BoardCache {
    project_id: String,
    status_field_id: String,
    status_options: HashMap<String, String>,
    blocker_field_id: String,
    blocker_options: HashMap<String, String>,
}

impl BoardCache {
    async fn fetch(coords: &BoardCoords) -> Result<Self, McpError> {
        let number = coords.number.to_string();
        let project_json = run_gh(&[
            "project",
            "view",
            &number,
            "--owner",
            &coords.owner,
            "--format",
            "json",
        ])
        .await?;
        let project: serde_json::Value = serde_json::from_str(&project_json).map_err(mcp)?;
        let project_id = project["id"]
            .as_str()
            .ok_or_else(|| mcp("project.id missing in `gh project view` output"))?
            .to_string();

        let fields_json = run_gh(&[
            "project",
            "field-list",
            &number,
            "--owner",
            &coords.owner,
            "--format",
            "json",
        ])
        .await?;
        let fields: serde_json::Value = serde_json::from_str(&fields_json).map_err(mcp)?;

        let mut cache = BoardCache::default();
        let empty = Vec::new();
        for field in fields["fields"].as_array().unwrap_or(&empty) {
            let name = field["name"].as_str().unwrap_or("");
            let id = field["id"].as_str().unwrap_or("").to_string();
            match name {
                "Status" => {
                    cache.status_field_id = id;
                    for opt in field["options"].as_array().unwrap_or(&empty) {
                        let n = opt["name"].as_str().unwrap_or("").to_string();
                        let i = opt["id"].as_str().unwrap_or("").to_string();
                        cache.status_options.insert(n, i);
                    }
                }
                "Blocker" => {
                    cache.blocker_field_id = id;
                    for opt in field["options"].as_array().unwrap_or(&empty) {
                        let n = opt["name"].as_str().unwrap_or("").to_string();
                        let i = opt["id"].as_str().unwrap_or("").to_string();
                        cache.blocker_options.insert(n, i);
                    }
                }
                _ => {}
            }
        }
        cache.project_id = project_id;
        Ok(cache)
    }
}

/// In-memory caches: which board each repo maps to (auto-discovered once), and
/// the field/option IDs per board (fetched once per board).
#[derive(Default)]
struct Caches {
    repo_board: HashMap<String, BoardCoords>,
    board: HashMap<(String, u32), BoardCache>,
}

#[derive(Clone)]
pub struct AgentBoardService {
    tool_router: ToolRouter<Self>,
    caches: Arc<RwLock<Caches>>,
}

impl Default for AgentBoardService {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl AgentBoardService {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
            caches: Arc::new(RwLock::new(Caches::default())),
        }
    }

    /// Resolve `repo_slug`'s board (auto-discovered + cached) and its field/
    /// option IDs (fetched + cached per board). First call per repo/board hits
    /// `gh`; later calls are in-memory.
    async fn board_for(&self, repo_slug: &str) -> Result<(BoardCoords, BoardCache), McpError> {
        let coords = { self.caches.read().await.repo_board.get(repo_slug).cloned() };
        let coords = match coords {
            Some(c) => c,
            None => {
                let c = discover_board(repo_slug).await;
                self.caches
                    .write()
                    .await
                    .repo_board
                    .insert(repo_slug.to_string(), c.clone());
                c
            }
        };
        let key = (coords.owner.clone(), coords.number);
        let cache = { self.caches.read().await.board.get(&key).cloned() };
        let cache = match cache {
            Some(c) => c,
            None => {
                let fresh = BoardCache::fetch(&coords).await?;
                self.caches.write().await.board.insert(key, fresh.clone());
                fresh
            }
        };
        Ok((coords, cache))
    }

    #[tool(
        name = "emit_event",
        description = "Append one structured row to ~/.claude/logs/agent-actions.log. \
        Use instead of the bash `log_event` helper — this tool enforces the tab-separated \
        format, ISO-8601 timestamps with timezone offset, and strips control characters. \
        `details` is rendered as space-separated `key=value` pairs in a stable order \
        (issue, reason, detail first, then alphabetical)."
    )]
    async fn emit_event(
        &self,
        Parameters(req): Parameters<EmitEventRequest>,
    ) -> Result<CallToolResult, McpError> {
        let repo = normalize_repo_name(req.repo.as_deref());
        let details_str = format_details(&req.details);
        append_audit_log(&req.skill, &req.action, &repo, &details_str)
            .await
            .map_err(mcp)?;
        Ok(text(format!(
            "Logged: {} {} (repo={})",
            req.skill, req.action, repo
        )))
    }

    #[tool(
        name = "move_card",
        description = "Move a project board card to a specific Status. \
        Status must be one of the project's Status options (Todo, In Progress, Done). \
        Looks up project + field + option IDs once and caches them in memory; \
        subsequent calls don't re-fetch. Fails loudly if the issue isn't on the board."
    )]
    async fn move_card(
        &self,
        Parameters(req): Parameters<MoveCardRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve the board from the repo the issue lives in (auto-discovered,
        // cached). The `repo` parameter now picks the board, not just the slug.
        let repo_slug = normalize_repo_slug(req.repo.as_deref());
        let (coords, cache) = self.board_for(&repo_slug).await?;
        let option_id = cache.status_options.get(&req.status).ok_or_else(|| {
            mcp(format!(
                "unknown status '{}': known are {:?}",
                req.status,
                cache.status_options.keys().collect::<Vec<_>>()
            ))
        })?;
        let item_id = find_item_id(req.issue, &coords).await?;
        run_gh(&[
            "project",
            "item-edit",
            "--project-id",
            &cache.project_id,
            "--id",
            &item_id,
            "--field-id",
            &cache.status_field_id,
            "--single-select-option-id",
            option_id,
        ])
        .await?;
        Ok(text(format!("Moved #{} to {}", req.issue, req.status)))
    }

    #[tool(
        name = "set_blocker",
        description = "Mark an issue blocked. Enforces an OPEN-state guard (returns a skip note \
        for CLOSED issues), idempotent across a 24-hour window for the same issue+reason. \
        Adds the `blocked` label, sets the Blocker project field, and — for asset-needed \
        or external-input reasons — posts an @-mention comment on the issue (gated by an \
        HTML-comment marker so it never double-fires). `unblocker_handle` is the GitHub \
        username to @-mention; omit to skip the mention."
    )]
    async fn set_blocker(
        &self,
        Parameters(req): Parameters<SetBlockerRequest>,
    ) -> Result<CallToolResult, McpError> {
        let reason = req.reason.as_str();
        let repo_slug = normalize_repo_slug(req.repo.as_deref());
        let repo_name = normalize_repo_name(req.repo.as_deref());

        // 0. State guard — refuse to block closed issues.
        let state = gh_issue_state(req.issue, &repo_slug).await?;
        if state != "OPEN" {
            return Ok(text(format!(
                "Skipped: #{} state is {}, not OPEN",
                req.issue, state
            )));
        }

        // 1. Idempotency — bail if the same blocker was logged in 24h.
        if recent_block(req.issue, reason).await.unwrap_or(false) {
            return Ok(text(format!(
                "Already blocked: #{} reason={} within 24h",
                req.issue, reason
            )));
        }

        // 2. Emit blocked event.
        let mut d = HashMap::new();
        d.insert("issue".to_string(), format!("#{}", req.issue));
        d.insert("reason".to_string(), reason.to_string());
        d.insert("detail".to_string(), req.detail.clone());
        append_audit_log(
            "board-issue-loop",
            "blocked",
            &repo_name,
            &format_details(&d),
        )
        .await
        .map_err(mcp)?;

        // 3. Set Blocker field on the project card. Field is the structured
        // source of truth; label (step 5) is its lightweight mirror. Field
        // first so a partial failure here doesn't leave a label without a
        // backing reason.
        let (coords, cache) = self.board_for(&repo_slug).await?;
        let option_id = cache
            .blocker_options
            .get(reason)
            .ok_or_else(|| mcp(format!("blocker option '{}' missing on project", reason)))?;
        let item_id = find_item_id(req.issue, &coords).await?;
        run_gh(&[
            "project",
            "item-edit",
            "--project-id",
            &cache.project_id,
            "--id",
            &item_id,
            "--field-id",
            &cache.blocker_field_id,
            "--single-select-option-id",
            option_id,
        ])
        .await?;

        // 4. Conditional @-mention comment, only for the two reasons where
        // a specific person is the unblocker.
        if matches!(
            req.reason,
            BlockerReason::AssetNeeded | BlockerReason::ExternalInput
        ) && !has_blocker_comment(req.issue, &repo_slug)
            .await
            .unwrap_or(false)
        {
            let mention = req
                .unblocker_handle
                .as_deref()
                .map(|h| format!("@{} when you have a moment, ", h))
                .unwrap_or_default();
            let body = format!(
                "Marking this blocked — {detail}. (`Blocker: {reason}` on the board.)\n\n{mention}this needs your input before we can move forward.\n\n<!-- agent-blocker-filed -->",
                detail = req.detail,
                reason = reason,
            );
            run_gh(&[
                "issue",
                "comment",
                &req.issue.to_string(),
                "--repo",
                &repo_slug,
                "--body",
                &body,
            ])
            .await?;
        }

        // 5. Sync the `blocked` label. Paired with the Blocker field — set
        // last so a failed field-set doesn't leave a dangling label.
        // Idempotent: gh ignores re-adding an existing label.
        run_gh(&[
            "issue",
            "edit",
            &req.issue.to_string(),
            "--repo",
            &repo_slug,
            "--add-label",
            "blocked",
        ])
        .await?;

        // 6. Push a heads-up to the supervisor's fleet inbox, so a block —
        // especially one needing their input — surfaces actively instead of
        // only as a dashboard state change they might miss. Best-effort.
        let _ = fleet::notify_supervisor(
            &repo_name,
            &format!("Blocked: #{} ({})", req.issue, reason),
            &format!(
                "{} — {}. Needs your input to proceed (board card + `blocked` label set).",
                reason, req.detail
            ),
        )
        .await;

        Ok(text(format!(
            "Blocked #{} (reason={}, detail={})",
            req.issue, reason, req.detail
        )))
    }

    #[tool(
        name = "clear_blocker",
        description = "Resolve a previously-set blocker. Emits `unblocked`, clears the \
        Blocker project field via GraphQL (gh project item-edit has no `--clear` for \
        single-select), and removes the `blocked` label. Safe to call when no blocker \
        is set (each step is idempotent)."
    )]
    async fn clear_blocker(
        &self,
        Parameters(req): Parameters<ClearBlockerRequest>,
    ) -> Result<CallToolResult, McpError> {
        let repo_slug = normalize_repo_slug(req.repo.as_deref());
        let repo_name = normalize_repo_name(req.repo.as_deref());

        // 1. Emit unblocked event.
        let mut d = HashMap::new();
        d.insert("issue".to_string(), format!("#{}", req.issue));
        append_audit_log(
            "board-issue-loop",
            "unblocked",
            &repo_name,
            &format_details(&d),
        )
        .await
        .map_err(mcp)?;

        // 2. Clear Blocker field via GraphQL.
        let (coords, cache) = self.board_for(&repo_slug).await?;
        let item_id = find_item_id(req.issue, &coords).await?;
        run_gh(&[
            "api",
            "graphql",
            "-f",
            "query=mutation($project: ID!, $item: ID!, $field: ID!) { clearProjectV2ItemFieldValue(input: { projectId: $project, itemId: $item, fieldId: $field }) { clientMutationId } }",
            "-f",
            &format!("project={}", cache.project_id),
            "-f",
            &format!("item={}", item_id),
            "-f",
            &format!("field={}", cache.blocker_field_id),
        ])
        .await?;

        // 3. Remove the blocked label. `gh issue edit --remove-label` errors
        // if the label isn't currently set; treat that as success.
        let _ = run_gh(&[
            "issue",
            "edit",
            &req.issue.to_string(),
            "--repo",
            &repo_slug,
            "--remove-label",
            "blocked",
        ])
        .await;

        Ok(text(format!("Cleared blocker for #{}", req.issue)))
    }

    #[tool(
        name = "sweep_agent_prs",
        description = "List open agent-authored PRs as a compact one-row-per-PR table. \
        Replaces ~5 gh calls per PR (state + comments + checks + behind-main) with one tool call \
        and one small text block in the agent's context. Each row: `#N  draft=<bool>  unanswered=<n>  \
        failing=<n>  behind=<n>  branch=...  sha=...`. Filtered to agent-authored PRs: \
        branch matches one of the conventional commit prefixes (fix|feat|chore|docs|test|refactor) \
        AND the PR body carries the `🤖 Generated with [Claude Code]` footer. Use \
        `comment_threads(pr)` and `comment_body(comment_id)` to drill into a specific PR — those \
        tools fold suggestion-fence bodies by default and let the agent fetch the full text only \
        when it decides to apply. With no `repo`, sweeps every registered agent's repo and unions \
        the results (pass `repo` to scope to a single one); any agent PR not already in the event \
        log is registered so it surfaces in the cockpit's dashboard."
    )]
    async fn sweep_agent_prs(
        &self,
        Parameters(req): Parameters<SweepAgentPrsRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Which repos to sweep. An explicit `repo` scopes to that one
        // (unchanged). With no `repo`, sweep every *registered agent's*
        // repo and union — so the cockpit sees every agent's PRs, not just
        // the default repo's. Falls back to the default when the registry
        // is empty/unreadable (fresh environment → v0.1 behaviour).
        let repos: Vec<String> = match req.repo.as_deref() {
            Some(r) => vec![normalize_repo_slug(Some(r))],
            None => {
                let mut rs: Vec<String> = match fleet::list_agents().await {
                    Ok(agents) => agents
                        .into_iter()
                        // Skip the human, repo-less agents, and glob repos
                        // (e.g. code-review's `acme/*` — not a single
                        // sweepable repo).
                        .filter(|a| {
                            a.kind == "agent"
                                && !a.repo.is_empty()
                                && a.repo != "-"
                                && !a.repo.contains('*')
                        })
                        .map(|a| normalize_repo_slug(Some(&a.repo)))
                        .collect(),
                    Err(_) => Vec::new(),
                };
                rs.sort();
                rs.dedup();
                // Fall back to the configured default repo (if any) when no
                // agents are registered yet; otherwise sweep nothing.
                let default = default_repo_slug();
                if rs.is_empty() && !default.is_empty() {
                    rs.push(default);
                }
                rs
            }
        };

        // PRs already in the event log (any repo), so the back-fill below
        // only emits for PRs the cockpit hasn't seen — idempotent across
        // repeated sweeps.
        let logged = read_logged_prs().await;
        // PRs already linked to a shared task (any of them), so we can flag the
        // open PRs that AREN'T — nudging the agent to link them (or add a
        // `Fixes #N`) so the shared-task reconcile can track them.
        let linked_to_task: std::collections::HashSet<(String, u64)> = fleet::list_shared_tasks()
            .await
            .iter()
            .flat_map(|t| {
                t.prs
                    .iter()
                    .map(|r| (normalize_repo_slug(Some(r.repo.as_str())), r.number))
            })
            .collect();

        let multi = repos.len() > 1;
        let mut out = String::new();
        let mut emitted = 0usize;
        let mut reconciled = 0usize;
        let mut unlinked = 0usize;
        for repo_slug in &repos {
            if multi {
                out.push_str(&format!("== {repo_slug} ==\n"));
            }
            let rows = match sweep_one_repo(repo_slug).await {
                Ok(rows) => rows,
                Err(e) => {
                    out.push_str(&format!("  (sweep failed: {e})\n"));
                    continue;
                }
            };
            if rows.is_empty() {
                out.push_str("(no open agent-authored PRs)\n");
            }
            for (number, branch, sha, is_draft, unanswered, failing, behind) in &rows {
                out.push_str(&format!(
                    "#{number}  draft={is_draft}  unanswered={unanswered}  failing={failing}  behind={behind}  branch={branch}  sha={short}",
                    short = &sha[..sha.len().min(8)],
                ));
                if !linked_to_task.contains(&(repo_slug.clone(), *number)) {
                    out.push_str("  ⚠ no shared-task link");
                    unlinked += 1;
                }
                out.push('\n');
                // Back-fill: register PRs the cockpit hasn't logged yet so
                // they surface in the dashboard. `pr-opened` keys a PR-only
                // work item (InReview); a non-draft PR also gets
                // `ready-flipped` so it lands in the right state. The full
                // slug in the repo column lets the cockpit attribute it to
                // the owning client.
                if !logged.all.contains(&(repo_slug.clone(), *number)) {
                    let _ = append_audit_log(
                        "board-sweep",
                        "pr-opened",
                        repo_slug,
                        &format!("pr=#{number}"),
                    )
                    .await;
                    if !is_draft {
                        let _ = append_audit_log(
                            "board-sweep",
                            "ready-flipped",
                            repo_slug,
                            &format!("pr=#{number}"),
                        )
                        .await;
                    }
                    emitted += 1;
                }
            }

            // Reconcile stale cards: any *active* card for this repo that's no
            // longer in the live open set was merged or closed outside an agent
            // loop (e.g. a merge done through the GitHub UI). Re-query each and
            // emit a terminal event so the cockpit clears it. Scoped to the
            // swept repo, so an un-swept repo's cards are never touched. The
            // stale set is normally tiny, and a card drops out of it for good
            // once its terminal event lands — so the gh calls don't pile up.
            let open_now: std::collections::HashSet<u64> = rows.iter().map(|r| r.0).collect();
            let mut stale: Vec<u64> = logged
                .active
                .iter()
                .filter(|(r, _)| r == repo_slug)
                .map(|(_, n)| *n)
                .filter(|n| !open_now.contains(n))
                .collect();
            stale.sort_unstable();
            for n in stale {
                match pr_state(n, repo_slug).await {
                    Ok(PrState::Merged) => {
                        let _ = append_audit_log(
                            "board-sweep",
                            "pr-merged",
                            repo_slug,
                            &format!("pr=#{n}"),
                        )
                        .await;
                        out.push_str(&format!("  reconciled #{n}: merged → card cleared\n"));
                        reconciled += 1;
                    }
                    Ok(PrState::Closed) => {
                        let _ = append_audit_log(
                            "board-sweep",
                            "card-done",
                            repo_slug,
                            &format!("pr=#{n}"),
                        )
                        .await;
                        out.push_str(&format!("  reconciled #{n}: closed → card retired\n"));
                        reconciled += 1;
                    }
                    // Still open (e.g. no longer agent-authored) or the query
                    // failed — leave the card as-is.
                    Ok(PrState::Open) | Err(_) => {}
                }
            }
        }
        if emitted > 0 {
            out.push_str(&format!(
                "\n({emitted} new PR{} surfaced to the cockpit)\n",
                if emitted == 1 { "" } else { "s" }
            ));
        }
        if reconciled > 0 {
            out.push_str(&format!(
                "({reconciled} stale card{} reconciled out)\n",
                if reconciled == 1 { "" } else { "s" }
            ));
        }
        if unlinked > 0 {
            out.push_str(&format!(
                "\n⚠ {unlinked} open PR(s) not linked to a shared task — add \"Fixes #<issue>\" (the reconcile auto-links those) or call link_shared_task(id, pr).\n"
            ));
        }
        Ok(text(out))
    }

    #[tool(
        name = "comment_threads",
        description = "List every review comment on a PR in compact form — one row per comment, \
        with id / author / type (inline|top) / path:line for inline / 80-char preview / total body chars. \
        Folds code-suggestion fences and any body > preview length so the agent's context stays small. \
        Use `comment_body(comment_id)` to fetch the full text + suggestion when the agent decides \
        to apply a specific comment. Includes both inline review comments and top-level PR thread comments."
    )]
    async fn comment_threads(
        &self,
        Parameters(req): Parameters<CommentThreadsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let repo_slug = normalize_repo_slug(req.repo.as_deref());
        let inline_json = run_gh(&[
            "api",
            "--paginate",
            &format!("repos/{}/pulls/{}/comments", repo_slug, req.pr),
        ])
        .await?;
        let top_json = run_gh(&[
            "api",
            "--paginate",
            &format!("repos/{}/issues/{}/comments", repo_slug, req.pr),
        ])
        .await?;

        let inline: serde_json::Value = serde_json::from_str(&inline_json).map_err(mcp)?;
        let top: serde_json::Value = serde_json::from_str(&top_json).map_err(mcp)?;
        let empty = Vec::new();

        let mut out = String::new();
        let inline_arr = inline.as_array().unwrap_or(&empty);
        let top_arr = top.as_array().unwrap_or(&empty);
        if inline_arr.is_empty() && top_arr.is_empty() {
            return Ok(text(format!("(no comments on PR #{})", req.pr)));
        }

        for c in inline_arr {
            let id = c["id"].as_u64().unwrap_or(0);
            let author = c["user"]["login"].as_str().unwrap_or("?");
            let body = c["body"].as_str().unwrap_or("");
            let path = c["path"].as_str().unwrap_or("?");
            let line = c["line"]
                .as_u64()
                .or_else(|| c["original_line"].as_u64())
                .unwrap_or(0);
            out.push_str(&format!(
                "{id}  inline  {author:<22}  {path}:{line}  body_chars={body_chars}  preview={preview:?}\n",
                body_chars = body.chars().count(),
                preview = preview_body(body, 80),
            ));
        }
        for c in top_arr {
            let id = c["id"].as_u64().unwrap_or(0);
            let author = c["user"]["login"].as_str().unwrap_or("?");
            let body = c["body"].as_str().unwrap_or("");
            out.push_str(&format!(
                "{id}  top     {author:<22}  —                                    body_chars={body_chars}  preview={preview:?}\n",
                body_chars = body.chars().count(),
                preview = preview_body(body, 80),
            ));
        }
        Ok(text(out))
    }

    #[tool(
        name = "mark_ready",
        description = "Flip a draft PR to ready-for-review. ENFORCES the skill's `Don't mark \
        ready unless explicitly delegated` rule at the tool layer: refuses with an error if \
        `by_supervisor` is false. This means the agent can never auto-flip — the supervisor's \
        explicit consent is encoded in the call itself, not just in skill prose the LLM has to \
        remember. Emits `ready-flipped pr=#<N> by=supervisor-delegated` on success."
    )]
    async fn mark_ready(
        &self,
        Parameters(req): Parameters<MarkReadyRequest>,
    ) -> Result<CallToolResult, McpError> {
        if !req.by_supervisor {
            return Err(mcp(
                "Refusing to mark ready: by_supervisor must be true. The supervisor must \
                 explicitly delegate this action — the agent never flips on its own initiative.",
            ));
        }
        let repo_slug = normalize_repo_slug(req.repo.as_deref());
        let repo_name = normalize_repo_name(req.repo.as_deref());

        run_gh(&["pr", "ready", &req.pr.to_string(), "--repo", &repo_slug]).await?;

        let mut d = HashMap::new();
        d.insert("pr".to_string(), format!("#{}", req.pr));
        d.insert("by".to_string(), "supervisor-delegated".to_string());
        append_audit_log(
            "pr-review-fixup",
            "ready-flipped",
            &repo_name,
            &format_details(&d),
        )
        .await
        .map_err(mcp)?;

        Ok(text(format!(
            "Marked #{} ready (supervisor-delegated)",
            req.pr
        )))
    }

    #[tool(
        name = "file_followup_issue",
        description = "Create a follow-up issue for an out-of-scope review comment, add it to \
        the project board, and emit the `followup-filed` event. Returns the new issue number \
        and URL. The body is taken as-is — the caller is expected to include the parent-PR \
        reference (the skill convention is `Surfaced as a follow-up from #<PR>.` at the top). \
        Auto-adds to acme project #14 in the default column (Backlog/Todo per the project's \
        workflow); the agent doesn't pre-position the card on the board."
    )]
    async fn file_followup_issue(
        &self,
        Parameters(req): Parameters<FileFollowupIssueRequest>,
    ) -> Result<CallToolResult, McpError> {
        let repo_slug = normalize_repo_slug(req.repo.as_deref());
        let repo_name = normalize_repo_name(req.repo.as_deref());

        // 1. Create the issue. `gh issue create` prints the URL on stdout
        // on success.
        let url_stdout = run_gh(&[
            "issue", "create", "--repo", &repo_slug, "--title", &req.title, "--body", &req.body,
        ])
        .await?;
        let url = url_stdout.trim();
        let issue_number = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| {
                mcp(format!(
                    "could not parse issue number from `gh` output: {url:?}"
                ))
            })?;

        // 2. Add to the repo's board (auto-discovered from the repo it was
        // filed in).
        let (coords, _) = self.board_for(&repo_slug).await?;
        run_gh(&[
            "project",
            "item-add",
            &coords.number.to_string(),
            "--owner",
            &coords.owner,
            "--url",
            url,
        ])
        .await?;

        // 3. Emit the event.
        let mut d = HashMap::new();
        d.insert("pr".to_string(), format!("#{}", req.parent_pr));
        d.insert("new_issue".to_string(), format!("#{}", issue_number));
        append_audit_log(
            "pr-review-fixup",
            "followup-filed",
            &repo_name,
            &format_details(&d),
        )
        .await
        .map_err(mcp)?;

        Ok(text(format!(
            "Filed #{} (from PR #{}): {}",
            issue_number, req.parent_pr, url
        )))
    }

    #[tool(
        name = "comment_body",
        description = "Return the full body of a single review comment by its ID. Use this when \
        the agent decides to apply or quote-reply to a specific comment surfaced by `comment_threads`. \
        Tries the inline-comments endpoint first, then top-level. Returns the raw markdown including \
        any ```suggestion fence, so the agent can apply it verbatim."
    )]
    async fn comment_body(
        &self,
        Parameters(req): Parameters<CommentBodyRequest>,
    ) -> Result<CallToolResult, McpError> {
        let id = req.comment_id;
        let repo_slug = normalize_repo_slug(req.repo.as_deref());
        if let Ok(json) =
            run_gh(&["api", &format!("repos/{}/pulls/comments/{}", repo_slug, id)]).await
        {
            let v: serde_json::Value = serde_json::from_str(&json).map_err(mcp)?;
            if let Some(body) = v["body"].as_str() {
                return Ok(text(body.to_string()));
            }
        }
        if let Ok(json) = run_gh(&[
            "api",
            &format!("repos/{}/issues/comments/{}", repo_slug, id),
        ])
        .await
        {
            let v: serde_json::Value = serde_json::from_str(&json).map_err(mcp)?;
            if let Some(body) = v["body"].as_str() {
                return Ok(text(body.to_string()));
            }
        }
        Err(mcp(format!(
            "comment {} not found (tried inline + top-level)",
            id
        )))
    }

    // ---- fleet multi-agent coordination -------------------------------
    //
    // These write the shared `~/.claude/lakitu-fleet/` store that the
    // `lakitu` TUI renders (agents pane + inboxes). See
    // `src/fleet.rs` and `lakitu/DESIGN.md` for the contract.

    #[tool(
        name = "register_agent",
        description = "Register this agent in the shared fleet store so it shows up in the \
        lakitu TUI agents pane and is visible to peers via `list_agents`. Writes \
        ~/.claude/lakitu-fleet/agents/<name>.json and creates the agent's inbox. Call once at \
        startup. `name` is your stable identity + address — a human-friendly handle (kebab-case), \
        free to pick (need not match the repo), but keep it consistent and export it as \
        LAKITU_FLEET_NAME so the inbox/presence hooks find you. `role` is a short function label \
        (e.g. 'code review', 'scan backend'), distinct from the name. `repo`/`board` are \
        free-form labels (board convention: '<owner>/<projectNumber>')."
    )]
    async fn register_agent(
        &self,
        Parameters(req): Parameters<RegisterAgentRequest>,
    ) -> Result<CallToolResult, McpError> {
        let name = fleet::register(
            &req.name,
            &req.repo,
            &req.board,
            req.description.as_deref(),
            req.role.as_deref(),
        )
        .await
        .map_err(mcp)?;
        Ok(text(format!(
            "Registered agent '{name}'{} (repo={}, board={}){}",
            req.role
                .as_deref()
                .map(|r| format!(" [{r}]"))
                .unwrap_or_default(),
            req.repo,
            req.board,
            req.description
                .as_deref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default()
        )))
    }

    #[tool(
        name = "rename_agent",
        description = "Rename an agent: move its registry + inbox (keeping unread AND the read \
        archive) to the new handle and drop the old entry, so identity + history survive a \
        rename instead of rm-ing store files by hand. Refuses if the new name is already taken."
    )]
    async fn rename_agent(
        &self,
        Parameters(req): Parameters<RenameAgentRequest>,
    ) -> Result<CallToolResult, McpError> {
        let new = fleet::rename_agent(&req.old_name, &req.new_name)
            .await
            .map_err(mcp)?;
        Ok(text(format!(
            "Renamed '{}' → '{}'",
            fleet::sanitize(&req.old_name),
            new
        )))
    }

    #[tool(
        name = "deregister_agent",
        description = "Remove an agent from the store entirely — registry, heartbeat, and inbox. \
        Clean teardown (pairs with register_agent) for retiring an agent. Drops inbox history; \
        use rename_agent instead if you're renaming and want to keep it."
    )]
    async fn deregister_agent(
        &self,
        Parameters(req): Parameters<DeregisterAgentRequest>,
    ) -> Result<CallToolResult, McpError> {
        let name = fleet::deregister_agent(&req.name).await.map_err(mcp)?;
        Ok(text(format!(
            "Deregistered '{name}' (registry + inbox removed)"
        )))
    }

    #[tool(
        name = "heartbeat",
        description = "Update this agent's presence in the fleet store: `state` is one of \
        idle / working / blocked / waiting, and `task` is an optional one-line description of \
        what you're doing right now. Use `blocked` when you need the SUPERVISOR (drives the \
        cockpit's 'needs you' alert — pair with notify_supervisor); use `waiting` when you're \
        stuck on a PEER or external event (another client's release, CI) — shown as a calm ◐, \
        no alert. Call at step boundaries (after picking an issue, when blocked/waiting, when \
        going idle) so the TUI shows live status. Overwrites the previous heartbeat. Agents \
        with no heartbeat in 15 minutes render as stale."
    )]
    async fn heartbeat(
        &self,
        Parameters(req): Parameters<HeartbeatRequest>,
    ) -> Result<CallToolResult, McpError> {
        let name = fleet::heartbeat(&req.name, req.state.as_str(), req.task.as_deref())
            .await
            .map_err(mcp)?;
        Ok(text(format!(
            "Heartbeat: '{name}' is {}{}",
            req.state.as_str(),
            req.task.map(|t| format!(" — {t}")).unwrap_or_default()
        )))
    }

    #[tool(
        name = "send_message",
        description = "Send a message to another agent's inbox. It appears in that agent's \
        inbox in the TUI and is returned by their `read_inbox`. Use for coordination, \
        questions, or feature requests across repos (e.g. ask the api agent to expose \
        a new tool). `from` is your agent name, `to` is the recipient's name (see \
        `list_agents`), `title` is a short subject, `body` is the full message. The recipient \
        need not be online — the message waits until they read it."
    )]
    async fn send_message(
        &self,
        Parameters(req): Parameters<SendMessageRequest>,
    ) -> Result<CallToolResult, McpError> {
        let id = fleet::send_message(&req.from, &req.to, &req.title, &req.body)
            .await
            .map_err(mcp)?;
        Ok(text(format!(
            "Sent message {id} from '{}' to '{}': {}",
            req.from,
            fleet::sanitize(&req.to),
            req.title
        )))
    }

    #[tool(
        name = "read_inbox",
        description = "Read this agent's inbox, newest first. By default (`mark_read` omitted \
        or true) the returned messages are moved to the read archive so they aren't \
        reprocessed on the next call; pass mark_read=false to peek without consuming. Call at \
        loop boundaries to pick up requests from other agents. Each message shows time, \
        sender, title, and body."
    )]
    async fn read_inbox(
        &self,
        Parameters(req): Parameters<ReadInboxRequest>,
    ) -> Result<CallToolResult, McpError> {
        let mark = req.mark_read.unwrap_or(true);
        let msgs = fleet::read_inbox(&req.name, mark).await.map_err(mcp)?;
        if msgs.is_empty() {
            return Ok(text(format!(
                "Inbox empty for '{}'.",
                fleet::sanitize(&req.name)
            )));
        }
        let mut out = String::new();
        for m in &msgs {
            out.push_str(&format!(
                "[{time}] {title}  (from {from})\n  {body}\n\n",
                time = m.time,
                title = m.title,
                from = m.from,
                body = m.body.replace('\n', "\n  "),
            ));
        }
        out.push_str(&format!(
            "({} message{}{})",
            msgs.len(),
            if msgs.len() == 1 { "" } else { "s" },
            if mark {
                " — moved to read archive"
            } else {
                " — left unread"
            },
        ));
        Ok(text(out))
    }

    #[tool(
        name = "wait_for_message",
        description = "Block until an unread message lands in this agent's inbox, or until \
        `timeout_sec` elapses — then return a compact summary WITHOUT archiving, so the resumed \
        turn can call read_inbox to consume + read bodies normally. For a resident agent (e.g. \
        Codex) that wants to PARK its current turn and resume it in-thread the moment mail \
        arrives, rather than spawning a throwaway wake that loses chat context. Returns at once \
        if unread mail already exists; otherwise polls the inbox about every 2s. IMPORTANT: this \
        holds the tool call open for the whole wait. To survive a client whose per-call watchdog \
        fires on inactivity (e.g. Codex at ~120s), it emits a progress notification about every 25s \
        while parked (`still_waiting elapsed=Ns`) to keep the channel active — automatic, no caller \
        setup or progressToken needed. (A client with a hard wall-clock per-request cap still needs \
        that cap to exceed `timeout_sec` — park-and-repeat for longer idles.) While parked, consider \
        a heartbeat with state=waiting, task=\"waiting for inbox\". Default timeout 300s; capped at 3600s."
    )]
    async fn wait_for_message(
        &self,
        Parameters(req): Parameters<WaitForMessageRequest>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        const MAX_WAIT: u64 = 3600; // hard cap (1h) regardless of request
        const POLL: u64 = 2; // seconds between inbox checks
        const PING: u64 = 25; // seconds between keepalive progress notifications
        let timeout = req.timeout_sec.unwrap_or(300).min(MAX_WAIT);
        let name = fleet::sanitize(&req.name);
        // A progress token to address keepalive notifications to: the caller's
        // if it supplied one, else a generated one. Codex's tool wrapper can't
        // pass a progressToken, so without this fallback its long parks get no
        // keepalive traffic and its ~120s inactivity watchdog aborts the call.
        // The token mainly labels the notification; the periodic traffic itself
        // is what keeps the channel active (over stdio it's written regardless).
        let progress_token = ctx.meta.get_progress_token().unwrap_or_else(|| {
            ProgressToken(NumberOrString::String(Arc::from(format!("waitmsg:{name}"))))
        });

        let mut waited = 0u64;
        let mut last_ping = 0u64;
        loop {
            // Peek WITHOUT consuming: leave the mail unread so the agent's
            // resumed turn triages it normally via read_inbox.
            let msgs = fleet::read_inbox(&req.name, false).await.map_err(mcp)?;
            if !msgs.is_empty() {
                let mut out = format!("status=message  unread={}  inbox='{name}'\n", msgs.len());
                for m in &msgs {
                    out.push_str(&format!(
                        "  [{time}] {title}  (from {from})  id={id}\n",
                        time = m.time,
                        title = m.title,
                        from = m.from,
                        id = m.id,
                    ));
                }
                out.push_str("(left unread — call read_inbox to consume + read bodies)");
                return Ok(text(out));
            }
            if waited >= timeout {
                return Ok(text(format!(
                    "status=timeout  unread=0  inbox='{name}'  (waited {timeout}s, no new mail)"
                )));
            }
            // Keepalive: a periodic progress notification keeps an
            // inactivity-based client watchdog from firing during a long, quiet
            // wait. Always emitted (token generated above if the caller gave
            // none). Best-effort — ignore send errors.
            if waited - last_ping >= PING {
                let _ = ctx
                    .peer
                    .send_notification(ServerNotification::ProgressNotification(
                        ProgressNotification::new(ProgressNotificationParam {
                            progress_token: progress_token.clone(),
                            progress: waited as f64,
                            total: Some(timeout as f64),
                            message: Some(format!("still_waiting elapsed={waited}s unread=0")),
                        }),
                    ))
                    .await;
                last_ping = waited;
            }
            let step = POLL.min(timeout - waited);
            tokio::time::sleep(std::time::Duration::from_secs(step)).await;
            waited += step;
        }
    }

    #[tool(
        name = "broadcast",
        description = "Send one message to EVERY other registered client's inbox at once (a \
        group announcement / recap). Same as send_message but without `to` — it fans out to \
        all current agents and the supervisor. Use sparingly, for things the whole group \
        should see (e.g. a heads-up, or a milestone recap to the supervisor + peers)."
    )]
    async fn broadcast(
        &self,
        Parameters(req): Parameters<BroadcastRequest>,
    ) -> Result<CallToolResult, McpError> {
        let n = fleet::broadcast(&req.from, &req.title, &req.body)
            .await
            .map_err(mcp)?;
        Ok(text(format!(
            "Broadcast '{}' from '{}' to {} recipient(s)",
            req.title, req.from, n
        )))
    }

    #[tool(
        name = "notify_supervisor",
        description = "Send a short recap / TLDR to the human supervisor(s) — the `human` \
        client(s) in the store. Use at milestones (PR opened/merged, blocked, a unit of work \
        finished) so the supervisor can follow progress from their inbox without reading \
        everything. `from` is your name; `title` a one-line summary; `body` the recap (what \
        changed + why + current state + any ask). Finds the supervisor automatically — no need \
        to know their name. No-op (with a note) if no human is registered."
    )]
    async fn notify_supervisor(
        &self,
        Parameters(req): Parameters<NotifySupervisorRequest>,
    ) -> Result<CallToolResult, McpError> {
        let names = fleet::notify_supervisor(&req.from, &req.title, &req.body)
            .await
            .map_err(mcp)?;
        if names.is_empty() {
            return Ok(text(
                "No supervisor registered (no human client) — recap not sent.".to_string(),
            ));
        }
        Ok(text(format!(
            "Recap '{}' sent to: {}",
            req.title,
            names.join(", ")
        )))
    }

    #[tool(
        name = "list_agents",
        description = "List all registered agents in the fleet store with their current \
        state (idle/working/blocked/unknown), repo, board, current task, and unread-message \
        count. Use this for awareness of which peers exist and what they're doing before \
        sending a message."
    )]
    async fn list_agents(
        &self,
        Parameters(_req): Parameters<ListAgentsRequest>,
    ) -> Result<CallToolResult, McpError> {
        let agents = fleet::list_agents().await.map_err(mcp)?;
        if agents.is_empty() {
            return Ok(text("(no agents registered)".to_string()));
        }
        let mut out = String::new();
        for a in &agents {
            out.push_str(&format!(
                "{name}  kind={kind}{role}  state={state}  repo={repo}  board={board}  unread={unread}{help}{seen}{task}\n",
                name = a.name,
                kind = a.kind,
                role = a.role.as_ref().map(|r| format!("  role=\"{r}\"")).unwrap_or_default(),
                state = a.state,
                repo = a.repo,
                board = a.board,
                unread = a.unread,
                help = a.description.as_ref().map(|d| format!("  helps=\"{d}\"")).unwrap_or_default(),
                seen = a.last_seen.as_ref().map(|s| format!("  seen={s}")).unwrap_or_default(),
                task = a.task.as_ref().map(|t| format!("  task={t}")).unwrap_or_default(),
            ));
        }
        Ok(text(out))
    }

    // ---- persona: identity + relationships ----------------------------
    //
    // Self-authored identity + private peer-notes, persisted in the fleet
    // store and auto-loaded back into context at the start of every session
    // (incl. after a compact) by the SessionStart hook
    // (`persona-sessionstart.sh`). See `src/persona.rs`.

    #[tool(
        name = "set_identity",
        description = "Save (or update) THIS agent's persona — the self-card that makes you *you* \
        across sessions. Persisted to ~/.claude/lakitu-fleet/personas/<name>/ and auto-loaded back \
        into your context at the start of every session (including after a compaction) by the \
        SessionStart hook, so you resume being the same character instead of re-inventing yourself \
        each time. `tagline` is a one-line essence (e.g. 'methodical backend gremlin, allergic to \
        flaky tests'); `bio` is freeform markdown — how you work, your voice, what you care about. \
        Partial updates are fine: omit a field to keep its previous value. Call once you've picked \
        your name/character, then whenever who-you-are meaningfully shifts. Self-cards are PUBLIC — \
        peers read yours via get_identity."
    )]
    async fn set_identity(
        &self,
        Parameters(req): Parameters<SetIdentityRequest>,
    ) -> Result<CallToolResult, McpError> {
        let name = persona::set_identity(&req.name, req.tagline.as_deref(), req.bio.as_deref())
            .await
            .map_err(mcp)?;
        Ok(text(format!("Saved persona for '{name}'.")))
    }

    #[tool(
        name = "get_identity",
        description = "Read another agent's self-card (their public persona prose), so you know who \
        you're working with before you message or rely on them. Returns their identity.md, or a \
        note if they haven't written one yet. Pairs with list_agents: that tells you who exists, \
        this tells you who they *are*."
    )]
    async fn get_identity(
        &self,
        Parameters(req): Parameters<GetIdentityRequest>,
    ) -> Result<CallToolResult, McpError> {
        match persona::get_identity(&req.name).await.map_err(mcp)? {
            Some(card) => Ok(text(card)),
            None => Ok(text(format!(
                "'{}' hasn't written a persona yet.",
                fleet::sanitize(&req.name)
            ))),
        }
    }

    #[tool(
        name = "remember_peer",
        description = "Record a private, dated note about a PEER — what you've learned about how \
        they work, what they're good at, how you get along. This is your own memory (stored under \
        your persona, not theirs) and accumulates over time, so your relationships have history and \
        survive across sessions. Append one observation per call. Optional `affinity` (-5..=+5) \
        captures the rapport (−5 friction, +5 great rapport). Use whenever a collaboration teaches \
        you something about a teammate."
    )]
    async fn remember_peer(
        &self,
        Parameters(req): Parameters<RememberPeerRequest>,
    ) -> Result<CallToolResult, McpError> {
        let peer = persona::remember_peer(&req.name, &req.peer, &req.note, req.affinity)
            .await
            .map_err(mcp)?;
        Ok(text(format!("Noted about '{peer}'.")))
    }

    #[tool(
        name = "recall_peers",
        description = "Read back ALL your private peer-notes (every teammate you've logged, with \
        the full note history). Your session already auto-loads these at startup, so use this \
        mid-session to refresh — e.g. before reaching out to someone — or to review who you know."
    )]
    async fn recall_peers(
        &self,
        Parameters(req): Parameters<RecallPeersRequest>,
    ) -> Result<CallToolResult, McpError> {
        let peers = persona::recall_peers(&req.name).await.map_err(mcp)?;
        if peers.is_empty() {
            return Ok(text(format!(
                "No peer notes yet for '{}'.",
                fleet::sanitize(&req.name)
            )));
        }
        let mut out = String::new();
        for (peer, notes) in &peers {
            out.push_str(&format!("== {peer} ==\n{}\n\n", notes.trim()));
        }
        Ok(text(out))
    }

    // ---- tasks: a private per-agent reminder list ---------------------
    //
    // The work-equivalent of the persona: jot a reminder so a message that
    // lands mid-task isn't forgotten, and have it survive compaction (the
    // SessionStart hook re-injects your open tasks). Deliberately NOT a GitHub
    // issue — issues are the durable, shared, reviewable unit of work; a task
    // is your own scratchpad. Rendered live in the cockpit (a checklist under
    // you; PR-linked tasks nest under that PR's row). See `src/fleet.rs`.

    #[tool(
        name = "add_task",
        description = "Add a private reminder to YOUR OWN task list — a lightweight to-do so a \
        message or idea that lands mid-work isn't forgotten. Your open tasks are re-injected into \
        your context at the start of every session (incl. after a compaction) and shown live in \
        the cockpit. This is NOT a GitHub issue: use an issue for durable, shared, reviewable work; \
        use a task for in-the-moment 'don't forget X' notes. `text` is the one-line title; \
        optionally add a longer `body` (the note/details — it reads like an inbox entry in the \
        cockpit). Optionally attach it to a PR (`pr_repo`+`pr_number`) — it then renders as a \
        subtree of that PR in the cockpit — and/or record the inbox message it came from \
        (`from_msg`). Returns the new task id."
    )]
    async fn add_task(
        &self,
        Parameters(req): Parameters<AddTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let pr = match (req.pr_repo.as_deref(), req.pr_number) {
            (Some(repo), Some(number)) if !repo.trim().is_empty() => Some(fleet::TaskPr {
                repo: repo.to_string(),
                number,
            }),
            _ => None,
        };
        let task = fleet::add_task(
            &req.name,
            &req.text,
            req.body.clone(),
            pr,
            req.from_msg.clone(),
        )
        .await
        .map_err(mcp)?;
        let pr_note = task
            .pr
            .as_ref()
            .map(|p| format!(" [{}#{}]", p.repo, p.number))
            .unwrap_or_default();
        Ok(text(format!(
            "Added task {id}{pr_note}: {text}",
            id = task.id,
            text = task.text
        )))
    }

    #[tool(
        name = "read_tasks",
        description = "Read YOUR OWN task list (open tasks only by default; pass include_done=true \
        to see completed ones too). Call at loop boundaries — same discipline as read_inbox — to \
        pick up reminders you jotted earlier. Each row shows a checkbox, the task id (for \
        complete_task / drop_task), the text, and any attached PR or source message."
    )]
    async fn read_tasks(
        &self,
        Parameters(req): Parameters<ReadTasksRequest>,
    ) -> Result<CallToolResult, McpError> {
        let include_done = req.include_done.unwrap_or(false);
        let all = fleet::read_tasks(&req.name).await;
        let shown: Vec<&fleet::Task> = all.iter().filter(|t| include_done || !t.done).collect();
        if shown.is_empty() {
            return Ok(text(format!(
                "No {}tasks for '{}'.",
                if include_done { "" } else { "open " },
                fleet::sanitize(&req.name)
            )));
        }
        let mut out = String::new();
        for t in &shown {
            let check = if t.done { "[x]" } else { "[ ]" };
            let pr =
                t.pr.as_ref()
                    .map(|p| format!("  ({}#{})", p.repo, p.number))
                    .unwrap_or_default();
            let from = t
                .from_msg
                .as_ref()
                .map(|m| format!("  (from msg {m})"))
                .unwrap_or_default();
            out.push_str(&format!(
                "{check} {id}  {text}{pr}{from}\n",
                id = t.id,
                text = t.text
            ));
        }
        let open = all.iter().filter(|t| !t.done).count();
        let done = all.len() - open;
        out.push_str(&format!("({open} open, {done} done)"));
        Ok(text(out))
    }

    #[tool(
        name = "complete_task",
        description = "Mark one of YOUR tasks done by its id (from read_tasks). It stays in the \
        list as completed (so the cockpit can show recent history) rather than vanishing — use \
        drop_task to remove it entirely."
    )]
    async fn complete_task(
        &self,
        Parameters(req): Parameters<TaskIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let ok = fleet::set_task_done(&req.name, &req.id, true)
            .await
            .map_err(mcp)?;
        Ok(text(if ok {
            format!("Completed task {}.", req.id)
        } else {
            format!(
                "No task {} found for '{}'.",
                req.id,
                fleet::sanitize(&req.name)
            )
        }))
    }

    #[tool(
        name = "drop_task",
        description = "Remove one of YOUR tasks from the list entirely by its id (from \
        read_tasks). Use when a reminder is obsolete; use complete_task instead to keep it as \
        done history."
    )]
    async fn drop_task(
        &self,
        Parameters(req): Parameters<TaskIdRequest>,
    ) -> Result<CallToolResult, McpError> {
        let ok = fleet::drop_task(&req.name, &req.id).await.map_err(mcp)?;
        Ok(text(if ok {
            format!("Dropped task {}.", req.id)
        } else {
            format!(
                "No task {} found for '{}'.",
                req.id,
                fleet::sanitize(&req.name)
            )
        }))
    }

    #[tool(
        name = "create_shared_task",
        description = "Create a SHARED task — a team- or fleet-scoped goal that groups board issues \
        + PRs across agents, with participants and a Start→Goal timeline. Use this for coordinated \
        work spanning multiple agents/PRs (a release, a cross-repo feature), NOT a private reminder \
        (use add_task for those). You become the owner + first participant. Returns the new id; link \
        issues/PRs with link_shared_task and move it with advance_shared_task."
    )]
    async fn create_shared_task(
        &self,
        Parameters(req): Parameters<CreateSharedTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let st = fleet::create_shared_task(
            &req.owner,
            &req.title,
            req.goal.clone(),
            req.scope,
            req.team.clone(),
        )
        .await
        .map_err(mcp)?;
        Ok(text(format!(
            "Created shared task {id} ({scope}): {title}",
            id = st.id,
            scope = st.scope.as_str(),
            title = st.title
        )))
    }

    #[tool(
        name = "link_shared_task",
        description = "Link a board issue or PR to a shared task (idempotent). Pins the link to \
        {repo, number} so the cockpit/web shows the related work and the reconcile sweep can track \
        it. `kind` is 'issue' or 'pr'."
    )]
    async fn link_shared_task(
        &self,
        Parameters(req): Parameters<LinkSharedTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let st = fleet::link_shared_task(&req.id, req.kind, &req.repo, req.number)
            .await
            .map_err(mcp)?;
        Ok(text(format!(
            "Linked {kind} {repo}#{number} to shared task {id} ({ni} issues, {npr} PRs).",
            kind = req.kind.as_str(),
            repo = req.repo,
            number = req.number,
            id = st.id,
            ni = st.issues.len(),
            npr = st.prs.len(),
        )))
    }

    #[tool(
        name = "join_shared_task",
        description = "Add yourself to a shared task's participants (idempotent) — do this when you \
        start contributing to a shared goal, so the cockpit/web shows you on it."
    )]
    async fn join_shared_task(
        &self,
        Parameters(req): Parameters<JoinSharedTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let st = fleet::join_shared_task(&req.id, &req.name)
            .await
            .map_err(mcp)?;
        Ok(text(format!(
            "{name} joined shared task {id} ({n} participants).",
            name = fleet::sanitize(&req.name),
            id = st.id,
            n = st.participants.len(),
        )))
    }

    #[tool(
        name = "advance_shared_task",
        description = "Move a shared task to a new state (open / active / blocked / in-review / \
        done), appending a timeline entry stamped with who moved it. Idempotent (no-op if already \
        in that state). Use it to reflect real progress on the shared goal."
    )]
    async fn advance_shared_task(
        &self,
        Parameters(req): Parameters<AdvanceSharedTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let st = fleet::advance_shared_task(&req.id, req.state, &req.by, req.note.as_deref())
            .await
            .map_err(mcp)?;
        Ok(text(format!(
            "Shared task {id} → {state}.",
            id = st.id,
            state = st.state.as_str(),
        )))
    }

    #[tool(
        name = "list_shared_tasks",
        description = "List SHARED tasks (team/fleet goals). Pass your `name` to see only the ones \
        you're a participant in; omit it to list all. Each row shows the id, state, scope, title, \
        and participant / issue / PR counts. Use the id with link/join/advance_shared_task."
    )]
    async fn list_shared_tasks(
        &self,
        Parameters(req): Parameters<ListSharedTasksRequest>,
    ) -> Result<CallToolResult, McpError> {
        let include_done = req.include_done.unwrap_or(false);
        let me = req.name.as_ref().map(|n| fleet::sanitize(n));
        let all = fleet::list_shared_tasks().await;
        let shown: Vec<&fleet::SharedTask> = all
            .iter()
            .filter(|t| include_done || t.state != fleet::SharedTaskState::Done)
            .filter(|t| match &me {
                Some(n) => t.participants.iter().any(|p| p == n),
                None => true,
            })
            .collect();
        if shown.is_empty() {
            return Ok(text("No shared tasks.".to_string()));
        }
        let mut out = String::new();
        for t in &shown {
            out.push_str(&format!(
                "{id}  [{state}] {scope}  {title}  ({np}p {ni}i {npr}pr)\n",
                id = t.id,
                state = t.state.as_str(),
                scope = t.scope.as_str(),
                title = t.title,
                np = t.participants.len(),
                ni = t.issues.len(),
                npr = t.prs.len(),
            ));
        }
        Ok(text(out))
    }

    #[tool(
        name = "sweep_shared_tasks",
        description = "Reconcile shared tasks against GitHub. For each shared task: \
        discover the PRs that close its linked issues (auto-linking them and crediting \
        each closing PR's author as a participant), then advance the task's state from \
        the real PR states — an open PR => active, a merged one => in-review. Never \
        auto-completes (done stays a human call) and never regresses or overrides a \
        manual blocked/done. Idempotent; safe to run on a schedule. Pass `id` to \
        reconcile one task, or omit to sweep all."
    )]
    async fn sweep_shared_tasks(
        &self,
        Parameters(req): Parameters<SweepSharedTasksRequest>,
    ) -> Result<CallToolResult, McpError> {
        let want = req.id.as_deref().map(fleet::sanitize);
        let tasks = fleet::list_shared_tasks().await;
        let mut out = String::new();
        let mut advanced = 0usize;
        let mut notified = 0usize;
        for t in &tasks {
            if want.as_ref().is_some_and(|w| &t.id != w) {
                continue;
            }
            // Links we already have; discover more from each linked issue.
            let mut linked: std::collections::HashSet<(String, u64)> =
                t.prs.iter().map(|r| (r.repo.clone(), r.number)).collect();
            // PRs allowed to drive the state advance: pre-existing/manual links +
            // closers (the authoritative "does the work" set). Auto-discovered
            // reference-only PRs LINK (for visibility) but must not move task
            // state — a referencer is any PR that *mentions* the issue, not one
            // that does its work, so a tangential merged mention shouldn't advance
            // the goal.
            let mut advance_prs: std::collections::HashSet<(String, u64)> =
                t.prs.iter().map(|r| (r.repo.clone(), r.number)).collect();
            let mut joined: std::collections::HashSet<String> =
                t.participants.iter().cloned().collect();
            let mut new_links = 0usize;
            let mut new_parts = 0usize;
            for issue in &t.issues {
                if issue.repo.split_once('/').is_none() {
                    continue; // malformed ref — skip so one bad ref can't abort the pass
                }
                // Closers (home-repo, Fixes #N) are authoritative — they drive
                // state. Cross-repo PRs that only *reference* the goal-issue are
                // discovered too (the close query misses out-of-repo contributors,
                // since GitHub can't auto-close across repos) but link for
                // visibility only — they're kept out of advance_prs above.
                let closers = issue_closing_prs(&issue.repo, issue.number).await;
                let referencers = issue_referencing_prs(&issue.repo, issue.number).await;
                advance_prs.extend(closers.iter().map(|(r, n, _)| (r.clone(), *n)));
                for (pr_repo, pr, author) in closers.into_iter().chain(referencers) {
                    let key = (pr_repo.clone(), pr);
                    if !linked.contains(&key)
                        && fleet::link_shared_task(&t.id, fleet::RefKind::Pr, &pr_repo, pr)
                            .await
                            .is_ok()
                    {
                        linked.insert(key);
                        new_links += 1;
                    }
                    if !author.is_empty()
                        && !joined.contains(&author)
                        && fleet::join_shared_task(&t.id, &author).await.is_ok()
                    {
                        joined.insert(author.clone());
                        new_parts += 1;
                    }
                }
            }
            // GitHub state of every linked ref — cached on the ref for the
            // snapshot's status pills, and (for PRs) the forward-only advance
            // signal. PRs: open|draft|merged|closed; issues: open|closed.
            let mut states: Vec<(String, u64, String)> = Vec::new();
            for issue in &t.issues {
                if let Some(s) = ref_state(fleet::RefKind::Issue, &issue.repo, issue.number).await {
                    states.push((issue.repo.clone(), issue.number, s));
                }
            }
            let mut open_pr: Option<(String, u64)> = None;
            let mut merged_pr: Option<(String, u64)> = None;
            // PRs that crossed into "merged" *this* pass — the per-PR merge edge
            // (prior cached state wasn't already "merged"). We notify on these.
            let mut newly_merged: Vec<(String, u64)> = Vec::new();
            for (repo, pr) in &linked {
                if let Some(s) = ref_state(fleet::RefKind::Pr, repo, *pr).await {
                    // Cache the pill for every linked PR (visibility), but only an
                    // advance-eligible PR (closer / manual link) may move state or
                    // fire a merge-notification — a reference-only mention must not.
                    if advance_prs.contains(&(repo.clone(), *pr)) {
                        let prior = t
                            .prs
                            .iter()
                            .find(|r| &r.repo == repo && r.number == *pr)
                            .and_then(|r| r.state.as_deref());
                        if fleet::is_merge_edge(prior, &s) {
                            newly_merged.push((repo.clone(), *pr));
                        }
                        match s.as_str() {
                            "open" | "draft" if open_pr.is_none() => {
                                open_pr = Some((repo.clone(), *pr))
                            }
                            "merged" if merged_pr.is_none() => {
                                merged_pr = Some((repo.clone(), *pr))
                            }
                            _ => {}
                        }
                    }
                    states.push((repo.clone(), *pr, s));
                }
            }
            fleet::cache_ref_states(&t.id, &states).await.ok();
            let note = match (&merged_pr, &open_pr) {
                (Some((r, n)), _) => format!("merged {r}#{n}"),
                (None, Some((r, n))) => format!("PR {r}#{n} open"),
                _ => String::new(),
            };
            if let Ok(Some(next)) = fleet::reconcile_advance(
                &t.id,
                open_pr.is_some(),
                merged_pr.is_some(),
                Some(note.as_str()),
            )
            .await
            {
                out.push_str(&format!("  {}: -> {}  ({note})\n", t.id, next.as_str()));
                advanced += 1;
            }
            // Notify the agent(s) registered for each just-merged PR's repo
            // (repo-owner routing; fall back to the task owner if none). The
            // cached "merged" state written above guards against re-notifying.
            for (repo, pr) in &newly_merged {
                let mut targets = fleet::agents_for_repo(repo).await;
                if targets.is_empty() {
                    targets.push(t.owner.clone());
                }
                let title = format!("PR merged: {repo}#{pr}");
                let body = format!(
                    "{repo}#{pr} is merged — it's linked to shared task {} \"{}\". \
                     You're getting this as the agent registered for {repo}.",
                    t.id, t.title
                );
                for to in &targets {
                    if fleet::send_message("reconcile", to, &title, &body)
                        .await
                        .is_ok()
                    {
                        out.push_str(&format!("  {}: notified {to} ({repo}#{pr} merged)\n", t.id));
                        notified += 1;
                    }
                }
            }
            if new_links > 0 || new_parts > 0 {
                out.push_str(&format!(
                    "  {}: +{new_links} PR(s), +{new_parts} participant(s)\n",
                    t.id
                ));
            }
        }
        if out.is_empty() {
            out.push_str("(no shared-task changes)\n");
        }
        out.push_str(&format!(
            "({advanced} state(s) advanced, {notified} merge-notification(s) sent)\n"
        ));
        Ok(text(out))
    }
}

#[tool_handler]
impl ServerHandler for AgentBoardService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "lakitu-mcp".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
            instructions: Some(
                "MCP primitives for the agent-driven issue→PR loop. \
                Use `emit_event` for any agent-actions.log row; `move_card`, `set_blocker`, \
                and `clear_blocker` for board mutations. The board is auto-discovered \
                per repo (the Project v2 linked to it), falling back to acme/#14. \
                Multi-agent coordination (rendered by the lakitu TUI): \
                `register_agent` once at startup, `heartbeat` at step boundaries, \
                `list_agents` to see peers, `send_message` / `read_inbox` to talk to them. \
                Persona: `set_identity` to persist who you are and `remember_peer` to record what \
                you learn about teammates — both auto-load into your context next session. \
                Tasks: `add_task` / `read_tasks` keep a private reminder list (so a message that \
                lands mid-work isn't forgotten) — shown in the cockpit and re-loaded at SessionStart."
                    .into(),
            ),
        }
    }
}

// ---- Tool request types ----------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EmitEventRequest {
    #[schemars(
        description = "Skill emitting the event, e.g. 'board-issue-loop' or 'pr-review-fixup'."
    )]
    pub skill: String,
    #[schemars(
        description = "Action verb, e.g. 'blocked', 'pr-opened', 'sweep'. See each skill's audit-log table for the vocabulary."
    )]
    pub action: String,
    #[schemars(description = "Repo name without owner — e.g. 'web'. Defaults to 'web'.")]
    #[serde(default)]
    pub repo: Option<String>,
    #[schemars(
        description = "Free-form key/value pairs. Renders as space-separated 'k=v' in the log; keys 'issue', 'reason', 'detail' sort first for readability."
    )]
    #[serde(default)]
    pub details: HashMap<String, String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MoveCardRequest {
    #[schemars(description = "Issue number (the bare integer, e.g. 90).")]
    pub issue: u64,
    #[schemars(
        description = "Status name on the project board. Must be one of: Todo, In Progress, Done."
    )]
    pub status: String,
    #[schemars(
        description = "Optional repo the issue lives in. Bare name or 'owner/repo'. Selects the board (the Project v2 linked to that repo; falls back to acme/#14). Defaults to 'acme/web'."
    )]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetBlockerRequest {
    #[schemars(description = "Issue number.")]
    pub issue: u64,
    #[schemars(
        description = "Reason for the block. See the board-issue-loop skill for what each one means."
    )]
    pub reason: BlockerReason,
    #[schemars(description = "Short free-form detail, e.g. 'svg-from-jon' or 'v1.0-cut-pending'.")]
    pub detail: String,
    #[schemars(
        description = "GitHub username to @-mention in the issue comment (only used for asset-needed and external-input). Omit to skip the mention."
    )]
    #[serde(default)]
    pub unblocker_handle: Option<String>,
    #[schemars(description = "Optional repo. Bare name or 'owner/repo'. Defaults to 'acme/web'.")]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClearBlockerRequest {
    #[schemars(description = "Issue number.")]
    pub issue: u64,
    #[schemars(description = "Optional repo. Bare name or 'owner/repo'. Defaults to 'acme/web'.")]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SweepAgentPrsRequest {
    // v0.1 took no parameters. `repo` added in v0.2 so a caller can
    // sweep PRs in api / other sibling repos. Default keeps the
    // skill-documented behaviour (web).
    #[schemars(
        description = "Optional repo to sweep. Accepts a bare name (e.g. 'api', assumed under 'acme') or full 'owner/repo'. Defaults to 'acme/web'."
    )]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SweepSharedTasksRequest {
    #[schemars(
        description = "Optional shared-task id to reconcile just that one. Omit to reconcile every shared task."
    )]
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommentThreadsRequest {
    #[schemars(description = "PR number (the bare integer, e.g. 103).")]
    pub pr: u64,
    #[schemars(description = "Optional repo. Bare name or 'owner/repo'. Defaults to 'acme/web'.")]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CommentBodyRequest {
    #[schemars(
        description = "Comment id as returned by `comment_threads`. Inline and top-level comments use different ID namespaces; this tool tries both."
    )]
    pub comment_id: u64,
    #[schemars(description = "Optional repo. Bare name or 'owner/repo'. Defaults to 'acme/web'.")]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct MarkReadyRequest {
    #[schemars(description = "PR number to flip from draft to ready-for-review.")]
    pub pr: u64,
    #[schemars(
        description = "MUST be true: this tool refuses unless the supervisor has explicitly delegated the ready-flip. Encodes the skill's `agent never flips on its own initiative` rule at the call site."
    )]
    pub by_supervisor: bool,
    #[schemars(description = "Optional repo. Bare name or 'owner/repo'. Defaults to 'acme/web'.")]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FileFollowupIssueRequest {
    #[schemars(
        description = "PR the follow-up was surfaced from (for audit log + traceability). Bare number."
    )]
    pub parent_pr: u64,
    #[schemars(description = "Issue title — short imperative summary of the observation.")]
    pub title: String,
    #[schemars(
        description = "Issue body. Convention is to lead with `Surfaced as a follow-up from #<PR>.` so the connection is visible on the issue page itself."
    )]
    pub body: String,
    #[schemars(
        description = "Optional repo to file the follow-up issue in. Bare name or 'owner/repo'. Defaults to 'acme/web'. The follow-up lands on that repo's board (its linked Project v2; falls back to acme/#14)."
    )]
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RegisterAgentRequest {
    #[schemars(
        description = "Stable agent handle, kebab-case (e.g. 'vscode-bot'). Becomes the file/inbox name; non-path-safe characters are replaced with '-'."
    )]
    pub name: String,
    #[schemars(description = "Repo this agent works, free-form label (e.g. 'acme/web').")]
    pub repo: String,
    #[schemars(
        description = "Git board this agent is connected to. Convention: '<owner>/<projectNumber>' (e.g. 'acme/14')."
    )]
    pub board: String,
    #[schemars(
        description = "Optional short function label, 1-3 words (e.g. 'code review', 'scan backend', 'VS Code UI', 'supervisor'). Distinct from the name: the name is identity/address, the role is what you do. Shown as a chip in the cockpit and in list_agents so peers can route by capability."
    )]
    #[serde(default)]
    pub role: Option<String>,
    #[schemars(
        description = "Optional one-line capability blurb: what this agent is for and what peers can ask it to do (e.g. 'api backend — ask me to expose MCP tools or adjust scan schemas'). This is what other agents read in list_agents to decide who to message. Stable identity, distinct from the transient heartbeat task."
    )]
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RenameAgentRequest {
    #[schemars(description = "Current agent name.")]
    pub old_name: String,
    #[schemars(description = "New agent name (kebab-case). Refused if already taken.")]
    pub new_name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DeregisterAgentRequest {
    #[schemars(description = "Agent name to remove entirely (registry + heartbeat + inbox).")]
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HeartbeatRequest {
    #[schemars(description = "The agent's own name (as passed to register_agent).")]
    pub name: String,
    #[schemars(description = "Current state.")]
    pub state: AgentStateArg,
    #[schemars(
        description = "Optional one-line description of the current task, e.g. 'issue #90: fix watcher leak'."
    )]
    #[serde(default)]
    pub task: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMessageRequest {
    #[schemars(description = "Sender agent name (your own name).")]
    pub from: String,
    #[schemars(description = "Recipient agent name (see list_agents).")]
    pub to: String,
    #[schemars(description = "Short subject line.")]
    pub title: String,
    #[schemars(
        description = "Full message body. Can be a feature request, question, or coordination note."
    )]
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadInboxRequest {
    #[schemars(description = "The agent's own name whose inbox to read.")]
    pub name: String,
    #[schemars(
        description = "If true (default), returned messages are archived to read/ so they aren't seen again. Pass false to peek without consuming."
    )]
    #[serde(default)]
    pub mark_read: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitForMessageRequest {
    #[schemars(description = "The agent's own name whose inbox to wait on.")]
    pub name: String,
    #[schemars(
        description = "Max seconds to block before returning status=timeout. Default 300; capped at 3600. Your MCP client's per-request timeout must exceed this."
    )]
    #[serde(default)]
    pub timeout_sec: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BroadcastRequest {
    #[schemars(description = "Sender agent name (your own name).")]
    pub from: String,
    #[schemars(description = "Short subject line.")]
    pub title: String,
    #[schemars(description = "Full message body — sent to every other client.")]
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotifySupervisorRequest {
    #[schemars(description = "Sender agent name (your own name).")]
    pub from: String,
    #[schemars(description = "One-line summary of the recap.")]
    pub title: String,
    #[schemars(description = "The recap body: what changed + why + current state + any ask.")]
    pub body: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListAgentsRequest {}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetIdentityRequest {
    #[schemars(
        description = "Your own agent name (as registered). The persona is stored under this handle."
    )]
    pub name: String,
    #[schemars(description = "One-line essence of who you are. Omit to keep the current tagline.")]
    #[serde(default)]
    pub tagline: Option<String>,
    #[schemars(
        description = "Freeform markdown: how you work, your voice, what you care about — anything that makes you you. Omit to keep the current bio."
    )]
    #[serde(default)]
    pub bio: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetIdentityRequest {
    #[schemars(
        description = "The agent whose self-card you want to read (a peer's name from list_agents)."
    )]
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RememberPeerRequest {
    #[schemars(description = "Your own agent name — the note is stored under your persona.")]
    pub name: String,
    #[schemars(description = "The peer this note is about.")]
    pub peer: String,
    #[schemars(description = "What you learned/observed about them — one observation.")]
    pub note: String,
    #[schemars(description = "Optional rapport score from -5 (friction) to +5 (great rapport).")]
    #[serde(default)]
    pub affinity: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecallPeersRequest {
    #[schemars(description = "Your own agent name whose peer-notes to read back.")]
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddTaskRequest {
    #[schemars(description = "Your own agent name — the task is added to your list.")]
    pub name: String,
    #[schemars(
        description = "The reminder title — short and actionable (e.g. 'reply to samus about the schema', 'update the changelog before merging')."
    )]
    pub text: String,
    #[schemars(
        description = "Optional longer note/details for the task (the 'message' — shown in the cockpit's task detail). Use it when the title alone isn't enough."
    )]
    #[serde(default)]
    pub body: Option<String>,
    #[schemars(
        description = "Optional repo ('owner/name') of a PR this task hangs off. Together with pr_number, the task renders as a subtree of that PR in the cockpit."
    )]
    #[serde(default)]
    pub pr_repo: Option<String>,
    #[schemars(description = "Optional PR number this task hangs off (pair with pr_repo).")]
    #[serde(default)]
    pub pr_number: Option<u64>,
    #[schemars(
        description = "Optional id of the inbox message this task was spun off from (provenance) — e.g. a message you couldn't action yet but don't want to lose."
    )]
    #[serde(default)]
    pub from_msg: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadTasksRequest {
    #[schemars(description = "Your own agent name whose task list to read.")]
    pub name: String,
    #[schemars(
        description = "If true, include completed tasks too. Default false (open tasks only)."
    )]
    #[serde(default)]
    pub include_done: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskIdRequest {
    #[schemars(description = "Your own agent name (whose list this task id belongs to).")]
    pub name: String,
    #[schemars(description = "The task id — the 6-char id shown by read_tasks.")]
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateSharedTaskRequest {
    #[schemars(
        description = "Your own agent name — you become the shared task's owner and first participant."
    )]
    pub owner: String,
    #[schemars(
        description = "One-line title of the shared goal (e.g. 'Release 0.3.1', 'Ship the web UI')."
    )]
    pub title: String,
    #[schemars(description = "Optional longer statement of the goal / definition of done.")]
    #[serde(default)]
    pub goal: Option<String>,
    #[schemars(description = "Who shares it: 'team' (one board) or 'fleet' (everyone).")]
    pub scope: fleet::TaskScope,
    #[schemars(
        description = "For scope=team: the board it belongs to, 'owner/projectNumber' (e.g. 'fossid-ab/14'). Required for team scope; ignored for fleet."
    )]
    #[serde(default)]
    pub team: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinkSharedTaskRequest {
    #[schemars(description = "The shared task id (from create_shared_task / list_shared_tasks).")]
    pub id: String,
    #[schemars(description = "What you're linking: 'issue' or 'pr'.")]
    pub kind: fleet::RefKind,
    #[schemars(description = "The repo of the issue/PR, 'owner/name' (e.g. 'dac2k9/lakitu-oss').")]
    pub repo: String,
    #[schemars(description = "The issue or PR number.")]
    pub number: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JoinSharedTaskRequest {
    #[schemars(description = "Your own agent name — added to the shared task's participants.")]
    pub name: String,
    #[schemars(description = "The shared task id to join.")]
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AdvanceSharedTaskRequest {
    #[schemars(description = "Your own agent name — recorded in the timeline as who moved it.")]
    pub by: String,
    #[schemars(description = "The shared task id to advance.")]
    pub id: String,
    #[schemars(description = "The new state: open, active, blocked, in-review, or done.")]
    pub state: fleet::SharedTaskState,
    #[schemars(
        description = "Optional short note for the timeline — why the move happened (e.g. 'CI green, ready for review')."
    )]
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListSharedTasksRequest {
    #[schemars(
        description = "Optional: your agent name, to show only shared tasks you're a participant in. Omit to list all."
    )]
    #[serde(default)]
    pub name: Option<String>,
    #[schemars(description = "If true, include done tasks too. Default false (hides completed).")]
    #[serde(default)]
    pub include_done: Option<bool>,
}

/// Declared agent state. Lowercase on the wire to match the heartbeat
/// file's `state` field that the TUI reads.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentStateArg {
    Idle,
    Working,
    /// Blocked on the supervisor — a decision/answer only they can give. Drives
    /// the cockpit's "needs you" alert. Pair with notify_supervisor.
    Blocked,
    /// Waiting on a peer or external event (another client's release, CI, …) —
    /// stuck, but not the supervisor's call. Shows as a calm ◐, not the alert.
    Waiting,
}

impl AgentStateArg {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Waiting => "waiting",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BlockerReason {
    ReleaseGate,
    ProductDecision,
    Editorial,
    AssetNeeded,
    RepoAdmin,
    Legal,
    ExternalInput,
}

impl BlockerReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::ReleaseGate => "release-gate",
            Self::ProductDecision => "product-decision",
            Self::Editorial => "editorial",
            Self::AssetNeeded => "asset-needed",
            Self::RepoAdmin => "repo-admin",
            Self::Legal => "legal",
            Self::ExternalInput => "external-input",
        }
    }
}

// ---- Helpers ---------------------------------------------------------------

fn mcp(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

fn text(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![Content::text(s.into())])
}

async fn run_gh(args: &[&str]) -> Result<String, McpError> {
    let output = tokio::process::Command::new("gh")
        .args(args)
        .output()
        .await
        .map_err(|e| mcp(format!("gh spawn failed: {e}")))?;
    if !output.status.success() {
        return Err(mcp(format!(
            "gh exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

async fn gh_issue_state(issue: u64, repo: &str) -> Result<String, McpError> {
    let json = run_gh(&[
        "issue",
        "view",
        &issue.to_string(),
        "--repo",
        repo,
        "--json",
        "state",
    ])
    .await?;
    let v: serde_json::Value = serde_json::from_str(&json).map_err(mcp)?;
    Ok(v["state"].as_str().unwrap_or("UNKNOWN").to_string())
}

async fn has_blocker_comment(issue: u64, repo: &str) -> Result<bool, McpError> {
    let json = run_gh(&[
        "issue",
        "view",
        &issue.to_string(),
        "--repo",
        repo,
        "--json",
        "comments",
    ])
    .await?;
    let v: serde_json::Value = serde_json::from_str(&json).map_err(mcp)?;
    let empty = Vec::new();
    for c in v["comments"].as_array().unwrap_or(&empty) {
        if c["body"]
            .as_str()
            .unwrap_or("")
            .contains("<!-- agent-blocker-filed -->")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn find_item_id(issue: u64, coords: &BoardCoords) -> Result<String, McpError> {
    let json = run_gh(&[
        "project",
        "item-list",
        &coords.number.to_string(),
        "--owner",
        &coords.owner,
        "--format",
        "json",
        "--limit",
        "200",
    ])
    .await?;
    let v: serde_json::Value = serde_json::from_str(&json).map_err(mcp)?;
    let empty = Vec::new();
    for item in v["items"].as_array().unwrap_or(&empty) {
        if item["content"]["number"].as_u64() == Some(issue) {
            return Ok(item["id"].as_str().unwrap_or("").to_string());
        }
    }
    Err(mcp(format!(
        "issue #{} not found on project {}/#{} (page 1/200)",
        issue, coords.owner, coords.number
    )))
}

async fn recent_block(issue: u64, reason: &str) -> Result<bool, std::io::Error> {
    let path = audit_log_path();
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Ok(false);
    }
    let content = tokio::fs::read_to_string(&path).await?;
    let cutoff = chrono::Local::now() - chrono::Duration::hours(24);
    let needle = format!("\tblocked\tissue=#{} reason={}", issue, reason);
    for line in content.lines().rev() {
        if !line.contains(&needle) {
            continue;
        }
        let Some(ts_str) = line.split('\t').next() else {
            continue;
        };
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(ts_str) {
            if ts.with_timezone(&chrono::Local) > cutoff {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn format_details(map: &HashMap<String, String>) -> String {
    let mut pairs: Vec<(&String, &String)> = map.iter().collect();
    // Conventional ordering: issue → reason → detail → rest alphabetical.
    // Matches the existing bash log_event call sites and keeps grep-friendly.
    pairs.sort_by(|a, b| {
        let rank = |k: &str| match k {
            "issue" => 0,
            "reason" => 1,
            "detail" => 2,
            _ => 3,
        };
        rank(a.0).cmp(&rank(b.0)).then_with(|| a.0.cmp(b.0))
    });
    pairs
        .iter()
        .map(|(k, v)| {
            let sanitized = v.replace(['\n', '\r', '\t'], " ");
            format!("{k}={sanitized}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn append_audit_log(
    skill: &str,
    action: &str,
    repo: &str,
    details: &str,
) -> Result<(), std::io::Error> {
    use tokio::io::AsyncWriteExt;
    let ts = chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string();
    let line = format!("{ts}\t{skill}\t{repo}\t{action}\t{details}\n");
    let path = audit_log_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    f.write_all(line.as_bytes()).await?;
    Ok(())
}

fn audit_log_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home)
        .join(".claude")
        .join("logs")
        .join("agent-actions.log")
}

// ---- Sweep helpers ---------------------------------------------------------

/// One enriched open-PR row: number, branch, head sha, draft flag, and the
/// unanswered-comment / failing-check / behind-main counts.
type SweepRow = (u64, String, String, bool, usize, usize, u64);

/// Sweep one repo's open, agent-authored PRs, enriching each with
/// comment / failing-check / behind-main counts (the three counts run in
/// parallel per PR — serial gh calls would be ~15s; this keeps it ~3s).
async fn sweep_one_repo(repo_slug: &str) -> Result<Vec<SweepRow>, McpError> {
    // List open PRs with the fields we need. `body` is the second half of
    // the agent-authorship gate (footer check); listing it inline avoids an
    // extra `gh pr view` per candidate.
    let json = run_gh(&[
        "pr",
        "list",
        "--repo",
        repo_slug,
        "--state",
        "open",
        "--limit",
        "50",
        "--json",
        "number,headRefName,headRefOid,isDraft,body",
    ])
    .await?;
    let prs: serde_json::Value = serde_json::from_str(&json).map_err(mcp)?;
    let empty = Vec::new();
    let pr_list = prs.as_array().unwrap_or(&empty);

    let mut tasks = Vec::new();
    for pr_val in pr_list {
        let Some(branch) = pr_val["headRefName"].as_str() else {
            continue;
        };
        // Two-signal authorship gate: branch convention + body footer.
        // Either alone is insufficient — humans use `fix/` branches too,
        // and the footer alone would let through PRs the agent commented
        // on but didn't open.
        if !AGENT_BRANCH_PREFIXES.iter().any(|p| branch.starts_with(p)) {
            continue;
        }
        let body = pr_val["body"].as_str().unwrap_or("");
        if !body.contains(AGENT_FOOTER) {
            continue;
        }
        let number = pr_val["number"].as_u64().unwrap_or(0);
        let sha = pr_val["headRefOid"].as_str().unwrap_or("").to_string();
        let is_draft = pr_val["isDraft"].as_bool().unwrap_or(false);
        let branch = branch.to_string();
        let task_repo = repo_slug.to_string();
        tasks.push(tokio::spawn(async move {
            let (unanswered, failing, behind) = tokio::join!(
                count_comments(number, &task_repo),
                count_failing_checks(&sha, &task_repo),
                count_behind_main(&branch, &task_repo),
            );
            (
                number,
                branch,
                sha,
                is_draft,
                unanswered.unwrap_or(0),
                failing.unwrap_or(0),
                behind.unwrap_or(0),
            )
        }));
    }

    let mut rows: Vec<SweepRow> = Vec::new();
    for t in tasks {
        if let Ok(row) = t.await {
            rows.push(row);
        }
    }
    rows.sort_by_key(|r| r.0);
    Ok(rows)
}

/// PRs seen in the event log, keyed by `(repo slug, number)`. `all` is every PR
/// ever logged (so sweep back-fill stays idempotent); `active` is the still-open
/// cards — those with a `pr-opened` but no terminal (`pr-merged`/`card-done`)
/// event yet — which reconciliation re-checks against the live open set.
/// Best-effort: a missing/unreadable log yields empty sets. Repo columns are
/// normalized to full slugs so short-name rows (`api`) and full-slug rows
/// (`acme/api`) compare equal.
#[derive(Default)]
struct LoggedPrs {
    all: std::collections::HashSet<(String, u64)>,
    active: std::collections::HashSet<(String, u64)>,
}

async fn read_logged_prs() -> LoggedPrs {
    let mut all = std::collections::HashSet::new();
    let mut terminated = std::collections::HashSet::new();
    let Ok(content) = tokio::fs::read_to_string(audit_log_path()).await else {
        return LoggedPrs::default();
    };
    for line in content.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 5 {
            continue;
        }
        if let Some(n) = parse_pr_ref(cols[4]) {
            let key = (normalize_repo_slug(Some(cols[2])), n);
            // cols[3] is the action column.
            if matches!(cols[3], "pr-merged" | "card-done") {
                terminated.insert(key.clone());
            }
            all.insert(key);
        }
    }
    let active = all.difference(&terminated).cloned().collect();
    LoggedPrs { all, active }
}

/// High-level PR state, for reconciling a stale card against GitHub.
enum PrState {
    Open,
    Closed,
    Merged,
}

/// The GitHub state of a linked ref, as a short string for the snapshot's status
/// pills. PRs: "open" | "draft" | "merged" | "closed"; issues: "open" | "closed".
/// Returns `None` on any gh/JSON error, so a bad ref never aborts the sweep.
async fn ref_state(kind: fleet::RefKind, repo_slug: &str, number: u64) -> Option<String> {
    let n = number.to_string();
    let out = match kind {
        fleet::RefKind::Pr => {
            let json = run_gh(&[
                "pr",
                "view",
                &n,
                "--repo",
                repo_slug,
                "--json",
                "state,isDraft",
            ])
            .await
            .ok()?;
            let v: serde_json::Value = serde_json::from_str(&json).ok()?;
            match v["state"].as_str()? {
                "MERGED" => "merged",
                "CLOSED" => "closed",
                _ if v["isDraft"].as_bool().unwrap_or(false) => "draft",
                _ => "open",
            }
        }
        fleet::RefKind::Issue => {
            let json = run_gh(&["issue", "view", &n, "--repo", repo_slug, "--json", "state"])
                .await
                .ok()?;
            let v: serde_json::Value = serde_json::from_str(&json).ok()?;
            match v["state"].as_str()? {
                "CLOSED" => "closed",
                _ => "open",
            }
        }
    };
    Some(out.to_string())
}

/// Re-query a single PR's state — used when a logged card is no longer in the
/// live open set, to decide whether it merged, closed, or is just no longer
/// agent-authored (still open).
async fn pr_state(number: u64, repo_slug: &str) -> Result<PrState, McpError> {
    let json = run_gh(&[
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        repo_slug,
        "--json",
        "state",
    ])
    .await?;
    let v: serde_json::Value = serde_json::from_str(&json).map_err(mcp)?;
    Ok(match v["state"].as_str().unwrap_or("") {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ => PrState::Open,
    })
}

/// The PRs GitHub considers will close `issue` in `repo_slug`, via the issue's
/// `closedByPullRequestsReferences` — the authoritative `Fixes #N` linkage, not
/// body-parsing. Each entry is `(pr_number, author_login)`. A malformed slug, a
/// gh error, or unexpected JSON yields an empty list, so one bad ref can never
/// abort the whole reconcile pass.
async fn issue_closing_prs(repo_slug: &str, issue: u64) -> Vec<(String, u64, String)> {
    let Some((owner, name)) = repo_slug.split_once('/') else {
        return Vec::new();
    };
    if owner.is_empty() || name.is_empty() {
        return Vec::new();
    }
    const QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){issue(number:$number){closedByPullRequestsReferences(first:20,includeClosedPrs:true){nodes{number repository{nameWithOwner} author{login}}}}}}";
    let q = format!("query={QUERY}");
    let o = format!("owner={owner}");
    let nm = format!("name={name}");
    let num = format!("number={issue}");
    let json = match run_gh(&["api", "graphql", "-f", &q, "-f", &o, "-f", &nm, "-F", &num]).await {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else {
        return Vec::new();
    };
    let Some(arr) =
        v["data"]["repository"]["issue"]["closedByPullRequestsReferences"]["nodes"].as_array()
    else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|n| {
            let number = n["number"].as_u64()?;
            let repo = n["repository"]["nameWithOwner"].as_str()?.to_string();
            let author = n["author"]["login"].as_str().unwrap_or("").to_string();
            Some((repo, number, author))
        })
        .collect()
}

/// PRs that *reference* `issue` in `repo_slug` from any repo — read off the
/// issue's cross-reference timeline. Complements `issue_closing_prs`: a
/// cross-repo contributor can't GitHub-auto-close the goal-issue, so it never
/// shows up as a closer, but it does cross-reference it. Keeps only open/merged
/// PR sources (drops non-PR refs + abandoned, closed-unmerged PRs). Same
/// fail-soft contract: any error → empty list.
async fn issue_referencing_prs(repo_slug: &str, issue: u64) -> Vec<(String, u64, String)> {
    let Some((owner, name)) = repo_slug.split_once('/') else {
        return Vec::new();
    };
    if owner.is_empty() || name.is_empty() {
        return Vec::new();
    }
    const QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){issue(number:$number){timelineItems(itemTypes:[CROSS_REFERENCED_EVENT],first:100){nodes{... on CrossReferencedEvent{source{... on PullRequest{number state repository{nameWithOwner} author{login}}}}}}}}}";
    let q = format!("query={QUERY}");
    let o = format!("owner={owner}");
    let nm = format!("name={name}");
    let num = format!("number={issue}");
    let json = match run_gh(&["api", "graphql", "-f", &q, "-f", &o, "-f", &nm, "-F", &num]).await {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    parse_referencing_prs(&json)
}

/// Pure parser for `issue_referencing_prs`' GraphQL response: pull the
/// cross-referenced PR sources (open or merged) as `(repo, number, author)`.
/// Non-PR sources and closed-unmerged PRs are dropped. Split out so it is
/// unit-testable without a live `gh`.
fn parse_referencing_prs(json: &str) -> Vec<(String, u64, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(nodes) = v["data"]["repository"]["issue"]["timelineItems"]["nodes"].as_array() else {
        return Vec::new();
    };
    nodes
        .iter()
        .filter_map(|n| {
            let src = &n["source"];
            // PR sources populate the inline fragment; non-PR refs (issues) don't.
            let number = src["number"].as_u64()?;
            if !matches!(src["state"].as_str(), Some("OPEN") | Some("MERGED")) {
                return None; // skip closed-unmerged + anything unexpected
            }
            let repo = src["repository"]["nameWithOwner"].as_str()?.to_string();
            let author = src["author"]["login"].as_str().unwrap_or("").to_string();
            Some((repo, number, author))
        })
        .collect()
}

/// Extract the PR number from a `pr=#<n>` token in an event's details field.
fn parse_pr_ref(details: &str) -> Option<u64> {
    let idx = details.find("pr=#")?;
    let digits: String = details[idx + 4..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

/// Total review comments on a PR — inline review-comments + top-level
/// issue comments. "Unanswered" is a v0.1 simplification: count every
/// thread, don't try to detect whether the agent already replied. The
/// agent calls `comment_threads(pr)` to see which are actually open.
async fn count_comments(pr: u64, repo: &str) -> Result<usize, McpError> {
    let inline = run_gh(&[
        "api",
        "--paginate",
        &format!("repos/{}/pulls/{}/comments", repo, pr),
        "--jq",
        "length",
    ])
    .await?;
    let top = run_gh(&[
        "api",
        "--paginate",
        &format!("repos/{}/issues/{}/comments", repo, pr),
        "--jq",
        "length",
    ])
    .await?;
    let i: usize = sum_jq_lines(&inline);
    let t: usize = sum_jq_lines(&top);
    Ok(i + t)
}

/// `gh api --paginate ... --jq length` emits one integer per page. Sum
/// them to get the total across pages. Single-page responses are just
/// one integer; this still works.
fn sum_jq_lines(stdout: &str) -> usize {
    stdout
        .lines()
        .filter_map(|l| l.trim().parse::<usize>().ok())
        .sum()
}

async fn count_failing_checks(sha: &str, repo: &str) -> Result<usize, McpError> {
    let json = run_gh(&[
        "api",
        &format!("repos/{}/commits/{}/check-runs", repo, sha),
        "--jq",
        ".check_runs | map(select(.conclusion != \"success\" and .conclusion != null)) | length",
    ])
    .await?;
    Ok(json.trim().parse().unwrap_or(0))
}

/// Commits the agent branch is behind `main`. Uses GitHub's `compare`
/// REST endpoint so the MCP doesn't need a local git checkout.
async fn count_behind_main(branch: &str, repo: &str) -> Result<u64, McpError> {
    let json = run_gh(&[
        "api",
        &format!("repos/{}/compare/main...{}", repo, branch),
        "--jq",
        ".behind_by",
    ])
    .await?;
    Ok(json.trim().parse().unwrap_or(0))
}

/// One-line preview for review-comment bodies. Collapses whitespace,
/// strips embedded newlines (so a multi-line suggestion fence reads as
/// a single normalised string), truncates at `max_chars` with an
/// ellipsis. The full body is fetched separately via `comment_body`.
fn preview_body(body: &str, max_chars: usize) -> String {
    let mut compact: String = body.replace(['\n', '\r', '\t'], " ");
    while compact.contains("  ") {
        compact = compact.replace("  ", " ");
    }
    let trimmed = compact.trim();
    let count = trimmed.chars().count();
    if count > max_chars {
        let head: String = trimmed.chars().take(max_chars).collect();
        format!("{}…", head.trim_end())
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_details_orders_canonical_keys_first() {
        let mut m = HashMap::new();
        m.insert("zebra".to_string(), "1".to_string());
        m.insert("reason".to_string(), "asset-needed".to_string());
        m.insert("issue".to_string(), "#90".to_string());
        m.insert("detail".to_string(), "svg".to_string());
        m.insert("alpha".to_string(), "2".to_string());
        assert_eq!(
            format_details(&m),
            "issue=#90 reason=asset-needed detail=svg alpha=2 zebra=1"
        );
    }

    #[test]
    fn format_details_strips_control_chars() {
        let mut m = HashMap::new();
        m.insert("detail".to_string(), "line1\nline2\twith\ttabs".to_string());
        assert_eq!(format_details(&m), "detail=line1 line2 with tabs");
    }

    #[test]
    fn blocker_reason_kebab_case() {
        assert_eq!(BlockerReason::AssetNeeded.as_str(), "asset-needed");
        assert_eq!(BlockerReason::ProductDecision.as_str(), "product-decision");
        assert_eq!(BlockerReason::ExternalInput.as_str(), "external-input");
    }

    #[test]
    fn preview_body_collapses_and_truncates() {
        let body = "Hello\n\nworld\twith\nlots\nof\nwhitespace and a fairly long body that should be truncated at 40";
        let p = preview_body(body, 40);
        assert!(p.ends_with("…"), "expected ellipsis, got {p:?}");
        assert!(p.chars().count() <= 41, "preview too long: {p:?}");
        assert!(!p.contains("  "), "collapsed whitespace failed: {p:?}");
        assert!(!p.contains('\n'), "newlines stripped: {p:?}");
    }

    #[test]
    fn preview_body_short_bodies_pass_through() {
        assert_eq!(preview_body("LGTM", 80), "LGTM");
    }

    #[test]
    fn sum_jq_lines_handles_pagination() {
        // gh api --paginate emits one int per page; we sum.
        assert_eq!(sum_jq_lines("3\n5\n2\n"), 10);
        assert_eq!(sum_jq_lines("0\n"), 0);
        assert_eq!(sum_jq_lines(""), 0);
        assert_eq!(sum_jq_lines("4"), 4);
    }

    #[test]
    fn parse_pr_ref_extracts_number() {
        // Bare and with trailing tokens (real log details shapes).
        assert_eq!(parse_pr_ref("pr=#60"), Some(60));
        assert_eq!(
            parse_pr_ref("pr=#52 comment=3288468269 short=foo"),
            Some(52)
        );
        assert_eq!(parse_pr_ref("pr=#98 issue=#90"), Some(98));
        // No PR ref, or a non-PR ref → None (so dedup ignores the row).
        assert_eq!(parse_pr_ref("issue=#90 reason=release-gate"), None);
        assert_eq!(parse_pr_ref("note=lakitu-mcp wired in"), None);
        assert_eq!(parse_pr_ref(""), None);
    }

    #[test]
    fn parse_referencing_prs_keeps_open_merged_cross_repo() {
        let json = r#"{"data":{"repository":{"issue":{"timelineItems":{"nodes":[
            {"source":{"number":349,"state":"OPEN","repository":{"nameWithOwner":"fossid-ab/fossid-toolbox"},"author":{"login":"rush"}}},
            {"source":{"number":76,"state":"MERGED","repository":{"nameWithOwner":"fossid-ab/fossid-mcp"},"author":{"login":"link"}}},
            {"source":{"number":12,"state":"CLOSED","repository":{"nameWithOwner":"fossid-ab/x"},"author":{"login":"abandoned"}}},
            {"source":{}}
        ]}}}}}"#;
        assert_eq!(
            parse_referencing_prs(json),
            vec![
                (
                    "fossid-ab/fossid-toolbox".to_string(),
                    349,
                    "rush".to_string()
                ),
                ("fossid-ab/fossid-mcp".to_string(), 76, "link".to_string()),
            ],
            "open + merged cross-repo PR refs kept; closed-unmerged + non-PR source dropped"
        );
        // Fail-soft: bad / empty JSON → no refs (never aborts the sweep).
        assert!(parse_referencing_prs("not json").is_empty());
        assert!(parse_referencing_prs("{}").is_empty());
    }
}
