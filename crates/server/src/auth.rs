//! Bearer-token auth: one layer over every route except `/healthz`. It puts
//! the caller's [`Principal`] in request extensions; handlers read it from there.

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use tracing::{debug, error, trace};

use crate::store::Store;
use crate::telemetry;

/// Middleware: 401 without a valid, unrevoked bearer token.
pub async fn require_token(State(store): State<Store>, mut req: Request, next: Next) -> Response {
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = token else {
        debug!(path = %req.uri().path(), "auth: no bearer token");
        telemetry::record_auth_failure("missing_token");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // never log the token, not even a prefix
    match store.principal(&hash_token(token.trim())).await {
        Ok(Some(principal)) => {
            trace!(user = principal.user_id, device = %principal.device, "auth ok");
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Ok(None) => {
            debug!(path = %req.uri().path(), "auth: unknown or revoked token");
            telemetry::record_auth_failure("unknown_token");
            StatusCode::UNAUTHORIZED.into_response()
        }
        Err(e) => {
            error!(error = %format!("{e:#}"), "auth: token lookup failed");
            telemetry::record_auth_failure("lookup_error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Tokens are stored only as this hash.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 256 random bits, hex, with a prefix that makes leaked tokens easy to grep for.
pub fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(format!(
        "sm_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
