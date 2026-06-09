//! App state + event loop.
//!
//! Three input streams feed the UI:
//!   1. Crossterm events (keyboard + mouse) via `EventStream`.
//!   2. Log lines arriving on the `mpsc::Receiver<Event>` from `log::spawn`.
//!   3. `gh`-CLI enrichment results (issue/PR title + lifecycle state)
//!      arriving on the `mpsc::Receiver<MetaUpdate>`.
//!   4. A periodic tick (every 80 ms) so the spinner animates.
//!
//! `tokio::select!` multiplexes them. Mutating events trigger a redraw
//! on the next loop iteration.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use color_eyre::Result;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::client;
use crate::event::{Event, RefKind};
use crate::gh::{self, Meta, MetaUpdate};
use crate::log;
use crate::remote::{self, WriteCmd};
use crate::store::{self, Agent, Message, Project, StoreSnapshot, Task, Usage};
use crate::ui;
use crate::work::{WorkItems, WorkKey, WorkState};

const TICK_MS: u64 = 80;
const SKILL_FILTERS: &[Option<&str>] = &[None, Some("board-issue-loop"), Some("pr-review-fixup")];

/// Which pane has keyboard focus. Default is the event stream (today's
/// behavior); `DoneReview` activates when the user presses `d` and
/// shifts j/k/enter to operate on the Done items at the bottom of the
/// work-items pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusMode {
    Events,
    DoneReview,
    /// Keyboard drives the Clients pane: j/k select a client, Enter opens
    /// its inbox, esc returns focus to the event stream. Entered with `a`.
    Clients,
}

/// Which field the compose modal's cursor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeField {
    Recipient,
    Title,
    Body,
}

impl ComposeField {
    fn next(self) -> Self {
        match self {
            ComposeField::Recipient => ComposeField::Title,
            ComposeField::Title => ComposeField::Body,
            ComposeField::Body => ComposeField::Recipient,
        }
    }
    fn prev(self) -> Self {
        match self {
            ComposeField::Recipient => ComposeField::Body,
            ComposeField::Title => ComposeField::Recipient,
            ComposeField::Body => ComposeField::Title,
        }
    }
}

/// Who a composed message is addressed to.
#[derive(Debug, Clone)]
pub enum ComposeTarget {
    /// Broadcast — fan out to every other client's inbox.
    Everyone,
    /// A whole team/project. Delivered to the coordinator alone if it has one
    /// (they triage for the team), otherwise to every member.
    Team {
        id: String,
        name: String,
    },
    Client(String),
}

impl ComposeTarget {
    pub fn label(&self) -> String {
        match self {
            ComposeTarget::Everyone => "everyone (broadcast)".to_string(),
            ComposeTarget::Team { name, .. } => format!("team: {name}"),
            ComposeTarget::Client(n) => n.clone(),
        }
    }
}

/// State of the in-cockpit compose modal (you → a client, or → everyone).
#[derive(Debug, Clone)]
pub struct Compose {
    pub targets: Vec<ComposeTarget>,
    pub target_idx: usize,
    pub title: String,
    pub body: String,
    /// Caret position (char index) within `title` / `body`, so you can move
    /// back and edit mid-text instead of only appending at the end.
    pub title_cursor: usize,
    pub body_cursor: usize,
    pub field: ComposeField,
    pub error: Option<String>,
    /// True once a send is staged (Enter from the body): the modal shows a
    /// confirm prompt and waits for an explicit confirm before dispatching.
    pub confirming: bool,
}

impl Compose {
    /// The focused text field and its caret, or `None` on the recipient
    /// (which isn't free-text — arrows cycle it).
    fn active_field_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.field {
            ComposeField::Title => Some((&mut self.title, &mut self.title_cursor)),
            ComposeField::Body => Some((&mut self.body, &mut self.body_cursor)),
            ComposeField::Recipient => None,
        }
    }
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Byte offset of char index `i` (clamped to the string end).
fn byte_at(s: &str, i: usize) -> usize {
    s.char_indices().nth(i).map(|(b, _)| b).unwrap_or(s.len())
}

/// Insert `ch` at the caret (char index) and advance the caret.
fn insert_at(s: &mut String, cursor: &mut usize, ch: char) {
    let i = (*cursor).min(char_len(s));
    let b = byte_at(s, i);
    s.insert(b, ch);
    *cursor = i + 1;
}

/// Delete the char before the caret (Backspace).
fn backspace_at(s: &mut String, cursor: &mut usize) {
    let i = (*cursor).min(char_len(s));
    if i == 0 {
        return;
    }
    s.replace_range(byte_at(s, i - 1)..byte_at(s, i), "");
    *cursor = i - 1;
}

/// Delete the char at the caret (Delete); caret stays put.
fn delete_at(s: &mut String, cursor: &mut usize) {
    let n = char_len(s);
    let i = (*cursor).min(n);
    if i >= n {
        return;
    }
    s.replace_range(byte_at(s, i)..byte_at(s, i + 1), "");
}

/// Move the caret one line up/down within a multi-line string, keeping the
/// column where possible. Returns the new char index (unchanged at the edge).
fn move_line(s: &str, cursor: usize, down: bool) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let cur = cursor.min(chars.len());
    let mut line_start = 0;
    for (i, &c) in chars.iter().enumerate().take(cur) {
        if c == '\n' {
            line_start = i + 1;
        }
    }
    let col = cur - line_start;
    if down {
        let Some(nl) = (cur..chars.len()).find(|&i| chars[i] == '\n') else {
            return cur; // last line
        };
        let nls = nl + 1;
        let nle = (nls..chars.len())
            .find(|&i| chars[i] == '\n')
            .unwrap_or(chars.len());
        nls + col.min(nle - nls)
    } else {
        if line_start == 0 {
            return cur; // first line
        }
        let prev_end = line_start - 1;
        let prev_start = (0..prev_end)
            .rev()
            .find(|&i| chars[i] == '\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        prev_start + col.min(prev_end - prev_start)
    }
}

/// A selectable row in the Clients tree: a client header, or one of its
/// work-items. The renderer rebuilds this list each frame (it knows the
/// visible, unfolded order) and stores it on `App`, so the key handlers can
/// act on the selected row — open an inbox, or open a ticket's PR/issue —
/// without recomputing the tree.
#[derive(Debug, Clone)]
pub enum TreeRow {
    /// A project header — index into `projects`. Space folds it; `X` removes
    /// it (members float). Not tied to a client.
    Project(usize),
    /// A client header — index into `roster`. Enter opens its inbox.
    Client(usize),
    /// A work-item under a client. Enter opens the PR (or the issue if there's
    /// no PR); Shift+Enter opens the issue; `x` dismisses it from the board.
    Item {
        repo: String,
        pr: Option<u64>,
        issue: Option<u64>,
    },
    /// A task under a client (or under you). Enter opens the tasks modal at it;
    /// `c` composes a message about it (to that client, or pick a recipient for
    /// your own). `owner` is whose list it's on; `text` is its title.
    Task {
        owner: String,
        id: String,
        text: String,
    },
}

pub struct App {
    pub events: Vec<Event>,
    pub work: WorkItems,
    pub selected: usize,
    /// True when the event-stream selection should snap to the newest
    /// event (index 0 in the reversed display) as new entries arrive.
    /// Cleared by an explicit scroll-down; re-set when the user scrolls
    /// back to the top.
    pub follow_newest: bool,
    pub skill_filter: Option<String>,
    pub skill_filter_idx: usize,
    pub show_help: bool,
    /// Whether the event-stream pane is shown. Hidden by default — the
    /// tree + work-items panes carry the picture now; the raw event log
    /// is detail you opt into with `l`. The status bar's live event
    /// count stays visible either way.
    pub show_log: bool,
    /// Whether the standalone by-state Work-items pane is shown. Hidden for
    /// now — each client's work-items live in its tree box, so the separate
    /// pane is redundant. Gates the pane, the `d` Done-review, and its hint.
    pub show_work_pane: bool,
    /// Whether *agent* clients' tasks show in the pane (badge + checklist).
    /// Toggled with `Tab` to declutter; your own (supervisor) tasks always show.
    pub show_client_tasks: bool,
    pub tick: usize,
    /// Rebuilt every frame by `ui::render` so the mouse handler knows
    /// where each `#N` token landed.
    pub click_targets: Vec<ClickTarget>,
    /// Current (row, column) of the mouse cursor in terminal cells.
    /// Updated on `MouseEventKind::Moved`. None before the first move
    /// or after the cursor leaves the terminal. Used to highlight
    /// the clickable token under the mouse and surface its URL in the
    /// status bar.
    pub hover_pos: Option<(u16, u16)>,
    /// Issue/PR metadata fetched from `gh`. Keyed by `(kind, number)`.
    /// Missing entry = not yet fetched. Some(default Meta) = fetch
    /// completed but returned nothing (e.g. gh missing).
    pub titles: HashMap<(RefKind, u64), Option<Meta>>,
    fetch_requested: HashSet<(RefKind, u64)>,
    /// GitHub repo existence cache ("owner/name" → resolves?), filled by
    /// background `gh` checks; gates whether an agent's role links to its repo.
    pub repo_exists: Arc<Mutex<HashMap<String, bool>>>,
    /// Repos a check has already been spawned for (dedup; main-thread only).
    repo_checks: HashSet<String>,
    meta_tx: mpsc::Sender<MetaUpdate>,
    /// Which pane handles keyboard input. See `FocusMode`.
    pub focus_mode: FocusMode,
    /// Selection index into the list of visible Done items when
    /// `focus_mode == DoneReview`. Reset to 0 each time the user
    /// re-enters DoneReview mode.
    pub done_selected: usize,
    /// Work-items the supervisor has dismissed from the board — via `x` on any
    /// item, or `c` in the story modal. Keyed by `WorkKey` so an item of any
    /// kind (issue- or PR-keyed) and any state can be hidden. Populated by
    /// ingesting `card-acknowledged` events from the audit log, so dismissals
    /// survive restarts and propagate to sibling TUIs. Items in this set are
    /// hidden from `visible_work_items`.
    pub acknowledged: HashSet<WorkKey>,
    /// When `Some(issue_number)`, the story modal is open for that
    /// issue. Enter from DoneReview sets this; `c` (close+ack) or `esc`
    /// (close only) clears it.
    pub show_story_for: Option<u64>,
    /// Attached agents, freshest snapshot from the fleet store poller.
    /// Ordered attention-first (see `store::read_snapshot`).
    pub roster: Vec<Agent>,
    /// Messages per agent name (newest first), read + unread.
    pub inboxes: HashMap<String, Vec<Message>>,
    /// Tasks per agent name (stored order, oldest first), open + done. Rendered
    /// as a count badge + checklist in the agents pane and in the tasks modal.
    pub tasks: HashMap<String, Vec<Task>>,
    /// When `Some(name)`, the tasks modal is open for that client (`T` in the
    /// Clients pane). Cleared by `esc`.
    pub show_tasks_for: Option<String>,
    /// Selected task row within the open tasks modal.
    pub tasks_selected: usize,
    /// When `Some(buffer)`, the add-task input line is active inside the tasks
    /// modal and `buffer` is what's been typed. `None` = browsing the list.
    pub task_input: Option<String>,
    /// Selection index into `tree_rows` when `focus_mode == Clients` — walks
    /// clients *and* their work-items, not just clients.
    pub tree_selected: usize,
    /// Flattened selectable rows of the Clients tree (clients + their visible
    /// work-items), rebuilt by `ui::render_agents_pane` each frame. The key
    /// handlers index into this with `tree_selected`.
    pub tree_rows: Vec<TreeRow>,
    /// When `Some(name)`, the inbox modal is open for that client. Set by
    /// Enter in Clients focus; cleared by `esc`.
    pub show_inbox_for: Option<String>,
    /// Selected message index within the open inbox modal (master/detail).
    pub inbox_selected: usize,
    /// Vertical scroll offset (lines) into the selected message's body, so long
    /// messages are fully readable. Reset to 0 when the selection changes.
    pub inbox_scroll: u16,
    /// True when a delete of the selected inbox message is awaiting confirmation
    /// (armed by `Delete`; `y`/Enter confirms, any other key cancels). Cleared
    /// when the inbox opens/closes.
    pub inbox_delete_armed: bool,
    /// The fleet store root — the local config root (your remembered name) and,
    /// in local mode, where this cockpit reads from and writes to.
    pub store_root: PathBuf,
    /// Sends store mutations (messages, projects, disconnect, register) to the
    /// background writer task, which applies them locally or to the daemon.
    pub write_tx: mpsc::UnboundedSender<WriteCmd>,
    /// Your own name as a client, if set (via `--me`/`$LAKITU_FLEET_ME`/remembered
    /// or the first-run prompt). `None` until you pick one.
    pub me: Option<String>,
    /// When `Some`, the compose modal is open. See `Compose`.
    pub compose: Option<Compose>,
    /// When `Some(buffer)`, the first-run "pick your name" prompt is open and
    /// `buffer` is what's been typed so far.
    pub name_prompt: Option<String>,
    /// When `Some(buffer)`, the "new project" name prompt is open (key `P`/`p`).
    pub new_project: Option<String>,
    /// When `Some((id, buffer))`, the "rename project" prompt is open (key `r`
    /// on a project row); `buffer` starts at the current name.
    pub rename_project: Option<(String, String)>,
    /// When `Some(name)`, a "disconnect this client?" confirm dialog is open
    /// for that client. `D` in the Clients pane sets it; Enter/y disconnects,
    /// Esc/n cancels.
    pub confirm_disconnect: Option<String>,
    /// Vertical scroll offset (rows) of the help overlay; reset to 0 when it
    /// opens, clamped to the wrapped content height during render.
    pub help_scroll: u16,
    /// Child process of the inbox-waker (`waker.sh`) when toggled on with `w`.
    /// `None` = off. Spawned as a child of the cockpit and killed on
    /// toggle-off and on exit, so it never outlives the dashboard — you flip
    /// it, you own it.
    pub waker_child: Option<std::process::Child>,
    /// Client names whose subtree is collapsed in the Clients tree (their
    /// owned work-items hidden). Absent = expanded. Toggled with space.
    /// Project rows fold under the key `proj:<id>`.
    pub collapsed: HashSet<String>,
    /// Supervisor-defined projects (groupings of clients), from the store.
    /// Clients in no project's `members` render in the "Floating" group.
    pub projects: Vec<Project>,
    /// Account rate-limit usage (5h / 7d), from agents' statusLine reports.
    pub usage: Option<Usage>,
    /// After an action moves a client between sections (e.g. `m`), the next
    /// render re-points `tree_selected` at this client so the cursor follows it.
    pub reselect_client: Option<String>,
    /// Same, but for a project that was reordered (`m` on a project row) — the
    /// next render re-points the cursor at this project id.
    pub reselect_project: Option<String>,
    /// Terminal image-protocol picker (kitty/iterm/sixel), detected at startup.
    /// `None` if detection failed — icons are skipped then.
    pub picker: Option<Picker>,
    /// Per-client icon render state, keyed by client name. `Some(proto)` = an
    /// icon loaded from `agents/<name>.icon.*`; `None` = looked, found none.
    /// Populated before each draw by `ensure_icons`.
    pub icons: HashMap<String, Option<StatefulProtocol>>,
    /// Source images for iconed clients, retained so we can re-transmit
    /// (recreate the protocol) after clearing stale kitty placements.
    pub icon_src: HashMap<String, image::DynamicImage>,
    /// Last rendered top-row Y of each iconed client's avatar. A change means
    /// the client moved — ghost-prone in terminals that don't clean up kitty
    /// placements on move (e.g. Warp), so it triggers a clear.
    pub icon_y: HashMap<String, u16>,
    /// Set during render when an avatar moved; consumed by the event loop to
    /// delete all kitty placements and re-transmit, clearing the ghost.
    pub clear_images: bool,
}

#[derive(Debug, Clone)]
pub struct ClickTarget {
    pub row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub url: String,
}

impl App {
    pub fn new(
        meta_tx: mpsc::Sender<MetaUpdate>,
        store_root: PathBuf,
        me: Option<String>,
        write_tx: mpsc::UnboundedSender<WriteCmd>,
    ) -> Self {
        App {
            events: Vec::new(),
            work: WorkItems::new(),
            selected: 0,
            follow_newest: true,
            skill_filter: None,
            skill_filter_idx: 0,
            show_help: false,
            show_log: false,
            show_work_pane: false,
            show_client_tasks: true,
            tick: 0,
            click_targets: Vec::new(),
            hover_pos: None,
            titles: HashMap::new(),
            fetch_requested: HashSet::new(),
            repo_exists: Arc::new(Mutex::new(HashMap::new())),
            repo_checks: HashSet::new(),
            meta_tx,
            // Start on the Clients pane — it's the primary view now, so you
            // don't have to press `a` first.
            focus_mode: FocusMode::Clients,
            done_selected: 0,
            acknowledged: HashSet::new(),
            show_story_for: None,
            roster: Vec::new(),
            inboxes: HashMap::new(),
            tasks: HashMap::new(),
            show_tasks_for: None,
            tasks_selected: 0,
            task_input: None,
            tree_selected: 0,
            tree_rows: Vec::new(),
            show_inbox_for: None,
            inbox_selected: 0,
            inbox_scroll: 0,
            inbox_delete_armed: false,
            store_root,
            write_tx,
            // No remembered name yet → open the first-run prompt.
            name_prompt: me.is_none().then(String::new),
            new_project: None,
            rename_project: None,
            confirm_disconnect: None,
            help_scroll: 0,
            me,
            compose: None,
            waker_child: None,
            collapsed: HashSet::new(),
            projects: Vec::new(),
            usage: None,
            reselect_client: None,
            reselect_project: None,
            picker: None,
            icons: HashMap::new(),
            icon_src: HashMap::new(),
            icon_y: HashMap::new(),
            clear_images: false,
        }
    }

    /// Load icon render-protocols for any roster client with an icon file
    /// (`agents/<name>.icon.{webp,png,jpg,jpeg}`) not yet cached. Cheap after
    /// the first pass (only new clients are loaded). `None` cached = no icon /
    /// load failed. No-op without a picker (terminal can't show images).
    pub fn ensure_icons(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let names: Vec<String> = self.roster.iter().map(|a| a.name.clone()).collect();
        for name in names {
            if self.icons.contains_key(&name) {
                continue;
            }
            match load_icon_image(&self.store_root, &name) {
                Some(img) => {
                    // Keep the decoded image so a later clear can re-transmit it.
                    self.icons
                        .insert(name.clone(), Some(picker.new_resize_protocol(img.clone())));
                    self.icon_src.insert(name, img);
                }
                None => {
                    self.icons.insert(name, None);
                }
            }
        }
    }

    /// Drop and rebuild every icon protocol from its retained source image, so
    /// the next render re-transmits each one. Paired with a kitty delete-all by
    /// the event loop to clear stale placements (ghosts) after a client moved.
    pub fn retransmit_icons(&mut self) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        for (name, img) in &self.icon_src {
            self.icons
                .insert(name.clone(), Some(picker.new_resize_protocol(img.clone())));
        }
    }

    /// True when the inbox-waker child is running.
    pub fn waker_running(&self) -> bool {
        self.waker_child.is_some()
    }

    /// Toggle the inbox-waker (`<store>/waker.sh`) on/off. On = spawn it as a
    /// child of the cockpit (so it lives only while the dashboard is open and
    /// you've turned it on); off = kill it. Entirely user-driven — it only
    /// runs while you have it switched on here.
    fn toggle_waker(&mut self) {
        if let Some(mut child) = self.waker_child.take() {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        let script = self.store_root.join("waker.sh");
        if !script.exists() {
            return; // waker not installed → nothing to run
        }
        if let Ok(child) = std::process::Command::new("sh")
            .arg(&script)
            .env("LAKITU_FLEET_ROOT", &self.store_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            self.waker_child = Some(child);
        }
    }

    /// Open the compose modal. Pre-selects the highlighted row's target when
    /// the Clients pane has focus — a client → that client, a project → its
    /// team, a work-item → the client that owns it (title pre-filled to its
    /// `PR #N` / `issue #N` ref, caret dropped in the body) — otherwise
    /// defaults to "everyone". With no `me` set yet, opens the name prompt
    /// instead (you need an identity to send from).
    pub(crate) fn open_compose(&mut self) {
        let Some(me) = self.me.clone() else {
            self.name_prompt = Some(String::new());
            return;
        };
        let targets = self.compose_targets(&me);
        // Pre-select the selected row's target: a project → its team, a client
        // → that client, a work-item → the client that owns it.
        let mut target_idx = 0;
        let mut prefill_title = String::new();
        let mut start_field = ComposeField::Recipient;
        if self.focus_mode == FocusMode::Clients {
            match self.tree_rows.get(self.tree_selected) {
                Some(TreeRow::Project(idx)) => {
                    if let Some(id) = self.projects.get(*idx).map(|p| p.id.clone()) {
                        target_idx = targets
                            .iter()
                            .position(
                                |t| matches!(t, ComposeTarget::Team { id: tid, .. } if *tid == id),
                            )
                            .unwrap_or(0);
                    }
                }
                Some(TreeRow::Client(i)) => {
                    if let Some(name) = self.roster.get(*i).map(|a| a.name.clone()) {
                        target_idx = targets
                            .iter()
                            .position(|t| matches!(t, ComposeTarget::Client(n) if *n == name))
                            .unwrap_or(0);
                    }
                }
                Some(TreeRow::Item {
                    repo, pr, issue, ..
                }) => {
                    // Self-label the message with what you're looking at: the
                    // PR if there is one, else the issue.
                    if let Some(n) = pr {
                        prefill_title = format!("PR #{n}");
                    } else if let Some(n) = issue {
                        prefill_title = format!("issue #{n}");
                    }
                    // Owner = the client this item is nested under (work-item
                    // repo == client repo, mirroring `client_tree`). An
                    // unassigned item (no matching client) leaves the recipient
                    // at the default so you still pick one.
                    if let Some(name) = self
                        .roster
                        .iter()
                        .find(|a| a.repo == *repo && a.name != me)
                        .map(|a| a.name.clone())
                    {
                        if let Some(pos) = targets
                            .iter()
                            .position(|t| matches!(t, ComposeTarget::Client(n) if *n == name))
                        {
                            target_idx = pos;
                            // Recipient + title are set — drop the caret
                            // straight into the body so you can just type.
                            start_field = ComposeField::Body;
                        }
                    }
                }
                Some(TreeRow::Task { owner, text, .. }) => {
                    // Self-label the message with the task title.
                    prefill_title = format!("task: {}", text.chars().take(60).collect::<String>());
                    if *owner != me {
                        // A client's task → message that client; caret to the body.
                        if let Some(pos) = targets
                            .iter()
                            .position(|t| matches!(t, ComposeTarget::Client(n) if n == owner))
                        {
                            target_idx = pos;
                            start_field = ComposeField::Body;
                        }
                    } else {
                        // Your own task → pick who to send it to (a team, say);
                        // recipient stays at the default and the caret starts there.
                        start_field = ComposeField::Recipient;
                    }
                }
                _ => {}
            }
        }
        let title_cursor = prefill_title.chars().count();
        self.compose = Some(Compose {
            targets,
            target_idx,
            title: prefill_title,
            body: String::new(),
            title_cursor,
            body_cursor: 0,
            field: start_field,
            error: None,
            confirming: false,
        });
    }

    /// Build the recipient list for a new message: everyone, each team, then
    /// each other client.
    fn compose_targets(&self, me: &str) -> Vec<ComposeTarget> {
        let mut targets = vec![ComposeTarget::Everyone];
        for p in &self.projects {
            targets.push(ComposeTarget::Team {
                id: p.id.clone(),
                name: p.name.clone(),
            });
        }
        for a in &self.roster {
            if a.name != me {
                targets.push(ComposeTarget::Client(a.name.clone()));
            }
        }
        targets
    }

    /// Reply to the message under the inbox cursor: recipient = its sender,
    /// title "re: <title>" (no "re: re:" stacking), caret dropped in the body.
    /// Hands off to the compose modal. No-op with no message selected.
    pub(crate) fn open_reply(&mut self) {
        let Some(me) = self.me.clone() else {
            self.name_prompt = Some(String::new());
            return;
        };
        let Some((from, title)) = self
            .open_inbox()
            .get(self.inbox_selected)
            .map(|m| (m.from.clone(), m.title.clone()))
        else {
            return;
        };
        let mut targets = self.compose_targets(&me);
        // Reply to the sender even if they aren't a current roster client.
        if from != me
            && !targets
                .iter()
                .any(|t| matches!(t, ComposeTarget::Client(n) if *n == from))
        {
            targets.push(ComposeTarget::Client(from.clone()));
        }
        let target_idx = targets
            .iter()
            .position(|t| matches!(t, ComposeTarget::Client(n) if *n == from))
            .unwrap_or(0);
        let reply_title = if title.trim_start().to_ascii_lowercase().starts_with("re:") {
            title
        } else {
            format!("re: {title}")
        };
        let title_cursor = reply_title.chars().count();
        self.show_inbox_for = None; // hand off to the compose modal
        self.compose = Some(Compose {
            targets,
            target_idx,
            title: reply_title,
            body: String::new(),
            title_cursor,
            body_cursor: 0,
            field: ComposeField::Body,
            error: None,
            confirming: false,
        });
    }

    /// Turn the selected inbox message into a task on that inbox owner's list —
    /// the "don't lose a message that arrived mid-work" bridge. The task text is
    /// the message title (falling back to its first body line), and `from_msg`
    /// records the message id for provenance.
    pub(crate) fn task_from_selected_message(&mut self) {
        let Some(owner) = self.show_inbox_for.clone() else {
            return;
        };
        let payload = {
            let msgs = self.open_inbox();
            let sel = self.inbox_selected.min(msgs.len().saturating_sub(1));
            msgs.get(sel).map(|m| {
                // Title = the message subject (fall back to its first body line);
                // body = the full message text, so the task keeps the whole note.
                let text = if !m.title.trim().is_empty() {
                    m.title.clone()
                } else {
                    let b = m.body.trim();
                    if b.is_empty() {
                        format!("follow up with {}", m.from)
                    } else {
                        b.lines().next().unwrap_or("").chars().take(80).collect()
                    }
                };
                let body = {
                    let b = m.body.trim();
                    if b.is_empty() || b == text.trim() {
                        None
                    } else {
                        Some(m.body.clone())
                    }
                };
                (text, body, m.id.clone())
            })
        };
        if let Some((text, body, id)) = payload {
            let _ = self.write_tx.send(WriteCmd::AddTask {
                owner,
                text,
                body,
                pr: None,
                from_msg: Some(id),
            });
        }
    }

    /// Delete the selected inbox message (discards it from the store). Called
    /// once the delete-confirm is accepted; the row clears on the next poll.
    pub(crate) fn delete_selected_message(&mut self) {
        let Some(owner) = self.show_inbox_for.clone() else {
            return;
        };
        let id = {
            let msgs = self.open_inbox();
            let sel = self.inbox_selected.min(msgs.len().saturating_sub(1));
            msgs.get(sel).map(|m| m.id.clone())
        };
        if let Some(id) = id {
            let _ = self.write_tx.send(WriteCmd::DeleteMessage { owner, id });
            // If we deleted the last row, pull the cursor back so it stays in
            // range once the list shrinks (the renderer also clamps).
            let len = self.open_inbox().len();
            if self.inbox_selected + 1 >= len {
                self.inbox_selected = self.inbox_selected.saturating_sub(1);
            }
        }
    }

    /// Send the composed message (or set an error and keep the modal open if
    /// it's empty). Closes the modal on success.
    fn send_compose(&mut self) {
        let Some(me) = self.me.clone() else {
            self.compose = None;
            return;
        };
        let Some(c) = self.compose.as_ref() else {
            return;
        };
        if c.title.trim().is_empty() && c.body.trim().is_empty() {
            if let Some(c) = self.compose.as_mut() {
                c.error = Some("Type a title or body before sending.".to_string());
            }
            return;
        }
        let title = c.title.trim().to_string();
        let body = c.body.clone();
        let target = c.targets[c.target_idx].clone();
        match target {
            ComposeTarget::Everyone => {
                let recipients: Vec<String> = self
                    .roster
                    .iter()
                    .map(|a| a.name.clone())
                    .filter(|n| *n != me)
                    .collect();
                let _ = self.write_tx.send(WriteCmd::Broadcast {
                    from: me.clone(),
                    recipients,
                    title: title.clone(),
                    body: body.clone(),
                });
            }
            ComposeTarget::Team { id, .. } => {
                // A team with a coordinator routes to the coordinator alone
                // (they triage for the team); otherwise it fans out to every
                // member.
                if let Some(p) = self.projects.iter().find(|p| p.id == id) {
                    let recipients: Vec<String> = match &p.coordinator {
                        Some(coord) => vec![coord.clone()],
                        None => p.members.iter().filter(|m| **m != me).cloned().collect(),
                    };
                    let _ = self.write_tx.send(WriteCmd::Broadcast {
                        from: me.clone(),
                        recipients,
                        title: title.clone(),
                        body: body.clone(),
                    });
                }
            }
            ComposeTarget::Client(name) => {
                let _ = self.write_tx.send(WriteCmd::SendMessage {
                    from: me.clone(),
                    to: name,
                    title: title.clone(),
                    body: body.clone(),
                });
            }
        }
        self.compose = None;
    }

    /// The agent currently under the Clients-pane selection — `Some` only when
    /// the selected row is a client header (not a work-item).
    pub fn selected_agent(&self) -> Option<&Agent> {
        match self.tree_rows.get(self.tree_selected) {
            Some(TreeRow::Client(i)) => self.roster.get(*i),
            _ => None,
        }
    }

    /// Row indices that begin a group: the top (row 0, the floating section)
    /// and every project header. Used by Shift+↑/↓ to hop between groups.
    fn group_starts(&self) -> Vec<usize> {
        let mut starts = Vec::new();
        if !self.tree_rows.is_empty() {
            starts.push(0);
        }
        for (i, r) in self.tree_rows.iter().enumerate() {
            if i != 0 && matches!(r, TreeRow::Project(_)) {
                starts.push(i);
            }
        }
        starts
    }

    /// Fold-state key for the selected foldable row (a project or a client),
    /// or `None` for a work-item / no selection.
    fn selected_fold_key(&self) -> Option<String> {
        match self.tree_rows.get(self.tree_selected) {
            Some(TreeRow::Project(idx)) => {
                self.projects.get(*idx).map(|p| format!("proj:{}", p.id))
            }
            Some(TreeRow::Client(i)) => self.roster.get(*i).map(|a| a.name.clone()),
            _ => None,
        }
    }

    /// Index of the project that lists `client` as a member, if any.
    fn project_of(&self, client: &str) -> Option<usize> {
        self.projects
            .iter()
            .position(|p| p.members.iter().any(|m| m == client))
    }

    /// Cycle the selected client's membership: Floating → project 0 → … →
    /// last → Floating. No-op when no client is selected or no projects exist.
    fn cycle_membership(&mut self) {
        let Some(TreeRow::Client(i)) = self.tree_rows.get(self.tree_selected) else {
            return;
        };
        let Some(name) = self.roster.get(*i).map(|a| a.name.clone()) else {
            return;
        };
        if self.projects.is_empty() {
            return;
        }
        // Current slot: None (floating) = 0, project k = k + 1. Advance, wrap
        // back to floating after the last project.
        let cur = self.project_of(&name).map(|k| k + 1).unwrap_or(0);
        let next = (cur + 1) % (self.projects.len() + 1);
        let target = (next > 0).then(|| self.projects[next - 1].id.clone());
        let _ = self.write_tx.send(WriteCmd::SetMembership {
            client: name.clone(),
            project_id: target,
        });
        self.reselect_client = Some(name); // keep the cursor on the moved client (reflected next poll)
    }

    /// Move the selected project one slot down (last wraps to the top of the
    /// project list — never above the floating clients). No-op otherwise.
    fn move_selected_project_down(&mut self) {
        let Some(TreeRow::Project(idx)) = self.tree_rows.get(self.tree_selected) else {
            return;
        };
        let Some(id) = self.projects.get(*idx).map(|p| p.id.clone()) else {
            return;
        };
        let _ = self.write_tx.send(WriteCmd::MoveProjectDown(id.clone()));
        self.reselect_project = Some(id); // keep the cursor on the moved project (reflected next poll)
    }

    /// Toggle the selected client as its current project's coordinator. No-op
    /// when the selection isn't a client, or the client is floating.
    fn toggle_selected_coordinator(&mut self) {
        let Some(TreeRow::Client(i)) = self.tree_rows.get(self.tree_selected) else {
            return;
        };
        let Some(name) = self.roster.get(*i).map(|a| a.name.clone()) else {
            return;
        };
        if let Some(pi) = self.project_of(&name) {
            let id = self.projects[pi].id.clone();
            let _ = self.write_tx.send(WriteCmd::ToggleCoordinator {
                id,
                client: name.clone(),
            });
            self.reselect_client = Some(name); // coordinator sorts first; follow it (next poll)
        }
    }

    /// Remove the selected project (members float). No-op unless a project row
    /// is selected.
    fn remove_selected_project(&mut self) {
        if let Some(TreeRow::Project(idx)) = self.tree_rows.get(self.tree_selected) {
            if let Some(id) = self.projects.get(*idx).map(|p| p.id.clone()) {
                let _ = self.write_tx.send(WriteCmd::RemoveProject(id));
            }
        }
    }

    /// Messages in the inbox the modal is currently showing (newest first).
    /// Empty when no modal is open or the agent has no inbox dir.
    pub fn open_inbox(&self) -> &[Message] {
        self.show_inbox_for
            .as_ref()
            .and_then(|name| self.inboxes.get(name))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Mark the currently-selected inbox message read — but only when you're
    /// viewing your OWN inbox (peeking at a peer's inbox must never consume
    /// their mail). Called when you open the inbox and as you navigate it, so
    /// simply viewing a message reads it. No-op if already read / not yours.
    /// The on-disk move is reflected in the pane on the next store poll.
    fn read_selected_in_own_inbox(&self) {
        let (Some(owner), Some(me)) = (self.show_inbox_for.as_ref(), self.me.as_ref()) else {
            return;
        };
        if owner != me {
            return;
        }
        if let Some(msg) = self.open_inbox().get(self.inbox_selected) {
            if !msg.read {
                let id = msg.id.clone();
                let _ = self.write_tx.send(WriteCmd::MarkRead {
                    owner: owner.clone(),
                    id,
                });
            }
        }
    }

    /// Done items the supervisor hasn't dismissed yet, in display order
    /// (matches the order `ui::visible_work_items` renders them). Used
    /// by the DoneReview selection logic and by Enter-to-open-story.
    pub fn done_items_visible(&self) -> Vec<&crate::work::WorkItem> {
        self.work
            .sorted()
            .into_iter()
            .filter(|w| {
                w.state == WorkState::Done
                    && w.issue.is_some()
                    && !self.acknowledged.contains(&w.key)
            })
            .collect()
    }

    /// The Done item currently under the DoneReview selection, or None
    /// if the list is empty. The caller must already be in DoneReview
    /// mode; otherwise the selection index is meaningless.
    pub fn selected_done_issue(&self) -> Option<u64> {
        self.done_items_visible()
            .get(self.done_selected)
            .and_then(|w| w.issue)
    }

    /// Click target currently under the mouse cursor, or `None`.
    pub fn hovered_target(&self) -> Option<&ClickTarget> {
        let (row, col) = self.hover_pos?;
        self.click_targets
            .iter()
            .find(|t| t.row == row && col >= t.col_start && col < t.col_end)
    }

    fn cycle_skill_filter(&mut self) {
        self.skill_filter_idx = (self.skill_filter_idx + 1) % SKILL_FILTERS.len();
        self.skill_filter = SKILL_FILTERS[self.skill_filter_idx].map(String::from);
        self.follow_newest = true;
        self.selected = 0;
    }

    // Reversed-display scroll semantics: index 0 is newest (top),
    // higher indices are older (further down).
    fn scroll_up(&mut self) {
        // Visual "up" = toward newest = decrement index.
        self.selected = self.selected.saturating_sub(1);
        // If we've climbed back to the top we're following live again.
        self.follow_newest = self.selected == 0;
    }

    fn scroll_down(&mut self, max_idx: usize) {
        // Visual "down" = toward older = increment index.
        if self.selected + 1 < max_idx {
            self.selected += 1;
        }
        self.follow_newest = false;
    }

    fn jump_top(&mut self) {
        // Top of the reversed view = newest event.
        self.selected = 0;
        self.follow_newest = true;
    }

    fn jump_bottom(&mut self, max_idx: usize) {
        // Bottom of the reversed view = oldest event.
        self.selected = max_idx.saturating_sub(1);
        self.follow_newest = false;
    }

    /// Route a mouse-wheel notch to whatever scrollable surface is in front, so
    /// the wheel scrolls *what you're looking at*: an open help/tasks/inbox
    /// modal, else the focused pane — the Clients tree (the main view, which
    /// scrolls to follow its selection) or the event stream. Text-entry /
    /// confirm modals swallow the wheel so it never disturbs a selection hidden
    /// behind them.
    fn scroll_wheel(&mut self, down: bool) {
        // Modals that own a text buffer or a yes/no choice: ignore the wheel.
        if self.compose.is_some()
            || self.new_project.is_some()
            || self.rename_project.is_some()
            || self.confirm_disconnect.is_some()
            || self.show_story_for.is_some()
        {
            return;
        }
        // Help overlay: scroll its body (render clamps the offset).
        if self.show_help {
            self.help_scroll = if down {
                self.help_scroll.saturating_add(3)
            } else {
                self.help_scroll.saturating_sub(3)
            };
            return;
        }
        // Tasks modal (when browsing, not typing): move the task selection.
        if self.show_tasks_for.is_some() && self.task_input.is_none() {
            if let Some(name) = self.show_tasks_for.clone() {
                let tasks = self.tasks.get(&name).cloned().unwrap_or_default();
                let n = store::task_display_order(&tasks).len();
                if down {
                    if self.tasks_selected + 1 < n {
                        self.tasks_selected += 1;
                    }
                } else {
                    self.tasks_selected = self.tasks_selected.saturating_sub(1);
                }
            }
            return;
        }
        // Inbox modal: step through messages (mirrors j/k; resets body scroll).
        if self.show_inbox_for.is_some() {
            let len = self.open_inbox().len();
            if down {
                if self.inbox_selected + 1 < len {
                    self.inbox_selected += 1;
                }
            } else {
                self.inbox_selected = self.inbox_selected.saturating_sub(1);
            }
            self.inbox_scroll = 0;
            return;
        }
        // Base view: the Clients tree follows the cursor; the event-stream pane
        // uses its reversed-index scroll.
        match self.focus_mode {
            FocusMode::Clients => {
                if down {
                    if self.tree_selected + 1 < self.tree_rows.len() {
                        self.tree_selected += 1;
                    }
                } else {
                    self.tree_selected = self.tree_selected.saturating_sub(1);
                }
            }
            _ => {
                if down {
                    let len = self.filtered_events().len();
                    self.scroll_down(len);
                } else {
                    self.scroll_up();
                }
            }
        }
    }

    /// Open the first #N reference of the currently-selected event in
    /// the user's browser.
    fn open_selected(&self) {
        let Some(ev) = self.filtered_events().get(self.selected).copied() else {
            return;
        };
        let Some(first) = ev.refs.first() else { return };
        let url = first
            .kind
            .url(&resolve_repo(&ev.repo, &self.roster), first.number);
        let _ = webbrowser::open(&url);
    }

    /// Open the PR (preferred) or the issue for the story modal's target
    /// in the user's browser. PR wins because for a Done item the PR is
    /// where the actual changes live and where reviewers' comments are.
    fn open_story_target(&self, issue: u64) {
        let Some(w) = self.work.get(issue) else {
            return;
        };
        let repo = resolve_repo(&w.repo, &self.roster);
        let url = if let Some(pr) = w.pr {
            RefKind::Pr.url(&repo, pr)
        } else {
            RefKind::Issue.url(&repo, issue)
        };
        let _ = webbrowser::open(&url);
    }

    fn filtered_events(&self) -> Vec<&Event> {
        self.events
            .iter()
            .filter(|e| {
                self.skill_filter
                    .as_ref()
                    .map(|f| &e.skill == f)
                    .unwrap_or(true)
            })
            .collect()
    }

    /// Look at the current work items and spawn `gh` fetches for any
    /// issue/PR we haven't requested yet. Idempotent — re-requests are
    /// gated by `fetch_requested`.
    fn enqueue_meta_fetches(&mut self) {
        let mut pending: Vec<(RefKind, u64, String)> = Vec::new();
        for w in self.work.sorted() {
            if let Some(issue) = w.issue {
                let issue_key = (RefKind::Issue, issue);
                if !self.fetch_requested.contains(&issue_key) {
                    self.fetch_requested.insert(issue_key);
                    pending.push((RefKind::Issue, issue, w.repo.clone()));
                }
            }
            if let Some(pr) = w.pr {
                let pr_key = (RefKind::Pr, pr);
                if !self.fetch_requested.contains(&pr_key) {
                    self.fetch_requested.insert(pr_key);
                    pending.push((RefKind::Pr, pr, w.repo.clone()));
                }
            }
        }
        for (kind, number, repo) in pending {
            let tx = self.meta_tx.clone();
            tokio::spawn(async move {
                let meta = match kind {
                    RefKind::Issue | RefKind::NewIssue => gh::fetch_issue(repo, number).await,
                    RefKind::Pr => gh::fetch_pr(repo, number).await,
                };
                let _ = tx
                    .send(MetaUpdate {
                        kind,
                        number,
                        meta: meta.unwrap_or_default(),
                    })
                    .await;
            });
        }
    }

    /// Spawn a one-time GitHub existence check for each roster repo that could
    /// be a link (owner/name, not a glob/local — see `ui::repo_url`). The result
    /// lands in `repo_exists`; the next render tick picks it up. Idempotent.
    fn ensure_repo_checks(&mut self) {
        let todo: Vec<String> = self
            .roster
            .iter()
            .map(|a| a.repo.clone())
            .filter(|r| crate::ui::repo_url(r).is_some() && !self.repo_checks.contains(r))
            .collect();
        for repo in todo {
            self.repo_checks.insert(repo.clone());
            let cache = self.repo_exists.clone();
            tokio::spawn(async move {
                let exists = gh::repo_exists(&repo).await;
                if let Ok(mut m) = cache.lock() {
                    m.insert(repo, exists);
                }
            });
        }
    }
}

/// Resolve a repo string to a full `owner/name` slug. The agent log often records
/// just the bare repo name (e.g. `fossid-vscode`); the registered agents carry
/// full slugs, so we infer the owner from them — matching the bare name to the
/// agent that owns a repo with that basename (handles multi-org fleets, e.g.
/// `local/codex` alongside `fossid-ab/*`). Falls back to the fleet's most common
/// owner, then an optional `LAKITU_DEFAULT_OWNER`, then leaves it bare. Inferring
/// from the roster means no per-machine env var is needed.
pub(crate) fn resolve_repo(repo: &str, roster: &[Agent]) -> String {
    if repo.contains('/') {
        return repo.to_string();
    }
    // 1. An agent whose repo basename matches → adopt that agent's owner.
    for a in roster {
        if let Some((owner, name)) = a.repo.split_once('/') {
            if name == repo && !owner.is_empty() {
                return format!("{owner}/{repo}");
            }
        }
    }
    // 2. The fleet's most common owner (single-org fleets resolve cleanly).
    if let Some(owner) = most_common_owner(roster) {
        return format!("{owner}/{repo}");
    }
    // 3. Optional explicit override; otherwise leave it bare.
    match std::env::var("LAKITU_DEFAULT_OWNER")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(owner) => format!("{owner}/{repo}"),
        None => repo.to_string(),
    }
}

/// The owner prefix shared by the most registered agents — the fleet's org, used
/// to resolve a bare repo that matches no agent by name. `None` if no agent has
/// an `owner/name` repo.
fn most_common_owner(roster: &[Agent]) -> Option<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for a in roster {
        if let Some((owner, _)) = a.repo.split_once('/') {
            if !owner.is_empty() && owner != "-" {
                *counts.entry(owner).or_default() += 1;
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|&(_, n)| n)
        .map(|(owner, _)| owner.to_string())
}

pub async fn run(
    log_path: PathBuf,
    store_root: PathBuf,
    me: Option<String>,
    source: store::Source,
) -> Result<()> {
    let (meta_tx, mut meta_rx) = mpsc::channel::<MetaUpdate>(64);
    // Store writes (messages, projects, disconnect, register) go through a
    // background task so the input handler stays synchronous — it applies each
    // to the local store or the remote daemon, per `source`.
    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<WriteCmd>();
    {
        let source = source.clone();
        tokio::spawn(async move {
            while let Some(cmd) = write_rx.recv().await {
                remote::apply_write(&source, cmd).await;
            }
        });
    }
    // Registration happens in `main` (so it applies before --dump-store too).
    let mut app = App::new(meta_tx, store_root.clone(), me, write_tx);
    // Detect the terminal's image protocol (kitty/iterm/sixel) for client
    // icons — query before raw mode / the alt screen. Best-effort: None just
    // means no icons render.
    app.picker = Picker::from_query_stdio().ok();
    let mut log_rx = log::spawn(log_path);
    let mut store_rx = store::spawn(source);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // Enable the enhanced keyboard protocol where the terminal supports it, so
    // the compose editor can distinguish Shift/Alt+Enter (newline) from plain
    // Enter (send). No-op / fallback to send-on-Enter where unsupported.
    let kbd_enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if kbd_enhanced {
        let _ = execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(
        &mut terminal,
        &mut app,
        &mut log_rx,
        &mut meta_rx,
        &mut store_rx,
    )
    .await;

    disable_raw_mode()?;
    if kbd_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    // Stop the inbox-waker if it was toggled on — it must not outlive the cockpit.
    if let Some(mut child) = app.waker_child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }

    result
}

async fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    log_rx: &mut mpsc::Receiver<Event>,
    meta_rx: &mut mpsc::Receiver<MetaUpdate>,
    store_rx: &mut mpsc::Receiver<StoreSnapshot>,
) -> Result<()> {
    let mut term_events = EventStream::new();
    let mut tick = interval(Duration::from_millis(TICK_MS));

    loop {
        app.ensure_icons();
        terminal.draw(|f| ui::render(f, app))?;

        // An avatar moved this frame: in terminals that don't clean up kitty
        // placements on move (e.g. Warp) the old one lingers as a ghost. Delete
        // every placement, re-transmit the icons, and redraw at once so they
        // re-place cleanly. Only fires on the (infrequent) frames where a row
        // shifted, so there's no steady-state flicker.
        if app.clear_images {
            app.clear_images = false;
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b_Ga=d,d=A\x1b\\");
            let _ = out.flush();
            app.retransmit_icons();
            // Force a full redraw next frame. ratatui-image marks image cells
            // `skip`, so the frame-diff alone won't re-emit the transmit escape
            // after the delete-all — without this the avatar stays gone.
            let _ = terminal.clear();
            continue;
        }

        tokio::select! {
            _ = tick.tick() => {
                app.tick = app.tick.wrapping_add(1);
            }
            Some(ev) = log_rx.recv() => {
                // `card-acknowledged` is a TUI-emitted event marking that the
                // supervisor dismissed a Done item from view. Picked up here
                // so re-runs (and other TUI instances watching the same log)
                // honor the same dismissals.
                if ev.action == "card-acknowledged" {
                    // Keyed by WorkKey: an `issue=#n` row hides the issue-keyed
                    // item, a `pr=#n` row a PR-only item.
                    for r in &ev.refs {
                        match r.kind {
                            RefKind::Issue => {
                                app.acknowledged.insert(WorkKey::Issue(r.number));
                            }
                            RefKind::Pr => {
                                app.acknowledged.insert(WorkKey::Pr(r.number));
                            }
                            _ => {}
                        }
                    }
                }
                app.work.ingest(&ev);
                app.events.push(ev);
                if app.follow_newest {
                    // Index 0 in the reversed display = newest event.
                    app.selected = 0;
                }
                app.enqueue_meta_fetches();
            }
            Some(update) = meta_rx.recv() => {
                let meta = if update.meta.title.is_some() || update.meta.merged || update.meta.closed {
                    Some(update.meta)
                } else {
                    // gh returned nothing useful (missing, 404, …). Cache
                    // a sentinel so we don't retry.
                    None
                };
                app.titles.insert((update.kind, update.number), meta);
            }
            Some(snap) = store_rx.recv() => {
                app.roster = snap.agents;
                app.ensure_repo_checks();
                app.inboxes = snap.inboxes;
                app.tasks = snap.tasks;
                app.projects = snap.projects;
                app.usage = snap.usage; // was missing → the session/weekly chip never showed
                // Keep selections in range as the roster / inbox shrink.
                // (tree_selected is also re-clamped by the renderer against the
                // freshly-built tree_rows; this guards the gap before a redraw.)
                if app.tree_selected >= app.tree_rows.len() {
                    app.tree_selected = app.tree_rows.len().saturating_sub(1);
                }
                let inbox_len = app.open_inbox().len();
                if app.inbox_selected >= inbox_len {
                    app.inbox_selected = inbox_len.saturating_sub(1);
                }
                // If the agent whose inbox is open vanished, close the modal.
                if let Some(name) = &app.show_inbox_for {
                    if !app.roster.iter().any(|a| &a.name == name) {
                        app.show_inbox_for = None;
                    }
                }
            }
            Some(Ok(term_ev)) = term_events.next() => {
                match handle_input(app, term_ev) {
                    InputResult::Continue => {}
                    InputResult::Quit => return Ok(()),
                }
            }
        }
    }
}

enum InputResult {
    Continue,
    Quit,
}

fn handle_input(app: &mut App, ev: CtEvent) -> InputResult {
    match ev {
        // First-run name prompt — owns input while open: type your name,
        // Enter to join as a client, Esc to skip (run without an identity).
        CtEvent::Key(key) if key.kind == KeyEventKind::Press && app.name_prompt.is_some() => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => app.name_prompt = None,
                KeyCode::Enter => {
                    if let Some(buf) = app.name_prompt.take() {
                        let name = buf.trim().to_string();
                        if !name.is_empty() {
                            let _ = client::remember_me(&app.store_root, &name);
                            let _ = app.write_tx.send(WriteCmd::Register(name.clone()));
                            app.me = Some(name);
                        }
                    }
                }
                KeyCode::Backspace => {
                    if let Some(b) = app.name_prompt.as_mut() {
                        b.pop();
                    }
                }
                KeyCode::Char(ch) if !ctrl => {
                    if let Some(b) = app.name_prompt.as_mut() {
                        b.push(ch);
                    }
                }
                _ => {}
            }
        }
        // New-project name prompt — owns input while open: type a name, Enter
        // creates it, Esc cancels.
        CtEvent::Key(key) if key.kind == KeyEventKind::Press && app.new_project.is_some() => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => app.new_project = None,
                KeyCode::Enter => {
                    if let Some(buf) = app.new_project.take() {
                        let _ = app.write_tx.send(WriteCmd::CreateProject(buf));
                    }
                }
                KeyCode::Backspace => {
                    if let Some(b) = app.new_project.as_mut() {
                        b.pop();
                    }
                }
                KeyCode::Char(ch) if !ctrl => {
                    if let Some(b) = app.new_project.as_mut() {
                        b.push(ch);
                    }
                }
                _ => {}
            }
        }
        // Rename-project prompt — owns input while open: edit the name, Enter
        // saves, Esc cancels.
        CtEvent::Key(key) if key.kind == KeyEventKind::Press && app.rename_project.is_some() => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Esc => app.rename_project = None,
                KeyCode::Enter => {
                    if let Some((id, buf)) = app.rename_project.take() {
                        let _ = app.write_tx.send(WriteCmd::RenameProject { id, name: buf });
                    }
                }
                KeyCode::Backspace => {
                    if let Some((_, b)) = app.rename_project.as_mut() {
                        b.pop();
                    }
                }
                KeyCode::Char(ch) if !ctrl => {
                    if let Some((_, b)) = app.rename_project.as_mut() {
                        b.push(ch);
                    }
                }
                _ => {}
            }
        }
        // Disconnect confirm — owns input while open: Enter/y disconnects the
        // client, Esc/n cancels.
        CtEvent::Key(key)
            if key.kind == KeyEventKind::Press && app.confirm_disconnect.is_some() =>
        {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if let Some(name) = app.confirm_disconnect.take() {
                        let _ = app.write_tx.send(WriteCmd::Disconnect(name));
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    app.confirm_disconnect = None;
                }
                _ => {}
            }
        }
        // Help overlay — scroll it while open; ? / esc / q close it.
        CtEvent::Key(key) if key.kind == KeyEventKind::Press && app.show_help => match key.code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                app.show_help = false;
                app.help_scroll = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                app.help_scroll = app.help_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                app.help_scroll = app.help_scroll.saturating_sub(1);
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                app.help_scroll = app.help_scroll.saturating_add(10);
            }
            KeyCode::PageUp => {
                app.help_scroll = app.help_scroll.saturating_sub(10);
            }
            KeyCode::Home | KeyCode::Char('g') => app.help_scroll = 0,
            _ => {}
        },
        // Compose modal — owns input while open. Tab cycles fields, arrows
        // pick the recipient, typing edits Title/Body, Enter advances (and
        // sends from Body), Esc cancels.
        CtEvent::Key(key) if key.kind == KeyEventKind::Press && app.compose.is_some() => {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let field = app.compose.as_ref().map(|c| c.field);
            // When a send is staged, gate input on an explicit confirm:
            // Enter/y dispatch for real, Esc/n return to editing, all else is
            // ignored so a stray keystroke can't send or get lost mid-edit.
            let confirming = app.compose.as_ref().map(|c| c.confirming).unwrap_or(false);
            if confirming {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y') => app.send_compose(),
                    KeyCode::Esc | KeyCode::Char('n') => {
                        if let Some(c) = app.compose.as_mut() {
                            c.confirming = false;
                        }
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Esc => app.compose = None,
                    KeyCode::Tab => {
                        if let Some(c) = app.compose.as_mut() {
                            c.field = c.field.next();
                        }
                    }
                    KeyCode::BackTab => {
                        if let Some(c) = app.compose.as_mut() {
                            c.field = c.field.prev();
                        }
                    }
                    KeyCode::Enter => {
                        // Shift/Alt+Enter inserts a newline in the body; plain
                        // Enter sends (from the body) or advances (other fields).
                        let newline = key
                            .modifiers
                            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                        match field {
                            Some(ComposeField::Body) if newline => {
                                if let Some(c) = app.compose.as_mut() {
                                    insert_at(&mut c.body, &mut c.body_cursor, '\n');
                                }
                            }
                            Some(ComposeField::Body) => {
                                // Stage the send — the confirm prompt + a second
                                // Enter (or `y`) actually dispatches it.
                                if let Some(c) = app.compose.as_mut() {
                                    if c.title.trim().is_empty() && c.body.trim().is_empty() {
                                        c.error = Some(
                                            "Type a title or body before sending.".to_string(),
                                        );
                                    } else {
                                        c.error = None;
                                        c.confirming = true;
                                    }
                                }
                            }
                            Some(_) => {
                                if let Some(c) = app.compose.as_mut() {
                                    c.field = c.field.next();
                                }
                            }
                            None => {}
                        }
                    }
                    KeyCode::Left | KeyCode::Up if field == Some(ComposeField::Recipient) => {
                        if let Some(c) = app.compose.as_mut() {
                            c.target_idx = if c.target_idx == 0 {
                                c.targets.len() - 1
                            } else {
                                c.target_idx - 1
                            };
                        }
                    }
                    KeyCode::Right | KeyCode::Down if field == Some(ComposeField::Recipient) => {
                        if let Some(c) = app.compose.as_mut() {
                            c.target_idx = (c.target_idx + 1) % c.targets.len();
                        }
                    }
                    // Caret movement within Title/Body (Recipient handled above).
                    KeyCode::Left => {
                        if let Some(c) = app.compose.as_mut() {
                            if let Some((_, cur)) = c.active_field_mut() {
                                *cur = cur.saturating_sub(1);
                            }
                        }
                    }
                    KeyCode::Right => {
                        if let Some(c) = app.compose.as_mut() {
                            if let Some((s, cur)) = c.active_field_mut() {
                                *cur = (*cur + 1).min(char_len(s));
                            }
                        }
                    }
                    KeyCode::Up if field == Some(ComposeField::Body) => {
                        if let Some(c) = app.compose.as_mut() {
                            c.body_cursor = move_line(&c.body, c.body_cursor, false);
                        }
                    }
                    KeyCode::Down if field == Some(ComposeField::Body) => {
                        if let Some(c) = app.compose.as_mut() {
                            c.body_cursor = move_line(&c.body, c.body_cursor, true);
                        }
                    }
                    KeyCode::Home => {
                        if let Some(c) = app.compose.as_mut() {
                            if let Some((_, cur)) = c.active_field_mut() {
                                *cur = 0;
                            }
                        }
                    }
                    KeyCode::End => {
                        if let Some(c) = app.compose.as_mut() {
                            if let Some((s, cur)) = c.active_field_mut() {
                                *cur = char_len(s);
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(c) = app.compose.as_mut() {
                            if let Some((s, cur)) = c.active_field_mut() {
                                backspace_at(s, cur);
                            }
                        }
                    }
                    KeyCode::Delete => {
                        if let Some(c) = app.compose.as_mut() {
                            if let Some((s, cur)) = c.active_field_mut() {
                                delete_at(s, cur);
                            }
                        }
                    }
                    KeyCode::Char(ch) if !ctrl => {
                        if let Some(c) = app.compose.as_mut() {
                            if let Some((s, cur)) = c.active_field_mut() {
                                insert_at(s, cur, ch);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Inbox modal — owns input exclusively while open. j/k walk the
        // message list (master/detail), `r` archives the selected message in
        // your own inbox, esc closes back to the Clients pane.
        CtEvent::Key(key) if key.kind == KeyEventKind::Press && app.show_inbox_for.is_some() => {
            match key.code {
                // A pending delete captures the next key: y/Enter confirms,
                // anything else cancels (disarmed below before dispatch).
                _ if app.inbox_delete_armed => {
                    app.inbox_delete_armed = false;
                    if matches!(key.code, KeyCode::Char('y' | 'Y') | KeyCode::Enter) {
                        app.delete_selected_message();
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => app.show_inbox_for = None,
                KeyCode::Char('k') | KeyCode::Up => {
                    app.inbox_selected = app.inbox_selected.saturating_sub(1);
                    app.inbox_scroll = 0;
                    app.read_selected_in_own_inbox();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    let len = app.open_inbox().len();
                    if app.inbox_selected + 1 < len {
                        app.inbox_selected += 1;
                    }
                    app.inbox_scroll = 0;
                    app.read_selected_in_own_inbox();
                }
                // Page through the message list (≈10 at a time); the view follows
                // the selection. Reset the body scroll on the new message.
                KeyCode::PageDown => {
                    let len = app.open_inbox().len();
                    if len > 0 {
                        app.inbox_selected = (app.inbox_selected + 10).min(len - 1);
                    }
                    app.inbox_scroll = 0;
                    app.read_selected_in_own_inbox();
                }
                KeyCode::PageUp => {
                    app.inbox_selected = app.inbox_selected.saturating_sub(10);
                    app.inbox_scroll = 0;
                    app.read_selected_in_own_inbox();
                }
                // Scroll the selected message's body (clamped to content in the renderer).
                KeyCode::Char(' ') => app.inbox_scroll = app.inbox_scroll.saturating_add(8),
                KeyCode::Char('b') => app.inbox_scroll = app.inbox_scroll.saturating_sub(8),
                // Reply to the selected message (recipient + "re:" title pre-filled).
                KeyCode::Char('r') => app.open_reply(),
                // Turn the selected message into a task on this inbox's list, so a
                // message that arrived mid-work isn't lost (records its id as source).
                KeyCode::Char('t') => app.task_from_selected_message(),
                // Arm a delete-confirm for the selected message (Delete key).
                KeyCode::Delete => {
                    if !app.open_inbox().is_empty() {
                        app.inbox_delete_armed = true;
                    }
                }
                _ => {}
            }
        }
        // Story modal — owns input exclusively while open.
        CtEvent::Key(key) if key.kind == KeyEventKind::Press && app.show_story_for.is_some() => {
            match key.code {
                KeyCode::Char('c') => {
                    if let Some(issue) = app.show_story_for {
                        // Persist via the audit log so dismissal survives
                        // restarts and propagates to sibling TUIs.
                        let _ = append_acknowledgement(WorkKey::Issue(issue));
                        app.acknowledged.insert(WorkKey::Issue(issue));
                    }
                    app.show_story_for = None;
                    // Rebound selection: if the list shrank past the
                    // current index, pull back; if empty, leave DoneReview.
                    let max = app.done_items_visible().len();
                    if app.done_selected >= max && max > 0 {
                        app.done_selected = max - 1;
                    }
                    if max == 0 {
                        app.focus_mode = FocusMode::Events;
                    }
                }
                KeyCode::Char('o') => {
                    if let Some(issue) = app.show_story_for {
                        app.open_story_target(issue);
                    }
                }
                KeyCode::Esc => app.show_story_for = None,
                _ => {}
            }
        }
        // Tasks modal — owns input exclusively while open. Browse with j/k;
        // space toggles done, `a` adds, `d` drops, esc closes. When the add
        // line is active (`task_input`), keys edit that buffer instead.
        CtEvent::Key(key) if key.kind == KeyEventKind::Press && app.show_tasks_for.is_some() => {
            let name = app.show_tasks_for.clone().unwrap_or_default();
            if app.task_input.is_some() {
                match key.code {
                    KeyCode::Esc => app.task_input = None,
                    KeyCode::Enter => {
                        if let Some(buf) = app.task_input.take() {
                            let text = buf.trim().to_string();
                            if !text.is_empty() {
                                let _ = app.write_tx.send(WriteCmd::AddTask {
                                    owner: name,
                                    text,
                                    body: None,
                                    pr: None,
                                    from_msg: None,
                                });
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(b) = app.task_input.as_mut() {
                            b.pop();
                        }
                    }
                    KeyCode::Char(c) => {
                        if let Some(b) = app.task_input.as_mut() {
                            b.push(c);
                        }
                    }
                    _ => {}
                }
            } else {
                let tasks = app.tasks.get(&name).cloned().unwrap_or_default();
                let order = store::task_display_order(&tasks);
                let sel = order.get(app.tasks_selected).copied();
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        app.show_tasks_for = None;
                        app.task_input = None;
                        app.tasks_selected = 0;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.tasks_selected = app.tasks_selected.saturating_sub(1);
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if app.tasks_selected + 1 < order.len() {
                            app.tasks_selected += 1;
                        }
                    }
                    // Toggle the selected task done/open.
                    KeyCode::Char(' ') => {
                        if let Some(i) = sel {
                            let _ = app.write_tx.send(WriteCmd::SetTaskDone {
                                owner: name,
                                id: tasks[i].id.clone(),
                                done: !tasks[i].done,
                            });
                        }
                    }
                    // Start the add-task input line.
                    KeyCode::Char('a') | KeyCode::Char('n') => {
                        app.task_input = Some(String::new());
                    }
                    // Drop the selected task; keep the cursor in range after it goes.
                    KeyCode::Char('d') | KeyCode::Delete => {
                        if let Some(i) = sel {
                            let _ = app.write_tx.send(WriteCmd::DropTask {
                                owner: name,
                                id: tasks[i].id.clone(),
                            });
                            if app.tasks_selected + 1 >= order.len() {
                                app.tasks_selected = app.tasks_selected.saturating_sub(1);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        CtEvent::Key(key) if key.kind == KeyEventKind::Press => match key.code {
            KeyCode::Char('q') | KeyCode::Esc if app.show_help => {
                app.show_help = false;
            }
            // DoneReview navigation: j/k cycle Done items, enter opens
            // the story modal, esc returns focus to the event stream.
            KeyCode::Esc if app.focus_mode == FocusMode::DoneReview => {
                app.focus_mode = FocusMode::Events;
            }
            KeyCode::Enter if app.focus_mode == FocusMode::DoneReview => {
                if let Some(issue) = app.selected_done_issue() {
                    app.show_story_for = Some(issue);
                }
            }
            KeyCode::Char('k') | KeyCode::Up if app.focus_mode == FocusMode::DoneReview => {
                app.done_selected = app.done_selected.saturating_sub(1);
            }
            KeyCode::Char('j') | KeyCode::Down if app.focus_mode == FocusMode::DoneReview => {
                let len = app.done_items_visible().len();
                if app.done_selected + 1 < len {
                    app.done_selected += 1;
                }
            }
            // Clients-pane navigation: j/k walk clients AND their work-items,
            // enter acts on the selected row (inbox for a client; PR for a
            // ticket — shift+enter opens the ticket's issue), esc returns focus.
            KeyCode::Esc if app.focus_mode == FocusMode::Clients => {
                app.focus_mode = FocusMode::Events;
            }
            KeyCode::Enter if app.focus_mode == FocusMode::Clients => {
                let shift = key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
                match app.tree_rows.get(app.tree_selected).cloned() {
                    Some(TreeRow::Client(i)) => {
                        if let Some(name) = app.roster.get(i).map(|a| a.name.clone()) {
                            app.show_inbox_for = Some(name);
                            app.inbox_selected = 0;
                            app.inbox_scroll = 0;
                            app.inbox_delete_armed = false;
                            app.read_selected_in_own_inbox();
                        }
                    }
                    Some(TreeRow::Item {
                        repo, pr, issue, ..
                    }) => {
                        // Enter → the PR (where the work is); shift+enter → the
                        // ticket. Each falls back to the other if only one exists.
                        let url = if shift {
                            issue
                                .map(|n| RefKind::Issue.url(&repo, n))
                                .or_else(|| pr.map(|n| RefKind::Pr.url(&repo, n)))
                        } else {
                            pr.map(|n| RefKind::Pr.url(&repo, n))
                                .or_else(|| issue.map(|n| RefKind::Issue.url(&repo, n)))
                        };
                        if let Some(u) = url {
                            let _ = webbrowser::open(&u);
                        }
                    }
                    Some(TreeRow::Task { owner, id, .. }) => {
                        // Open the tasks modal for the owner, with this task selected.
                        let tasks = app.tasks.get(&owner).cloned().unwrap_or_default();
                        let order = store::task_display_order(&tasks);
                        app.tasks_selected =
                            order.iter().position(|&i| tasks[i].id == id).unwrap_or(0);
                        app.show_tasks_for = Some(owner);
                        app.task_input = None;
                    }
                    // A project header has nothing to "open" — space folds it.
                    Some(TreeRow::Project(_)) | None => {}
                }
            }
            // `t` opens the tasks modal for the selected client (or the owning
            // client of a selected work-item).
            KeyCode::Char('t') if app.focus_mode == FocusMode::Clients => {
                let name = match app.tree_rows.get(app.tree_selected) {
                    Some(TreeRow::Client(i)) => app.roster.get(*i).map(|a| a.name.clone()),
                    Some(TreeRow::Item { repo, .. }) => app
                        .roster
                        .iter()
                        .find(|a| &a.repo == repo)
                        .map(|a| a.name.clone()),
                    _ => None,
                };
                if let Some(name) = name {
                    app.show_tasks_for = Some(name);
                    app.tasks_selected = 0;
                    app.task_input = None;
                }
            }
            // Shift+↑/↓ hop between groups: the previous/next group start (the
            // top, or a project header). Must precede the plain ↑/↓ arms.
            KeyCode::Up
                if app.focus_mode == FocusMode::Clients
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Some(&prev) = app
                    .group_starts()
                    .iter()
                    .rev()
                    .find(|&&i| i < app.tree_selected)
                {
                    app.tree_selected = prev;
                }
            }
            KeyCode::Down
                if app.focus_mode == FocusMode::Clients
                    && key.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                if let Some(&next) = app.group_starts().iter().find(|&&i| i > app.tree_selected) {
                    app.tree_selected = next;
                }
            }
            KeyCode::Char('k') | KeyCode::Up if app.focus_mode == FocusMode::Clients => {
                app.tree_selected = app.tree_selected.saturating_sub(1);
            }
            KeyCode::Char('j') | KeyCode::Down if app.focus_mode == FocusMode::Clients => {
                if app.tree_selected + 1 < app.tree_rows.len() {
                    app.tree_selected += 1;
                }
            }
            // Page through a long roster (the pane scrolls to follow the cursor).
            KeyCode::PageUp if app.focus_mode == FocusMode::Clients => {
                app.tree_selected = app.tree_selected.saturating_sub(10);
            }
            KeyCode::PageDown if app.focus_mode == FocusMode::Clients => {
                if !app.tree_rows.is_empty() {
                    app.tree_selected = (app.tree_selected + 10).min(app.tree_rows.len() - 1);
                }
            }
            // ←/→ collapse/expand the selected group (a project or a client).
            KeyCode::Right if app.focus_mode == FocusMode::Clients => {
                if let Some(key) = app.selected_fold_key() {
                    app.collapsed.remove(&key);
                }
            }
            KeyCode::Left if app.focus_mode == FocusMode::Clients => {
                if let Some(key) = app.selected_fold_key() {
                    app.collapsed.insert(key);
                }
            }
            // Space collapses/expands the selected row's subtree — a client's
            // work-items, or a project's member clients.
            KeyCode::Char(' ') if app.focus_mode == FocusMode::Clients => {
                match app.tree_rows.get(app.tree_selected) {
                    Some(TreeRow::Client(i)) => {
                        if let Some(name) = app.roster.get(*i).map(|a| a.name.clone()) {
                            if !app.collapsed.remove(&name) {
                                app.collapsed.insert(name);
                            }
                        }
                    }
                    Some(TreeRow::Project(idx)) => {
                        if let Some(p) = app.projects.get(*idx) {
                            let key = format!("proj:{}", p.id);
                            if !app.collapsed.remove(&key) {
                                app.collapsed.insert(key);
                            }
                        }
                    }
                    _ => {}
                }
            }
            // `x` dismisses a finished (Done/Merged) ticket from the view —
            // acknowledges its issue so it drops out (persisted to the audit
            // log so it stays gone). No-op on active or ticketless items.
            KeyCode::Char('x') if app.focus_mode == FocusMode::Clients => {
                // Dismiss the selected work-item from the board — any state, not
                // just finished. Keyed by the issue when ticketed, else the PR.
                // Persisted to the audit log so it stays gone across restarts.
                let key = match app.tree_rows.get(app.tree_selected) {
                    Some(TreeRow::Item { issue: Some(n), .. }) => Some(WorkKey::Issue(*n)),
                    Some(TreeRow::Item {
                        pr: Some(n),
                        issue: None,
                        ..
                    }) => Some(WorkKey::Pr(*n)),
                    _ => None,
                };
                if let Some(key) = key {
                    let _ = append_acknowledgement(key);
                    app.acknowledged.insert(key);
                }
            }
            // Projects (Clients focus): `P` new project, `m` cycle the selected
            // client through Floating + projects, `*` toggle it as its
            // project's coordinator, `X` remove the selected project.
            KeyCode::Char('P') | KeyCode::Char('p') if app.focus_mode == FocusMode::Clients => {
                app.new_project = Some(String::new());
            }
            KeyCode::Char('r') if app.focus_mode == FocusMode::Clients => {
                if let Some(TreeRow::Project(idx)) = app.tree_rows.get(app.tree_selected) {
                    if let Some(p) = app.projects.get(*idx) {
                        app.rename_project = Some((p.id.clone(), p.name.clone()));
                    }
                }
            }
            KeyCode::Char('m') if app.focus_mode == FocusMode::Clients => {
                match app.tree_rows.get(app.tree_selected) {
                    // On a project row, `m` reorders the project; on a client
                    // row it cycles the client's membership.
                    Some(TreeRow::Project(_)) => app.move_selected_project_down(),
                    _ => app.cycle_membership(),
                }
            }
            KeyCode::Char('*') if app.focus_mode == FocusMode::Clients => {
                app.toggle_selected_coordinator();
            }
            KeyCode::Char('X') if app.focus_mode == FocusMode::Clients => {
                app.remove_selected_project();
            }
            // `D` opens a confirm dialog to disconnect the selected client
            // (remove it from the store + cockpit). Never your own row.
            KeyCode::Char('D') if app.focus_mode == FocusMode::Clients => {
                let target = app
                    .selected_agent()
                    .filter(|a| app.me.as_deref() != Some(a.name.as_str()))
                    .map(|a| a.name.clone());
                if let Some(name) = target {
                    app.confirm_disconnect = Some(name);
                }
            }
            // `a` toggles focus to the agents pane (only if any are attached).
            KeyCode::Char('a') => {
                if app.roster.is_empty() {
                    // Nothing to focus; ignore.
                } else if app.focus_mode == FocusMode::Clients {
                    app.focus_mode = FocusMode::Events;
                } else {
                    app.focus_mode = FocusMode::Clients;
                    app.tree_selected =
                        app.tree_selected.min(app.tree_rows.len().saturating_sub(1));
                }
            }
            // `d` toggles into Done review — only when the Work-items pane is
            // shown (the review highlights a row in that pane) and there are
            // Done items to review.
            KeyCode::Char('d') => {
                if app.show_work_pane && !app.done_items_visible().is_empty() {
                    app.focus_mode = FocusMode::DoneReview;
                    app.done_selected = 0;
                }
            }
            KeyCode::Char('q') => return InputResult::Quit,
            KeyCode::Char('?') => {
                app.show_help = !app.show_help;
                app.help_scroll = 0;
            }
            // `l` reveals/hides the raw event-stream pane (hidden by default).
            KeyCode::Char('l') => app.show_log = !app.show_log,
            // `Tab` hides/shows agent clients' tasks in the pane (declutter);
            // your own tasks always stay visible.
            KeyCode::Tab => app.show_client_tasks = !app.show_client_tasks,
            KeyCode::Char('s') => app.cycle_skill_filter(),
            // `c` composes a message (to the selected client, or everyone).
            // With no name set yet, this opens the first-run name prompt.
            KeyCode::Char('c') => app.open_compose(),
            // `w` toggles the inbox-waker (wakes stopped agents on new mail).
            KeyCode::Char('w') => app.toggle_waker(),
            KeyCode::Char('o') => app.open_selected(),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_up(),
            KeyCode::Char('j') | KeyCode::Down => {
                let len = app.filtered_events().len();
                app.scroll_down(len);
            }
            KeyCode::Home => app.jump_top(),
            KeyCode::End => {
                let len = app.filtered_events().len();
                app.jump_bottom(len);
            }
            _ => {}
        },
        CtEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(target) = hit_test(&app.click_targets, mouse.row, mouse.column) {
                    let _ = webbrowser::open(&target.url);
                }
            }
            MouseEventKind::Moved | MouseEventKind::Drag(_) => {
                // Track hover position for visual hover affordance + status-bar URL hint.
                app.hover_pos = Some((mouse.row, mouse.column));
            }
            MouseEventKind::ScrollUp => app.scroll_wheel(false),
            MouseEventKind::ScrollDown => app.scroll_wheel(true),
            _ => {}
        },
        _ => {}
    }
    InputResult::Continue
}

fn hit_test(targets: &[ClickTarget], row: u16, col: u16) -> Option<&ClickTarget> {
    targets
        .iter()
        .find(|t| t.row == row && col >= t.col_start && col < t.col_end)
}

/// Decode a client's icon from `<store>/agents/<name>.icon.{webp,png,jpg,jpeg}`,
/// the first that exists. `None` when there's no icon file or it won't decode.
/// Transparent padding is trimmed so the artwork fills the avatar slot instead
/// of rendering small with a wide gap to the name.
fn load_icon_image(store_root: &std::path::Path, name: &str) -> Option<image::DynamicImage> {
    let dir = store_root.join("agents");
    for ext in ["webp", "png", "jpg", "jpeg"] {
        let p = dir.join(format!("{name}.icon.{ext}"));
        if p.exists() {
            return image::open(&p).ok().map(trim_transparent);
        }
    }
    None
}

/// Crop fully/near-transparent borders so the visible artwork fills the frame.
/// A square icon with lots of empty alpha around it otherwise renders tiny in
/// its cell box, leaving a big gap before the name. No-op for opaque images.
fn trim_transparent(img: image::DynamicImage) -> image::DynamicImage {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            if rgba.get_pixel(x, y)[3] > 8 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    if x1 < x0 || y1 < y0 {
        return img; // fully transparent — leave as-is
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    if cw == w && ch == h {
        return img; // nothing to trim
    }
    img.crop_imm(x0, y0, cw, ch)
}

/// Append a `card-acknowledged` row to the audit log. Synchronous file
/// append — one line per keypress is well within blocking-IO budget.
/// Best-effort: a write failure (full disk, perm denied) is silently
/// dropped — the in-memory `acknowledged` set still hides the item this
/// session, but the dismissal won't survive a restart. We don't error
/// to the user because the alternative (a modal saying "couldn't write
/// log") is worse UX for a TUI niceity.
fn append_acknowledgement(key: WorkKey) -> std::io::Result<()> {
    use std::io::Write;
    let ts = chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string();
    let detail = match key {
        WorkKey::Issue(n) => format!("issue=#{n}"),
        WorkKey::Pr(n) => format!("pr=#{n}"),
    };
    let line = format!("{ts}\tlakitu-tui\tweb\tcard-acknowledged\t{detail}\n");
    let path = audit_log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())
}

fn audit_log_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home)
        .join(".claude")
        .join("logs")
        .join("agent-actions.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_repo_infers_owner_from_the_roster() {
        fn ag(repo: &str) -> Agent {
            Agent {
                name: "n".into(),
                kind: crate::store::ClientKind::Agent,
                repo: repo.into(),
                board: "-".into(),
                role: None,
                description: None,
                state: crate::store::AgentState::Idle,
                task: None,
                last_seen: None,
                stale: false,
                unread: 0,
                context_pct: None,
            }
        }
        let roster = vec![
            ag("fossid-ab/fossid-vscode"),
            ag("local/codex"),
            ag("fossid-ab/fossid-mcp"),
        ];
        // A bare name matching an agent's repo basename → that agent's owner.
        assert_eq!(
            resolve_repo("fossid-vscode", &roster),
            "fossid-ab/fossid-vscode"
        );
        assert_eq!(resolve_repo("codex", &roster), "local/codex");
        // Already-qualified passes through untouched.
        assert_eq!(resolve_repo("acme/x", &roster), "acme/x");
        // No name match → the fleet's most common owner (fossid-ab here).
        assert_eq!(resolve_repo("mystery", &roster), "fossid-ab/mystery");
    }

    #[test]
    fn insert_and_backspace_at_caret() {
        // Insert "ac", move the caret back one, insert 'b' → "abc".
        let mut s = String::new();
        let mut cur = 0;
        insert_at(&mut s, &mut cur, 'a');
        insert_at(&mut s, &mut cur, 'c');
        assert_eq!((s.as_str(), cur), ("ac", 2));
        cur -= 1; // ← Left
        insert_at(&mut s, &mut cur, 'b');
        assert_eq!(
            (s.as_str(), cur),
            ("abc", 2),
            "insert lands mid-text, not at the end"
        );
        // Backspace removes the char before the caret ('b').
        backspace_at(&mut s, &mut cur);
        assert_eq!((s.as_str(), cur), ("ac", 1));
    }

    #[test]
    fn delete_at_caret_keeps_position() {
        let mut s = String::from("abc");
        let mut cur = 1; // before 'b'
        delete_at(&mut s, &mut cur);
        assert_eq!(
            (s.as_str(), cur),
            ("ac", 1),
            "Delete removes char at caret, caret stays"
        );
        // Delete at end is a no-op.
        let mut cur_end = 2;
        delete_at(&mut s, &mut cur_end);
        assert_eq!((s.as_str(), cur_end), ("ac", 2));
    }

    #[test]
    fn editing_is_utf8_safe() {
        // Multi-byte chars must not panic or split a codepoint.
        let mut s = String::from("a—b"); // em dash is 3 bytes
        let mut cur = 2; // before 'b' (3 chars: a, —, b)
        insert_at(&mut s, &mut cur, 'X');
        assert_eq!((s.as_str(), cur), ("a—Xb", 3));
        backspace_at(&mut s, &mut cur); // removes 'X'
        assert_eq!(s, "a—b");
        // Backspace across the em dash.
        let mut c2 = 2;
        backspace_at(&mut s, &mut c2);
        assert_eq!((s.as_str(), c2), ("ab", 1));
    }

    #[test]
    fn move_line_keeps_column() {
        let s = "abc\nde\nfghi";
        // On line 0 col 2 ('c' is idx 2). Down → line 1, clamped to its len (2).
        assert_eq!(move_line(s, 2, true), 4 + 2); // "abc\n" = 4 chars, +col2 → 6
        // From line 1 (idx 5, col 1) up → line 0 col 1 (idx 1).
        assert_eq!(move_line(s, 5, false), 1);
        // Up on the first line stays put; down on the last line stays put.
        assert_eq!(move_line(s, 1, false), 1);
        assert_eq!(move_line(s, 9, true), 9);
    }
}
