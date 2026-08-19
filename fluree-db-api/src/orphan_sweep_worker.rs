//! Periodic orphan sweep: reclaim index artifacts no live index chain references.
//!
//! # Why this exists
//!
//! [`clean_garbage`](fluree_db_indexer::clean_garbage) reclaims by NAME — a root's
//! garbage manifest lists what the previous version replaced, and the collector
//! releases exactly those CIDs. Anything orphaned another way is invisible to it,
//! and `clean_garbage` deliberately steps over an absent manifest and defers it to
//! the storage sweep. Until now nothing ran that sweep except an operator calling
//! `/v1/fluree/sweep` by hand, so deferred artifacts accumulated indefinitely.
//!
//! Measured on a four-ledger deployment before this existed: **80,994 orphaned
//! artifacts totalling 57.25 GiB, 46 % of the volume**, dominated by 74,079 shared
//! dictionary blobs (37.67 GiB) rather than index history. Retention settings could
//! never have reclaimed any of it — `gc_max_old_indexes` bounds the *reachable*
//! chain, and none of these were on it.
//!
//! # Why the API layer and not the indexer
//!
//! The sweep's own contract: *"Callers that intend to reclaim must hold the ledger's
//! index build excluded for the whole span of planning and deleting — a build
//! publishes its artifacts before the root that references them, so a concurrent
//! build's output is indistinguishable from an orphan."*
//!
//! That hold, and the per-branch enumeration a ledger-wide sweep needs, exist only
//! here. Driving the sweep from inside the indexer would mean sweeping unheld, where
//! a build's freshly written artifacts look exactly like orphans — the one mistake
//! this must not make, because its consequence is deleting live data. So this drives
//! the existing held path rather than reimplementing an unheld one.
//!
//! # Safety
//!
//! [`Fluree::sweep_index_storage`] plans and deletes under a single hold, so the two
//! are atomic with respect to index builds. `delete` still defaults to false: the
//! worker reports what it would reclaim and touches nothing until a deployment opts
//! in, because the failure mode of a wrong sweep is silent data loss, not wasted
//! space.

use crate::Fluree;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// The sweep period for `mins`, floored at one minute.
///
/// Split out from [`OrphanSweepWorker::new`] so the floor is testable on its own.
/// Asserting it by re-evaluating the same expression in a test proves nothing —
/// clippy caught exactly that, and it was right.
fn interval_from_mins(mins: u64) -> Duration {
    Duration::from_secs(mins.max(1) * 60)
}

/// Drives [`Fluree::plan_index_sweep`] / [`Fluree::sweep_index_storage`] on a timer.
pub struct OrphanSweepWorker {
    fluree: Arc<Fluree>,
    interval: Duration,
    delete: bool,
}

impl OrphanSweepWorker {
    /// `interval_mins` is clamped to at least 1: a full prefix listing is the
    /// expensive part of a sweep (~100k objects on a ledger with a few hundred
    /// index versions), so a zero or sub-minute interval would spend the server on
    /// storage round trips.
    pub fn new(fluree: Arc<Fluree>, interval_mins: u64, delete: bool) -> Self {
        Self {
            fluree,
            interval: interval_from_mins(interval_mins),
            delete,
        }
    }

    /// Sweep every ledger, forever, one interval apart.
    ///
    /// Sleeps FIRST. At start-up the indexer is still restoring and may not have
    /// published a root yet; a ledger whose index head is absent has no live set to
    /// subtract, and sweeping then would classify a great deal as orphaned. Waiting
    /// one interval costs nothing — these artifacts have been accumulating for days.
    pub async fn run(self) {
        info!(
            interval_secs = self.interval.as_secs(),
            delete = self.delete,
            "orphan sweep worker started"
        );
        loop {
            tokio::time::sleep(self.interval).await;
            self.sweep_all().await;
        }
    }

    async fn sweep_all(&self) {
        // Deduped by LEDGER NAME, not by name:branch. A sweep is ledger-wide
        // because dict blobs are shared across branches, and the API rejects a
        // branch-qualified alias for exactly that reason — sweeping one branch
        // would orphan dicts another branch still reads.
        let names: BTreeSet<String> = match self.fluree.nameservice().all_records().await {
            Ok(records) => records
                .into_iter()
                .filter(|r| !r.retracted)
                .map(|r| r.name)
                .collect(),
            Err(e) => {
                // Never fatal: a nameservice blip must not end the worker, or the
                // sweep silently stops for the process's lifetime and the leak
                // resumes with no signal.
                warn!(error = %e, "orphan sweep: could not list ledgers, skipping this pass");
                return;
            }
        };

        for name in names {
            if self.delete {
                match self.fluree.sweep_index_storage(&name).await {
                    Ok(result) if result.reclaimed > 0 || !result.failures.is_empty() => info!(
                        ledger = %name,
                        reclaimed = result.reclaimed,
                        failures = result.failures.len(),
                        "orphan sweep reclaimed index artifacts"
                    ),
                    Ok(_) => debug!(ledger = %name, "orphan sweep: nothing to reclaim"),
                    Err(e) => warn!(error = %e, ledger = %name, "orphan sweep failed"),
                }
            } else {
                match self.fluree.plan_index_sweep(&name).await {
                    // Report at INFO whenever anything is unreferenced. That number
                    // is the only visibility a deployment has into unreachable
                    // bytes, and its absence is why a volume filled twice before
                    // anyone knew this class of leak existed.
                    Ok(plan) if !plan.orphans.is_empty() => info!(
                        ledger = %name,
                        orphans = plan.orphans.len(),
                        scanned = plan.scanned,
                        live = plan.live,
                        "orphan sweep candidates found; set gc_orphan_delete to reclaim them"
                    ),
                    Ok(plan) => debug!(
                        ledger = %name,
                        scanned = plan.scanned,
                        live = plan.live,
                        "orphan sweep: no candidates"
                    ),
                    Err(e) => warn!(error = %e, ledger = %name, "orphan sweep plan failed"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interval is a floor, not a suggestion: a zero would turn a full prefix
    /// listing — the expensive part of a sweep — into a hot loop against storage.
    #[test]
    fn interval_is_floored_at_one_minute() {
        assert_eq!(interval_from_mins(0), Duration::from_secs(60), "zero");
        assert_eq!(interval_from_mins(1), Duration::from_secs(60), "one");
        // Above the floor it is exactly what was asked for, in minutes.
        assert_eq!(interval_from_mins(60), Duration::from_secs(3_600));
        assert_eq!(interval_from_mins(1_440), Duration::from_secs(86_400));
    }
}
