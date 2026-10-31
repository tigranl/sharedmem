//! `GET /v1/context`: what a new session should know (Q1). Structured only;
//! the hook computes staleness locally and renders the block.

use axum::Extension;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;
use sm_proto::Context;
use tracing::{debug, error};

use crate::store::{Principal, Store};

/// Enough for any budget the hook renders; the hook truncates further.
const LIMIT: u32 = 100;

/// Query string of `GET /v1/context`.
#[derive(Deserialize)]
pub struct Params {
    repo: Option<String>,
}

/// Handler for `GET /v1/context`; personal memories only without `repo`.
pub async fn get(
    State(store): State<Store>,
    Extension(p): Extension<Principal>,
    Query(q): Query<Params>,
) -> Result<Json<Context>, StatusCode> {
    let repo = q.repo.filter(|r| !r.is_empty());
    match store.context(&p, repo.as_deref(), LIMIT).await {
        Ok(memories) => {
            debug!(user = p.user_id, device = %p.device, ?repo, memories = memories.len(), "context");
            Ok(Json(Context { memories }))
        }
        Err(e) => {
            error!(?repo, error = %format!("{e:#}"), "context failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}
