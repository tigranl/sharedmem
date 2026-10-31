//! Integration tests: the real router against a temp SQLite file.

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sm_server::{auth, http, store::Store};
use tower::ServiceExt;

struct App {
    router: Router,
    store: Store,
    db: std::path::PathBuf,
    _dir: TempDir,
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn app() -> App {
    let dir = std::env::temp_dir().join(format!("sm-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("sm.db");
    let store = Store::open(&db).unwrap();
    App {
        router: http::router(store.clone()),
        store,
        db,
        _dir: TempDir(dir),
    }
}

impl App {
    async fn token(&self, user: &str, device: &str) -> String {
        let token = auth::new_token().unwrap();
        self.store
            .add_token(user, device, &auth::hash_token(&token))
            .await
            .unwrap();
        token
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())),
        )
    }

    async fn get(&self, token: Option<&str>, uri: &str) -> (StatusCode, Value) {
        let mut req = Request::get(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        self.send(req.body(Body::empty()).unwrap()).await
    }

    async fn post(&self, token: &str, uri: &str, body: Value) -> StatusCode {
        let req = Request::post(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(req).await.0
    }

    /// One MCP `tools/call`; returns the tool result object.
    async fn call(&self, token: &str, tool: &str, args: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": tool, "arguments": args}});
        let req = Request::post("/mcp")
            .header(header::HOST, "localhost")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .body(Body::from(body.to_string()))
            .unwrap();
        let (status, v) = self.send(req).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        v["result"].clone()
    }

    async fn remember(&self, token: &str, args: Value) -> String {
        let r = self.call(token, "remember", args).await;
        assert_eq!(r["isError"], false, "{r}");
        r["structuredContent"]["id"].as_str().unwrap().to_string()
    }

    async fn recall(&self, token: &str, args: Value) -> Vec<Value> {
        let r = self.call(token, "recall", args).await;
        assert_eq!(r["isError"], false, "{r}");
        r["structuredContent"]["memories"]
            .as_array()
            .unwrap()
            .clone()
    }
}

const REPO: &str = "github.com/tigran/osshunt";

#[tokio::test]
async fn auth_refuses_missing_and_bad_tokens() {
    let app = app();
    let good = app.token("a", "laptop").await;
    assert_eq!(app.get(None, "/healthz").await.0, StatusCode::OK);
    for uri in ["/v1/context", "/mcp"] {
        assert_eq!(app.get(None, uri).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            app.get(Some("sm_wrong"), uri).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(app.get(Some(&good), "/v1/context").await.0, StatusCode::OK);
    // a valid token sent without the Bearer scheme is still refused
    let req = Request::get("/v1/context")
        .header(header::AUTHORIZATION, &good)
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.send(req).await.0, StatusCode::UNAUTHORIZED);

    // revoking, as the README describes, locks the device out
    let conn = rusqlite::Connection::open(&app.db).unwrap();
    conn.execute(
        "UPDATE tokens SET revoked_at = 1 WHERE device = 'laptop'",
        [],
    )
    .unwrap();
    assert_eq!(
        app.get(Some(&good), "/v1/context").await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn remember_then_recall() {
    let app = app();
    let t = app.token("a", "laptop").await;
    let id = app
        .remember(
            &t,
            json!({"text": "Seeded ATS slugs are often wrong; fix companies.yaml first.",
                   "repo": REPO, "anchors": ["./companies.yaml"]}),
        )
        .await;
    app.remember(
        &t,
        json!({"text": "Prefers building over scoping questions."}),
    )
    .await;

    let hits = app
        .recall(&t, json!({"query": "ATS slugs", "repo": REPO}))
        .await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["id"], id.as_str());
    assert_eq!(hits[0]["scope"], "repo");
    assert_eq!(hits[0]["anchors"][0]["path"], "companies.yaml");
    assert_eq!(hits[0]["staleness"], json!(["unknown"]));

    let personal = app.recall(&t, json!({"query": "building"})).await;
    assert_eq!(personal[0]["scope"], "personal");
}

#[tokio::test]
async fn recall_ranks_given_repo_first() {
    let app = app();
    let t = app.token("a", "laptop").await;
    app.remember(
        &t,
        json!({"text": "deploy deploy deploy with make", "repo": "x/other"}),
    )
    .await;
    let mine = app
        .remember(&t, json!({"text": "deploy via CI", "repo": REPO}))
        .await;
    let hits = app
        .recall(&t, json!({"query": "deploy", "repo": REPO}))
        .await;
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0]["id"], mine.as_str());
}

#[tokio::test]
async fn key_supersedes_older_fact() {
    let app = app();
    let t = app.token("a", "laptop").await;
    let old = app
        .remember(
            &t,
            json!({"text": "tests run with pytest", "repo": REPO, "key": "test-cmd"}),
        )
        .await;
    let new = app
        .remember(
            &t,
            json!({"text": "tests run with uv run pytest", "repo": REPO, "key": "test-cmd"}),
        )
        .await;
    assert_ne!(old, new);
    let hits = app.recall(&t, json!({"query": "pytest"})).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["id"], new.as_str());

    // same key in another repo is a different fact
    app.remember(
        &t,
        json!({"text": "tests run with cargo", "repo": "x/other", "key": "test-cmd"}),
    )
    .await;
    assert_eq!(app.recall(&t, json!({"query": "tests"})).await.len(), 2);

    // the superseded row is kept, pointing at its replacement
    let conn = rusqlite::Connection::open(&app.db).unwrap();
    let superseded_by: String = conn
        .query_row(
            "SELECT superseded_by FROM memories WHERE id = ?1",
            [&old],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(superseded_by, new);
}

#[tokio::test]
async fn users_cannot_see_or_forget_each_others_memories() {
    let app = app();
    let a = app.token("a", "laptop").await;
    let b = app.token("b", "desktop").await;
    let personal = app
        .remember(&a, json!({"text": "a's secret preference for tabs"}))
        .await;
    let repo = app
        .remember(
            &a,
            json!({"text": "a's repo fact about tabs", "repo": REPO, "anchors": ["x.rs"]}),
        )
        .await;
    app.remember(&b, json!({"text": "b likes tabs too", "key": "editor"}))
        .await;
    // b's key does not supersede a's facts
    app.remember(&a, json!({"text": "a likes spaces", "key": "editor"}))
        .await;

    let b_hits = app.recall(&b, json!({"query": "tabs spaces"})).await;
    assert_eq!(b_hits.len(), 1, "{b_hits:?}");
    assert_eq!(b_hits[0]["text"], "b likes tabs too");
    assert_eq!(
        app.recall(&a, json!({"query": "tabs spaces"})).await.len(),
        3
    );

    let (_, ctx) = app.get(Some(&b), &format!("/v1/context?repo={REPO}")).await;
    assert_eq!(ctx["memories"].as_array().unwrap().len(), 1);

    for id in [&personal, &repo] {
        let r = app.call(&b, "forget", json!({"id": id})).await;
        assert_eq!(r["isError"], true, "{r}");
    }
    let baseline = json!({"commit": "abc123", "blobs": [{"path": "x.rs", "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"}]});
    assert_eq!(
        app.post(&b, &format!("/v1/memories/{repo}/anchors"), baseline)
            .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.recall(&a, json!({"query": "tabs spaces"})).await.len(),
        3
    );

    let r = app.call(&a, "forget", json!({"id": personal})).await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(
        app.recall(&a, json!({"query": "tabs spaces"})).await.len(),
        2
    );
}

#[tokio::test]
async fn anchor_baseline_is_set_once_and_returned_in_context() {
    let app = app();
    let t = app.token("a", "laptop").await;
    app.remember(&t, json!({"text": "personal note"})).await;
    let id = app
        .remember(
            &t,
            json!({"text": "fact about two files", "repo": REPO,
                             "anchors": ["src/a.rs", "src/b.rs"]}),
        )
        .await;
    let (_, ctx) = app.get(Some(&t), &format!("/v1/context?repo={REPO}")).await;
    assert_eq!(
        ctx["memories"][0]["anchors"][0]["blob_at_save"],
        Value::Null
    );

    let blob = "ce013625030ba8dba906f756967f9e9ca394464a";
    let body = json!({"commit": "0123abcd", "blobs": [{"path": "src/a.rs", "blob": blob}]});
    let uri = format!("/v1/memories/{id}/anchors");
    assert_eq!(app.post(&t, &uri, body).await, StatusCode::NO_CONTENT);
    // a later post cannot move the baseline
    let other = json!({"commit": "ffff", "blobs": [{"path": "src/a.rs", "blob": "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"}]});
    assert_eq!(app.post(&t, &uri, other).await, StatusCode::NO_CONTENT);
    let bad = json!({"commit": null, "blobs": [{"path": "src/a.rs", "blob": "nope"}]});
    assert_eq!(app.post(&t, &uri, bad).await, StatusCode::BAD_REQUEST);

    let (status, ctx) = app.get(Some(&t), &format!("/v1/context?repo={REPO}")).await;
    assert_eq!(status, StatusCode::OK);
    let mems = ctx["memories"].as_array().unwrap();
    assert_eq!(mems.len(), 2);
    assert_eq!(mems[0]["scope"], "repo");
    assert_eq!(mems[1]["scope"], "personal");
    assert_eq!(
        mems[0]["anchors"],
        json!([
            {"path": "src/a.rs", "blob_at_save": blob, "commit_at_save": "0123abcd"},
            {"path": "src/b.rs", "blob_at_save": null, "commit_at_save": null}
        ])
    );

    // outside a repo: personal only
    let (_, ctx) = app.get(Some(&t), "/v1/context").await;
    assert_eq!(ctx["memories"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn fts_special_characters_are_safe() {
    let app = app();
    let t = app.token("a", "laptop").await;
    app.remember(&t, json!({"text": "the foo service needs bar configured"}))
        .await;
    for q in [
        "foo\"bar AND (",
        "NEAR(",
        "*",
        "\"",
        "-foo",
        "foo:bar",
        ")))",
    ] {
        let r = app.call(&t, "recall", json!({"query": q})).await;
        assert_eq!(r["isError"], false, "{q}: {r}");
    }
    assert_eq!(
        app.recall(&t, json!({"query": "foo\"bar AND ("}))
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn remember_rejects_bad_input_loudly() {
    let app = app();
    let t = app.token("a", "laptop").await;
    for args in [
        json!({"text": "  "}),
        json!({"text": "x", "scope": "repo"}),
        json!({"text": "x", "scope": "team"}),
        json!({"text": "x", "anchors": ["a.rs"]}),
        json!({"text": "x", "repo": REPO, "anchors": ["/etc/passwd"]}),
        json!({"text": "x", "repo": REPO, "anchors": ["../up.rs"]}),
    ] {
        let r = app.call(&t, "remember", args.clone()).await;
        assert_eq!(r["isError"], true, "{args}: {r}");
    }
}
