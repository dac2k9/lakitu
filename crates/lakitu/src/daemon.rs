//! HTTP daemon mode (`lakitu serve`).
//!
//! Serves the same `AgentBoardService` tools as the stdio binary, but over
//! MCP-over-HTTP (rmcp's streamable-HTTP transport) so agents on other machines
//! can reach one shared fleet store. A single bearer token (`LAKITU_FLEET_TOKEN`)
//! gates every request. The stdio path (default, no subcommand) is unchanged.
//!
//! Phase 2 mounts a `/v1` REST API (for the hooks + the remote cockpit) onto
//! this same router, under this same auth layer.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::Response,
};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::server::AgentBoardService;

/// Default loopback bind. Safe by default — expose the daemon on a private
/// interface (tailscale / reverse proxy), don't bind `0.0.0.0` directly.
const DEFAULT_LISTEN: &str = "127.0.0.1:8787";

/// Run the coordination daemon: MCP-over-HTTP at `/mcp`, bearer-gated.
pub async fn serve() -> Result<()> {
    let listen = std::env::var("LAKITU_FLEET_LISTEN")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_LISTEN.to_string());

    // Fail fast if launched without a token — the daemon must never run open.
    let token = std::env::var("LAKITU_FLEET_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .context(
            "LAKITU_FLEET_TOKEN must be set (non-empty) to run `serve` — it bearer-gates every request",
        )?;
    let token = Arc::new(token);

    // One shared service; the per-session factory clones it so the project/field
    // ID cache (Arc<RwLock<…>>) is shared across all HTTP sessions instead of
    // starting cold on every connection.
    let shared = AgentBoardService::new();
    let ct = CancellationToken::new();

    let mcp = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(shared.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig {
            stateful_mode: true,
            sse_keep_alive: Some(Duration::from_secs(15)),
            sse_retry: Some(Duration::from_secs(3)),
            cancellation_token: ct.child_token(),
        },
    );

    // Bearer-gated surface: MCP-over-HTTP (`/mcp`) + the `/v1` REST API. Every
    // request here must present `Authorization: Bearer $LAKITU_FLEET_TOKEN`.
    let gated = Router::new()
        .nest_service("/mcp", mcp)
        .merge(crate::rest::router())
        .layer(middleware::from_fn_with_state(token, bearer_auth));

    // The read-only web cockpit (`/`) renders `fleet::snapshot()` in-process, so
    // the browser carries no token — which means it must never be reachable
    // off-box. We therefore mount it OUTSIDE the bearer layer, and ONLY on a
    // loopback bind; on any non-loopback bind it stays disabled (front it with a
    // TLS-terminating proxy, or use the TUI). Keep this guard if you add web
    // routes — it is the entire basis for skipping auth there.
    let loopback = listen
        .parse::<std::net::SocketAddr>()
        .map(|a| a.ip().is_loopback())
        .unwrap_or(false);
    let app = if loopback {
        tracing::info!("web cockpit at / (loopback, read-only, unauthenticated mirror)");
        gated.merge(crate::web::router())
    } else {
        tracing::warn!(
            %listen,
            "web cockpit DISABLED on non-loopback bind (its `/` mirror is unauthenticated); \
             front it with a TLS proxy or use the TUI"
        );
        gated
    };

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(%listen, "lakitu-mcp daemon listening (MCP at /mcp)");

    // Periodic reconcile loop. Re-derives shared-task state from real GitHub PR
    // state on a timer, so a merged PR advances its task (open => active,
    // merged => in-review) and fires its one-shot merge notification WITHOUT an
    // agent having to call `sweep_shared_tasks`. Read-only `gh` + store writes,
    // stamped `by = "reconcile"`; per-task errors are swallowed so the loop
    // survives a flaky `gh`. Interval is LAKITU_RECONCILE_SECS (default 150 =
    // 2.5 min); set it to 0 to disable. The child token stops it on shutdown.
    let reconcile_secs = std::env::var("LAKITU_RECONCILE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(150);
    if reconcile_secs > 0 {
        let reconcile_ct = ct.child_token();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(reconcile_secs));
            // If a pass runs long (slow gh), skip missed ticks instead of bursting.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let report = crate::server::reconcile_shared_tasks(None).await;
                        if report.advanced > 0 || report.notified > 0 {
                            tracing::info!(
                                advanced = report.advanced,
                                notified = report.notified,
                                "reconcile loop: shared tasks updated from GitHub"
                            );
                        }
                    }
                    _ = reconcile_ct.cancelled() => break,
                }
            }
        });
        tracing::info!(secs = reconcile_secs, "reconcile loop armed");
    } else {
        tracing::info!("reconcile loop disabled (LAKITU_RECONCILE_SECS=0)");
    }

    let shutdown_ct = ct.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal — cancelling sessions");
            shutdown_ct.cancel();
        })
        .await
        .context("daemon server error")?;
    Ok(())
}

/// Bearer-token gate over every route. Reads `Authorization: Bearer <token>`,
/// constant-time compares it against `LAKITU_FLEET_TOKEN`, 401s otherwise.
async fn bearer_auth(
    State(expected): State<Arc<String>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(tok) if constant_time_eq(tok.as_bytes(), expected.as_bytes()) => {
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Length-aware constant-time byte compare, so a wrong token can't be recovered
/// from response timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrey"));
        assert!(!constant_time_eq(b"secret", b"secre")); // length mismatch
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
