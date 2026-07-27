//! BM25 full-text index maintenance endpoints:
//! `POST /v1/fluree/bm25/track`, `POST /v1/fluree/bm25/untrack`,
//! `GET /v1/fluree/bm25/tracking`.
//!
//! Index *creation* (`fluree bm25 create`) stays a Rust-API/CLI operation.
//! These routes only govern **maintenance**: which indexes the server's
//! in-process [`Bm25MaintenanceWorker`](fluree_db_api::Bm25MaintenanceWorker)
//! re-syncs when their source ledgers commit.
//!
//! The worker is event-driven off this instance's nameservice event bus, which
//! only the process that writes a commit publishes on — so it runs on write
//! nodes only, and peer-mode requests are forwarded to the transaction server
//! like any other write.
//!
//! Tracking is not a durable object: the worker adopts every persisted,
//! non-retracted BM25 record at start-up, so `track` is really "adopt this one
//! now, and sync it now" — useful right after an out-of-process
//! `fluree bm25 create`, which publishes no event this node can hear.

use crate::config::ServerRole;
use crate::error::{Result, ServerError};
use crate::extract::FlureeHeaders;
use crate::state::AppState;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

use super::ledger::forward_write_request;

/// Request body for `POST /v1/fluree/bm25/track` and `/untrack`.
#[derive(Deserialize)]
pub struct Bm25TrackRequest {
    /// BM25 index id (`name` or `name:branch`).
    pub index: String,
}

/// Register a BM25 index with the maintenance worker and sync it immediately.
/// The worker then re-syncs it whenever a source ledger commits.
///
/// POST /v1/fluree/bm25/track
pub async fn bm25_track(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if state.config.server_role == ServerRole::Peer {
        return forward_write_request(&state, request).await;
    }
    bm25_track_local(state, request).await.into_response()
}

async fn bm25_track_local(state: Arc<AppState>, request: Request) -> Result<impl IntoResponse> {
    let req = parse_track_request(request).await?;
    let worker = worker(&state)?;

    // Persist the intent first, so it survives a restart and is visible to
    // other processes (`fluree bm25 list`). Rejects unknown / retracted /
    // non-BM25 ids, so a typo fails here rather than registering a source that
    // can never sync.
    let was_tracked = state
        .fluree
        .set_bm25_tracked(&req.index, true)
        .await
        .map_err(ServerError::Api)?;

    // Republishing the config also wakes this node's worker, but that is
    // asynchronous — register directly so the response is the truth.
    worker
        .register_graph_source(&req.index)
        .await
        .map_err(ServerError::Api)?;

    // Immediate first sync so an index created out of process catches up
    // without waiting for the next commit. A no-op when already at head.
    let result = worker
        .sync_now(&req.index)
        .await
        .map_err(ServerError::Api)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "index": req.index,
            "tracked": true,
            "was_tracked": was_tracked,
            "tracked_indexes": worker.registered_graph_sources().len(),
            "initial": {
                "graph_source_id": result.graph_source_id,
                "upserted": result.upserted,
                "removed": result.removed,
                "affected_subjects": result.affected_subjects,
                "old_watermark": result.old_watermark,
                "new_watermark": result.new_watermark,
                "was_full_resync": result.was_full_resync,
            },
        })),
    ))
}

/// Stop maintaining a BM25 index (the index itself is left in place).
///
/// POST /v1/fluree/bm25/untrack
pub async fn bm25_untrack(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if state.config.server_role == ServerRole::Peer {
        return forward_write_request(&state, request).await;
    }
    bm25_untrack_local(state, request).await.into_response()
}

async fn bm25_untrack_local(state: Arc<AppState>, request: Request) -> Result<impl IntoResponse> {
    let req = parse_track_request(request).await?;
    let worker = worker(&state)?;

    // Durable: clearing the flag on the record means a restart (and the
    // start-up enumeration) won't quietly re-adopt the index.
    let was_tracked = state
        .fluree
        .set_bm25_tracked(&req.index, false)
        .await
        .map_err(ServerError::Api)?;
    let removed = worker.unregister_graph_source(&req.index);

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "index": req.index,
            "removed": removed,
            "was_tracked": was_tracked,
            "tracked_indexes": worker.registered_graph_sources().len(),
        })),
    ))
}

/// Maintenance-worker status: tracked indexes with their live staleness, plus
/// cumulative worker stats. Staleness is a nameservice read per index (no index
/// bytes are loaded).
///
/// GET /v1/fluree/bm25/tracking
pub async fn bm25_tracking_status(State(state): State<Arc<AppState>>) -> Response {
    // The worker is in-process, so the answering server's pid *is* the worker's
    // — the one thing a nameservice-only reader (`fluree bm25 list`) can't know.
    let pid = std::process::id();

    let Some(worker) = state.bm25_worker.as_ref() else {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "running": false, "pid": pid, "indexes": [] })),
        )
            .into_response();
    };

    let mut ids = worker.registered_graph_sources();
    ids.sort();

    let mut indexes = Vec::with_capacity(ids.len());
    for id in ids {
        match state.fluree.check_bm25_staleness(&id).await {
            Ok(check) => indexes.push(serde_json::json!({
                "index": id,
                "source_ledger": check.source_ledger,
                "index_t": check.index_t,
                "ledger_t": check.ledger_t,
                "is_stale": check.is_stale,
                "lag": check.lag,
            })),
            // A record that vanished (dropped between listing and checking)
            // shouldn't fail the whole status read.
            Err(e) => indexes.push(serde_json::json!({
                "index": id,
                "error": e.to_string(),
            })),
        }
    }

    let stats = worker.stats();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "running": true,
            "pid": pid,
            "watched_ledgers": worker.watched_ledgers(),
            "indexes": indexes,
            "stats": {
                "syncs_performed": stats.syncs_performed,
                "syncs_failed": stats.syncs_failed,
                "events_received": stats.events_received,
                "registered_graph_sources": stats.registered_graph_sources,
            }
        })),
    )
        .into_response()
}

/// The node's maintenance worker, or a 400 explaining why there isn't one.
fn worker(state: &AppState) -> Result<&fluree_db_api::Bm25WorkerHandle> {
    state.bm25_worker.as_ref().ok_or_else(|| {
        ServerError::bad_request("BM25 maintenance worker is not running on this node")
    })
}

async fn parse_track_request(request: Request) -> Result<Bm25TrackRequest> {
    let _headers = FlureeHeaders::from_headers(request.headers())?;
    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 1024)
        .await
        .map_err(|e| ServerError::bad_request(format!("Failed to read body: {e}")))?;
    let req: Bm25TrackRequest = serde_json::from_slice(&body_bytes)
        .map_err(|e| ServerError::bad_request(format!("Invalid JSON: {e}")))?;
    if req.index.trim().is_empty() {
        return Err(ServerError::bad_request("index is required"));
    }
    Ok(req)
}
