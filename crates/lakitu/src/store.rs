//! Reader for the fleet multi-agent store (`~/.claude/lakitu-fleet/`).
//!
//! See `DESIGN.md` for the on-disk contract. This module is read-only —
//! the write side lives in the `lakitu-mcp` MCP. We poll the store on
//! the same cadence as the log tailer (`log.rs`) and emit a fresh
//! [`StoreSnapshot`] on the channel whenever anything changed.
//!
//! Layout recap:
//! ```text
//! ~/.claude/lakitu-fleet/
//!   agents/<name>.json            registry  {name, repo, board, started}
//!   agents/<name>.heartbeat.json  presence  {ts, state, task}
//!   inbox/<name>/<ts>-<id>.json   unread message {id, time, from, title, body}
//!   inbox/<name>/read/<...>.json  read message
//! ```
//!
//! Everything is best-effort: missing dirs read as empty, malformed JSON
//! files are skipped (logged via `tracing`) so one bad file can't take
//! down the pane.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, FixedOffset};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A heartbeat older than this marks the agent *stale* (rendered dimmed),
/// whatever its declared state. Lenient on purpose: Claude Code agents
/// heartbeat when they call a tool, not on a background timer, so a
/// working agent can legitimately go several minutes between beats.
const STALE_AFTER_MINUTES: i64 = 15;

/// Declared agent state from the heartbeat file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Idle,
    Working,
    /// Blocked on the **supervisor** — a decision/answer only they can give.
    /// This is the "needs you" state: it drives the status-bar alert.
    Blocked,
    /// Waiting on a **peer or external event** (another client's release, CI,
    /// a legal sign-off…) — stuck, but NOT the supervisor's call. Kept distinct
    /// from `Blocked` so the cockpit shows where the supervisor's attention is
    /// actually needed vs. what's just waiting on someone else.
    Waiting,
    /// No heartbeat file yet, or an unrecognized `state` string.
    Unknown,
}

impl AgentState {
    pub(crate) fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "idle" => AgentState::Idle,
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            "waiting" => AgentState::Waiting,
            _ => AgentState::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Waiting => "waiting",
            AgentState::Unknown => "unknown",
        }
    }

    pub fn color(self) -> Color {
        match self {
            AgentState::Working => Color::Cyan,
            AgentState::Blocked => Color::Rgb(220, 130, 40),
            AgentState::Waiting => Color::Rgb(210, 170, 60),
            AgentState::Idle => Color::Rgb(140, 140, 140),
            AgentState::Unknown => Color::DarkGray,
        }
    }

    /// Attention-first ordering for the agents pane: blocked-on-supervisor
    /// first (needs a human), then waiting-on-peer (stuck, but not on you),
    /// then active, then idle/unknown at the bottom.
    fn sort_rank(self) -> u8 {
        match self {
            AgentState::Blocked => 0,
            AgentState::Waiting => 1,
            AgentState::Working => 2,
            AgentState::Idle => 3,
            AgentState::Unknown => 4,
        }
    }
}

/// What kind of client a roster entry is — an LM agent or the human
/// supervisor. Humans don't heartbeat; they render as always-present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    Agent,
    Human,
}

impl ClientKind {
    pub(crate) fn parse(s: &str) -> Self {
        if s.eq_ignore_ascii_case("human") {
            ClientKind::Human
        } else {
            ClientKind::Agent
        }
    }
}

/// One attached client — registry fields plus current presence.
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    /// Agent vs human supervisor.
    pub kind: ClientKind,
    pub repo: String,
    pub board: String,
    /// Short function label (e.g. "code review", "scan backend"), distinct
    /// from the name — the name is identity/address, the role is what it does.
    /// From the registry; `None` if the agent registered without one.
    pub role: Option<String>,
    /// Stable, self-authored capability blurb — what this agent is for and
    /// what peers can ask it to do. From the registry, distinct from the
    /// transient `task`. `None` if the agent registered without one.
    pub description: Option<String>,
    pub state: AgentState,
    /// Free-form one-liner of what the agent is doing right now.
    pub task: Option<String>,
    /// Heartbeat timestamp. `None` if no heartbeat file exists yet.
    pub last_seen: Option<DateTime<FixedOffset>>,
    /// True when `last_seen` is older than [`STALE_AFTER_MINUTES`].
    pub stale: bool,
    /// Count of unread messages in this agent's inbox.
    pub unread: usize,
    /// Context-window usage percent (0–100) reported by the agent's statusLine
    /// (`agents/<name>.context.json`). `None` if not reported.
    pub context_pct: Option<u8>,
}

/// One inbox message.
#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub time: Option<DateTime<FixedOffset>>,
    pub from: String,
    pub title: String,
    pub body: String,
    /// True when the message lives under `inbox/<name>/read/`.
    pub read: bool,
}

/// A PR a task hangs off — renders the task as a child of that PR's work-item
/// row. `owner/name` + number.
#[derive(Debug, Clone)]
pub struct TaskPr {
    pub repo: String,
    pub number: u64,
}

/// One agent task — a private, lightweight reminder (distinct from a GitHub
/// issue). Authored by the agent (or the supervisor, from the cockpit) and
/// rendered as a checklist under the agent; a task carrying `pr` nests under
/// that PR's work-item row instead.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: String,
    /// One-line title.
    pub text: String,
    /// Optional longer note (the task's "message"), shown in the detail pane.
    pub body: Option<String>,
    pub done: bool,
    pub created: Option<DateTime<FixedOffset>>,
    pub pr: Option<TaskPr>,
    /// Inbox message id this task was spun off from (provenance), if any.
    pub from_msg: Option<String>,
}

/// One open PR an agent has in flight — recorded at creation by the `open_pr`
/// MCP tool (in `prs/<agent>.json`) and reconciled by the sweep (merged/closed
/// dropped). Shown under its opening agent so the cockpit sees it by
/// construction. Distinct from [`TaskPr`] (a PR a private task hangs off).
#[derive(Debug, Clone)]
pub struct OpenPr {
    pub repo: String,
    pub number: u64,
    pub title: String,
    /// Cached GitHub state for the status pill: open/draft/merged/closed.
    /// `None` until the sweep has observed it.
    pub state: Option<String>,
}

/// Full picture of the store at one poll.
#[derive(Debug, Clone, Default)]
pub struct StoreSnapshot {
    /// Registered agents, attention-first (blocked → working → idle), then
    /// by name.
    pub agents: Vec<Agent>,
    /// Messages per agent name, newest first. Includes read + unread.
    pub inboxes: HashMap<String, Vec<Message>>,
    /// Tasks per agent name, in stored order (oldest first). Open + done.
    pub tasks: HashMap<String, Vec<Task>>,
    /// Open PRs per agent name (recorded at creation by `open_pr`), stored
    /// order. The roster holds only OPEN PRs — the sweep drops merged/closed.
    pub open_prs: HashMap<String, Vec<OpenPr>>,
    /// Supervisor-defined projects (groupings of clients), in declared order.
    pub projects: Vec<Project>,
    /// Account rate-limit usage from the freshest agent's statusLine report
    /// (it's account-global, so any session sees the same numbers). `None`
    /// until some agent reports it.
    pub usage: Option<Usage>,
}

/// Account-level rate-limit usage (percent of the window consumed), reported
/// by Claude Code's statusLine and shared via the context files.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub five_hour_pct: f32,
    pub seven_day_pct: f32,
    /// Unix seconds when this reading was written by a statusLine. The numbers
    /// only move when a session makes an API call, so an idle fleet freezes
    /// them — the cockpit uses this to show the reading's age and grey it once
    /// stale, rather than letting a frozen value look live.
    pub ts: i64,
    /// Unix seconds when the 5-hour / 7-day window next resets (`resets_at` from
    /// the statusLine). `None` if not reported. The cockpit shows the time left.
    pub five_hour_reset: Option<i64>,
    pub seven_day_reset: Option<i64>,
}

#[derive(Deserialize)]
struct ContextFile {
    #[serde(default)]
    pct: Option<f32>,
    #[serde(default)]
    rl5h: Option<f32>,
    #[serde(default)]
    rl7d: Option<f32>,
    #[serde(default)]
    rl5h_reset: Option<i64>,
    #[serde(default)]
    rl7d_reset: Option<i64>,
    #[serde(default)]
    ts: Option<i64>,
}

/// A supervisor-defined project: a freeform grouping of clients, orthogonal to
/// repo/board (it can span repos). Membership lives here — not on the agent
/// registry, which agents rewrite on every heartbeat and don't know their
/// project — so the cockpit owns it cleanly. A client in no project's
/// `members` is "floating" (a cross-project helper, e.g. a reviewer/tester).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    /// Stable slug (path/key-safe). Used for fold state + membership.
    pub id: String,
    /// Display name.
    pub name: String,
    /// A member client designated to lead this project. Display-only for now
    /// (rendered with ★ and sorted first); no behavior attached yet.
    #[serde(default)]
    pub coordinator: Option<String>,
    /// Member client names. A name not registered as an agent is ignored.
    #[serde(default)]
    pub members: Vec<String>,
}

#[derive(Deserialize, Default)]
struct ProjectsFile {
    #[serde(default)]
    projects: Vec<Project>,
}

// ---- on-disk shapes --------------------------------------------------------

#[derive(Deserialize)]
struct RegistryFile {
    name: Option<String>,
    repo: Option<String>,
    board: Option<String>,
    /// Short function label (e.g. "code review"), distinct from the name.
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// "human" for the supervisor; absent/anything else = agent.
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Deserialize)]
struct HeartbeatFile {
    ts: Option<String>,
    state: Option<String>,
    #[serde(default)]
    task: Option<String>,
}

#[derive(Deserialize)]
struct MessageFile {
    #[serde(default)]
    id: String,
    #[serde(default)]
    time: Option<String>,
    #[serde(default)]
    from: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct TaskFile {
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    pr: Option<TaskPrFile>,
    #[serde(default)]
    from_msg: Option<String>,
}

#[derive(Deserialize)]
struct TaskPrFile {
    #[serde(default)]
    repo: String,
    #[serde(default)]
    number: u64,
}

#[derive(Deserialize)]
struct OpenPrFile {
    #[serde(default)]
    repo: String,
    #[serde(default)]
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    state: Option<String>,
}

/// Default store root: `$HOME/.claude/lakitu-fleet`.
pub fn default_store_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".claude").join("lakitu-fleet")
}

/// Where the poller reads the fleet from: the local store directory, or a
/// remote daemon over HTTP (`--server` mode). Cheap to clone.
#[derive(Clone)]
pub enum Source {
    Local(PathBuf),
    Remote(crate::remote::RemoteClient),
}

/// Spawn the store poller. Emits a [`StoreSnapshot`] whenever the store
/// changes (and once on startup). The channel closes when the UI drops
/// its receiver.
pub fn spawn(source: Source) -> mpsc::Receiver<StoreSnapshot> {
    let (tx, rx) = mpsc::channel::<StoreSnapshot>(8);
    tokio::spawn(async move {
        let mut ticker = interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_fp: Option<u64> = None;
        loop {
            ticker.tick().await;
            if tx.is_closed() {
                return;
            }
            let snap = match &source {
                Source::Local(root) => read_snapshot(root).await,
                // On a transient fetch error, skip this tick so the cockpit
                // keeps showing the last good snapshot instead of flashing empty.
                Source::Remote(rc) => match rc.snapshot().await {
                    Some(s) => s,
                    None => continue,
                },
            };
            let fp = fingerprint(&snap);
            if last_fp != Some(fp) {
                last_fp = Some(fp);
                if tx.send(snap).await.is_err() {
                    return;
                }
            }
        }
    });
    rx
}

/// Read the whole store once. Never errors — a missing or unreadable
/// store yields an empty snapshot.
pub async fn read_snapshot(root: &Path) -> StoreSnapshot {
    let agents_dir = root.join("agents");
    let inbox_root = root.join("inbox");

    // 1. Registry + heartbeat → agents.
    let mut agents: Vec<Agent> = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&agents_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            // Registry files only: `<name>.json` — never the `<name>.heartbeat.json`
            // presence file nor the `<name>.wake.json` wake-config.
            if !fname.ends_with(".json")
                || fname.ends_with(".heartbeat.json")
                || fname.ends_with(".wake.json")
                || fname.ends_with(".context.json")
            {
                continue;
            }
            let stem = fname.trim_end_matches(".json").to_string();
            match read_agent(&agents_dir, &stem).await {
                Some(agent) => agents.push(agent),
                None => tracing::debug!(agent = %stem, "skipping unreadable registry file"),
            }
        }
    }

    // 2. Inboxes — one dir per agent under inbox/.
    let mut inboxes: HashMap<String, Vec<Message>> = HashMap::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&inbox_root).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if !entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            let msgs = read_inbox(&entry.path()).await;
            inboxes.insert(name, msgs);
        }
    }

    // 3. Fold unread counts back onto the agents.
    for agent in &mut agents {
        agent.unread = inboxes
            .get(&agent.name)
            .map(|m| m.iter().filter(|m| !m.read).count())
            .unwrap_or(0);
    }

    // 4. Attention-first ordering: stale sinks, then by state, then name.
    agents.sort_by(|a, b| {
        // The human (you) sits at the top of your own board; then agents,
        // stale last, attention-first within that.
        (b.kind == ClientKind::Human)
            .cmp(&(a.kind == ClientKind::Human))
            .then_with(|| a.stale.cmp(&b.stale))
            .then_with(|| a.state.sort_rank().cmp(&b.state.sort_rank()))
            .then_with(|| a.name.cmp(&b.name))
    });

    // 5. Projects (supervisor-defined groupings). Missing file ⇒ none.
    let projects = read_projects(root).await;

    // 5b. Tasks — one `tasks/<name>.json` array per agent. Missing dir ⇒ none.
    let tasks = read_tasks_dir(root).await;

    // 5c. Open PRs — one `prs/<name>.json` array per agent, recorded at
    //     creation by the `open_pr` tool. Missing dir ⇒ none.
    let open_prs = read_open_prs_dir(root).await;

    // 6. Context/usage reports (per-agent `<name>.context.json`, written by the
    //    statusLine). Fold context% onto agents; take rate-limit usage from the
    //    freshest report (it's account-global).
    let mut usage: Option<(i64, Usage)> = None;
    for agent in &mut agents {
        if let Some(cf) = read_context_file(&agents_dir, &agent.name).await {
            agent.context_pct = cf.pct.map(|p| p.round().clamp(0.0, 100.0) as u8);
            if let (Some(ts), Some(a), Some(b)) = (cf.ts, cf.rl5h, cf.rl7d) {
                if usage.map(|(t, _)| ts > t).unwrap_or(true) {
                    usage = Some((
                        ts,
                        Usage {
                            five_hour_pct: a,
                            seven_day_pct: b,
                            ts,
                            five_hour_reset: cf.rl5h_reset,
                            seven_day_reset: cf.rl7d_reset,
                        },
                    ));
                }
            }
        }
    }

    StoreSnapshot {
        agents,
        inboxes,
        tasks,
        open_prs,
        projects,
        usage: usage.map(|(_, u)| u),
    }
}

/// Read every `tasks/<name>.json` → tasks keyed by agent name. Missing dir,
/// unreadable or malformed files ⇒ that agent simply has no tasks (best-effort,
/// like the inbox/project readers).
async fn read_tasks_dir(root: &Path) -> HashMap<String, Vec<Task>> {
    let mut out: HashMap<String, Vec<Task>> = HashMap::new();
    let Ok(mut entries) = tokio::fs::read_dir(root.join("tasks")).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if !entry
            .file_type()
            .await
            .map(|t| t.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        match serde_json::from_str::<Vec<TaskFile>>(&raw) {
            Ok(files) => {
                out.insert(stem, files.into_iter().map(task_from_file).collect());
            }
            Err(err) => {
                tracing::debug!(?err, path = %path.display(), "skipping malformed tasks file")
            }
        }
    }
    out
}

fn task_from_file(f: TaskFile) -> Task {
    Task {
        id: f.id,
        text: f.text,
        body: f.body.filter(|b| !b.trim().is_empty()),
        done: f.done,
        created: f
            .created
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok()),
        pr: f.pr.filter(|p| !p.repo.trim().is_empty()).map(|p| TaskPr {
            repo: p.repo,
            number: p.number,
        }),
        from_msg: f.from_msg.filter(|m| !m.trim().is_empty()),
    }
}

/// Read every `prs/<name>.json` → open PRs keyed by agent name. Missing dir,
/// unreadable or malformed files ⇒ that agent simply has no open PRs
/// (best-effort, like the task reader). Drops entries with a blank repo.
async fn read_open_prs_dir(root: &Path) -> HashMap<String, Vec<OpenPr>> {
    let mut out: HashMap<String, Vec<OpenPr>> = HashMap::new();
    let Ok(mut entries) = tokio::fs::read_dir(root.join("prs")).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if !entry
            .file_type()
            .await
            .map(|t| t.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        match serde_json::from_str::<Vec<OpenPrFile>>(&raw) {
            Ok(files) => {
                let prs: Vec<OpenPr> = files
                    .into_iter()
                    .filter(|p| !p.repo.trim().is_empty())
                    .map(|p| OpenPr {
                        repo: p.repo,
                        number: p.number,
                        title: p.title,
                        state: p.state.filter(|s| !s.trim().is_empty()),
                    })
                    .collect();
                out.insert(stem, prs);
            }
            Err(err) => {
                tracing::debug!(?err, path = %path.display(), "skipping malformed prs file")
            }
        }
    }
    out
}

/// The display/selection order for an agent's tasks: PR-linked tasks grouped by
/// first-seen PR, then loose (no-PR) tasks. Returns indices into `tasks`. Shared
/// by the tasks-modal renderer (`ui`) and its key handler (`app`) so the cursor
/// lines up with what's drawn.
pub fn task_display_order(tasks: &[Task]) -> Vec<usize> {
    let mut pr_keys: Vec<(&str, u64)> = Vec::new();
    for t in tasks {
        if let Some(p) = &t.pr {
            let k = (p.repo.as_str(), p.number);
            if !pr_keys.contains(&k) {
                pr_keys.push(k);
            }
        }
    }
    let mut order = Vec::with_capacity(tasks.len());
    for (repo, num) in &pr_keys {
        for (i, t) in tasks.iter().enumerate() {
            if t.pr
                .as_ref()
                .map(|p| p.repo == *repo && p.number == *num)
                .unwrap_or(false)
            {
                order.push(i);
            }
        }
    }
    for (i, t) in tasks.iter().enumerate() {
        if t.pr.is_none() {
            order.push(i);
        }
    }
    order
}

/// Read one agent's `<name>.context.json` (written by the statusLine). Missing
/// or malformed ⇒ `None`.
async fn read_context_file(agents_dir: &Path, name: &str) -> Option<ContextFile> {
    let raw = tokio::fs::read_to_string(agents_dir.join(format!("{name}.context.json")))
        .await
        .ok()?;
    serde_json::from_str::<ContextFile>(&raw).ok()
}

/// Read `projects.json` (`{ "projects": [...] }`). Missing or malformed ⇒ no
/// projects (the fleet just renders flat, as before).
async fn read_projects(root: &Path) -> Vec<Project> {
    let Ok(raw) = tokio::fs::read_to_string(root.join("projects.json")).await else {
        return Vec::new();
    };
    serde_json::from_str::<ProjectsFile>(&raw)
        .map(|f| f.projects)
        .unwrap_or_default()
}

/// Read one agent's registry + heartbeat. Returns `None` if the registry
/// file is missing/malformed (an agent with no registry isn't shown).
async fn read_agent(agents_dir: &Path, name: &str) -> Option<Agent> {
    let reg_path = agents_dir.join(format!("{name}.json"));
    let reg_raw = tokio::fs::read_to_string(&reg_path).await.ok()?;
    let reg: RegistryFile = serde_json::from_str(&reg_raw).ok()?;
    // The directory stem is authoritative for the name; the file's `name`
    // field is a convenience that should match but we don't depend on it.
    let display_name = reg.name.unwrap_or_else(|| name.to_string());
    let kind = reg
        .kind
        .as_deref()
        .map(ClientKind::parse)
        .unwrap_or(ClientKind::Agent);

    let mut state = AgentState::Unknown;
    let mut task = None;
    let mut last_seen = None;
    let hb_path = agents_dir.join(format!("{name}.heartbeat.json"));
    if let Ok(raw) = tokio::fs::read_to_string(&hb_path).await {
        if let Ok(hb) = serde_json::from_str::<HeartbeatFile>(&raw) {
            state = hb
                .state
                .as_deref()
                .map(AgentState::parse)
                .unwrap_or(AgentState::Unknown);
            task = hb.task.filter(|t| !t.trim().is_empty());
            last_seen = hb
                .ts
                .as_deref()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok());
        }
    }

    let stale = match kind {
        // The human supervisor is always present — they don't heartbeat.
        ClientKind::Human => false,
        ClientKind::Agent => match last_seen {
            Some(ts) => {
                let mins = chrono::Local::now()
                    .fixed_offset()
                    .signed_duration_since(ts)
                    .num_minutes();
                mins > STALE_AFTER_MINUTES
            }
            // No heartbeat at all → treat as stale so it reads as "not live".
            None => true,
        },
    };

    Some(Agent {
        name: display_name,
        kind,
        repo: reg.repo.unwrap_or_default(),
        board: reg.board.unwrap_or_default(),
        role: reg.role.filter(|r| !r.trim().is_empty()),
        description: reg.description.filter(|d| !d.trim().is_empty()),
        state,
        task,
        last_seen,
        stale,
        unread: 0,
        context_pct: None,
    })
}

/// Read all messages in one inbox dir — top-level files (unread) plus
/// anything under `read/`. Newest first.
async fn read_inbox(dir: &Path) -> Vec<Message> {
    let mut msgs = Vec::new();
    read_message_dir(dir, false, &mut msgs).await;
    read_message_dir(&dir.join("read"), true, &mut msgs).await;
    // Newest first; messages with no parseable time sort last.
    msgs.sort_by_key(|m| std::cmp::Reverse(m.time));
    msgs
}

async fn read_message_dir(dir: &Path, read: bool, out: &mut Vec<Message>) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        match serde_json::from_str::<MessageFile>(&raw) {
            Ok(m) => out.push(Message {
                id: m.id,
                time: m
                    .time
                    .as_deref()
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok()),
                from: m.from,
                title: m.title,
                body: m.body,
                read,
            }),
            Err(err) => tracing::debug!(?err, path = %path.display(), "skipping malformed message"),
        }
    }
}

/// Cheap change-detector over the salient fields, so we only push a new
/// snapshot (and trigger a redraw) when something actually moved.
fn fingerprint(snap: &StoreSnapshot) -> u64 {
    let mut h = DefaultHasher::new();
    for a in &snap.agents {
        a.name.hash(&mut h);
        (a.kind == ClientKind::Human).hash(&mut h);
        a.repo.hash(&mut h);
        a.board.hash(&mut h);
        a.role.hash(&mut h);
        a.description.hash(&mut h);
        a.state.label().hash(&mut h);
        a.stale.hash(&mut h);
        a.task.hash(&mut h);
        a.last_seen.map(|t| t.timestamp()).hash(&mut h);
        a.unread.hash(&mut h);
        a.context_pct.hash(&mut h);
    }
    // Inbox identity: per agent, the set of message ids and their read flag.
    let mut names: Vec<&String> = snap.inboxes.keys().collect();
    names.sort();
    for name in names {
        name.hash(&mut h);
        for m in &snap.inboxes[name] {
            m.id.hash(&mut h);
            m.read.hash(&mut h);
        }
    }
    // Task identity: per agent, each task's id + done + text + PR link, so an
    // add / complete / drop / edit re-renders the agents pane.
    let mut tnames: Vec<&String> = snap.tasks.keys().collect();
    tnames.sort();
    for name in tnames {
        name.hash(&mut h);
        for t in &snap.tasks[name] {
            t.id.hash(&mut h);
            t.done.hash(&mut h);
            t.text.hash(&mut h);
            if let Some(pr) = &t.pr {
                pr.repo.hash(&mut h);
                pr.number.hash(&mut h);
            }
        }
    }
    // Open-PR identity: per agent, each PR's {repo, number} + cached state, so
    // a record / reconcile-drop / state-change re-renders the agents pane.
    let mut pnames: Vec<&String> = snap.open_prs.keys().collect();
    pnames.sort();
    for name in pnames {
        name.hash(&mut h);
        for pr in &snap.open_prs[name] {
            pr.repo.hash(&mut h);
            pr.number.hash(&mut h);
            pr.state.hash(&mut h);
        }
    }
    // Project identity: order, name, coordinator, membership.
    for p in &snap.projects {
        p.id.hash(&mut h);
        p.name.hash(&mut h);
        p.coordinator.hash(&mut h);
        for m in &p.members {
            m.hash(&mut h);
        }
    }
    if let Some(u) = &snap.usage {
        (u.five_hour_pct as i32).hash(&mut h);
        (u.seven_day_pct as i32).hash(&mut h);
        // Re-emit when a fresh reading lands (even at the same %), so the
        // cockpit's age display resets instead of drifting into "stale".
        u.ts.hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Build a throwaway store under the temp dir, unique per test name.
    fn scratch(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("fleet-test-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("agents")).unwrap();
        root
    }

    fn write(path: PathBuf, contents: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn reads_registry_heartbeat_and_inbox() {
        let root = scratch("basic");
        let now = chrono::Local::now().to_rfc3339();

        // alice: live + working, one unread + one read message.
        write(
            root.join("agents/alice.json"),
            r#"{"name":"alice","repo":"acme/web","board":"acme/14","started":"2026-05-29T10:00:00+02:00"}"#,
        );
        write(
            root.join("agents/alice.heartbeat.json"),
            &format!(r#"{{"ts":"{now}","state":"working","task":"issue #90"}}"#),
        );
        write(
            root.join("inbox/alice/20260529T100600-aaa.json"),
            r#"{"id":"aaa","time":"2026-05-29T10:06:00+02:00","from":"bob","title":"hi","body":"need a thing"}"#,
        );
        write(
            root.join("inbox/alice/read/20260529T090000-old.json"),
            r#"{"id":"old","time":"2026-05-29T09:00:00+02:00","from":"bob","title":"older","body":"done"}"#,
        );

        // bob: registered but never sent a heartbeat → stale + unknown.
        write(
            root.join("agents/bob.json"),
            r#"{"name":"bob","repo":"acme/api","board":"acme/14"}"#,
        );

        let snap = read_snapshot(&root).await;

        assert_eq!(snap.agents.len(), 2);
        // Working agent sorts before the stale/unknown one.
        let alice = &snap.agents[0];
        assert_eq!(alice.name, "alice");
        assert_eq!(alice.state, AgentState::Working);
        assert!(!alice.stale);
        assert_eq!(alice.unread, 1, "one top-level message is unread");
        assert_eq!(alice.task.as_deref(), Some("issue #90"));

        let bob = &snap.agents[1];
        assert_eq!(bob.name, "bob");
        assert_eq!(bob.state, AgentState::Unknown);
        assert!(bob.stale, "no heartbeat ⇒ stale");

        // Inbox: two messages, newest first, read flag honored.
        let inbox = &snap.inboxes["alice"];
        assert_eq!(inbox.len(), 2);
        assert_eq!(inbox[0].id, "aaa");
        assert!(!inbox[0].read);
        assert!(inbox[1].read);

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn human_client_is_read_and_never_stale() {
        let root = scratch("human");
        // A human registry with no heartbeat — must still read as live.
        write(
            root.join("agents/you.json"),
            r#"{"name":"you","kind":"human","repo":"-","board":"-","description":"Supervisor"}"#,
        );
        let snap = read_snapshot(&root).await;
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.agents[0].kind, ClientKind::Human);
        assert!(!snap.agents[0].stale, "humans don't go stale");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn missing_store_is_empty_not_an_error() {
        let root = std::env::temp_dir().join(format!("fleet-test-{}-absent", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let snap = read_snapshot(&root).await;
        assert!(snap.agents.is_empty());
        assert!(snap.inboxes.is_empty());
    }

    #[tokio::test]
    async fn stale_when_heartbeat_old() {
        let root = scratch("stale");
        let old = (chrono::Local::now() - chrono::Duration::minutes(STALE_AFTER_MINUTES + 5))
            .to_rfc3339();
        write(
            root.join("agents/slow.json"),
            r#"{"name":"slow","repo":"r","board":"b"}"#,
        );
        write(
            root.join("agents/slow.heartbeat.json"),
            &format!(r#"{{"ts":"{old}","state":"working"}}"#),
        );
        let snap = read_snapshot(&root).await;
        assert_eq!(snap.agents.len(), 1);
        assert!(
            snap.agents[0].stale,
            "heartbeat older than the window ⇒ stale"
        );
        assert_eq!(
            snap.agents[0].state,
            AgentState::Working,
            "declared state preserved"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn reads_tasks_and_orders_pr_first() {
        let root = scratch("tasks");
        write(
            root.join("agents/aria.json"),
            r#"{"name":"aria","repo":"acme/lakitu","board":"-"}"#,
        );
        write(
            root.join("tasks/aria.json"),
            r#"[
              {"id":"t1","text":"loose one","done":false,"created":"2026-06-01T10:00:00+02:00"},
              {"id":"t2","text":"on pr 12","done":false,"created":"2026-06-01T10:01:00+02:00","pr":{"repo":"acme/lakitu","number":12},"from_msg":"m9"},
              {"id":"t3","text":"done one","done":true,"created":"2026-06-01T10:02:00+02:00"}
            ]"#,
        );

        let snap = read_snapshot(&root).await;
        let tasks = snap
            .tasks
            .get("aria")
            .expect("aria has tasks in the snapshot");
        assert_eq!(tasks.len(), 3);
        let t2 = tasks.iter().find(|t| t.id == "t2").unwrap();
        assert_eq!(t2.pr.as_ref().unwrap().number, 12);
        assert_eq!(t2.from_msg.as_deref(), Some("m9"));

        // Display order: PR-linked first (t2), then loose tasks in list order.
        let order = task_display_order(tasks);
        let ids: Vec<&str> = order.iter().map(|&i| tasks[i].id.as_str()).collect();
        assert_eq!(ids, vec!["t2", "t1", "t3"]);

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn reads_open_prs_per_agent() {
        let root = scratch("open-prs");
        write(
            root.join("agents/aria.json"),
            r#"{"name":"aria","repo":"acme/lakitu","board":"-"}"#,
        );
        // The `prs/<name>.json` roster written by the `open_pr` tool: state is
        // optional (unset until reconciled) and a blank-repo entry is dropped.
        write(
            root.join("prs/aria.json"),
            r#"[
              {"repo":"acme/lakitu","number":12,"title":"wrapped-gh P1","url":"https://x/pull/12","state":"open","created":"2026-06-22T10:00:00+02:00"},
              {"repo":"acme/lakitu","number":14,"title":"draft work","url":"https://x/pull/14","state":"draft","created":"2026-06-22T10:01:00+02:00"},
              {"repo":"acme/lakitu","number":22,"title":"no state yet","url":"https://x/pull/22","created":"2026-06-22T10:02:00+02:00"},
              {"repo":"","number":99,"title":"blank repo","url":"u","created":"2026-06-22T10:03:00+02:00"}
            ]"#,
        );

        let snap = read_snapshot(&root).await;
        let prs = snap
            .open_prs
            .get("aria")
            .expect("aria has open PRs in the snapshot");
        assert_eq!(prs.len(), 3, "blank-repo entry dropped");
        assert_eq!(prs[0].number, 12);
        assert_eq!(prs[0].state.as_deref(), Some("open"));
        assert_eq!(prs[1].state.as_deref(), Some("draft"));
        assert_eq!(prs[2].state, None, "unobserved state stays None");

        // The fingerprint covers open PRs, so a roster change re-renders.
        let fp_before = fingerprint(&snap);
        write(
            root.join("prs/aria.json"),
            r#"[{"repo":"acme/lakitu","number":12,"title":"wrapped-gh P1","url":"https://x/pull/12","state":"merged","created":"2026-06-22T10:00:00+02:00"}]"#,
        );
        let snap2 = read_snapshot(&root).await;
        assert_ne!(
            fp_before,
            fingerprint(&snap2),
            "an open-PR state change changes the fingerprint"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
