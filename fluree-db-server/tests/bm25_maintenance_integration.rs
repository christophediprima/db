//! End-to-end coverage for BM25 index auto-maintenance.
//!
//! The property that matters: once the server knows about a BM25 index, a
//! commit to its source ledger makes the index catch up **on its own** — no
//! `fluree bm25 sync`, no HTTP call. That is what `Bm25MaintenanceWorker`
//! (spawned by `AppState::with_fluree`) buys, and it is only testable through
//! a real write: the worker is driven by the in-process event bus, so the
//! commit has to come from this same `Fluree` instance.
//!
//! Also covered: the two ways an index becomes known (an in-process create
//! auto-registers off the config event; a restart re-adopts everything
//! persisted) and the `/bm25/track|untrack|tracking` routes around them.

use axum::body::Body;
use axum::Router;
use fluree_db_api::Bm25CreateConfig;
use fluree_db_server::routes::build_router;
use fluree_db_server::{AppState, ServerConfig, TelemetryConfig};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

const LEDGER: &str = "bm25docs:main";
const INDEX_NAME: &str = "bm25docs-search";
const INDEX: &str = "bm25docs-search:main";

/// Worst-case wait for the worker to observe a commit and finish a sync. Well
/// past the 100ms debounce plus a sync of a three-document index; a failure
/// here means the worker isn't running, not that it's slow.
const SETTLE: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);

async fn state_in(tmp: &TempDir) -> Arc<AppState> {
    let cfg = ServerConfig {
        cors_enabled: false,
        indexing_enabled: false,
        storage_path: Some(tmp.path().to_path_buf()),
        ..Default::default()
    };
    let telemetry = TelemetryConfig::with_server_config(&cfg);
    Arc::new(AppState::new(cfg, telemetry).await.expect("AppState::new"))
}

async fn json_body(resp: http::Response<Body>) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).expect("valid JSON response");
    (status, json)
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    json_body(resp).await
}

async fn post(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    json_body(resp).await
}

async fn insert(app: &Router, body: &Value) -> i64 {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/fluree/insert")
                .header("content-type", "application/json")
                .header("fluree-ledger", LEDGER)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, json) = json_body(resp).await;
    assert_eq!(status, StatusCode::OK, "insert failed: {json}");
    json.get("t").and_then(Value::as_i64).expect("commit t")
}

/// Seed `LEDGER` with three documents and return a router over `state`.
async fn seed_ledger(state: &Arc<AppState>) -> Router {
    let app = build_router(state.clone());

    let (status, json) = post(&app, "/v1/fluree/create", &json!({ "ledger": LEDGER })).await;
    assert_eq!(status, StatusCode::CREATED, "create failed: {json}");

    insert(
        &app,
        &json!({
            "@context": { "ex": "http://example.org/" },
            "@graph": [
                { "@id": "ex:doc1", "@type": "ex:Doc", "ex:title": "Hello world" },
                { "@id": "ex:doc2", "@type": "ex:Doc", "ex:title": "Hello rust" },
                { "@id": "ex:doc3", "@type": "ex:Doc", "ex:title": "Systems programming" }
            ]
        }),
    )
    .await;

    app
}

/// The indexing query is persisted on the graph-source record, so the worker
/// re-runs it on sync without anyone passing it again.
fn indexing_query() -> Value {
    json!({
        "@context": { "ex": "http://example.org/" },
        "where": [{ "@id": "?x", "@type": "ex:Doc", "ex:title": "?title" }],
        "select": { "?x": ["@id", "ex:title"] }
    })
}

async fn create_index(state: &Arc<AppState>) {
    create_named_index(state, INDEX_NAME, true).await;
}

/// Create a BM25 index over `LEDGER`, tracked or not — `tracked` is the
/// persisted flag behind `fluree bm25 create --no-track`.
async fn create_named_index(state: &Arc<AppState>, name: &str, tracked: bool) {
    state
        .fluree
        .create_full_text_index(
            Bm25CreateConfig::new(name, LEDGER, indexing_query()).with_tracked(tracked),
        )
        .await
        .expect("create_full_text_index");
}

/// The entry for `index` in a `GET /bm25/tracking` body, if the worker has it.
fn entry_for<'a>(body: &'a Value, index: &str) -> Option<&'a Value> {
    body.get("indexes")?
        .as_array()?
        .iter()
        .find(|e| e.get("index").and_then(Value::as_str) == Some(index))
}

/// The entry for `INDEX` in a `GET /bm25/tracking` body, if the worker has it.
fn tracked_entry(body: &Value) -> Option<&Value> {
    entry_for(body, INDEX)
}

fn entry_i64(body: &Value, field: &str) -> Option<i64> {
    tracked_entry(body)?.get(field)?.as_i64()
}

fn entry_bool(body: &Value, field: &str) -> Option<bool> {
    tracked_entry(body)?.get(field)?.as_bool()
}

fn stat(body: &Value, field: &str) -> Option<u64> {
    body.get("stats")?.get(field)?.as_u64()
}

/// Poll `GET /bm25/tracking` until `pred` holds, or fail with the last body.
async fn poll_tracking(app: &Router, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + SETTLE;
    loop {
        let (status, body) = get(app, "/v1/fluree/bm25/tracking").await;
        assert_eq!(status, StatusCode::OK, "tracking status: {body}");
        if pred(&body) {
            return body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; last /bm25/tracking body: {body}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// The headline property: a commit to the source ledger makes the index catch
/// up by itself. Also pins the auto-register path — the index is created
/// in-process *after* the worker starts, so only the `GraphSourceConfigPublished`
/// event can have told it.
#[tokio::test]
async fn commit_to_source_ledger_auto_syncs_the_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = state_in(&tmp).await;
    let app = seed_ledger(&state).await;

    create_index(&state).await;

    let body = poll_tracking(&app, "the new index to be auto-registered", |b| {
        tracked_entry(b).is_some()
    })
    .await;
    assert_eq!(
        body.get("running").and_then(Value::as_bool),
        Some(true),
        "worker should be running on a non-peer node: {body}"
    );
    let indexed_t = entry_i64(&body, "index_t").expect("index_t");

    // A new commit on the source ledger — through the ordinary write route.
    let commit_t = insert(
        &app,
        &json!({
            "@context": { "ex": "http://example.org/" },
            "@id": "ex:doc4",
            "@type": "ex:Doc",
            "ex:title": "Rust programming language"
        }),
    )
    .await;
    assert!(
        commit_t > indexed_t,
        "the insert should leave the index behind ({commit_t} vs {indexed_t})"
    );

    // Nobody syncs it. The worker does.
    let body = poll_tracking(&app, "the index to catch up on its own", |b| {
        entry_bool(b, "is_stale") == Some(false) && entry_i64(b, "index_t") >= Some(commit_t)
    })
    .await;

    assert_eq!(entry_i64(&body, "lag"), Some(0), "{body}");
    assert_eq!(
        tracked_entry(&body)
            .and_then(|e| e.get("source_ledger"))
            .and_then(Value::as_str),
        Some(LEDGER),
        "{body}"
    );
    assert!(
        stat(&body, "syncs_performed").unwrap_or(0) >= 1,
        "a sync should be counted: {body}"
    );
    assert_eq!(stat(&body, "syncs_failed"), Some(0), "{body}");
}

/// Restart recovery / out-of-process discovery: an index that already exists in
/// the nameservice is adopted by a freshly-started worker, with no `track` call.
/// This is the `fluree bm25 create`-under-`docker exec` case — that process
/// publishes no event the server can hear, so start-up enumeration is the only
/// thing that finds it.
#[tokio::test]
async fn a_restarted_worker_adopts_persisted_indexes() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // First instance: seed the ledger and the index, then go away.
    let first = state_in(&tmp).await;
    let _first_app = seed_ledger(&first).await;
    create_index(&first).await;
    drop(first);

    // Second instance over the same storage — nothing registers the index.
    let state = state_in(&tmp).await;
    let app = build_router(state.clone());

    let body = poll_tracking(&app, "start-up enumeration to adopt the index", |b| {
        tracked_entry(b).is_some()
    })
    .await;
    assert_eq!(
        entry_bool(&body, "is_stale"),
        Some(false),
        "the adopted index was already current: {body}"
    );
    assert!(
        body.get("watched_ledgers")
            .and_then(Value::as_array)
            .is_some_and(|l| l.iter().any(|v| v.as_str() == Some(LEDGER))),
        "the source ledger should be watched: {body}"
    );
}

/// `track` is idempotent and re-syncs; `untrack` removes the registration.
#[tokio::test]
async fn track_and_untrack_round_trip() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = state_in(&tmp).await;
    let app = seed_ledger(&state).await;
    create_index(&state).await;

    // Let the create's config event be consumed first, so the auto-register
    // can't land after the `untrack` below and re-add the index.
    poll_tracking(&app, "the new index to be auto-registered", |b| {
        tracked_entry(b).is_some()
    })
    .await;

    let (status, body) = post(&app, "/v1/fluree/bm25/track", &json!({ "index": INDEX })).await;
    assert_eq!(status, StatusCode::OK, "track failed: {body}");
    assert_eq!(body.get("tracked").and_then(Value::as_bool), Some(true));
    // The index was created at head, so the immediate sync is a no-op.
    let initial = body.get("initial").expect("initial sync result");
    assert_eq!(
        initial.get("old_watermark"),
        initial.get("new_watermark"),
        "tracking a current index should not move the watermark: {body}"
    );

    let (status, body) = post(&app, "/v1/fluree/bm25/untrack", &json!({ "index": INDEX })).await;
    assert_eq!(status, StatusCode::OK, "untrack failed: {body}");
    assert_eq!(body.get("removed").and_then(Value::as_bool), Some(true));
    assert_eq!(
        body.get("tracked_indexes").and_then(Value::as_u64),
        Some(0),
        "{body}"
    );

    // Untracking something already untracked is a no-op, not an error.
    let (status, body) = post(&app, "/v1/fluree/bm25/untrack", &json!({ "index": INDEX })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("removed").and_then(Value::as_bool), Some(false));
}

/// `fluree bm25 create --no-track` must actually opt out: a worker that adopts
/// everything else in the same enumeration pass leaves this one alone.
#[tokio::test]
async fn an_untracked_index_is_never_adopted() {
    const UNTRACKED: &str = "bm25docs-untracked:main";

    let tmp = tempfile::tempdir().expect("tempdir");

    let first = state_in(&tmp).await;
    let _first_app = seed_ledger(&first).await;
    create_named_index(&first, "bm25docs-untracked", false).await;
    create_named_index(&first, INDEX_NAME, true).await;
    drop(first);

    let state = state_in(&tmp).await;
    let app = build_router(state.clone());

    // The tracked index appearing proves the enumeration ran and saw both
    // records — so the untracked one was considered and skipped, not just late.
    let body = poll_tracking(
        &app,
        "start-up enumeration to adopt the tracked index",
        |b| tracked_entry(b).is_some(),
    )
    .await;
    assert!(
        entry_for(&body, UNTRACKED).is_none(),
        "an index created with tracked=false must not be maintained: {body}"
    );

    // And a commit doesn't drag it in either.
    insert(
        &app,
        &json!({
            "@context": { "ex": "http://example.org/" },
            "@id": "ex:doc9",
            "@type": "ex:Doc",
            "ex:title": "Another document"
        }),
    )
    .await;
    let body = poll_tracking(
        &app,
        "the tracked index to catch up after the commit",
        |b| entry_bool(b, "is_stale") == Some(false),
    )
    .await;
    assert!(
        entry_for(&body, UNTRACKED).is_none(),
        "a source-ledger commit must not adopt an untracked index: {body}"
    );
}

/// `untrack` is durable: it clears the persisted flag, so the next start-up
/// enumeration doesn't quietly re-adopt the index.
#[tokio::test]
async fn untrack_survives_a_restart() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let first = state_in(&tmp).await;
    let app = seed_ledger(&first).await;
    create_index(&first).await;
    poll_tracking(&app, "the new index to be auto-registered", |b| {
        tracked_entry(b).is_some()
    })
    .await;

    let (status, body) = post(&app, "/v1/fluree/bm25/untrack", &json!({ "index": INDEX })).await;
    assert_eq!(status, StatusCode::OK, "untrack failed: {body}");
    assert_eq!(body.get("was_tracked").and_then(Value::as_bool), Some(true));
    drop(app);
    drop(first);

    let state = state_in(&tmp).await;
    let app = build_router(state.clone());

    // Give the fresh worker its enumeration pass, then assert it stayed out.
    let (status, body) = get(&app, "/v1/fluree/bm25/tracking").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("running").and_then(Value::as_bool), Some(true));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let (_, body) = get(&app, "/v1/fluree/bm25/tracking").await;
    assert!(
        tracked_entry(&body).is_none(),
        "untrack must survive a restart: {body}"
    );

    // `track` puts it back, durably.
    let (status, body) = post(&app, "/v1/fluree/bm25/track", &json!({ "index": INDEX })).await;
    assert_eq!(status, StatusCode::OK, "re-track failed: {body}");
    assert_eq!(
        body.get("was_tracked").and_then(Value::as_bool),
        Some(false),
        "{body}"
    );
    assert!(tracked_entry(&get(&app, "/v1/fluree/bm25/tracking").await.1).is_some());
}

/// A typo must fail at `track` rather than silently registering something that
/// can never sync.
#[tokio::test]
async fn track_rejects_unknown_and_non_bm25_ids() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let state = state_in(&tmp).await;
    let app = seed_ledger(&state).await;

    let (status, _) = post(
        &app,
        "/v1/fluree/bm25/track",
        &json!({ "index": "no-such-index:main" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A plain ledger is not a BM25 graph source.
    let (status, _) = post(&app, "/v1/fluree/bm25/track", &json!({ "index": LEDGER })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = post(&app, "/v1/fluree/bm25/track", &json!({ "index": "" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = post(&app, "/v1/fluree/bm25/track", &json!({ "nope": 1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
