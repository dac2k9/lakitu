//! Centralized, XDG-compliant resolution of lakitu's on-disk locations.
//!
//! Everything lakitu persists is *state* — agent registry/presence, inboxes,
//! tasks, personas, and the structured action log. That is neither config nor
//! cache, so per the [XDG Base Directory spec] it belongs under
//! `$XDG_STATE_HOME` (default `~/.local/state`).
//!
//! Historically lakitu wrote under `~/.claude/lakitu-fleet` (and the log under
//! `~/.claude/logs`). To avoid orphaning a running fleet on upgrade, we keep
//! reading the legacy location *when it already exists* — fresh installs land in
//! the XDG location, existing ones stay put until the user moves them. An
//! explicit `$LAKITU_FLEET_ROOT` / `$GENBOT_ROOT` override always wins.
//!
//! Every store/log path resolver in the crate delegates here so the resolution
//! rules can't drift between the writers (MCP/daemon) and the reader (TUI).
//!
//! [XDG Base Directory spec]: https://specifications.freedesktop.org/basedir-spec/latest/

use std::path::PathBuf;

/// `$HOME`, or `/tmp` when it is unset/empty (matches the pre-XDG fallback so
/// behavior in a HOME-less environment is unchanged).
fn home() -> PathBuf {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from("/tmp"),
    }
}

/// The XDG state base: `$XDG_STATE_HOME` if set (and non-empty), else
/// `~/.local/state`.
fn state_home() -> PathBuf {
    match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(".local").join("state"),
    }
}

/// The pre-XDG store root: `~/.claude/lakitu-fleet`.
fn legacy_store_root() -> PathBuf {
    home().join(".claude").join("lakitu-fleet")
}

/// The fleet multi-agent store root.
///
/// Resolution order:
/// 1. `$LAKITU_FLEET_ROOT` / `$GENBOT_ROOT` — explicit override (daemon,
///    relocated/remote store, tests).
/// 2. `$XDG_STATE_HOME/lakitu/fleet` if it already exists.
/// 3. The legacy `~/.claude/lakitu-fleet` if *it* exists (don't orphan a
///    running fleet on upgrade).
/// 4. Otherwise the XDG location — so fresh installs land in the right place.
pub fn fleet_store_root() -> PathBuf {
    for var in ["LAKITU_FLEET_ROOT", "GENBOT_ROOT"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    let xdg = state_home().join("lakitu").join("fleet");
    if xdg.exists() {
        return xdg;
    }
    let legacy = legacy_store_root();
    if legacy.exists() {
        return legacy;
    }
    xdg
}

/// Path to lakitu's structured action log (`agent-actions.log`): appended by
/// `emit_event` and the web cockpit, tailed by the TUI's activity feed.
///
/// Lives at `$XDG_STATE_HOME/lakitu/logs/agent-actions.log`, falling back to the
/// legacy `~/.claude/logs/agent-actions.log` when that file already exists (so
/// the reader and writers agree on which file is live).
pub fn agent_actions_log() -> PathBuf {
    let xdg = state_home()
        .join("lakitu")
        .join("logs")
        .join("agent-actions.log");
    if xdg.exists() {
        return xdg;
    }
    let legacy = home()
        .join(".claude")
        .join("logs")
        .join("agent-actions.log");
    if legacy.exists() {
        return legacy;
    }
    xdg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet::TEST_ENV_LOCK;

    /// Snapshot + restore the env vars these tests mutate, so they compose with
    /// the other HOME-mutating tests under the shared lock.
    struct EnvGuard(Vec<(&'static str, Option<String>)>);
    impl EnvGuard {
        fn capture() -> Self {
            let vars = ["HOME", "XDG_STATE_HOME", "LAKITU_FLEET_ROOT", "GENBOT_ROOT"];
            EnvGuard(vars.iter().map(|&k| (k, std::env::var(k).ok())).collect())
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.0 {
                // SAFETY: TEST_ENV_LOCK serializes the env-mutating tests.
                unsafe {
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }
    }

    #[test]
    fn explicit_override_wins_over_everything() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::capture();
        unsafe {
            std::env::set_var("LAKITU_FLEET_ROOT", "/tmp/lakitu-override");
            std::env::set_var("XDG_STATE_HOME", "/tmp/whatever");
            std::env::remove_var("GENBOT_ROOT");
        }
        assert_eq!(fleet_store_root(), PathBuf::from("/tmp/lakitu-override"));
    }

    #[test]
    fn fresh_install_lands_in_xdg_state() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::capture();
        let tmp = std::env::temp_dir().join(format!("lakitu-xdg-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let xdg = tmp.join("state");
        let home = tmp.join("home");
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_STATE_HOME", &xdg);
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }
        // Neither location exists yet → fresh installs land in XDG.
        assert_eq!(fleet_store_root(), xdg.join("lakitu").join("fleet"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn legacy_store_is_kept_when_it_exists() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::capture();
        let tmp = std::env::temp_dir().join(format!("lakitu-xdg-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let xdg = tmp.join("state");
        let home = tmp.join("home");
        let legacy = home.join(".claude").join("lakitu-fleet");
        std::fs::create_dir_all(&legacy).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_STATE_HOME", &xdg);
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }
        // XDG dir absent but legacy present → keep using legacy (don't orphan it).
        assert_eq!(fleet_store_root(), legacy);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn xdg_wins_once_it_exists_even_if_legacy_also_does() {
        let _env = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = EnvGuard::capture();
        let tmp = std::env::temp_dir().join(format!("lakitu-xdg-both-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let xdg = tmp.join("state");
        let home = tmp.join("home");
        let xdg_store = xdg.join("lakitu").join("fleet");
        std::fs::create_dir_all(&xdg_store).unwrap();
        std::fs::create_dir_all(home.join(".claude").join("lakitu-fleet")).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_STATE_HOME", &xdg);
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }
        assert_eq!(fleet_store_root(), xdg_store);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
