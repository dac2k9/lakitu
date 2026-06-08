//! One parsed line of the agent activity log.
//!
//! The log is tab-separated:
//!   `<ISO-8601 ts>\t<skill>\t<repo>\t<action>\t<details>`
//!
//! `details` is free-form key=value pairs separated by spaces; we extract
//! known keys (issue=#N, pr=#N, new_issue=#N) so the UI can render them
//! as clickable hyperlinks.

use chrono::{DateTime, FixedOffset};
use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct Event {
    pub timestamp: DateTime<FixedOffset>,
    pub skill: String,
    pub repo: String,
    pub action: String,
    pub details: String,
    /// Parsed `(kind, number)` pairs from `details`: `("pr", 98)`,
    /// `("issue", 90)`, `("new_issue", 101)`. Used to render hyperlinks.
    pub refs: Vec<Reference>,
}

#[derive(Debug, Clone)]
pub struct Reference {
    pub kind: RefKind,
    pub number: u64,
    /// Byte offset within `details` where this ref starts (after the
    /// `kind=` prefix). The TUI uses this for click hit-testing.
    pub start: usize,
    /// Byte length of the `#N` token (`#` plus digits).
    pub len: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    Issue,
    Pr,
    NewIssue,
}

impl RefKind {
    pub fn url(&self, repo: &str, number: u64) -> String {
        let path = match self {
            RefKind::Pr => "pull",
            RefKind::Issue | RefKind::NewIssue => "issues",
        };
        format!("https://github.com/{repo}/{path}/{number}")
    }
}

impl Event {
    /// Parse one log line. Returns `None` for malformed input — we'd
    /// rather skip a bad line than crash the TUI.
    pub fn from_log_line(line: &str) -> Option<Event> {
        let mut parts = line.splitn(5, '\t');
        let ts_str = parts.next()?;
        let skill = parts.next()?.to_string();
        let repo = parts.next()?.to_string();
        let action = parts.next()?.to_string();
        let details = parts.next().unwrap_or("").to_string();

        let timestamp = DateTime::parse_from_rfc3339(ts_str).ok()?;
        let refs = extract_refs(&details);

        Some(Event {
            timestamp,
            skill,
            repo,
            action,
            details,
            refs,
        })
    }

    /// Background color for the action cell — see CLAUDE-Code-style log
    /// colors in the spec.
    pub fn action_color(&self) -> Color {
        match self.action.as_str() {
            "pick" | "pr-opened" | "applied" | "rebased" | "gemini-retriggered"
            | "ready-flipped" => Color::Green,
            "sweep" | "act-start" | "act-end" | "behind-main" | "force-push" => Color::Cyan,
            "ambiguous" | "disagreed" | "skip" | "skip-owned" | "pause" | "question" => {
                Color::Yellow
            }
            a if a.contains("conflict") || a.contains("error") || a.contains("failed") => {
                Color::Red
            }
            "followup-filed" | "issue-commented" | "assigned" | "card-in-progress" | "branch"
            | "take-owned" | "scope-override" | "bundle" => Color::Magenta,
            _ => Color::White,
        }
    }

    pub fn skill_color(&self) -> Color {
        match self.skill.as_str() {
            "pr-review-fixup" => Color::LightMagenta,
            "board-issue-loop" => Color::LightCyan,
            _ => Color::Gray,
        }
    }
}

/// Find `issue=#N`, `pr=#N`, and `new_issue=#N` references in details.
/// Order matters: scan for `new_issue=` first since `issue=` is a substring.
fn extract_refs(details: &str) -> Vec<Reference> {
    let mut refs = Vec::new();
    let patterns: &[(&str, RefKind)] = &[
        ("new_issue=#", RefKind::NewIssue),
        ("issue=#", RefKind::Issue),
        ("pr=#", RefKind::Pr),
    ];

    let mut cursor = 0;
    while cursor < details.len() {
        // Skip ahead to whichever next pattern occurs first.
        let next_match = patterns
            .iter()
            .filter_map(|(prefix, kind)| {
                details[cursor..]
                    .find(prefix)
                    .map(|idx| (cursor + idx, *prefix, *kind))
            })
            .min_by_key(|(idx, _, _)| *idx);

        let Some((match_start, prefix, kind)) = next_match else {
            break;
        };
        let num_start = match_start + prefix.len() - 1; // include the '#'

        // Read consecutive digits after the '#'.
        let rest = &details[num_start + 1..];
        let digit_len: usize = rest.chars().take_while(|c| c.is_ascii_digit()).count();
        if digit_len == 0 {
            cursor = match_start + prefix.len();
            continue;
        }
        let number_str = &rest[..digit_len];
        if let Ok(number) = number_str.parse::<u64>() {
            refs.push(Reference {
                kind,
                number,
                start: num_start,
                len: 1 + digit_len, // '#' + digits
            });
        }
        cursor = num_start + 1 + digit_len;
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_line() {
        let line = "2026-05-10T14:17:01+02:00\tboard-issue-loop\tweb\tpr-opened\tpr=#98 issue=#90";
        let ev = Event::from_log_line(line).unwrap();
        assert_eq!(ev.skill, "board-issue-loop");
        assert_eq!(ev.action, "pr-opened");
        assert_eq!(ev.refs.len(), 2);
        assert_eq!(ev.refs[0].kind, RefKind::Pr);
        assert_eq!(ev.refs[0].number, 98);
        assert_eq!(ev.refs[1].kind, RefKind::Issue);
        assert_eq!(ev.refs[1].number, 90);
    }

    #[test]
    fn parses_new_issue_first() {
        // `new_issue=#101` must not be misclassified as `issue=#101`.
        let line = "2026-05-10T14:17:01+02:00\tpr-review-fixup\tweb\tfollowup-filed\tpr=#98 new_issue=#101";
        let ev = Event::from_log_line(line).unwrap();
        let kinds: Vec<RefKind> = ev.refs.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![RefKind::Pr, RefKind::NewIssue]);
    }

    #[test]
    fn ref_url_pr() {
        let url = RefKind::Pr.url("acme/web", 98);
        assert_eq!(url, "https://github.com/acme/web/pull/98");
    }

    #[test]
    fn ref_url_issue() {
        let url = RefKind::Issue.url("acme/web", 90);
        assert_eq!(url, "https://github.com/acme/web/issues/90");
    }

    #[test]
    fn malformed_line_returns_none() {
        assert!(Event::from_log_line("not enough\ttabs").is_none());
        assert!(Event::from_log_line("").is_none());
    }
}
