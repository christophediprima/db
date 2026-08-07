//! `POST /bm25/track|untrack` and `GET /bm25/tracking`.
//!
//! Driven through the axum `Router` rather than against `AppState` directly,
//! because the thing under test is the wiring: whether a request handler can
//! reach the maintenance worker at all. `FlureeServer::run` owns the spawn, so
//! these publish a worker into the same slot `run` uses and then go through
//! the routes.

use axum::body::Body;
use axum::http::{self, Request, StatusCode};
use axum::Router;
use fluree_db_api::{Bm25CreateConfig, Bm25MaintenanceWorker};
use fluree_db_server::{AppState, ServerConfig, TelemetryConfig};
use serde_json::{json, Value};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

fn index_query() -> Value {
    json!({
        "@context": {"ex": "http://example.org/"},
        "where": [{"@id": "?x", "@type": "ex:Doc", "ex:title": "?t"}],
        "select": {"?x": ["@id", "ex:title"]}
    })
}

async fn test_state() -> (TempDir, Arc<AppState>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = ServerConfig {
        cors_enabled: false,
        indexing_enabled: false,
        bm25_auto_sync: true,
        storage_path: Some(tmp.path().to_path_buf()),
        ..Default::default()
    };
    let telemetry = TelemetryConfig::with_server_config(&cfg);
    let state = Arc::new(AppState::new(cfg, telemetry).await.expect("AppState::new"));
    (tmp, state)
}

async fn seed(state: &Arc<AppState>, tracked: bool) -> String {
    state
        .fluree
        .create_ledger("docs:main")
        .await
        .expect("create ledger");
    let ledger = state.fluree.ledger("docs:main").await.expect("ledger");
    state
        .fluree
        .insert(
            ledger,
            &json!({
                "@context": {"ex": "http://example.org/"},
                "@graph": [{"@id": "ex:doc1", "@type": "ex:Doc", "ex:title": "Rust guide"}]
            }),
        )
        .await
        .expect("insert");

    state
        .fluree
        .create_full_text_index(
            Bm25CreateConfig::new("docsearch", "docs:main", index_query()).with_tracked(tracked),
        )
        .await
        .expect("create index")
        .graph_source_id
}

/// Publish a worker into the same slot `FlureeServer::run` fills, so the
/// routes see one. Returns its handle.
fn install_worker(state: &Arc<AppState>) -> fluree_db_api::Bm25WorkerHandle {
    let worker = Bm25MaintenanceWorker::new(Arc::clone(&state.fluree));
    let handle = worker.handle();
    *state.bm25_worker_slot().lock() = Some(handle.clone());
    handle
}

async fn json_body(resp: http::Response<Body>) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn post(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    json_body(app.clone().oneshot(req).await.unwrap()).await
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
    json_body(app.clone().oneshot(req).await.unwrap()).await
}

/// The round trip: untrack persists the flag *and* drops the registration on
/// the answering node; track puts both back.
#[tokio::test]
async fn track_and_untrack_round_trip() {
    let (_tmp, state) = test_state().await;
    let gs_id = seed(&state, true).await;
    let handle = install_worker(&state);
    let record = state
        .fluree
        .nameservice()
        .lookup_graph_source(&gs_id)
        .await
        .unwrap()
        .unwrap();
    handle.register_graph_source_with_deps(&record.graph_source_id, &record.dependencies);
    let app = fluree_db_server::routes::build_router(Arc::clone(&state));

    let (status, body) = post(&app, "/v1/fluree/bm25/untrack", &json!({"index": gs_id})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tracked"], json!(false));
    assert_eq!(body["was_tracked"], json!(true));
    assert_eq!(body["registered"], json!(false));
    assert!(
        !handle.registered_graph_sources().contains(&gs_id),
        "untrack must drop the registration on this node, not just persist"
    );
    assert!(
        !fluree_db_api::bm25_tracked(
            &state
                .fluree
                .nameservice()
                .lookup_graph_source(&gs_id)
                .await
                .unwrap()
                .unwrap()
        ),
        "untrack must be durable, or a restart re-adopts it"
    );

    let (status, body) = post(&app, "/v1/fluree/bm25/track", &json!({"index": gs_id})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tracked"], json!(true));
    assert_eq!(body["was_tracked"], json!(false));
    assert_eq!(body["registered"], json!(true));
    assert!(handle.registered_graph_sources().contains(&gs_id));
}

/// The flag is the durable answer, so it must land even where no worker runs —
/// otherwise `--bm25-auto-sync` on a peer, or an untracked-by-default node,
/// could never be configured at all.
#[tokio::test]
async fn tracking_persists_with_no_worker_on_this_node() {
    let (_tmp, state) = test_state().await;
    let gs_id = seed(&state, true).await;
    let app = fluree_db_server::routes::build_router(Arc::clone(&state));

    let (status, body) = post(&app, "/v1/fluree/bm25/untrack", &json!({"index": gs_id})).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["registered"],
        json!(false),
        "no worker here, so nothing was registered"
    );
    assert!(!fluree_db_api::bm25_tracked(
        &state
            .fluree
            .nameservice()
            .lookup_graph_source(&gs_id)
            .await
            .unwrap()
            .unwrap()
    ));
}

/// A typo must fail rather than half-apply.
#[tokio::test]
async fn track_rejects_an_unknown_index() {
    let (_tmp, state) = test_state().await;
    seed(&state, true).await;
    let app = fluree_db_server::routes::build_router(Arc::clone(&state));

    let (status, _) = post(
        &app,
        "/v1/fluree/bm25/track",
        &json!({"index": "nope:main"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = post(&app, "/v1/fluree/bm25/track", &json!({"index": "  "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// `running: false` is a real answer, not an error: a durable `tracked: true`
/// still syncs nothing if no node runs a worker, and that is exactly what an
/// operator needs to be able to see.
#[tokio::test]
async fn tracking_reports_no_worker_without_failing() {
    let (_tmp, state) = test_state().await;
    seed(&state, true).await;
    let app = fluree_db_server::routes::build_router(Arc::clone(&state));

    let (status, body) = get(&app, "/v1/fluree/bm25/tracking").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["running"], json!(false));
    assert_eq!(body["indexes"], json!([]));
}

/// With a worker, the report names what that worker has actually adopted —
/// the one question no nameservice read can answer.
#[tokio::test]
async fn tracking_reports_what_this_node_adopted() {
    let (_tmp, state) = test_state().await;
    let gs_id = seed(&state, true).await;
    let handle = install_worker(&state);
    handle.register_graph_source_with_deps(&gs_id, &["docs:main".to_string()]);
    let app = fluree_db_server::routes::build_router(Arc::clone(&state));

    let (status, body) = get(&app, "/v1/fluree/bm25/tracking").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["running"], json!(true));
    assert_eq!(body["indexes"], json!([gs_id]));
    assert_eq!(body["watched_ledgers"], json!(["docs:main"]));
}
