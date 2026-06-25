//! Regression: a registered Iceberg/R2RML graph source must be queryable by
//! alias over the HTTP query path (`POST /v1/fluree/query/<gs>`), instead of
//! failing with `Serialization error: missing field f:ledger` (the alias being
//! deserialized as a ledger `NsFileV2` record) or a bare "ledger not found".
//!
//! These exercise the broken path the shipped tests never covered: the server
//! query handlers (`execute_query` for JSON-LD, `execute_sparql_ledger` for
//! SPARQL) resolving a graph-source alias through the nameservice. The server
//! uses a file-backed nameservice, which is required to reproduce the bug — the
//! in-memory backend keeps graph sources in a separate map and never
//! deserializes the on-disk record.
#![cfg(feature = "iceberg")]

use axum::body::Body;
use fluree_db_api::R2rmlCreateConfig;
use fluree_db_server::{routes::build_router, AppState, ServerConfig, TelemetryConfig};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const MAPPING_TTL: &str = r#"
@prefix rr: <http://www.w3.org/ns/r2rml#> .
@prefix ex: <http://example.org/> .

<http://example.org/mapping#M> a rr:TriplesMap ;
    rr:logicalTable [ rr:tableName "openflights.airlines" ] ;
    rr:subjectMap [
        rr:template "http://example.org/airline/{id}" ;
        rr:class ex:Airline
    ] ;
    rr:predicateObjectMap [
        rr:predicate ex:name ;
        rr:objectMap [ rr:column "name" ]
    ] .
"#;

/// Build a file-backed server state with a single Iceberg/R2RML graph source
/// `gs:main` registered. The catalog URI is bogus, but the historical bug fired
/// during alias resolution, before any catalog call.
async fn state_with_graph_source() -> (TempDir, Arc<AppState>) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg = ServerConfig {
        cors_enabled: false,
        indexing_enabled: false,
        storage_path: Some(tmp.path().to_path_buf()),
        ..Default::default()
    };
    let telemetry = TelemetryConfig::with_server_config(&cfg);
    let state = Arc::new(AppState::new(cfg, telemetry).await.expect("AppState::new"));

    state
        .fluree
        .create_r2rml_graph_source(
            R2rmlCreateConfig::new(
                "gs",
                "https://example.invalid",
                "openflights.airlines",
                MAPPING_TTL,
            )
            .with_mapping_media_type("text/turtle"),
        )
        .await
        .expect("graph source registration should succeed");

    (tmp, state)
}

async fn body_text(resp: http::Response<Body>) -> (StatusCode, String) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// JSON-LD `POST /v1/fluree/query/gs:main` (the `execute_query` path).
#[tokio::test]
async fn jsonld_query_by_graph_source_alias_resolves() {
    let (_tmp, state) = state_with_graph_source().await;
    let app = build_router(state);

    let body = json!({
        "@context": {"ex": "http://example.org/"},
        "select": ["?s"],
        "where": [["?s", "a", "ex:Airline"]]
    });
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/fluree/query/gs:main")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, text) = body_text(resp).await;
    // Before the fix: 500 "Serialization error: missing field `f:ledger`".
    assert!(
        !text.contains("f:ledger"),
        "graph-source alias must not be deserialized as a ledger record; got {status}: {text}"
    );
    // It must resolve as a graph source, not report the ledger as missing.
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "graph-source alias should resolve, not 404; body: {text}"
    );
}

/// SPARQL `POST /v1/fluree/query/gs:main` — the exact shape from the bug report
/// (`execute_sparql_ledger` path). The query reaches the R2RML engine (which
/// rejects the fully-unbound pattern), proving alias resolution succeeded;
/// crucially it is no longer the `f:ledger` deserialization failure.
#[tokio::test]
async fn sparql_query_by_graph_source_alias_resolves() {
    let (_tmp, state) = state_with_graph_source().await;
    let app = build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/fluree/query/gs:main")
                .header("content-type", "application/sparql-query")
                .body(Body::from("SELECT ?s ?p ?o WHERE { ?s ?p ?o } LIMIT 1"))
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, text) = body_text(resp).await;
    assert!(
        !text.contains("f:ledger"),
        "graph-source alias must not be deserialized as a ledger record; got {status}: {text}"
    );
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "graph-source alias should resolve, not 404; body: {text}"
    );
}
