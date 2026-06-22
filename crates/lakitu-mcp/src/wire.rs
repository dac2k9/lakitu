//! The HTTP wire contract between the daemon and the remote cockpit.
//!
//! These DTOs mirror the cockpit's `store::{StoreSnapshot, Agent, Message,
//! Project, Usage}` (in the `lakitu` repo). The daemon serializes them; the
//! cockpit deserializes and converts into its own types. Keep the two in sync:
//! enums travel as lowercase strings (`state`, `kind`), timestamps as RFC3339
//! strings — exactly what the cockpit already parses from the on-disk store, so
//! its `AgentState::parse` / `DateTime::parse_from_rfc3339` work unchanged.

use std::collections::BTreeMap;

use serde::Serialize;

#[derive(Serialize, Default)]
pub struct SnapshotDto {
    pub agents: Vec<AgentDto>,
    /// Per-agent inbox, keyed by name; newest message first. Read + unread.
    pub inboxes: BTreeMap<String, Vec<MessageDto>>,
    /// Per-agent task list, keyed by name; declared order (oldest first). Open
    /// + done — the cockpit filters/folds.
    pub tasks: BTreeMap<String, Vec<TaskDto>>,
    /// Team/fleet-scoped shared tasks — goals grouping issues + PRs across
    /// agents, with participants and a Start→Goal timeline. Rendered by the web
    /// `/tasks` view; the TUI ignores them for now (unknown field → dropped).
    pub shared_tasks: Vec<SharedTaskDto>,
    pub projects: Vec<ProjectDto>,
    pub usage: Option<UsageDto>,
}

#[derive(Serialize)]
pub struct AgentDto {
    pub name: String,
    /// "agent" | "human".
    pub kind: String,
    pub repo: String,
    pub board: String,
    pub role: Option<String>,
    pub description: Option<String>,
    /// "idle" | "working" | "blocked" | "waiting" | "unknown".
    pub state: String,
    pub task: Option<String>,
    /// Heartbeat timestamp, RFC3339. `None` if the agent never heartbeat.
    pub last_seen: Option<String>,
    pub stale: bool,
    pub unread: u32,
    pub context_pct: Option<u8>,
}

#[derive(Serialize)]
pub struct MessageDto {
    pub id: String,
    /// RFC3339; `None` if unparseable/absent.
    pub time: Option<String>,
    pub from: String,
    pub title: String,
    pub body: String,
    /// True when the message lives under `inbox/<name>/read/`.
    pub read: bool,
}

#[derive(Serialize)]
pub struct ProjectDto {
    pub id: String,
    pub name: String,
    pub coordinator: Option<String>,
    pub members: Vec<String>,
}

/// One agent task — a private, lightweight reminder (distinct from a GitHub
/// issue, which is the durable/shared unit of work). The cockpit renders these
/// as a checklist under the agent; a task carrying `pr` nests under that PR's
/// work-item row instead.
#[derive(Serialize)]
pub struct TaskDto {
    pub id: String,
    pub text: String,
    /// Optional longer note (the task's "message").
    pub body: Option<String>,
    pub done: bool,
    /// RFC3339 creation time.
    pub created: String,
    /// PR this task hangs off, if any (renders as a subtree of that PR).
    pub pr: Option<TaskPrDto>,
    /// Message id this task was spun off from (provenance), if any.
    pub from_msg: Option<String>,
}

#[derive(Serialize)]
pub struct TaskPrDto {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
}

/// A team/fleet-scoped shared task: a goal grouping board issues + PRs across
/// agents, with participants and an append-only Start→Goal timeline. Distinct
/// from [`TaskDto`] (a private, per-agent reminder).
#[derive(Serialize)]
pub struct SharedTaskDto {
    pub id: String,
    pub title: String,
    pub goal: Option<String>,
    /// "team" | "fleet".
    pub scope: String,
    /// For team scope: the board, `owner/projectNumber`.
    pub team: Option<String>,
    pub owner: String,
    pub participants: Vec<String>,
    pub issues: Vec<TaskRefDto>,
    pub prs: Vec<TaskRefDto>,
    /// "open" | "active" | "blocked" | "in-review" | "done".
    pub state: String,
    pub timeline: Vec<TaskEventDto>,
    /// RFC3339.
    pub created: String,
    /// RFC3339.
    pub updated: String,
}

/// A board issue or PR linked to a [`SharedTaskDto`].
#[derive(Serialize)]
pub struct TaskRefDto {
    /// `owner/name`.
    pub repo: String,
    pub number: u64,
}

/// One transition in a [`SharedTaskDto`]'s timeline.
#[derive(Serialize)]
pub struct TaskEventDto {
    /// State moved to: "open" | "active" | "blocked" | "in-review" | "done".
    pub state: String,
    /// RFC3339.
    pub ts: String,
    /// Agent (or "reconcile") who made the move.
    pub by: String,
}

#[derive(Serialize)]
pub struct UsageDto {
    pub five_hour_pct: f32,
    pub seven_day_pct: f32,
    pub ts: i64,
    /// Unix seconds when each rolling window next resets (`resets_at`). `None`
    /// if not reported.
    pub five_hour_reset: Option<i64>,
    pub seven_day_reset: Option<i64>,
}
