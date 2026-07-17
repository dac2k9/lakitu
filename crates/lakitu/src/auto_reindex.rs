//! Auto-reindex-on-merge — the box-local trigger for repos whose CI can't
//! reach this daemon (e.g. fossid-toolbox: GitHub-cloud-only runners, no
//! tunnel to a loopback endpoint). Fully opt-in via env; inert by default so
//! a lakitu deployment with no graph-brain sibling pays nothing for it.
//!
//! Design (agreed with graph-brain's owner, 2026-07-02): lakitu is the
//! always-on component and already detects merges (the shared-task reconcile
//! loop's per-PR merge-edge tracking), so it owns the trigger — subprocess
//! the reindex command for any watched repo that just merged, then restart
//! itself to serve the fresh artifact. The reindex command is expected to be
//! idempotent/self-correcting (it diffs against its own last-indexed SHA), so
//! an imprecise or duplicate trigger is harmless, just a wasted no-op run —
//! this code doesn't need to guarantee exactly-once delivery.
//!
//! Config lives entirely in env (`LAKITU_AUTO_REINDEX_REPOS`/`_CMD`/
//! `_RESTART_CMD`), never a hardcoded path — this crate ships as OSS and most
//! deployments have no graph-brain sibling to point at.

use std::collections::BTreeSet;

pub(crate) struct ReindexConfig {
    repos: BTreeSet<String>,
    reindex_cmd: Vec<String>,
    restart_cmd: Option<Vec<String>>,
}

impl ReindexConfig {
    /// `None` if the feature isn't configured at all (the common case) or is
    /// misconfigured (repos set with no command to run it — logged, not
    /// fatal: a misconfigured auto-reindex must never take the daemon down).
    pub(crate) fn from_env() -> Option<Self> {
        let repos: BTreeSet<String> = std::env::var("LAKITU_AUTO_REINDEX_REPOS")
            .ok()
            .filter(|v| !v.trim().is_empty())?
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if repos.is_empty() {
            return None;
        }
        let Some(reindex_cmd) = parse_cmd("LAKITU_AUTO_REINDEX_CMD") else {
            tracing::warn!(
                "LAKITU_AUTO_REINDEX_REPOS is set but LAKITU_AUTO_REINDEX_CMD isn't \
                 -- auto-reindex-on-merge disabled"
            );
            return None;
        };
        let restart_cmd = parse_cmd("LAKITU_AUTO_REINDEX_RESTART_CMD");
        Some(Self {
            repos,
            reindex_cmd,
            restart_cmd,
        })
    }

    fn watches(&self, repo: &str) -> bool {
        self.repos.contains(repo)
    }
}

/// Whitespace-split, no shell involved anywhere in this module (the repo slug
/// is passed as a plain argument, never interpolated into a shell string) —
/// simple is safe here, and every path/arg in practice is space-free.
fn parse_cmd(var: &str) -> Option<Vec<String>> {
    let raw = std::env::var(var).ok()?;
    let argv: Vec<String> = raw.split_whitespace().map(str::to_string).collect();
    (!argv.is_empty()).then_some(argv)
}

/// Distinct repos among `newly_merged` that `config` watches — pure and
/// order-stable so 3 merged PRs to the same repo in one reconcile pass
/// collapse to a single trigger, and so it's directly testable without env
/// or subprocesses.
fn repos_to_trigger(config: &ReindexConfig, newly_merged: &[(String, u64)]) -> BTreeSet<String> {
    newly_merged
        .iter()
        .map(|(repo, _)| repo.clone())
        .filter(|r| config.watches(r))
        .collect()
}

/// Given this reconcile pass's newly-merged `(repo, pr_number)` pairs, fire a
/// reindex (then restart) for each distinct watched repo. Runs detached
/// (`tokio::spawn`) so a slow reindex never delays the reconcile loop's own
/// cadence. No-op entirely if the feature isn't configured.
pub(crate) fn trigger_for_merges(newly_merged: &[(String, u64)]) {
    let Some(config) = ReindexConfig::from_env() else {
        return;
    };
    for repo in repos_to_trigger(&config, newly_merged) {
        let reindex_cmd = config.reindex_cmd.clone();
        let restart_cmd = config.restart_cmd.clone();
        tokio::spawn(run_reindex_then_restart(repo, reindex_cmd, restart_cmd));
    }
}

/// Run `reindex_cmd <repo>`; only on success, run `restart_cmd`. Both are
/// awaited — for a real restart command that kills this process (the whole
/// point), the second await typically never observably completes, which is
/// fine; see its call site for why awaiting anyway is still the right call.
async fn run_reindex_then_restart(
    repo: String,
    reindex_cmd: Vec<String>,
    restart_cmd: Option<Vec<String>>,
) {
    tracing::info!(%repo, cmd = ?reindex_cmd, "auto-reindex: merge detected, running reindex");
    let status = tokio::process::Command::new(&reindex_cmd[0])
        .args(&reindex_cmd[1..])
        .arg(&repo)
        .status()
        .await;
    match status {
        Ok(s) if s.success() => tracing::info!(%repo, "auto-reindex: succeeded"),
        Ok(s) => {
            tracing::warn!(
                %repo, code = ?s.code(),
                "auto-reindex: reindex command exited non-zero, not restarting"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(%repo, error = %e, "auto-reindex: failed to spawn reindex command, not restarting");
            return;
        }
    }
    let Some(restart_cmd) = restart_cmd else {
        tracing::info!(
            %repo,
            "auto-reindex: no restart command configured -- restart the daemon to serve the fresh index"
        );
        return;
    };
    tracing::info!(%repo, cmd = ?restart_cmd, "auto-reindex: restarting daemon to serve the fresh index");
    // Awaited, even though a real restart command (e.g. start.sh) kills THIS
    // process as its first act -- meaning this await typically never
    // observably completes, the process just ends. That's fine, not a bug to
    // avoid: awaiting (vs. fire-and-forget spawn) costs nothing in that case,
    // and means a MISconfigured restart command that doesn't actually
    // restart anything is surfaced via a log line instead of silently eaten.
    match tokio::process::Command::new(&restart_cmd[0])
        .args(&restart_cmd[1..])
        .status()
        .await
    {
        Ok(s) if !s.success() => {
            tracing::warn!(%repo, code = ?s.code(), "auto-reindex: restart command exited non-zero");
        }
        Err(e) => tracing::warn!(%repo, error = %e, "auto-reindex: failed to spawn restart command"),
        Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every test in this module that touches
    /// LAKITU_AUTO_REINDEX_* env vars -- process-global state, and Rust runs
    /// tests in parallel by default.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_env() {
        for v in [
            "LAKITU_AUTO_REINDEX_REPOS",
            "LAKITU_AUTO_REINDEX_CMD",
            "LAKITU_AUTO_REINDEX_RESTART_CMD",
        ] {
            // SAFETY: caller holds ENV_LOCK for the duration of the test.
            unsafe { std::env::remove_var(v) };
        }
    }

    #[test]
    fn from_env_is_none_by_default() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        assert!(ReindexConfig::from_env().is_none());
    }

    #[test]
    fn from_env_is_none_when_repos_set_but_cmd_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        // SAFETY: holding ENV_LOCK for the duration of this test.
        unsafe { std::env::set_var("LAKITU_AUTO_REINDEX_REPOS", "fossid-ab/fossid-toolbox") };
        assert!(
            ReindexConfig::from_env().is_none(),
            "repos with no command to run must disable the feature, not panic or half-configure"
        );
        clear_env();
    }

    #[test]
    fn from_env_parses_repos_and_cmd() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        // SAFETY: holding ENV_LOCK for the duration of this test.
        unsafe {
            std::env::set_var(
                "LAKITU_AUTO_REINDEX_REPOS",
                " fossid-ab/fossid-toolbox , dac2k9/lakitu ",
            );
            std::env::set_var(
                "LAKITU_AUTO_REINDEX_CMD",
                "python3 /path/to/reindex.py --fetch",
            );
            std::env::set_var("LAKITU_AUTO_REINDEX_RESTART_CMD", "/path/to/start.sh");
        }
        let cfg = ReindexConfig::from_env().expect("both repos and cmd are set");
        assert!(cfg.watches("fossid-ab/fossid-toolbox"), "trims whitespace");
        assert!(cfg.watches("dac2k9/lakitu"));
        assert!(!cfg.watches("fossid-ab/fossid-mcp"), "unlisted repo unwatched");
        assert_eq!(
            cfg.reindex_cmd,
            vec!["python3", "/path/to/reindex.py", "--fetch"]
        );
        assert_eq!(
            cfg.restart_cmd,
            Some(vec!["/path/to/start.sh".to_string()])
        );
        clear_env();
    }

    #[test]
    fn from_env_restart_cmd_is_optional() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_env();
        // SAFETY: holding ENV_LOCK for the duration of this test.
        unsafe {
            std::env::set_var("LAKITU_AUTO_REINDEX_REPOS", "dac2k9/lakitu");
            std::env::set_var("LAKITU_AUTO_REINDEX_CMD", "true");
        }
        let cfg = ReindexConfig::from_env().expect("repos + cmd set, restart optional");
        assert_eq!(cfg.restart_cmd, None);
        clear_env();
    }

    #[test]
    fn repos_to_trigger_dedupes_and_filters_unwatched() {
        let cfg = ReindexConfig {
            repos: ["fossid-ab/fossid-toolbox".to_string()].into(),
            reindex_cmd: vec!["true".to_string()],
            restart_cmd: None,
        };
        let merged = vec![
            ("fossid-ab/fossid-toolbox".to_string(), 1),
            ("fossid-ab/fossid-toolbox".to_string(), 2), // same repo, different PR
            ("fossid-ab/fossid-mcp".to_string(), 3),     // unwatched
        ];
        let triggered = repos_to_trigger(&cfg, &merged);
        assert_eq!(
            triggered,
            ["fossid-ab/fossid-toolbox".to_string()].into(),
            "dedupes same-repo merges and drops unwatched repos"
        );
    }

    /// Exercises the actual subprocess-chaining logic for real (not mocked)
    /// using universally-available unix commands instead of depending on a
    /// real reindex.py: `true` always exits 0, so the restart command (here
    /// `touch <marker>`) must run.
    #[tokio::test]
    async fn reindex_success_triggers_restart() {
        let marker = std::env::temp_dir().join(format!(
            "lakitu-auto-reindex-test-marker-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        run_reindex_then_restart(
            "irrelevant/repo".to_string(),
            vec!["true".to_string()],
            Some(vec![
                "touch".to_string(),
                marker.to_str().unwrap().to_string(),
            ]),
        )
        .await;
        assert!(
            marker.exists(),
            "a successful reindex must run the restart command"
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// The failure-side counterpart: `false` always exits non-zero, so the
    /// restart command must NOT run — a failed reindex must never restart
    /// the daemon into a state it hasn't actually verified.
    #[tokio::test]
    async fn reindex_failure_skips_restart() {
        let marker = std::env::temp_dir().join(format!(
            "lakitu-auto-reindex-test-marker-fail-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        run_reindex_then_restart(
            "irrelevant/repo".to_string(),
            vec!["false".to_string()],
            Some(vec![
                "touch".to_string(),
                marker.to_str().unwrap().to_string(),
            ]),
        )
        .await;
        assert!(
            !marker.exists(),
            "a failed reindex must never run the restart command"
        );
    }
}
