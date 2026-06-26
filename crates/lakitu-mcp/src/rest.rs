//! The `/v1` REST API the daemon exposes alongside `/mcp`, for the shell hooks
//! and the remote cockpit. Handlers reuse the `fleet`/`persona` store helpers;
//! the daemon's shared bearer-auth layer covers these routes too.
//!
//! axum 0.8 path-param syntax is `{name}`.

use axum::{
    Json, Router,
    extract::Path,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};
use serde::Deserialize;
use serde_json::json;

use crate::wire::SnapshotDto;
use crate::{fleet, persona};

pub fn router() -> Router {
    Router::new()
        // Reads
        .route("/v1/snapshot", get(snapshot))
        // Hook-backing endpoints
        .route(
            "/v1/agents/{name}/state",
            patch(set_state).delete(set_offline),
        )
        .route("/v1/agents/{name}/unread-count", get(unread_count))
        .route("/v1/agents/{name}/context", put(put_context))
        .route("/v1/agents/{name}/persona", get(get_persona))
        .route("/v1/agents/{name}/tasks", get(tasks_list).post(task_add))
        .route(
            "/v1/agents/{name}/tasks/{id}",
            patch(task_set_done).delete(task_drop),
        )
        // Cockpit writes
        .route("/v1/register", post(register))
        .route("/v1/messages", post(send_message))
        .route("/v1/broadcast", post(broadcast))
        .route("/v1/messages/read", post(mark_read))
        .route("/v1/messages/delete", post(delete_message))
        .route("/v1/projects", post(create_project))
        .route(
            "/v1/projects/{id}",
            patch(rename_project).delete(remove_project),
        )
        .route("/v1/projects/{id}/move", post(move_project))
        .route("/v1/projects/{id}/coordinator", post(toggle_coordinator))
        .route("/v1/projects/membership", put(set_membership))
        .route("/v1/agents/{name}", delete(disconnect))
        .route("/v1/agents/{name}/icon", get(icon))
        .route("/v1/shared-tasks/{id}/archive", post(archive_shared_task))
}

// ---- Reads -----------------------------------------------------------------

/// Full fleet snapshot — what the remote cockpit polls in place of the local store.
async fn snapshot() -> Json<SnapshotDto> {
    Json(fleet::snapshot().await)
}

// ---- Hook-backing endpoints ------------------------------------------------

#[derive(Deserialize)]
struct StateReq {
    state: String,
}

/// Auto-presence (PATCH) — the HTTP form of `state-hook.sh working|idle|blocked`.
async fn set_state(Path(name): Path<String>, Json(req): Json<StateReq>) -> StatusCode {
    match fleet::set_state(&name, &req.state).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// SessionEnd (DELETE) — drop the heartbeat so the agent reads offline.
async fn set_offline(Path(name): Path<String>) -> StatusCode {
    let _ = fleet::set_offline(&name).await;
    StatusCode::NO_CONTENT
}

/// Stop-hook inbox gate — unread count for `name`.
async fn unread_count(Path(name): Path<String>) -> Json<serde_json::Value> {
    Json(json!({ "unread": fleet::unread_count(&name).await }))
}

#[derive(Deserialize)]
struct ContextReq {
    pct: Option<f64>,
    rl5h: Option<f64>,
    rl7d: Option<f64>,
    rl5h_reset: Option<i64>,
    rl7d_reset: Option<i64>,
}

/// statusLine usage write (PUT) — the HTTP form of `context-statusline.sh`.
async fn put_context(Path(name): Path<String>, Json(req): Json<ContextReq>) -> StatusCode {
    match fleet::write_context(
        &name,
        req.pct,
        req.rl5h,
        req.rl7d,
        req.rl5h_reset,
        req.rl7d_reset,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// SessionStart persona — identity + peer notes, or 204 if the agent has none yet.
async fn get_persona(Path(name): Path<String>) -> Response {
    match persona::session_context(&name).await {
        Some(ctx) => Json(json!({ "context": ctx })).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// An agent's full task list (open + done) — backs the SessionStart tasks hook
/// (it filters to open) and is also a handy read for tooling.
async fn tasks_list(Path(name): Path<String>) -> Json<serde_json::Value> {
    Json(json!({ "tasks": fleet::read_tasks(&name).await }))
}

#[derive(Deserialize)]
struct AddTaskReq {
    text: String,
    #[serde(default)]
    body: Option<String>,
    pr_repo: Option<String>,
    pr_number: Option<u64>,
    from_msg: Option<String>,
}

/// Add a task to an agent's list (remote cockpit / tooling); returns the new task.
async fn task_add(Path(name): Path<String>, Json(req): Json<AddTaskReq>) -> Response {
    let pr = match (req.pr_repo.as_deref(), req.pr_number) {
        (Some(repo), Some(number)) if !repo.trim().is_empty() => Some(fleet::TaskPr {
            repo: repo.to_string(),
            number,
        }),
        _ => None,
    };
    match fleet::add_task(&name, &req.text, req.body, pr, req.from_msg).await {
        Ok(task) => Json(task).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct TaskDoneReq {
    done: bool,
}

/// Set a task's done flag (remote cockpit checkbox); 404 if the id isn't found.
async fn task_set_done(
    Path((name, id)): Path<(String, String)>,
    Json(req): Json<TaskDoneReq>,
) -> StatusCode {
    match fleet::set_task_done(&name, &id, req.done).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Remove a task by id (remote cockpit); 404 if the id isn't found.
async fn task_drop(Path((name, id)): Path<(String, String)>) -> StatusCode {
    match fleet::drop_task(&name, &id).await {
        Ok(true) => StatusCode::NO_CONTENT,
        Ok(false) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Deserialize)]
struct ArchiveReq {
    by: String,
}

/// Archive a shared task (remote cockpit / web ✕-to-close). Idempotent. A bad id
/// or IO error → 500; the web reloads the snapshot on failure either way.
async fn archive_shared_task(Path(id): Path<String>, Json(req): Json<ArchiveReq>) -> StatusCode {
    match fleet::archive_shared_task(&id, &req.by).await {
        Ok(_) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

// ---- Cockpit writes --------------------------------------------------------

#[derive(Deserialize)]
struct RegisterReq {
    name: String,
}

/// Register the human supervisor (the cockpit user) as a client.
async fn register(Json(req): Json<RegisterReq>) -> StatusCode {
    match fleet::register_human(&req.name).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Deserialize)]
struct SendReq {
    from: String,
    to: String,
    title: String,
    body: String,
}

/// Send one message; returns the new message id.
async fn send_message(Json(req): Json<SendReq>) -> Response {
    match fleet::send_message(&req.from, &req.to, &req.title, &req.body).await {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

#[derive(Deserialize)]
struct BroadcastReq {
    from: String,
    recipients: Vec<String>,
    title: String,
    body: String,
}

/// Fan a message out to an explicit recipient list; returns how many landed.
async fn broadcast(Json(req): Json<BroadcastReq>) -> Json<serde_json::Value> {
    let mut delivered = 0u32;
    for to in &req.recipients {
        if fleet::send_message(&req.from, to, &req.title, &req.body)
            .await
            .is_ok()
        {
            delivered += 1;
        }
    }
    Json(json!({ "delivered": delivered }))
}

#[derive(Deserialize)]
struct MarkReadReq {
    owner: String,
    id: String,
}

/// Archive a read message into the owner's `read/`.
async fn mark_read(Json(req): Json<MarkReadReq>) -> StatusCode {
    match fleet::mark_read(&req.owner, &req.id).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Delete a message from the owner's inbox (unread or archived). Reuses the
/// mark-read request shape ({owner, id}).
async fn delete_message(Json(req): Json<MarkReadReq>) -> StatusCode {
    match fleet::delete_message(&req.owner, &req.id).await {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

/// Create a project; returns the updated project list.
async fn create_project(Json(req): Json<NameReq>) -> Response {
    project_result(fleet::create_project(&req.name).await)
}

/// Rename a project by id; returns the updated list.
async fn rename_project(Path(id): Path<String>, Json(req): Json<NameReq>) -> Response {
    project_result(fleet::rename_project(&id, &req.name).await)
}

/// Move a project one slot later (last wraps front); returns the updated list.
async fn move_project(Path(id): Path<String>) -> Response {
    project_result(fleet::move_project_down(&id).await)
}

/// Remove a project; returns the updated list.
async fn remove_project(Path(id): Path<String>) -> Response {
    project_result(fleet::remove_project(&id).await)
}

#[derive(Deserialize)]
struct MembershipReq {
    client: String,
    /// `None`/absent ⇒ float the client (remove from all projects).
    project_id: Option<String>,
}

/// Set a client's project membership; returns the updated list.
async fn set_membership(Json(req): Json<MembershipReq>) -> Response {
    project_result(fleet::set_membership(&req.client, req.project_id.as_deref()).await)
}

#[derive(Deserialize)]
struct ClientReq {
    client: String,
}

/// Toggle a client as a project's coordinator; returns the updated list.
async fn toggle_coordinator(Path(id): Path<String>, Json(req): Json<ClientReq>) -> Response {
    project_result(fleet::toggle_coordinator(&id, &req.client).await)
}

/// Disconnect a client (remove its files + inbox + project membership);
/// returns the updated project list.
async fn disconnect(Path(name): Path<String>) -> Response {
    project_result(fleet::disconnect_client(&name).await)
}

/// An agent's avatar image bytes for the remote cockpit, or 404 if none.
async fn icon(Path(name): Path<String>) -> Response {
    match fleet::read_icon(&name).await {
        Some((bytes, ct)) => ([(header::CONTENT_TYPE, ct)], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Shared shaping for the project-mutating endpoints: the updated list as JSON,
/// or 500 on a write error.
fn project_result(r: anyhow::Result<Vec<crate::wire::ProjectDto>>) -> Response {
    match r {
        Ok(projects) => Json(projects).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
