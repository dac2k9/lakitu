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
    extract::{Path, Request},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use maud::{DOCTYPE, Markup, html};

use crate::wire::{AgentDto, MessageDto, ProjectDto, SnapshotDto, TaskDto, UsageDto};

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
async fn view_partial(Path(tab): Path<String>) -> Html<String> {
    let snap = crate::fleet::snapshot().await;
    let view = match tab.as_str() {
        "tasks" => tasks_fragment(&snap),
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
    let is_supervisor = snap
        .agents
        .iter()
        .any(|a| a.kind == "human" && a.name == name);
    let mut recipients: Vec<&str> = snap
        .agents
        .iter()
        .filter(|a| a.kind != "human")
        .map(|a| a.name.as_str())
        .collect();
    recipients.sort_unstable();
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
            div class="composer" {
                @if is_supervisor {
                    form data-msg-form data-inbox=(name) {
                        select class="composer-to" name="to" required {
                            option value="" disabled selected { "to…" }
                            @for r in &recipients { option value=(r) { (r) } }
                        }
                        input class="composer-title" name="title" placeholder="subject" required;
                        textarea class="composer-body" name="body" placeholder="message…" required {}
                        button type="submit" class="composer-send" { "send" }
                    }
                } @else {
                    form data-msg-form data-inbox=(name) data-send-to=(name) {
                        input class="composer-title" name="title" placeholder="subject" required;
                        textarea class="composer-body" name="body" placeholder=(format!("message {name}…")) required {}
                        button type="submit" class="composer-send" { "send" }
                    }
                }
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

/// The "tasks" tab — a placeholder until the full tasks view lands. Static (no
/// self-poll); switching to Fleet swaps `#view` back to the live board.
fn tasks_fragment(_snap: &SnapshotDto) -> Markup {
    html! {
        main id="view" class="live" {
            div class="coming-soon" {
                div class="cs-glyph" { "◷" }
                div class="cs-title" { "Tasks" }
                div class="cs-sub" { "Coming soon — a fleet-wide view of every client's tasks." }
            }
        }
    }
}

fn page(snap: &SnapshotDto) -> Markup {
    // The web cockpit acts AS the supervisor. The bearer token is injected into
    // the (loopback-only, host-guarded) page so the browser can call /v1 for
    // writes — which keeps the web routes themselves GET-only (see daemon.rs).
    let token = std::env::var("LAKITU_FLEET_TOKEN").unwrap_or_default();
    let me = snap
        .agents
        .iter()
        .find(|a| a.kind == "human")
        .map_or("", |a| a.name.as_str());
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="lakitu-token" content=(token);
                meta name="lakitu-me" content=(me);
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
                            button class="tbox" data-task-done data-owner=(a.name) data-id=(t.id) title="mark done" { "▢" }
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
