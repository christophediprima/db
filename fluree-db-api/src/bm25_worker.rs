//! Background BM25 maintenance worker.
//!
//! Keeps BM25 indexes fresh by reacting to nameservice events: when a ledger an
//! index depends on advances, the worker debounces briefly and then re-syncs
//! the index via [`Fluree::sync_bm25_index`] (incremental from the persisted
//! watermark, full resync only when it has to be).
//!
//! # Where this has to run
//!
//! The event bus is **per-`Fluree`-instance and in-process** — only the process
//! that *writes* a commit publishes the event, and a second process reading the
//! same storage hears nothing. So the worker is only useful inside the process
//! that writes the source ledgers: in practice `fluree server`, which spawns it
//! on non-peer nodes. A one-shot CLI invocation or a standalone daemon over the
//! same storage directory would never be woken.
//!
//! Indexes created by *another* process (`fluree bm25 create`) publish no event
//! this worker can hear, so on start it also enumerates the nameservice
//! ([`Bm25WorkerHandle::register_all_bm25_indexes`]) and adopts everything
//! already persisted. That doubles as restart recovery: nothing has to be
//! re-registered after a bounce.
//!
//! # Which indexes
//!
//! Maintenance is opt-out per index, via the `tracked` flag persisted on the
//! graph-source record ([`Bm25CreateConfig::tracked`](crate::Bm25CreateConfig),
//! `fluree bm25 create --no-track`, [`Fluree::set_bm25_tracked`]). Because it
//! lives on the record, it survives restarts and is visible to any process
//! reading the nameservice — `fluree bm25 list` shows it as a column. An index
//! created before the flag existed reads as tracked.
//!
//! Unlike [`crate::vector_worker`] (still single-threaded `Rc`/`RefCell`), this
//! worker owns an `Arc<Fluree>` and shares its state behind `Arc<Mutex<…>>`, so
//! it is `Send` and spawns with a plain `tokio::spawn` — same shape as
//! [`crate::materialize_worker`].
//!
//! # Example
//!
//! ```ignore
//! use std::sync::Arc;
//! use fluree_db_api::{Bm25MaintenanceWorker, FlureeBuilder};
//!
//! let fluree = Arc::new(FlureeBuilder::file(".fluree/storage").build_client().await?);
//!
//! let worker = Bm25MaintenanceWorker::new(Arc::clone(&fluree));
//! let handle = worker.handle();
//! let task = tokio::spawn(worker.run());
//!
//! // Persisted indexes are adopted on start; register a new one explicitly:
//! handle.register_graph_source("my-search:main").await?;
//!
//! handle.stop();
//! task.await.ok();
//! ```

use crate::{ApiError, Bm25SyncResult, Fluree, Result};
use fluree_db_core::ledger_id::normalize_ledger_id;
use fluree_db_nameservice::{GraphSourceType, NameServiceEvent};
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{self, Duration, Instant};
use tracing::{debug, error, info, warn};

/// Type alias for a pinned boxed future used in the BM25 sync worker.
type SyncFuture = Pin<Box<dyn Future<Output = (String, Result<()>)> + Send>>;

/// How long the loop sleeps when it has nothing pending. Events wake it
/// immediately; this only bounds how long a cooperative `stop()` takes to be
/// noticed, so it's coarse on purpose — an idle server shouldn't wake 10×/s.
const IDLE_TICK: Duration = Duration::from_secs(1);

/// Canonicalize a ledger/graph-source alias to `name:branch`.
///
/// Aliases reach this worker in two spellings. A graph source's stored
/// `dependencies` may omit the branch (a bare `name` means `name:main` to
/// Fluree), while `LedgerCommitPublished` always carries the canonical
/// `name:branch`; and an HTTP caller may `track "foo"` for a record whose id is
/// `foo:main`. Every key that goes into or out of the maps is normalized here,
/// so a bare alias matches its commit events and can't register the same index
/// twice under two names.
fn canonical_alias(alias: &str) -> String {
    normalize_ledger_id(alias).unwrap_or_else(|_| alias.to_string())
}

/// Configuration for the BM25 maintenance worker.
#[derive(Debug, Clone)]
pub struct Bm25WorkerConfig {
    /// Maximum number of concurrent sync operations.
    pub max_concurrent_syncs: usize,
    /// Whether to auto-register BM25 graph sources as they are created.
    pub auto_register: bool,
    /// Debounce interval in milliseconds (delay sync to batch rapid commits).
    pub debounce_ms: u64,
    /// Whether to enumerate persisted BM25 indexes from the nameservice on
    /// start. This is how indexes created by another process — and everything
    /// registered before a restart — get adopted.
    pub discover_on_start: bool,
    /// Whether to sync indexes found stale by the start-up enumeration. The
    /// staleness check is a nameservice read; only indexes actually behind
    /// their source ledger are synced.
    pub sync_stale_on_start: bool,
}

impl Default for Bm25WorkerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_syncs: 4,
            auto_register: true,
            debounce_ms: 100,
            discover_on_start: true,
            sync_stale_on_start: true,
        }
    }
}

/// Statistics for the maintenance worker.
#[derive(Debug, Clone, Default)]
pub struct Bm25WorkerStats {
    /// Total number of sync operations performed.
    pub syncs_performed: u64,
    /// Number of sync operations that failed.
    pub syncs_failed: u64,
    /// Number of events received.
    pub events_received: u64,
    /// Number of registered graph sources.
    pub registered_graph_sources: usize,
}

/// Registration bookkeeping for the BM25 maintenance worker.
///
/// Plain data — the worker shares it behind an `Arc<Mutex<…>>`.
pub struct Bm25WorkerState {
    /// Reverse dependency map: ledger_id -> set of graph source IDs.
    ledger_to_graph_sources: HashMap<String, HashSet<String>>,
    /// Forward map: graph_source_id -> set of ledger_ides (for unregistration).
    gs_to_ledgers: HashMap<String, HashSet<String>>,
    /// Statistics.
    stats: Bm25WorkerStats,
}

impl Bm25WorkerState {
    /// Create a new empty worker state.
    pub fn new() -> Self {
        Self {
            ledger_to_graph_sources: HashMap::new(),
            gs_to_ledgers: HashMap::new(),
            stats: Bm25WorkerStats::default(),
        }
    }

    /// Register a graph source with its dependencies.
    pub fn register_graph_source(&mut self, graph_source_id: &str, dependencies: &[String]) {
        let graph_source_id = canonical_alias(graph_source_id);
        let deps_set: HashSet<String> = dependencies.iter().map(|d| canonical_alias(d)).collect();

        // Drop any stale reverse edges first: a re-register with a changed
        // dependency set must not leave the old ledgers pointing here.
        self.unregister_graph_source(&graph_source_id);

        // Update forward map
        self.gs_to_ledgers
            .insert(graph_source_id.clone(), deps_set.clone());

        // Update reverse map
        for ledger in &deps_set {
            self.ledger_to_graph_sources
                .entry(ledger.clone())
                .or_default()
                .insert(graph_source_id.clone());
        }

        self.stats.registered_graph_sources = self.gs_to_ledgers.len();
        debug!(
            graph_source_id,
            ?dependencies,
            "Registered graph source for maintenance"
        );
    }

    /// Unregister a graph source. Returns whether it was registered.
    pub fn unregister_graph_source(&mut self, graph_source_id: &str) -> bool {
        let graph_source_id = canonical_alias(graph_source_id);
        let removed = if let Some(ledgers) = self.gs_to_ledgers.remove(&graph_source_id) {
            // Remove from reverse map
            for ledger in ledgers {
                if let Some(graph_sources) = self.ledger_to_graph_sources.get_mut(&ledger) {
                    graph_sources.remove(&graph_source_id);
                    if graph_sources.is_empty() {
                        self.ledger_to_graph_sources.remove(&ledger);
                    }
                }
            }
            true
        } else {
            false
        };
        self.stats.registered_graph_sources = self.gs_to_ledgers.len();
        if removed {
            debug!(
                graph_source_id,
                "Unregistered graph source from maintenance"
            );
        }
        removed
    }

    /// Get graph sources that depend on a ledger.
    pub fn graph_sources_for_ledger(&self, ledger_id: &str) -> Vec<String> {
        self.ledger_to_graph_sources
            .get(&canonical_alias(ledger_id))
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get all registered graph sources.
    pub fn registered_graph_sources(&self) -> Vec<String> {
        self.gs_to_ledgers.keys().cloned().collect()
    }

    /// Whether a graph source is registered for maintenance.
    pub fn is_registered(&self, graph_source_id: &str) -> bool {
        self.gs_to_ledgers
            .contains_key(&canonical_alias(graph_source_id))
    }

    /// Get all watched ledgers.
    pub fn watched_ledgers(&self) -> Vec<String> {
        self.ledger_to_graph_sources.keys().cloned().collect()
    }

    /// Record a sync operation.
    pub fn record_sync(&mut self, success: bool) {
        self.stats.syncs_performed += 1;
        if !success {
            self.stats.syncs_failed += 1;
        }
    }

    /// Record an event.
    pub fn record_event(&mut self) {
        self.stats.events_received += 1;
    }

    /// Get current stats.
    pub fn stats(&self) -> &Bm25WorkerStats {
        &self.stats
    }
}

impl Default for Bm25WorkerState {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to a running [`Bm25MaintenanceWorker`]: register / unregister
/// indexes, sync one on demand, read stats, and request stop. Cheap to clone
/// (all shared state is behind `Arc`). Shared between the spawned worker task
/// and request handlers.
#[derive(Clone)]
pub struct Bm25WorkerHandle {
    fluree: Arc<Fluree>,
    state: Arc<Mutex<Bm25WorkerState>>,
    stop: Arc<AtomicBool>,
}

impl Bm25WorkerHandle {
    /// Register a BM25 index for automatic maintenance, resolving its source
    /// ledgers from the nameservice. The worker then syncs it whenever any of
    /// those ledgers commits. Idempotent.
    ///
    /// Errors if the graph source doesn't exist, is retracted, or isn't BM25.
    pub async fn register_graph_source(&self, graph_source_id: &str) -> Result<()> {
        let record = self
            .fluree
            .nameservice()
            .lookup_graph_source(graph_source_id)
            .await?
            .ok_or_else(|| {
                ApiError::NotFound(format!("Graph source not found: {graph_source_id}"))
            })?;

        if !record.is_bm25() {
            return Err(ApiError::Config(format!(
                "Not a BM25 graph source: {graph_source_id} (type {:?})",
                record.source_type
            )));
        }
        if record.retracted {
            return Err(ApiError::Config(format!(
                "Cannot track a retracted graph source: {graph_source_id}"
            )));
        }

        // Register under the record's own canonical id, not the caller's
        // spelling — `track "foo"` and the start-up enumeration must land on
        // the same key, or the index ends up registered (and synced) twice.
        self.register_graph_source_with_deps(&record.graph_source_id, &record.dependencies);
        Ok(())
    }

    /// Register a graph source with explicit dependencies (no nameservice lookup).
    pub fn register_graph_source_with_deps(&self, graph_source_id: &str, dependencies: &[String]) {
        self.lock()
            .register_graph_source(graph_source_id, dependencies);
    }

    /// Reconcile one graph source against the nameservice: register it if it is
    /// a live, tracked BM25 index, unregister it otherwise. Returns whether it
    /// is registered afterwards.
    ///
    /// This is how a `tracked` flip, a retraction, or a dependency change is
    /// absorbed — the config event says only that *something* changed, so the
    /// record is the source of truth.
    pub async fn sync_registration(&self, graph_source_id: &str) -> Result<bool> {
        let record = self
            .fluree
            .nameservice()
            .lookup_graph_source(graph_source_id)
            .await?;

        let keep = record
            .as_ref()
            .is_some_and(|r| r.is_bm25() && !r.retracted && crate::bm25_tracked(r));

        match (keep, record) {
            (true, Some(record)) => {
                self.register_graph_source_with_deps(&record.graph_source_id, &record.dependencies);
                Ok(true)
            }
            _ => {
                self.unregister_graph_source(graph_source_id);
                Ok(false)
            }
        }
    }

    /// Adopt every persisted, non-retracted, **tracked** BM25 index in the
    /// nameservice, and drop registrations that no longer qualify. Returns how
    /// many are registered. This is the discovery path for indexes created out
    /// of process, and the restart-recovery path.
    pub async fn register_all_bm25_indexes(&self) -> Result<usize> {
        let records = self.fluree.nameservice().all_graph_source_records().await?;

        let mut keep: HashSet<String> = HashSet::new();
        for record in records
            .iter()
            .filter(|r| r.is_bm25() && !r.retracted && crate::bm25_tracked(r))
        {
            self.register_graph_source_with_deps(&record.graph_source_id, &record.dependencies);
            keep.insert(canonical_alias(&record.graph_source_id));
        }

        // Anything registered that the nameservice no longer vouches for —
        // retracted, untracked, or dropped while we weren't listening. Without
        // this, a missed `GraphSourceRetracted` would leave a dead index
        // registered forever, erroring on every commit.
        for stale in self
            .registered_graph_sources()
            .into_iter()
            .filter(|id| !keep.contains(id))
        {
            info!(graph_source = %stale, "Dropping BM25 index that is no longer tracked");
            self.unregister_graph_source(&stale);
        }

        Ok(keep.len())
    }

    /// Unregister a graph source from automatic maintenance. Returns whether it
    /// was registered. The index itself is left untouched.
    pub fn unregister_graph_source(&self, graph_source_id: &str) -> bool {
        self.lock().unregister_graph_source(graph_source_id)
    }

    /// Whether a graph source is registered for maintenance.
    pub fn is_registered(&self, graph_source_id: &str) -> bool {
        self.lock().is_registered(graph_source_id)
    }

    /// Get all registered graph sources.
    pub fn registered_graph_sources(&self) -> Vec<String> {
        self.lock().registered_graph_sources()
    }

    /// Get all ledgers whose commits trigger a sync.
    pub fn watched_ledgers(&self) -> Vec<String> {
        self.lock().watched_ledgers()
    }

    /// Sync one index now, counting the outcome in the worker's stats. A no-op
    /// when the index is already at its source ledger's head.
    pub async fn sync_now(&self, graph_source_id: &str) -> Result<Bm25SyncResult> {
        debug!(graph_source = %graph_source_id, "Syncing BM25 index");
        match self.fluree.sync_bm25_index(graph_source_id).await {
            Ok(result) => {
                self.lock().record_sync(true);
                info!(
                    graph_source = %graph_source_id,
                    upserted = result.upserted,
                    removed = result.removed,
                    new_watermark = result.new_watermark,
                    "BM25 index sync completed"
                );
                Ok(result)
            }
            Err(e) => {
                self.lock().record_sync(false);
                error!(graph_source = %graph_source_id, error = %e, "BM25 index sync failed");
                Err(e)
            }
        }
    }

    /// Get current worker statistics.
    pub fn stats(&self) -> Bm25WorkerStats {
        self.lock().stats().clone()
    }

    /// Request the worker to stop.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        info!("BM25 maintenance worker stop requested");
    }

    /// Whether stop has been requested.
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bm25WorkerState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// What one nameservice event asks the worker to do.
///
/// Split out because the two halves have different costs: `sync` is answered
/// from the in-memory dependency map, while `reconcile` needs a nameservice
/// read (the config event says *something* changed, not what), so the loop has
/// to do it asynchronously.
#[derive(Debug, Default, Clone)]
pub struct Bm25EventAction {
    /// Indexes whose source ledger advanced — enqueue a debounced sync.
    pub sync: Vec<String>,
    /// A BM25 graph source whose config was republished: re-read the record and
    /// register or unregister it accordingly.
    pub reconcile: Option<String>,
}

/// BM25 maintenance worker.
///
/// Monitors nameservice events and automatically syncs BM25 indexes when their
/// source ledgers are updated.
pub struct Bm25MaintenanceWorker {
    fluree: Arc<Fluree>,
    config: Bm25WorkerConfig,
    handle: Bm25WorkerHandle,
}

impl Bm25MaintenanceWorker {
    /// Create a new maintenance worker with default config.
    pub fn new(fluree: Arc<Fluree>) -> Self {
        Self::with_config(fluree, Bm25WorkerConfig::default())
    }

    /// Create a new maintenance worker with custom config.
    pub fn with_config(fluree: Arc<Fluree>, config: Bm25WorkerConfig) -> Self {
        Self {
            handle: Bm25WorkerHandle {
                fluree: Arc::clone(&fluree),
                state: Arc::new(Mutex::new(Bm25WorkerState::new())),
                stop: Arc::new(AtomicBool::new(false)),
            },
            fluree,
            config,
        }
    }

    /// Get a clonable handle to register indexes / read stats / stop the worker.
    pub fn handle(&self) -> Bm25WorkerHandle {
        self.handle.clone()
    }

    /// Process a single nameservice event.
    ///
    /// Returns the list of graph source IDs that need syncing.
    pub fn process_event(&self, event: &NameServiceEvent) -> Bm25EventAction {
        self.handle.lock().record_event();

        match event {
            NameServiceEvent::LedgerCommitPublished {
                ledger_id,
                commit_t,
                ..
            } => {
                let graph_sources = self.handle.lock().graph_sources_for_ledger(ledger_id);
                if !graph_sources.is_empty() {
                    info!(
                        ledger = %ledger_id,
                        commit_t,
                        gs_count = graph_sources.len(),
                        "Ledger commit triggers BM25 index sync"
                    );
                }
                Bm25EventAction {
                    sync: graph_sources,
                    reconcile: None,
                }
            }
            NameServiceEvent::LedgerIndexPublished {
                ledger_id, index_t, ..
            } => {
                // Index updates don't require graph source sync (commit already triggered it)
                debug!(ledger = %ledger_id, index_t, "Ledger index published (no BM25 sync needed)");
                Bm25EventAction::default()
            }
            NameServiceEvent::GraphSourceConfigPublished {
                graph_source_id,
                source_type,
                ..
            } => {
                // Only BM25 graph sources — vector/R2RML/Iceberg sources have
                // their own maintenance paths. The event carries no config, so
                // whether this one is *tracked* needs a nameservice read: hand
                // it back for the loop to reconcile asynchronously.
                let reconcile = (self.config.auto_register
                    && *source_type == GraphSourceType::Bm25)
                    .then(|| graph_source_id.clone());
                Bm25EventAction {
                    sync: vec![],
                    reconcile,
                }
            }
            NameServiceEvent::GraphSourceRetracted { graph_source_id } => {
                // Unregister retracted graph source
                if self.handle.unregister_graph_source(graph_source_id) {
                    info!(graph_source = %graph_source_id, "Unregistered retracted BM25 index");
                }
                Bm25EventAction::default()
            }
            _ => Bm25EventAction::default(), // Other events don't trigger sync
        }
    }

    /// Sync a single graph source, counting the outcome in the worker's stats.
    pub async fn sync_graph_source(&self, graph_source_id: &str) -> Result<()> {
        self.handle.sync_now(graph_source_id).await.map(|_| ())
    }

    /// Re-read one graph source's record and register or unregister it to match
    /// — how a `tracked` flip or a dependency change is absorbed.
    async fn reconcile_registration(&self, graph_source_id: &str) {
        match self.handle.sync_registration(graph_source_id).await {
            Ok(true) => {
                info!(graph_source = %graph_source_id, "Tracking BM25 index for maintenance");
            }
            Ok(false) => {
                debug!(graph_source = %graph_source_id, "BM25 index is not tracked; not maintaining it");
            }
            Err(e) => {
                warn!(graph_source = %graph_source_id, error = %e, "Failed to reconcile BM25 index registration");
            }
        }
    }

    /// Adopt persisted, tracked BM25 indexes and return the subset that is
    /// already behind its source ledger (so the caller can sync them).
    /// Staleness is a nameservice read — no index bytes are loaded.
    ///
    /// `sync_stale` is separate from discovery because the two callers want
    /// different things: start-up honours `config.sync_stale_on_start`, while a
    /// lagged event bus always needs the catch-up (the dropped commit events
    /// were the only notice those indexes would have got).
    async fn discover_indexes(&self, sync_stale: bool) -> Vec<String> {
        let count = match self.handle.register_all_bm25_indexes().await {
            Ok(count) => count,
            Err(e) => {
                warn!(
                    error = %e,
                    "BM25 maintenance worker: index discovery failed; \
                     falling back to event-driven registration only"
                );
                return vec![];
            }
        };
        info!(
            indexes = count,
            "BM25 maintenance worker adopted persisted indexes"
        );

        if !sync_stale {
            return vec![];
        }

        let mut stale = Vec::new();
        for graph_source_id in self.handle.registered_graph_sources() {
            match self.fluree.check_bm25_staleness(&graph_source_id).await {
                Ok(check) if check.is_stale => {
                    info!(
                        graph_source = %graph_source_id,
                        index_t = check.index_t,
                        ledger_t = check.ledger_t,
                        lag = check.lag,
                        "BM25 index is stale at start-up; scheduling sync"
                    );
                    stale.push(graph_source_id);
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(graph_source = %graph_source_id, error = %e, "BM25 staleness check failed");
                }
            }
        }
        stale
    }

    /// Run the maintenance loop until [`Bm25WorkerHandle::stop`] is requested.
    /// Spawn this with `tokio::spawn(worker.run())`.
    pub async fn run(self) {
        info!(
            debounce_ms = self.config.debounce_ms,
            max_concurrent_syncs = self.config.max_concurrent_syncs,
            "Starting BM25 maintenance worker"
        );

        // Subscribe to all nameservice events (ledger and graph source changes).
        // Subscribe *before* the start-up enumeration so a commit landing during
        // discovery is buffered by the broadcast channel rather than missed.
        let mut subscription = self
            .fluree
            .event_bus()
            .subscribe(fluree_db_nameservice::SubscriptionScope::All);

        // Debounced batching: we accumulate graph sources to sync and flush them after `debounce_ms`.
        let mut pending: HashSet<String> = HashSet::new();
        let mut next_flush: Option<Instant> = None;

        if self.config.discover_on_start {
            pending.extend(self.discover_indexes(self.config.sync_stale_on_start).await);
            if !pending.is_empty() {
                next_flush = Some(Instant::now());
            }
        }

        // In-flight syncs (bounded by config.max_concurrent_syncs), and which
        // indexes they cover — see the dedup in the flush block below.
        let mut in_flight: futures::stream::FuturesUnordered<SyncFuture> =
            futures::stream::FuturesUnordered::new();
        let mut in_flight_ids: HashSet<String> = HashSet::new();

        loop {
            // Check for stop request
            if self.handle.is_stopped() {
                info!("BM25 maintenance worker stopping");
                break;
            }

            // Flush pending syncs if debounce timer elapsed and we have capacity.
            let now = Instant::now();
            let can_flush = next_flush.map(|t| now >= t).unwrap_or(false);
            if can_flush {
                // One sync per index at a time. A ledger committing faster than
                // its index syncs would otherwise stack duplicate syncs of the
                // same index — same work, same window, and they'd crowd out
                // other indexes. An index left in `pending` because it's busy
                // is flushed on a later tick, so the newer commit still lands.
                let ready: Vec<String> = pending
                    .iter()
                    .filter(|id| !in_flight_ids.contains(*id))
                    .take(
                        self.config
                            .max_concurrent_syncs
                            .saturating_sub(in_flight.len()),
                    )
                    .cloned()
                    .collect();
                for graph_source_id in ready {
                    pending.remove(&graph_source_id);
                    in_flight_ids.insert(graph_source_id.clone());

                    // Own everything the sync needs so the future is 'static +
                    // Send and this task stays spawnable on the shared runtime.
                    let handle = self.handle.clone();
                    let id = graph_source_id.clone();
                    in_flight.push(Box::pin(async move {
                        let res = handle.sync_now(&id).await.map(|_| ());
                        (graph_source_id, res)
                    }));
                }

                // If we've drained pending, clear flush deadline; otherwise keep flushing.
                if pending.is_empty() {
                    next_flush = None;
                } else {
                    next_flush =
                        Some(Instant::now() + Duration::from_millis(self.config.debounce_ms));
                }
            }

            // Sleep until the next flush deadline, or — with nothing pending —
            // an idle tick just long enough to keep stop latency bounded. (The
            // server's real teardown is `JoinHandle::abort`; this tick only
            // bounds a cooperative `stop()`.)
            let sleep_until = next_flush.unwrap_or_else(|| Instant::now() + IDLE_TICK);
            let sleep_fut = time::sleep_until(sleep_until);
            tokio::pin!(sleep_fut);

            tokio::select! {
                biased;

                // Prefer stop checks + flushing, but still service events promptly.
                res = subscription.receiver.recv() => {
                    match res {
                        Ok(event) => {
                            let action = self.process_event(&event);
                            if let Some(graph_source_id) = action.reconcile {
                                self.reconcile_registration(&graph_source_id).await;
                            }
                            if !action.sync.is_empty() {
                                for gs in action.sync {
                                    pending.insert(gs);
                                }
                                next_flush = Some(Instant::now() + Duration::from_millis(self.config.debounce_ms));
                            }
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            // Do NOT resubscribe: `Lagged` has already moved this
                            // receiver to the oldest surviving event, so a fresh
                            // subscription would start at the tail and throw the
                            // rest of the ring away. Keep it, and re-enumerate —
                            // the evicted events were the only notice those
                            // indexes were going to get, and a sync of an index
                            // already at head is a cheap no-op.
                            warn!(
                                skipped,
                                "BM25 maintenance worker lagged on the event bus; re-running discovery"
                            );
                            pending.extend(self.discover_indexes(true).await);
                            if !pending.is_empty() {
                                next_flush = Some(Instant::now());
                            }
                        }
                        Err(RecvError::Closed) => {
                            // The bus outlives us in practice (we hold an
                            // `Arc<Fluree>`), so this means teardown — and
                            // resubscribing would spin.
                            info!("Event bus closed; BM25 maintenance worker exiting");
                            break;
                        }
                    }
                }

                // Complete one in-flight sync.
                Some((graph_source_id, res)) = in_flight.next() => {
                    in_flight_ids.remove(&graph_source_id);
                    if let Err(e) = res {
                        warn!(graph_source = %graph_source_id, error = %e, "Failed to sync BM25 index");
                    }
                }

                // Debounce tick / stop-check tick
                () = &mut sleep_fut => {}
            }
        }

        info!("BM25 maintenance worker stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_state_register_graph_source() {
        let mut state = Bm25WorkerState::new();

        state.register_graph_source(
            "search:main",
            &["ledger1:main".to_string(), "ledger2:main".to_string()],
        );

        assert_eq!(state.registered_graph_sources(), vec!["search:main"]);
        assert!(state
            .watched_ledgers()
            .contains(&"ledger1:main".to_string()));
        assert!(state
            .watched_ledgers()
            .contains(&"ledger2:main".to_string()));

        let graph_sources = state.graph_sources_for_ledger("ledger1:main");
        assert_eq!(graph_sources, vec!["search:main"]);
    }

    #[test]
    fn test_worker_state_unregister_graph_source() {
        let mut state = Bm25WorkerState::new();

        state.register_graph_source("search:main", &["ledger1:main".to_string()]);
        state.register_graph_source("other:main", &["ledger1:main".to_string()]);

        // Both graph sources depend on ledger1
        let graph_sources = state.graph_sources_for_ledger("ledger1:main");
        assert_eq!(graph_sources.len(), 2);

        // Unregister one
        assert!(state.unregister_graph_source("search:main"));

        let graph_sources = state.graph_sources_for_ledger("ledger1:main");
        assert_eq!(graph_sources, vec!["other:main"]);

        // Unregister the other
        assert!(state.unregister_graph_source("other:main"));

        let graph_sources = state.graph_sources_for_ledger("ledger1:main");
        assert!(graph_sources.is_empty());
        assert!(state.watched_ledgers().is_empty());

        // Unregistering something unknown reports no-op.
        assert!(!state.unregister_graph_source("search:main"));
    }

    #[test]
    fn test_worker_state_multiple_dependencies() {
        let mut state = Bm25WorkerState::new();

        // gs1 depends on ledger1 and ledger2
        state.register_graph_source(
            "gs1:main",
            &["ledger1:main".to_string(), "ledger2:main".to_string()],
        );
        // gs2 depends on ledger2 and ledger3
        state.register_graph_source(
            "gs2:main",
            &["ledger2:main".to_string(), "ledger3:main".to_string()],
        );

        // ledger1 triggers only gs1
        let graph_sources = state.graph_sources_for_ledger("ledger1:main");
        assert_eq!(graph_sources, vec!["gs1:main"]);

        // ledger2 triggers both
        let mut graph_sources = state.graph_sources_for_ledger("ledger2:main");
        graph_sources.sort();
        assert_eq!(graph_sources, vec!["gs1:main", "gs2:main"]);

        // ledger3 triggers only gs2
        let graph_sources = state.graph_sources_for_ledger("ledger3:main");
        assert_eq!(graph_sources, vec!["gs2:main"]);
    }

    #[test]
    fn test_worker_stats() {
        let mut state = Bm25WorkerState::new();

        state.register_graph_source("gs:main", &["ledger:main".to_string()]);
        assert_eq!(state.stats().registered_graph_sources, 1);

        state.record_event();
        state.record_event();
        assert_eq!(state.stats().events_received, 2);

        state.record_sync(true);
        state.record_sync(false);
        assert_eq!(state.stats().syncs_performed, 2);
        assert_eq!(state.stats().syncs_failed, 1);
    }

    #[test]
    fn branchless_dependency_alias_matches_canonical_commit_event() {
        let mut state = Bm25WorkerState::new();

        // A stored dependency may omit the branch; commit events always carry
        // the canonical `name:branch`.
        state.register_graph_source("search:main", &["ledger1".to_string()]);

        assert_eq!(
            state.graph_sources_for_ledger("ledger1:main"),
            vec!["search:main"],
            "a bare `name` dependency must be woken by `name:main` commits"
        );
        assert_eq!(state.watched_ledgers(), vec!["ledger1:main"]);
    }

    #[test]
    fn branchless_graph_source_id_is_the_same_registration() {
        let mut state = Bm25WorkerState::new();

        // What an HTTP `track {"index":"search"}` would pass...
        state.register_graph_source("search", &["ledger1:main".to_string()]);
        // ...and what the start-up enumeration passes for the same record.
        state.register_graph_source("search:main", &["ledger1:main".to_string()]);

        assert_eq!(
            state.registered_graph_sources(),
            vec!["search:main"],
            "the two spellings must be one registration, not two"
        );
        assert_eq!(
            state.graph_sources_for_ledger("ledger1:main"),
            vec!["search:main"]
        );
        assert!(state.is_registered("search"));
        assert!(
            state.unregister_graph_source("search"),
            "untrack by the bare name must find it"
        );
        assert!(state.registered_graph_sources().is_empty());
    }

    #[test]
    fn re_register_replaces_stale_dependency_edges() {
        let mut state = Bm25WorkerState::new();

        state.register_graph_source("search:main", &["ledger1:main".to_string()]);
        // Re-published config now points at a different source ledger.
        state.register_graph_source("search:main", &["ledger2:main".to_string()]);

        assert!(
            state.graph_sources_for_ledger("ledger1:main").is_empty(),
            "the old reverse edge must not survive a re-register"
        );
        assert_eq!(
            state.graph_sources_for_ledger("ledger2:main"),
            vec!["search:main"]
        );
        assert_eq!(state.stats().registered_graph_sources, 1);
    }
}
