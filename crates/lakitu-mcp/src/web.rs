//! Read-only web cockpit (`GET /`) served by `lakitu-mcp serve` on loopback —
//! "Lakitu's lens".
//!
//! A browser-facing mirror of the TUI: it renders the same `fleet::snapshot()`
//! the cockpit reads, as server-side HTML (maud), framed as a live viewfinder
//! onto the fleet — the lens locks FOCUS on whoever needs you, and the clients
//! are grouped under their team. It live-refreshes via htmx polling and is
//! **read-only**: every mutation still goes through `/v1` behind the bearer
//! layer. Because rendering happens in-process from the snapshot, the browser
//! needs no token; `daemon.rs` mounts these routes OUTSIDE the bearer layer and
//! ONLY on a loopback bind.

use axum::{
    Router,
    extract::{Path, Query, Request},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use maud::{DOCTYPE, Markup, html};

use crate::wire::{
    AgentDto, MessageDto, ProjectDto, SharedTaskDto, SnapshotDto, TaskDto, TaskEventDto,
    TaskRefDto, UsageDto,
};

/// The web routes, rooted at `/`. Mounted OUTSIDE the bearer-auth layer — the
/// caller (`daemon.rs`) must guarantee a loopback bind before merging this.
pub fn router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/partial/view/{tab}", get(view_partial))
        .route("/partial/inbox/{name}", get(inbox_partial))
        .route("/assets/app.css", get(css))
        .route("/assets/app.js", get(js))
        .route("/assets/htmx.min.js", get(htmx))
        .layer(middleware::from_fn(host_guard))
}

/// Reject requests whose `Host` header isn't a loopback host — defense in depth
/// against DNS-rebinding. The loopback *bind* (enforced in `daemon.rs`) stops
/// off-box network reach; this stops a malicious page the operator visits from
/// rebinding a hostname to `127.0.0.1:<port>` and reading the unauthenticated UI
/// same-origin. Only the host is checked (the port is irrelevant), and it is
/// fail-closed: a missing or unparseable `Host` is rejected.
async fn host_guard(req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(host_only)
        .is_some_and(is_loopback_host);
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "forbidden: the web cockpit is loopback-only\n",
        )
            .into_response()
    }
}

/// The host portion of a `Host` header value, dropping any `:port` and keeping
/// IPv6 brackets (e.g. `[::1]:8787` -> `[::1]`, `127.0.0.1:8787` -> `127.0.0.1`).
fn host_only(h: &str) -> &str {
    if h.starts_with('[') {
        return h.find(']').map(|end| &h[..=end]).unwrap_or(h);
    }
    h.rsplit_once(':').map(|(host, _)| host).unwrap_or(h)
}

fn is_loopback_host(h: &str) -> bool {
    matches!(h, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

async fn index() -> Html<String> {
    Html(page(&crate::fleet::snapshot().await).into_string())
}

/// The htmx-polled fragment for a tab's view (`fleet`, `tasks`, …) — swaps and
/// self-polls the `#view` region.
async fn view_partial(
    Path(tab): Path<String>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Html<String> {
    let snap = crate::fleet::snapshot().await;
    let view = match tab.as_str() {
        "tasks" => tasks_fragment(&snap, q.get("show_done").is_some_and(|v| v == "1")),
        _ => live(&snap),
    };
    Html(view.into_string())
}

/// An agent's inbox thread, rendered into the drawer (read-only).
async fn inbox_partial(Path(name): Path<String>) -> Html<String> {
    Html(inbox_drawer(&name, &crate::fleet::snapshot().await).into_string())
}

fn inbox_drawer(name: &str, snap: &SnapshotDto) -> Markup {
    let msgs = snap.inboxes.get(name);
    let total = msgs.map_or(0, Vec::len);
    let unread = msgs.map_or(0, |m| m.iter().filter(|x| !x.read).count());
    html! {
        div class="drawer-backdrop" data-close-drawer="1" {}
        aside class="drawer-panel" {
            div class="drawer-head" {
                div class="drawer-title-wrap" {
                    span class="drawer-title" { "✉ " (name) }
                    span class="drawer-sub" { (unread) " unread · " (total) " total" }
                }
                button class="drawer-close" data-close-drawer="1" aria-label="close inbox" { "✕" }
            }
            @if let Some(list) = msgs {
                @if list.is_empty() {
                    div class="drawer-empty" { "Inbox empty." }
                } @else {
                    div class="thread" {
                        @for m in list { (message_item(m)) }
                    }
                }
            } @else {
                div class="drawer-empty" { "Inbox empty." }
            }
        }
    }
}

fn message_item(m: &MessageDto) -> Markup {
    html! {
        article class=(if m.read { "msg" } else { "msg unread" }) {
            div class="msg-head" {
                span class="msg-from" { (m.from) }
                @if let Some(t) = &m.time { span class="msg-time" { (short_time(t)) } }
            }
            div class="msg-title" { (m.title) }
            div class="msg-body" { (m.body) }
        }
    }
}

fn short_time(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|t| t.format("%b %d · %H:%M").to_string())
        .unwrap_or_else(|_| ts.to_string())
}

async fn css() -> impl IntoResponse {
    asset(
        "text/css; charset=utf-8",
        include_str!("../assets/web/app.css"),
    )
}
async fn js() -> impl IntoResponse {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../assets/web/app.js"),
    )
}
async fn htmx() -> impl IntoResponse {
    asset(
        "text/javascript; charset=utf-8",
        include_str!("../assets/web/htmx.min.js"),
    )
}
fn asset(ct: &'static str, body: &'static str) -> impl IntoResponse {
    // no-store: the cockpit iterates fast and is served locally, so always hand
    // the browser fresh CSS/JS rather than risk a stale cached stylesheet (e.g.
    // an old sheet styling a since-changed element).
    (
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
}

// ---- Templates -------------------------------------------------------------

/// The "tasks" tab — a fleet-wide view of every shared task: a card per
/// `SharedTask` with its scope, participants, linked issues/PRs, and a
/// Start→Goal timeline. Live like the Fleet board (self-polls `#view`) at a
/// calmer 5s, since shared tasks move on GitHub-sync cadence. Done tasks are
/// hidden by default (mirrors the TUI's `include_done=false`) so the board
/// stays on live work; `show_done` reveals them and is preserved across polls.
fn tasks_fragment(snap: &SnapshotDto, show_done: bool) -> Markup {
    // Counts span ALL shared tasks, so the "done" vital stays honest even when
    // the done cards are hidden.
    let count = |s: &str| snap.shared_tasks.iter().filter(|t| t.state == s).count();
    let done_total = count("done");

    // Drop done unless asked; then urgency-sort (attention first, ties broken by
    // most-recently-updated — RFC3339 sorts chronologically as plain strings).
    let mut tasks: Vec<&SharedTaskDto> = snap
        .shared_tasks
        .iter()
        .filter(|t| show_done || t.state != "done")
        .collect();
    tasks.sort_by(|a, b| {
        task_urgency(&a.state)
            .cmp(&task_urgency(&b.state))
            .then_with(|| b.updated.cmp(&a.updated))
    });

    // The self-poll carries the toggle so a refresh doesn't reset it.
    let poll_url = if show_done {
        "/partial/view/tasks?show_done=1"
    } else {
        "/partial/view/tasks"
    };

    html! {
        main id="view" class="live tasks" hx-get=(poll_url) hx-trigger="every 5s" hx-swap="outerHTML" {
            section class="telemetry" {
                div class="vitals" {
                    (vital("shared", snap.shared_tasks.len(), "v-on"))
                    (vital("active", count("active"), "v-work"))
                    (vital("in review", count("in-review"), "v-rev"))
                    (vital("blocked", count("blocked"), "v-block"))
                    (vital("done", done_total, "v-done"))
                }
                @if done_total > 0 {
                    @if show_done {
                        button class="done-toggle" hx-get="/partial/view/tasks" hx-target="#view" hx-swap="outerHTML" { "hide done" }
                    } @else {
                        button class="done-toggle" hx-get="/partial/view/tasks?show_done=1" hx-target="#view" hx-swap="outerHTML" { "show done (" (done_total) ")" }
                    }
                }
            }

            @if tasks.is_empty() {
                @if snap.shared_tasks.is_empty() {
                    div class="coming-soon" {
                        div class="cs-glyph" { "◷" }
                        div class="cs-title" { "No shared tasks yet" }
                        div class="cs-sub" {
                            "A shared task groups issues + PRs across the fleet toward one goal. "
                            "Create one with " code { "create_shared_task" }
                            " — it self-populates and self-advances as PRs land on GitHub."
                        }
                    }
                } @else {
                    div class="coming-soon" {
                        div class="cs-glyph" { "✓" }
                        div class="cs-title" { "All clear" }
                        div class="cs-sub" {
                            "All " (done_total) " shared task" @if done_total != 1 { "s" } " done. "
                            button class="done-toggle inline" hx-get="/partial/view/tasks?show_done=1" hx-target="#view" hx-swap="outerHTML" { "show done" }
                        }
                    }
                }
            } @else {
                div class="tgrid" {
                    @for t in &tasks { (shared_task_card(t, snap)) }
                }
            }
        }
    }
}

/// One shared-task card: head (state glyph · title · scope · state) over the
/// goal, participants, linked issues/PRs, and the Start→Goal timeline.
fn shared_task_card(t: &SharedTaskDto, snap: &SnapshotDto) -> Markup {
    let cls = task_state_cls(&t.state);
    let active = t.state == "active";
    let card_cls = format!(
        "tcard{}",
        if t.state == "blocked" { " blocked" } else { "" }
    );
    let linked = t.issues.len() + t.prs.len();

    html! {
        article class=(card_cls) {
            div class="tcard-head" {
                span class=(format!("glyph {cls}")) data-spin[active] { (task_state_glyph(&t.state)) }
                div class="id" {
                    span class="name" { (t.title) }
                    span class="role" { (scope_label(t)) }
                }
                span class=(format!("state-label {cls}")) { (t.state) }
            }

            @if let Some(goal) = &t.goal {
                div class=(format!("now {cls}")) { span class="now-text" { "→ " (goal) } }
            }

            @if !t.participants.is_empty() {
                div class="prow" {
                    @for p in &t.participants { (participant_chip(p, snap)) }
                }
            }

            @if linked > 0 {
                div class="refs" {
                    @for i in &t.issues { (ref_pill(i, "issue")) }
                    @for p in &t.prs { (ref_pill(p, "pr")) }
                }
            }

            @if !t.timeline.is_empty() { (timeline_strip(&t.timeline)) }

            div class="meta" {
                span { (linked) " linked" }
                span class="dot-sep" { "·" }
                span {
                    (t.participants.len())
                    @if t.participants.len() == 1 { " participant" } @else { " participants" }
                }
                span class="dot-sep" { "·" }
                span { "updated " (short_time(&t.updated)) }
            }
        }
    }
}

/// A participant chip. Per the reconcile contract, `participants[]` mixes fleet
/// agent names (the owner + anyone who ran `join_shared_task`) with GitHub
/// logins (a closing PR's author, auto-credited by the sweep). Distinguish
/// them: a name that matches the roster renders as an agent chip with its live
/// state glyph; anything else is a plain `@github-login` chip.
fn participant_chip(p: &str, snap: &SnapshotDto) -> Markup {
    match snap.agents.iter().find(|a| a.name == *p) {
        Some(a) => {
            let cls = color_cls(a);
            html! {
                span class="pchip agent" title=(format!("{p} · fleet agent")) {
                    span class=(format!("pglyph {cls}")) { (glyph_for(a)) }
                    (p)
                }
            }
        }
        None => html! {
            span class="pchip gh" title="GitHub contributor — auto-credited from a closing PR" {
                "@" (p)
            }
        },
    }
}

/// A linked issue or PR, as a pill that opens the item on GitHub. `kind` picks
/// the path segment (`pull` vs `issues`), the type emoji, and the title word.
/// The ref's cached `state` (reconcile-populated, snapshot-only — no live gh)
/// drives a trailing status dot + a border tint; `None` (not yet reconciled)
/// falls back to type-only, so an un-synced ref still renders cleanly.
fn ref_pill(r: &TaskRefDto, kind: &str) -> Markup {
    let short = r.repo.rsplit('/').next().unwrap_or(&r.repo);
    let (seg, emoji, word) = if kind == "pr" {
        ("pull", "🔀", "PR")
    } else {
        ("issues", "🔘", "issue")
    };
    let href = format!("https://github.com/{}/{}/{}", r.repo, seg, r.number);
    let state_cls = r.state.as_deref().map(ref_state_cls).unwrap_or("");
    let title = match &r.state {
        Some(s) => format!("{word} {}#{} · {s} — open on GitHub", r.repo, r.number),
        None => format!("{word} {}#{} — open on GitHub", r.repo, r.number),
    };
    html! {
        a class=(format!("pill {kind} {state_cls}")) href=(href) target="_blank" rel="noopener noreferrer"
            title=(title) {
            span class="pill-mark" { (emoji) }
            (short) "#" (r.number)
            @if let Some(s) = &r.state {
                @let dot = ref_state_dot(s);
                @if !dot.is_empty() { span class="pill-state" { (dot) } }
            }
        }
    }
}

/// The status dot for a linked ref's cached GitHub state: 🟢 open · ⚪ draft ·
/// 🟣 merged · 🔴 closed. Unknown ⇒ empty (no dot).
fn ref_state_dot(state: &str) -> &'static str {
    match state {
        "open" => "🟢",
        "draft" => "⚪",
        "merged" => "🟣",
        "closed" => "🔴",
        _ => "",
    }
}

/// A border-tint class for a linked ref's state — a quieter scannability cue
/// alongside the dot.
fn ref_state_cls(state: &str) -> &'static str {
    match state {
        "open" => "s-open",
        "draft" => "s-draft",
        "merged" => "s-merged",
        "closed" => "s-closed",
        _ => "",
    }
}

/// The Start→Goal timeline: append-only state transitions, oldest→newest, the
/// current state emphasized. Each step's tooltip carries who moved it, when, and
/// the optional "why" note (`reconcile` marks an auto-sync move); the latest
/// note is surfaced inline below as the current "why". Scrolls if it overflows.
fn timeline_strip(events: &[TaskEventDto]) -> Markup {
    let last = events.len().saturating_sub(1);
    html! {
        div class="tl-wrap" {
            div class="tl" {
                @for (i, e) in events.iter().enumerate() {
                    @if i > 0 { span class="tl-arrow" { "→" } }
                    span class=(format!("tl-step {}{}", task_state_cls(&e.state), if i == last { " cur" } else { "" }))
                        title=(event_title(e)) {
                        (e.state)
                    }
                }
            }
        }
        @if let Some(note) = events.last().and_then(|e| e.note.as_deref()) {
            div class="tl-note" { "↳ " (note) }
        }
    }
}

/// Tooltip for a timeline step: `state · by who · when`, with the "why" note
/// appended when present.
fn event_title(e: &TaskEventDto) -> String {
    let base = format!("{} · by {} · {}", e.state, e.by, short_time(&e.ts));
    match &e.note {
        Some(n) => format!("{base} — {n}"),
        None => base,
    }
}

/// A human label for a task's scope: `fleet-wide`, or `team · <owner/projNo>`.
fn scope_label(t: &SharedTaskDto) -> String {
    match t.scope.as_str() {
        "fleet" => "fleet-wide".to_string(),
        "team" => match &t.team {
            Some(team) => format!("team · {team}"),
            None => "team".to_string(),
        },
        other => other.to_string(),
    }
}

fn page(snap: &SnapshotDto) -> Markup {
    // Read-only mirror: no bearer token in the page (the web performs no writes).
    // Write-actions return in a later release behind a same-origin CSRF token.
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Lakitu · fleet lens" }
                link rel="stylesheet" href="/assets/app.css";
                script src="/assets/htmx.min.js" defer {}
                script src="/assets/app.js" defer {}
            }
            body {
                div class="viewfinder" aria-hidden="true" {
                    span class="vf tl" {} span class="vf tr" {}
                    span class="vf bl" {} span class="vf br" {}
                }
                header class="topbar" {
                    div class="scope" {
                        span class="live-tag" { span class="rec" {} "LIVE" }
                        div class="brand" {
                            span class="brand-name" { "LAKITU" }
                            span class="brand-sub" { "fleet lens" }
                        }
                        div class="clock" id="clock" { "··:··:··" }
                    }
                    nav class="tabs" {
                        button class="tab active" data-tab="fleet"
                            hx-get="/partial/view/fleet" hx-target="#view" hx-swap="outerHTML" { "Fleet" }
                        button class="tab" data-tab="tasks"
                            hx-get="/partial/view/tasks" hx-target="#view" hx-swap="outerHTML" { "Tasks" }
                    }
                }
                (live(snap))
                footer class="foot" {
                    "read-only mirror · live feed, refreshes every 2s · "
                    span class="muted" { "writes go through the TUI" }
                }
                div id="drawer" {}
            }
        }
    }
}

/// The live region htmx swaps every 2s: telemetry, the FOCUS panel, and the
/// clients grouped under their teams.
fn live(snap: &SnapshotDto) -> Markup {
    let agents: Vec<&AgentDto> = snap.agents.iter().filter(|a| a.kind != "human").collect();
    let humans: Vec<&AgentDto> = snap.agents.iter().filter(|a| a.kind == "human").collect();

    let working = agents
        .iter()
        .filter(|a| a.state == "working" && !a.stale)
        .count();
    let blocked = agents
        .iter()
        .filter(|a| a.state == "blocked" && !a.stale)
        .count();
    let waiting = agents
        .iter()
        .filter(|a| a.state == "waiting" && !a.stale)
        .count();
    let stale = agents.iter().filter(|a| a.stale).count();
    let blocked_list: Vec<&AgentDto> = agents
        .iter()
        .filter(|a| a.state == "blocked" && !a.stale)
        .copied()
        .collect();

    // Group clients under their team (project membership), urgency-sorted within
    // each team. A client in no project falls into the "Unassigned" group.
    let teams: Vec<(&ProjectDto, Vec<&AgentDto>)> = snap
        .projects
        .iter()
        .map(|p| {
            let mut members: Vec<&AgentDto> = p
                .members
                .iter()
                .filter_map(|m| agents.iter().find(|a| a.name == *m).copied())
                .collect();
            members.sort_by_key(|a| (urgency(a), a.name.to_lowercase()));
            (p, members)
        })
        .collect();
    let mut unassigned: Vec<&AgentDto> = agents
        .iter()
        .filter(|a| !snap.projects.iter().any(|p| p.members.contains(&a.name)))
        .copied()
        .collect();
    unassigned.sort_by_key(|a| (urgency(a), a.name.to_lowercase()));

    html! {
        main id="view" class="live" hx-get="/partial/view/fleet" hx-trigger="every 2s" hx-swap="outerHTML" {
            section class="telemetry" {
                div class="vitals" {
                    (vital("online", agents.len(), "v-on"))
                    (vital("working", working, "v-work"))
                    (vital("needs you", blocked, "v-block"))
                    (vital("waiting", waiting, "v-wait"))
                    (vital("stale", stale, "v-stale"))
                }
                @for h in &humans {
                    button class="you" title="open your inbox"
                        hx-get=(format!("/partial/inbox/{}", h.name))
                        hx-target="#drawer" hx-swap="innerHTML" {
                        span class="glyph st-you" { "◆" }
                        span class="you-name" { (h.name) }
                        span class="you-tag" { "you" }
                        @if h.unread > 0 { span class="badge unread" { (h.unread) " ✉" } }
                    }
                }
                @if let Some(u) = &snap.usage { (usage_chip(u)) }
            }

            (focus_panel(&blocked_list))

            @if agents.is_empty() {
                div class="empty" {
                    "No clients in frame yet. Bring one up with "
                    code { "lakitu-mcp" } " in a repo."
                }
            }
            @for (p, members) in &teams {
                @if !members.is_empty() { (team_section(Some(*p), members, snap)) }
            }
            @if !unassigned.is_empty() { (team_section(None, &unassigned, snap)) }
        }
    }
}

/// One team: a project header (name, coordinator, live health) over its member
/// clients. `project = None` renders the "Unassigned" catch-all group.
fn team_section(project: Option<&ProjectDto>, members: &[&AgentDto], snap: &SnapshotDto) -> Markup {
    let working = members
        .iter()
        .filter(|a| a.state == "working" && !a.stale)
        .count();
    let blocked = members
        .iter()
        .filter(|a| a.state == "blocked" && !a.stale)
        .count();
    let (name, coord, extra) = match project {
        Some(p) => (p.name.as_str(), p.coordinator.as_deref(), ""),
        None => ("Unassigned", None, " unassigned"),
    };

    html! {
        section class=(format!("team{extra}")) {
            div class="team-head" {
                span class="team-name" { (name) }
                @if let Some(c) = coord { span class="coord" title="coordinator" { "★ " (c) } }
                span class="team-stat" {
                    (members.len()) @if members.len() == 1 { " client" } @else { " clients" }
                    @if working > 0 { " · " span class="st-working" { (working) " working" } }
                    @if blocked > 0 { " · " span class="st-blocked" { (blocked) " need you" } }
                }
            }
            div class="agents" {
                @for a in members { (agent_card(a, snap)) }
            }
        }
    }
}

/// The lens focuses on whoever needs you. Blocked agents get a prominent,
/// reticle-framed panel; with none, a calm "all clear" line.
fn focus_panel(blocked: &[&AgentDto]) -> Markup {
    if blocked.is_empty() {
        return html! {
            div class="focus clear" {
                span class="rec" {}
                span { "all clear — nothing needs your attention" }
            }
        };
    }
    html! {
        section class="focus alert" aria-live="polite" {
            div class="focus-eyebrow" {
                "⚠ " (blocked.len())
                @if blocked.len() == 1 { " agent needs you" } @else { " agents need you" }
            }
            @for a in blocked {
                div class="focus-row" {
                    span class="focus-name" { (a.name) }
                    @if let Some(role) = &a.role { span class="focus-role" { (role) } }
                    @if let Some(task) = &a.task { span class="focus-task" { (task) } }
                }
            }
        }
    }
}

fn agent_card(a: &AgentDto, snap: &SnapshotDto) -> Markup {
    let working = a.state == "working" && !a.stale;
    let cls = color_cls(a);
    let card_cls = format!(
        "card{}{}",
        if a.state == "blocked" && !a.stale {
            " blocked"
        } else {
            ""
        },
        if a.stale { " stale" } else { "" },
    );
    let open: Vec<&TaskDto> = snap
        .tasks
        .get(&a.name)
        .map(|v| v.iter().filter(|t| !t.done).collect())
        .unwrap_or_default();

    html! {
        article class=(card_cls) {
            div class="card-head" {
                span class=(format!("glyph {cls}")) data-spin[working] { (glyph_for(a)) }
                div class="id" {
                    span class="name" { (a.name) }
                    @if let Some(role) = &a.role { span class="role" { (role) } }
                }
                div class="head-right" {
                    button
                        class=(if a.unread > 0 { "badge unread open-inbox" } else { "badge open-inbox" })
                        hx-get=(format!("/partial/inbox/{}", a.name))
                        hx-target="#drawer"
                        hx-swap="innerHTML"
                        title="open inbox" {
                        @if a.unread > 0 { (a.unread) " ✉" } @else { "✉" }
                    }
                    @if let Some(p) = a.context_pct { (ctx_chip(p)) }
                }
            }

            div class="repo" { (a.repo) }

            @if let Some(task) = &a.task {
                div class=(format!("now {cls}")) { span class="now-text" { (task) } }
            } @else {
                div class="now idle-now" { "standing by" }
            }

            div class="meta" {
                span class=(format!("state-label {cls}")) { (label_for(a)) }
                span class="dot-sep" { "·" }
                @if open.is_empty() {
                    span class="muted" { "no tasks" }
                } @else {
                    span { (open.len()) " task" @if open.len() != 1 { "s" } }
                }
                span class="dot-sep" { "·" }
                span { (seen(a)) }
            }

            @if !open.is_empty() {
                ul class="tasklist" {
                    @for t in open.iter().take(3) {
                        li {
                            span class="tbox" { "▢" }
                            span class="ttext" { (t.text) }
                        }
                    }
                    @if open.len() > 3 {
                        li class="more" { "+ " (open.len() - 3) " more" }
                    }
                }
            }
        }
    }
}

fn vital(label: &str, n: usize, cls: &str) -> Markup {
    html! {
        div class=(format!("vital {cls}")) {
            span class="vital-n" { (n) }
            span class="vital-l" { (label) }
        }
    }
}

fn ctx_chip(pct: u8) -> Markup {
    html! { span class=(format!("ctx {}", level_cls(pct as f32))) title="context used" { "⌁ " (pct) "%" } }
}

fn usage_chip(u: &UsageDto) -> Markup {
    html! {
        div class="usage" title="rate-limit usage (5h / 7d)" {
            (gauge("5h", u.five_hour_pct))
            (gauge("7d", u.seven_day_pct))
        }
    }
}

fn gauge(label: &str, pct: f32) -> Markup {
    let p = pct.clamp(0.0, 100.0);
    html! {
        div class="gauge" {
            span class="gauge-l" { (label) }
            div class="gauge-bar" {
                div class=(format!("gauge-fill {}", level_cls(p))) style=(format!("width:{p:.0}%")) {}
            }
            span class="gauge-n" { (format!("{p:.0}%")) }
        }
    }
}

// ---- State vocabulary (mirrors the TUI) ------------------------------------

/// Urgency for the in-team sort: blocked → working → waiting → idle → other,
/// with anything stale sinking to the bottom.
fn urgency(a: &AgentDto) -> u8 {
    if a.stale {
        return 9;
    }
    match a.state.as_str() {
        "blocked" => 0,
        "working" => 1,
        "waiting" => 2,
        "idle" => 3,
        _ => 4,
    }
}

/// Urgency for the shared-task sort: blocked → in-review → active → open → done.
fn task_urgency(s: &str) -> u8 {
    match s {
        "blocked" => 0,
        "in-review" => 1,
        "active" => 2,
        "open" => 3,
        "done" => 4,
        _ => 5,
    }
}

/// The colour class for a shared-task state (`ts-*`), distinct from the agent
/// `st-*` classes: open=idle, active=live, blocked=focus, in-review=hold,
/// done=faint.
fn task_state_cls(s: &str) -> &'static str {
    match s {
        "open" => "ts-open",
        "active" => "ts-active",
        "blocked" => "ts-blocked",
        "in-review" => "ts-review",
        "done" => "ts-done",
        _ => "ts-unknown",
    }
}

/// The glyph for a shared-task state: `○` open, `⠋` active (spins), `⚠` blocked,
/// `◑` in-review, `✓` done.
fn task_state_glyph(s: &str) -> &'static str {
    match s {
        "open" => "○",
        "active" => "⠋",
        "blocked" => "⚠",
        "in-review" => "◑",
        "done" => "✓",
        _ => "·",
    }
}

/// The colour class for a state — same semantics as the TUI's state indicator.
fn color_cls(a: &AgentDto) -> &'static str {
    if a.stale {
        return "st-stale";
    }
    match a.state.as_str() {
        "working" => "st-working",
        "blocked" => "st-blocked",
        "waiting" => "st-waiting",
        "idle" => "st-idle",
        _ => "st-unknown",
    }
}

/// The glyph for a state — reused verbatim from the cockpit: spinner seed `⠋`
/// (animated by app.js), `⚠` blocked, `◐` waiting, `•` idle, `○` stale.
fn glyph_for(a: &AgentDto) -> &'static str {
    if a.stale {
        return "○";
    }
    match a.state.as_str() {
        "working" => "⠋",
        "blocked" => "⚠",
        "waiting" => "◐",
        "idle" => "•",
        _ => "·",
    }
}

fn label_for(a: &AgentDto) -> &'static str {
    if a.stale {
        return "stale";
    }
    match a.state.as_str() {
        "working" => "working",
        "blocked" => "blocked",
        "waiting" => "waiting",
        "idle" => "idle",
        _ => "unknown",
    }
}

fn level_cls(pct: f32) -> &'static str {
    if pct >= 85.0 {
        "lvl-hi"
    } else if pct >= 60.0 {
        "lvl-mid"
    } else {
        "lvl-ok"
    }
}

/// Humanized "seen" line from the RFC3339 heartbeat timestamp.
fn seen(a: &AgentDto) -> String {
    let Some(ts) = &a.last_seen else {
        return "never seen".to_string();
    };
    let Ok(t) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return "—".to_string();
    };
    let secs = (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds();
    let h = humanize(secs.max(0) as u64);
    if a.stale {
        format!("lost signal {h}")
    } else {
        format!("seen {h} ago")
    }
}

fn humanize(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}
