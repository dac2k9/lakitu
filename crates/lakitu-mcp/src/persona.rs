//! Persona store — each agent's self-authored identity and private notes on
//! peers, persisted under `~/.claude/lakitu-fleet/personas/<name>/` so an
//! agent's chosen name, character, and relationships survive across sessions
//! (and across compaction).
//!
//! Read side: the SessionStart hook (`persona-sessionstart.sh`) injects an
//! agent's identity + peer-notes back into context at the start of every
//! session — including after a `/compact` — so it resumes *being* itself
//! instead of re-inventing a character each time.
//! Write side: the MCP persona tools call the helpers here.
//!
//! Layout:
//! ```text
//! ~/.claude/lakitu-fleet/personas/<name>/
//!   identity.md      self-card prose (PUBLIC — peers read it via get_identity)
//!   identity.json    structured mirror { name, tagline, bio, updated }
//!   peers/<peer>.md  PRIVATE, append-only notes on one peer (+ affinity)
//! ```
//!
//! Self-cards are public (so relationships are grounded in how a peer
//! describes *themselves*, not hearsay); peer-notes are private to the
//! holder. Notes are append-only — a relationship has history.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::json;
use tokio::fs;

use crate::fleet::{sanitize, store_root};

fn personas_root() -> PathBuf {
    store_root().join("personas")
}

fn persona_dir(name: &str) -> PathBuf {
    personas_root().join(sanitize(name))
}

/// Build the SessionStart persona context for `name` (identity + private peer
/// notes), mirroring `persona-sessionstart.sh`'s `additionalContext`. `None`
/// when the agent has no persona on file yet, so fresh agents stay undisturbed.
pub async fn session_context(name: &str) -> Option<String> {
    let sname = sanitize(name);
    let dir = personas_root().join(&sname);

    let identity = fs::read_to_string(dir.join("identity.md"))
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut peers: Vec<(String, String)> = Vec::new();
    if let Ok(mut rd) = fs::read_dir(dir.join("peers")).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let peer = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if let Ok(body) = fs::read_to_string(&p).await {
                let body = body.trim().to_string();
                if !body.is_empty() {
                    peers.push((peer, body));
                }
            }
        }
    }
    peers.sort_by(|a, b| a.0.cmp(&b.0));

    if identity.is_none() && peers.is_empty() {
        return None;
    }

    let mut parts: Vec<String> = vec![
        "# Your lakitu persona (persisted identity — resume being this)".to_string(),
        format!(
            "You are **{sname}**. Below is your own self-authored identity and your \
private notes on teammates, restored from the fleet store so you carry across \
sessions and survive compaction. Speak and act as this character. Keep it \
current with the persona MCP tools: `set_identity` when who-you-are shifts, \
`remember_peer` when you form or update an impression of a teammate."
        ),
    ];
    if let Some(id) = identity {
        parts.push(format!("## Identity\n{id}"));
    }
    if !peers.is_empty() {
        let joined = peers
            .iter()
            .map(|(p, b)| format!("### {p}\n{b}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        parts.push(format!("## Your peers (private notes)\n{joined}"));
    }
    Some(parts.join("\n\n"))
}

fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// Write `bytes` to `path` atomically (temp sibling + rename), creating parent
/// dirs as needed. The temp name keeps the full filename + `.tmp` so two
/// atomic writes in the same dir (identity.json / identity.md) don't collide.
async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.to_string_lossy()));
    fs::write(&tmp, bytes).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

/// Render the structured card into the markdown the hook injects.
fn render_identity_md(doc: &serde_json::Value) -> String {
    let name = doc["name"].as_str().unwrap_or("unknown");
    let mut s = format!("# {name}\n");
    if let Some(t) = doc["tagline"].as_str().filter(|t| !t.trim().is_empty()) {
        s.push_str(&format!("\n> {t}\n"));
    }
    if let Some(b) = doc["bio"].as_str().filter(|b| !b.trim().is_empty()) {
        s.push_str(&format!("\n{b}\n"));
    }
    if let Some(u) = doc["updated"].as_str() {
        s.push_str(&format!("\n_Updated {u}_\n"));
    }
    s
}

/// Create or update an agent's self-card. Partial: omitted fields keep their
/// previous value, so the agent can tweak just the tagline without clobbering
/// the bio. Writes identity.json (structured) + identity.md (the prose the
/// SessionStart hook injects). Returns the sanitized name.
pub async fn set_identity(name: &str, tagline: Option<&str>, bio: Option<&str>) -> Result<String> {
    let safe = sanitize(name);
    let dir = persona_dir(name);
    let json_path = dir.join("identity.json");
    // Merge over any existing card so a partial update doesn't wipe fields.
    let mut doc = match fs::read_to_string(&json_path).await {
        Ok(raw) => serde_json::from_str::<serde_json::Value>(&raw).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    doc["name"] = json!(safe);
    if let Some(t) = tagline {
        doc["tagline"] = json!(t);
    }
    if let Some(b) = bio {
        doc["bio"] = json!(b);
    }
    doc["updated"] = json!(now_iso());

    write_atomic(&json_path, &serde_json::to_vec_pretty(&doc)?).await?;
    write_atomic(&dir.join("identity.md"), render_identity_md(&doc).as_bytes()).await?;
    Ok(safe)
}

/// Return an agent's self-card prose (identity.md), or None if none written.
/// Use to learn who a peer is before relying on them.
pub async fn get_identity(name: &str) -> Result<Option<String>> {
    let path = persona_dir(name).join("identity.md");
    match fs::read_to_string(&path).await {
        Ok(s) => Ok(Some(s)),
        Err(_) => Ok(None),
    }
}

/// Append a dated note about `peer` to the holder's private peer-log
/// (personas/<name>/peers/<peer>.md). Notes accumulate. `affinity` (optional,
/// -5..=5) is recorded inline. Returns the sanitized peer name.
pub async fn remember_peer(
    name: &str,
    peer: &str,
    note: &str,
    affinity: Option<i64>,
) -> Result<String> {
    let peer = sanitize(peer);
    let path = persona_dir(name).join("peers").join(format!("{peer}.md"));
    let mut body = match fs::read_to_string(&path).await {
        Ok(s) => s,
        Err(_) => format!("# Notes on {peer}\n"),
    };
    if !body.ends_with('\n') {
        body.push('\n');
    }
    let aff = affinity
        .map(|a| format!(" _(affinity {a:+})_"))
        .unwrap_or_default();
    body.push_str(&format!("\n- [{ts}]{aff} {note}\n", ts = now_iso()));
    write_atomic(&path, body.as_bytes()).await?;
    Ok(peer)
}

/// Return the holder's private notes on every peer they've logged, as
/// (peer, notes) pairs sorted by peer name. Empty if none.
pub async fn recall_peers(name: &str) -> Result<Vec<(String, String)>> {
    let dir = persona_dir(name).join("peers");
    let mut out = Vec::new();
    if let Ok(mut rd) = fs::read_dir(&dir).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let peer = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if let Ok(s) = fs::read_to_string(&p).await {
                out.push((peer, s));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Follow a rename through the persona store: move the agent's own persona
/// dir, and rewrite every *other* agent's `peers/<old>.md` note-file to
/// `peers/<new>.md`, so identity AND the relationships peers built about this
/// agent survive the rename. Best-effort (mirrors `fleet::rename_in_projects`).
pub async fn rename_persona(old: &str, new: &str) {
    let old = sanitize(old);
    let new = sanitize(new);
    if old == new {
        return;
    }
    let root = personas_root();

    // 1. The renamed agent's own persona dir, with the name field/heading fixed.
    let from = root.join(&old);
    let to = root.join(&new);
    if fs::try_exists(&from).await.unwrap_or(false) && !fs::try_exists(&to).await.unwrap_or(false) {
        if fs::rename(&from, &to).await.is_ok() {
            let jp = to.join("identity.json");
            if let Ok(raw) = fs::read_to_string(&jp).await {
                if let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(&raw) {
                    doc["name"] = json!(new);
                    if let Ok(bytes) = serde_json::to_vec_pretty(&doc) {
                        let _ = write_atomic(&jp, &bytes).await;
                    }
                    let _ =
                        write_atomic(&to.join("identity.md"), render_identity_md(&doc).as_bytes())
                            .await;
                }
            }
        }
    }

    // 2. Every other agent's note-file about `old` → `new`.
    if let Ok(mut rd) = fs::read_dir(&root).await {
        while let Ok(Some(e)) = rd.next_entry().await {
            let peers = e.path().join("peers");
            let of = peers.join(format!("{old}.md"));
            let nf = peers.join(format!("{new}.md"));
            if fs::try_exists(&of).await.unwrap_or(false)
                && !fs::try_exists(&nf).await.unwrap_or(false)
            {
                let _ = fs::rename(&of, &nf).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // _env guard intentionally held across .await
    async fn identity_merges_and_peers_accumulate() {
        // Serialize against the other HOME-mutating tests (see fleet::TEST_ENV_LOCK).
        let _env = crate::fleet::TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("persona-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: the TEST_ENV_LOCK guard above keeps any other test from
        // reading or writing HOME while we hold it.
        unsafe {
            std::env::set_var("HOME", &home);
            // A dev's exported root override would otherwise hijack the temp store.
            std::env::remove_var("LAKITU_FLEET_ROOT");
            std::env::remove_var("GENBOT_ROOT");
        }

        // Full card, then a partial update that must preserve the bio.
        set_identity("samus", Some("bounty hunter"), Some("methodical, terse"))
            .await
            .unwrap();
        set_identity("samus", Some("bounty hunter, refactor-first"), None)
            .await
            .unwrap();
        let card = get_identity("samus").await.unwrap().unwrap();
        assert!(card.contains("refactor-first"), "tagline updated");
        assert!(card.contains("methodical, terse"), "bio preserved on partial update");

        // Peer notes accumulate (append-only) and round-trip via recall.
        remember_peer("samus", "protoman", "great on CI", Some(3))
            .await
            .unwrap();
        remember_peer("samus", "protoman", "hates broad diffs", Some(2))
            .await
            .unwrap();
        let peers = recall_peers("samus").await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].0, "protoman");
        assert!(peers[0].1.contains("great on CI"));
        assert!(peers[0].1.contains("hates broad diffs"), "notes accumulate");
        assert!(peers[0].1.contains("affinity +3"));

        // Rename carries the persona and follows peers' notes about the agent.
        remember_peer("nucleus", "samus", "solid pair", None)
            .await
            .unwrap();
        rename_persona("samus", "aran").await;
        assert!(get_identity("samus").await.unwrap().is_none(), "old name gone");
        assert!(get_identity("aran").await.unwrap().is_some(), "persona moved");
        let nucleus_peers = recall_peers("nucleus").await.unwrap();
        assert_eq!(nucleus_peers[0].0, "aran", "peer note about samus follows the rename");

        let _ = std::fs::remove_dir_all(&home);
    }
}
