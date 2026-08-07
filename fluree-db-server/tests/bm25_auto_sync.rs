//! Auto-sync keeps a BM25 index current without anyone calling `sync`.
//!
//! These drive the worker directly against a server `AppState` rather than
//! booting a listener: `FlureeServer::run` owns the spawn, and starting a real
//! server would mean binding a port and racing shutdown. What matters here is
//! the behavior the flag buys — a commit advances the index watermark on its
//! own — plus the registration pass that `auto_register` alone does not cover.

use fluree_db_api::{Bm25CreateConfig, Bm25MaintenanceWorker, Bm25WorkerConfig};
use fluree_db_server::{indexes_to_auto_sync, AppState, ServerConfig, TelemetryConfig};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Indexes every `ex:Doc`'s `ex:title`.
fn index_query() -> serde_json::Value {
    json!({
        "@context": {"ex": "http://example.org/"},
        "where": [{"@id": "?x", "@type": "ex:Doc", "ex:title": "?t"}],
        "select": {"?x": ["@id", "ex:title"]}
    })
}

async fn test_state(bm25_auto_sync: bool) -> (TempDir, Arc<AppState>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = ServerConfig {
        cors_enabled: false,
        indexing_enabled: false,
        bm25_auto_sync,
        storage_path: Some(tmp.path().to_path_buf()),
        ..Default::default()
    };
    let telemetry = TelemetryConfig::with_server_config(&cfg);
    let state = Arc::new(AppState::new(cfg, telemetry).await.expect("AppState::new"));
    (tmp, state)
}

/// `docs:main` with one doc and a `docsearch:main` index built over it.
async fn seed(state: &Arc<AppState>) -> String {
    state
        .fluree
        .create_ledger("docs:main")
        .await
        .expect("create ledger");
    insert_doc(state, "ex:doc1", "Rust programming guide").await;

    let created = state
        .fluree
        .create_full_text_index(Bm25CreateConfig::new(
            "docsearch",
            "docs:main",
            index_query(),
        ))
        .await
        .expect("create index");
    created.graph_source_id
}

async fn insert_doc(state: &Arc<AppState>, id: &str, title: &str) {
    let ledger = state.fluree.ledger("docs:main").await.expect("load ledger");
    let doc = json!({
        "@context": {"ex": "http://example.org/"},
        "@id": id,
        "@type": "ex:Doc",
        "ex:title": title
    });
    state.fluree.insert(ledger, &doc).await.expect("insert doc");
}

async fn index_watermark(state: &Arc<AppState>, graph_source_id: &str) -> i64 {
    state
        .fluree
        .nameservice()
        .lookup_graph_source(graph_source_id)
        .await
        .expect("lookup")
        .expect("index should exist")
        .index_t
}

/// Poll until the watermark advances past `from`, or give up.
async fn await_watermark_past(state: &Arc<AppState>, graph_source_id: &str, from: i64) -> i64 {
    for _ in 0..100 {
        let t = index_watermark(state, graph_source_id).await;
        if t > from {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    index_watermark(state, graph_source_id).await
}

fn worker(state: &Arc<AppState>) -> Bm25MaintenanceWorker {
    Bm25MaintenanceWorker::with_config(
        Arc::clone(&state.fluree),
        Bm25WorkerConfig {
            debounce_ms: 10,
            ..Bm25WorkerConfig::default()
        },
    )
}

/// The whole point of the feature: commit to the source ledger and the index
/// catches up on its own, with no `sync` call.
#[tokio::test]
async fn commit_advances_the_index_without_an_explicit_sync() {
    let (_tmp, state) = test_state(true).await;
    let gs_id = seed(&state).await;
    let before = index_watermark(&state, &gs_id).await;

    let worker = worker(&state);
    let handle = worker.handle();
    // Registration pass: the index already existed, so `auto_register` alone
    // would never have picked it up.
    handle.register_graph_source_with_deps(&gs_id, &["docs:main".to_string()]);
    let task = tokio::spawn(async move { worker.run().await });

    insert_doc(&state, "ex:doc2", "Rust and WebAssembly").await;

    let after = await_watermark_past(&state, &gs_id, before).await;
    handle.stop();
    let _ = task.await;

    assert!(
        after > before,
        "auto-sync should advance the watermark ({before} -> {after})"
    );
    assert!(
        handle.stats().syncs_performed >= 1,
        "worker should record the sync: {:?}",
        handle.stats()
    );
}

/// Nothing runs when the flag is off — the index goes stale and stays stale.
#[tokio::test]
async fn index_stays_stale_when_auto_sync_is_off() {
    let (_tmp, state) = test_state(false).await;
    let gs_id = seed(&state).await;
    let before = index_watermark(&state, &gs_id).await;

    assert!(
        !state.config.bm25_auto_sync,
        "flag should default off in this fixture"
    );

    insert_doc(&state, "ex:doc2", "Rust and WebAssembly").await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        index_watermark(&state, &gs_id).await,
        before,
        "no worker is running, so the watermark must not move"
    );
}

/// Auto-sync must be off by default, so an upgrade does not silently start
/// writing in the background.
#[tokio::test]
async fn auto_sync_defaults_off() {
    assert!(!ServerConfig::default().bm25_auto_sync);
}

/// A retracted index must not be re-registered by the startup pass — syncing
/// one is refused, so registering it would just log failures every commit.
#[tokio::test]
async fn dropped_indexes_are_not_registered() {
    let (_tmp, state) = test_state(true).await;
    let gs_id = seed(&state).await;
    state
        .fluree
        .drop_full_text_index(&gs_id)
        .await
        .expect("drop index");

    let records = state
        .fluree
        .nameservice()
        .all_graph_source_records()
        .await
        .expect("list graph sources");
    let live = indexes_to_auto_sync(&records);

    assert!(
        live.is_empty(),
        "the dropped index should not be eligible for registration: {live:?}"
    );
}

/// An index that fell behind while the server was down must catch up when it
/// comes back, with nobody committing to wake it. Registration alone only
/// reacts to future commits, so on a quiet ledger this is the difference
/// between "current shortly after boot" and "stale indefinitely".
#[tokio::test]
async fn a_stale_index_catches_up_at_startup_without_a_commit() {
    let (_tmp, state) = test_state(true).await;
    let gs_id = seed(&state).await;

    // Commit past the index while nothing is maintaining it — the state a
    // restart finds after writes landed with the worker down.
    insert_doc(&state, "ex:doc2", "Rust ownership").await;
    let stale_at = index_watermark(&state, &gs_id).await;
    let ledger_t = state.fluree.ledger("docs:main").await.expect("ledger").t();
    assert!(
        stale_at < ledger_t,
        "precondition: the index should be behind ({stale_at} vs {ledger_t})"
    );

    // Boot the worker the way the server does, catch-up pass included.
    let (worker, _handle) = fluree_db_server::build_bm25_worker(Arc::clone(&state.fluree)).await;
    let task = tokio::spawn(async move { worker.run().await });

    let caught_up = await_watermark_past(&state, &gs_id, stale_at).await;

    assert!(
        caught_up >= ledger_t,
        "the index should have caught up to {ledger_t}, got {caught_up}"
    );
    task.abort();
}
