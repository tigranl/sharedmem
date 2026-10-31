//! Prometheus metrics. Every metric name lives here; other modules call the
//! `record_*` functions. [`router`] serves `GET /metrics` on its own listener,
//! apart from the authed router, so scraping needs no token.
//!
//! Label values are bounded: route templates, methods, status codes, scopes
//! and fixed `&'static str` outcomes. Never ids, users, devices, repos, paths,
//! query text or memory text.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Router;
use axum::extract::{MatchedPath, State};
use axum::http::{Method, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{BuildError, Matcher, PrometheusBuilder, PrometheusHandle};
use tracing::error;

use crate::store::{Saved, Stats, Store};

const HTTP_REQUESTS: &str = "sharedmem_http_requests_total";
const HTTP_REQUEST_DURATION: &str = "sharedmem_http_request_duration_seconds";
const AUTH_FAILURES: &str = "sharedmem_auth_failures_total";
const TOOL_CALLS: &str = "sharedmem_tool_calls_total";
const TOOL_DURATION: &str = "sharedmem_tool_duration_seconds";
const RECALL_RESULTS: &str = "sharedmem_recall_results";
const MEMORIES_SAVED: &str = "sharedmem_memories_saved_total";
const MEMORIES_SUPERSEDED: &str = "sharedmem_memories_superseded_total";
const ANCHOR_BASELINES: &str = "sharedmem_anchor_baselines_total";
const CONTEXT_MEMORIES: &str = "sharedmem_context_memories";
const MEMORIES: &str = "sharedmem_memories";
const ANCHORS: &str = "sharedmem_anchors";
const TOKENS: &str = "sharedmem_tokens";
const DB_SIZE: &str = "sharedmem_db_size_bytes";
const SCHEMA_VERSION: &str = "sharedmem_schema_version";
const BUILD_INFO: &str = "sharedmem_build_info";

#[derive(Clone, Copy)]
enum Kind {
    Counter,
    Gauge,
    Histogram,
}

/// One metric's `# HELP` text.
struct Doc {
    name: &'static str,
    kind: Kind,
    help: &'static str,
}

/// Every metric. [`install`] registers these as `# HELP` lines, and a test
/// checks that the README's metrics table lists exactly these names.
const DOCS: [Doc; 16] = [
    Doc {
        name: HTTP_REQUESTS,
        kind: Kind::Counter,
        help: "Requests that matched a route, by route template, method and status.",
    },
    Doc {
        name: HTTP_REQUEST_DURATION,
        kind: Kind::Histogram,
        help: "Time to serve a request that matched a route.",
    },
    Doc {
        name: AUTH_FAILURES,
        kind: Kind::Counter,
        help: "Requests refused by the bearer-token check, by reason.",
    },
    Doc {
        name: TOOL_CALLS,
        kind: Kind::Counter,
        help: "MCP tool calls by tool and outcome: ok, invalid input, or server error.",
    },
    Doc {
        name: TOOL_DURATION,
        kind: Kind::Histogram,
        help: "Time per MCP tool call.",
    },
    Doc {
        name: RECALL_RESULTS,
        kind: Kind::Histogram,
        help: "Memories returned by each successful recall; the le=0 bucket counts empty ones.",
    },
    Doc {
        name: MEMORIES_SAVED,
        kind: Kind::Counter,
        help: "Memories saved with remember, by scope.",
    },
    Doc {
        name: MEMORIES_SUPERSEDED,
        kind: Kind::Counter,
        help: "Saves whose key replaced an older memory.",
    },
    Doc {
        name: ANCHOR_BASELINES,
        kind: Kind::Counter,
        help: "Anchor-baseline posts from the hook after remember, by outcome.",
    },
    Doc {
        name: CONTEXT_MEMORIES,
        kind: Kind::Histogram,
        help: "Memories returned for a session start, before the hook trims them to its budget.",
    },
    Doc {
        name: MEMORIES,
        kind: Kind::Gauge,
        help: "Memories in the database by scope and state, read at scrape time.",
    },
    Doc {
        name: ANCHORS,
        kind: Kind::Gauge,
        help: "Anchors with and without a recorded baseline; missing means the post-save hook never reported.",
    },
    Doc {
        name: TOKENS,
        kind: Kind::Gauge,
        help: "Device tokens by state.",
    },
    Doc {
        name: DB_SIZE,
        kind: Kind::Gauge,
        help: "Size of the database file plus its WAL.",
    },
    Doc {
        name: SCHEMA_VERSION,
        kind: Kind::Gauge,
        help: "Applied schema migration (PRAGMA user_version).",
    },
    Doc {
        name: BUILD_INFO,
        kind: Kind::Gauge,
        help: "Always 1; the version label is the running sm-server version.",
    },
];

/// Seconds, 1 ms to 2.5 s.
const LATENCY_BUCKETS: [f64; 11] = [
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
];
/// Memories per response; the `le="0"` bucket counts the empty ones.
const COUNT_BUCKETS: [f64; 8] = [0.0, 1.0, 2.0, 3.0, 5.0, 10.0, 20.0, 50.0];

const UPKEEP_EVERY: Duration = Duration::from_secs(5);

/// Installs the global recorder and records `sharedmem_build_info`. Then spawn
/// [`run_upkeep`] and serve [`router`]. Fails if a recorder is already installed.
pub fn install() -> Result<PrometheusHandle, BuildError> {
    // without explicit buckets the exporter renders summaries, which have no `le` buckets
    let mut builder = PrometheusBuilder::new();
    for name in [HTTP_REQUEST_DURATION, TOOL_DURATION] {
        builder =
            builder.set_buckets_for_metric(Matcher::Full(name.to_owned()), &LATENCY_BUCKETS)?;
    }
    for name in [RECALL_RESULTS, CONTEXT_MEMORIES] {
        builder = builder.set_buckets_for_metric(Matcher::Full(name.to_owned()), &COUNT_BUCKETS)?;
    }
    let handle = builder.install_recorder()?;
    describe();
    gauge!(BUILD_INFO, "version" => env!("CARGO_PKG_VERSION")).set(1.0);
    Ok(handle)
}

/// Registers [`DOCS`] with the installed recorder. A metric's `# HELP` line
/// appears once the metric has a series.
fn describe() {
    for doc in &DOCS {
        match doc.kind {
            Kind::Counter => describe_counter!(doc.name, doc.help),
            Kind::Gauge => describe_gauge!(doc.name, doc.help),
            Kind::Histogram => describe_histogram!(doc.name, doc.help),
        }
    }
}

/// Runs forever. `install_recorder` leaves upkeep to the caller; without it,
/// histogram samples pile up in memory when nothing scrapes.
pub async fn run_upkeep(handle: PrometheusHandle) {
    let mut every = tokio::time::interval(UPKEEP_EVERY);
    loop {
        every.tick().await;
        handle.run_upkeep();
    }
}

/// `GET /metrics`: refreshes the database gauges, then renders every metric.
/// `db` is the database file, for `sharedmem_db_size_bytes`.
pub fn router(handle: PrometheusHandle, store: Store, db: PathBuf) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(Scrape { handle, store, db })
}

#[derive(Clone)]
struct Scrape {
    handle: PrometheusHandle,
    store: Store,
    db: PathBuf,
}

async fn scrape(State(s): State<Scrape>) -> impl IntoResponse {
    match s.store.stats().await {
        Ok(stats) => set_db_gauges(&stats),
        // still serve the request metrics; the db gauges keep their last values
        Err(e) => error!(error = %format!("{e:#}"), "metrics: db stats failed"),
    }
    gauge!(DB_SIZE).set(db_size(&s.db) as f64);
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        s.handle.render(),
    )
}

fn set_db_gauges(s: &Stats) {
    for m in &s.memories {
        let scope = m.scope.as_str();
        gauge!(MEMORIES, "scope" => scope, "state" => "active").set(m.active as f64);
        gauge!(MEMORIES, "scope" => scope, "state" => "superseded").set(m.superseded as f64);
        gauge!(MEMORIES, "scope" => scope, "state" => "deleted").set(m.deleted as f64);
    }
    gauge!(ANCHORS, "baseline" => "set").set(s.anchors_with_baseline as f64);
    gauge!(ANCHORS, "baseline" => "missing").set(s.anchors_missing_baseline as f64);
    gauge!(TOKENS, "state" => "active").set(s.tokens_active as f64);
    gauge!(TOKENS, "state" => "revoked").set(s.tokens_revoked as f64);
    gauge!(SCHEMA_VERSION).set(s.schema_version);
}

/// The database file plus its WAL, which holds recent writes until a checkpoint.
fn db_size(db: &Path) -> u64 {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    [db, Path::new(&wal)]
        .iter()
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

/// A request that matched a route, labelled by the route's template so ids
/// in the path never become label values.
pub fn record_request(route: &MatchedPath, method: &Method, status: StatusCode, elapsed: Duration) {
    let (route, method) = (route.as_str().to_owned(), method_label(method));
    let status = status.as_u16().to_string();
    counter!(HTTP_REQUESTS, "route" => route.clone(), "method" => method, "status" => status)
        .increment(1);
    histogram!(HTTP_REQUEST_DURATION, "route" => route, "method" => method).record(elapsed);
}

/// `/mcp` takes any method, so a client could mint label values; unknown ones share one.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::DELETE => "DELETE",
        _ => "other",
    }
}

/// A refused request: `reason` is `missing_token`, `unknown_token` or `lookup_error`.
pub fn record_auth_failure(reason: &'static str) {
    counter!(AUTH_FAILURES, "reason" => reason).increment(1);
}

/// One MCP tool call: `outcome` is `ok`, `invalid` or `error`.
pub fn record_tool_call(tool: &'static str, outcome: &'static str, elapsed: Duration) {
    counter!(TOOL_CALLS, "tool" => tool, "outcome" => outcome).increment(1);
    histogram!(TOOL_DURATION, "tool" => tool).record(elapsed);
}

/// How many memories one successful `recall` returned.
pub fn record_recall_results(count: usize) {
    histogram!(RECALL_RESULTS).record(count as f64);
}

/// A saved memory, and whether its key replaced an older one.
pub fn record_memory_saved(saved: &Saved) {
    counter!(MEMORIES_SAVED, "scope" => saved.memory.scope.as_str()).increment(1);
    if saved.superseded {
        counter!(MEMORIES_SUPERSEDED).increment(1);
    }
}

/// One anchor-baseline post: `outcome` is `set`, `not_found`, `invalid` or `error`.
pub fn record_anchor_baselines(outcome: &'static str) {
    counter!(ANCHOR_BASELINES, "outcome" => outcome).increment(1);
}

/// How many memories `GET /v1/context` returned, before the hook truncates them.
pub fn record_context_memories(count: usize) {
    histogram!(CONTEXT_MEMORIES).record(count as f64);
}

#[cfg(test)]
mod tests {
    use super::*;

    const README: &str = include_str!("../../../README.md");

    /// Names in the README's metrics table: the first cell of rows that start
    /// with a metric name.
    fn readme_metrics() -> Vec<&'static str> {
        README
            .lines()
            .filter(|line| line.starts_with("| `sharedmem_"))
            .filter_map(|line| line.split('`').nth(1))
            .collect()
    }

    #[test]
    fn readme_table_lists_exactly_the_metrics() {
        let documented = readme_metrics();
        for doc in &DOCS {
            assert!(
                documented.contains(&doc.name),
                "{} is missing from the README metrics table",
                doc.name
            );
        }
        for name in &documented {
            assert!(
                DOCS.iter().any(|doc| doc.name == *name),
                "the README documents {name}, which telemetry.rs doesn't define"
            );
        }
        assert_eq!(
            documented.len(),
            DOCS.len(),
            "a metric is listed twice in the README"
        );
    }
}
