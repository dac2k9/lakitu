//! Aggregate the event stream into per-work-item state.
//!
//! A "work item" is one issue (and optionally the PR that fixes it).
//! Events arrive chronologically; we fold them into the work item keyed
//! by issue number. PR-only events (`act-start pr=#M`, `applied pr=#M`,
//! …) resolve to the issue via a `pr → issue` map that's populated when
//! `pr-opened pr=#M issue=#N` is seen.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, FixedOffset};
use ratatui::style::Color;

use crate::event::{Event, RefKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    Triaged,
    InProgress,
    InReview,
    WaitingForInput,
    /// Structural blocker — work paused pending an external resolution
    /// (release cut, asset from a teammate, legal sign-off, etc.).
    /// Distinct from `WaitingForInput`: that's the agent asking a
    /// question; this is "can't continue until X happens."
    Blocked,
    ReadyForMerge,
    /// PR merged — terminal state. The agent doesn't merge itself, but it
    /// emits `pr-merged` when sweep observes the merge happened. Having it
    /// in the log makes the TUI independent of live `gh` calls (avoids
    /// dependence on GraphQL quota for state inference).
    Merged,
    /// Card moved to Done after PR merge. Strictly more terminal than
    /// `Merged`: this is the agent's explicit retire signal, fired only
    /// after the closing issue is CLOSED on GitHub and the card was In
    /// Progress. The TUI's `visible_work_items` filter drops `Done` items
    /// from the active pane — they live only in the audit log thereafter.
    Done,
    Skipped,
    Owned,
}

impl WorkState {
    pub fn label(self) -> &'static str {
        match self {
            WorkState::Triaged => "Triaged",
            WorkState::InProgress => "In Progress",
            // The agent's PRs are always opened as draft. Once flipped via
            // `ready-flipped` the state moves to ReadyForMerge, so anything
            // still in InReview is by construction still in draft —
            // surface that in the label so the supervisor knows it needs
            // a flip when judged ready.
            WorkState::InReview => "In Review (draft)",
            WorkState::WaitingForInput => "Waiting for Input",
            WorkState::Blocked => "Blocked",
            WorkState::ReadyForMerge => "Ready for Merge",
            WorkState::Merged => "Merged",
            WorkState::Done => "Done",
            WorkState::Skipped => "Skipped",
            WorkState::Owned => "Owned by Other",
        }
    }

    pub fn color(self) -> Color {
        match self {
            // Triaged is "agent looked, decided actionable, hasn't started"
            // — an active state with low priority. DarkGray (used for
            // Skipped) was too dim to read on a dark terminal theme; this
            // mid-gray is visibly subdued but still legible.
            WorkState::Triaged => Color::Rgb(170, 170, 170),
            WorkState::InProgress => Color::Cyan,
            WorkState::InReview => Color::LightBlue,
            WorkState::WaitingForInput => Color::Yellow,
            // Orange — distinct from yellow (Waiting for Input) and red
            // (Owned by Other). Signals "stuck, not necessarily on you."
            WorkState::Blocked => Color::Rgb(220, 130, 40),
            WorkState::ReadyForMerge => Color::Green,
            // Matches the meta-derived "Merged" override in `ui.rs" so the
            // visual is the same whether the log carries the truth or gh does.
            WorkState::Merged => Color::Rgb(80, 160, 80),
            // `Done` items are filtered from the active pane today; color
            // only matters if a future view (e.g. "show recent completions")
            // surfaces them.
            WorkState::Done => Color::Rgb(80, 160, 80),
            WorkState::Skipped => Color::DarkGray,
            WorkState::Owned => Color::Red,
        }
    }

    /// Sort order for the work-items pane: things needing attention
    /// first, finished/skipped at the bottom.
    pub fn sort_rank(self) -> u8 {
        match self {
            WorkState::ReadyForMerge => 0,
            WorkState::WaitingForInput => 1,
            WorkState::Blocked => 2,
            WorkState::InReview => 3,
            WorkState::InProgress => 4,
            WorkState::Triaged => 5,
            WorkState::Owned => 6,
            WorkState::Skipped => 7,
            // Merged sits at the very bottom — terminal, no action needed.
            WorkState::Merged => 8,
            // Done is below Merged in rank but in practice filtered from
            // view; rank still defined for completeness and for any future
            // "show recent completions" toggle that needs an order.
            WorkState::Done => 9,
        }
    }
}

/// Stable identity of a work item. An issue when a board ticket is linked,
/// otherwise the PR itself — so PR work with no ticket is still tracked and
/// shown (every PR registered for visibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkKey {
    Issue(u64),
    Pr(u64),
}

#[derive(Debug, Clone)]
pub struct WorkItem {
    /// Stable identity — `Issue` when there's a ticket, else `Pr`.
    pub key: WorkKey,
    /// The linked board issue, when there is one. `None` for PR-only items.
    pub issue: Option<u64>,
    pub pr: Option<u64>,
    pub repo: String,
    pub state: WorkState,
    pub last_event: DateTime<FixedOffset>,
    pub last_action: String,
    /// Free-form note describing the state — e.g. the question text
    /// for Waiting for Input, the reason for Skipped, etc.
    pub note: String,
}

#[derive(Debug, Default)]
pub struct WorkItems {
    items: BTreeMap<WorkKey, WorkItem>,
    pr_to_issue: HashMap<u64, u64>,
}

impl WorkItems {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event into the aggregated state.
    pub fn ingest(&mut self, ev: &Event) {
        let (issue, pr) = identify_targets(ev, &self.pr_to_issue);

        // ANY event carrying both `pr=#M` and `issue=#N` pairs them — not just
        // `pr-opened`. If a PR-only item already exists for that PR (PR work
        // seen before the pairing — e.g. `act-start pr=#M` before `pr-merged
        // pr=#M issue=#N`), fold it onto the issue key so the work shows once,
        // under its ticket. Without this the orphan lingered (often as "Merged"
        // via gh metadata) and a later `card-done issue=#N` retired a *separate*
        // issue-keyed item, never the orphan.
        if let (Some(pr_n), Some(issue_n)) = (pr_from_event(ev), issue) {
            self.pr_to_issue.insert(pr_n, issue_n);
            if let Some(mut existing) = self.items.remove(&WorkKey::Pr(pr_n)) {
                existing.key = WorkKey::Issue(issue_n);
                existing.issue = Some(issue_n);
                self.items
                    .entry(WorkKey::Issue(issue_n))
                    .or_insert(existing);
            }
        }

        // Key by the issue when we know it; otherwise by the PR, so PR work
        // with no board ticket is still tracked and visible. Events tied to
        // neither (sweep summaries, etc.) aren't aggregated.
        let key = match (issue, pr) {
            (Some(i), _) => WorkKey::Issue(i),
            (None, Some(p)) => WorkKey::Pr(p),
            (None, None) => return,
        };

        let item = self.items.entry(key).or_insert_with(|| WorkItem {
            key,
            issue,
            pr,
            // Store the repo as the log recorded it (often a bare name); the
            // owner is resolved from the live roster at render time via
            // `app::resolve_repo`, which the ingest path can't see here.
            repo: ev.repo.clone(),
            state: WorkState::Triaged,
            last_event: ev.timestamp,
            last_action: ev.action.clone(),
            note: String::new(),
        });

        item.last_event = ev.timestamp;
        item.last_action = ev.action.clone();
        if pr.is_some() {
            item.pr = pr;
        }
        if issue.is_some() {
            item.issue = issue;
        }

        item.state = transition(item.state, ev);
        // A merged PR with no ticket has no board card or closing issue left
        // to retire it — the merge *is* its completion. Promote it straight
        // to Done so it leaves the active pane instead of lingering at Merged
        // forever. A ticketed PR is untouched here: it stays Merged until
        // `card-done` fires once its closing issue is closed.
        if ev.action == "pr-merged" && item.issue.is_none() {
            item.state = WorkState::Done;
        }
        item.note = note_for(ev, item.state);
    }

    /// Look up a work item by issue number. Used by the story view to
    /// resolve the issue → PR + repo trio without re-scanning `sorted()`.
    pub fn get(&self, issue: u64) -> Option<&WorkItem> {
        self.items.get(&WorkKey::Issue(issue))
    }

    /// Items sorted for display — attention-needing on top.
    pub fn sorted(&self) -> Vec<&WorkItem> {
        let mut v: Vec<&WorkItem> = self.items.values().collect();
        v.sort_by(|a, b| {
            a.state
                .sort_rank()
                .cmp(&b.state.sort_rank())
                .then_with(|| b.last_event.cmp(&a.last_event))
        });
        v
    }
}

/// Pull the issue/pr the event refers to, mapping pr→issue when possible.
fn identify_targets(ev: &Event, pr_to_issue: &HashMap<u64, u64>) -> (Option<u64>, Option<u64>) {
    let mut issue = None;
    let mut pr = None;
    for r in &ev.refs {
        match r.kind {
            // `#0` is never a real issue — older pr-review-fixup sweeps wrote
            // `issue=#0` as a "no closing ticket" sentinel. Treat it as no
            // ticket so those PRs key by the PR and retire like any other
            // ticketless merge, instead of collecting under a phantom #0.
            RefKind::Issue if r.number != 0 => issue.get_or_insert(r.number),
            RefKind::Issue => continue,
            RefKind::Pr => pr.get_or_insert(r.number),
            // new_issue refers to a *spawned* item (e.g. follow-up); it's
            // its own work-item track, not part of the surfacing PR's.
            RefKind::NewIssue => continue,
        };
    }
    // If we only have a pr=#M but no issue=#N, resolve via the cached map.
    if issue.is_none() {
        if let Some(p) = pr {
            if let Some(i) = pr_to_issue.get(&p) {
                issue = Some(*i);
            }
        }
    }
    (issue, pr)
}

fn pr_from_event(ev: &Event) -> Option<u64> {
    ev.refs
        .iter()
        .find(|r| r.kind == RefKind::Pr)
        .map(|r| r.number)
}

fn transition(current: WorkState, ev: &Event) -> WorkState {
    use WorkState::*;
    match ev.action.as_str() {
        "pick" => Triaged,
        "card-in-progress" | "branch" => InProgress,
        "pr-opened" => InReview,
        // PR-side activity. A force-push (or apply/rebase/gemini-retrigger)
        // does NOT toggle GitHub's draft flag — once `ready-flipped` has
        // moved the work item past InReview, subsequent PR activity must
        // not demote it back. Terminal states stay terminal. Anything
        // earlier or InReview itself is unaffected.
        "applied" | "rebased" | "force-push" | "gemini-retriggered" | "behind-main"
        | "act-start" | "act-end" => match current {
            ReadyForMerge | Merged | Done | Skipped | Owned => current,
            _ => InReview,
        },
        "ambiguous" | "pause" | "question" => WaitingForInput,
        "blocked" => Blocked,
        // `unblocked` — work resumes. We don't track a "previous" state
        // to restore, so the next real event (act-start, branch, etc.)
        // will move the item to the right bucket. In the meantime,
        // InProgress signals "actively moving again."
        "unblocked" => InProgress,
        "ready-flipped" => ReadyForMerge,
        // Emitted by pr-review-fixup sweep when a previously-open agent PR
        // is observed as MERGED. Terminal: no further events transition
        // out of Merged (except `card-done` which retires it further).
        "pr-merged" => Merged,
        // Emitted by the sweep after `pr-merged` + a successful `move_card`
        // to Done on the closing issue. Strictly more terminal than Merged;
        // filtered from the active pane by `visible_work_items` in `ui.rs`.
        "card-done" => Done,
        "skip" => Skipped,
        "skip-owned" => Owned,
        // Supervisor explicitly took an owned issue → drop back to InProgress.
        "take-owned" => InProgress,
        // followup-filed is a side effect on the surfacing PR, not a state change.
        // issue-commented is informational. assigned likewise.
        _ => current,
    }
}

fn note_for(ev: &Event, state: WorkState) -> String {
    match state {
        WorkState::WaitingForInput => {
            // Pull a `reason=` segment if present.
            extract_key(&ev.details, "reason=").unwrap_or_else(|| ev.action.clone())
        }
        WorkState::Blocked => {
            // Prefer detail=<short> as the human label; fall back to
            // reason=<normalized> so the user sees something either way.
            extract_key(&ev.details, "detail=")
                .or_else(|| extract_key(&ev.details, "reason="))
                .unwrap_or_default()
        }
        WorkState::Skipped => extract_key(&ev.details, "reason=").unwrap_or_default(),
        WorkState::Owned => extract_key(&ev.details, "current=").unwrap_or_default(),
        _ => String::new(),
    }
}

fn extract_key(details: &str, key: &str) -> Option<String> {
    let start = details.find(key)? + key.len();
    let end = details[start..]
        .find(' ')
        .map(|i| start + i)
        .unwrap_or(details.len());
    Some(details[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(line: &str) -> Event {
        Event::from_log_line(line).expect("test line must parse")
    }

    #[test]
    fn lifecycle_to_in_review() {
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-10T14:17:01+02:00\tboard-issue-loop\tweb\tpick\tissue=#90",
        ));
        w.ingest(&ev(
            "2026-05-10T14:17:02+02:00\tboard-issue-loop\tweb\tcard-in-progress\tissue=#90",
        ));
        w.ingest(&ev(
            "2026-05-10T14:17:03+02:00\tboard-issue-loop\tweb\tbranch\tname=fix/issue-90-x",
        ));
        w.ingest(&ev(
            "2026-05-10T14:17:04+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90",
        ));
        let items = w.sorted();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].issue, Some(90));
        assert_eq!(items[0].pr, Some(98));
        assert_eq!(items[0].state, WorkState::InReview);
    }

    #[test]
    fn pr_only_event_creates_pr_keyed_item() {
        // A PR with no linked issue (pr-review-fixup acting on an open PR
        // that has no board ticket). It must still show up as a work item.
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-22T15:00:00+02:00\tpr-review-fixup\tapi\tact-start\tpr=#52",
        ));
        let items = w.sorted();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].issue, None, "no board ticket");
        assert_eq!(items[0].pr, Some(52));
        assert_eq!(items[0].key, WorkKey::Pr(52));
    }

    #[test]
    fn pr_opened_promotes_pr_only_item_to_issue() {
        let mut w = WorkItems::new();
        // PR work seen first with no issue → PR-only item.
        w.ingest(&ev(
            "2026-05-22T15:00:00+02:00\tpr-review-fixup\tapi\tact-start\tpr=#52",
        ));
        assert_eq!(w.sorted().len(), 1);
        // Linkage appears later → fold onto the issue, still a single item.
        w.ingest(&ev(
            "2026-05-22T15:05:00+02:00\tboard-issue-loop\tapi\tpr-opened\tpr=#52 issue=#40",
        ));
        let items = w.sorted();
        assert_eq!(items.len(), 1, "promoted, not duplicated");
        assert_eq!(items[0].issue, Some(40));
        assert_eq!(items[0].pr, Some(52));
        assert_eq!(items[0].key, WorkKey::Issue(40));
    }

    #[test]
    fn pr_only_event_resolves_to_issue() {
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-10T14:17:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90",
        ));
        // Subsequent pr-only event should resolve.
        w.ingest(&ev(
            "2026-05-11T09:30:00+02:00\tpr-review-fixup\tweb\tapplied\tpr=#98 comment=1",
        ));
        let items = w.sorted();
        assert_eq!(items[0].pr, Some(98));
        assert_eq!(items[0].state, WorkState::InReview);
    }

    #[test]
    fn waiting_for_input() {
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-10T14:17:00+02:00\tboard-issue-loop\tweb\tpick\tissue=#90",
        ));
        w.ingest(&ev("2026-05-10T14:17:05+02:00\tboard-issue-loop\tweb\tpause\tissue=#90 reason=scope-too-big"));
        let items = w.sorted();
        assert_eq!(items[0].state, WorkState::WaitingForInput);
        assert_eq!(items[0].note, "scope-too-big");
    }

    #[test]
    fn ready_for_merge_after_flip() {
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-10T14:17:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90",
        ));
        w.ingest(&ev("2026-05-11T09:30:00+02:00\tpr-review-fixup\tweb\tready-flipped\tpr=#98 by=supervisor-delegated"));
        let items = w.sorted();
        assert_eq!(items[0].state, WorkState::ReadyForMerge);
    }

    #[test]
    fn blocked_then_unblocked() {
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-11T10:00:00+02:00\tboard-issue-loop\tweb\tpick\tissue=#79",
        ));
        w.ingest(&ev("2026-05-11T10:01:00+02:00\tboard-issue-loop\tweb\tblocked\tissue=#79 reason=release-gate detail=v1.0-cut-pending"));
        let items = w.sorted();
        assert_eq!(items[0].state, WorkState::Blocked);
        assert_eq!(items[0].note, "v1.0-cut-pending");

        // Resume.
        w.ingest(&ev(
            "2026-05-12T08:00:00+02:00\tboard-issue-loop\tweb\tunblocked\tissue=#79",
        ));
        let items = w.sorted();
        assert_eq!(items[0].state, WorkState::InProgress);
    }

    #[test]
    fn force_push_after_ready_flipped_keeps_ready() {
        // Regression: previously a force-push on a ready-flipped PR demoted
        // it back to InReview, making the TUI label go from "Ready for
        // Merge" to "In Review (draft)" — even though GitHub still has
        // isDraft=false. force-push doesn't toggle draft on GitHub, so
        // the work item shouldn't transition.
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-11T10:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#103 issue=#101",
        ));
        w.ingest(&ev("2026-05-11T12:25:00+02:00\tpr-review-fixup\tweb\tready-flipped\tpr=#103 by=supervisor-delegated"));
        assert_eq!(w.sorted()[0].state, WorkState::ReadyForMerge);

        w.ingest(&ev(
            "2026-05-11T12:30:00+02:00\tpr-review-fixup\tweb\tforce-push\tpr=#103 sha=abc1234",
        ));
        w.ingest(&ev(
            "2026-05-11T12:31:00+02:00\tpr-review-fixup\tweb\tapplied\tpr=#103 comment=42",
        ));
        w.ingest(&ev(
            "2026-05-11T12:32:00+02:00\tpr-review-fixup\tweb\trebased\tpr=#103",
        ));
        assert_eq!(w.sorted()[0].state, WorkState::ReadyForMerge);
    }

    #[test]
    fn pr_merged_is_terminal_and_sorts_last() {
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-10T14:17:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90",
        ));
        w.ingest(&ev("2026-05-10T14:17:05+02:00\tboard-issue-loop\tweb\tready-flipped\tpr=#98 by=supervisor-delegated"));
        // Active item competing for the top spot.
        w.ingest(&ev(
            "2026-05-11T08:00:00+02:00\tboard-issue-loop\tweb\tpick\tissue=#101",
        ));
        w.ingest(&ev(
            "2026-05-11T08:01:00+02:00\tboard-issue-loop\tweb\tcard-in-progress\tissue=#101",
        ));
        // Now the PR is observed as merged.
        w.ingest(&ev(
            "2026-05-11T12:00:00+02:00\tpr-review-fixup\tweb\tpr-merged\tpr=#98 issue=#90",
        ));

        let items = w.sorted();
        // Merged sits below active work, regardless of recency.
        let labels: Vec<&'static str> = items.iter().map(|i| i.state.label()).collect();
        assert_eq!(labels, vec!["In Progress", "Merged"]);
    }

    #[test]
    fn card_done_supersedes_merged() {
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-10T14:17:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90",
        ));
        w.ingest(&ev(
            "2026-05-11T12:00:00+02:00\tpr-review-fixup\tweb\tpr-merged\tpr=#98 issue=#90",
        ));
        assert_eq!(w.sorted()[0].state, WorkState::Merged);

        w.ingest(&ev(
            "2026-05-11T12:00:05+02:00\tboard-issue-loop\tweb\tcard-done\tissue=#90",
        ));
        assert_eq!(w.sorted()[0].state, WorkState::Done);
    }

    #[test]
    fn pr_merged_pairing_folds_orphan_so_card_done_retires() {
        // The web bug: PR work seen first WITHOUT the issue linkage,
        // so an orphan PR-only item exists; pr-merged carries the pairing, and
        // card-done must then retire the *folded* item — not leave the orphan.
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-06-01T10:00:00+02:00\tpr-review-fixup\tweb\tact-start\tpr=#150",
        ));
        assert_eq!(w.sorted().len(), 1);
        assert_eq!(w.sorted()[0].key, WorkKey::Pr(150), "orphan keyed by PR");

        w.ingest(&ev(
            "2026-06-01T10:01:00+02:00\tpr-review-fixup\tweb\tpr-merged\tpr=#150 issue=#116",
        ));
        assert_eq!(w.sorted().len(), 1, "folded — no duplicate row");
        assert_eq!(
            w.sorted()[0].key,
            WorkKey::Issue(116),
            "re-keyed onto the issue"
        );
        assert_eq!(w.sorted()[0].pr, Some(150));
        assert_eq!(w.sorted()[0].state, WorkState::Merged);

        w.ingest(&ev(
            "2026-06-01T10:02:00+02:00\tboard-issue-loop\tweb\tcard-done\tissue=#116",
        ));
        assert_eq!(w.sorted().len(), 1);
        assert_eq!(
            w.sorted()[0].state,
            WorkState::Done,
            "card-done retires the merged item"
        );
    }

    #[test]
    fn ticketless_pr_merged_goes_straight_to_done() {
        let mut w = WorkItems::new();
        // A PR with no linked ticket — keyed by the PR itself.
        w.ingest(&ev(
            "2026-05-10T14:17:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#60",
        ));
        w.ingest(&ev(
            "2026-05-10T14:17:05+02:00\tboard-issue-loop\tweb\tready-flipped\tpr=#60",
        ));
        assert_eq!(w.sorted()[0].state, WorkState::ReadyForMerge);

        // Observed merged. With no card/issue to retire it, the merge is its
        // completion — straight to Done (vs. a ticketed PR, which stays
        // Merged until card-done).
        w.ingest(&ev(
            "2026-05-11T12:00:00+02:00\tpr-review-fixup\tweb\tpr-merged\tpr=#60",
        ));
        let item = &w.sorted()[0];
        assert_eq!(item.state, WorkState::Done);
        assert!(item.issue.is_none(), "still ticketless");
        assert_eq!(item.pr, Some(60));
    }

    #[test]
    fn issue_zero_sentinel_is_treated_as_ticketless() {
        // Older sweeps wrote `issue=#0` for a no-ticket PR; it must retire to
        // Done (ticketless), not linger under a phantom issue #0.
        let mut w = WorkItems::new();
        w.ingest(&ev(
            "2026-05-31T00:23:08+02:00\tpr-review-fixup\tweb\tpr-merged\tpr=#148 issue=#0",
        ));
        let items = w.sorted();
        assert_eq!(items.len(), 1, "no phantom #0 item");
        let item = &items[0];
        assert_eq!(item.key, WorkKey::Pr(148), "keyed by the PR, not issue #0");
        assert!(item.issue.is_none(), "#0 is not a real ticket");
        assert_eq!(item.state, WorkState::Done, "ticketless merge retires");
    }

    #[test]
    fn sort_attention_first() {
        let mut w = WorkItems::new();
        // Skipped item, oldest.
        w.ingest(&ev("2026-05-09T10:00:00+02:00\tboard-issue-loop\tweb\tskip\tissue=#80 reason=assigned-to-other"));
        // In progress item, mid-age.
        w.ingest(&ev(
            "2026-05-10T14:17:00+02:00\tboard-issue-loop\tweb\tpick\tissue=#90",
        ));
        w.ingest(&ev(
            "2026-05-10T14:18:00+02:00\tboard-issue-loop\tweb\tbranch\tissue=#90 name=x",
        ));
        // Ready-for-merge, newest. Should be first.
        w.ingest(&ev(
            "2026-05-11T09:00:00+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#100 issue=#95",
        ));
        w.ingest(&ev("2026-05-11T09:30:00+02:00\tpr-review-fixup\tweb\tready-flipped\tpr=#100 by=supervisor-delegated"));

        let items = w.sorted();
        let labels: Vec<&'static str> = items.iter().map(|i| i.state.label()).collect();
        assert_eq!(labels, vec!["Ready for Merge", "In Progress", "Skipped"]);
    }
}
