//! BM25 full-text search index commands.
//!
//! BM25 is a Fluree *graph source* (like Iceberg/R2RML). Index **creation** and
//! **sync** have no HTTP route or other shipped entrypoint today — they are
//! Rust-API-only operations (`create_full_text_index` / `sync_bm25_index`).
//! These commands expose that API so an index can be built and kept fresh
//! reproducibly, running **in-process** against local storage via
//! [`build_fluree`]. Native file storage coordinates writers with a per-file
//! advisory flock (not an exclusive whole-store lock), so `create`/`drop`/`sync`
//! work under `docker exec` against a directory a server is already serving:
//! each writes new content-addressed snapshots plus/against a graph-source
//! nameservice record (no key the server writes). `sync` is incremental
//! (watermark-based) and lets a maintenance job keep an index current as its
//! source ledger is materialized. Querying the resulting index is done
//! separately — through `fluree-search-httpd` (`POST /v1/search`, reading the
//! same storage), or embedded via an FQL `f:searchText` query.
//!
//! # Who keeps an index fresh
//!
//! A running `fluree server` has a BM25 maintenance worker that re-syncs
//! **tracked** indexes whenever their source ledger commits, so `sync` from a
//! cron is only needed for indexes that opt out (`create --no-track`, listed by
//! `list --untracked`). Tracking is persisted on the index record; `list` shows
//! it in the TRACKED column.
//!
//! That worker is driven by the server's *in-process* event bus, which this
//! one-shot CLI cannot publish on — so an index created here is invisible to a
//! running server until it restarts (its start-up enumeration adopts every
//! tracked index) or someone calls `POST /v1/fluree/bm25/track`.

use crate::cli::Bm25Action;
use crate::context::build_fluree;
use crate::error::{CliError, CliResult};
use crate::input;
use colored::Colorize;
use fluree_db_api::server_defaults::FlureeDir;
use fluree_db_api::Bm25CreateConfig;
use std::path::Path;

pub async fn run(action: Bm25Action, dirs: &FlureeDir) -> CliResult<()> {
    match action {
        Bm25Action::Create {
            name,
            ledger,
            branch,
            query,
            query_file,
            k1,
            b,
            no_track,
            track: _,
        } => {
            run_create(
                &name,
                &ledger,
                &branch,
                query.as_deref(),
                query_file.as_deref(),
                k1,
                b,
                !no_track,
                dirs,
            )
            .await
        }
        Bm25Action::Drop { index } => run_drop(&index, dirs).await,
        Bm25Action::Sync { index } => run_sync(&index, dirs).await,
        Bm25Action::List { stale, untracked } => run_list(stale, untracked, dirs).await,
    }
}

/// One row of `bm25 list`.
struct IndexRow {
    name: String,
    branch: String,
    source: String,
    index_t: i64,
    ledger_t: Option<i64>,
    stale: bool,
    tracked: bool,
}

/// List BM25 indexes with their source ledger, staleness and tracking — what a
/// maintenance job enumerates to decide which to `sync`. An index is STALE when
/// its source ledger's commit `t` has advanced past the index's watermark
/// (`index_t`), and TRACKED when a running server's maintenance worker is meant
/// to keep it fresh (persisted on the record; see `bm25 create --no-track`).
///
/// TRACKED is *intent*, not liveness: this CLI reads storage, not the server, so
/// it cannot tell whether a worker process is actually up. `GET
/// /v1/fluree/bm25/tracking` on the server answers that, and reports its pid.
async fn run_list(stale_only: bool, untracked_only: bool, dirs: &FlureeDir) -> CliResult<()> {
    use comfy_table::{ContentArrangement, Table};
    use fluree_db_api::bm25_tracked;
    use std::collections::HashMap;

    let fluree = build_fluree(dirs)?;
    let ledgers = fluree.nameservice().all_records().await?;
    let sources = fluree.nameservice().all_graph_source_records().await?;

    // Source-ledger alias -> current commit t (skip retracted).
    let commit_t: HashMap<String, i64> = ledgers
        .iter()
        .filter(|r| !r.retracted)
        .map(|r| (format!("{}:{}", r.name, r.branch), r.commit_t))
        .collect();

    let mut rows: Vec<IndexRow> = sources
        .iter()
        .filter(|r| r.is_bm25() && !r.retracted)
        .map(|gs| {
            let source = gs.dependencies.first().cloned().unwrap_or_default();
            // A stored dependency alias may omit the branch (a bare `name` means
            // `name:main` to Fluree), while ledger records are keyed `name:branch`
            // — so try the alias as-is, then with an implicit `:main`.
            let ledger_t = commit_t
                .get(&source)
                .or_else(|| commit_t.get(&format!("{source}:main")))
                .copied();
            IndexRow {
                name: gs.name.clone(),
                branch: gs.branch.clone(),
                source,
                index_t: gs.index_t,
                ledger_t,
                stale: ledger_t.is_some_and(|lt| gs.index_t < lt),
                tracked: bm25_tracked(gs),
            }
        })
        .collect();
    rows.sort_by(|a, b| (&a.name, &a.branch).cmp(&(&b.name, &b.branch)));

    // Script-friendly mode: just the aliases, one per line, so a maintenance
    // loop can do: `for i in $(fluree bm25 list --stale --untracked); do
    // fluree bm25 sync --index "$i"; done`. `--untracked` narrows that to the
    // indexes no server worker is keeping fresh — the ones a cron still owns.
    if stale_only || untracked_only {
        for row in rows
            .iter()
            .filter(|r| (!stale_only || r.stale) && (!untracked_only || !r.tracked))
        {
            println!("{}:{}", row.name, row.branch);
        }
        return Ok(());
    }

    if rows.is_empty() {
        println!("No BM25 full-text indexes found. Run 'fluree bm25 create ...' to add one.");
        return Ok(());
    }

    // Same look and feel as `fluree list` (comfy_table, dynamic arrangement).
    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec![
        "NAME",
        "BRANCH",
        "SOURCE LEDGER",
        "INDEX_T",
        "LEDGER_T",
        "STALE",
        "TRACKED",
    ]);
    for row in &rows {
        let index_t_str = if row.index_t > 0 {
            row.index_t.to_string()
        } else {
            "-".to_string()
        };
        let ledger_t_str = row
            .ledger_t
            .map_or_else(|| "-".to_string(), |v| v.to_string());
        table.add_row(vec![
            row.name.clone(),
            row.branch.clone(),
            row.source.clone(),
            index_t_str,
            ledger_t_str,
            if row.stale { "YES" } else { "no" }.to_string(),
            if row.tracked { "yes" } else { "NO" }.to_string(),
        ]);
    }
    println!("{table}");
    if rows.iter().any(|r| !r.tracked) {
        println!(
            "\nTRACKED=NO: no server maintenance worker keeps this index fresh — \
             sync it yourself ('fluree bm25 list --untracked' lists them)."
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_create(
    name: &str,
    ledger: &str,
    branch: &str,
    query_inline: Option<&str>,
    query_file: Option<&Path>,
    k1: Option<f64>,
    b: Option<f64>,
    tracked: bool,
    dirs: &FlureeDir,
) -> CliResult<()> {
    // Resolve the indexing query: -e inline > -f file > stdin.
    let source = input::resolve_input(query_inline, None, query_file, None)?;
    let content = input::read_input(&source)?;
    let query: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| CliError::Input(format!("indexing query must be valid JSON: {e}")))?;

    let mut config = Bm25CreateConfig::new(name, ledger, query)
        .with_branch(branch)
        .with_tracked(tracked);
    if let Some(k1) = k1 {
        config = config.with_k1(k1);
    }
    if let Some(b) = b {
        config = config.with_b(b);
    }
    config.validate().map_err(CliError::Api)?;

    eprintln!(
        "  {} indexing {} -> {}:{}...",
        "bm25:".cyan().bold(),
        ledger,
        name,
        branch
    );

    let fluree = build_fluree(dirs)?;
    let result = fluree
        .create_full_text_index(config)
        .await
        .map_err(CliError::Api)?;

    println!(
        "Created full-text index {} (docs={}, terms={}, index_t={}, tracked={}).",
        result.graph_source_id, result.doc_count, result.term_count, result.index_t, tracked
    );
    if tracked {
        println!(
            "  A running server adopts it at its next restart, or right away via \
             POST /v1/fluree/bm25/track {{\"index\":\"{}\"}}.",
            result.graph_source_id
        );
    } else {
        println!(
            "  Not tracked — sync it yourself: fluree bm25 sync --index {}.",
            result.graph_source_id
        );
    }
    Ok(())
}

async fn run_sync(index: &str, dirs: &FlureeDir) -> CliResult<()> {
    eprintln!("  {} syncing {}...", "bm25:".cyan().bold(), index);

    let fluree = build_fluree(dirs)?;
    let result = fluree.sync_bm25_index(index).await.map_err(CliError::Api)?;

    if result.old_watermark == result.new_watermark && result.upserted == 0 && result.removed == 0 {
        println!(
            "Full-text index {} already up to date (watermark {}).",
            result.graph_source_id, result.new_watermark
        );
    } else {
        println!(
            "Synced full-text index {} ({} upserted, {} removed, {} subject{}; \
             watermark {} -> {}{}).",
            result.graph_source_id,
            result.upserted,
            result.removed,
            result.affected_subjects,
            if result.affected_subjects == 1 {
                ""
            } else {
                "s"
            },
            result.old_watermark,
            result.new_watermark,
            if result.was_full_resync {
                ", full resync"
            } else {
                ""
            }
        );
    }
    Ok(())
}

async fn run_drop(index: &str, dirs: &FlureeDir) -> CliResult<()> {
    let fluree = build_fluree(dirs)?;
    let result = fluree
        .drop_full_text_index(index)
        .await
        .map_err(CliError::Api)?;

    if result.was_already_retracted {
        println!("Full-text index {index} was already retracted.");
    } else {
        println!(
            "Dropped full-text index {} (deleted {} snapshot{}).",
            result.graph_source_id,
            result.deleted_snapshots,
            if result.deleted_snapshots == 1 {
                ""
            } else {
                "s"
            }
        );
    }
    Ok(())
}
