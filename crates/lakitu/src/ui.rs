//! ratatui rendering for the agent activity feed.
//!
//! Layout:
//!   ┌─ Status bar ──────────────────────────────────────┐
//!   │ ⠋ watching  N events  filter: <skill>  ? for help │
//!   ├─ Work items ──────────────────────────────────────┤
//!   │ Ready for Merge   #98 fix: …  [issue-commented]   │
//!   │ In Review         #100  [act-end]  2m ago         │
//!   ├───────────────────────────────────────────────────┤
//!   │ HH:MM  skill         action         details ...   │
//!   │ ...                                               │
//!   └───────────────────────────────────────────────────┘
//!
//! Each `#N` token is recorded as a `ClickTarget` so the mouse handler
//! in `app.rs` can map clicks back to GitHub URLs.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Widget,
    Wrap,
};

use crate::app::{App, ClickTarget, ComposeField, FocusMode, TreeRow};
use crate::event::{Event, RefKind};
use crate::gh::Meta;
use crate::store::{Agent, AgentState, ClientKind};
use crate::work::WorkItem;

/// Secondary text (timestamps, ages, key hints, tags). Brighter than
/// ratatui's built-in `DarkGray`, still clearly subordinate to primary
/// content.
const SECONDARY_FG: Color = Color::Rgb(140, 140, 140);

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Mascot avatar (kitty-protocol image) dimensions in terminal cells. The
/// cockpit's avatar (`CORNER_AVATAR`) is pinned to the top-right corner at
/// `ICON_W × ICON_H`. Width is the minimum that keeps a portrait icon at the
/// full `ICON_H` rows (Fit-scaled, so a wider slot just adds dead space; a
/// narrower one shrinks the image below 2 rows).
const ICON_W: u16 = 3;
const ICON_H: u16 = 2;

/// Roster name whose icon is shown as the cockpit's corner mascot.
const CORNER_AVATAR: &str = "lakitu";

/// Width of the event row's prefix columns (HH:MM + skill + action +
/// separator spaces). Anything in `details` lands at this column.
const EVENT_PREFIX_WIDTH: u16 = 5 /* HH:MM */ + 1 + 18 + 1 + 22 + 1;

pub fn render(frame: &mut Frame, app: &mut App) {
    app.click_targets.clear();

    let area_h = frame.area().height;
    let area_w = frame.area().width;

    // Clients pane is a tree: project headers, clients + their owned items, a
    // Floating group, and an Unassigned group. Size it to the visible rows,
    // capped at ~half screen. Mirror render_agents_pane's section layout.
    // Box inner width = pane border (2) + box indent (3) + box border (2) = 7.
    let box_inner_w = area_w.saturating_sub(7);
    // Per section: a project divider is 1 row; a client is a header row + a box
    // (status + items + tasks + 2 borders). `section_height` is the shared
    // source so the pane size and the scroll math agree.
    let est_tasks = rendered_open_tasks(app);
    let est_items: Vec<crate::work::WorkItem> = app
        .work
        .sorted()
        .into_iter()
        .filter(|w| !app.acknowledged.contains(&w.key))
        .cloned()
        .collect();
    let tree_lines: u16 = build_sections(app)
        .iter()
        .map(|s| section_height(app, s, &est_tasks, &est_items, box_inner_w))
        .sum();
    // Clients pane is the main content now (work-items live inside each
    // client's box). Cap it at ~half-screen only when the standalone work
    // pane is shown; otherwise let it fill, reserving room for the log when on.
    let clients_cap = if app.show_work_pane {
        (area_h / 2).max(3)
    } else {
        let reserve = if app.show_log { (area_h / 2).max(3) } else { 0 };
        area_h.saturating_sub(1 + reserve).max(3)
    };
    let agents_pane_height = (tree_lines.max(1) + 2).clamp(3, clients_cap);

    // Build the vertical layout: status bar, clients pane, then the optional
    // standalone Work-items pane, the optional event log, and a spacer. Track
    // each optional chunk's index so we render into the right slot.
    let mut constraints = vec![
        Constraint::Length(1),                  // status bar
        Constraint::Length(agents_pane_height), // clients pane
    ];
    let mut next = 2usize;
    let mut used = 1 + agents_pane_height;

    let work_idx = if app.show_work_pane {
        let visible = visible_work_items(app);
        let has_done = visible
            .iter()
            .any(|w| w.state == crate::work::WorkState::Done);
        let item_count = (visible.len().max(1) as u16) + if has_done { 1 } else { 0 };
        let work_cap = if app.show_log {
            14
        } else {
            area_h.saturating_sub(used + 1).max(5)
        };
        let work_pane_height = (item_count + 2).clamp(5, work_cap);
        constraints.push(Constraint::Length(work_pane_height));
        used += work_pane_height;
        let i = next;
        next += 1;
        Some(i)
    } else {
        None
    };

    // Event stream is hidden by default (toggle `l`). When shown, cap ~half
    // the remaining height; a spacer soaks up the rest either way.
    let event_idx = if app.show_log {
        let leftover = area_h.saturating_sub(used);
        let event_h = leftover.min(area_h / 2).max(3).min(leftover.max(1));
        let spacer_h = leftover.saturating_sub(event_h);
        constraints.push(Constraint::Length(event_h));
        constraints.push(Constraint::Length(spacer_h));
        Some(next)
    } else {
        constraints.push(Constraint::Length(area_h.saturating_sub(used)));
        None
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());

    // Reserve the top-right corner for the mascot avatar so the status bar
    // text never runs under it.
    let show_corner = matches!(app.icons.get(CORNER_AVATAR), Some(Some(_)));
    let mut status_area = chunks[0];
    if show_corner {
        status_area.width = status_area.width.saturating_sub(ICON_W + 1);
    }
    render_status_bar(frame, status_area, app);
    render_agents_pane(frame, chunks[1], app);
    if let Some(i) = work_idx {
        render_work_items(frame, chunks[i], app);
    }
    if let Some(i) = event_idx {
        render_event_list(frame, chunks[i], app);
    }

    // Mascot avatar, pinned to the top-right corner (rendered after the panes
    // so it sits on top; a Clear gives it a clean background under the icon's
    // transparency). It only moves on a width change (resize), caught by the
    // clear-on-move path via `clear_images`.
    if show_corner {
        let cw = ICON_W.min(area_w);
        let rect = Rect {
            x: area_w.saturating_sub(cw),
            y: 0,
            width: cw,
            height: ICON_H.min(area_h),
        };
        if let Some(prev) = app.icon_y.insert(CORNER_AVATAR.to_string(), rect.x) {
            if prev != rect.x {
                app.clear_images = true;
            }
        }
        if let Some(proto) = app.icons.get_mut(CORNER_AVATAR).and_then(|o| o.as_mut()) {
            frame.render_widget(ratatui::widgets::Clear, rect);
            frame.render_stateful_widget(ratatui_image::StatefulImage::default(), rect, proto);
        }
    }

    if let Some(name) = app.show_inbox_for.clone() {
        render_inbox_modal(frame, app, &name);
    }
    if let Some(name) = app.show_tasks_for.clone() {
        render_tasks_modal(frame, app, &name);
    }
    if let Some(issue) = app.show_story_for {
        render_story_modal(frame, app, issue);
    }
    if app.compose.is_some() {
        render_compose_modal(frame, app);
    }
    if app.show_help {
        render_help_overlay(frame, app);
    }
    if app.new_project.is_some() {
        render_new_project_modal(frame, app);
    }
    if app.rename_project.is_some() {
        render_rename_project_modal(frame, app);
    }
    if app.confirm_disconnect.is_some() {
        render_disconnect_confirm(frame, app);
    }
    // First-run prompt sits on top of everything else.
    if app.name_prompt.is_some() {
        render_name_prompt(frame, app);
    }
}

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans: Vec<Span> = Vec::new();
    if let Some(filter) = &app.skill_filter {
        spans.push(Span::raw("  ·  filter: "));
        spans.push(Span::styled(
            filter.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Persistent badge when the inbox-waker is on (visible in every focus mode).
    if app.waker_running() {
        spans.push(Span::styled(
            "  ⚡ waker",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Unmissable alert: clients blocked on the SUPERVISOR (not peers) need you.
    // Reverse-video so it can't be glossed over; shown in every focus mode.
    let blocked = app
        .roster
        .iter()
        .filter(|a| a.state == AgentState::Blocked && !a.stale)
        .count();
    if blocked > 0 {
        spans.push(Span::styled(
            format!("  ⚠ {blocked} blocked — needs you ", blocked = blocked),
            Style::default()
                .fg(Color::Rgb(220, 130, 40))
                .add_modifier(Modifier::BOLD | Modifier::REVERSED),
        ));
    }
    // Calm, non-reverse count of clients waiting on a peer/external — stuck,
    // but not your call, so it's informational, not an alert.
    let waiting = app
        .roster
        .iter()
        .filter(|a| a.state == AgentState::Waiting && !a.stale)
        .count();
    if waiting > 0 {
        spans.push(Span::styled(
            format!("  ◐ {waiting} waiting", waiting = waiting),
            Style::default().fg(Color::Rgb(210, 170, 60)),
        ));
    }

    // Hover hint: when the mouse is over a clickable, show its URL.
    // Pre-empts the key hints — hover is the more interesting state.
    // DoneReview mode pre-empts both — its own key set replaces the
    // default hint cluster so the supervisor sees the relevant binds.

    // A hovered link's URL takes over the hint area; otherwise just the `?`
    // help pointer (the full key reference lives in the help overlay).
    if let Some(target) = app.hovered_target() {
        spans.push(Span::raw("  ·  "));
        spans.push(Span::styled(
            "↗ open ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            target.url.clone(),
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::UNDERLINED),
        ));
    } else {
        spans.push(Span::styled("   ? ", Style::default().fg(SECONDARY_FG)));
        spans.push(Span::styled("help", Style::default().fg(SECONDARY_FG)));
    }

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Reset));
    frame.render_widget(bar, area);

    // Account rate-limit usage, pinned to the right edge so a long left cluster
    // can't clip it (5h ≈ Claude's "current session", 7d = the weekly cap).
    if let Some(u) = &app.usage {
        // These numbers only move when a session makes an API call (Claude Code
        // reads them from the rate-limit headers and writes them via the
        // statusLine). An idle fleet freezes them — and a frozen reading
        // over-states you, since the rolling windows recover. So once a reading
        // is older than USAGE_STALE_SECS, grey it out and show its age, rather
        // than letting a stale number masquerade as live.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let age = (now - u.ts).max(0);
        let stale = age > USAGE_STALE_SECS;
        let pct = |p: f32| {
            let color = if stale { SECONDARY_FG } else { usage_color(p) };
            Span::styled(
                format!("{p:.0}%"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        };
        let dim = |s: &'static str| Span::styled(s, Style::default().fg(SECONDARY_FG));
        // " (1h10m)" — time until that window's rolling limit resets / frees up.
        let reset = |at: Option<i64>| -> Option<Span> {
            let rem = at? - now;
            (rem > 0).then(|| {
                Span::styled(
                    format!(" ({})", fmt_reset(rem)),
                    Style::default().fg(SECONDARY_FG),
                )
            })
        };
        let mut cells = vec![dim("◔ session "), pct(u.five_hour_pct)];
        cells.extend(reset(u.five_hour_reset));
        cells.push(dim(" · weekly "));
        cells.push(pct(u.seven_day_pct));
        cells.extend(reset(u.seven_day_reset));
        if stale {
            cells.push(Span::styled(
                format!(" · {} old", fmt_age(age)),
                Style::default().fg(SECONDARY_FG),
            ));
        }
        cells.push(dim(" "));
        // Reserve the top-right corner for the mascot avatar so the chip clears it.
        let reserve = ICON_W + 1;
        let uw = (cells
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>() as u16)
            .min(area.width.saturating_sub(reserve));
        let urect = Rect {
            x: area.x + area.width.saturating_sub(uw + reserve),
            y: area.y,
            width: uw,
            height: area.height,
        };
        frame.render_widget(Paragraph::new(Line::from(cells)), urect);
    }
}

/// A usage reading older than this (seconds) is treated as stale: the chip
/// greys out and shows the reading's age. ~1.5 min — comfortably longer than
/// the sub-minute cadence an active fleet refreshes at, so it only trips when
/// the whole fleet has gone idle and nothing is reporting fresh numbers.
const USAGE_STALE_SECS: i64 = 90;

/// Colour for a rate-limit percentage: calm grey under 70%, amber as it nears
/// the cap, bold red past 85% — so a near-limit actually grabs the eye.
fn usage_color(pct: f32) -> Color {
    if pct >= 85.0 {
        Color::Rgb(220, 80, 80)
    } else if pct >= 70.0 {
        Color::Rgb(220, 170, 60)
    } else {
        SECONDARY_FG
    }
}

/// Compact age for a stale usage reading: "2m", "1h".
fn fmt_age(secs: i64) -> String {
    if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}m", (secs / 60).max(1))
    }
}

/// Compact "time until reset" for the usage chip: "47m", "1h10m", "6d14h".
fn fmt_reset(secs: i64) -> String {
    let s = secs.max(0);
    let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
    if d > 0 {
        if h > 0 {
            format!("{d}d{h}h")
        } else {
            format!("{d}d")
        }
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

/// One row-group in the Clients tree: a client (roster index) or the
/// "Unassigned" bucket, plus the work-items owned by it (by-repo match).
struct ClientGroup {
    client: Option<usize>,
    key: String,
    items: Vec<WorkItem>,
}

/// Group the visible work-items under their owning client (work-item repo ==
/// client repo), with an "Unassigned" group for items whose repo matches no
/// registered client. Returns owned data (clones), so callers can mutate App.
fn client_tree(app: &App) -> Vec<ClientGroup> {
    // Resolve each work-item's repo to a full owner/name slug from the roster,
    // so bare log repos (e.g. `fossid-vscode`) match their owning agent — and
    // produce correct GitHub links — instead of falling into "Unassigned".
    let items: Vec<WorkItem> = visible_work_items(app)
        .into_iter()
        .map(|mut w| {
            w.repo = crate::app::resolve_repo(&w.repo, &app.roster);
            w
        })
        .collect();
    let repos: std::collections::HashSet<&str> =
        app.roster.iter().map(|a| a.repo.as_str()).collect();
    let mut groups: Vec<ClientGroup> = app
        .roster
        .iter()
        .enumerate()
        .map(|(i, agent)| ClientGroup {
            client: Some(i),
            key: agent.name.clone(),
            items: items
                .iter()
                .filter(|w| w.repo == agent.repo)
                .cloned()
                .collect(),
        })
        .collect();
    let unassigned: Vec<WorkItem> = items
        .iter()
        .filter(|w| !repos.contains(w.repo.as_str()))
        .cloned()
        .collect();
    if !unassigned.is_empty() {
        groups.push(ClientGroup {
            client: None,
            key: "·unassigned·".to_string(),
            items: unassigned,
        });
    }
    groups
}

/// One row-section of the Clients tree, in render order. Clients outside any
/// project render first (at the top), then each project with its members, then
/// the unassigned-items bucket.
enum Section {
    /// A project header — index into `app.projects`. Foldable.
    ProjectHeader(usize),
    /// A client (or the unassigned-items bucket) and its work-items.
    Group(ClientGroup),
}

/// Fold-state key for a project row.
fn project_fold_key(id: &str) -> String {
    format!("proj:{id}")
}

/// Order the per-client groups for the tree: clients outside any project
/// render first (at the top, in roster order — that's you + cross-project
/// helpers), then each project with its registered members (coordinator
/// first), then the unassigned-items bucket. With no projects it's just the
/// flat fleet, exactly as before.
fn build_sections(app: &App) -> Vec<Section> {
    use std::collections::{HashMap, HashSet};
    let mut by_name: HashMap<String, ClientGroup> = HashMap::new();
    let mut order: Vec<String> = Vec::new(); // roster order of client names
    let mut unassigned: Option<ClientGroup> = None;
    for g in client_tree(app) {
        match g.client {
            Some(_) => {
                order.push(g.key.clone());
                by_name.insert(g.key.clone(), g);
            }
            None => unassigned = Some(g),
        }
    }

    // Resolve project membership up front (coordinator-first, deduped: a name
    // listed in two projects lands in the first) so we know which clients are
    // "outside" / floating.
    let mut placed: HashSet<String> = HashSet::new();
    let mut members: Vec<Vec<String>> = Vec::with_capacity(app.projects.len());
    for p in &app.projects {
        let mut names: Vec<String> = Vec::new();
        if let Some(c) = p.coordinator.as_ref() {
            if by_name.contains_key(c) && placed.insert(c.clone()) {
                names.push(c.clone());
            }
        }
        for m in &p.members {
            if Some(m) != p.coordinator.as_ref()
                && by_name.contains_key(m)
                && placed.insert(m.clone())
            {
                names.push(m.clone());
            }
        }
        members.push(names);
    }

    let mut sections = Vec::new();
    // 1. Outside / floating clients on top, in roster order (no header — the
    //    default ungrouped area; project headers below delimit the rest).
    for name in &order {
        if !placed.contains(name) {
            if let Some(g) = by_name.remove(name) {
                sections.push(Section::Group(g));
            }
        }
    }
    // 2. Projects, each with its members (hidden when folded).
    for (idx, p) in app.projects.iter().enumerate() {
        sections.push(Section::ProjectHeader(idx));
        if app.collapsed.contains(&project_fold_key(&p.id)) {
            continue;
        }
        for name in &members[idx] {
            if let Some(g) = by_name.remove(name) {
                sections.push(Section::Group(g));
            }
        }
    }
    // 3. Unassigned work-items last.
    if let Some(g) = unassigned {
        sections.push(Section::Group(g));
    }
    sections
}

/// The Clients pane — a tree: one row per client (agents + you), each with the
/// work-items it owns nested beneath it (collapsible with space), plus an
/// "Unassigned" group for items whose repo has no client. The work-items pane
/// below still shows everything by state; this view shows *who owns what*.
/// Visual height (rows) one tree section occupies in the clients pane: a project
/// divider is 1; a client is a header row plus its box (status + items + open
/// tasks + 2 borders, or nothing when empty). The single source for both the
/// pane-size estimate (in `render`) and the scroll math (in `render_agents_pane`).
/// Open tasks that actually render in the tree (fold + the Tab toggle applied),
/// across every client. Shared by the pane-size estimator and the renderer so
/// the task-adoption math (which work-items move under a task) agrees in both.
fn rendered_open_tasks(app: &App) -> Vec<crate::store::Task> {
    app.roster
        .iter()
        .filter(|a| {
            !app.collapsed.contains(&a.name)
                && (app.show_client_tasks || a.kind == ClientKind::Human)
        })
        .filter_map(|a| app.tasks.get(&a.name))
        .flat_map(|ts| ts.iter().filter(|t| !t.done).cloned())
        .collect()
}

/// Box content rows for a client group: status lines + work-items NOT adopted
/// by a task + the client's tasks + the work-item each task adopts as a child.
/// The single source of truth for box height — shared by `section_height` (which
/// sizes the off-screen buffer) and the renderer below, so the two can't drift
/// and clip rows. `all_tasks` is the set of tasks that actually render (fold +
/// Tab toggle already applied); `all_items` is every work-item, for resolving a
/// task's linked board issue/PR (which may live in another client's repo).
fn group_content_rows(
    app: &App,
    g: &ClientGroup,
    all_tasks: &[crate::store::Task],
    all_items: &[crate::work::WorkItem],
    box_inner_w: u16,
) -> u16 {
    let status = match g.client {
        Some(i)
            if app.roster[i].kind == ClientKind::Agent
                && app.roster[i]
                    .task
                    .as_deref()
                    .map(|t| !t.trim().is_empty())
                    .unwrap_or(false) =>
        {
            wrap_text(app.roster[i].task.as_deref().unwrap(), box_inner_w).len() as u16
        }
        _ => 0,
    };
    if app.collapsed.contains(&g.key) {
        return status; // folded: items + tasks hidden, status still shows
    }
    // Work-items not adopted by any rendered task — adopted ones move under
    // their task, so they don't also stand alone here.
    let items = g
        .items
        .iter()
        .filter(|w| !all_tasks.iter().any(|t| task_on_item(t, w)))
        .count() as u16;
    let (tasks, children) = match g.client {
        Some(i) if app.show_client_tasks || app.roster[i].kind == ClientKind::Human => {
            let open: Vec<&crate::store::Task> = app
                .tasks
                .get(&app.roster[i].name)
                .map(|ts| ts.iter().filter(|t| !t.done).collect())
                .unwrap_or_default();
            let children = open
                .iter()
                .filter(|t| all_items.iter().any(|w| task_on_item(t, w)))
                .count() as u16;
            (open.len() as u16, children)
        }
        _ => (0, 0),
    };
    status + items + tasks + children
}

fn section_height(
    app: &App,
    section: &Section,
    all_tasks: &[crate::store::Task],
    all_items: &[crate::work::WorkItem],
    box_inner_w: u16,
) -> u16 {
    match section {
        Section::ProjectHeader(_) => 1,
        Section::Group(g) => {
            let content = group_content_rows(app, g, all_tasks, all_items, box_inner_w);
            1 + if content > 0 { content + 2 } else { 0 }
        }
    }
}

fn render_agents_pane(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus_mode == FocusMode::Clients;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        })
        .title(Span::styled(
            format!(
                " Clients ({}){} ",
                app.roster.len(),
                if app.show_client_tasks {
                    ""
                } else {
                    " · tasks hidden"
                }
            ),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.roster.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  (no clients yet — agents appear once they register; pass --me <name> to add yourself)",
                Style::default().fg(SECONDARY_FG),
            ))),
            inner,
        );
        return;
    }

    let sections = build_sections(app);
    let avail = inner.height;
    const BOX_X: u16 = 3;
    let box_inner_w = inner.width.saturating_sub(BOX_X + 2);

    // Tasks that actually render (fold + the Tab toggle applied), across every
    // client — used to decide which work-items a task "adopts" as its child (so
    // they get pulled out of their own section and shown under the task). The
    // gate here mirrors the per-box one below. `all_items` resolves a task's
    // linked board issue/PR, which may live in another client's repo.
    let all_tasks = rendered_open_tasks(app);
    let all_items: Vec<crate::work::WorkItem> = app
        .work
        .sorted()
        .into_iter()
        .filter(|w| !app.acknowledged.contains(&w.key))
        .map(|w| {
            let mut w = w.clone();
            w.repo = crate::app::resolve_repo(&w.repo, &app.roster);
            w
        })
        .collect();

    // The whole tree is rendered into an off-screen buffer of its full height,
    // then the visible window is blitted into the pane — scrolling to follow the
    // selection. This keeps boxes/borders intact (no partial-box clipping).
    let content_h: u16 = sections
        .iter()
        .map(|s| section_height(app, s, &all_tasks, &all_items, box_inner_w))
        .sum::<u16>()
        .max(1);
    let cbuf_h = content_h.max(avail).max(1);
    let mut cbuf = Buffer::empty(Rect::new(inner.x, 0, inner.width, cbuf_h));

    let mut row_y: u16 = 0; // content-relative (row in cbuf)
    // Selectable rows + their content-y (so the selection can drive the scroll),
    // built in render order so `tree_selected` lines up. Click targets are
    // collected content-relative and translated to screen rows after scrolling.
    let selected = app.tree_selected;
    let mut rows: Vec<TreeRow> = Vec::new();
    let mut row_ys: Vec<u16> = Vec::new();
    let mut click_local: Vec<(u16, u16, u16, String)> = Vec::new();

    for section in &sections {
        // Project / Floating headers are 1-row dividers; only a project header
        // is selectable (TreeRow::Project). Other sections fall through to the
        // existing client / unassigned-items rendering below.
        let group = match section {
            Section::ProjectHeader(idx) => {
                let p = &app.projects[*idx];
                let collapsed = app.collapsed.contains(&project_fold_key(&p.id));
                let marker = if collapsed { '▸' } else { '▾' };
                let spans = vec![
                    Span::styled(format!("{marker} "), Style::default().fg(SECONDARY_FG)),
                    Span::styled(
                        p.name.clone(),
                        Style::default()
                            .fg(Color::Rgb(150, 170, 255))
                            .add_modifier(Modifier::BOLD),
                    ),
                ];
                let mut style = Style::default();
                if focused && rows.len() == selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Paragraph::new(Line::from(spans)).style(style).render(
                    Rect {
                        x: inner.x,
                        y: row_y,
                        width: inner.width,
                        height: 1,
                    },
                    &mut cbuf,
                );
                rows.push(TreeRow::Project(*idx));
                row_ys.push(row_y);
                row_y += 1;
                continue;
            }
            Section::Group(g) => g,
        };
        let collapsed = app.collapsed.contains(&group.key);
        let header_rows = 1u16;
        match group.client {
            Some(i) => {
                let marker = if group.items.is_empty() {
                    ' '
                } else if collapsed {
                    '▸'
                } else {
                    '▾'
                };
                let mut style = Style::default();
                if app.roster[i].stale {
                    style = style.add_modifier(Modifier::DIM);
                }
                if focused && rows.len() == selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let is_leader = app
                    .projects
                    .iter()
                    .any(|p| p.coordinator.as_deref() == Some(app.roster[i].name.as_str()));
                // The role links to the repo only when it's a valid owner/name
                // (not a glob/local) AND confirmed to exist on GitHub (cached).
                let repo = &app.roster[i].repo;
                let exists = app
                    .repo_exists
                    .lock()
                    .map(|m| m.get(repo).copied().unwrap_or(false))
                    .unwrap_or(false);
                let linkable = repo_url(repo).is_some() && exists;
                // Agent tasks hide behind the Tab toggle; the human's always show.
                let show_tasks = app.show_client_tasks || app.roster[i].kind == ClientKind::Human;
                let open_tasks = if show_tasks {
                    app.tasks
                        .get(&app.roster[i].name)
                        .map(|ts| ts.iter().filter(|t| !t.done).count())
                        .unwrap_or(0)
                } else {
                    0
                };
                let (line, role_hit) = build_agent_line(
                    &app.roster[i],
                    app.tick,
                    marker,
                    is_leader,
                    row_y,
                    inner.x,
                    None,
                    linkable,
                    open_tasks,
                );
                Paragraph::new(line).style(style).render(
                    Rect {
                        x: inner.x,
                        y: row_y,
                        width: inner.width,
                        height: 1,
                    },
                    &mut cbuf,
                );
                // The role chip links to the agent's repo (see build_agent_line).
                if let Some((rc, rw)) = role_hit {
                    if let Some(url) = repo_url(&app.roster[i].repo) {
                        click_local.push((row_y, inner.x + rc, inner.x + rc + rw, url));
                    }
                }
                // Context-window usage, hard right of the row. Gray normally,
                // amber past 70%, bold orange past 85% — an at-a-glance "compact
                // me" signal (auto-compaction only kicks in near the limit).
                if let Some(pct) = app.roster[i].context_pct {
                    let s = format!("{pct}%");
                    let w = s.len() as u16;
                    if inner.width > w + 1 {
                        let st = if pct >= 85 {
                            Style::default()
                                .fg(Color::Rgb(220, 130, 40))
                                .add_modifier(Modifier::BOLD)
                        } else if pct >= 70 {
                            Style::default().fg(Color::Rgb(210, 170, 60))
                        } else {
                            Style::default().fg(SECONDARY_FG)
                        };
                        Paragraph::new(Span::styled(s, st)).render(
                            Rect {
                                x: inner.x + inner.width - w,
                                y: row_y,
                                width: w,
                                height: 1,
                            },
                            &mut cbuf,
                        );
                    }
                }
                rows.push(TreeRow::Client(i));
                row_ys.push(row_y);
            }
            None => {
                let marker = if collapsed { '▸' } else { '▾' };
                Paragraph::new(Line::from(vec![
                    Span::styled(format!("{marker} "), Style::default().fg(SECONDARY_FG)),
                    Span::styled(
                        format!("Unassigned ({})", group.items.len()),
                        Style::default()
                            .fg(SECONDARY_FG)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]))
                .render(
                    Rect {
                        x: inner.x,
                        y: row_y,
                        width: inner.width,
                        height: 1,
                    },
                    &mut cbuf,
                );
            }
        }
        row_y += header_rows;

        // A boxed section per client holding its current task (italic, wrapped)
        // and its work-items. Either alone draws the box: a client with only a
        // status gets a status-only box; one with only items gets an items-only
        // box. The status shows even when folded (it's client status); items
        // are hidden when folded. Box indented to col 3 (BOX_X, set above).

        // Status (the agent's current task), wrapped to the box width. Owned
        // up front so we don't hold a borrow on `app` across the item loop.
        let status_lines: Vec<String> = match group.client {
            Some(i) => match (app.roster[i].kind, app.roster[i].task.as_deref()) {
                (ClientKind::Agent, Some(t)) if !t.trim().is_empty() => wrap_text(t, box_inner_w),
                _ => Vec::new(),
            },
            None => Vec::new(),
        };
        let stale = group.client.map(|i| app.roster[i].stale).unwrap_or(false);

        // Open tasks for this client (hidden when folded, like work-items).
        // Owned up front so we don't hold a borrow on `app` across the loop.
        let open_tasks: Vec<crate::store::Task> = match group.client {
            // Hidden for agents when the Tab toggle is off; always shown for you.
            Some(i)
                if !collapsed
                    && (app.show_client_tasks || app.roster[i].kind == ClientKind::Human) =>
            {
                app.tasks
                    .get(&app.roster[i].name)
                    .map(|ts| ts.iter().filter(|t| !t.done).cloned().collect())
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
        // Whose list the tasks belong to — used to make them selectable rows.
        let task_owner = group
            .client
            .and_then(|i| app.roster.get(i))
            .map(|a| a.name.clone())
            .unwrap_or_default();

        // Box height from the shared `group_content_rows` so it matches exactly
        // what the loop below renders (status + non-adopted items + tasks +
        // adopted children) — otherwise rows clip or the box gaps.
        let content = group_content_rows(app, group, &all_tasks, &all_items, box_inner_w);
        if content == 0 {
            continue;
        }
        // Full box height — content is rendered into the off-screen buffer in
        // full and scrolled, so no per-pane clipping here.
        let box_h = content + 2;
        let box_rect = Rect {
            x: inner.x + BOX_X,
            y: row_y,
            width: inner.width.saturating_sub(BOX_X),
            height: box_h,
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(SECONDARY_FG));
        let box_inner = block.inner(box_rect);
        block.render(box_rect, &mut cbuf);

        let mut off: u16 = 0; // row within the box

        // 1. Status — italic, wrapped, at the top of the box.
        for sl in &status_lines {
            if off >= box_inner.height {
                break;
            }
            let mut st = Style::default()
                .fg(Color::Rgb(180, 180, 180))
                .add_modifier(Modifier::ITALIC);
            if stale {
                st = st.add_modifier(Modifier::DIM);
            }
            Paragraph::new(Line::from(Span::styled(sl.clone(), st))).render(
                Rect {
                    x: box_inner.x,
                    y: box_inner.y + off,
                    width: box_inner.width,
                    height: 1,
                },
                &mut cbuf,
            );
            off += 1;
        }

        // 2. Work-items below the status (hidden when folded). Items a task has
        // adopted render under their task in step 3, so skip them here.
        if !collapsed {
            for w in group
                .items
                .iter()
                .filter(|w| !all_tasks.iter().any(|t| task_on_item(t, w)))
            {
                if off >= box_inner.height {
                    break;
                }
                let iy = box_inner.y + off;
                let item_x = box_inner.x;
                let item_w = box_inner.width;
                let meta_issue = w
                    .issue
                    .and_then(|n| app.titles.get(&(RefKind::Issue, n)).cloned().flatten());
                let meta_pr =
                    w.pr.and_then(|n| app.titles.get(&(RefKind::Pr, n)).cloned().flatten());
                let (line, refs) = build_work_item_line(
                    w,
                    meta_issue.as_ref(),
                    meta_pr.as_ref(),
                    item_w,
                    item_x,
                    iy,
                    None,
                );
                let mut style = Style::default();
                if matches!(
                    w.state,
                    crate::work::WorkState::Done | crate::work::WorkState::Merged
                ) {
                    style = style.add_modifier(Modifier::DIM);
                }
                if focused && rows.len() == selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Paragraph::new(line).style(style).render(
                    Rect {
                        x: item_x,
                        y: iy,
                        width: item_w,
                        height: 1,
                    },
                    &mut cbuf,
                );
                for (col_start, col_end, url) in refs {
                    click_local.push((iy, item_x + col_start, item_x + col_end, url));
                }
                rows.push(TreeRow::Item {
                    repo: w.repo.clone(),
                    pr: w.pr,
                    issue: w.issue,
                });
                row_ys.push(iy);
                off += 1;
            }
        }

        // 3. Tasks — each rendered flat under the client. A task linked to a
        // board issue/PR adopts that work-item as an indented child row beneath
        // it (the "what this client is working on" view); the work-item may live
        // in another repo. Tasks with no live work-item just render on their own.
        for t in &open_tasks {
            if off >= box_inner.height {
                break;
            }
            let task_sel = focused && rows.len() == selected;
            rows.push(TreeRow::Task {
                owner: task_owner.clone(),
                id: t.id.clone(),
                text: t.text.clone(),
            });
            row_ys.push(box_inner.y + off);
            let st = if task_sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Paragraph::new(task_line(t, stale, false, box_inner.width))
                .style(st)
                .render(
                    Rect {
                        x: box_inner.x,
                        y: box_inner.y + off,
                        width: box_inner.width,
                        height: 1,
                    },
                    &mut cbuf,
                );
            off += 1;

            // The board issue/PR this task is linked to, as an indented child.
            if let Some(w) = all_items.iter().find(|w| task_on_item(t, w)) {
                if off >= box_inner.height {
                    break;
                }
                let iy = box_inner.y + off;
                let indent = 4u16; // aligns the child under the task's "☐ "
                let meta_issue = w
                    .issue
                    .and_then(|n| app.titles.get(&(RefKind::Issue, n)).cloned().flatten());
                let meta_pr =
                    w.pr.and_then(|n| app.titles.get(&(RefKind::Pr, n)).cloned().flatten());
                let (line, refs) = build_work_item_line(
                    w,
                    meta_issue.as_ref(),
                    meta_pr.as_ref(),
                    box_inner.width.saturating_sub(indent),
                    box_inner.x + indent,
                    iy,
                    None,
                );
                let mut spans = line.spans;
                spans.insert(0, Span::styled("  ↳ ", Style::default().fg(SECONDARY_FG)));
                let mut style = Style::default();
                if matches!(
                    w.state,
                    crate::work::WorkState::Done | crate::work::WorkState::Merged
                ) {
                    style = style.add_modifier(Modifier::DIM);
                }
                if focused && rows.len() == selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Paragraph::new(Line::from(spans)).style(style).render(
                    Rect {
                        x: box_inner.x,
                        y: iy,
                        width: box_inner.width,
                        height: 1,
                    },
                    &mut cbuf,
                );
                for (col_start, col_end, url) in refs {
                    click_local.push((
                        iy,
                        box_inner.x + indent + col_start,
                        box_inner.x + indent + col_end,
                        url,
                    ));
                }
                rows.push(TreeRow::Item {
                    repo: w.repo.clone(),
                    pr: w.pr,
                    issue: w.issue,
                });
                row_ys.push(iy);
                off += 1;
            }
        }

        row_y += box_h;
    }

    // Publish the selectable rows for the key handlers, and keep the selection
    // in range against what we just rendered.
    app.tree_rows = rows;
    // A pending action asked the cursor to follow a client that just moved
    // sections — re-point the selection at its new row.
    if let Some(name) = app.reselect_client.take() {
        if let Some(pos) = app.tree_rows.iter().position(|r| {
            matches!(r, TreeRow::Client(i) if app.roster.get(*i).map(|a| a.name == name).unwrap_or(false))
        }) {
            app.tree_selected = pos;
        }
    }
    if let Some(id) = app.reselect_project.take() {
        if let Some(pos) = app.tree_rows.iter().position(|r| {
            matches!(r, TreeRow::Project(i) if app.projects.get(*i).map(|p| p.id == id).unwrap_or(false))
        }) {
            app.tree_selected = pos;
        }
    }
    if app.tree_selected >= app.tree_rows.len() {
        app.tree_selected = app.tree_rows.len().saturating_sub(1);
    }

    // Scroll the off-screen content so the selected row stays visible, then blit
    // the visible window into the pane. Selection-following: walking the tree
    // with j/k scrolls the dashboard once it overflows.
    let sel_y = row_ys.get(app.tree_selected).copied().unwrap_or(0);
    let max_scroll = content_h.saturating_sub(avail);
    let scroll = if sel_y >= avail {
        (sel_y + 1 - avail).min(max_scroll)
    } else {
        0
    };

    {
        let fbuf = frame.buffer_mut();
        for sy in 0..avail {
            let cy = scroll + sy;
            if cy >= cbuf_h {
                break;
            }
            for sx in 0..inner.width {
                if let Some(src) = cbuf.cell((inner.x + sx, cy)).cloned() {
                    if let Some(dst) = fbuf.cell_mut((inner.x + sx, inner.y + sy)) {
                        *dst = src;
                    }
                }
            }
        }
    }

    // Translate the collected click targets (role / PR-issue links) to screen
    // rows, keeping only those currently visible.
    for (cy, col_start, col_end, url) in click_local {
        if cy >= scroll && cy < scroll + avail {
            app.click_targets.push(ClickTarget {
                row: inner.y + cy - scroll,
                col_start,
                col_end,
                url,
            });
        }
    }

    // Hover highlight: light up (REVERSED) the click target under the cursor,
    // using the SAME screen-space `click_targets` as clicks — so the highlight
    // can never drift from the actual hotspot. This pane renders into an
    // off-screen buffer at content coordinates, so per-row hover can't be
    // resolved during the build (the screen row isn't known until the blit);
    // doing it here, post-blit, keeps hover and click in lock-step.
    if let Some((hr, hc)) = app.hover_pos {
        let fb = frame.buffer_mut();
        for t in &app.click_targets {
            if t.row == hr && hc >= t.col_start && hc < t.col_end {
                for cx in t.col_start..t.col_end {
                    if let Some(cell) = fb.cell_mut((cx, t.row)) {
                        cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                    }
                }
            }
        }
    }

    // Scrollbar on the right border when the tree overflows the pane — a
    // proportional thumb with ↑/↓ end arrows, matching the help/inbox panes.
    // The position tracks the blit offset; `max_scroll + 1` is the number of
    // distinct scroll positions (see `scroll` above).
    if max_scroll > 0 {
        let mut sb = ScrollbarState::new(max_scroll as usize + 1).position(scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
            area,
            &mut sb,
        );
    }
}

/// Spans for one agent row. `stale`/selection styling is applied by the
/// caller as a row-level `Style`, so this only carries per-span colors.
/// GitHub URL for an agent's `repo` ("owner/name"), or `None` for a bare /
/// placeholder repo (e.g. the human supervisor's "-"). Makes the role a link.
pub(crate) fn repo_url(repo: &str) -> Option<String> {
    let r = repo.trim();
    let (owner, name) = r.split_once('/')?;
    // Skip fleet-wide globs (e.g. acme/*, a code-review agent) and
    // non-GitHub owners (local/…) — they don't resolve to a real repo URL.
    if owner.is_empty() || name.is_empty() || owner == "local" || name.contains(['*', '?']) {
        return None;
    }
    Some(format!("https://github.com/{r}"))
}

/// Build an agent's row. Returns the line plus, when the role chip is a
/// clickable repo link, its column range relative to the line start
/// `(col_start, width)` so the caller can register a click target.
// All params are per-row render inputs (position, hover, link/task state);
// bundling them into a struct adds more churn than it removes here.
#[allow(clippy::too_many_arguments)]
fn build_agent_line(
    a: &Agent,
    tick: usize,
    marker: char,
    is_leader: bool,
    row_abs: u16,
    x_abs: u16,
    hover: Option<(u16, u16)>,
    repo_linkable: bool,
    open_tasks: usize,
) -> (Line<'_>, Option<(u16, u16)>) {
    // Leading collapse marker (▸/▾, or a space when the client has no items),
    // then the client. The human supervisor renders distinctly (◆ + "you"),
    // no spinner, no stale dim.
    let marker_span = Span::styled(format!("{marker} "), Style::default().fg(SECONDARY_FG));
    if a.kind == ClientKind::Human {
        let mut spans = vec![
            marker_span,
            Span::styled(
                " ◆ ",
                Style::default()
                    .fg(Color::Rgb(120, 200, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<14}", clip(&a.name, 14)),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{:<24}", "you (supervisor)"),
                Style::default().fg(Color::Rgb(120, 200, 255)),
            ),
        ];
        if a.unread > 0 {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("✉ {}", a.unread),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if open_tasks > 0 {
            spans.push(Span::raw("  "));
            spans.push(task_badge(open_tasks));
        }
        return (Line::from(spans), None);
    }

    // Name styling. A group leader (its project's coordinator) reads in orange
    // with a trailing ★ (the star lives inside the 14-col field so the role
    // column still lines up). An idle client recedes — soft grey, no bold — so
    // the working / blocked / waiting agents are the ones that catch the eye.
    // Everyone else is bright white + bold.
    let name_field = if is_leader {
        format!("{:<14}", format!("{} ★", clip(&a.name, 12)))
    } else {
        format!("{:<14}", clip(&a.name, 14))
    };
    let name_style = if is_leader {
        Style::default()
            .fg(Color::Rgb(220, 130, 40))
            .add_modifier(Modifier::BOLD)
    } else if a.state == AgentState::Idle {
        Style::default().fg(Color::Rgb(170, 170, 170))
    } else {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    };
    let mut spans = vec![
        marker_span,
        state_indicator(a.state, a.stale, tick),
        Span::styled(name_field, name_style),
        Span::raw(" "),
    ];
    // Role chip — the agent's function, distinct from its name. Soft blue so it
    // reads as a label. When the agent has a GitHub repo, the role is a link
    // (underlined) you can click to open the repo. Split text/padding so only the
    // text underlines + falls in the click region, and the trailing age/✉ columns
    // stay aligned. The animated dot conveys state, so no [working]/[idle] label.
    let role_text = clip(a.role.as_deref().unwrap_or(""), 15);
    let role_w = role_text.chars().count() as u16;
    let clickable = role_w > 0 && repo_linkable;
    let role_col: u16 = spans.iter().map(|s| s.content.chars().count() as u16).sum();
    // Hovering the clickable role lights up its background (REVERSED), the same
    // affordance PR/issue links use.
    let role_abs = x_abs + role_col;
    let role_hovered = clickable
        && hover
            .map(|(hr, hc)| hr == row_abs && hc >= role_abs && hc < role_abs + role_w)
            .unwrap_or(false);
    let mut role_style = Style::default().fg(Color::Rgb(130, 180, 210));
    if clickable {
        role_style = role_style.add_modifier(Modifier::UNDERLINED);
    }
    if role_hovered {
        role_style = role_style.add_modifier(Modifier::REVERSED);
    }
    spans.push(Span::styled(role_text, role_style));
    let pad = 15u16.saturating_sub(role_w);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad as usize)));
    }
    let role_hit = clickable.then_some((role_col, role_w));
    // Age first, so the time column lines up across every client row; the
    // unread badge trails it (optional, so keeping it last avoids shoving the
    // time column around). The current task renders on its own italic line
    // beneath the node (see `render_agents_pane`), not inline here.
    if let Some(ts) = a.last_seen {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            age_string(ts),
            Style::default().fg(SECONDARY_FG),
        ));
    }
    if a.unread > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("✉ {}", a.unread),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Open-task count — a glanceable "this client has N reminders" chip, trailing
    // the unread badge (same placement logic). The full list is the `T` modal.
    if open_tasks > 0 {
        spans.push(Span::raw("  "));
        spans.push(task_badge(open_tasks));
    }
    (Line::from(spans), role_hit)
}

/// The open-task count chip (`☐ N`) shown on a client row — soft teal so it
/// reads as a secondary marker next to the yellow unread badge.
fn task_badge(open: usize) -> Span<'static> {
    Span::styled(
        format!("☐ {open}"),
        Style::default().fg(Color::Rgb(150, 200, 180)),
    )
}

/// Does task `t` hang off work-item `w`? (Same PR number, compatible repo.)
fn task_on_item(t: &crate::store::Task, w: &crate::work::WorkItem) -> bool {
    match (&t.pr, w.pr) {
        (Some(p), Some(n)) => p.number == n && pr_repo_matches(&p.repo, &w.repo),
        _ => false,
    }
}

/// Lenient repo match: tasks store `owner/name`, but a work-item's repo may be a
/// bare short name or a full slug — treat them equal if either equals the other
/// or their basenames match.
fn pr_repo_matches(task_repo: &str, item_repo: &str) -> bool {
    if task_repo == item_repo {
        return true;
    }
    fn base(s: &str) -> &str {
        s.rsplit('/').next().unwrap_or(s)
    }
    base(task_repo) == base(item_repo)
}

/// One clipped line for a task in the agents pane — the title only, so the
/// dashboard stays compact (the full title + body live in the tasks modal).
/// `child` nests it under its PR's work-item row (`  ↳ ☐ …`); otherwise a flat
/// checklist line (`☐ …`). A long title is clipped with an ellipsis.
fn task_line(t: &crate::store::Task, stale: bool, child: bool, max_w: u16) -> Line<'static> {
    let (box_style, text_style) = task_styles(stale);
    let text_col: usize = if child { 6 } else { 2 }; // "  ↳ ☐ " | "☐ "
    let usable = (max_w as usize).saturating_sub(text_col).max(1);
    let mut spans: Vec<Span> = Vec::new();
    if child {
        spans.push(Span::styled("  ↳ ", Style::default().fg(SECONDARY_FG)));
    }
    spans.push(Span::styled("☐ ", box_style));
    spans.push(Span::styled(clip(&t.text, usable), text_style));
    Line::from(spans)
}

/// Shared (checkbox, text) styles for a task line, dimmed when the client is stale.
fn task_styles(stale: bool) -> (Style, Style) {
    let mut box_style = Style::default().fg(Color::Rgb(150, 200, 180));
    let mut text_style = Style::default().fg(Color::Rgb(165, 175, 170));
    if stale {
        box_style = box_style.add_modifier(Modifier::DIM);
        text_style = text_style.add_modifier(Modifier::DIM);
    }
    (box_style, text_style)
}

/// State indicator. **Motion** encodes the everyday states so they read at a
/// glance; the attention states use a self-evident symbol instead of a blink:
///   - `working` — a braille spinner (active / in motion)
///   - `idle`    — a steady dot (at rest)
///   - `blocked` — a bold ⚠ (needs the supervisor; mirrors the status-bar alert)
///   - `waiting` — a steady amber ◐ (stuck on a peer/external, not on you)
///   - stale     — a dim-grey hollow dot ○ (we aren't hearing from it)
/// Every glyph is a width-1 char in a 3-cell " X " slot, so the name column
/// stays aligned across all states.
/// Driven by `app.tick` (80 ms), the same clock as the status-bar spinner.
fn state_indicator(state: AgentState, stale: bool, tick: usize) -> Span<'static> {
    let color = state.color();
    if stale {
        // Static dim-grey hollow dot — colorable (unlike a color emoji), aligns
        // with the other dots, and reads as "dormant, no signal".
        return Span::styled(
            " ○ ".to_string(),
            Style::default()
                .fg(SECONDARY_FG)
                .add_modifier(Modifier::DIM),
        );
    }
    match state {
        AgentState::Working => {
            let f = SPINNER_FRAMES[tick % SPINNER_FRAMES.len()];
            Span::styled(
                format!(" {f} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )
        }
        // Bold warning sign — the same ⚠ as the status-bar "N blocked" alert.
        // Static: the alert + top-of-tree sort carry the attention, so the row
        // just needs to be unmistakably "stuck, needs you".
        AgentState::Blocked => Span::styled(
            " ⚠ ".to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        // Half-filled amber dot — "stuck, but waiting on a peer, not on you".
        // Steady (not alarming) and a distinct fill from idle ● / stale ○.
        AgentState::Waiting => Span::styled(" ◐ ".to_string(), Style::default().fg(color)),
        AgentState::Idle => Span::styled(" ● ".to_string(), Style::default().fg(color)),
        AgentState::Unknown => Span::styled(
            " · ".to_string(),
            Style::default().fg(color).add_modifier(Modifier::DIM),
        ),
    }
}

/// Truncate `s` to at most `max` chars, appending an ellipsis when cut.
/// Result is always `<= max` chars so callers can pad with `{:<max$}`.
fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else if max <= 1 {
        "…".to_string()
    } else {
        let keep: String = s.chars().take(max - 1).collect();
        format!("{keep}…")
    }
}

/// Greedy word-wrap `text` to `width` columns (char-based). Words longer than
/// `width` are hard-broken. Always returns at least one line. Both `render`
/// (for height) and `render_agents_pane` (for drawing) call this, so the line
/// count stays consistent.
fn wrap_text(text: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if word.chars().count() > width {
            // Hard-break a word that can't fit on one line.
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut chunk = String::new();
            for ch in word.chars() {
                if chunk.chars().count() == width {
                    lines.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            cur = chunk;
            continue;
        }
        let cur_len = cur.chars().count();
        let need = if cur_len == 0 {
            word.chars().count()
        } else {
            cur_len + 1 + word.chars().count()
        };
        if need > width {
            lines.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Style applied to a clickable `#N` token when the mouse is hovering
/// over it. Brighter color + reverse-video to give clear visual
/// feedback that "this is what would open if you click."
fn hover_ref_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED | Modifier::REVERSED)
}

fn normal_ref_style() -> Style {
    Style::default()
        .fg(Color::LightBlue)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}

/// Is the cell at `row, col_start..col_end` currently hovered?
fn is_hovered(hover: Option<(u16, u16)>, row: u16, col_start: u16, col_end: u16) -> bool {
    hover
        .map(|(r, c)| r == row && c >= col_start && c < col_end)
        .unwrap_or(false)
}

/// True when gh tells us this item is done — PR merged, or either side closed.
/// Used to push completed items to the bottom of the work-items pane so
/// active work isn't visually crowded by recently-finished cards. Source of
/// truth is gh, not the event log: the agent doesn't merge, the supervisor does.
fn is_meta_done(app: &App, w: &WorkItem) -> bool {
    let issue_closed = w
        .issue
        .and_then(|i| app.titles.get(&(RefKind::Issue, i)))
        .and_then(|m| m.as_ref())
        .map(|m| m.closed)
        .unwrap_or(false);
    let pr_done =
        w.pr.and_then(|p| app.titles.get(&(RefKind::Pr, p)))
            .and_then(|m| m.as_ref())
            .map(|m| m.merged || m.closed)
            .unwrap_or(false);
    issue_closed || pr_done
}

/// True for items the user shouldn't see at all: the issue is closed
/// AND no PR was ever linked to it in the log. That pattern is noise —
/// abandoned issues, duplicate-marks, or pre-state-guard test emissions
/// against already-closed issues. Items WITH a PR stay visible regardless
/// of issue state, so recently-merged work (#90 → #98) keeps its
/// "Merged" row at the bottom as a recent-success record.
fn is_closed_no_pr(app: &App, w: &WorkItem) -> bool {
    if w.pr.is_some() {
        return false;
    }
    let Some(issue) = w.issue else {
        return false;
    };
    app.titles
        .get(&(RefKind::Issue, issue))
        .and_then(|m| m.as_ref())
        .map(|m| m.closed)
        .unwrap_or(false)
}

/// Items to show in the work-items pane. Filters:
///   - closed-no-pr noise (abandoned/test issues),
///   - acknowledged Done items (supervisor dismissed via `c` in the
///     story modal — persists across restarts via `card-acknowledged`
///     in the audit log).
/// Done items that are NOT acknowledged stay visible at the bottom,
/// below a "── Done ──" divider (rendered by `render_work_items`).
///
/// Returns owned clones so the immutable borrow on `app` ends at the
/// call site — the render loop needs `&mut app.click_targets` while
/// iterating, and tying the result to `&app` blocks that.
fn visible_work_items(app: &App) -> Vec<WorkItem> {
    let mut items: Vec<WorkItem> = app
        .work
        .sorted()
        .into_iter()
        .filter(|w| !is_closed_no_pr(app, w))
        .filter(|w| {
            // `card-done` (state Done) auto-retires: the agent moved the board
            // card to Done, so it leaves the active view on its own. Other
            // finished items (Merged / gh-merged/closed, no card-done yet) stay
            // until dismissed with `x` (acknowledged) — or immediately if
            // ticketless (nothing to retire).
            let finished =
                matches!(w.state, crate::work::WorkState::Merged) || is_meta_done(app, w);
            // Retire from the active view when: the agent moved the card to Done;
            // a ticketless finished item (nothing left to retire it); or the
            // supervisor dismissed it with `x` / `c` (any item, any state).
            let retire = w.state == crate::work::WorkState::Done
                || (finished && w.issue.is_none())
                || app.acknowledged.contains(&w.key);
            !retire
        })
        .cloned()
        .collect();
    items.sort_by_key(|a| is_meta_done(app, a));
    items
}

fn render_work_items(frame: &mut Frame, area: Rect, app: &mut App) {
    let items = visible_work_items(app);

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " Work items ",
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if items.is_empty() {
        let placeholder = Paragraph::new(Line::from(Span::styled(
            "  (no work items yet — events tied to issue=#N / pr=#N will show here)",
            Style::default().fg(SECONDARY_FG),
        )));
        frame.render_widget(placeholder, inner);
        return;
    }

    // Walk items, inserting a "── Done ──" divider on the transition from
    // active to done, dimming Done rows, and reverse-highlighting the
    // selected Done row when in DoneReview mode. row_y advances per
    // rendered row (divider counts as one).
    let mut row_y = inner.y;
    let mut seen_done = false;
    let mut done_idx: usize = 0;
    let avail = inner.height as usize;
    let mut rendered: usize = 0;
    for w in items.iter() {
        if rendered >= avail {
            break;
        }
        let is_done = w.state == crate::work::WorkState::Done;
        if is_done && !seen_done {
            seen_done = true;
            let divider = build_done_divider(inner.width);
            frame.render_widget(
                Paragraph::new(divider),
                Rect {
                    x: inner.x,
                    y: row_y,
                    width: inner.width,
                    height: 1,
                },
            );
            row_y += 1;
            rendered += 1;
            if rendered >= avail {
                break;
            }
        }

        let meta_issue: Option<Meta> = w
            .issue
            .and_then(|i| app.titles.get(&(RefKind::Issue, i)).cloned().flatten());
        let meta_pr: Option<Meta> =
            w.pr.and_then(|p| app.titles.get(&(RefKind::Pr, p)).cloned().flatten());

        let max_width = inner.width;
        let (line, refs) = build_work_item_line(
            w,
            meta_issue.as_ref(),
            meta_pr.as_ref(),
            max_width,
            inner.x,
            row_y,
            app.hover_pos,
        );

        let mut row_style = Style::default();
        if is_done {
            row_style = row_style.add_modifier(Modifier::DIM);
        }
        let is_selected_done =
            is_done && app.focus_mode == FocusMode::DoneReview && done_idx == app.done_selected;
        if is_selected_done {
            row_style = row_style.add_modifier(Modifier::REVERSED);
        }

        let para = Paragraph::new(line).style(row_style);
        frame.render_widget(
            para,
            Rect {
                x: inner.x,
                y: row_y,
                width: inner.width,
                height: 1,
            },
        );

        for (col_start, col_end, url) in refs {
            app.click_targets.push(ClickTarget {
                row: row_y,
                col_start: inner.x + col_start,
                col_end: inner.x + col_end,
                url,
            });
        }

        if is_done {
            done_idx += 1;
        }
        row_y += 1;
        rendered += 1;
    }
}

/// Build the spans for one work-item row PLUS the (col_start, col_end, url)
/// triples for any `#N` references on that row. `max_width` is the inner
/// pane width; the title is truncated to fit. `pane_x` / `row_y` plus
/// `app` are used to compute hover state per token so we can style the
/// hovered ref distinctly.
fn build_work_item_line<'a>(
    w: &'a WorkItem,
    meta_issue: Option<&Meta>,
    meta_pr: Option<&Meta>,
    max_width: u16,
    pane_x: u16,
    row_y: u16,
    hover_pos: Option<(u16, u16)>,
) -> (Line<'a>, Vec<(u16, u16, String)>) {
    let mut spans = Vec::new();
    let mut targets: Vec<(u16, u16, String)> = Vec::new();
    let mut col: u16 = 0;

    // 1. State label (colored + bold). Not clickable — the #issue and PR
    // columns below carry the links. If gh says the PR is merged/closed, that
    // overrides the event-derived state (GitHub is the source of truth).
    let (label_text, label_color) = if meta_pr.map(|m| m.merged).unwrap_or(false) {
        ("Merged", Color::Rgb(80, 160, 80))
    } else if meta_pr.map(|m| m.closed).unwrap_or(false)
        || meta_issue.map(|m| m.closed).unwrap_or(false)
    {
        ("Closed", SECONDARY_FG)
    } else {
        (w.state.label(), w.state.color())
    };
    spans.push(Span::styled(
        format!("{label_text:<17}"),
        Style::default()
            .fg(label_color)
            .add_modifier(Modifier::BOLD),
    ));
    col += 17;

    // 2. Fixed-width ref columns so the title aligns across every row: the
    // issue (#N → issue) then the PR (PR #N → the PR). Each is clickable and
    // blank-padded when absent, so ticketed and ticketless rows line up.
    spans.push(Span::raw("  "));
    col += 2;
    const ISSUE_W: u16 = 5; // "#" + up to 4 digits
    if let Some(issue) = w.issue {
        let text = format!("#{issue}");
        let len = text.chars().count() as u16;
        let start = col;
        let hover = is_hovered(hover_pos, row_y, pane_x + start, pane_x + start + len);
        spans.push(Span::styled(
            text,
            if hover {
                hover_ref_style()
            } else {
                normal_ref_style()
            },
        ));
        targets.push((start, start + len, RefKind::Issue.url(&w.repo, issue)));
        let pad = ISSUE_W.saturating_sub(len);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad as usize)));
        }
        col += len.max(ISSUE_W);
    } else {
        spans.push(Span::raw(" ".repeat(ISSUE_W as usize)));
        col += ISSUE_W;
    }

    spans.push(Span::raw("  "));
    col += 2;
    const PR_W: u16 = 7; // "PR #" + up to 3 digits
    if let Some(pr) = w.pr {
        let text = format!("PR #{pr}");
        let len = text.chars().count() as u16;
        let start = col;
        let hover = is_hovered(hover_pos, row_y, pane_x + start, pane_x + start + len);
        spans.push(Span::styled(
            text,
            if hover {
                hover_ref_style()
            } else {
                normal_ref_style()
            },
        ));
        targets.push((start, start + len, RefKind::Pr.url(&w.repo, pr)));
        let pad = PR_W.saturating_sub(len);
        if pad > 0 {
            spans.push(Span::raw(" ".repeat(pad as usize)));
        }
        col += len.max(PR_W);
    } else {
        spans.push(Span::raw(" ".repeat(PR_W as usize)));
        col += PR_W;
    }

    // 4. Title (PR title preferred; fallback to issue title; both optional).
    let title = meta_pr
        .and_then(|m| m.title.as_deref())
        .or_else(|| meta_issue.and_then(|m| m.title.as_deref()));
    if let Some(t) = title {
        spans.push(Span::raw("  "));
        col += 2;
        let trimmed = truncate_to_remaining(t, &mut col, max_width.saturating_sub(20));
        spans.push(Span::styled(trimmed, Style::default().fg(Color::White)));
    }

    // 5. Last action tag.
    spans.push(Span::raw("  "));
    let tag = format!("[{}]", w.last_action);
    spans.push(Span::styled(tag, Style::default().fg(SECONDARY_FG)));

    // 6. Optional note (e.g. reason for Skipped, current= for Owned).
    if !w.note.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            w.note.clone(),
            Style::default().fg(Color::Yellow),
        ));
    }

    // 7. Age.
    spans.push(Span::raw("  "));
    let age = age_string(w.last_event);
    spans.push(Span::styled(age, Style::default().fg(SECONDARY_FG)));

    // After step 4 we no longer need to track `col`; it's tracked
    // through step 4 only because the title truncation depends on
    // remaining width. The unused `col` after that is intentional.
    let _ = col;
    (Line::from(spans), targets)
}

/// Truncate `s` so the cumulative col doesn't exceed `room`. Returns an
/// owned `String` for ratatui's `Span::styled` (it accepts `Cow<str>`).
fn truncate_to_remaining(s: &str, col: &mut u16, room: u16) -> String {
    let s_width = s.chars().count() as u16;
    if s_width <= room {
        *col += s_width;
        s.to_string()
    } else if room <= 1 {
        // Almost no space left; emit ellipsis only.
        *col += 1;
        "…".to_string()
    } else {
        let keep = (room - 1) as usize;
        let truncated: String = s.chars().take(keep).collect();
        *col += room;
        format!("{truncated}…")
    }
}

fn age_string(then: chrono::DateTime<chrono::FixedOffset>) -> String {
    let now = chrono::Local::now().fixed_offset();
    let delta = now.signed_duration_since(then);
    let mins = delta.num_minutes();
    if mins < 1 {
        "just now".to_string()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if mins < 60 * 24 {
        format!("{}h ago", mins / 60)
    } else {
        format!("{}d ago", mins / (60 * 24))
    }
}

fn render_event_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default().borders(Borders::TOP).title("");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Newest first in the displayed list. The underlying `events` Vec
    // stays chronological (older entries at lower indices) so other code
    // — work-item aggregation, etc. — keeps its time-ordered semantics.
    // We just flip the iteration here.
    let visible: Vec<&Event> = app
        .events
        .iter()
        .rev()
        .filter(|e| {
            app.skill_filter
                .as_ref()
                .map(|f| &e.skill == f)
                .unwrap_or(true)
        })
        .collect();

    if visible.is_empty() {
        return;
    }

    // Snap to newest when following live. Index 0 = newest in the
    // reversed display.
    if app.follow_newest {
        app.selected = 0;
    }

    let visible_rows = inner.height as usize;
    let total = visible.len();

    // Compute viewport so `selected` is always on-screen. Default top of
    // the viewport is `selected` itself; if that would push the viewport
    // past the end, shift up.
    let mut viewport_top = app.selected.saturating_sub(visible_rows / 4);
    if viewport_top + visible_rows > total {
        viewport_top = total.saturating_sub(visible_rows);
    }
    let end = (viewport_top + visible_rows).min(total);

    for (rel_idx, ev) in visible[viewport_top..end].iter().enumerate() {
        let row_y = inner.y + rel_idx as u16;
        let abs_idx = viewport_top + rel_idx;
        let is_selected = abs_idx == app.selected;

        if ev.action == "loop-start" {
            let line = render_loop_start_divider(ev);
            frame.render_widget(
                Paragraph::new(line),
                Rect {
                    x: inner.x,
                    y: row_y,
                    width: inner.width,
                    height: 1,
                },
            );
            continue;
        }

        let (line, refs) = build_event_line(ev, inner.x, row_y, is_selected, app);
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                x: inner.x,
                y: row_y,
                width: inner.width,
                height: 1,
            },
        );

        for (col_start, col_end, url) in refs {
            app.click_targets.push(ClickTarget {
                row: row_y,
                col_start,
                col_end,
                url,
            });
        }
    }
}

/// Build a single event row's spans and the list of clickable refs
/// (absolute column bounds in terminal cells) for hit-testing.
/// `pane_x` is the inner-pane left edge; `row_y` is the row's terminal
/// y; hover state lives in `app.hover_pos`.
fn build_event_line<'a>(
    e: &'a Event,
    pane_x: u16,
    row_y: u16,
    is_selected: bool,
    app: &App,
) -> (Line<'a>, Vec<(u16, u16, String)>) {
    let ts = e.timestamp.format("%H:%M").to_string();
    let mut spans = vec![
        Span::styled(format!("{ts} "), Style::default().fg(SECONDARY_FG)),
        Span::styled(pad_to(&e.skill, 18), Style::default().fg(e.skill_color())),
        Span::raw(" "),
        Span::styled(
            pad_to(&e.action, 22),
            Style::default()
                .fg(e.action_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];

    let mut targets: Vec<(u16, u16, String)> = Vec::new();
    let details = &e.details;
    let mut cursor = 0;
    for r in &e.refs {
        if r.start > cursor {
            spans.push(Span::raw(&details[cursor..r.start]));
        }
        let token = &details[r.start..r.start + r.len];
        let col_start_abs = pane_x + EVENT_PREFIX_WIDTH + r.start as u16;
        let col_end_abs = col_start_abs + r.len as u16;
        let hovered = is_hovered(app.hover_pos, row_y, col_start_abs, col_end_abs);
        spans.push(Span::styled(
            token,
            if hovered {
                hover_ref_style()
            } else {
                normal_ref_style()
            },
        ));
        targets.push((
            col_start_abs,
            col_end_abs,
            r.kind
                .url(&crate::app::resolve_repo(&e.repo, &app.roster), r.number),
        ));
        cursor = r.start + r.len;
    }
    if cursor < details.len() {
        spans.push(Span::raw(&details[cursor..]));
    }

    let mut line = Line::from(spans);
    if is_selected {
        // Subtle highlight on the selected row — distinct from hover.
        line = line.style(Style::default().bg(Color::Rgb(40, 40, 40)));
    }
    (line, targets)
}

/// `loop-start` events render as a section divider. Used to visually
/// separate iterations of the agent loop in a busy stream.
fn render_loop_start_divider(e: &Event) -> Line<'_> {
    let ts = e.timestamp.format("%H:%M").to_string();
    // Best-effort: read the `issue=#N` from refs.
    let issue_label = e
        .refs
        .iter()
        .find(|r| r.kind == RefKind::Issue)
        .map(|r| format!("issue #{}", r.number))
        .unwrap_or_else(|| "new loop".to_string());
    Line::from(vec![
        Span::styled("─── ", Style::default().fg(Color::Cyan)),
        Span::styled(
            "Loop start: ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            issue_label,
            Style::default()
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ),
        Span::styled(format!("  {ts}  "), Style::default().fg(SECONDARY_FG)),
        Span::styled(
            "──────────────────────────────────────────────────────",
            Style::default().fg(Color::Cyan),
        ),
    ])
}

fn render_help_overlay(frame: &mut Frame, app: &mut App) {
    let area = centered_rect(80, 85, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);
    let lines = vec![
        Line::from(vec![Span::styled(
            "lakitu",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(""),
        Line::from("Live feed of agent activity. Tails ~/.claude/logs/agent-actions.log."),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Clients pane",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  Agents + you: state dot · name · role · ✉ unread · ☐ open tasks · age."),
        Line::from(
            "  Dot: spins=working, ⚠=needs you, ◐=waiting on a peer, ●=idle, ○=stale. ◆ = you.",
        ),
        Line::from("  Below the node: the agent's current task in italics (what it's working on)."),
        Line::from(
            "  Each client's owned work-items (matched by repo) nest beneath it; ▾/▸ folds.",
        ),
        Line::from(
            "  Open tasks show as a checklist in the box — PR-linked ones nest under their PR row.",
        ),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Work items",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  Each row: state · #issue · PR #N · title · [last action] · age — the #issue"),
        Line::from("  and PR #N are clickable. They live in each client's box now; the standalone"),
        Line::from("  by-state pane (and its `d` Done-review) is hidden for now."),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Event stream",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from(
            "  Raw agent activity — chronological, color-coded. Hidden by default; press l.",
        ),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Keys",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  ↑/↓ or j/k    scroll"),
        Line::from("  Home/End      jump to first / last event"),
        Line::from("  s             cycle skill filter (all → board-issue-loop → pr-review-fixup)"),
        Line::from("  l             show / hide the event stream (hidden by default)"),
        Line::from("  Tab           hide / show agent tasks in the pane (your own always show)"),
        Line::from("  o             open the selected event's first #N reference in browser"),
        Line::from("  Click on #N   open in browser (mouse)"),
        Line::from("  Scroll wheel  scroll the focused pane / open modal (mouse)"),
        Line::from("  a             focus the Clients pane"),
        Line::from("  c             compose a message (to a client, or everyone)"),
        Line::from("  w             toggle the inbox-waker (wake stopped agents on new mail)"),
        Line::from("  ?             toggle this help (j/k or ↑/↓ scroll it · esc closes)"),
        Line::from("  q             quit"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "In the Clients pane",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  ↑/↓ or j/k    move between clients, their work-items, and tasks"),
        Line::from("  PgUp/PgDn     jump ~10 rows — the pane scrolls to follow the cursor"),
        Line::from(
            "  enter         client: inbox · ticket: open the PR · task: open the tasks list",
        ),
        Line::from("  shift+enter   on a ticket: open the issue (the board ticket)"),
        Line::from("  x             dismiss the selected work-item from the board (persists)"),
        Line::from("  space         fold / unfold the selected client's work-items"),
        Line::from("  t             open the tasks list for the selected client"),
        Line::from("  m             move a client between Floating and projects"),
        Line::from("  *             promote the selected client to group leader (coordinator)"),
        Line::from("  P / r / X     new project · rename a project row · remove a project"),
        Line::from("  c             compose about the selected row (a PR or task → its owner)"),
        Line::from("  D             disconnect the selected client (asks to confirm)"),
        Line::from("  esc           leave the Clients pane"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "In the inbox modal",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  ↑/↓ or j/k    select a message — viewing it marks it read (your own inbox)"),
        Line::from("  r             reply to the selected message"),
        Line::from("  t             turn the selected message into a task (keeps its source)"),
        Line::from("  DEL           delete the selected message (asks to confirm)"),
        Line::from("  esc           close the inbox"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "In the tasks modal",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  ↑/↓ or j/k    select a task (grouped under their PR; loose ones below)"),
        Line::from("  space         toggle the selected task done / open"),
        Line::from("  a             add a task (type it · enter saves · esc cancels)"),
        Line::from("  d             drop the selected task"),
        Line::from("  esc           close"),
        Line::from("  (the detail pane shows the selected task's full text + its message)"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "In the compose modal",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  Tab           move between recipient / title / message"),
        Line::from("  ←/→           change recipient (a client, or everyone)"),
        Line::from("  enter         send (when on the message field)"),
        Line::from("  shift+enter   newline in the message (terminal permitting)"),
        Line::from("  esc           cancel"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "In Done-review mode",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  ↑/↓ or j/k    move between Done items"),
        Line::from("  enter         open the story modal for the selected Done item"),
        Line::from("  esc           leave Done-review, return focus to events"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "In the story modal",
            Style::default().add_modifier(Modifier::BOLD),
        )]),
        Line::from("  c             close & dismiss (emits card-acknowledged; persists)"),
        Line::from("  o             open the PR (or issue if no PR) in browser"),
        Line::from("  esc           back to Done review without dismissing"),
        Line::from(""),
        Line::from(vec![Span::styled(
            "Click any #N to open its GitHub issue / PR.",
            Style::default().fg(SECONDARY_FG),
        )]),
    ];
    let block = Block::default().borders(Borders::ALL).title(" Help ");
    let inner = block.inner(area);
    // Estimate the wrapped height (ratatui's own `line_count` is private): each
    // line takes ceil(width / inner_width) rows, +1 slack for word-wrap so the
    // last line stays reachable.
    let iw = inner.width.max(1) as usize;
    let total: u16 = lines
        .iter()
        .map(|line| {
            let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            if w <= iw {
                1u16
            } else {
                (w.div_ceil(iw) + 1) as u16
            }
        })
        .sum();
    let max_scroll = total.saturating_sub(inner.height);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let para = Paragraph::new(lines)
        .block(block)
        .style(Style::default().bg(Color::Reset))
        .wrap(Wrap { trim: false })
        .scroll((app.help_scroll, 0));
    frame.render_widget(para, area);
    if max_scroll > 0 {
        let mut sb =
            ScrollbarState::new(max_scroll as usize + 1).position(app.help_scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
            area,
            &mut sb,
        );
    }
}

/// One-row "── Done ──" separator between active work items and the
/// dimmed Done section underneath. Width is the inner pane width so the
/// line stretches; the dashes are SECONDARY_FG (same gray as other
/// chrome).
fn build_done_divider(width: u16) -> Line<'static> {
    let label = " Done ";
    let dashes_total = (width as usize).saturating_sub(label.len() + 4);
    let left = "── ";
    let right_dashes = dashes_total.saturating_sub(left.len());
    let right: String = std::iter::repeat_n('─', right_dashes).collect();
    Line::from(vec![
        Span::styled(left, Style::default().fg(SECONDARY_FG)),
        Span::styled(
            label.trim(),
            Style::default()
                .fg(SECONDARY_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(right, Style::default().fg(SECONDARY_FG)),
    ])
}

/// Inbox modal (#5/#6) — opened from the Agents pane via Enter. Master /
/// detail: the message list (newest first) is on top, the selected
/// message's full body below. Owns keyboard input while open (j/k select,
/// esc closes — see `handle_input` in `app.rs`).
fn render_inbox_modal(frame: &mut Frame, app: &mut App, name: &str) {
    let area = centered_rect(75, 75, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);

    // Cloned so the rest of the function can mutate `app` (the body scroll
    // offset) without holding an immutable borrow of the inbox.
    let msgs = app.open_inbox().to_vec();
    let unread = msgs.iter().filter(|m| !m.read).count();
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(
            " Inbox — {name}  ({} message{}, {unread} unread) ",
            msgs.len(),
            if msgs.len() == 1 { "" } else { "s" }
        ),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Identity / capability header (who this agent is + what peers can ask
    // it for), then the message area, then the footer.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(inner);
    render_agent_header(frame, outer[0], app.roster.iter().find(|a| a.name == name));

    if msgs.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  (inbox empty)",
                Style::default().fg(SECONDARY_FG),
            ))),
            outer[1],
        );
        render_inbox_footer(frame, outer[2], app.inbox_delete_armed);
        return;
    }

    // Body: message list (top), detail (bottom).
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Min(3)])
        .split(outer[1]);

    let sel = app.inbox_selected.min(msgs.len().saturating_sub(1));

    // Message ids that have already been turned into a task (via `t`), so the
    // list can mark them — visible confirmation that the action landed.
    let tasked: std::collections::HashSet<String> = app
        .tasks
        .get(name)
        .map(|ts| ts.iter().filter_map(|t| t.from_msg.clone()).collect())
        .unwrap_or_default();

    // --- message list ---
    let list_area = parts[0];
    let avail = list_area.height as usize;
    // Reserve the right column for a scrollbar when there are more messages than
    // fit, so rows don't render under it.
    let overflow = msgs.len() > avail;
    let row_w = if overflow {
        list_area.width.saturating_sub(1)
    } else {
        list_area.width
    };
    let top = if avail > 0 && sel >= avail {
        sel + 1 - avail
    } else {
        0
    };
    for (i, m) in msgs.iter().enumerate().skip(top).take(avail) {
        let row_y = list_area.y + (i - top) as u16;
        let ts = m
            .time
            .map(|t| t.format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "  --   ".to_string());
        let title_style = if m.read {
            Style::default().fg(Color::White)
        } else {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        };
        let is_tasked = tasked.contains(&m.id);
        let mut spans = vec![
            Span::styled(
                if m.read { "  " } else { "• " },
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(format!("{ts}  "), Style::default().fg(SECONDARY_FG)),
            Span::styled(
                format!("{:<14}", clip(&m.from, 14)),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  "),
            Span::styled(
                clip(
                    &m.title,
                    row_w.saturating_sub(if is_tasked { 30 } else { 28 }) as usize,
                ),
                title_style,
            ),
        ];
        // A teal ☐ marks a message you've already turned into a task.
        if is_tasked {
            spans.push(Span::styled(
                " ☐",
                Style::default().fg(Color::Rgb(150, 200, 180)),
            ));
        }
        let mut row_style = Style::default();
        if i == sel {
            row_style = row_style.add_modifier(Modifier::REVERSED);
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(row_style),
            Rect {
                x: list_area.x,
                y: row_y,
                width: row_w,
                height: 1,
            },
        );
    }
    if overflow {
        let mut sb = ScrollbarState::new(msgs.len()).position(sel);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(Some("↑"))
                .end_symbol(Some("↓")),
            list_area,
            &mut sb,
        );
    }

    // --- detail of the selected message ---
    if let Some(m) = msgs.get(sel) {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("From: ", Style::default().fg(SECONDARY_FG)),
            Span::styled(
                m.from.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("     "),
            Span::styled("Time: ", Style::default().fg(SECONDARY_FG)),
            Span::styled(
                m.time
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "(unknown)".to_string()),
                Style::default().fg(SECONDARY_FG),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            m.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        for l in m.body.lines() {
            lines.push(Line::from(Span::raw(l.to_string())));
        }
        // Scrollable: estimate the wrapped height (ratatui's `line_count` is
        // private), clamp the offset, and show a scrollbar when the body
        // overflows. Reserve the right column for the bar; the TOP border row.
        let detail = parts[1];
        let text_area = Rect {
            width: detail.width.saturating_sub(1),
            ..detail
        };
        let visible = detail.height.saturating_sub(1);
        let iw = text_area.width.max(1) as usize;
        let total: u16 = lines
            .iter()
            .map(|line| {
                let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                if w <= iw {
                    1
                } else {
                    (w.div_ceil(iw) + 1) as u16
                }
            })
            .sum();
        let max_scroll = total.saturating_sub(visible);
        app.inbox_scroll = app.inbox_scroll.min(max_scroll);
        let para = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::TOP))
            .scroll((app.inbox_scroll, 0));
        frame.render_widget(para, text_area);
        if max_scroll > 0 {
            let mut sb =
                ScrollbarState::new(max_scroll as usize + 1).position(app.inbox_scroll as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("↑"))
                    .end_symbol(Some("↓")),
                detail,
                &mut sb,
            );
        }
    }

    // --- footer ---
    render_inbox_footer(frame, outer[2], app.inbox_delete_armed);
}

/// Identity + capability header for the inbox modal: repo · board · state on
/// one line, the self-authored "Helps with:" capability blurb below. This is
/// the agent's stable bio (from the registry) — what peers can ask it to do.
fn render_agent_header(frame: &mut Frame, area: Rect, agent: Option<&Agent>) {
    let Some(a) = agent else { return };
    let mut id_line = vec![
        Span::styled("repo ", Style::default().fg(SECONDARY_FG)),
        Span::styled(a.repo.clone(), Style::default().fg(Color::White)),
        Span::styled("   board ", Style::default().fg(SECONDARY_FG)),
        Span::styled(a.board.clone(), Style::default().fg(Color::White)),
        Span::raw("   "),
        Span::styled(
            format!("[{}]", a.state.label()),
            Style::default()
                .fg(a.state.color())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if a.stale {
        id_line.push(Span::styled("  (stale)", Style::default().fg(SECONDARY_FG)));
    }
    let helps = a
        .description
        .clone()
        .unwrap_or_else(|| "(no description set)".to_string());
    let lines = vec![
        Line::from(id_line),
        Line::from(vec![
            Span::styled(
                "Helps with: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(helps, Style::default().fg(Color::White)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn render_inbox_footer(frame: &mut Frame, area: Rect, delete_armed: bool) {
    // A pending delete takes over the footer with a confirm prompt.
    if delete_armed {
        let warn = Style::default()
            .fg(Color::Rgb(220, 130, 40))
            .add_modifier(Modifier::BOLD);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Delete this message? ", warn),
                Span::styled("y", warn),
                Span::styled(
                    " to confirm · any other key cancels",
                    Style::default().fg(SECONDARY_FG),
                ),
            ])),
            area,
        );
        return;
    }
    let key = Style::default().fg(SECONDARY_FG);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("j/k", key),
            Span::styled(" select  ", key),
            Span::styled("PgUp/Dn", key),
            Span::styled(" page  ", key),
            Span::styled("space/b", key),
            Span::styled(" body  ", key),
            Span::styled("r", key),
            Span::styled(" reply  ", key),
            Span::styled("t", key),
            Span::styled(" →task  ", key),
            Span::styled("DEL", key),
            Span::styled(" delete  ", key),
            Span::styled("esc", key),
            Span::styled(" close", key),
        ])),
        area,
    );
}

/// Tasks modal (`T` on a selected client): the agent's reminder list, grouped
/// by PR (the "subtree under the PR") with a General group for loose tasks. The
/// supervisor can toggle, drop, and add tasks here. Owns keyboard while open.
fn render_tasks_modal(frame: &mut Frame, app: &mut App, name: &str) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);

    let tasks = app.tasks.get(name).cloned().unwrap_or_default();
    let open = tasks.iter().filter(|t| !t.done).count();
    let done = tasks.len() - open;
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" Tasks — {name}  ({open} open, {done} done) "),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // header (3) | list (min) | add-input (0/1) | footer (1)
    let input_h: u16 = if app.task_input.is_some() { 1 } else { 0 };
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .split(inner);
    render_agent_header(frame, outer[0], app.roster.iter().find(|a| a.name == name));

    // Selection + display order come from the shared helper, so the cursor lines
    // up with the key handler. Headers (PR group / General) are inserted at each
    // PR-key change — but only when there's at least one PR group; an all-loose
    // list renders as a clean flat checklist with no headers.
    let order = crate::store::task_display_order(&tasks);
    if app.tasks_selected >= order.len() {
        app.tasks_selected = order.len().saturating_sub(1);
    }
    let sel_task = order.get(app.tasks_selected).copied();
    let has_pr = tasks.iter().any(|t| t.pr.is_some());

    if order.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  (no tasks yet — press `a` to add one)",
                Style::default().fg(SECONDARY_FG),
            ))),
            outer[1],
        );
    } else {
        // List (top) + a full-text detail of the selected task (bottom), so long
        // text is fully readable — the inbox master/detail pattern.
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Min(3)])
            .split(outer[1]);
        let list_area = body[0];
        let textw = list_area.width.saturating_sub(1) as usize; // reserve scrollbar col
        let mut rows: Vec<(Line, bool)> = Vec::new();
        let mut sel_display = 0usize;
        let mut prev_key: Option<Option<(String, u64)>> = None;
        for &i in &order {
            let key = tasks[i].pr.as_ref().map(|p| (p.repo.clone(), p.number));
            if has_pr && prev_key.as_ref() != Some(&key) {
                let h = match &key {
                    Some((repo, num)) => format!("PR {repo}#{num}"),
                    None => "General".to_string(),
                };
                rows.push((
                    Line::from(Span::styled(
                        h,
                        Style::default()
                            .fg(Color::Rgb(150, 170, 255))
                            .add_modifier(Modifier::BOLD),
                    )),
                    false,
                ));
            }
            prev_key = Some(key);
            let selected = Some(i) == sel_task;
            if selected {
                sel_display = rows.len();
            }
            rows.push((task_modal_line(&tasks[i], textw), selected));
        }
        let n = rows.len();
        let avail = list_area.height as usize;
        let overflow = n > avail;
        let roww = if overflow {
            list_area.width.saturating_sub(1)
        } else {
            list_area.width
        };
        let top = if avail > 0 && sel_display >= avail {
            sel_display + 1 - avail
        } else {
            0
        };
        for (di, (line, selected)) in rows.into_iter().enumerate().skip(top).take(avail) {
            let st = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            frame.render_widget(
                Paragraph::new(line).style(st),
                Rect {
                    x: list_area.x,
                    y: list_area.y + (di - top) as u16,
                    width: roww,
                    height: 1,
                },
            );
        }
        if overflow {
            let mut sb = ScrollbarState::new(n).position(sel_display);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(Some("↑"))
                    .end_symbol(Some("↓")),
                list_area,
                &mut sb,
            );
        }

        // Detail of the selected task: its metadata, then the full text wrapped
        // so a long task is fully readable (the list row above is clipped).
        if let Some(i) = sel_task {
            let t = &tasks[i];
            let mut meta: Vec<Span> = vec![
                Span::styled(
                    if t.done { "[done]" } else { "[open]" },
                    if t.done {
                        Style::default().fg(SECONDARY_FG)
                    } else {
                        Style::default().fg(Color::Rgb(150, 200, 180))
                    },
                ),
                Span::raw("  "),
            ];
            if let Some(p) = &t.pr {
                meta.push(Span::styled(
                    format!("PR {}#{}", p.repo, p.number),
                    Style::default().fg(Color::Rgb(150, 170, 255)),
                ));
                meta.push(Span::raw("  "));
            }
            if let Some(m) = &t.from_msg {
                meta.push(Span::styled(
                    format!("✉ from msg {m}"),
                    Style::default().fg(SECONDARY_FG),
                ));
                meta.push(Span::raw("  "));
            }
            if let Some(c) = t.created {
                meta.push(Span::styled(
                    format!("added {}", age_string(c)),
                    Style::default().fg(SECONDARY_FG),
                ));
            }
            let mut detail = vec![
                Line::from(meta),
                Line::from(""),
                Line::from(Span::styled(
                    t.text.clone(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )),
            ];
            // The body (the "message") below the title, blank-line separated.
            if let Some(b) = &t.body {
                detail.push(Line::from(""));
                for bl in b.lines() {
                    detail.push(Line::from(Span::styled(
                        bl.to_string(),
                        Style::default().fg(Color::Rgb(200, 200, 200)),
                    )));
                }
            }
            frame.render_widget(
                Paragraph::new(detail)
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::TOP)),
                body[1],
            );
        }
    }

    // Add-task input line (active when `task_input` is Some).
    if let Some(buf) = app.task_input.as_ref() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "＋ ",
                    Style::default()
                        .fg(Color::Rgb(150, 200, 180))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(buf.clone()),
                Span::styled("▏", Style::default().fg(Color::White)),
            ])),
            outer[2],
        );
    }

    render_tasks_footer(frame, outer[3], app.task_input.is_some());
}

/// One task row in the tasks modal: `[x]/[ ] text  <age>  ✉`. Done tasks dim;
/// the `✉` marks a task spun off from an inbox message.
fn task_modal_line(t: &crate::store::Task, max_w: usize) -> Line<'static> {
    let check = if t.done { "[x] " } else { "[ ] " };
    let check_style = if t.done {
        Style::default().fg(SECONDARY_FG)
    } else {
        Style::default().fg(Color::Rgb(150, 200, 180))
    };
    let text_style = if t.done {
        Style::default()
            .fg(SECONDARY_FG)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(Color::White)
    };
    let age = t
        .created
        .map(|c| format!("  {}", age_string(c)))
        .unwrap_or_default();
    let from = if t.from_msg.is_some() { "  ✉" } else { "" };
    let usable = max_w
        .saturating_sub(check.len() + age.chars().count() + from.chars().count())
        .max(1);
    let mut spans = vec![
        Span::styled(check, check_style),
        Span::styled(clip(&t.text, usable), text_style),
    ];
    if !age.is_empty() {
        spans.push(Span::styled(age, Style::default().fg(SECONDARY_FG)));
    }
    if !from.is_empty() {
        spans.push(Span::styled(from, Style::default().fg(SECONDARY_FG)));
    }
    Line::from(spans)
}

fn render_tasks_footer(frame: &mut Frame, area: Rect, input_mode: bool) {
    let hint: Vec<(&str, &str)> = if input_mode {
        vec![
            ("type", " the task   "),
            ("Enter", " add   "),
            ("Esc", " cancel"),
        ]
    } else {
        vec![
            ("j/k", " select   "),
            ("space", " toggle done   "),
            ("a", " add   "),
            ("d", " drop   "),
            ("esc", " close"),
        ]
    };
    let mut spans = Vec::new();
    for (k, label) in hint {
        spans.push(Span::styled(k, Style::default().fg(SECONDARY_FG)));
        spans.push(Span::styled(label, Style::default().fg(SECONDARY_FG)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Compose modal (#D): you → a client, or → everyone. Three fields
/// (recipient / title / body); the focused field is highlighted and shows a
/// block cursor. Owns keyboard input while open (see `handle_input`).
fn render_compose_modal(frame: &mut Frame, app: &App) {
    let Some(c) = app.compose.as_ref() else {
        return;
    };
    let area = centered_rect(70, 60, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);

    let me = app.me.as_deref().unwrap_or("you");
    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" Compose — from {me} "),
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // recipient
            Constraint::Length(1), // title
            Constraint::Min(3),    // body
            Constraint::Length(1), // footer / error
        ])
        .split(inner);

    // Recipient — ◀ value ▶ when focused.
    let rec_focused = c.field == ComposeField::Recipient;
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("To:      ", field_label_style(rec_focused)),
            Span::styled(
                if rec_focused { "◀ " } else { "  " },
                Style::default().fg(SECONDARY_FG),
            ),
            Span::styled(
                c.targets[c.target_idx].label(),
                if rec_focused {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                },
            ),
            Span::styled(
                if rec_focused { " ▶" } else { "" },
                Style::default().fg(SECONDARY_FG),
            ),
        ])),
        parts[0],
    );

    // Title (single line) — caret shown at its position when focused.
    let title_focused = c.field == ComposeField::Title;
    let title_line = if title_focused {
        let mut spans = vec![Span::styled("Title:   ", field_label_style(true))];
        // usize::MAX = never wrap — the title is a single row; take that one line.
        spans.extend(
            lines_with_cursor(&c.title, c.title_cursor, usize::MAX)
                .remove(0)
                .spans,
        );
        Line::from(spans)
    } else {
        Line::from(vec![
            Span::styled("Title:   ", field_label_style(false)),
            Span::styled(c.title.clone(), Style::default().fg(Color::White)),
        ])
    };
    frame.render_widget(Paragraph::new(title_line), parts[1]);

    // Body (wraps) — caret shown at its position (any line) when focused.
    let body_focused = c.field == ComposeField::Body;
    let mut body_lines = vec![Line::from(Span::styled(
        "Message:",
        field_label_style(body_focused),
    ))];
    if body_focused {
        body_lines.extend(lines_with_cursor(
            &c.body,
            c.body_cursor,
            parts[2].width as usize,
        ));
    } else {
        for l in c.body.split('\n') {
            body_lines.push(Line::from(Span::raw(l.to_string())));
        }
    }
    frame.render_widget(Paragraph::new(body_lines), parts[2]);

    // Footer / error / confirm prompt.
    let footer = if c.confirming {
        Line::from(vec![
            Span::styled(
                format!("Send to {}?  ", c.targets[c.target_idx].label()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Enter/y",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" send   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("Esc/n", Style::default().fg(SECONDARY_FG)),
            Span::styled(" keep editing", Style::default().fg(SECONDARY_FG)),
        ])
    } else if let Some(err) = &c.error {
        Line::from(Span::styled(
            err.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(vec![
            Span::styled("Tab", Style::default().fg(SECONDARY_FG)),
            Span::styled(" field   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("←/→", Style::default().fg(SECONDARY_FG)),
            Span::styled(" recipient   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("Enter", Style::default().fg(SECONDARY_FG)),
            Span::styled(" send   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("Shift+Enter", Style::default().fg(SECONDARY_FG)),
            Span::styled(" newline   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("Esc", Style::default().fg(SECONDARY_FG)),
            Span::styled(" cancel", Style::default().fg(SECONDARY_FG)),
        ])
    };
    frame.render_widget(Paragraph::new(footer), parts[3]);
}

/// First-run "pick your name" prompt (#D) — shown when no `me` is set.
/// Confirm dialog for disconnecting a client (key `D` in the Clients pane).
fn render_disconnect_confirm(frame: &mut Frame, app: &App) {
    let Some(name) = app.confirm_disconnect.as_ref() else {
        return;
    };
    let area = centered_rect(60, 30, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "Disconnect client",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("Remove "),
            Span::styled(
                name.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" from the cockpit?"),
        ]),
        Line::from("Deletes its registration, presence, and inbox."),
        Line::from("(Re-registers if the agent is still running.)"),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Enter/y",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" disconnect   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("Esc/n", Style::default().fg(SECONDARY_FG)),
            Span::styled(" cancel", Style::default().fg(SECONDARY_FG)),
        ]),
    ];
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Disconnect "))
        .style(Style::default().bg(Color::Reset));
    frame.render_widget(para, area);
}

fn render_name_prompt(frame: &mut Frame, app: &App) {
    let Some(buf) = app.name_prompt.as_ref() else {
        return;
    };
    let area = centered_rect(60, 35, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "Join the network",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Pick your name as a client — agents address you by it,"),
        Line::from("and you get your own inbox. Something memorable / stable."),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Name: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                buf.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("\u{2588}", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Enter", Style::default().fg(SECONDARY_FG)),
            Span::styled(" join   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("Esc", Style::default().fg(SECONDARY_FG)),
            Span::styled(" skip (just watch)", Style::default().fg(SECONDARY_FG)),
        ]),
    ];
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" Welcome "))
        .style(Style::default().bg(Color::Reset));
    frame.render_widget(para, area);
}

/// "New project" name prompt (key `P` in the Clients pane).
fn render_new_project_modal(frame: &mut Frame, app: &App) {
    let Some(buf) = app.new_project.as_ref() else {
        return;
    };
    let area = centered_rect(60, 30, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "New project",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("A grouping for clients. Move clients in with `m`; a client"),
        Line::from("in no project floats (available to all)."),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Name: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                buf.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("\u{2588}", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Enter", Style::default().fg(SECONDARY_FG)),
            Span::styled(" create   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("Esc", Style::default().fg(SECONDARY_FG)),
            Span::styled(" cancel", Style::default().fg(SECONDARY_FG)),
        ]),
    ];
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" New project "),
        )
        .style(Style::default().bg(Color::Reset));
    frame.render_widget(para, area);
}

/// "Rename project" prompt (key `r` on a project row).
fn render_rename_project_modal(frame: &mut Frame, app: &App) {
    let Some((_, buf)) = app.rename_project.as_ref() else {
        return;
    };
    let area = centered_rect(60, 25, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "Rename project",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Name: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                buf.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("\u{2588}", Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Enter", Style::default().fg(SECONDARY_FG)),
            Span::styled(" save   ", Style::default().fg(SECONDARY_FG)),
            Span::styled("Esc", Style::default().fg(SECONDARY_FG)),
            Span::styled(" cancel", Style::default().fg(SECONDARY_FG)),
        ]),
    ];
    let para = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Rename project "),
        )
        .style(Style::default().bg(Color::Reset));
    frame.render_widget(para, area);
}

/// Cyan+bold when the field is focused, dim otherwise.
fn field_label_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(SECONDARY_FG)
    }
}

/// A reverse-video block over `s` — the visible caret cell.
fn cursor_block(s: &str) -> Span<'static> {
    Span::styled(
        s.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::REVERSED),
    )
}

/// Render `text` as display lines with the caret (a reverse-video block) at
/// char index `cursor` — over the char there, or a trailing block at the end —
/// so the focused compose field shows where edits land mid-text. Honors
/// embedded newlines and hard-wraps (char-based) to `width` columns.
///
/// We wrap here rather than via ratatui's `Paragraph::wrap`: its `trim: false`
/// word-wrapper renders a whitespace-only line (e.g. the empty body, where the
/// caret sits alone) as *two* rows — which showed up as a phantom blank line
/// above the caret that vanished on the first keystroke.
fn lines_with_cursor(text: &str, cursor: usize, width: usize) -> Vec<Line<'static>> {
    let total = text.chars().count();
    let cur = cursor.min(total);
    let white = Style::default().fg(Color::White);
    let width = width.max(1);
    let mut lines: Vec<Line> = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut seg = String::new();
    let mut col = 0usize; // visible columns used on the current line
    for (i, ch) in text.chars().enumerate() {
        // Soft-wrap before a visible char that would overflow the line.
        if ch != '\n' && col >= width {
            if !seg.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut seg), white));
            }
            lines.push(Line::from(std::mem::take(&mut spans)));
            col = 0;
        }
        if i == cur {
            if !seg.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut seg), white));
            }
            if ch == '\n' {
                spans.push(cursor_block(" "));
                lines.push(Line::from(std::mem::take(&mut spans)));
                col = 0;
            } else {
                spans.push(cursor_block(&ch.to_string()));
                col += 1;
            }
            continue;
        }
        if ch == '\n' {
            if !seg.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut seg), white));
            }
            lines.push(Line::from(std::mem::take(&mut spans)));
            col = 0;
        } else {
            seg.push(ch);
            col += 1;
        }
    }
    if !seg.is_empty() {
        spans.push(Span::styled(seg, white));
    }
    if cur >= total {
        // Caret past the last char — bump it to a fresh line if this one is full.
        if col >= width {
            lines.push(Line::from(std::mem::take(&mut spans)));
        }
        spans.push(cursor_block(" "));
    }
    lines.push(Line::from(spans));
    lines
}

/// Story modal for a Done work item — opened from DoneReview via Enter.
/// Shows the issue/PR titles, lifecycle states, and the chronological
/// list of events for this issue from the audit log. The modal owns
/// keyboard input while open (see `handle_input` in `app.rs`):
///   - `c` writes a `card-acknowledged` row to the audit log and
///     dismisses the item from view.
///   - `o` opens the PR (or issue if no PR) in the browser.
///   - `esc` closes the modal without dismissing — so the user can
///     scan it and come back later.
fn render_story_modal(frame: &mut Frame, app: &App, issue: u64) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(ratatui::widgets::Clear, area);

    let work_item = app.work.get(issue);
    let pr = work_item.and_then(|w| w.pr);
    let repo = work_item.map(|w| w.repo.as_str()).unwrap_or("acme/web");

    let issue_meta = app.titles.get(&(RefKind::Issue, issue)).cloned().flatten();
    let pr_meta = pr.and_then(|p| app.titles.get(&(RefKind::Pr, p)).cloned().flatten());

    let issue_title = issue_meta
        .as_ref()
        .and_then(|m| m.title.clone())
        .unwrap_or_else(|| "(title not loaded)".to_string());
    let pr_title = pr_meta.as_ref().and_then(|m| m.title.clone());

    let issue_state = match issue_meta.as_ref().map(|m| m.closed) {
        Some(true) => "CLOSED",
        Some(false) => "OPEN",
        None => "?",
    };
    let pr_state = match pr_meta.as_ref() {
        Some(m) if m.merged => "MERGED",
        Some(m) if m.closed => "CLOSED",
        Some(_) => "OPEN",
        None => "?",
    };

    let title = match pr {
        Some(p) => format!(" #{issue} → PR #{p} "),
        None => format!(" #{issue} "),
    };

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        issue_title.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    if let Some(t) = &pr_title {
        lines.push(Line::from(Span::styled(
            format!("PR: {t}"),
            Style::default().fg(SECONDARY_FG),
        )));
    }
    lines.push(Line::from(""));

    let pr_label = pr
        .map(|p| format!("PR #{p}"))
        .unwrap_or_else(|| "no PR".into());
    lines.push(Line::from(vec![
        Span::raw("Issue: "),
        Span::styled(issue_state, Style::default().fg(state_color(issue_state))),
        Span::raw("    "),
        Span::raw(format!("{pr_label}: ")),
        Span::styled(pr_state, Style::default().fg(state_color(pr_state))),
        Span::raw("    "),
        Span::styled(format!("repo: {repo}"), Style::default().fg(SECONDARY_FG)),
    ]));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        "Chronology",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    for ev in events_for_issue(app, issue, pr) {
        let ts = ev.timestamp.format("%H:%M").to_string();
        let mut details = ev.details.clone();
        // Strip the issue=/pr= boilerplate from details for readability —
        // it's already implied by being in this modal.
        details = details
            .replace(&format!("issue=#{issue}"), "")
            .trim()
            .to_string();
        if let Some(p) = pr {
            details = details.replace(&format!("pr=#{p}"), "").trim().to_string();
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {ts}  "), Style::default().fg(SECONDARY_FG)),
            Span::styled(
                format!("{:<22}", ev.action),
                Style::default().fg(action_color(&ev.action)),
            ),
            Span::raw(" "),
            Span::raw(details),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "[c] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("close   "),
        Span::styled(
            "[o] ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("open in browser   "),
        Span::styled(
            "[esc] ",
            Style::default()
                .fg(SECONDARY_FG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("back"),
    ]));

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        title,
        Style::default().add_modifier(Modifier::BOLD),
    ));
    let para = Paragraph::new(lines)
        .block(block)
        .style(Style::default().bg(Color::Reset));
    frame.render_widget(para, area);
}

/// Events tied to an issue (and its associated PR, if any). Used by
/// the story modal to render the chronology. Filters in chronological
/// order — `app.events` is push-order so that's already chronological.
fn events_for_issue(app: &App, issue: u64, pr: Option<u64>) -> Vec<&Event> {
    app.events
        .iter()
        .filter(|ev| {
            ev.refs.iter().any(|r| match r.kind {
                RefKind::Issue => r.number == issue,
                RefKind::Pr => Some(r.number) == pr,
                RefKind::NewIssue => false,
            })
        })
        .collect()
}

fn state_color(state: &str) -> Color {
    match state {
        "OPEN" => Color::Green,
        "MERGED" => Color::Rgb(80, 160, 80),
        "CLOSED" => SECONDARY_FG,
        _ => SECONDARY_FG,
    }
}

fn action_color(action: &str) -> Color {
    match action {
        "pick" | "card-in-progress" | "branch" | "pr-opened" | "issue-commented" => Color::Cyan,
        "applied" | "rebased" | "force-push" | "gemini-retriggered" => Color::LightBlue,
        "ready-flipped" | "pr-merged" | "card-done" => Color::Green,
        "pause" | "blocked" | "question" | "ambiguous" => Color::Yellow,
        "skip" | "skip-owned" => Color::Red,
        _ => Color::White,
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r)[1];

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup)[1]
}

fn pad_to(s: &str, width: usize) -> String {
    if s.chars().count() >= width {
        s.to_string()
    } else {
        format!("{s:<width$}", s = s, width = width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Agent, AgentState, ClientKind, Message};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::mpsc;

    /// Flatten the rendered buffer into one string for substring asserts.
    fn screen(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// One terminal row as a string — for asserting *which line* text is on.
    fn row(terminal: &Terminal<TestBackend>, y: usize) -> String {
        let buf = terminal.backend().buffer();
        let w = buf.area.width as usize;
        buf.content[y * w..(y + 1) * w]
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn app_with_one_agent() -> App {
        let (tx, _rx) = mpsc::channel(1);
        let (wtx, _wrx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            tx,
            std::path::PathBuf::from("/tmp/fleet-test"),
            Some("tester".to_string()),
            wtx,
        );
        let now = chrono::Local::now().fixed_offset();
        app.roster = vec![Agent {
            name: "vscode-bot".into(),
            kind: ClientKind::Agent,
            repo: "acme/web".into(),
            board: "acme/14".into(),
            role: Some("VS Code UI".into()),
            description: Some("VS Code extension; ask me to wire UI against new MCP tools.".into()),
            state: AgentState::Working,
            task: Some("issue #90".into()),
            last_seen: Some(now),
            stale: false,
            unread: 1,
            context_pct: None,
        }];
        app.inboxes.insert(
            "vscode-bot".into(),
            vec![Message {
                id: "abc".into(),
                time: Some(now),
                from: "mcp-bot".into(),
                title: "need fetch schema".into(),
                body: "please expose fetch_match_ranges".into(),
                read: false,
            }],
        );
        app
    }

    #[test]
    fn agents_pane_renders_roster() {
        let mut app = app_with_one_agent();
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Clients"), "pane title");
        assert!(text.contains("vscode-bot"), "agent name");
        assert!(text.contains("VS Code UI"), "role chip");
        assert!(!text.contains("web"), "repo column is hidden");
        assert!(!text.contains("acme/14"), "board column is hidden");
        assert!(
            !text.contains("[working]"),
            "state text label is gone — the dot conveys it"
        );
    }

    #[test]
    fn task_adopts_its_linked_work_item_as_a_child() {
        use crate::store::{Task, TaskPr};
        let mut app = app_with_one_agent();
        app.show_client_tasks = true;
        // A board PR opened in the agent's repo.
        app.work.ingest(
            &crate::event::Event::from_log_line(
                "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tissue=#187 pr=#188",
            )
            .unwrap(),
        );
        // A task on the agent's list, linked to that PR.
        app.tasks.insert(
            "vscode-bot".into(),
            vec![Task {
                id: "tk1".into(),
                text: "wire the thing".into(),
                body: None,
                done: false,
                created: None,
                pr: Some(TaskPr {
                    repo: "acme/web".into(),
                    number: 188,
                }),
                from_msg: None,
            }],
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // The linked work-item renders exactly once — as the task's child, never
        // also standalone in its own section.
        let n188 = app
            .tree_rows
            .iter()
            .filter(|r| matches!(r, TreeRow::Item { pr: Some(188), .. }))
            .count();
        assert_eq!(
            n188, 1,
            "the linked work-item appears once (no standalone duplicate)"
        );
        // …and it sits directly beneath the task that adopts it.
        let pos = app
            .tree_rows
            .iter()
            .position(|r| matches!(r, TreeRow::Item { pr: Some(188), .. }))
            .unwrap();
        assert!(
            matches!(app.tree_rows.get(pos - 1), Some(TreeRow::Task { .. })),
            "the work-item is nested under the task above it"
        );
        let scr = screen(&terminal);
        assert!(scr.contains("wire the thing"), "the task renders");
        assert!(
            scr.contains('↳') && scr.contains("PR #188"),
            "the work-item shows as an indented child"
        );
    }

    #[test]
    fn blocked_on_supervisor_vs_peer_render_distinctly() {
        let mut app = app_with_one_agent();
        let now = chrono::Local::now().fixed_offset();
        let mk = |name: &str, state| Agent {
            name: name.into(),
            kind: ClientKind::Agent,
            repo: "acme/x".into(),
            board: "acme/14".into(),
            role: None,
            description: None,
            state,
            task: Some("status".into()),
            last_seen: Some(now),
            stale: false,
            unread: 0,
            context_pct: None,
        };
        app.roster = vec![
            mk("needsme", AgentState::Blocked),
            mk("onpeer", AgentState::Waiting),
        ];
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        // Loud alert only for the supervisor-block; calm count for the peer one.
        assert!(
            text.contains("1 blocked — needs you"),
            "supervisor-block alert"
        );
        assert!(text.contains("1 waiting"), "peer-block calm count");
        // Distinct row glyphs: ⚠ (needs you) vs ◐ (waiting on peer).
        assert!(text.contains('⚠'), "blocked-on-supervisor shows ⚠");
        assert!(text.contains('◐'), "waiting-on-peer shows ◐");
    }

    #[test]
    fn work_items_pane_hidden_by_default() {
        let mut app = app_with_one_agent();
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            !screen(&terminal).contains("Work items"),
            "standalone work-items pane is hidden by default"
        );

        app.show_work_pane = true;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen(&terminal).contains("Work items"),
            "work-items pane shows when enabled"
        );
    }

    #[test]
    fn inbox_modal_renders_message() {
        let mut app = app_with_one_agent();
        app.show_inbox_for = Some("vscode-bot".into());
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Inbox"), "modal title");
        assert!(text.contains("Helps with"), "capability header label");
        assert!(text.contains("wire UI"), "agent description rendered");
        assert!(text.contains("need fetch schema"), "message title");
        assert!(text.contains("mcp-bot"), "message sender");
        assert!(text.contains("please expose"), "message body");
    }

    #[test]
    fn inbox_body_scrolls_when_long() {
        let mut app = app_with_one_agent();
        // A 40-line body that overflows the detail pane in a short terminal.
        let body = (1..=40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.inboxes.insert(
            "vscode-bot".into(),
            vec![Message {
                id: "zz".into(),
                time: None,
                from: "mcp-bot".into(),
                title: "long one".into(),
                body,
                read: true,
            }],
        );
        app.show_inbox_for = Some("vscode-bot".into());
        app.inbox_selected = 0;
        app.inbox_scroll = 0;

        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        let top = screen(&t);
        assert!(top.contains("line 1"), "shows the start of the body");
        assert!(
            top.contains('↓'),
            "a scrollbar appears when the body overflows"
        );
        assert!(
            !top.contains("line 40"),
            "the tail is off-screen before scrolling"
        );

        // Scroll past the end (the renderer clamps) — the tail becomes visible.
        app.inbox_scroll = 200;
        t.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen(&t).contains("line 40"), "scrolling reveals the tail");
    }

    #[test]
    fn reply_prefills_sender_and_re_title() {
        let mut app = app_with_one_agent();
        app.show_inbox_for = Some("vscode-bot".into());
        app.inbox_selected = 0;
        app.open_reply();
        // Inbox handed off to a compose addressed back to the sender.
        assert!(app.show_inbox_for.is_none(), "inbox closed for compose");
        let c = app.compose.as_ref().expect("compose opened");
        assert_eq!(c.title, "re: need fetch schema", "re: + previous title");
        assert!(
            matches!(&c.targets[c.target_idx], crate::app::ComposeTarget::Client(n) if n == "mcp-bot"),
            "addressed to the sender"
        );
        assert_eq!(
            c.field,
            crate::app::ComposeField::Body,
            "caret starts in the body"
        );
    }

    #[test]
    fn inbox_list_scrolls_when_many_messages() {
        let mut app = app_with_one_agent();
        let msgs: Vec<Message> = (1..=30)
            .map(|i| Message {
                id: format!("m{i}"),
                time: None,
                from: "mcp-bot".into(),
                title: format!("msg {i}"),
                body: "x".into(),
                read: true,
            })
            .collect();
        app.inboxes.insert("vscode-bot".into(), msgs);
        app.show_inbox_for = Some("vscode-bot".into());
        app.inbox_selected = 0;
        // Short modal → the 30-message list overflows its pane (body is 1 line,
        // so the only scrollbar is the list's).
        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&t);
        assert!(text.contains("msg 1"), "shows the top of the list");
        assert!(
            text.contains('↓'),
            "a scrollbar appears when the list overflows"
        );
    }

    #[test]
    fn role_links_to_repo() {
        let mut app = app_with_one_agent(); // vscode-bot, repo acme/web
        // The link is gated on a confirmed-existing repo (cached gh check).
        app.repo_exists
            .lock()
            .unwrap()
            .insert("acme/web".into(), true);
        let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            app.click_targets
                .iter()
                .any(|c| c.url == "https://github.com/acme/web"),
            "the role chip registers a click target opening the agent's repo"
        );
    }

    #[test]
    fn role_not_linked_until_repo_confirmed() {
        let mut app = app_with_one_agent();
        // No repo_exists entry yet → not confirmed → the role is not a link.
        let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            !app.click_targets.iter().any(|c| c.url.contains("web")),
            "role is not a link until the repo is confirmed to exist"
        );
    }

    #[test]
    fn repo_url_skips_globs_and_local() {
        assert_eq!(
            repo_url("acme/web").as_deref(),
            Some("https://github.com/acme/web")
        );
        assert!(
            repo_url("acme/*").is_none(),
            "fleet-wide glob (code review)"
        );
        assert!(
            repo_url("local/token-optimization").is_none(),
            "non-GitHub owner"
        );
        assert!(repo_url("-").is_none(), "human placeholder");
        assert!(repo_url("").is_none());
    }

    #[test]
    fn state_indicator_animates_by_state() {
        // Working spins — frame advances tick to tick.
        assert_ne!(
            state_indicator(AgentState::Working, false, 0).content,
            state_indicator(AgentState::Working, false, 1).content,
            "working spinner should advance"
        );
        // Blocked is a steady warning sign now (no blink) — the status-bar
        // alert carries the attention; the row symbol is self-evident.
        let blocked = state_indicator(AgentState::Blocked, false, 0);
        assert_eq!(
            blocked.content,
            state_indicator(AgentState::Blocked, false, 5).content,
            "blocked no longer animates"
        );
        assert!(
            blocked.content.contains('⚠'),
            "blocked shows a warning sign"
        );
        // Idle is steady — same glyph regardless of tick.
        assert_eq!(
            state_indicator(AgentState::Idle, false, 0).content,
            state_indicator(AgentState::Idle, false, 9).content,
            "idle should not animate"
        );
        // Stale is frozen even for an otherwise-animated state, and shows ○.
        let stale = state_indicator(AgentState::Working, true, 0);
        assert_eq!(
            stale.content,
            state_indicator(AgentState::Working, true, 4).content,
            "stale should not animate"
        );
        assert!(stale.content.contains('○'), "stale shows a hollow dot");
    }

    #[test]
    fn empty_roster_shows_placeholder_not_panic() {
        let (tx, _rx) = mpsc::channel(1);
        let (wtx, _wrx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            tx,
            std::path::PathBuf::from("/tmp/fleet-test"),
            Some("tester".to_string()),
            wtx,
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen(&terminal).contains("no clients yet"));
    }

    #[test]
    fn tree_nests_work_items_under_owning_client() {
        let mut app = app_with_one_agent(); // vscode-bot, repo acme/web
        // A work item in that repo should nest under the client.
        let ev = crate::event::Event::from_log_line(
            "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90",
        )
        .unwrap();
        app.work.ingest(&ev);
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("vscode-bot"), "client row present");
        assert!(text.contains("#90"), "owned item nested under its client");
    }

    #[test]
    fn tree_navigation_walks_clients_and_items() {
        let mut app = app_with_one_agent(); // vscode-bot
        app.work.ingest(
            &crate::event::Event::from_log_line(
                "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90",
            )
            .unwrap(),
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // The flattened rows hold the client header then its work-item.
        assert!(
            matches!(app.tree_rows.first(), Some(TreeRow::Client(_))),
            "first selectable row is the client"
        );
        let item_idx = app
            .tree_rows
            .iter()
            .position(|r| matches!(r, TreeRow::Item { .. }))
            .expect("the work-item is selectable");

        // Selecting the client resolves to the client; selecting the item does not.
        app.tree_selected = 0;
        assert_eq!(
            app.selected_agent().map(|a| a.name.as_str()),
            Some("vscode-bot")
        );
        app.tree_selected = item_idx;
        assert!(app.selected_agent().is_none(), "an item row isn't a client");

        // The item carries the PR + issue for Enter / Shift+Enter to open.
        match &app.tree_rows[item_idx] {
            TreeRow::Item { pr, issue, .. } => {
                assert_eq!(*pr, Some(98), "Enter opens this PR");
                assert_eq!(*issue, Some(90), "Shift+Enter opens this issue");
            }
            _ => panic!("expected an item row"),
        }
    }

    #[test]
    fn compose_from_a_pr_addresses_its_owner_and_titles_it() {
        let mut app = app_with_one_agent(); // vscode-bot, repo acme/web
        app.work.ingest(
            &crate::event::Event::from_log_line(
                "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90",
            )
            .unwrap(),
        );
        // Render so the Clients tree flattens into selectable rows.
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let item_idx = app
            .tree_rows
            .iter()
            .position(|r| matches!(r, TreeRow::Item { .. }))
            .expect("the PR work-item is selectable");

        // Focus that PR row and compose: the modal pre-addresses the owning
        // client, pre-fills the title with the PR ref, and drops the caret in
        // the body.
        app.focus_mode = FocusMode::Clients;
        app.tree_selected = item_idx;
        app.open_compose();

        let c = app.compose.as_ref().expect("compose modal opened");
        assert_eq!(
            c.targets[c.target_idx].label(),
            "vscode-bot",
            "addressed to the PR's owner"
        );
        assert_eq!(c.title, "PR #98", "title pre-filled with the PR ref");
        assert_eq!(c.field, ComposeField::Body, "caret starts in the body");
    }

    #[test]
    fn compose_empty_body_has_no_phantom_line_above_caret() {
        let mut app = app_with_one_agent();
        app.compose = Some(crate::app::Compose {
            targets: vec![crate::app::ComposeTarget::Client("vscode-bot".into())],
            target_idx: 0,
            title: String::new(),
            body: String::new(),
            title_cursor: 0,
            body_cursor: 0,
            field: ComposeField::Body,
            error: None,
            confirming: false,
        });
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        let buf = t.backend().buffer();
        let w = buf.area.width as usize;
        let row_text = |y: usize| -> String {
            buf.content[y * w..(y + 1) * w]
                .iter()
                .map(|c| c.symbol())
                .collect()
        };
        let label_row = (0..buf.area.height as usize)
            .find(|&y| row_text(y).contains("Message:"))
            .expect("Message: label present");
        let caret_row = buf
            .content
            .iter()
            .enumerate()
            .filter(|(_, c)| c.modifier.contains(Modifier::REVERSED))
            .map(|(i, _)| i / w)
            .find(|&y| y > label_row)
            .expect("caret rendered in the body");
        assert_eq!(
            caret_row,
            label_row + 1,
            "the empty-body caret sits directly under the Message: label — no phantom blank line"
        );
    }

    #[test]
    fn compose_body_wraps_long_lines() {
        let mut app = app_with_one_agent();
        app.compose = Some(crate::app::Compose {
            targets: vec![crate::app::ComposeTarget::Client("vscode-bot".into())],
            target_idx: 0,
            title: String::new(),
            body: "x".repeat(200),
            title_cursor: 0,
            body_cursor: 0,
            field: ComposeField::Body,
            error: None,
            confirming: false,
        });
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        let buf = t.backend().buffer();
        let w = buf.area.width as usize;
        let x_rows = (0..buf.area.height as usize)
            .filter(|&y| {
                let line: String = buf.content[y * w..(y + 1) * w]
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                line.matches('x').count() > 3
            })
            .count();
        assert!(
            x_rows >= 2,
            "a long body wraps onto multiple rows (got {x_rows})"
        );
    }

    #[test]
    fn help_documents_the_group_leader_shortcut() {
        let (tx, _rx) = mpsc::channel(1);
        let (wtx, _wrx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            tx,
            std::path::PathBuf::from("/tmp/fleet-test"),
            Some("tester".to_string()),
            wtx,
        );
        app.show_help = true;
        // Tall enough that the (long) help isn't vertically clipped.
        let mut t = Terminal::new(TestBackend::new(160, 90)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&t);
        assert!(
            text.contains("group leader"),
            "help explains promoting a client to group leader"
        );
        assert!(
            text.contains("coordinator"),
            "help names the coordinator role"
        );
    }

    #[test]
    fn help_scrolls_with_a_scrollbar_when_overflowing() {
        let (tx, _rx) = mpsc::channel(1);
        let (wtx, _wrx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            tx,
            std::path::PathBuf::from("/tmp/fleet-test"),
            Some("tester".to_string()),
            wtx,
        );
        app.show_help = true;
        // Short + narrow terminal → the help wraps and overflows, so a
        // scrollbar must appear and rendering must not panic.
        let mut t = Terminal::new(TestBackend::new(90, 20)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen(&t).contains('↓'),
            "a scrollbar end-arrow shows when help overflows"
        );
    }

    #[test]
    fn idle_clients_render_dimmer_than_active_ones() {
        // True if the "vscode-bot" name cell is bold (bright) in the current render.
        let name_is_bold = |app: &mut App| -> bool {
            let mut t = Terminal::new(TestBackend::new(140, 30)).unwrap();
            t.draw(|f| render(f, app)).unwrap();
            let buf = t.backend().buffer();
            let w = buf.area.width as usize;
            for y in 0..buf.area.height as usize {
                let row: String = buf.content[y * w..(y + 1) * w]
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                if let Some(b) = row.find("vscode-bot") {
                    let col = row[..b].chars().count();
                    return buf.content[y * w + col].modifier.contains(Modifier::BOLD);
                }
            }
            panic!("vscode-bot row not found");
        };
        let mut app = app_with_one_agent();
        app.roster[0].state = AgentState::Working;
        assert!(
            name_is_bold(&mut app),
            "a working client's name is bold/bright"
        );
        app.roster[0].state = AgentState::Idle;
        assert!(
            !name_is_bold(&mut app),
            "an idle client's name is dimmed (not bold)"
        );
    }

    #[test]
    fn status_bar_shows_session_and_weekly_usage() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut app = app_with_one_agent();
        app.usage = Some(crate::store::Usage {
            five_hour_pct: 90.0,
            seven_day_pct: 88.0,
            ts: now,
            five_hour_reset: Some(now + 70 * 60), // 1h10m out
            seven_day_reset: Some(now + 6 * 86400 + 14 * 3600), // 6d14h out
        });
        let mut t = Terminal::new(TestBackend::new(160, 30)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&t);
        assert!(text.contains("session 90%"), "shows the 5h session usage");
        assert!(text.contains("weekly 88%"), "shows the 7d weekly usage");
        // Reset countdowns appear after each percentage.
        assert!(
            text.contains("1h10m"),
            "shows time until the 5h window resets"
        );
        assert!(
            text.contains("6d14h"),
            "shows time until the 7d window resets"
        );
        // A fresh reading is not labelled stale.
        assert!(!text.contains("old"), "fresh usage carries no age label");
    }

    #[test]
    fn status_bar_marks_stale_usage_with_age() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut app = app_with_one_agent();
        // A reading from ~10 min ago: past USAGE_STALE_SECS, so the chip should
        // still show the numbers but tag them with their age.
        app.usage = Some(crate::store::Usage {
            five_hour_pct: 90.0,
            seven_day_pct: 88.0,
            ts: now - 600,
            five_hour_reset: None,
            seven_day_reset: None,
        });
        let mut t = Terminal::new(TestBackend::new(160, 30)).unwrap();
        t.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&t);
        assert!(
            text.contains("session 90%"),
            "stale reading still shows numbers"
        );
        assert!(
            text.contains("10m old"),
            "stale reading is tagged with its age"
        );
    }

    #[test]
    fn merged_ticket_shows_until_dismissed_with_x() {
        let mut app = app_with_one_agent(); // task is "issue #90" — use other ids
        // Merged but NOT card-done → stays visible until the supervisor dismisses.
        for line in [
            "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#199 issue=#191",
            "2026-05-29T15:01:00+02:00\tpr-review-fixup\tweb\tpr-merged\tpr=#199 issue=#191",
        ] {
            app.work
                .ingest(&crate::event::Event::from_log_line(line).unwrap());
        }
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen(&terminal).contains("#191"), "merged ticket shows");
        assert!(
            app.tree_rows.iter().any(|r| matches!(
                r,
                TreeRow::Item {
                    issue: Some(191),
                    ..
                }
            )),
            "the merged ticket is a selectable row (x would dismiss it)"
        );
        // `x` → acknowledged → drops out of the view.
        app.acknowledged.insert(crate::work::WorkKey::Issue(191));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            !screen(&terminal).contains("#191"),
            "dismissed ticket is hidden"
        );
    }

    #[test]
    fn x_dismisses_an_unfinished_item_not_just_done_merged() {
        let mut app = app_with_one_agent();
        // A blocked, non-finished work-item — like the parked #79/#87/#89 that
        // sit waiting on a human and shouldn't clutter the board.
        app.work.ingest(
            &crate::event::Event::from_log_line(
                "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tblocked\tissue=#77 reason=product-decision",
            )
            .unwrap(),
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(screen(&terminal).contains("#77"), "blocked item shows");

        // Dismissing it (what `x` does) hides it even though it isn't finished.
        app.acknowledged.insert(crate::work::WorkKey::Issue(77));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            !screen(&terminal).contains("#77"),
            "a dismissed blocked item is hidden"
        );
    }

    #[test]
    fn card_done_item_auto_retires() {
        let mut app = app_with_one_agent();
        // pr-merged then card-done → Done → leaves the active pane on its own,
        // no `x` needed (the agent already moved the board card to Done).
        for line in [
            "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#199 issue=#191",
            "2026-05-29T15:01:00+02:00\tpr-review-fixup\tweb\tpr-merged\tpr=#199 issue=#191",
            "2026-05-29T15:02:00+02:00\tboard-issue-loop\tweb\tcard-done\tissue=#191",
        ] {
            app.work
                .ingest(&crate::event::Event::from_log_line(line).unwrap());
        }
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            !screen(&terminal).contains("#191"),
            "card-done item auto-retires without a dismiss"
        );
    }

    #[test]
    fn agent_status_renders_in_box_below_node() {
        let mut app = app_with_one_agent(); // vscode-bot, task = "issue #90", no items
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // The status is not inline on the node row...
        let name_row = (0..30)
            .find(|&y| row(&terminal, y).contains("vscode-bot"))
            .expect("client row present");
        assert!(
            !row(&terminal, name_row).contains("issue #90"),
            "status is not inline on the node row"
        );
        // ...it renders on a line below the node — and inside a box (a
        // status-only box here, since this client has no work-items).
        let status_row = (name_row + 1..30).find(|&y| row(&terminal, y).contains("issue #90"));
        assert!(status_row.is_some(), "status renders below the node");
        assert!(
            screen(&terminal).contains('╭'),
            "status sits inside a rounded box"
        );
    }

    #[test]
    fn ticketless_merged_pr_is_hidden_from_active_pane() {
        let mut app = app_with_one_agent();
        // Ticketless PR opened then merged → Done → retired from the pane.
        for line in [
            "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#777",
            "2026-05-29T15:01:00+02:00\tpr-review-fixup\tweb\tpr-merged\tpr=#777",
        ] {
            app.work
                .ingest(&crate::event::Event::from_log_line(line).unwrap());
        }
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        // Not in the tree or the work-items pane (log is hidden by default).
        assert!(
            !screen(&terminal).contains("#777"),
            "retired ticketless merge is hidden from the active pane"
        );
    }

    #[test]
    fn pr_column_shows_and_links() {
        let mut app = app_with_one_agent();
        // Ticketless PR (no issue), in review.
        app.work.ingest(
            &crate::event::Event::from_log_line(
                "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#60",
            )
            .unwrap(),
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        // The PR number shows in its own column, and it's clickable.
        assert!(
            screen(&terminal).contains("PR #60"),
            "PR column shows the number"
        );
        assert!(
            app.click_targets
                .iter()
                .any(|t| t.url.ends_with("/pull/60")),
            "PR column links to the PR"
        );
    }

    #[test]
    fn clients_pane_hover_highlights_the_link_under_the_cursor() {
        // Regression: the pane renders into an off-screen buffer at *content*
        // coordinates, so a link's hover highlight must be resolved against the
        // screen-space click target (post-blit) — not the content row, which
        // lit the link up several rows above where it's actually drawn.
        let mut app = app_with_one_agent();
        app.work.ingest(
            &crate::event::Event::from_log_line(
                "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#60",
            )
            .unwrap(),
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();

        // First render (no hover) populates the click targets.
        app.hover_pos = None;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let (trow, tcol) = app
            .click_targets
            .iter()
            .find(|t| t.url.ends_with("/pull/60"))
            .map(|t| (t.row, t.col_start))
            .expect("the PR link is a click target");

        let cell_reversed = |terminal: &Terminal<TestBackend>, x: u16, y: u16| -> bool {
            let buf = terminal.backend().buffer();
            let w = buf.area.width;
            buf.content[(y * w + x) as usize]
                .modifier
                .contains(Modifier::REVERSED)
        };

        // Without hover the link isn't highlighted (and isn't the selected row).
        assert!(
            !cell_reversed(&terminal, tcol, trow),
            "the PR link is not reversed until hovered"
        );

        // Hovering the link's own screen cell lights it up — on its row, not a
        // few rows above it (the off-by-`inner.y` regression).
        app.hover_pos = Some((trow, tcol));
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            cell_reversed(&terminal, tcol, trow),
            "hovering the PR link reverses its own cell (no row offset)"
        );
    }

    #[test]
    fn issue_column_keeps_titles_aligned() {
        use crate::work::{WorkItem, WorkKey, WorkState};
        let ts = chrono::DateTime::parse_from_rfc3339("2026-05-29T15:00:00+02:00").unwrap();
        let mk = |key, issue, pr| WorkItem {
            key,
            issue,
            pr,
            repo: "acme/web".into(),
            state: WorkState::InReview,
            last_event: ts,
            last_action: "act-start".into(),
            note: String::new(),
        };
        let ticketed = mk(WorkKey::Issue(90), Some(90), Some(98));
        let ticketless = mk(WorkKey::Pr(60), None, Some(60));
        let text = |w: &WorkItem| {
            build_work_item_line(w, None, None, 120, 0, 0, None)
                .0
                .spans
                .iter()
                .map(|s| s.content.as_ref().to_string())
                .collect::<String>()
        };
        // The fixed-width issue + PR columns mean everything after them lines
        // up — the [last-action] tag lands at the same column with or without a
        // ticket.
        assert_eq!(
            text(&ticketed).find('['),
            text(&ticketless).find('['),
            "columns after the refs align regardless of board ticket"
        );
        assert!(
            text(&ticketed).contains("#90") && text(&ticketed).contains("PR #98"),
            "ticketed shows its issue and PR"
        );
        assert!(
            text(&ticketless).contains("PR #60") && !text(&ticketless).contains("#90"),
            "ticketless shows its PR, blank issue column"
        );
    }

    #[test]
    fn event_log_hidden_until_toggled() {
        let mut app = app_with_one_agent();
        // An event whose detail token appears only in the event pane (not
        // ingested into work, so it can't show up in the tree/work panes).
        let ev = crate::event::Event::from_log_line(
            "2026-05-29T15:00:00+02:00\tboard-issue-loop\tweb\tsweep\tnote=LOGMARKERZZZ",
        )
        .unwrap();
        app.events.push(ev);

        // Hidden by default — the status bar carries the count, not the log.
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            !screen(&terminal).contains("LOGMARKERZZZ"),
            "event stream hidden by default"
        );

        // `l` reveals it.
        app.show_log = true;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen(&terminal).contains("LOGMARKERZZZ"),
            "event stream shown after toggle"
        );
    }

    #[test]
    fn human_client_renders_as_you() {
        let mut app = app_with_one_agent();
        let now = chrono::Local::now().fixed_offset();
        app.roster.insert(
            0,
            Agent {
                name: "tester".into(),
                kind: ClientKind::Human,
                repo: "-".into(),
                board: "-".into(),
                role: None,
                description: None,
                state: AgentState::Unknown,
                task: None,
                last_seen: Some(now),
                stale: false,
                unread: 0,
                context_pct: None,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("tester"), "human name");
        assert!(text.contains("you (supervisor)"), "human rendered as you");
    }

    #[test]
    fn compose_modal_renders() {
        let mut app = app_with_one_agent();
        app.compose = Some(crate::app::Compose {
            targets: vec![
                crate::app::ComposeTarget::Everyone,
                crate::app::ComposeTarget::Client("vscode-bot".into()),
            ],
            target_idx: 1,
            title: "ping".into(),
            body: "no action needed".into(),
            title_cursor: 0,
            body_cursor: 16,
            field: ComposeField::Body,
            error: None,
            confirming: false,
        });
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Compose"), "compose title");
        assert!(text.contains("vscode-bot"), "selected recipient");
        assert!(text.contains("ping"), "title field");
        assert!(text.contains("no action needed"), "body field");
    }

    #[test]
    fn compose_modal_shows_confirm_prompt_when_staged() {
        let mut app = app_with_one_agent();
        app.compose = Some(crate::app::Compose {
            targets: vec![
                crate::app::ComposeTarget::Everyone,
                crate::app::ComposeTarget::Client("vscode-bot".into()),
            ],
            target_idx: 1,
            title: "ping".into(),
            body: "no action needed".into(),
            title_cursor: 0,
            body_cursor: 16,
            field: ComposeField::Body,
            error: None,
            confirming: true,
        });
        let mut terminal = Terminal::new(TestBackend::new(140, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(
            text.contains("Send to vscode-bot?"),
            "confirm prompt names the recipient"
        );
        assert!(
            text.contains("keep editing"),
            "confirm prompt offers a way back"
        );
        // The composed content is still visible to review before confirming.
        assert!(
            text.contains("no action needed"),
            "body still shown under the prompt"
        );
    }

    #[test]
    fn project_tree_groups_members_and_floats_the_rest() {
        let mut app = app_with_one_agent();
        let now = chrono::Local::now().fixed_offset();
        // A second agent that won't be in the project — it should float.
        app.roster.push(Agent {
            name: "protoman".into(),
            kind: ClientKind::Agent,
            repo: "acme/review".into(),
            board: "acme/14".into(),
            role: Some("reviewer".into()),
            description: None,
            state: AgentState::Idle,
            task: None,
            last_seen: Some(now),
            stale: false,
            unread: 0,
            context_pct: Some(42),
        });
        app.projects = vec![crate::store::Project {
            id: "auth".into(),
            name: "Auth Revamp".into(),
            coordinator: Some("vscode-bot".into()),
            members: vec!["vscode-bot".into()],
        }];
        app.focus_mode = FocusMode::Clients;
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        assert!(text.contains("Auth Revamp"), "project header rendered");
        assert!(
            text.contains("vscode-bot ★"),
            "coordinator marked with a trailing star on its row"
        );
        assert!(text.contains("protoman"), "outside client still listed");
        assert!(text.contains("42%"), "per-agent context % rendered");
        // Outside / floating clients render on top, above the project sections.
        let outside = text.find("protoman").unwrap();
        let project = text.find("Auth Revamp").unwrap();
        assert!(outside < project, "outside client renders above projects");
    }

    #[test]
    fn task_line_is_single_clipped_title() {
        let long = "a much longer task title that clearly exceeds twenty columns wide";
        let t = crate::store::Task {
            id: "t".into(),
            text: long.into(),
            body: None,
            done: false,
            created: None,
            pr: None,
            from_msg: None,
        };
        // The pane shows one clipped line (full text lives in the modal detail).
        let line = task_line(&t, false, false, 20);
        let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        assert!(w <= 20, "clipped to the box width, got {w}");
        assert!(
            line.spans.iter().any(|s| s.content.contains('…')),
            "a long title shows an ellipsis"
        );
    }

    #[test]
    fn tasks_modal_shows_full_text_of_long_task() {
        let mut app = app_with_one_agent();
        let long =
            "remember to reconcile the dedup key across autoscan sources before the followup"
                .to_string();
        app.tasks.insert(
            "vscode-bot".into(),
            vec![crate::store::Task {
                id: "t1".into(),
                text: long,
                body: Some("the longer note that lives in the detail pane".into()),
                done: false,
                created: None,
                pr: None,
                from_msg: None,
            }],
        );
        app.show_tasks_for = Some("vscode-bot".into());
        app.tasks_selected = 0;
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let text = screen(&terminal);
        // The list row clips; only the wrapped detail pane shows the tail word.
        assert!(
            text.contains("followup"),
            "detail pane shows the full task text, wrapped"
        );
        // The body (the "message") renders in the detail too.
        assert!(
            text.contains("detail pane"),
            "task body renders in the detail"
        );
    }

    #[test]
    fn tasks_are_navigable_rows_and_compose_addresses_owner() {
        use crate::app::{ComposeTarget, TreeRow};
        let mut app = app_with_one_agent();
        app.tasks.insert(
            "vscode-bot".into(),
            vec![crate::store::Task {
                id: "tk1".into(),
                text: "wire the new endpoint".into(),
                body: None,
                done: false,
                created: None,
                pr: None,
                from_msg: None,
            }],
        );
        app.focus_mode = FocusMode::Clients;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();

        // The task is a selectable tree row (walked by j/k like a work-item).
        let pos = app
            .tree_rows
            .iter()
            .position(|r| matches!(r, TreeRow::Task { id, .. } if id == "tk1"))
            .expect("task appears as a selectable tree row");

        // Composing about it pre-addresses the owner and titles the message.
        app.tree_selected = pos;
        app.open_compose();
        let c = app.compose.as_ref().expect("compose opened");
        assert!(
            c.title.starts_with("task: wire the new endpoint"),
            "title references the task: {:?}",
            c.title
        );
        assert!(
            matches!(&c.targets[c.target_idx], ComposeTarget::Client(n) if n == "vscode-bot"),
            "message is addressed to the task's owner",
        );
    }

    #[test]
    fn clients_pane_scrolls_to_follow_selection() {
        let now = chrono::Local::now().fixed_offset();
        let mut app = app_with_one_agent();
        // Many agents so the roster overflows a short pane.
        app.roster = (0..30)
            .map(|n| Agent {
                name: format!("agent{n:02}"),
                kind: ClientKind::Agent,
                repo: "acme/x".into(),
                board: "acme/14".into(),
                role: None,
                description: None,
                state: AgentState::Idle,
                task: None,
                last_seen: Some(now),
                stale: false,
                unread: 0,
                context_pct: None,
            })
            .collect();
        app.focus_mode = FocusMode::Clients;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();

        // Selection at the top: the first agent is visible.
        app.tree_selected = 0;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let first = match &app.tree_rows[0] {
            TreeRow::Client(i) => app.roster[*i].name.clone(),
            _ => unreachable!("first row is a client"),
        };
        let last_idx = app.tree_rows.len() - 1;
        let last = match &app.tree_rows[last_idx] {
            TreeRow::Client(i) => app.roster[*i].name.clone(),
            _ => unreachable!("last row is a client"),
        };
        assert!(
            screen(&terminal).contains(&first),
            "top agent visible when selected at top"
        );

        // Select the last agent: the pane scrolls to it; the top scrolls off.
        app.tree_selected = last_idx;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let scr = screen(&terminal);
        assert!(scr.contains(&last), "last agent scrolled into view");
        assert!(!scr.contains(&first), "top agent scrolled off-view");
    }

    #[test]
    fn clients_pane_shows_a_scrollbar_when_overflowing() {
        let now = chrono::Local::now().fixed_offset();
        let mut app = app_with_one_agent();
        app.focus_mode = FocusMode::Clients;
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();

        // One agent fits the pane: no scrollbar (arrows live only in the help
        // overlay, which is closed here).
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            !screen(&terminal).contains('↓'),
            "no scrollbar when the roster fits the pane"
        );

        // Many agents overflow it: a scrollbar with ↑/↓ end-arrows appears.
        app.roster = (0..30)
            .map(|n| Agent {
                name: format!("agent{n:02}"),
                kind: ClientKind::Agent,
                repo: "acme/x".into(),
                board: "acme/14".into(),
                role: None,
                description: None,
                state: AgentState::Idle,
                task: None,
                last_seen: Some(now),
                stale: false,
                unread: 0,
                context_pct: None,
            })
            .collect();
        app.tree_selected = 0;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen(&terminal).contains('↓'),
            "a scrollbar end-arrow shows when the roster overflows the pane"
        );
    }

    #[test]
    fn inbox_marks_messages_that_have_a_task() {
        let mut app = app_with_one_agent();
        // A task spun off from the inbox message "abc" (see app_with_one_agent).
        app.tasks.insert(
            "vscode-bot".into(),
            vec![crate::store::Task {
                id: "tk".into(),
                text: "follow up on the schema".into(),
                body: None,
                done: false,
                created: None,
                pr: None,
                from_msg: Some("abc".into()),
            }],
        );
        app.show_inbox_for = Some("vscode-bot".into());
        app.inbox_selected = 0;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen(&terminal).contains('☐'),
            "a message that's been turned into a task is marked in the inbox list"
        );
    }

    #[test]
    fn tab_hides_agent_tasks_but_keeps_mine() {
        let now = chrono::Local::now().fixed_offset();
        let mk = |name: &str, kind| Agent {
            name: name.into(),
            kind,
            repo: "acme/x".into(),
            board: "acme/14".into(),
            role: None,
            description: None,
            state: AgentState::Idle,
            task: None,
            last_seen: Some(now),
            stale: false,
            unread: 0,
            context_pct: None,
        };
        let task = |text: &str| crate::store::Task {
            id: text.into(),
            text: text.into(),
            body: None,
            done: false,
            created: None,
            pr: None,
            from_msg: None,
        };
        let mut app = app_with_one_agent();
        app.roster = vec![
            mk("tester", ClientKind::Human),
            mk("bot", ClientKind::Agent),
        ];
        app.tasks
            .insert("tester".into(), vec![task("my own reminder")]);
        app.tasks.insert("bot".into(), vec![task("agent reminder")]);
        app.focus_mode = FocusMode::Clients;

        // Tab off: my task stays, the agent's is hidden.
        app.show_client_tasks = false;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        let scr = screen(&terminal);
        assert!(scr.contains("my own reminder"), "my own tasks always show");
        assert!(
            !scr.contains("agent reminder"),
            "agent tasks hidden when toggled off"
        );

        // Tab on: the agent's task returns.
        app.show_client_tasks = true;
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen(&terminal).contains("agent reminder"),
            "agent tasks return when toggled on"
        );
    }

    #[test]
    fn inbox_delete_shows_confirm_prompt() {
        let mut app = app_with_one_agent();
        app.show_inbox_for = Some("vscode-bot".into());
        app.inbox_delete_armed = true;
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| render(f, &mut app)).unwrap();
        assert!(
            screen(&terminal).contains("Delete this message?"),
            "an armed delete shows a confirm prompt in the footer"
        );
    }
}
