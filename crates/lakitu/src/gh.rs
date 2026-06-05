//! Async enrichment via the `gh` CLI.
//!
//! Fetches title + lifecycle state for an issue or PR. Used by the TUI
//! to show human-readable labels and detect that a PR is actually
//! merged (the agent log alone can't tell us that — merges happen on
//! GitHub, not in the agent).
//!
//! All calls are best-effort: `gh` missing, network blip, 404, timeout
//! → cache a sentinel and don't retry. The TUI falls back to bare `#N`.

use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::event::RefKind;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// What we cache per issue/PR.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub title: Option<String>,
    /// True for PRs that have been merged into base. Always false for
    /// issues (which can be closed but not merged).
    pub merged: bool,
    /// True for issues closed or PRs closed-without-merge. Distinguishes
    /// "still moving" from "no further action expected."
    pub closed: bool,
}

/// Message sent back to the app once a fetch completes.
#[derive(Debug, Clone)]
pub struct MetaUpdate {
    pub kind: RefKind,
    pub number: u64,
    pub meta: Meta,
}

/// Does `repo` ("owner/name") resolve to a repo we can see on GitHub? Used to
/// decide whether an agent's role is a clickable link (don't link a 404). A
/// `gh` failure / 404 / missing-gh all read as "no" — best-effort, see module doc.
pub async fn repo_exists(repo: &str) -> bool {
    run_gh(&["api", &format!("repos/{repo}"), "--silent"]).await.is_some()
}

/// Fetch metadata for an issue. Owner+name in `repo` (`acme/web`).
pub async fn fetch_issue(repo: String, number: u64) -> Option<Meta> {
    let json = run_gh(&[
        "issue",
        "view",
        &number.to_string(),
        "--repo",
        &repo,
        "--json",
        "title,state",
    ])
    .await?;

    let title = pluck_string(&json, "\"title\":")?;
    let state = pluck_string(&json, "\"state\":")?;
    Some(Meta {
        title: Some(title),
        merged: false,
        closed: state == "CLOSED",
    })
}

/// Fetch metadata for a PR. Differs from issue in that the GraphQL
/// schema also exposes `mergedAt`/`state == MERGED`.
pub async fn fetch_pr(repo: String, number: u64) -> Option<Meta> {
    let json = run_gh(&[
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        &repo,
        "--json",
        "title,state",
    ])
    .await?;

    let title = pluck_string(&json, "\"title\":")?;
    let state = pluck_string(&json, "\"state\":")?;
    Some(Meta {
        title: Some(title),
        // PR state strings from gh: OPEN, CLOSED, MERGED.
        merged: state == "MERGED",
        closed: state == "CLOSED",
    })
}

/// Shell out to `gh`. Returns the raw JSON body on success, `None` on
/// any failure (gh missing, non-zero exit, timeout). Failures are
/// silent by design — see module doc.
async fn run_gh(args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("gh");
    cmd.args(args);
    let fut = cmd.output();
    let out = match timeout(FETCH_TIMEOUT, fut).await {
        Ok(Ok(out)) => out,
        Ok(Err(err)) => {
            tracing::debug!(?err, ?args, "gh invocation failed");
            return None;
        }
        Err(_) => {
            tracing::debug!(?args, "gh invocation timed out");
            return None;
        }
    };
    if !out.status.success() {
        tracing::debug!(
            stderr = %String::from_utf8_lossy(&out.stderr),
            ?args,
            "gh returned non-zero"
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Naive JSON value extractor. The gh CLI's `--json` output is well-formed
/// JSON but bringing in `serde_json` for two string fields is overkill.
/// We look for `"key":"value"` literals.
fn pluck_string(json: &str, key_prefix: &str) -> Option<String> {
    let idx = json.find(key_prefix)?;
    let after_key = &json[idx + key_prefix.len()..];
    let quote_open = after_key.find('"')?;
    let value_start = quote_open + 1;
    // Find the next un-escaped quote.
    let mut end = value_start;
    let bytes = after_key.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'"' && bytes[end - 1] != b'\\' {
            break;
        }
        end += 1;
    }
    if end >= bytes.len() {
        return None;
    }
    let raw = &after_key[value_start..end];
    Some(unescape_json_string(raw))
}

/// Minimal JSON string unescape — handles \", \\, \n, \t, \/. Doesn't
/// handle \uXXXX; if those appear in real titles we'll see partial
/// strings but won't crash.
fn unescape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pluck_simple() {
        let json = r#"{"title":"hello","state":"OPEN"}"#;
        assert_eq!(pluck_string(json, "\"title\":").as_deref(), Some("hello"));
        assert_eq!(pluck_string(json, "\"state\":").as_deref(), Some("OPEN"));
    }

    #[test]
    fn pluck_escaped_quote() {
        let json = r#"{"title":"say \"hi\""}"#;
        assert_eq!(pluck_string(json, "\"title\":").as_deref(), Some("say \"hi\""));
    }

    #[test]
    fn pluck_missing_key() {
        let json = r#"{"state":"OPEN"}"#;
        assert!(pluck_string(json, "\"title\":").is_none());
    }
}
