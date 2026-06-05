//! Write side of the fleet store, for the human "client" (the cockpit).
//!
//! The TUI is otherwise a read-only viewer (see `store.rs`). This is the
//! small set of writes the supervisor performs *from* the cockpit: joining
//! the network as a client, sending / broadcasting messages, and archiving
//! messages they've read. It mirrors the `lakitu-mcp` MCP's `fleet.rs`
//! writer so both sides produce identical on-disk files (see `DESIGN.md`).
//!
//! Kept synchronous because it's driven from the input handler on a single
//! keypress — one or a few tiny JSON files, well within blocking-IO budget.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;

/// Disambiguates messages created within the same second.
static MSG_COUNTER: AtomicU64 = AtomicU64::new(0);

fn now_iso() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// Path-safe single component (mirrors the MCP's `sanitize`).
fn sanitize(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches('.');
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Register the human supervisor as a client (`kind: "human"`) so agents see
/// them in `list_agents` and the cockpit shows their row. Idempotent —
/// overwrites the registry file and ensures the inbox exists.
pub fn register_me(root: &Path, name: &str) -> io::Result<()> {
    let name = sanitize(name);
    let dir = root.join("agents");
    std::fs::create_dir_all(&dir)?;
    let obj = json!({
        "name": name,
        "kind": "human",
        "repo": "-",
        "board": "-",
        "description": "Supervisor — the human running the cockpit.",
        "started": now_iso(),
    });
    std::fs::write(
        dir.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&obj)?,
    )?;
    std::fs::create_dir_all(root.join("inbox").join(&name))?;
    Ok(())
}

/// Where the cockpit remembers the human's chosen name between launches.
pub fn me_path(root: &Path) -> std::path::PathBuf {
    root.join("me")
}

/// Load the remembered human name, if one was set.
pub fn load_me(root: &Path) -> Option<String> {
    std::fs::read_to_string(me_path(root))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Persist the human's chosen name so it sticks across restarts.
pub fn remember_me(root: &Path, name: &str) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::write(me_path(root), name.trim())
}

fn next_id() -> String {
    let nanos = chrono::Local::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    let c = MSG_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:06x}", ((nanos >> 8) ^ c.wrapping_mul(2_654_435_761)) & 0xff_ffff)
}

/// Write one message into `to`'s inbox. Returns the new message id.
pub fn send_message(root: &Path, from: &str, to: &str, title: &str, body: &str) -> io::Result<String> {
    let to = sanitize(to);
    let dir = root.join("inbox").join(&to);
    std::fs::create_dir_all(&dir)?;
    let id = next_id();
    let ts_file = chrono::Local::now().format("%Y%m%dT%H%M%S").to_string();
    let obj = json!({
        "id": id,
        "time": now_iso(),
        "from": from,
        "title": title,
        "body": body,
    });
    std::fs::write(
        dir.join(format!("{ts_file}-{id}.json")),
        serde_json::to_vec_pretty(&obj)?,
    )?;
    Ok(id)
}

/// Fan a message out to every recipient's inbox (broadcast / "everyone").
/// Returns how many were delivered.
pub fn broadcast(
    root: &Path,
    from: &str,
    recipients: &[String],
    title: &str,
    body: &str,
) -> usize {
    recipients
        .iter()
        .filter(|r| send_message(root, from, r, title, body).is_ok())
        .count()
}

/// Archive a message (move it to the owner's `read/` subdir) by id.
pub fn mark_read(root: &Path, owner: &str, msg_id: &str) -> io::Result<()> {
    let owner = sanitize(owner);
    let dir = root.join("inbox").join(&owner);
    let suffix = format!("-{msg_id}.json");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let Some(fname) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if fname.ends_with(&suffix) {
            let read_dir = dir.join("read");
            std::fs::create_dir_all(&read_dir)?;
            let _ = std::fs::rename(&p, read_dir.join(fname));
            break;
        }
    }
    Ok(())
}

/// Delete a message by id from `owner`'s inbox — removes the file whether it's
/// still unread (top-level) or already archived under `read/`. No-op if not
/// found. Unlike `mark_read` (which archives), this discards the message.
pub fn delete_message(root: &Path, owner: &str, msg_id: &str) -> io::Result<()> {
    let owner = sanitize(owner);
    let suffix = format!("-{msg_id}.json");
    let base = root.join("inbox").join(&owner);
    for dir in [base.clone(), base.join("read")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            if p.file_name().and_then(|s| s.to_str()).map(|f| f.ends_with(&suffix)).unwrap_or(false)
            {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    Ok(())
}

// ---- Tasks (per-agent reminder list) --------------------------------------
//
// One JSON array per agent at `tasks/<name>.json`. The supervisor adds /
// toggles / drops a client's tasks from the cockpit; these mirror the MCP's
// `fleet.rs` task ops so both writers produce identical files. Edits go through
// `serde_json::Value` so we preserve fields we don't touch (created, pr, …).

/// Disambiguates tasks created within the same instant.
static TASK_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tasks_path(root: &Path, owner: &str) -> PathBuf {
    root.join("tasks").join(format!("{}.json", sanitize(owner)))
}

fn read_tasks_value(root: &Path, owner: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(tasks_path(root, owner))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
        .unwrap_or_default()
}

fn write_tasks_value(root: &Path, owner: &str, tasks: &[serde_json::Value]) -> io::Result<()> {
    let dir = root.join("tasks");
    std::fs::create_dir_all(&dir)?;
    let path = tasks_path(root, owner);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(tasks)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Append a task to `owner`'s list. Returns the new task id.
pub fn add_task(
    root: &Path,
    owner: &str,
    text: &str,
    body: Option<&str>,
    pr: Option<(String, u64)>,
    from_msg: Option<&str>,
) -> io::Result<String> {
    let mut tasks = read_tasks_value(root, owner);
    let nanos = chrono::Local::now().timestamp_nanos_opt().unwrap_or(0) as u64;
    let c = TASK_COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = format!("{:06x}", ((nanos >> 8) ^ c.wrapping_mul(2_654_435_761)) & 0xff_ffff);
    let mut obj = json!({
        "id": id,
        "text": text.trim(),
        "done": false,
        "created": now_iso(),
    });
    if let Some(b) = body.map(str::trim).filter(|b| !b.is_empty()) {
        obj["body"] = json!(b);
    }
    if let Some((repo, number)) = pr {
        obj["pr"] = json!({ "repo": repo, "number": number });
    }
    if let Some(m) = from_msg {
        obj["from_msg"] = json!(m);
    }
    tasks.push(obj);
    write_tasks_value(root, owner, &tasks)?;
    Ok(id)
}

/// Set a task's `done` flag by id (no-op if the id isn't found).
pub fn set_task_done(root: &Path, owner: &str, id: &str, done: bool) -> io::Result<()> {
    let mut tasks = read_tasks_value(root, owner);
    for t in &mut tasks {
        if t.get("id").and_then(|v| v.as_str()) == Some(id) {
            t["done"] = json!(done);
        }
    }
    write_tasks_value(root, owner, &tasks)
}

/// Remove a task by id (no-op if the id isn't found).
pub fn drop_task(root: &Path, owner: &str, id: &str) -> io::Result<()> {
    let mut tasks = read_tasks_value(root, owner);
    tasks.retain(|t| t.get("id").and_then(|v| v.as_str()) != Some(id));
    write_tasks_value(root, owner, &tasks)
}

// ---- Projects (supervisor-defined groupings) ------------------------------
//
// Membership lives in `projects.json` (supervisor-owned), not on agent
// registries — agents rewrite those on every heartbeat and don't know their
// project. Each mutation reads → edits → atomically writes the file and
// returns the updated list so the cockpit can reflect it immediately (the
// poller would otherwise pick it up a tick later).

use crate::store::Project;
use std::path::PathBuf;

#[derive(serde::Deserialize, Default)]
struct ProjectsDoc {
    #[serde(default)]
    projects: Vec<Project>,
}

fn projects_path(root: &Path) -> PathBuf {
    root.join("projects.json")
}

pub fn read_projects(root: &Path) -> Vec<Project> {
    std::fs::read_to_string(projects_path(root))
        .ok()
        .and_then(|s| serde_json::from_str::<ProjectsDoc>(&s).ok())
        .map(|d| d.projects)
        .unwrap_or_default()
}

fn write_projects(root: &Path, projects: &[Project]) -> io::Result<()> {
    std::fs::create_dir_all(root)?;
    let path = projects_path(root);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&json!({ "projects": projects }))?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Create a project named `name` (slug derived + de-duplicated). Blank name is
/// a no-op. Returns the updated project list.
pub fn create_project(root: &Path, name: &str) -> io::Result<Vec<Project>> {
    let mut projects = read_projects(root);
    let name = name.trim();
    if !name.is_empty() {
        let base = sanitize(name).to_lowercase();
        let mut id = base.clone();
        let mut n = 2;
        while projects.iter().any(|p| p.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        projects.push(Project {
            id,
            name: name.to_string(),
            coordinator: None,
            members: Vec::new(),
        });
        write_projects(root, &projects)?;
    }
    Ok(projects)
}

/// Rename a project by id (keeps its id, members, coordinator). Blank name is
/// a no-op. Returns the updated list.
pub fn rename_project(root: &Path, id: &str, name: &str) -> io::Result<Vec<Project>> {
    let mut projects = read_projects(root);
    let name = name.trim();
    if !name.is_empty() {
        if let Some(p) = projects.iter_mut().find(|p| p.id == id) {
            p.name = name.to_string();
        }
        write_projects(root, &projects)?;
    }
    Ok(projects)
}

/// Move a project one slot later in the order; the last one wraps to the
/// front. Projects always render below the floating clients, so this only
/// reorders within the project list. Returns the updated list.
pub fn move_project_down(root: &Path, id: &str) -> io::Result<Vec<Project>> {
    let mut projects = read_projects(root);
    if let Some(i) = projects.iter().position(|p| p.id == id) {
        if i + 1 < projects.len() {
            projects.swap(i, i + 1);
        } else if projects.len() > 1 {
            let p = projects.remove(i);
            projects.insert(0, p);
        }
        write_projects(root, &projects)?;
    }
    Ok(projects)
}

/// Remove a project by id; its members simply float (no longer listed).
pub fn remove_project(root: &Path, id: &str) -> io::Result<Vec<Project>> {
    let mut projects = read_projects(root);
    projects.retain(|p| p.id != id);
    write_projects(root, &projects)?;
    Ok(projects)
}

/// Set a client's membership: `Some(id)` moves it into that project (removing
/// it from any other); `None` makes it floating. A client that leaves a
/// project it coordinated is dropped as that project's coordinator.
pub fn set_membership(
    root: &Path,
    client: &str,
    project_id: Option<&str>,
) -> io::Result<Vec<Project>> {
    let mut projects = read_projects(root);
    for p in &mut projects {
        p.members.retain(|m| m != client);
        if p.coordinator.as_deref() == Some(client) && Some(p.id.as_str()) != project_id {
            p.coordinator = None;
        }
    }
    if let Some(id) = project_id {
        if let Some(p) = projects.iter_mut().find(|p| p.id == id) {
            if !p.members.iter().any(|m| m == client) {
                p.members.push(client.to_string());
            }
        }
    }
    write_projects(root, &projects)?;
    Ok(projects)
}

/// Toggle `client` as `project_id`'s coordinator. Setting it also ensures
/// membership; if already the coordinator, clears it.
pub fn toggle_coordinator(
    root: &Path,
    project_id: &str,
    client: &str,
) -> io::Result<Vec<Project>> {
    let mut projects = read_projects(root);
    if let Some(p) = projects.iter_mut().find(|p| p.id == project_id) {
        if p.coordinator.as_deref() == Some(client) {
            p.coordinator = None;
        } else {
            p.coordinator = Some(client.to_string());
            if !p.members.iter().any(|m| m == client) {
                p.members.push(client.to_string());
            }
        }
    }
    write_projects(root, &projects)?;
    Ok(projects)
}

/// Disconnect a client: remove its registry, presence, context / icon / wake
/// sidecars and inbox from the store, and drop it from any project it was a
/// member or coordinator of. Best-effort on the files; returns the updated
/// project list so the cockpit reflects the membership change at once. The
/// client returns only if it re-registers (a still-running agent on its next
/// heartbeat) — this clears out stopped / retired ones.
pub fn disconnect_client(root: &Path, name: &str) -> io::Result<Vec<Project>> {
    let name = sanitize(name);
    let agents = root.join("agents");
    for suffix in ["json", "heartbeat.json", "wake.json", "context.json"] {
        let _ = std::fs::remove_file(agents.join(format!("{name}.{suffix}")));
    }
    // Icon sidecars (any extension, e.g. <name>.icon.webp).
    if let Ok(rd) = std::fs::read_dir(&agents) {
        let prefix = format!("{name}.icon.");
        for e in rd.flatten() {
            if let Some(f) = e.file_name().to_str() {
                if f.starts_with(&prefix) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    // Inbox (pending + read archive).
    let _ = std::fs::remove_dir_all(root.join("inbox").join(&name));
    // Drop from any project membership / coordinator slot.
    let mut projects = read_projects(root);
    let mut changed = false;
    for p in &mut projects {
        let before = p.members.len();
        p.members.retain(|m| m != &name);
        if p.coordinator.as_deref() == Some(name.as_str()) {
            p.coordinator = None;
            changed = true;
        }
        changed |= p.members.len() != before;
    }
    if changed {
        write_projects(root, &projects)?;
    }
    Ok(projects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn disconnect_client_removes_files_and_membership() {
        let root = scratch("disconnect");
        let agents = root.join("agents");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join("aria.json"), r#"{"name":"aria","kind":"agent"}"#).unwrap();
        fs::write(agents.join("aria.heartbeat.json"), r#"{"state":"idle"}"#).unwrap();
        fs::write(agents.join("aria.context.json"), r#"{"pct":10}"#).unwrap();
        fs::create_dir_all(root.join("inbox/aria")).unwrap();
        fs::write(root.join("inbox/aria/m.json"), r#"{"id":"x"}"#).unwrap();
        // Put aria in a project as its coordinator (also adds membership).
        let ps = create_project(&root, "team").unwrap();
        let id = ps[0].id.clone();
        toggle_coordinator(&root, &id, "aria").unwrap();

        let projects = disconnect_client(&root, "aria").unwrap();

        assert!(!agents.join("aria.json").exists(), "registry removed");
        assert!(!agents.join("aria.heartbeat.json").exists(), "heartbeat removed");
        assert!(!agents.join("aria.context.json").exists(), "context sidecar removed");
        assert!(!root.join("inbox/aria").exists(), "inbox removed");
        assert!(projects[0].members.iter().all(|m| m != "aria"), "dropped from members");
        assert!(projects[0].coordinator.is_none(), "cleared as coordinator");
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("fleet-client-test-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn delete_message_removes_unread_and_archived() {
        let root = scratch("delmsg");
        let id1 = send_message(&root, "bob", "aria", "still unread", "b").unwrap();
        let id2 = send_message(&root, "bob", "aria", "to archive", "b").unwrap();
        mark_read(&root, "aria", &id2).unwrap(); // id2 → read/
        assert_eq!(json_files(&root.join("inbox/aria")), 1, "one unread");
        assert_eq!(json_files(&root.join("inbox/aria/read")), 1, "one archived");

        // Delete works against the top-level (unread) message...
        delete_message(&root, "aria", &id1).unwrap();
        assert_eq!(json_files(&root.join("inbox/aria")), 0, "unread message removed");
        // ...and against the archived one.
        delete_message(&root, "aria", &id2).unwrap();
        assert_eq!(json_files(&root.join("inbox/aria/read")), 0, "archived message removed");

        // Deleting a missing id is a no-op (no error).
        delete_message(&root, "aria", "nope").unwrap();

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tasks_add_toggle_drop_round_trip() {
        let root = scratch("tasks");

        // add_task trims, defaults done=false, and stores body / pr / from_msg.
        let id1 = add_task(&root, "aria", "  reply to samus  ", None, None, None).unwrap();
        assert_eq!(id1.len(), 6);
        let id2 = add_task(
            &root,
            "aria",
            "ship the changelog",
            Some("cover the dedup fix and the new flag"),
            Some(("acme/lakitu".to_string(), 12)),
            Some("msg9"),
        )
        .unwrap();

        let read = |root: &Path| -> Vec<serde_json::Value> {
            serde_json::from_str(&fs::read_to_string(root.join("tasks/aria.json")).unwrap()).unwrap()
        };
        let v = read(&root);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0]["text"], "reply to samus", "text trimmed");
        assert_eq!(v[0]["done"], false, "new tasks start open");
        assert!(v[0].get("body").is_none(), "no body when none given");
        assert_eq!(v[1]["body"], "cover the dedup fix and the new flag");
        assert_eq!(v[1]["pr"]["number"], 12);
        assert_eq!(v[1]["from_msg"], "msg9");

        // Toggle one done; the file keeps it (now done), preserving other fields.
        set_task_done(&root, "aria", &id1, true).unwrap();
        let v = read(&root);
        assert_eq!(v[0]["done"], true);

        // Drop the other; only the first remains.
        drop_task(&root, "aria", &id2).unwrap();
        let v = read(&root);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["id"], id1.as_str());

        let _ = fs::remove_dir_all(&root);
    }

    fn json_files(dir: &Path) -> usize {
        fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_file())
                    .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn register_send_broadcast_markread_round_trip() {
        let root = scratch("rt");

        // register_me writes a human registry + an inbox dir.
        register_me(&root, "you").unwrap();
        let reg = fs::read_to_string(root.join("agents/you.json")).unwrap();
        assert!(reg.contains("\"kind\": \"human\""), "registered as human");
        assert!(root.join("inbox/you").is_dir(), "inbox created");

        // remember_me round-trips through the `me` config file.
        assert!(load_me(&root).is_none());
        remember_me(&root, "you").unwrap();
        assert_eq!(load_me(&root).as_deref(), Some("you"));

        // send_message lands one file in the recipient's inbox.
        let id = send_message(&root, "you", "vscode-bot", "hi", "please look").unwrap();
        assert_eq!(id.len(), 6);
        assert_eq!(json_files(&root.join("inbox/vscode-bot")), 1);

        // broadcast fans out to every recipient.
        let n = broadcast(&root, "you", &["a".to_string(), "b".to_string()], "all", "hello");
        assert_eq!(n, 2);
        assert_eq!(json_files(&root.join("inbox/a")), 1);
        assert_eq!(json_files(&root.join("inbox/b")), 1);

        // mark_read archives the message into read/.
        mark_read(&root, "vscode-bot", &id).unwrap();
        assert_eq!(json_files(&root.join("inbox/vscode-bot")), 0, "moved out of top level");
        assert_eq!(json_files(&root.join("inbox/vscode-bot/read")), 1, "into read archive");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn projects_create_move_coordinate_remove() {
        let root = scratch("projects");

        // Create two projects; the slug is derived from the name.
        let ps = create_project(&root, "Auth Revamp").unwrap();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].name, "Auth Revamp");
        let auth = ps[0].id.clone();
        let ps = create_project(&root, "Billing").unwrap();
        let billing = ps.iter().find(|p| p.name == "Billing").unwrap().id.clone();

        // Move a client in, then make it coordinator.
        let ps = set_membership(&root, "samus", Some(&auth)).unwrap();
        assert!(ps.iter().find(|p| p.id == auth).unwrap().members.contains(&"samus".into()));
        let ps = toggle_coordinator(&root, &auth, "samus").unwrap();
        assert_eq!(
            ps.iter().find(|p| p.id == auth).unwrap().coordinator.as_deref(),
            Some("samus")
        );

        // Moving it to another project drops membership AND coordinator on the
        // one it left.
        let ps = set_membership(&root, "samus", Some(&billing)).unwrap();
        let left = ps.iter().find(|p| p.id == auth).unwrap();
        assert!(left.members.is_empty(), "left the old project");
        assert!(left.coordinator.is_none(), "coordinator cleared on leave");
        assert!(ps.iter().find(|p| p.id == billing).unwrap().members.contains(&"samus".into()));

        // Removing a project just drops it — members float (no longer listed).
        let ps = remove_project(&root, &billing).unwrap();
        assert!(!ps.iter().any(|p| p.id == billing));
        assert!(!ps.iter().any(|p| p.members.contains(&"samus".into())), "samus floats");

        // Persisted to disk.
        let reread = read_projects(&root);
        assert_eq!(reread.len(), 1);
        assert_eq!(reread[0].id, auth);

        let _ = fs::remove_dir_all(&root);
    }
}
