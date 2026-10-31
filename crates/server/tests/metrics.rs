//! Metrics: drive the real router, then scrape `/metrics`. Its own test binary,
//! so the process-wide recorder is installed once and only this test records.

use std::sync::OnceLock;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use metrics_exporter_prometheus::PrometheusHandle;
use serde_json::{Value, json};
use sm_server::{auth, http, store::Store, telemetry};
use tower::ServiceExt;

const REPO: &str = "github.com/tigran/private-repo";
const TEXT: &str = "The staging deploy key lives in vault under ops-secret-path.";
const QUERY: &str = "xylophonequartz";

static RECORDER: OnceLock<PrometheusHandle> = OnceLock::new();

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn send(router: &Router, req: Request<Body>) -> (StatusCode, String) {
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn get(token: Option<&str>, uri: &str) -> Request<Body> {
    let mut req = Request::get(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    req.body(Body::empty()).unwrap()
}

fn post(token: &str, uri: &str, body: &Value) -> Request<Body> {
    Request::post(uri)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// One MCP `tools/call`; returns the tool result object.
async fn call(router: &Router, token: &str, tool: &str, args: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": tool, "arguments": args}});
    let req = Request::post("/mcp")
        .header(header::HOST, "localhost")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, text) = send(router, req).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str::<Value>(&text).unwrap()["result"].clone()
}

/// The value of the first series of `name` whose labels include all of `labels`.
fn value(text: &str, name: &str, labels: &[&str]) -> Option<f64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        if !rest.starts_with(['{', ' ']) {
            return None;
        }
        let (series, v) = line.rsplit_once(' ')?;
        if labels.iter().all(|l| series.contains(l)) {
            v.parse().ok()
        } else {
            None
        }
    })
}

#[track_caller]
fn assert_value(text: &str, name: &str, labels: &[&str], want: f64) {
    assert_eq!(
        value(text, name, labels),
        Some(want),
        "{name} {labels:?} in:\n{text}"
    );
}

fn has_uuid(text: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_hexdigit() || c == '-'))
        .any(|w| w.len() == 36 && w.matches('-').count() == 4)
}

#[tokio::test]
async fn metrics_reflect_requests_and_db_with_bounded_labels() {
    let handle = RECORDER
        .get_or_init(|| telemetry::install().expect("the only recorder in this test binary"))
        .clone();
    let dir = std::env::temp_dir().join(format!("sm-metrics-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let _dir = TempDir(dir.clone());
    let db = dir.join("sm.db");
    let store = Store::open(&db).unwrap();
    let app = http::router(store.clone());
    let metrics = telemetry::router(handle, store.clone(), db.clone());

    let token = auth::new_token().unwrap();
    store
        .add_token("tigran", "laptop", &auth::hash_token(&token))
        .await
        .unwrap();
    let revoked = auth::new_token().unwrap();
    store
        .add_token("tigran", "old-phone", &auth::hash_token(&revoked))
        .await
        .unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE tokens SET revoked_at = 1 WHERE device = 'old-phone'",
        [],
    )
    .unwrap();

    assert_eq!(send(&app, get(None, "/healthz")).await.0, StatusCode::OK);
    assert_eq!(
        send(&app, get(None, "/v1/context")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&app, get(Some(&revoked), "/mcp")).await.0,
        StatusCode::UNAUTHORIZED
    );
    let unmatched = "/v1/no-such-route";
    assert_eq!(
        send(&app, get(Some(&token), unmatched)).await.0,
        StatusCode::NOT_FOUND
    );
    // reaches rmcp through axum's nested wildcard, which sets no MatchedPath
    let nested = "/mcp/nested-tail-path";
    send(&app, get(Some(&token), nested)).await;

    // a keyed memory, then its replacement with two anchors; the superseded
    // memory's anchor must not count in sharedmem_anchors
    let first = call(
        &app,
        &token,
        "remember",
        json!({"text": "old fact", "repo": REPO, "key": "deploy", "anchors": ["ops/old.sh"]}),
    )
    .await;
    assert_eq!(first["isError"], false, "{first}");
    let r = call(
        &app,
        &token,
        "remember",
        json!({"text": TEXT, "repo": REPO, "key": "deploy", "anchors": ["ops/deploy.sh", "ops/vault.hcl"]}),
    )
    .await;
    assert_eq!(r["isError"], false, "{r}");
    let id = r["structuredContent"]["id"].as_str().unwrap().to_owned();
    let personal = call(&app, &token, "remember", json!({"text": "likes tabs"})).await;
    let personal_id = personal["structuredContent"]["id"].as_str().unwrap();
    let forgot = call(&app, &token, "forget", json!({"id": personal_id})).await;
    assert_eq!(forgot["isError"], false, "{forgot}");
    let bad = call(&app, &token, "remember", json!({"text": "  "})).await;
    assert_eq!(bad["isError"], true, "{bad}");

    let baseline = json!({"commit": "0123abcd", "blobs": [
        {"path": "ops/deploy.sh", "blob": "ce013625030ba8dba906f756967f9e9ca394464a"}]});
    let uri = format!("/v1/memories/{id}/anchors");
    // the second post sets nothing: the first baseline wins
    for _ in 0..2 {
        assert_eq!(
            send(&app, post(&token, &uri, &baseline)).await.0,
            StatusCode::NO_CONTENT
        );
    }

    let none = call(&app, &token, "recall", json!({"query": QUERY})).await;
    assert_eq!(none["structuredContent"]["memories"], json!([]), "{none}");
    let some = call(&app, &token, "recall", json!({"query": "staging deploy"})).await;
    assert_eq!(
        some["structuredContent"]["memories"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "{some}"
    );
    let (status, _) = send(&app, get(Some(&token), &format!("/v1/context?repo={REPO}"))).await;
    assert_eq!(status, StatusCode::OK);

    let (status, text) = send(&metrics, get(None, "/metrics")).await;
    assert_eq!(status, StatusCode::OK);

    // requests, by route template
    let requests = "sharedmem_http_requests_total";
    assert_value(
        &text,
        requests,
        &[r#"route="/healthz""#, r#"status="200""#],
        1.0,
    );
    assert_value(
        &text,
        requests,
        &[r#"route="/v1/context""#, r#"status="401""#],
        1.0,
    );
    assert_value(
        &text,
        requests,
        &[r#"route="/mcp""#, r#"method="GET""#, r#"status="401""#],
        1.0,
    );
    assert_value(
        &text,
        requests,
        &[r#"route="/mcp""#, r#"method="POST""#, r#"status="200""#],
        7.0,
    );
    assert_value(
        &text,
        requests,
        &[
            r#"route="/v1/memories/{id}/anchors""#,
            r#"method="POST""#,
            r#"status="204""#,
        ],
        2.0,
    );
    assert_value(
        &text,
        "sharedmem_http_request_duration_seconds_count",
        &[r#"route="/v1/memories/{id}/anchors""#],
        2.0,
    );
    for raw in [unmatched, nested, "__private__"] {
        assert!(!text.contains(raw), "{raw:?} became a route label:\n{text}");
    }

    assert_value(
        &text,
        "sharedmem_auth_failures_total",
        &[r#"reason="missing_token""#],
        1.0,
    );
    assert_value(
        &text,
        "sharedmem_auth_failures_total",
        &[r#"reason="unknown_token""#],
        1.0,
    );

    let tools = "sharedmem_tool_calls_total";
    assert_value(
        &text,
        tools,
        &[r#"tool="remember""#, r#"outcome="ok""#],
        3.0,
    );
    assert_value(
        &text,
        tools,
        &[r#"tool="remember""#, r#"outcome="invalid""#],
        1.0,
    );
    assert_value(&text, tools, &[r#"tool="recall""#, r#"outcome="ok""#], 2.0);
    assert_value(&text, tools, &[r#"tool="forget""#, r#"outcome="ok""#], 1.0);
    assert_value(
        &text,
        "sharedmem_tool_duration_seconds_count",
        &[r#"tool="remember""#],
        4.0,
    );

    // a real histogram, not a summary: the zero-result bucket exists
    assert_value(
        &text,
        "sharedmem_recall_results_bucket",
        &[r#"le="0""#],
        1.0,
    );
    assert_value(&text, "sharedmem_recall_results_count", &[], 2.0);
    assert_value(&text, "sharedmem_context_memories_count", &[], 1.0);

    assert_value(
        &text,
        "sharedmem_memories_saved_total",
        &[r#"scope="repo""#],
        2.0,
    );
    assert_value(
        &text,
        "sharedmem_memories_saved_total",
        &[r#"scope="personal""#],
        1.0,
    );
    assert_value(&text, "sharedmem_memories_superseded_total", &[], 1.0);
    assert_value(
        &text,
        "sharedmem_anchor_baselines_total",
        &[r#"outcome="set""#],
        1.0,
    );
    assert_value(
        &text,
        "sharedmem_anchor_baselines_total",
        &[r#"outcome="unmatched""#],
        1.0,
    );

    // gauges, read from the db at scrape time
    let memories = "sharedmem_memories";
    assert_value(
        &text,
        memories,
        &[r#"scope="repo""#, r#"state="active""#],
        1.0,
    );
    assert_value(
        &text,
        memories,
        &[r#"scope="repo""#, r#"state="superseded""#],
        1.0,
    );
    assert_value(
        &text,
        memories,
        &[r#"scope="repo""#, r#"state="deleted""#],
        0.0,
    );
    assert_value(
        &text,
        memories,
        &[r#"scope="personal""#, r#"state="active""#],
        0.0,
    );
    assert_value(
        &text,
        memories,
        &[r#"scope="personal""#, r#"state="deleted""#],
        1.0,
    );
    assert_value(&text, "sharedmem_anchors", &[r#"baseline="set""#], 1.0);
    assert_value(&text, "sharedmem_anchors", &[r#"baseline="missing""#], 1.0);
    assert_value(&text, "sharedmem_tokens", &[r#"state="active""#], 1.0);
    assert_value(&text, "sharedmem_tokens", &[r#"state="revoked""#], 1.0);
    let schema: u32 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_value(&text, "sharedmem_schema_version", &[], f64::from(schema));
    let size = value(&text, "sharedmem_db_size_bytes", &[]).expect("db size gauge");
    assert!(size > 0.0, "db size {size}");
    let version = format!(r#"version="{}""#, env!("CARGO_PKG_VERSION"));
    assert_value(&text, "sharedmem_build_info", &[&version], 1.0);

    // every exported metric carries its description
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("# TYPE ") else {
            continue;
        };
        let name = rest
            .split(' ')
            .next()
            .expect("a # TYPE line names its metric");
        assert!(
            text.contains(&format!("# HELP {name} ")),
            "{name} has no # HELP line"
        );
    }

    // nothing identifying, at any label
    assert!(!has_uuid(&text), "a uuid leaked into the metrics:\n{text}");
    for secret in [
        token.as_str(),
        revoked.as_str(),
        TEXT,
        "staging",
        QUERY,
        REPO,
        "ops/deploy.sh",
        "tigran",
        "laptop",
    ] {
        assert!(!text.contains(secret), "{secret:?} leaked into the metrics");
    }
}
