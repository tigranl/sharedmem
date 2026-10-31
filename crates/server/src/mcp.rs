//! MCP tools over rmcp streamable HTTP: remember, recall, forget.
//!
//! The principal comes from the auth layer: rmcp copies each HTTP request's
//! `http::request::Parts` into the tool call's `RequestContext::extensions`,
//! and the auth layer put the `Principal` in those parts' extensions.

use std::time::Instant;

use axum::http::request::Parts;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::Value;
use tracing::{debug, error, trace};

use crate::knowledge::{self, Error, Remember};
use crate::store::{Principal, Store};
use crate::telemetry;

/// The MCP server: one per request, since sessions are stateless.
#[derive(Clone)]
pub struct Mcp {
    store: Store,
}

/// Arguments of the `remember` tool.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct RememberArgs {
    /// The fact, in one or two sentences.
    text: String,
    /// The repo id from the sharedmem block at session start. Omit outside a repo.
    repo: Option<String>,
    /// "repo" (default when repo is given) or "personal" (facts about the user).
    scope: Option<String>,
    /// Stable name for the fact; saving with the same key replaces the older fact.
    key: Option<String>,
    /// Repo-relative paths of the files the fact is about.
    anchors: Option<Vec<String>>,
}

/// Arguments of the `recall` tool.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecallArgs {
    /// Words to search for.
    query: String,
    /// The current repo id; its memories rank first.
    repo: Option<String>,
    /// Maximum results, default 10.
    limit: Option<u32>,
}

/// Arguments of the `forget` tool.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ForgetArgs {
    /// Memory id from remember or recall.
    id: String,
}

fn principal(ctx: &RequestContext<RoleServer>) -> Result<Principal, ErrorData> {
    ctx.extensions
        .get::<Parts>()
        .and_then(|parts| parts.extensions.get::<Principal>())
        .cloned()
        // unreachable behind the auth layer; fail closed if it is ever mounted without it
        .ok_or_else(|| ErrorData::internal_error("no authenticated principal", None))
}

/// The tool's reply; also records the call's outcome and time since `start`.
fn result(
    tool: &'static str,
    start: Instant,
    r: Result<Value, Error>,
) -> Result<CallToolResult, ErrorData> {
    let (outcome, reply) = match r {
        Ok(v) => ("ok", Ok(CallToolResult::structured(v))),
        Err(Error::Invalid(msg)) => {
            debug!(tool, reason = %msg, "tool call rejected");
            (
                "invalid",
                Ok(CallToolResult::error(vec![ContentBlock::text(msg)])),
            )
        }
        Err(Error::Internal(e)) => {
            error!(tool, error = %format!("{e:#}"), "tool call failed");
            (
                "error",
                Err(ErrorData::internal_error("storage error", None)),
            )
        }
    };
    telemetry::record_tool_call(tool, outcome, start.elapsed());
    reply
}

#[tool_router]
impl Mcp {
    /// An MCP server over `store`.
    pub fn new(store: Store) -> Self {
        Mcp { store }
    }

    #[tool(
        description = "Save a durable fact for future sessions on any machine. \
        Returns the memory id. Never save secrets, tokens or credentials."
    )]
    async fn remember(
        &self,
        Parameters(a): Parameters<RememberArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = Instant::now();
        let p = principal(&ctx)?;
        // metadata only: the memory text stays out of logs
        debug!(
            user = p.user_id, device = %p.device, repo = ?a.repo, scope = ?a.scope, key = ?a.key,
            anchors = a.anchors.as_ref().map_or(0, Vec::len), chars = a.text.chars().count(),
            "remember"
        );
        let scope = match a.scope.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(s) => match s.parse() {
                Ok(scope) => Some(scope),
                Err(msg) => return result("remember", start, Err(Error::Invalid(msg))),
            },
        };
        let r = Remember {
            text: a.text,
            repo: a.repo,
            scope,
            key: a.key,
            anchors: a.anchors.unwrap_or_default(),
        };
        result(
            "remember",
            start,
            knowledge::remember(&self.store, &p, r).await.map(|saved| {
                telemetry::record_memory_saved(&saved);
                let m = saved.memory;
                debug!(
                    id = %m.id, scope = m.scope.as_str(), repo = ?m.repo,
                    superseded = saved.superseded, "remember saved"
                );
                serde_json::json!(m)
            }),
        )
    }

    #[tool(description = "Search saved memories by keywords, best match first. \
        Each result lists its anchors with a staleness flag; \"unknown\" means not checked against the code.")]
    async fn recall(
        &self,
        Parameters(a): Parameters<RecallArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = Instant::now();
        let p = principal(&ctx)?;
        debug!(user = p.user_id, device = %p.device, repo = ?a.repo, limit = ?a.limit, "recall");
        trace!(query = %a.query, "recall query");
        let r = knowledge::recall(&self.store, &p, &a.query, a.repo.as_deref(), a.limit).await;
        result(
            "recall",
            start,
            r.map(|memories| {
                debug!(results = memories.len(), "recall results");
                telemetry::record_recall_results(memories.len());
                serde_json::json!({ "memories": memories })
            }),
        )
    }

    #[tool(description = "Delete one of your memories by id.")]
    async fn forget(
        &self,
        Parameters(a): Parameters<ForgetArgs>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let start = Instant::now();
        let p = principal(&ctx)?;
        debug!(user = p.user_id, device = %p.device, id = %a.id, "forget");
        let r = match knowledge::forget(&self.store, &p, &a.id).await {
            Ok(true) => Ok(serde_json::json!({ "forgotten": a.id })),
            Ok(false) => Err(Error::Invalid(format!(
                "no memory {} of yours to forget",
                a.id
            ))),
            Err(e) => Err(e),
        };
        result("forget", start, r)
    }
}

#[tool_handler(
    name = "sharedmem",
    instructions = "sharedmem is the user's memory shared across their machines. \
Use `remember` to save durable facts worth knowing in a future session: decisions, conventions, \
gotchas, how to build or test, user preferences. Pass the repo id given in the sharedmem block at \
session start as `repo`, list the files the fact is about as repo-relative `anchors`, and pass a \
`key` to replace an older version of the same fact. Facts about the user rather than the code use \
scope \"personal\". Use `recall` before answering questions about past decisions or conventions, \
passing the same `repo`. A result whose anchor is changed or deleted may be out of date: check the \
code. Never store secrets, tokens, passwords or credentials."
)]
impl ServerHandler for Mcp {}
