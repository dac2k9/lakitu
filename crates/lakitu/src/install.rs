//! `lakitu install-hooks` — materialize the fleet lifecycle hooks and the
//! coordination skill into `~/.claude`, and wire the hooks into `settings.json`.
//!
//! The hook scripts + skill are embedded in the binary (see `assets/`), so a
//! `cargo install lakitu` user gets a working fleet without cloning the
//! repo. Idempotent: re-running only adds what's missing, and `settings.json`
//! is backed up before it's touched.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// The lifecycle hooks, embedded from `assets/hooks/` at build time.
const HOOKS: &[(&str, &str)] = &[
    (
        "state-hook.sh",
        include_str!("../assets/hooks/state-hook.sh"),
    ),
    (
        "inbox-check.sh",
        include_str!("../assets/hooks/inbox-check.sh"),
    ),
    (
        "context-statusline.sh",
        include_str!("../assets/hooks/context-statusline.sh"),
    ),
    (
        "persona-sessionstart.sh",
        include_str!("../assets/hooks/persona-sessionstart.sh"),
    ),
    (
        "tasks-sessionstart.sh",
        include_str!("../assets/hooks/tasks-sessionstart.sh"),
    ),
];

const SKILL: &str = include_str!("../assets/skill/fleet-coordination/SKILL.md");

/// (hook event, script, optional arg) — the wiring the hooks expect. Mirrors
/// `scripts/install-fleet.sh`.
const WIRING: &[(&str, &str, Option<&str>)] = &[
    ("PreToolUse", "state-hook.sh", Some("working")),
    ("PermissionRequest", "state-hook.sh", Some("blocked")),
    ("SessionEnd", "state-hook.sh", Some("offline")),
    ("SessionStart", "state-hook.sh", Some("idle")),
    ("SessionStart", "persona-sessionstart.sh", None),
    ("SessionStart", "tasks-sessionstart.sh", None),
    ("Stop", "inbox-check.sh", None),
    ("Stop", "state-hook.sh", Some("idle")),
];

pub fn run() -> Result<()> {
    let home = home_dir()?;
    let claude = home.join(".claude");
    let fleet = fleet_root(&home);

    // 1. Hook scripts → the fleet store dir (executable).
    std::fs::create_dir_all(&fleet).with_context(|| format!("creating {}", fleet.display()))?;
    for (name, body) in HOOKS {
        let path = fleet.join(name);
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        make_executable(&path)?;
    }
    println!(
        "✓ installed {} fleet hooks → {}",
        HOOKS.len(),
        fleet.display()
    );

    // 2. Coordination skill → ~/.claude/skills/.
    let skill_dir = claude.join("skills").join("fleet-coordination");
    std::fs::create_dir_all(&skill_dir)?;
    std::fs::write(skill_dir.join("SKILL.md"), SKILL)?;
    println!(
        "✓ installed skill → {}",
        skill_dir.join("SKILL.md").display()
    );

    // 3. Wire the hooks into settings.json (idempotent, with a backup).
    wire_settings(&claude.join("settings.json"), &fleet)?;

    print_next_steps(&fleet);
    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `$LAKITU_FLEET_ROOT` (also honoring the legacy `$GENBOT_ROOT`) or the default
/// `~/.claude/lakitu-fleet`.
fn fleet_root(home: &Path) -> PathBuf {
    for var in ["LAKITU_FLEET_ROOT", "GENBOT_ROOT"] {
        if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
            return PathBuf::from(v);
        }
    }
    home.join(".claude").join("lakitu-fleet")
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn hook_command(fleet: &Path, script: &str, arg: Option<&str>) -> String {
    let mut c = format!("sh '{}/{}'", fleet.display(), script);
    if let Some(a) = arg {
        c.push(' ');
        c.push_str(a);
    }
    c
}

fn wire_settings(path: &Path, fleet: &Path) -> Result<()> {
    let mut cfg: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    let before = cfg.clone();

    {
        let obj = cfg
            .as_object_mut()
            .context("settings.json is not a JSON object")?;
        let hooks = obj
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .context("settings.json `hooks` is not an object")?;
        for (event, script, arg) in WIRING {
            let command = hook_command(fleet, script, *arg);
            let blocks = hooks
                .entry((*event).to_string())
                .or_insert_with(|| json!([]));
            let arr = blocks
                .as_array_mut()
                .with_context(|| format!("hooks.{event} is not an array"))?;
            let present = arr.iter().any(|blk| {
                blk.get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hs| {
                        hs.iter()
                            .any(|hk| hk.get("command").and_then(|c| c.as_str()) == Some(&command))
                    })
                    .unwrap_or(false)
            });
            if !present {
                arr.push(json!({
                    "matcher": "*",
                    "hooks": [{ "type": "command", "command": command, "timeout": 10 }],
                }));
            }
        }
    }

    // statusLine — the context % + rate-limit chip.
    let sl_command = format!("sh '{}/context-statusline.sh'", fleet.display());
    let sl_present = cfg
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(|c| c.as_str())
        == Some(sl_command.as_str());
    if !sl_present {
        cfg.as_object_mut().unwrap().insert(
            "statusLine".to_string(),
            json!({ "type": "command", "command": sl_command, "padding": 0 }),
        );
    }

    if cfg == before {
        println!("✓ settings.json already wired — no change");
        return Ok(());
    }
    if path.exists() {
        let ts = chrono::Local::now().format("%Y%m%dT%H%M%S");
        let bak = path.with_file_name(format!("settings.json.bak-{ts}"));
        std::fs::write(&bak, serde_json::to_string_pretty(&before)?)?;
        println!("  backed up → {}", bak.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&cfg)?))?;
    println!("✓ wired fleet hooks into {}", path.display());
    Ok(())
}

fn print_next_steps(fleet: &Path) {
    println!(
        "\nFleet hooks installed (store: {}). Two steps to bring an agent online:\n\
         \n  1. Point the agent's MCP at `lakitu mcp` — in its .mcp.json (or ~/.claude.json):\n\
         \n       {{ \"mcpServers\": {{ \"lakitu-mcp\": {{ \"command\": \"lakitu\", \"args\": [\"mcp\"] }} }} }}\n\
         \n  2. Export a stable name so the hooks find its inbox/presence:\n\
         \n       export LAKITU_FLEET_NAME=<name>\n\
         \nThen watch the fleet from the cockpit:  lakitu",
        fleet.display()
    );
}
