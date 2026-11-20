//! The router: auth over everything except `/healthz`.

use std::time::Instant;

use axum::extract::{MatchedPath, Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router, middleware};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use sm_proto::{AnchorBaseline, HookRuns};
use tracing::{debug, error, warn};

use crate::knowledge::{self, Error};
use crate::store::{Principal, Store};
use crate::{auth, context, mcp::Mcp, telemetry};

/// All routes: `/healthz`, `/v1/...` and `/mcp`.
pub fn router(store: Store) -> Router {
    let mcp_store = store.clone();
    let mcp = StreamableHttpService::new(
        move || Ok(Mcp::new(mcp_store.clone())),
        NeverSessionManager::default().into(),
        // Stateless JSON: nothing to lose when the server restarts.
        // Host checks guard unauthenticated local servers against DNS rebinding;
        // every request here needs a bearer token, and Tailscale names must work.
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .with_sse_keep_alive(None)
            .disable_allowed_hosts(),
    );
    let protected = Router::new()
        .route("/v1/context", get(context::get))
        .route("/v1/memories/{id}/anchors", post(anchors))
        .route("/v1/hook-runs", post(hook_runs))
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn_with_state(
            store.clone(),
            auth::require_token,
        ));
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(protected)
        // a route layer, so requests that match no route are neither logged nor counted
        .route_layer(middleware::from_fn(log_request))
        .with_state(store)
}

/// One line per request: debug normally, warn on 5xx. Path only; query
/// strings and bodies are never logged. Metrics use the route template.
async fn log_request(req: Request, next: Next) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    // absent only under `/mcp/...`, which axum matches as a nested wildcard
    let route = req.extensions().get::<MatchedPath>().cloned();
    let start = Instant::now();
    let res = next.run(req).await;
    let elapsed = start.elapsed();
    if let Some(route) = &route {
        telemetry::record_request(route, &method, res.status(), elapsed);
    }
    let (status, ms) = (res.status().as_u16(), elapsed.as_millis() as u64);
    if res.status().is_server_error() {
        warn!(%method, %path, status, ms, "request");
    } else {
        debug!(%method, %path, status, ms, "request");
    }
    res
}

/// Most runs one `POST /v1/hook-runs` may carry; the hook sends at most 500.
const MAX_HOOK_RUNS: usize = 1_000;

/// The hook's reports of its own runs. They only feed metrics; nothing is stored.
async fn hook_runs(Json(body): Json<HookRuns>) -> StatusCode {
    if body.runs.len() > MAX_HOOK_RUNS {
        return StatusCode::PAYLOAD_TOO_LARGE;
    }
    for run in &body.runs {
        telemetry::record_hook_run(run);
    }
    debug!(runs = body.runs.len(), "hook runs reported");
    StatusCode::NO_CONTENT
}

async fn anchors(
    State(store): State<Store>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    Json(body): Json<AnchorBaseline>,
) -> StatusCode {
    let (paths, commit) = (body.blobs.len(), body.commit.clone());
    match knowledge::set_baselines(&store, &p, &id, body).await {
        // the hook drops anchors it can't read, so a post can match none
        Ok(Some(0)) => {
            debug!(user = p.user_id, %id, paths, "anchor baselines: no anchor without one matched");
            telemetry::record_anchor_baselines("unmatched");
            StatusCode::NO_CONTENT
        }
        Ok(Some(set)) => {
            debug!(user = p.user_id, %id, paths, set, ?commit, "anchor baselines set");
            telemetry::record_anchor_baselines("set");
            StatusCode::NO_CONTENT
        }
        Ok(None) => {
            debug!(user = p.user_id, %id, "anchor baselines: no such memory of this user");
            telemetry::record_anchor_baselines("not_found");
            StatusCode::NOT_FOUND
        }
        Err(Error::Invalid(reason)) => {
            debug!(user = p.user_id, %id, %reason, "anchor baselines rejected");
            telemetry::record_anchor_baselines("invalid");
            StatusCode::BAD_REQUEST
        }
        Err(Error::Internal(e)) => {
            error!(%id, error = %format!("{e:#}"), "anchor baselines failed");
            telemetry::record_anchor_baselines("error");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}
