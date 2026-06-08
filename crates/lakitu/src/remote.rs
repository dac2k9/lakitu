//! HTTP client for the lakitu daemon (`lakitu-mcp serve`), used when the
//! cockpit runs in `--server` mode against a remote fleet. It mirrors the local
//! `client.rs` writes and the `store.rs` snapshot read, but over the wire — the
//! daemon owns the store. The wire DTOs here are the deserializing counterparts
//! of the daemon's `wire.rs`.

use std::collections::HashMap;

use chrono::DateTime;
use serde::Deserialize;

use crate::store::{
    Agent, AgentState, ClientKind, Message, Project, StoreSnapshot, Task, TaskPr, Usage,
};

/// A bearer-authenticated HTTP client for one daemon. Cheap to clone (the inner
/// `reqwest::Client` shares its connection pool).
#[derive(Clone)]
pub struct RemoteClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl RemoteClient {
    /// `base` is the daemon root (e.g. `http://host:8787`); a trailing slash is
    /// trimmed. `token` is the shared `LAKITU_FLEET_TOKEN`.
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Fetch + convert the fleet snapshot. `None` on any network/parse error, so
    /// the poller can keep showing the last good snapshot instead of flapping.
    pub async fn snapshot(&self) -> Option<StoreSnapshot> {
        let resp = self
            .http
            .get(self.url("/v1/snapshot"))
            .bearer_auth(&self.token)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let dto = resp.json::<SnapshotDto>().await.ok()?;
        Some(dto.into_snapshot())
    }

    // ---- writes (fire-and-forget; the poll reflects the result) ----

    pub async fn register(&self, name: &str) {
        self.post("/v1/register", &serde_json::json!({ "name": name }))
            .await;
    }

    pub async fn send_message(&self, from: &str, to: &str, title: &str, body: &str) {
        self.post(
            "/v1/messages",
            &serde_json::json!({ "from": from, "to": to, "title": title, "body": body }),
        )
        .await;
    }

    pub async fn broadcast(&self, from: &str, recipients: &[String], title: &str, body: &str) {
        self.post(
            "/v1/broadcast",
            &serde_json::json!({ "from": from, "recipients": recipients, "title": title, "body": body }),
        )
        .await;
    }

    pub async fn mark_read(&self, owner: &str, id: &str) {
        self.post(
            "/v1/messages/read",
            &serde_json::json!({ "owner": owner, "id": id }),
        )
        .await;
    }

    pub async fn delete_message(&self, owner: &str, id: &str) {
        self.post(
            "/v1/messages/delete",
            &serde_json::json!({ "owner": owner, "id": id }),
        )
        .await;
    }

    pub async fn create_project(&self, name: &str) {
        self.post("/v1/projects", &serde_json::json!({ "name": name }))
            .await;
    }

    pub async fn rename_project(&self, id: &str, name: &str) {
        self.send(
            self.http
                .patch(self.url(&format!("/v1/projects/{id}")))
                .json(&serde_json::json!({ "name": name })),
        )
        .await;
    }

    pub async fn move_project_down(&self, id: &str) {
        self.send(self.http.post(self.url(&format!("/v1/projects/{id}/move"))))
            .await;
    }

    pub async fn remove_project(&self, id: &str) {
        self.send(self.http.delete(self.url(&format!("/v1/projects/{id}"))))
            .await;
    }

    pub async fn set_membership(&self, client: &str, project_id: Option<&str>) {
        self.post(
            "/v1/projects/membership",
            &serde_json::json!({ "client": client, "project_id": project_id }),
        )
        .await;
    }

    pub async fn toggle_coordinator(&self, id: &str, client: &str) {
        self.send(
            self.http
                .post(self.url(&format!("/v1/projects/{id}/coordinator")))
                .json(&serde_json::json!({ "client": client })),
        )
        .await;
    }

    pub async fn disconnect_client(&self, name: &str) {
        self.send(self.http.delete(self.url(&format!("/v1/agents/{name}"))))
            .await;
    }

    pub async fn add_task(
        &self,
        owner: &str,
        text: &str,
        body: Option<&str>,
        pr: Option<(String, u64)>,
        from_msg: Option<&str>,
    ) {
        let mut payload = serde_json::json!({ "text": text });
        if let Some(b) = body {
            payload["body"] = serde_json::json!(b);
        }
        if let Some((repo, number)) = pr {
            payload["pr_repo"] = serde_json::json!(repo);
            payload["pr_number"] = serde_json::json!(number);
        }
        if let Some(m) = from_msg {
            payload["from_msg"] = serde_json::json!(m);
        }
        self.post(&format!("/v1/agents/{owner}/tasks"), &payload)
            .await;
    }

    pub async fn set_task_done(&self, owner: &str, id: &str, done: bool) {
        self.send(
            self.http
                .patch(self.url(&format!("/v1/agents/{owner}/tasks/{id}")))
                .json(&serde_json::json!({ "done": done })),
        )
        .await;
    }

    pub async fn drop_task(&self, owner: &str, id: &str) {
        self.send(
            self.http
                .delete(self.url(&format!("/v1/agents/{owner}/tasks/{id}"))),
        )
        .await;
    }

    async fn post(&self, path: &str, body: &serde_json::Value) {
        self.send(self.http.post(self.url(path)).json(body)).await;
    }

    /// Apply bearer auth and fire the request, swallowing errors (best-effort —
    /// the next snapshot poll surfaces the real state).
    async fn send(&self, req: reqwest::RequestBuilder) {
        if let Err(e) = req.bearer_auth(&self.token).send().await {
            tracing::warn!(error = %e, "remote write failed");
        }
    }
}

// ---- the writer task: one channel, applied to whichever backend ------------

/// A store mutation the cockpit performs. Sent (non-blocking) from the input
/// handler to a background task, which applies it locally or over HTTP.
pub enum WriteCmd {
    Register(String),
    SendMessage {
        from: String,
        to: String,
        title: String,
        body: String,
    },
    Broadcast {
        from: String,
        recipients: Vec<String>,
        title: String,
        body: String,
    },
    MarkRead {
        owner: String,
        id: String,
    },
    DeleteMessage {
        owner: String,
        id: String,
    },
    CreateProject(String),
    RenameProject {
        id: String,
        name: String,
    },
    MoveProjectDown(String),
    RemoveProject(String),
    SetMembership {
        client: String,
        project_id: Option<String>,
    },
    ToggleCoordinator {
        id: String,
        client: String,
    },
    Disconnect(String),
    AddTask {
        owner: String,
        text: String,
        body: Option<String>,
        pr: Option<(String, u64)>,
        from_msg: Option<String>,
    },
    SetTaskDone {
        owner: String,
        id: String,
        done: bool,
    },
    DropTask {
        owner: String,
        id: String,
    },
}

/// Apply one write against the active source (local files or the remote daemon).
pub async fn apply_write(source: &crate::store::Source, cmd: WriteCmd) {
    match source {
        crate::store::Source::Local(root) => apply_local(root, cmd),
        crate::store::Source::Remote(rc) => apply_remote(rc, cmd).await,
    }
}

fn apply_local(root: &std::path::Path, cmd: WriteCmd) {
    use crate::client as c;
    use WriteCmd::*;
    let r: std::io::Result<()> = match cmd {
        Register(name) => c::register_me(root, &name),
        SendMessage {
            from,
            to,
            title,
            body,
        } => c::send_message(root, &from, &to, &title, &body).map(|_| ()),
        Broadcast {
            from,
            recipients,
            title,
            body,
        } => {
            c::broadcast(root, &from, &recipients, &title, &body);
            Ok(())
        }
        MarkRead { owner, id } => c::mark_read(root, &owner, &id),
        DeleteMessage { owner, id } => c::delete_message(root, &owner, &id),
        CreateProject(name) => c::create_project(root, &name).map(|_| ()),
        RenameProject { id, name } => c::rename_project(root, &id, &name).map(|_| ()),
        MoveProjectDown(id) => c::move_project_down(root, &id).map(|_| ()),
        RemoveProject(id) => c::remove_project(root, &id).map(|_| ()),
        SetMembership { client, project_id } => {
            c::set_membership(root, &client, project_id.as_deref()).map(|_| ())
        }
        ToggleCoordinator { id, client } => c::toggle_coordinator(root, &id, &client).map(|_| ()),
        Disconnect(name) => c::disconnect_client(root, &name).map(|_| ()),
        AddTask {
            owner,
            text,
            body,
            pr,
            from_msg,
        } => c::add_task(
            root,
            &owner,
            &text,
            body.as_deref(),
            pr,
            from_msg.as_deref(),
        )
        .map(|_| ()),
        SetTaskDone { owner, id, done } => c::set_task_done(root, &owner, &id, done),
        DropTask { owner, id } => c::drop_task(root, &owner, &id),
    };
    if let Err(e) = r {
        tracing::warn!(error = %e, "local write failed");
    }
}

async fn apply_remote(rc: &RemoteClient, cmd: WriteCmd) {
    use WriteCmd::*;
    match cmd {
        Register(name) => rc.register(&name).await,
        SendMessage {
            from,
            to,
            title,
            body,
        } => rc.send_message(&from, &to, &title, &body).await,
        Broadcast {
            from,
            recipients,
            title,
            body,
        } => rc.broadcast(&from, &recipients, &title, &body).await,
        MarkRead { owner, id } => rc.mark_read(&owner, &id).await,
        DeleteMessage { owner, id } => rc.delete_message(&owner, &id).await,
        CreateProject(name) => rc.create_project(&name).await,
        RenameProject { id, name } => rc.rename_project(&id, &name).await,
        MoveProjectDown(id) => rc.move_project_down(&id).await,
        RemoveProject(id) => rc.remove_project(&id).await,
        SetMembership { client, project_id } => {
            rc.set_membership(&client, project_id.as_deref()).await
        }
        ToggleCoordinator { id, client } => rc.toggle_coordinator(&id, &client).await,
        Disconnect(name) => rc.disconnect_client(&name).await,
        AddTask {
            owner,
            text,
            body,
            pr,
            from_msg,
        } => {
            rc.add_task(&owner, &text, body.as_deref(), pr, from_msg.as_deref())
                .await
        }
        SetTaskDone { owner, id, done } => rc.set_task_done(&owner, &id, done).await,
        DropTask { owner, id } => rc.drop_task(&owner, &id).await,
    }
}

// ---- wire DTOs (deserializing mirror of the daemon's wire.rs) ---------------

#[derive(Deserialize)]
struct SnapshotDto {
    agents: Vec<AgentDto>,
    inboxes: HashMap<String, Vec<MessageDto>>,
    /// `default` so a daemon predating tasks still deserializes.
    #[serde(default)]
    tasks: HashMap<String, Vec<TaskDto>>,
    projects: Vec<ProjectDto>,
    usage: Option<UsageDto>,
}

#[derive(Deserialize)]
struct AgentDto {
    name: String,
    kind: String,
    repo: String,
    board: String,
    role: Option<String>,
    description: Option<String>,
    state: String,
    task: Option<String>,
    last_seen: Option<String>,
    stale: bool,
    unread: u32,
    context_pct: Option<u8>,
}

#[derive(Deserialize)]
struct MessageDto {
    id: String,
    time: Option<String>,
    from: String,
    title: String,
    body: String,
    read: bool,
}

#[derive(Deserialize)]
struct ProjectDto {
    id: String,
    name: String,
    coordinator: Option<String>,
    members: Vec<String>,
}

#[derive(Deserialize)]
struct TaskDto {
    id: String,
    text: String,
    #[serde(default)]
    body: Option<String>,
    done: bool,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    pr: Option<TaskPrDto>,
    #[serde(default)]
    from_msg: Option<String>,
}

#[derive(Deserialize)]
struct TaskPrDto {
    repo: String,
    number: u64,
}

#[derive(Deserialize)]
struct UsageDto {
    five_hour_pct: f32,
    seven_day_pct: f32,
    ts: i64,
    #[serde(default)]
    five_hour_reset: Option<i64>,
    #[serde(default)]
    seven_day_reset: Option<i64>,
}

impl SnapshotDto {
    /// Convert the wire form into the cockpit's internal snapshot, reusing the
    /// same string parsers the local file reader uses (state/kind/RFC3339).
    fn into_snapshot(self) -> StoreSnapshot {
        let agents = self
            .agents
            .into_iter()
            .map(|a| Agent {
                name: a.name,
                kind: ClientKind::parse(&a.kind),
                repo: a.repo,
                board: a.board,
                role: a.role,
                description: a.description,
                state: AgentState::parse(&a.state),
                task: a.task,
                last_seen: a
                    .last_seen
                    .as_deref()
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok()),
                stale: a.stale,
                unread: a.unread as usize,
                context_pct: a.context_pct,
            })
            .collect();
        let inboxes = self
            .inboxes
            .into_iter()
            .map(|(name, msgs)| {
                let msgs = msgs
                    .into_iter()
                    .map(|m| Message {
                        id: m.id,
                        time: m
                            .time
                            .as_deref()
                            .and_then(|t| DateTime::parse_from_rfc3339(t).ok()),
                        from: m.from,
                        title: m.title,
                        body: m.body,
                        read: m.read,
                    })
                    .collect();
                (name, msgs)
            })
            .collect();
        let projects = self
            .projects
            .into_iter()
            .map(|p| Project {
                id: p.id,
                name: p.name,
                coordinator: p.coordinator,
                members: p.members,
            })
            .collect();
        let tasks = self
            .tasks
            .into_iter()
            .map(|(name, ts)| {
                let ts = ts
                    .into_iter()
                    .map(|t| Task {
                        id: t.id,
                        text: t.text,
                        body: t.body,
                        done: t.done,
                        created: t
                            .created
                            .as_deref()
                            .and_then(|s| DateTime::parse_from_rfc3339(s).ok()),
                        pr: t.pr.map(|p| TaskPr {
                            repo: p.repo,
                            number: p.number,
                        }),
                        from_msg: t.from_msg,
                    })
                    .collect();
                (name, ts)
            })
            .collect();
        let usage = self.usage.map(|u| Usage {
            five_hour_pct: u.five_hour_pct,
            seven_day_pct: u.seven_day_pct,
            ts: u.ts,
            five_hour_reset: u.five_hour_reset,
            seven_day_reset: u.seven_day_reset,
        });
        StoreSnapshot {
            agents,
            inboxes,
            tasks,
            projects,
            usage,
        }
    }
}
