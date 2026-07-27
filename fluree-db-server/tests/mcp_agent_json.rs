//! Integration tests for the MCP `sparql_query` tool's Agent JSON output.
//!
//! These exercise `FlureeToolService::execute_sparql_agent_json` directly — the testable
//! core of the `sparql_query` tool, which avoids needing an rmcp `RequestContext` — while
//! seeding data through the regular HTTP routes.

use axum::body::Body;
use fluree_db_server::mcp::tools::FlureeToolService;
use fluree_db_server::{routes::build_router, AppState, ServerConfig, TelemetryConfig};
use http::{Request, StatusCode};
use serde_json::Value as JsonValue;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

async fn test_state() -> (TempDir, Arc<AppState>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = ServerConfig {
        cors_enabled: false,
        indexing_enabled: false,
        storage_path: Some(tmp.path().to_path_buf()),
        ..Default::default()
    };
    let telemetry = TelemetryConfig::with_server_config(&cfg);
    let state = Arc::new(AppState::new(cfg, telemetry).await.expect("AppState::new"));
    (tmp, state)
}

async fn create_ledger(state: &Arc<AppState>, ledger: &str) {
    let app = build_router(state.clone());
    let body = serde_json::json!({ "ledger": ledger });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/fluree/create")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create ledger {ledger}");
}

async fn insert(state: &Arc<AppState>, ledger: &str, body: JsonValue) {
    let app = build_router(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/fluree/insert")
                .header("content-type", "application/json")
                .header("fluree-ledger", ledger)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "insert into {ledger}");
}

/// `ex:item{i} ex:name "name-{i}"` for `i in start..start+n`.
fn rows_graph_range(start: usize, n: usize) -> JsonValue {
    let graph: Vec<JsonValue> = (start..start + n)
        .map(
            |i| serde_json::json!({ "@id": format!("ex:item{i}"), "ex:name": format!("name-{i}") }),
        )
        .collect();
    serde_json::json!({
        "@context": { "ex": "http://example.org/" },
        "@graph": graph,
    })
}

fn rows_graph(n: usize) -> JsonValue {
    rows_graph_range(0, n)
}

const QUERY: &str = r"PREFIX ex: <http://example.org/>
SELECT ?s ?name WHERE { ?s ex:name ?name }";

#[tokio::test]
async fn agent_json_envelope_shape() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:shape").await;
    insert(&state, "test:shape", rows_graph(3)).await;

    let svc = FlureeToolService::new(state.clone());
    let env = svc
        .execute_sparql_agent_json("test:shape", QUERY, None, None, 32_768)
        .await
        .expect("query ok");

    assert!(
        env.get("schema").map(JsonValue::is_object).unwrap_or(false),
        "schema should be an object: {env}"
    );
    let rows = env
        .get("rows")
        .and_then(JsonValue::as_array)
        .expect("rows array");
    let row_count = env
        .get("rowCount")
        .and_then(JsonValue::as_u64)
        .expect("rowCount");
    assert_eq!(
        row_count as usize,
        rows.len(),
        "rowCount matches rows length"
    );
    assert_eq!(row_count, 3, "all three rows fit under the budget");
    assert_eq!(env.get("hasMore"), Some(&JsonValue::Bool(false)));
    assert!(
        env.get("t").and_then(JsonValue::as_i64).is_some(),
        "t (snapshot marker) present"
    );
    // The ledger-scoped MCP path never emits the FROM-rewritten resume query.
    assert!(env.get("resume").is_none(), "no resume key for MCP");
    // `iso` is intentionally dropped: it would be wall-clock query time, not the snapshot's
    // timestamp, and pagination keys on `t` — so a misleading field is simply omitted.
    assert!(env.get("iso").is_none(), "no iso key for MCP");
}

#[tokio::test]
async fn agent_json_byte_budget_truncates() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:trunc").await;
    insert(&state, "test:trunc", rows_graph(20)).await;

    let svc = FlureeToolService::new(state.clone());
    // Tiny budget forces truncation; the formatter always keeps at least one row.
    let env = svc
        .execute_sparql_agent_json("test:trunc", QUERY, None, None, 64)
        .await
        .expect("query ok");

    assert_eq!(
        env.get("hasMore"),
        Some(&JsonValue::Bool(true)),
        "byte budget should truncate: {env}"
    );
    let row_count = env
        .get("rowCount")
        .and_then(JsonValue::as_u64)
        .expect("rowCount") as usize;
    assert!(
        (1..20).contains(&row_count),
        "expected a partial page of rows, got {row_count}"
    );
}

#[tokio::test]
async fn agent_json_t_pinning_is_deterministic() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:pin").await;
    insert(&state, "test:pin", rows_graph_range(0, 3)).await;

    let svc = FlureeToolService::new(state.clone());

    // First call at latest: capture the snapshot `t` and the baseline row count.
    let env1 = svc
        .execute_sparql_agent_json("test:pin", QUERY, None, None, 32_768)
        .await
        .expect("latest query ok");
    let t1 = env1
        .get("t")
        .and_then(JsonValue::as_i64)
        .expect("t present");
    assert_eq!(env1.get("rowCount").and_then(JsonValue::as_u64), Some(3));

    // Advance the ledger with three more distinct rows.
    insert(&state, "test:pin", rows_graph_range(3, 3)).await;

    // Pinned to t1: still the original snapshot (3 rows), and `t` is echoed.
    let env_pinned = svc
        .execute_sparql_agent_json("test:pin", QUERY, None, Some(t1), 32_768)
        .await
        .expect("pinned query ok");
    assert_eq!(
        env_pinned.get("rowCount").and_then(JsonValue::as_u64),
        Some(3),
        "pinned snapshot is unchanged by later writes"
    );
    assert_eq!(
        env_pinned.get("t").and_then(JsonValue::as_i64),
        Some(t1),
        "pinned result echoes the requested t"
    );

    // Latest now sees all six rows.
    let env_latest = svc
        .execute_sparql_agent_json("test:pin", QUERY, None, None, 32_768)
        .await
        .expect("latest query ok");
    assert_eq!(
        env_latest.get("rowCount").and_then(JsonValue::as_u64),
        Some(6),
        "latest snapshot reflects the new rows"
    );
}

#[tokio::test]
async fn agent_json_identity_branch_shape() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:ident").await;
    insert(&state, "test:ident", rows_graph(2)).await;

    let svc = FlureeToolService::new(state.clone());
    // Exercise the identity/policy branch. The ledger defines no policy, so the exact row
    // count depends on default policy semantics — assert only that the envelope shape holds.
    let env = svc
        .execute_sparql_agent_json(
            "test:ident",
            QUERY,
            Some("did:key:z6MkExample"),
            None,
            32_768,
        )
        .await
        .expect("policy query ok");

    assert!(env.get("schema").map(JsonValue::is_object).unwrap_or(false));
    assert!(env.get("rows").map(JsonValue::is_array).unwrap_or(false));
    assert!(env.get("rowCount").and_then(JsonValue::as_u64).is_some());
    assert!(env
        .get("hasMore")
        .map(JsonValue::is_boolean)
        .unwrap_or(false));
}

#[tokio::test]
async fn agent_json_truncation_message_explains_pagination() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:msg").await;
    insert(&state, "test:msg", rows_graph(20)).await;

    let svc = FlureeToolService::new(state.clone());
    // Force truncation so the pagination guidance is appended to `message`.
    let env = svc
        .execute_sparql_agent_json("test:msg", QUERY, None, None, 64)
        .await
        .expect("query ok");

    assert_eq!(env.get("hasMore"), Some(&JsonValue::Bool(true)));
    let t = env.get("t").and_then(JsonValue::as_i64).expect("t present");
    let row_count = env
        .get("rowCount")
        .and_then(JsonValue::as_u64)
        .expect("rowCount present");
    let msg = env
        .get("message")
        .and_then(JsonValue::as_str)
        .expect("message present");
    assert!(
        msg.contains(&format!("t={t}")),
        "message should reference the snapshot t: {msg}"
    );
    assert!(
        msg.contains("ORDER BY"),
        "message should recommend ORDER BY: {msg}"
    );
    // Cumulative pagination: advance OFFSET by the returned rowCount, not by LIMIT.
    assert!(
        msg.contains("current OFFSET"),
        "message should tell the agent to advance its current OFFSET: {msg}"
    );
    assert!(
        msg.contains(&format!("{row_count} rows returned here")),
        "message should reference the returned rowCount as the OFFSET advance: {msg}"
    );
}

#[tokio::test]
async fn agent_json_rejects_non_select() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:kind").await;
    insert(&state, "test:kind", rows_graph(2)).await;

    let svc = FlureeToolService::new(state.clone());
    // The envelope (and byte budget) only make sense for a SELECT solution table; ASK and
    // CONSTRUCT/DESCRIBE must be rejected rather than return a contradictory shape.
    for q in [
        "PREFIX ex: <http://example.org/> ASK { ?s ex:name ?o }",
        "PREFIX ex: <http://example.org/> CONSTRUCT { ?s ex:name ?o } WHERE { ?s ex:name ?o }",
    ] {
        let err = svc
            .execute_sparql_agent_json("test:kind", q, None, None, 32_768)
            .await
            .expect_err("non-SELECT should be rejected");
        assert!(
            err.to_string().contains("SELECT"),
            "error should explain SELECT-only: {err}"
        );
    }
}

/// A SELECT-style FQL query over the same `ex:name` data returns the same Agent JSON
/// envelope shape as `sparql_query` — this is the path the MCP `fql_query` tool drives,
/// and (via `run_jsonld_subquery`) the same connection path the HTTP `/v1/fluree/query`
/// route uses, so a BM25 `f:searchText` block would resolve here too.
#[tokio::test]
async fn fql_agent_json_envelope_shape() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:fql").await;
    insert(&state, "test:fql", rows_graph(3)).await;

    // `fql_query` requires an identity (fail-closed), and an identity with no policy
    // assignments is default-DENY (sees 0 rows). `default-allow: true` opts into the
    // unrestricted default so this end-to-end test asserts real data flow; it also exercises
    // `force_identity_opts` merging `identity` into a pre-existing `opts` object.
    let query = serde_json::json!({
        "@context": { "ex": "http://example.org/" },
        "from": "test:fql",
        "where": [{ "@id": "?s", "ex:name": "?name" }],
        "select": ["?s", "?name"],
        "opts": { "default-allow": true },
    });

    let svc = FlureeToolService::new(state.clone());
    let env = svc
        .execute_fql_agent_json(&query, Some("did:key:z6MkExample"), 32_768)
        .await
        .expect("fql query ok");

    assert!(
        env.get("schema").map(JsonValue::is_object).unwrap_or(false),
        "schema should be an object: {env}"
    );
    let rows = env
        .get("rows")
        .and_then(JsonValue::as_array)
        .expect("rows array");
    let row_count = env
        .get("rowCount")
        .and_then(JsonValue::as_u64)
        .expect("rowCount");
    assert_eq!(
        row_count as usize,
        rows.len(),
        "rowCount matches rows length"
    );
    assert_eq!(row_count, 3, "all three rows returned");
    assert_eq!(env.get("hasMore"), Some(&JsonValue::Bool(false)));
}

/// `fql_query` fails closed when the token resolves no identity. Unlike `sparql_query`, an
/// FQL body can carry its own `opts.identity`, so running identity-less would let a caller
/// read under any identity it names — the tool refuses instead.
#[tokio::test]
async fn fql_requires_authenticated_identity() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:fqlident").await;
    insert(&state, "test:fqlident", rows_graph(2)).await;

    // A well-formed SELECT query (passes the select/from guards) that also tries to smuggle in
    // its own identity — it must still be rejected because the caller presented no identity.
    let query = serde_json::json!({
        "@context": { "ex": "http://example.org/" },
        "from": "test:fqlident",
        "where": [{ "@id": "?s", "ex:name": "?name" }],
        "select": ["?s", "?name"],
        "opts": { "identity": "did:key:z6MkSomeoneElse" },
    });

    let svc = FlureeToolService::new(state.clone());
    let err = svc
        .execute_fql_agent_json(&query, None, 32_768)
        .await
        .expect_err("identity-less fql_query should be rejected");
    assert!(
        err.to_string().contains("identity"),
        "error should explain the missing identity: {err}"
    );
}

/// `fql_query` requires a SELECT-style clause and a `from` — node/graph FQL and a
/// `from`-less body are rejected up front with a clear, learnable error.
#[tokio::test]
async fn fql_rejects_non_select_and_missing_from() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:fqlkind").await;
    insert(&state, "test:fqlkind", rows_graph(2)).await;

    let svc = FlureeToolService::new(state.clone());

    // Node/graph FQL (no select clause) has no solution-table shape.
    let no_select = serde_json::json!({
        "@context": { "ex": "http://example.org/" },
        "from": "test:fqlkind",
        "where": [{ "@id": "?s", "ex:name": "?name" }],
    });
    let err = svc
        .execute_fql_agent_json(&no_select, None, 32_768)
        .await
        .expect_err("non-SELECT FQL should be rejected");
    assert!(
        err.to_string().contains("SELECT"),
        "error should explain SELECT-only: {err}"
    );

    // A body without `from` cannot resolve a ledger.
    let no_from = serde_json::json!({
        "@context": { "ex": "http://example.org/" },
        "where": [{ "@id": "?s", "ex:name": "?name" }],
        "select": ["?s", "?name"],
    });
    let err = svc
        .execute_fql_agent_json(&no_from, None, 32_768)
        .await
        .expect_err("from-less FQL should be rejected");
    assert!(
        err.to_string().contains("from"),
        "error should explain the missing from clause: {err}"
    );
}

/// `SELECT ?s ?name … ORDER BY ?s` with optional `LIMIT n OFFSET m`.
fn paging_query(limit_offset: Option<(usize, usize)>) -> String {
    let base = "PREFIX ex: <http://example.org/>\n\
                SELECT ?s ?name WHERE { ?s ex:name ?name } ORDER BY ?s";
    match limit_offset {
        Some((limit, offset)) => format!("{base} LIMIT {limit} OFFSET {offset}"),
        None => base.to_string(),
    }
}

fn s_values(env: &JsonValue) -> Vec<String> {
    env.get("rows")
        .and_then(JsonValue::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.get("?s").and_then(JsonValue::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn agent_json_pinned_paging_is_disjoint_and_complete() {
    let (_tmp, state) = test_state().await;
    create_ledger(&state, "test:page").await;
    insert(&state, "test:page", rows_graph(4)).await;

    let svc = FlureeToolService::new(state.clone());

    // Pin the snapshot, then page it with ORDER BY so the contract — disjoint, complete pages —
    // is exercised end to end (snapshot fixity alone doesn't prove stable scan order).
    let base = svc
        .execute_sparql_agent_json("test:page", &paging_query(None), None, None, 32_768)
        .await
        .expect("latest ok");
    let t = base
        .get("t")
        .and_then(JsonValue::as_i64)
        .expect("t present");
    assert_eq!(base.get("rowCount").and_then(JsonValue::as_u64), Some(4));

    let page1 = svc
        .execute_sparql_agent_json(
            "test:page",
            &paging_query(Some((2, 0))),
            None,
            Some(t),
            32_768,
        )
        .await
        .expect("page1 ok");
    let page2 = svc
        .execute_sparql_agent_json(
            "test:page",
            &paging_query(Some((2, 2))),
            None,
            Some(t),
            32_768,
        )
        .await
        .expect("page2 ok");

    let s1 = s_values(&page1);
    let s2 = s_values(&page2);
    assert_eq!(s1.len(), 2, "page 1 returns 2 rows: {page1}");
    assert_eq!(s2.len(), 2, "page 2 returns 2 rows: {page2}");

    let mut union: Vec<String> = s1.iter().chain(s2.iter()).cloned().collect();
    union.sort();
    union.dedup();
    assert_eq!(
        union.len(),
        4,
        "pages are disjoint and together cover all 4 rows: {s1:?} {s2:?}"
    );
}
