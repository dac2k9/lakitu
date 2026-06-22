//! Write side of the fleet multi-agent store (`~/.claude/lakitu-fleet/`).
//!
//! Mirror of the on-disk contract documented in
//! `~/src/lakitu/DESIGN.md`. The `lakitu` TUI is the reader;
//! these helpers are the writers the agent calls via the MCP tools
//! (`register_agent`, `heartbeat`, `send_message`, `read_inbox`,
//! `list_agents`).
//!
//! Layout:
//! ```text
//! ~/.claude/lakitu-fleet/
//!   agents/<name>.json            registry
//!   agents/<name>.heartbeat.json  presence
//!   inbox/<name>/<ts>-<id>.json   unread message
//!   inbox/<name>/read/<...>.json  read message
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::fs;

use crate::wire::ProjectDto;

/// Disambiguates messages created within the same second.
static MSG_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Disambiguates tasks created within the same instant.
static TASK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Disambiguates shared tasks created within the same instant.
static SHARED_TASK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A short, collision-resistant 6-hex id from the wall clock + a process-local
/// counter. Shared id scheme for messages and tasks (good enough — these are
/// per-agent, low-volume, and only need to be unique within one list).
fn short_id(counter: &AtomicU64) -> String {
    let nanos = chrono::Local::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    let c = counter.fetch_add(1, Ordering::Relaxed);
    format!(
        "{:06x}",
        ((nanos >> 8) ^ c.wrapping_mul(2_654_435_761)) & 0xff_ffff
    )
}

/// Serializes tests that mutate the process-global `HOME` (fleet + persona),
/// so the parallel test runner can't let them stomp each other's temp store.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn store_root() -> PathBuf {
    // An explicit root override wins, so the daemon (and any relocated/remote
    // store) can be pointed somewhere other than the default. This matches the
    // cockpit's `LAKITU_FLEET_STORE` and the shell hooks' `LAKITU_FLEET_ROOT`,
    // unifying a contract that had drifted (the MCP previously had no override
    // at all). Legacy `GENBOT_ROOT` is honored for back-compat.
    for var in ["LAKITU_FLEET_ROOT", "GENBOT_ROOT"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".claude").join("lakitu-fleet")
}

/// Make an agent name safe to use as a path component. Keeps
/// `[A-Za-z0-9._-]`, maps everything else to `-`, and strips leading /
/// trailing dots so `.` / `..` can't escape the store.
pub fn sanitize(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('.');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// Does `s` look like a GitHub `owner/name` slug — exactly two non-empty,
/// slash-free segments? Used to reject a typo'd link ref before it's stored and
/// then silently fails to resolve in the reconcile's gh queries.
fn is_repo_slug(s: &str) -> bool {
    let mut parts = s.split('/');
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(o), Some(n), None) if !o.is_empty() && !n.is_empty()
    )
}

/// One inbox message — same shape the TUI reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub time: String,
    pub from: String,
    pub title: String,
    pub body: String,
}

/// A PR a task hangs off — renders the task as a subtree of that PR's
/// work-item row in the cockpit. `owner/name` + number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPr {
    pub repo: String,
    pub number: u64,
}

/// One agent task: a private, lightweight reminder the agent (or supervisor)
/// jots so a mid-work interruption isn't forgotten. Deliberately *not* a GitHub
/// issue — issues are the durable, shared, reviewable unit of work; a task is
/// the agent's own scratchpad, surfaced in the cockpit and re-injected at
/// SessionStart so it survives compaction. Persisted to `tasks/<name>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    /// The task's one-line title.
    pub text: String,
    /// Optional longer note — the "message" of a task, so it reads like an
    /// inbox entry (a message converted to a task keeps its body here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub done: bool,
    pub created: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<TaskPr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_msg: Option<String>,
}

/// A reference to a board issue or PR that a [`SharedTask`] groups together:
/// `owner/repo` + number. (Distinct from [`TaskPr`], the single PR a private
/// per-agent [`Task`] hangs off in the cockpit.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRef {
    pub repo: String,
    pub number: u64,
}

/// Whether a [`SharedTask`] is owned by one team (board) or the whole fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskScope {
    Team,
    Fleet,
}

/// A [`SharedTask`]'s lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SharedTaskState {
    Open,
    Active,
    Blocked,
    InReview,
    Done,
}

/// Whether a `link_shared_task` target is an issue or a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Issue,
    Pr,
}

impl TaskScope {
    /// The lowercase wire/display string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Team => "team",
            Self::Fleet => "fleet",
        }
    }
}

impl SharedTaskState {
    /// The wire/display string (kebab-case, so `InReview` → `in-review`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Active => "active",
            Self::Blocked => "blocked",
            Self::InReview => "in-review",
            Self::Done => "done",
        }
    }

    /// Rank on the open → active → in-review → done progression. `Blocked` sits
    /// off this axis (a manual "stuck" flag), so it shares Active's rank and is
    /// never auto-entered or left by the reconcile.
    fn progress_rank(self) -> u8 {
        match self {
            Self::Open => 0,
            Self::Active | Self::Blocked => 1,
            Self::InReview => 2,
            Self::Done => 3,
        }
    }

    /// The state the reconcile sweep would move a task to, given whether any of
    /// its linked PRs are open / merged — or `None` to leave it unchanged. This
    /// is the trust policy in one place: it only nudges the progression FORWARD
    /// (monotonic), never auto-completes (`Done` is a human call — no
    /// gate-bypass), and never overrides a manual `Blocked` or a terminal `Done`.
    pub fn reconciled_to(self, has_open_pr: bool, has_merged_pr: bool) -> Option<Self> {
        if matches!(self, Self::Done | Self::Blocked) {
            return None; // terminal / manual — reconcile never overrides
        }
        let target = if has_merged_pr {
            Self::InReview
        } else if has_open_pr {
            Self::Active
        } else {
            return None;
        };
        (target.progress_rank() > self.progress_rank()).then_some(target)
    }
}

impl RefKind {
    /// The display string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pr => "PR",
        }
    }
}

/// One append-only transition in a [`SharedTask`]'s timeline: the state it moved
/// to, when, and which agent (or the reconcile sweep) moved it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub state: SharedTaskState,
    pub ts: String,
    pub by: String,
    /// Optional short note — why the move happened (e.g. "merged owner/repo#9").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A shared task: a team- or fleet-scoped goal that *groups* board issues + PRs
/// across agents, with participants and an append-only timeline. Unlike the
/// per-agent [`Task`] (a private scratchpad), a SharedTask is the fleet's shared
/// unit of coordinated work — it *references* issues/PRs rather than duplicating
/// them, so the cockpit and web can show who's involved and how the work moves
/// Start→Goal. Persisted one-file-per-task at `tasks/shared/<id>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedTask {
    pub id: String,
    /// One-line title.
    pub title: String,
    /// Optional longer statement of the goal / definition of done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub scope: TaskScope,
    /// For `scope = team`: the board it belongs to (`owner/projectNumber`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// The agent who created it (also its first participant).
    pub owner: String,
    /// Agents involved — owner + explicit joiners + auto-added PR authors.
    #[serde(default)]
    pub participants: Vec<String>,
    /// Linked board issues.
    #[serde(default)]
    pub issues: Vec<TaskRef>,
    /// Linked PRs.
    #[serde(default)]
    pub prs: Vec<TaskRef>,
    pub state: SharedTaskState,
    /// Append-only state transitions, oldest first; the first entry is creation.
    #[serde(default)]
    pub timeline: Vec<TaskEvent>,
    pub created: String,
    pub updated: String,
}

/// Summary row for `list_agents` (peer awareness).
#[derive(Debug, Clone)]
pub struct AgentSummary {
    pub name: String,
    /// "agent" or "human" (the supervisor).
    pub kind: String,
    pub repo: String,
    pub board: String,
    /// Short function label (e.g. "code review"), distinct from the name.
    pub role: Option<String>,
    /// Stable capability blurb from the registry (what the agent is for).
    pub description: Option<String>,
    pub state: String,
    pub task: Option<String>,
    pub last_seen: Option<String>,
    pub unread: usize,
}

/// Register (or re-register) an agent. Writes the registry file and
/// ensures the agent's inbox dir exists so peers can message it at once.
/// Returns the sanitized name actually used on disk.
pub async fn register(
    name: &str,
    repo: &str,
    board: &str,
    description: Option<&str>,
    role: Option<&str>,
) -> Result<String> {
    let name = sanitize(name);
    let dir = store_root().join("agents");
    fs::create_dir_all(&dir).await?;
    let mut obj = json!({
        "name": name,
        "repo": repo,
        "board": board,
        "started": now_iso(),
    });
    if let Some(d) = description {
        if !d.trim().is_empty() {
            obj["description"] = json!(d);
        }
    }
    // Short function label (e.g. "code review", "scan backend"), distinct
    // from the free-picked name. Surfaced in the cockpit + list_agents so
    // peers can route by capability rather than guessing a name.
    if let Some(r) = role {
        if !r.trim().is_empty() {
            obj["role"] = json!(r);
        }
    }
    // Record the agent's checkout path (this MCP runs in the session's cwd)
    // so the external inbox-waker can relaunch the agent there when mail
    // arrives while it's stopped. Best-effort.
    if let Ok(cwd) = std::env::current_dir() {
        obj["path"] = json!(cwd.to_string_lossy());
    }
    fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&obj)?,
    )
    .await?;
    fs::create_dir_all(store_root().join("inbox").join(&name)).await?;
    Ok(name)
}

/// Rename an agent: move its registry + inbox (preserving unread *and* the
/// read archive) to the new handle, update the stored `name`, drop the old
/// entry. Refuses if `new` is already taken. Returns the sanitized new name.
pub async fn rename_agent(old: &str, new: &str) -> Result<String> {
    let old = sanitize(old);
    let new = sanitize(new);
    if old == new {
        return Ok(new);
    }
    let agents = store_root().join("agents");
    let old_reg = agents.join(format!("{old}.json"));
    let new_reg = agents.join(format!("{new}.json"));
    if !fs::try_exists(&old_reg).await.unwrap_or(false) {
        bail!("no agent '{old}' to rename");
    }
    if fs::try_exists(&new_reg).await.unwrap_or(false) {
        bail!("agent '{new}' already exists — pick a free name");
    }
    // Rewrite the registry under the new name, updating the `name` field.
    let raw = fs::read_to_string(&old_reg).await?;
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
    v["name"] = json!(new);
    fs::write(&new_reg, serde_json::to_vec_pretty(&v)?).await?;
    let _ = fs::remove_file(&old_reg).await;
    // Move the heartbeat, if present.
    let old_hb = agents.join(format!("{old}.heartbeat.json"));
    if fs::try_exists(&old_hb).await.unwrap_or(false) {
        let _ = fs::rename(&old_hb, agents.join(format!("{new}.heartbeat.json"))).await;
    }
    // Move the inbox dir (keeps unread + read/ archive) when the target is free.
    let inbox = store_root().join("inbox");
    let old_inbox = inbox.join(&old);
    let new_inbox = inbox.join(&new);
    if fs::try_exists(&old_inbox).await.unwrap_or(false)
        && !fs::try_exists(&new_inbox).await.unwrap_or(false)
    {
        let _ = fs::rename(&old_inbox, &new_inbox).await;
    }
    // Carry the statusLine context report and any avatar icon to the new name.
    let old_ctx = agents.join(format!("{old}.context.json"));
    if fs::try_exists(&old_ctx).await.unwrap_or(false) {
        let _ = fs::rename(&old_ctx, agents.join(format!("{new}.context.json"))).await;
    }
    for ext in ["webp", "png", "jpg", "jpeg"] {
        let oi = agents.join(format!("{old}.icon.{ext}"));
        if fs::try_exists(&oi).await.unwrap_or(false) {
            let _ = fs::rename(&oi, agents.join(format!("{new}.icon.{ext}"))).await;
        }
    }
    // Follow the rename in projects.json — membership + coordinator are keyed
    // by name, so without this a renamed agent falls out of its team.
    rename_in_projects(&old, &new).await;
    // Carry the persona (self-card + peer-notes) and fix peers' notes about
    // this agent, so identity + relationships survive the rename too.
    crate::persona::rename_persona(&old, &new).await;
    Ok(new)
}

/// Replace `old` with `new` everywhere it appears as a project member or
/// coordinator, so a rename keeps the agent in its team. Best-effort + atomic.
async fn rename_in_projects(old: &str, new: &str) {
    let path = store_root().join("projects.json");
    let Ok(raw) = fs::read_to_string(&path).await else {
        return;
    };
    let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let mut changed = false;
    if let Some(projects) = doc.get_mut("projects").and_then(|p| p.as_array_mut()) {
        for p in projects {
            if let Some(members) = p.get_mut("members").and_then(|m| m.as_array_mut()) {
                for m in members.iter_mut() {
                    if m.as_str() == Some(old) {
                        *m = json!(new);
                        changed = true;
                    }
                }
            }
            if p.get("coordinator").and_then(|c| c.as_str()) == Some(old) {
                p["coordinator"] = json!(new);
                changed = true;
            }
        }
    }
    if changed {
        if let Ok(bytes) = serde_json::to_vec_pretty(&doc) {
            let tmp = path.with_extension("json.tmp");
            if fs::write(&tmp, bytes).await.is_ok() {
                let _ = fs::rename(&tmp, &path).await;
            }
        }
    }
}

/// Remove an agent entirely — registry, heartbeat, and inbox. Clean teardown
/// (pairs with `register`); drops inbox history. Returns the sanitized name.
pub async fn deregister_agent(name: &str) -> Result<String> {
    let name = sanitize(name);
    let agents = store_root().join("agents");
    let _ = fs::remove_file(agents.join(format!("{name}.json"))).await;
    let _ = fs::remove_file(agents.join(format!("{name}.heartbeat.json"))).await;
    let _ = fs::remove_dir_all(store_root().join("inbox").join(&name)).await;
    let _ = fs::remove_dir_all(store_root().join("personas").join(&name)).await;
    Ok(name)
}

/// Update an agent's presence. Overwrites the heartbeat file.
pub async fn heartbeat(name: &str, state: &str, task: Option<&str>) -> Result<String> {
    let name = sanitize(name);
    let dir = store_root().join("agents");
    fs::create_dir_all(&dir).await?;
    let mut obj = json!({ "ts": now_iso(), "state": state });
    if let Some(t) = task {
        if !t.trim().is_empty() {
            obj["task"] = json!(t);
        }
    }
    fs::write(
        dir.join(format!("{name}.heartbeat.json")),
        serde_json::to_vec_pretty(&obj)?,
    )
    .await?;
    Ok(name)
}

/// Auto-presence update from the lifecycle hooks — the HTTP replacement for
/// `state-hook.sh`'s heartbeat write. Replicates its sticky semantics: a
/// DELIBERATE `blocked` (set via the heartbeat tool, no marker) survives auto
/// working/idle (the agent clears it); an AUTO `blocked` (from a permission
/// prompt) clears on any activity. Preserves the agent-authored `task` across
/// auto updates. No-op for an unregistered name (mirrors the script's guard).
pub async fn set_state(name: &str, state: &str) -> Result<()> {
    let name = sanitize(name);
    let dir = store_root().join("agents");
    if fs::metadata(dir.join(format!("{name}.json")))
        .await
        .is_err()
    {
        return Ok(()); // not a registered agent → ignore
    }
    let hb_path = dir.join(format!("{name}.heartbeat.json"));
    let cur: serde_json::Value = match fs::read_to_string(&hb_path).await {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    let cur_blocked = cur["state"].as_str() == Some("blocked");
    let cur_auto = cur["auto_blocked"].as_bool().unwrap_or(false);

    let mut state = state.to_string();
    let mut auto = false;
    if state == "blocked" {
        // Fresh block from the auto path is auto — unless we're already in a
        // deliberate block, which we must not downgrade.
        auto = !(cur_blocked && !cur_auto);
    } else if (state == "working" || state == "idle") && cur_blocked && !cur_auto {
        state = "blocked".to_string(); // deliberate blocked stays until cleared
    }

    let mut obj = json!({ "ts": now_iso(), "state": state });
    if auto {
        obj["auto_blocked"] = json!(true);
    }
    if let Some(task) = cur["task"].as_str() {
        if !task.trim().is_empty() {
            obj["task"] = json!(task);
        }
    }
    fs::create_dir_all(&dir).await?;
    fs::write(&hb_path, serde_json::to_vec(&obj)?).await?;
    Ok(())
}

/// Mark an agent offline by removing its heartbeat (SessionEnd → reads offline).
pub async fn set_offline(name: &str) -> Result<()> {
    let name = sanitize(name);
    let hb = store_root()
        .join("agents")
        .join(format!("{name}.heartbeat.json"));
    let _ = fs::remove_file(hb).await;
    Ok(())
}

/// Count unread (top-level) messages in `name`'s inbox — what the Stop-hook
/// gate checks before letting a session go idle.
pub async fn unread_count(name: &str) -> usize {
    let name = sanitize(name);
    let dir = store_root().join("inbox").join(&name);
    let mut n = 0;
    if let Ok(mut rd) = fs::read_dir(&dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if e.path().extension().and_then(|s| s.to_str()) == Some("json")
                && e.file_type().await.map(|t| t.is_file()).unwrap_or(false)
            {
                n += 1;
            }
        }
    }
    n
}

/// Write an agent's context/usage snapshot — the HTTP replacement for
/// `context-statusline.sh`'s write. `ts` is stamped server-side (epoch seconds).
pub async fn write_context(
    name: &str,
    pct: Option<f64>,
    rl5h: Option<f64>,
    rl7d: Option<f64>,
    rl5h_reset: Option<i64>,
    rl7d_reset: Option<i64>,
) -> Result<()> {
    let name = sanitize(name);
    let dir = store_root().join("agents");
    fs::create_dir_all(&dir).await?;
    let mut obj = json!({ "ts": chrono::Local::now().timestamp() });
    if let Some(p) = pct {
        obj["pct"] = json!(p);
    }
    if let Some(a) = rl5h {
        obj["rl5h"] = json!(a);
    }
    if let Some(b) = rl7d {
        obj["rl7d"] = json!(b);
    }
    if let Some(r) = rl5h_reset {
        obj["rl5h_reset"] = json!(r);
    }
    if let Some(r) = rl7d_reset {
        obj["rl7d_reset"] = json!(r);
    }
    fs::write(
        dir.join(format!("{name}.context.json")),
        serde_json::to_vec(&obj)?,
    )
    .await?;
    Ok(())
}

// ---- Cockpit-originated writes (the human supervisor, messages, projects) ----
// Ports of the cockpit's `client.rs` so the remote cockpit performs the same
// store mutations over HTTP. Each project op reads → edits → atomically writes
// `projects.json` and returns the updated list for an immediate UI refresh.

/// Register the human supervisor as a client (kind "human"). Mirror of the
/// cockpit's `register_me` — overwrites the registry, ensures the inbox dir.
pub async fn register_human(name: &str) -> Result<()> {
    let name = sanitize(name);
    let dir = store_root().join("agents");
    fs::create_dir_all(&dir).await?;
    let obj = json!({
        "name": name,
        "kind": "human",
        "repo": "-",
        "board": "-",
        "description": "Supervisor — the human running the cockpit.",
        "started": now_iso(),
    });
    fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&obj)?,
    )
    .await?;
    fs::create_dir_all(store_root().join("inbox").join(&name)).await?;
    Ok(())
}

/// Archive a message by id (move it to the owner's `read/`). Mirror of the
/// cockpit's `mark_read`.
pub async fn mark_read(owner: &str, msg_id: &str) -> Result<()> {
    let owner = sanitize(owner);
    let dir = store_root().join("inbox").join(&owner);
    let suffix = format!("-{msg_id}.json");
    let Ok(mut rd) = fs::read_dir(&dir).await else {
        return Ok(());
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        if !e.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let p = e.path();
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if fname.ends_with(&suffix) {
            let read_dir = dir.join("read");
            fs::create_dir_all(&read_dir).await?;
            let _ = fs::rename(&p, read_dir.join(fname)).await;
            break;
        }
    }
    Ok(())
}

/// Delete a message by id from `owner`'s inbox — top-level (unread) or `read/`
/// (archived). No-op if absent. Mirror of the cockpit's `delete_message`.
pub async fn delete_message(owner: &str, msg_id: &str) -> Result<()> {
    let owner = sanitize(owner);
    let suffix = format!("-{msg_id}.json");
    let base = store_root().join("inbox").join(&owner);
    for dir in [base.clone(), base.join("read")] {
        let Ok(mut rd) = fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if p.file_name()
                .and_then(|s| s.to_str())
                .map(|f| f.ends_with(&suffix))
                .unwrap_or(false)
            {
                let _ = fs::remove_file(&p).await;
            }
        }
    }
    Ok(())
}

/// An agent's avatar sidecar (`agents/<name>.icon.<ext>`) → bytes + content
/// type, for the remote cockpit to fetch and cache. `None` if absent.
pub async fn read_icon(name: &str) -> Option<(Vec<u8>, &'static str)> {
    let name = sanitize(name);
    let dir = store_root().join("agents");
    let prefix = format!("{name}.icon.");
    let mut rd = fs::read_dir(&dir).await.ok()?;
    while let Ok(Some(e)) = rd.next_entry().await {
        let p = e.path();
        let Some(f) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if f.starts_with(&prefix) {
            let ct = match p.extension().and_then(|s| s.to_str()) {
                Some("webp") => "image/webp",
                Some("png") => "image/png",
                Some("jpg") | Some("jpeg") => "image/jpeg",
                _ => "application/octet-stream",
            };
            if let Ok(bytes) = fs::read(&p).await {
                return Some((bytes, ct));
            }
        }
    }
    None
}

async fn write_project_list(root: &std::path::Path, projects: &[ProjectDto]) -> Result<()> {
    fs::create_dir_all(root).await?;
    let path = root.join("projects.json");
    let tmp = path.with_extension("json.tmp");
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(&json!({ "projects": projects }))?,
    )
    .await?;
    fs::rename(&tmp, &path).await?;
    Ok(())
}

/// Create a project (slug derived + de-duplicated). Blank name ⇒ no-op.
pub async fn create_project(name: &str) -> Result<Vec<ProjectDto>> {
    let root = store_root();
    let mut projects = read_projects(&root).await;
    let name = name.trim();
    if !name.is_empty() {
        let base = sanitize(name).to_lowercase();
        let mut id = base.clone();
        let mut n = 2;
        while projects.iter().any(|p| p.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        projects.push(ProjectDto {
            id,
            name: name.to_string(),
            coordinator: None,
            members: Vec::new(),
        });
        write_project_list(&root, &projects).await?;
    }
    Ok(projects)
}

/// Rename a project by id (keeps id, members, coordinator). Blank name ⇒ no-op.
pub async fn rename_project(id: &str, name: &str) -> Result<Vec<ProjectDto>> {
    let root = store_root();
    let mut projects = read_projects(&root).await;
    let name = name.trim();
    if !name.is_empty() {
        if let Some(p) = projects.iter_mut().find(|p| p.id == id) {
            p.name = name.to_string();
        }
        write_project_list(&root, &projects).await?;
    }
    Ok(projects)
}

/// Move a project one slot later; the last wraps to the front.
pub async fn move_project_down(id: &str) -> Result<Vec<ProjectDto>> {
    let root = store_root();
    let mut projects = read_projects(&root).await;
    if let Some(i) = projects.iter().position(|p| p.id == id) {
        if i + 1 < projects.len() {
            projects.swap(i, i + 1);
        } else if projects.len() > 1 {
            let p = projects.remove(i);
            projects.insert(0, p);
        }
        write_project_list(&root, &projects).await?;
    }
    Ok(projects)
}

/// Remove a project by id; its members simply float (no longer listed).
pub async fn remove_project(id: &str) -> Result<Vec<ProjectDto>> {
    let root = store_root();
    let mut projects = read_projects(&root).await;
    projects.retain(|p| p.id != id);
    write_project_list(&root, &projects).await?;
    Ok(projects)
}

/// Set a client's membership: `Some(id)` moves it into that project (out of any
/// other); `None` makes it floating. Leaving a project it coordinated drops it
/// as that project's coordinator.
pub async fn set_membership(client: &str, project_id: Option<&str>) -> Result<Vec<ProjectDto>> {
    let root = store_root();
    let mut projects = read_projects(&root).await;
    for p in &mut projects {
        p.members.retain(|m| m != client);
        if p.coordinator.as_deref() == Some(client) && Some(p.id.as_str()) != project_id {
            p.coordinator = None;
        }
    }
    if let Some(id) = project_id {
        if let Some(p) = projects.iter_mut().find(|p| p.id == id) {
            if !p.members.iter().any(|m| m == client) {
                p.members.push(client.to_string());
            }
        }
    }
    write_project_list(&root, &projects).await?;
    Ok(projects)
}

/// Toggle `client` as `project_id`'s coordinator (setting it also ensures
/// membership; if already coordinator, clears it).
pub async fn toggle_coordinator(project_id: &str, client: &str) -> Result<Vec<ProjectDto>> {
    let root = store_root();
    let mut projects = read_projects(&root).await;
    if let Some(p) = projects.iter_mut().find(|p| p.id == project_id) {
        if p.coordinator.as_deref() == Some(client) {
            p.coordinator = None;
        } else {
            p.coordinator = Some(client.to_string());
            if !p.members.iter().any(|m| m == client) {
                p.members.push(client.to_string());
            }
        }
    }
    write_project_list(&root, &projects).await?;
    Ok(projects)
}

/// Disconnect a client: remove its registry, presence, context/icon/wake
/// sidecars and inbox, and drop it from any project. Returns the updated
/// project list. Mirror of the cockpit's `disconnect_client`.
pub async fn disconnect_client(name: &str) -> Result<Vec<ProjectDto>> {
    let name = sanitize(name);
    let root = store_root();
    let agents = root.join("agents");
    for suffix in ["json", "heartbeat.json", "wake.json", "context.json"] {
        let _ = fs::remove_file(agents.join(format!("{name}.{suffix}"))).await;
    }
    if let Ok(mut rd) = fs::read_dir(&agents).await {
        let prefix = format!("{name}.icon.");
        while let Ok(Some(e)) = rd.next_entry().await {
            if let Some(f) = e.file_name().to_str() {
                if f.starts_with(&prefix) {
                    let _ = fs::remove_file(e.path()).await;
                }
            }
        }
    }
    let _ = fs::remove_dir_all(root.join("inbox").join(&name)).await;
    let mut projects = read_projects(&root).await;
    let mut changed = false;
    for p in &mut projects {
        let before = p.members.len();
        p.members.retain(|m| m != &name);
        if p.coordinator.as_deref() == Some(name.as_str()) {
            p.coordinator = None;
            changed = true;
        }
        changed |= p.members.len() != before;
    }
    if changed {
        write_project_list(&root, &projects).await?;
    }
    Ok(projects)
}

/// Deliver a message to `to`'s inbox. The recipient need not be
/// registered — the message waits until they read it. Returns the new
/// message id.
pub async fn send_message(from: &str, to: &str, title: &str, body: &str) -> Result<String> {
    let to = sanitize(to);
    let dir = store_root().join("inbox").join(&to);
    fs::create_dir_all(&dir).await?;

    let nanos = chrono::Local::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    let counter = MSG_COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = format!(
        "{:06x}",
        ((nanos >> 8) ^ counter.wrapping_mul(2_654_435_761)) & 0xff_ffff
    );
    let ts_file = chrono::Local::now().format("%Y%m%dT%H%M%S").to_string();

    let msg = Message {
        id: id.clone(),
        time: now_iso(),
        from: from.to_string(),
        title: title.to_string(),
        body: body.to_string(),
    };
    fs::write(
        dir.join(format!("{ts_file}-{id}.json")),
        serde_json::to_vec_pretty(&msg)?,
    )
    .await?;
    Ok(id)
}

/// Read `name`'s unread messages (newest first). When `mark_read`, each
/// returned message is moved to the `read/` archive so it isn't
/// reprocessed next time.
pub async fn read_inbox(name: &str, mark_read: bool) -> Result<Vec<Message>> {
    let name = sanitize(name);
    let dir = store_root().join("inbox").join(&name);

    let mut found: Vec<(PathBuf, Message)> = Vec::new();
    if let Ok(mut rd) = fs::read_dir(&dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if !e.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
                continue; // skips the `read` subdir
            }
            if let Ok(raw) = fs::read_to_string(&p).await {
                if let Ok(m) = serde_json::from_str::<Message>(&raw) {
                    found.push((p, m));
                }
            }
        }
    }
    // ISO-8601 strings sort chronologically; reverse for newest-first.
    found.sort_by(|a, b| b.1.time.cmp(&a.1.time));

    if mark_read && !found.is_empty() {
        let read_dir = dir.join("read");
        fs::create_dir_all(&read_dir).await?;
        for (p, _) in &found {
            if let Some(fname) = p.file_name() {
                let _ = fs::rename(p, read_dir.join(fname)).await;
            }
        }
    }
    Ok(found.into_iter().map(|(_, m)| m).collect())
}

/// Broadcast a message to every registered client's inbox except the sender.
/// Returns the number of recipients it was delivered to.
pub async fn broadcast(from: &str, title: &str, body: &str) -> Result<usize> {
    let agents = list_agents().await?;
    let mut delivered = 0;
    for a in &agents {
        if a.name == from {
            continue;
        }
        if send_message(from, &a.name, title, body).await.is_ok() {
            delivered += 1;
        }
    }
    Ok(delivered)
}

/// Send a recap to every human "client" (the supervisor). Returns the names
/// notified. Finds the supervisor(s) automatically by `kind == "human"`, so
/// callers don't need to know the supervisor's name.
pub async fn notify_supervisor(from: &str, title: &str, body: &str) -> Result<Vec<String>> {
    let agents = list_agents().await?;
    let mut notified = Vec::new();
    for a in &agents {
        if a.kind == "human"
            && a.name != from
            && send_message(from, &a.name, title, body).await.is_ok()
        {
            notified.push(a.name.clone());
        }
    }
    Ok(notified)
}

// ---- Tasks (per-agent reminder list) ---------------------------------------
// One JSON array per agent at `tasks/<name>.json`. Mutable list (toggle done,
// drop), so it's a single rewritten file — unlike the inbox's one-file-per-msg.

fn tasks_path(name: &str) -> PathBuf {
    store_root().join("tasks").join(format!("{name}.json"))
}

/// Read an agent's task list (open + done), in stored order (oldest first).
/// Missing/unreadable/malformed ⇒ empty.
pub async fn read_tasks(name: &str) -> Vec<Task> {
    let name = sanitize(name);
    match fs::read_to_string(tasks_path(&name)).await {
        Ok(raw) => serde_json::from_str::<Vec<Task>>(&raw).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Atomically rewrite an agent's task list. `name` must already be sanitized.
async fn write_tasks(name: &str, tasks: &[Task]) -> Result<()> {
    let dir = store_root().join("tasks");
    fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{name}.json"));
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(tasks)?).await?;
    fs::rename(&tmp, &path).await?;
    Ok(())
}

/// Append a task to an agent's list. Returns the created task (with its id).
pub async fn add_task(
    name: &str,
    text: &str,
    body: Option<String>,
    pr: Option<TaskPr>,
    from_msg: Option<String>,
) -> Result<Task> {
    let name = sanitize(name);
    let mut tasks = read_tasks(&name).await;
    let task = Task {
        id: short_id(&TASK_COUNTER),
        text: text.trim().to_string(),
        body: body.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()),
        done: false,
        created: now_iso(),
        pr,
        from_msg,
    };
    tasks.push(task.clone());
    write_tasks(&name, &tasks).await?;
    Ok(task)
}

/// Set a task's done flag by id. Returns true if a task matched.
pub async fn set_task_done(name: &str, id: &str, done: bool) -> Result<bool> {
    let name = sanitize(name);
    let mut tasks = read_tasks(&name).await;
    let mut found = false;
    for t in &mut tasks {
        if t.id == id {
            t.done = done;
            found = true;
        }
    }
    if found {
        write_tasks(&name, &tasks).await?;
    }
    Ok(found)
}

/// Remove a task by id. Returns true if a task was removed.
pub async fn drop_task(name: &str, id: &str) -> Result<bool> {
    let name = sanitize(name);
    let mut tasks = read_tasks(&name).await;
    let before = tasks.len();
    tasks.retain(|t| t.id != id);
    let removed = tasks.len() != before;
    if removed {
        write_tasks(&name, &tasks).await?;
    }
    Ok(removed)
}

// ---- Shared tasks (team/fleet goals grouping issues + PRs) -----------------
// One JSON file per shared task at `tasks/shared/<id>.json` — not one array per
// agent like private tasks. A shared task has many participants, so any agent
// reads/advances it independently and a write touches only that one file.

fn shared_tasks_dir() -> PathBuf {
    store_root().join("tasks").join("shared")
}

fn shared_task_path(id: &str) -> PathBuf {
    shared_tasks_dir().join(format!("{id}.json"))
}

/// Read one shared task by id. Missing/unreadable/malformed ⇒ `None`.
pub async fn read_shared_task(id: &str) -> Option<SharedTask> {
    let raw = fs::read_to_string(shared_task_path(&sanitize(id)))
        .await
        .ok()?;
    serde_json::from_str(&raw).ok()
}

/// All shared tasks, newest-created first. Missing dir ⇒ empty.
pub async fn list_shared_tasks() -> Vec<SharedTask> {
    let mut out = Vec::new();
    if let Ok(mut rd) = fs::read_dir(shared_tasks_dir()).await {
        while let Ok(Some(ent)) = rd.next_entry().await {
            let p = ent.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(raw) = fs::read_to_string(&p).await {
                if let Ok(st) = serde_json::from_str::<SharedTask>(&raw) {
                    out.push(st);
                }
            }
        }
    }
    out.sort_by(|a, b| b.created.cmp(&a.created));
    out
}

/// Atomically write a shared task (create or update). The tmp path is unique
/// per write — shared tasks are multi-writer, so two agents writing the SAME
/// task must not share a tmp file (their writes would interleave into one torn
/// file that then gets renamed into place). pid + a per-write id keeps it unique
/// across processes and within one. The rename is atomic, so a reader sees the
/// old file or the new one, never a partial write. (The read-modify-write
/// lost-update — two writers, last wins — is inherent to a lockless file store;
/// acceptable at this volume, revisit with a per-task lock if one gets hot.)
async fn write_shared_task(st: &SharedTask) -> Result<()> {
    let dir = shared_tasks_dir();
    fs::create_dir_all(&dir).await?;
    let path = shared_task_path(&st.id);
    let tmp = dir.join(format!(
        "{}.{}.{}.tmp",
        st.id,
        std::process::id(),
        short_id(&SHARED_TASK_COUNTER)
    ));
    fs::write(&tmp, serde_json::to_vec_pretty(st)?).await?;
    fs::rename(&tmp, &path).await?;
    Ok(())
}

/// Create a shared task. The owner becomes its first participant and creation is
/// recorded as the first timeline entry. Returns the created task.
pub async fn create_shared_task(
    owner: &str,
    title: &str,
    goal: Option<String>,
    scope: TaskScope,
    team: Option<String>,
) -> Result<SharedTask> {
    let owner = sanitize(owner);
    let title = title.trim();
    if title.is_empty() {
        bail!("shared task title must not be empty");
    }
    let team = team.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if scope == TaskScope::Team && team.is_none() {
        bail!("a team-scoped shared task needs a team (owner/projectNumber)");
    }
    let team = if scope == TaskScope::Team { team } else { None };

    // Shared ids live in ONE namespace across every agent's MCP process (unlike
    // per-agent task ids, which are namespaced by file), so guard against the
    // rare clock+counter clash rather than silently overwriting a peer's task.
    let mut id = short_id(&SHARED_TASK_COUNTER);
    for _ in 0..8 {
        if fs::metadata(shared_task_path(&id)).await.is_err() {
            break; // path is free
        }
        id = short_id(&SHARED_TASK_COUNTER);
    }

    let now = now_iso();
    let st = SharedTask {
        id,
        title: title.to_string(),
        goal: goal.map(|g| g.trim().to_string()).filter(|g| !g.is_empty()),
        scope,
        team,
        owner: owner.clone(),
        participants: vec![owner.clone()],
        issues: Vec::new(),
        prs: Vec::new(),
        state: SharedTaskState::Open,
        timeline: vec![TaskEvent {
            state: SharedTaskState::Open,
            ts: now.clone(),
            by: owner,
            note: None,
        }],
        created: now.clone(),
        updated: now,
    };
    write_shared_task(&st).await?;
    Ok(st)
}

/// Link a board issue or PR to a shared task (idempotent). Errors if no such task.
pub async fn link_shared_task(
    id: &str,
    kind: RefKind,
    repo: &str,
    number: u64,
) -> Result<SharedTask> {
    let mut st = match read_shared_task(id).await {
        Some(s) => s,
        None => bail!("no shared task '{}'", sanitize(id)),
    };
    let repo = repo.trim().to_string();
    if !is_repo_slug(&repo) {
        bail!("link repo must look like 'owner/name'");
    }
    let list = match kind {
        RefKind::Issue => &mut st.issues,
        RefKind::Pr => &mut st.prs,
    };
    if !list.iter().any(|r| r.repo == repo && r.number == number) {
        list.push(TaskRef { repo, number });
        st.updated = now_iso();
        write_shared_task(&st).await?;
    }
    Ok(st)
}

/// Add an agent to a shared task's participants (idempotent). Errors if no such task.
pub async fn join_shared_task(id: &str, agent: &str) -> Result<SharedTask> {
    let agent = sanitize(agent);
    let mut st = match read_shared_task(id).await {
        Some(s) => s,
        None => bail!("no shared task '{}'", sanitize(id)),
    };
    if !st.participants.contains(&agent) {
        st.participants.push(agent);
        st.updated = now_iso();
        write_shared_task(&st).await?;
    }
    Ok(st)
}

/// Move a shared task to a new state, appending a timeline entry (no-op if it is
/// already in that state). `by` is the agent — or "reconcile" — making the move.
pub async fn advance_shared_task(
    id: &str,
    state: SharedTaskState,
    by: &str,
    note: Option<&str>,
) -> Result<SharedTask> {
    let by = sanitize(by);
    let mut st = match read_shared_task(id).await {
        Some(s) => s,
        None => bail!("no shared task '{}'", sanitize(id)),
    };
    if st.state != state {
        let now = now_iso();
        st.state = state;
        st.timeline.push(TaskEvent {
            state,
            ts: now.clone(),
            by,
            note: note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
        });
        st.updated = now;
        write_shared_task(&st).await?;
    }
    Ok(st)
}

/// Reconcile a shared task's state from PR signals — atomically. Reads the
/// CURRENT state and applies [`SharedTaskState::reconciled_to`] to *that* state,
/// then writes, so a manual `done`/`blocked` set after a sweep sampled its gh
/// signals can't be clobbered (the decision and the write share one read). The
/// move is stamped `by = "reconcile"`. Returns the new state if it advanced.
pub async fn reconcile_advance(
    id: &str,
    has_open_pr: bool,
    has_merged_pr: bool,
    note: Option<&str>,
) -> Result<Option<SharedTaskState>> {
    let mut st = match read_shared_task(id).await {
        Some(s) => s,
        None => return Ok(None),
    };
    let Some(next) = st.state.reconciled_to(has_open_pr, has_merged_pr) else {
        return Ok(None);
    };
    let now = now_iso();
    st.state = next;
    st.timeline.push(TaskEvent {
        state: next,
        ts: now.clone(),
        by: "reconcile".to_string(),
        note: note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
    });
    st.updated = now;
    write_shared_task(&st).await?;
    Ok(Some(next))
}

/// List all registered agents with current presence + unread count.
pub async fn list_agents() -> Result<Vec<AgentSummary>> {
    let agents_dir = store_root().join("agents");
    let inbox_root = store_root().join("inbox");
    let mut out: Vec<AgentSummary> = Vec::new();

    let Ok(mut rd) = fs::read_dir(&agents_dir).await else {
        return Ok(out);
    };
    while let Ok(Some(e)) = rd.next_entry().await {
        let p = e.path();
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // Registry files only — skip the sidecar files that live next to a
        // registration: heartbeat presence, the codex-waker wake-config, and
        // the cockpit's per-agent context snapshot (agents/<name>.context.json).
        if !fname.ends_with(".json")
            || fname.ends_with(".heartbeat.json")
            || fname.ends_with(".wake.json")
            || fname.ends_with(".context.json")
        {
            continue;
        }
        let stem = fname.trim_end_matches(".json").to_string();
        let Ok(raw) = fs::read_to_string(&p).await else {
            continue;
        };
        let Ok(reg) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let name = reg["name"].as_str().unwrap_or(&stem).to_string();
        let kind = reg["kind"].as_str().unwrap_or("agent").to_string();
        let repo = reg["repo"].as_str().unwrap_or("").to_string();
        let board = reg["board"].as_str().unwrap_or("").to_string();
        let role = reg["role"].as_str().map(String::from);
        let description = reg["description"].as_str().map(String::from);

        let mut state = "unknown".to_string();
        let mut task = None;
        let mut last_seen = None;
        let hb = agents_dir.join(format!("{stem}.heartbeat.json"));
        if let Ok(hraw) = fs::read_to_string(&hb).await {
            if let Ok(h) = serde_json::from_str::<serde_json::Value>(&hraw) {
                if let Some(s) = h["state"].as_str() {
                    state = s.to_string();
                }
                task = h["task"].as_str().map(String::from);
                last_seen = h["ts"].as_str().map(String::from);
            }
        }

        let mut unread = 0;
        if let Ok(mut ird) = fs::read_dir(inbox_root.join(&stem)).await {
            while let Ok(Some(ie)) = ird.next_entry().await {
                if ie.path().extension().and_then(|s| s.to_str()) == Some("json")
                    && ie.file_type().await.map(|t| t.is_file()).unwrap_or(false)
                {
                    unread += 1;
                }
            }
        }

        out.push(AgentSummary {
            name,
            kind,
            repo,
            board,
            role,
            description,
            state,
            task,
            last_seen,
            unread,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// An agent's presence goes "stale" once its heartbeat is older than this.
/// Mirror of the cockpit's `STALE_AFTER_MINUTES` so the remote cockpit's
/// ordering and dimming match what a local (file-mode) cockpit would show.
const STALE_AFTER_MINUTES: i64 = 15;

/// Assemble the full store snapshot for the remote cockpit (`GET /v1/snapshot`).
/// Ports the cockpit's `read_snapshot`: registry+heartbeat → agents, inbox dirs
/// → messages, `projects.json`, freshest `<name>.context.json` → context% +
/// account usage, then attention-first ordering. Never errors — a missing or
/// unreadable store yields an empty snapshot.
pub async fn snapshot() -> crate::wire::SnapshotDto {
    use crate::wire::{AgentDto, MessageDto, SnapshotDto, UsageDto};

    let root = store_root();
    let agents_dir = root.join("agents");
    let inbox_root = root.join("inbox");

    // Agent base (registry + heartbeat + unread) via the existing primitive.
    let summaries = list_agents().await.unwrap_or_default();

    // Inboxes: one dir per agent, full messages (read + unread), newest first.
    let mut inboxes: std::collections::BTreeMap<String, Vec<MessageDto>> =
        std::collections::BTreeMap::new();
    if let Ok(mut rd) = fs::read_dir(&inbox_root).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            if e.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = e.file_name().to_str().map(String::from) {
                    inboxes.insert(name, read_inbox_messages(&e.path()).await);
                }
            }
        }
    }

    // Per-agent task lists: one JSON file per agent under tasks/.
    let mut tasks: std::collections::BTreeMap<String, Vec<crate::wire::TaskDto>> =
        std::collections::BTreeMap::new();
    if let Ok(mut rd) = fs::read_dir(root.join("tasks")).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                tasks.insert(stem.to_string(), read_task_dtos(&p).await);
            }
        }
    }

    // Shared tasks (team/fleet goals grouping issues + PRs across agents).
    let shared_tasks: Vec<crate::wire::SharedTaskDto> = list_shared_tasks()
        .await
        .into_iter()
        .map(shared_task_dto)
        .collect();

    // Per-agent context% + the freshest account rate-limit usage.
    let mut usage: Option<(i64, UsageDto)> = None;
    let mut agents: Vec<AgentDto> = Vec::with_capacity(summaries.len());
    for s in summaries {
        let mut context_pct = None;
        if let Ok(raw) =
            fs::read_to_string(agents_dir.join(format!("{}.context.json", s.name))).await
        {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                context_pct = v["pct"].as_f64().map(|p| p.round().clamp(0.0, 100.0) as u8);
                if let (Some(ts), Some(a), Some(b)) =
                    (v["ts"].as_i64(), v["rl5h"].as_f64(), v["rl7d"].as_f64())
                {
                    if usage.as_ref().map(|(t, _)| ts > *t).unwrap_or(true) {
                        usage = Some((
                            ts,
                            UsageDto {
                                five_hour_pct: a as f32,
                                seven_day_pct: b as f32,
                                ts,
                                five_hour_reset: v["rl5h_reset"].as_i64(),
                                seven_day_reset: v["rl7d_reset"].as_i64(),
                            },
                        ));
                    }
                }
            }
        }
        let last_seen_dt = s
            .last_seen
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
        let stale = is_stale(&s.kind, last_seen_dt);
        let unread = inboxes
            .get(&s.name)
            .map(|m| m.iter().filter(|m| !m.read).count() as u32)
            .unwrap_or(s.unread as u32);
        agents.push(AgentDto {
            name: s.name,
            kind: s.kind,
            repo: s.repo,
            board: s.board,
            role: s.role,
            description: s.description,
            state: s.state,
            task: s.task,
            last_seen: s.last_seen,
            stale,
            unread,
            context_pct,
        });
    }

    // Attention-first: human first, stale last, then state rank, then name.
    agents.sort_by(|a, b| {
        (b.kind == "human")
            .cmp(&(a.kind == "human"))
            .then(a.stale.cmp(&b.stale))
            .then(state_sort_rank(&a.state).cmp(&state_sort_rank(&b.state)))
            .then(a.name.cmp(&b.name))
    });

    SnapshotDto {
        agents,
        inboxes,
        tasks,
        shared_tasks,
        projects: read_projects(&root).await,
        usage: usage.map(|(_, u)| u),
    }
}

/// Convert a stored [`SharedTask`] into its wire DTO (enums → display strings).
fn shared_task_dto(st: SharedTask) -> crate::wire::SharedTaskDto {
    use crate::wire::{SharedTaskDto, TaskEventDto, TaskRefDto};
    let to_refs = |v: Vec<TaskRef>| -> Vec<TaskRefDto> {
        v.into_iter()
            .map(|r| TaskRefDto {
                repo: r.repo,
                number: r.number,
            })
            .collect()
    };
    SharedTaskDto {
        id: st.id,
        title: st.title,
        goal: st.goal,
        scope: st.scope.as_str().to_string(),
        team: st.team,
        owner: st.owner,
        participants: st.participants,
        issues: to_refs(st.issues),
        prs: to_refs(st.prs),
        state: st.state.as_str().to_string(),
        timeline: st
            .timeline
            .into_iter()
            .map(|e| TaskEventDto {
                state: e.state.as_str().to_string(),
                ts: e.ts,
                by: e.by,
                note: e.note,
            })
            .collect(),
        created: st.created,
        updated: st.updated,
    }
}

/// Read one `tasks/<name>.json` → wire DTOs, preserving stored order. A
/// malformed/unreadable file yields an empty list (best-effort, like the
/// inbox/project readers).
async fn read_task_dtos(path: &std::path::Path) -> Vec<crate::wire::TaskDto> {
    let Ok(raw) = fs::read_to_string(path).await else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<Task>>(&raw)
        .unwrap_or_default()
        .into_iter()
        .map(|t| crate::wire::TaskDto {
            id: t.id,
            text: t.text,
            body: t.body,
            done: t.done,
            created: t.created,
            pr: t.pr.map(|p| crate::wire::TaskPrDto {
                repo: p.repo,
                number: p.number,
            }),
            from_msg: t.from_msg,
        })
        .collect()
}

/// Read one inbox dir → messages (top-level = unread, `read/` = read), newest
/// first. Mirrors the cockpit's `read_inbox` reader.
async fn read_inbox_messages(dir: &std::path::Path) -> Vec<crate::wire::MessageDto> {
    let mut msgs = Vec::new();
    read_message_dir(dir, false, &mut msgs).await;
    read_message_dir(&dir.join("read"), true, &mut msgs).await;
    // Newest first; unparseable/absent times sort last.
    msgs.sort_by(|a, b| {
        let pa = a
            .time
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
        let pb = b
            .time
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
        pb.cmp(&pa)
    });
    msgs
}

async fn read_message_dir(
    dir: &std::path::Path,
    read: bool,
    out: &mut Vec<crate::wire::MessageDto>,
) {
    let Ok(mut entries) = fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path).await else {
            continue;
        };
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            out.push(crate::wire::MessageDto {
                id: v["id"].as_str().unwrap_or("").to_string(),
                time: v["time"].as_str().map(String::from),
                from: v["from"].as_str().unwrap_or("").to_string(),
                title: v["title"].as_str().unwrap_or("").to_string(),
                body: v["body"].as_str().unwrap_or("").to_string(),
                read,
            });
        }
    }
}

/// Read `projects.json` (`{ "projects": [...] }`) → DTOs. Missing/malformed ⇒ none.
async fn read_projects(root: &std::path::Path) -> Vec<crate::wire::ProjectDto> {
    let Ok(raw) = fs::read_to_string(root.join("projects.json")).await else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    v["projects"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|p| crate::wire::ProjectDto {
                    id: p["id"].as_str().unwrap_or("").to_string(),
                    name: p["name"].as_str().unwrap_or("").to_string(),
                    coordinator: p["coordinator"].as_str().map(String::from),
                    members: p["members"]
                        .as_array()
                        .map(|m| {
                            m.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Mirror of the cockpit's stale rule: the human is always present; an agent is
/// stale if it never heartbeat, or its last heartbeat is older than the cutoff.
fn is_stale(kind: &str, last_seen: Option<chrono::DateTime<chrono::FixedOffset>>) -> bool {
    if kind == "human" {
        return false;
    }
    match last_seen {
        Some(ts) => {
            chrono::Local::now()
                .fixed_offset()
                .signed_duration_since(ts)
                .num_minutes()
                > STALE_AFTER_MINUTES
        }
        None => true,
    }
}

/// Attention-first state ordering (mirror of the cockpit's `AgentState::sort_rank`).
fn state_sort_rank(state: &str) -> u8 {
    match state {
        "blocked" => 0,
        "waiting" => 1,
        "working" => 2,
        "idle" => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_traversal() {
        // The invariant that matters: the result is always a single, safe
        // path component — no separators, never `.`/`..`.
        let evil = sanitize("../../etc/passwd");
        assert!(!evil.contains('/'), "no path separators: {evil}");
        assert!(
            evil != "." && evil != "..",
            "not a traversal component: {evil}"
        );
        assert_eq!(sanitize(".."), "unnamed");
        assert_eq!(sanitize(""), "unnamed");
        assert_eq!(sanitize("vscode-bot"), "vscode-bot");
        assert_eq!(sanitize("Bot 1!"), "Bot-1-");
    }

    #[test]
    fn store_root_honors_env_override() {
        // Serialize against the HOME-mutating tests — we touch process-global env.
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("LAKITU_FLEET_ROOT", "/tmp/lakitu-root-override");
            std::env::remove_var("GENBOT_ROOT");
        }
        assert_eq!(
            store_root(),
            std::path::PathBuf::from("/tmp/lakitu-root-override"),
            "LAKITU_FLEET_ROOT overrides the default store path"
        );
        unsafe {
            std::env::remove_var("LAKITU_FLEET_ROOT");
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // _env guard intentionally held across .await
    async fn round_trip_register_heartbeat_message() {
        // Serialize every test that mutates the global HOME so the parallel
        // runner can't stomp our temp store; held for the whole test.
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Isolate the store under a temp HOME so we don't touch the real one.
        let home = std::env::temp_dir().join(format!("fleet-mcp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: the TEST_ENV_LOCK guard above keeps any other test from
        // reading or writing HOME while we hold it.
        unsafe {
            std::env::set_var("HOME", &home);
            // A dev's exported root override would otherwise hijack the temp store.
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }

        register(
            "alice",
            "acme/web",
            "acme/14",
            Some("VS Code side — ask me to wire UI"),
            Some("VS Code UI"),
        )
        .await
        .unwrap();
        heartbeat("alice", "working", Some("issue #90"))
            .await
            .unwrap();
        let id = send_message("bob", "alice", "hello", "need a thing")
            .await
            .unwrap();
        assert_eq!(id.len(), 6);

        // Sidecar files sitting next to the registry must NOT be read as agents
        // (else they'd phantom "alice.wake" / "alice.context" entries).
        std::fs::write(
            home.join(".claude/lakitu-fleet/agents/alice.wake.json"),
            br#"{"mode":"codex-app-server","wake_cmd":""}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".claude/lakitu-fleet/agents/alice.context.json"),
            br#"{"pct":42}"#,
        )
        .unwrap();

        let agents = list_agents().await.unwrap();
        assert_eq!(agents.len(), 1, "sidecar files must not count as agents");
        assert_eq!(agents[0].name, "alice");
        assert_eq!(agents[0].kind, "agent");
        assert_eq!(agents[0].state, "working");
        assert_eq!(agents[0].unread, 1);
        assert_eq!(
            agents[0].description.as_deref(),
            Some("VS Code side — ask me to wire UI"),
            "registry description round-trips"
        );
        assert_eq!(
            agents[0].role.as_deref(),
            Some("VS Code UI"),
            "registry role round-trips"
        );

        // First read returns + archives the message; second read is empty.
        let msgs = read_inbox("alice", true).await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].title, "hello");
        assert_eq!(msgs[0].from, "bob");
        let again = read_inbox("alice", true).await.unwrap();
        assert!(again.is_empty(), "message should have moved to read/");

        // notify_supervisor finds the human client and recaps to them.
        let agents_dir = home.join(".claude/lakitu-fleet/agents");
        std::fs::write(
            agents_dir.join("you.json"),
            r#"{"name":"you","kind":"human"}"#,
        )
        .unwrap();
        let notified = notify_supervisor("alice", "recap", "shipped the fix")
            .await
            .unwrap();
        assert_eq!(notified, vec!["you".to_string()]);
        let sup_inbox = read_inbox("you", false).await.unwrap();
        assert_eq!(sup_inbox.len(), 1);
        assert_eq!(sup_inbox[0].title, "recap");
        assert_eq!(sup_inbox[0].from, "alice");

        // rename moves the registry + inbox (incl. the read archive) and drops
        // the old entry; refuses a taken name.
        rename_agent("alice", "alice-2").await.unwrap();
        let agents = list_agents().await.unwrap();
        assert!(
            agents.iter().any(|a| a.name == "alice-2"),
            "renamed entry present"
        );
        assert!(!agents.iter().any(|a| a.name == "alice"), "old entry gone");
        assert!(
            home.join(".claude/lakitu-fleet/inbox/alice-2/read")
                .exists(),
            "read archive moved with the rename"
        );
        assert!(
            rename_agent("alice-2", "you").await.is_err(),
            "won't clobber an existing name"
        );

        // deregister removes it entirely.
        deregister_agent("alice-2").await.unwrap();
        assert!(
            !list_agents()
                .await
                .unwrap()
                .iter()
                .any(|a| a.name == "alice-2"),
            "deregistered entry gone"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // _env guard intentionally held across .await
    async fn round_trip_tasks() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("fleet-tasks-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: TEST_ENV_LOCK serializes the HOME-mutating tests.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }

        // Empty to start.
        assert!(read_tasks("aria").await.is_empty());

        // Add a loose task and a PR-linked one (with a body + message provenance).
        let t1 = add_task("aria", "  reply to samus  ", None, None, None)
            .await
            .unwrap();
        assert_eq!(t1.text, "reply to samus", "text is trimmed");
        assert_eq!(t1.id.len(), 6);
        assert!(t1.body.is_none());
        let t2 = add_task(
            "aria",
            "update the changelog",
            Some("  cover the dedup fix and the new flag  ".into()),
            Some(TaskPr {
                repo: "acme/lakitu".into(),
                number: 12,
            }),
            Some("9a8b7c".into()),
        )
        .await
        .unwrap();

        let tasks = read_tasks("aria").await;
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().all(|t| !t.done), "new tasks start open");
        assert_eq!(tasks[1].pr.as_ref().unwrap().number, 12);
        assert_eq!(tasks[1].from_msg.as_deref(), Some("9a8b7c"));
        assert_eq!(
            tasks[1].body.as_deref(),
            Some("cover the dedup fix and the new flag"),
            "body trimmed + stored"
        );

        // Complete one; it stays in the list, now done.
        assert!(set_task_done("aria", &t1.id, true).await.unwrap());
        let tasks = read_tasks("aria").await;
        assert_eq!(tasks.iter().filter(|t| t.done).count(), 1);
        assert!(
            !set_task_done("aria", "nope", true).await.unwrap(),
            "unknown id ⇒ false"
        );

        // Tasks surface in the snapshot under the agent's name.
        let snap = snapshot().await;
        let aria_tasks = snap.tasks.get("aria").expect("aria has tasks in snapshot");
        assert_eq!(aria_tasks.len(), 2);

        // Drop removes it.
        assert!(drop_task("aria", &t2.id).await.unwrap());
        assert!(
            !drop_task("aria", &t2.id).await.unwrap(),
            "second drop ⇒ false"
        );
        assert_eq!(read_tasks("aria").await.len(), 1);

        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // _env guard intentionally held across .await
    async fn round_trip_shared_tasks() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("fleet-shared-tasks-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: TEST_ENV_LOCK serializes the HOME-mutating tests.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }

        // Empty to start.
        assert!(list_shared_tasks().await.is_empty());

        // Create a fleet task: owner is first participant, title trimmed, creation
        // is the first timeline entry.
        let st = create_shared_task(
            "lakitu",
            "  Release 0.3.1  ",
            Some("  publish both crates  ".into()),
            TaskScope::Fleet,
            None,
        )
        .await
        .unwrap();
        assert_eq!(st.id.len(), 6);
        assert_eq!(st.title, "Release 0.3.1", "title trimmed");
        assert_eq!(st.goal.as_deref(), Some("publish both crates"));
        assert_eq!(st.scope, TaskScope::Fleet);
        assert!(st.team.is_none());
        assert_eq!(st.participants, vec!["lakitu".to_string()]);
        assert_eq!(st.state, SharedTaskState::Open);
        assert_eq!(st.timeline.len(), 1);
        assert_eq!(st.timeline[0].by, "lakitu");

        // Team scope requires a team; reads back by id; unknown id ⇒ None.
        assert!(
            create_shared_task("lakitu", "x", None, TaskScope::Team, None)
                .await
                .is_err(),
            "team scope without a team is rejected"
        );
        let team = create_shared_task(
            "toad",
            "Ship the web UI",
            None,
            TaskScope::Team,
            Some("  dac2k9/1  ".into()),
        )
        .await
        .unwrap();
        assert_eq!(team.team.as_deref(), Some("dac2k9/1"), "team trimmed");
        assert_eq!(
            read_shared_task(&st.id).await.unwrap().title,
            "Release 0.3.1"
        );
        assert!(read_shared_task("nope").await.is_none());

        // Linking is idempotent and pinned to {repo, number}.
        link_shared_task(&st.id, RefKind::Issue, "dac2k9/lakitu-oss", 5)
            .await
            .unwrap();
        link_shared_task(&st.id, RefKind::Pr, "dac2k9/lakitu-oss", 9)
            .await
            .unwrap();
        let linked = link_shared_task(&st.id, RefKind::Pr, "dac2k9/lakitu-oss", 9)
            .await
            .unwrap();
        assert_eq!(linked.issues.len(), 1);
        assert_eq!(linked.prs.len(), 1, "re-linking the same PR is idempotent");

        // Joining adds a participant; idempotent.
        join_shared_task(&st.id, "protoman").await.unwrap();
        let joined = join_shared_task(&st.id, "protoman").await.unwrap();
        assert_eq!(joined.participants.len(), 2, "re-join is idempotent");

        // Advancing appends a transition; same-state advance is a no-op.
        let adv = advance_shared_task(&st.id, SharedTaskState::Active, "lakitu", None)
            .await
            .unwrap();
        assert_eq!(adv.state, SharedTaskState::Active);
        assert_eq!(adv.timeline.len(), 2);
        let adv = advance_shared_task(&st.id, SharedTaskState::Active, "lakitu", None)
            .await
            .unwrap();
        assert_eq!(adv.timeline.len(), 2, "same-state advance is a no-op");
        advance_shared_task(&st.id, SharedTaskState::Done, "lakitu", None)
            .await
            .unwrap();

        // The store lists both (participant/done filtering lives in the tool layer).
        assert_eq!(list_shared_tasks().await.len(), 2);

        // Snapshot surfaces shared tasks with enums rendered as wire strings.
        let snap = snapshot().await;
        assert_eq!(snap.shared_tasks.len(), 2);
        let rel = snap
            .shared_tasks
            .iter()
            .find(|t| t.id == st.id)
            .expect("release task in snapshot");
        assert_eq!(rel.scope, "fleet");
        assert_eq!(rel.state, "done");
        assert_eq!(rel.prs.len(), 1);
        assert_eq!(rel.participants.len(), 2);
        assert_eq!(rel.timeline.last().unwrap().state, "done");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // _env guard intentionally held across .await
    async fn shared_task_store_robustness() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("fleet-shared-robust-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: TEST_ENV_LOCK serializes the HOME-mutating tests.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }

        // A real task to coexist with the bad files below.
        let st = create_shared_task("lakitu", "Real", None, TaskScope::Fleet, None)
            .await
            .unwrap();
        let dir = store_root().join("tasks").join("shared");

        // Malformed JSON is skipped on read — never crashes list/read.
        std::fs::write(dir.join("junk.json"), b"{ not valid json").unwrap();
        assert!(
            read_shared_task("junk").await.is_none(),
            "malformed => None"
        );
        let all = list_shared_tasks().await;
        assert_eq!(all.len(), 1, "malformed file skipped, real task kept");
        assert_eq!(all[0].id, st.id);

        // A file from a hypothetical newer version (extra unknown field) still
        // parses — no deny_unknown_fields => forward-compatible.
        let p = dir.join(format!("{}.json", st.id));
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        v["future_field"] = serde_json::json!("ignored by older readers");
        std::fs::write(&p, v.to_string()).unwrap();
        let reread = read_shared_task(&st.id)
            .await
            .expect("extra unknown field still parses");
        assert_eq!(reread.title, "Real");

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn reconcile_state_policy() {
        use SharedTaskState::*;
        // A linked open PR nudges toward active; a merged one toward in-review.
        assert_eq!(Open.reconciled_to(true, false), Some(Active));
        assert_eq!(Open.reconciled_to(false, true), Some(InReview));
        assert_eq!(Active.reconciled_to(false, true), Some(InReview));
        // Idempotent / no forward signal => no change.
        assert_eq!(Active.reconciled_to(true, false), None);
        assert_eq!(InReview.reconciled_to(false, true), None);
        assert_eq!(Open.reconciled_to(false, false), None);
        // Monotonic: a new open PR after in-review never regresses the task.
        assert_eq!(InReview.reconciled_to(true, false), None);
        // No gate-bypass / no override of manual or terminal states.
        assert_eq!(InReview.reconciled_to(true, true), None, "never auto-done");
        assert_eq!(Done.reconciled_to(true, true), None, "terminal");
        assert_eq!(
            Blocked.reconciled_to(false, true),
            None,
            "manual block kept"
        );
    }

    #[test]
    fn repo_slug_shape() {
        assert!(is_repo_slug("dac2k9/lakitu"));
        assert!(!is_repo_slug("lakitu"), "no slash");
        assert!(!is_repo_slug("a/b/c"), "too many segments");
        assert!(!is_repo_slug("/lakitu"), "empty owner");
        assert!(!is_repo_slug("dac2k9/"), "empty name");
        assert!(!is_repo_slug(""));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // _env guard intentionally held across .await
    async fn reconcile_advance_applies_policy_atomically() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home =
            std::env::temp_dir().join(format!("fleet-reconcile-adv-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: TEST_ENV_LOCK serializes the HOME-mutating tests.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }

        let st = create_shared_task("lakitu", "Reconcile me", None, TaskScope::Fleet, None)
            .await
            .unwrap();

        // A merged-PR signal advances open -> in-review, stamped by "reconcile" + the note.
        let moved = reconcile_advance(&st.id, false, true, Some("merged acme/x#9"))
            .await
            .unwrap();
        assert_eq!(moved, Some(SharedTaskState::InReview));
        let got = read_shared_task(&st.id).await.unwrap();
        assert_eq!(got.state, SharedTaskState::InReview);
        assert_eq!(got.timeline.last().unwrap().by, "reconcile");
        assert_eq!(
            got.timeline.last().unwrap().note.as_deref(),
            Some("merged acme/x#9")
        );

        // Idempotent: the same signal again => no change.
        assert_eq!(
            reconcile_advance(&st.id, false, true, None).await.unwrap(),
            None
        );

        // No-override: a human Done is never clobbered by a later reconcile.
        advance_shared_task(&st.id, SharedTaskState::Done, "dac", None)
            .await
            .unwrap();
        assert_eq!(
            reconcile_advance(&st.id, true, true, None).await.unwrap(),
            None
        );
        assert_eq!(
            read_shared_task(&st.id).await.unwrap().state,
            SharedTaskState::Done,
            "reconcile must not regress a human Done"
        );

        let _ = std::fs::remove_dir_all(&home);
    }
}
