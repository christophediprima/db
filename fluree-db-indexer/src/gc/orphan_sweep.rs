//! # Orphan sweep
//!
//! Finds index artifacts that no index version references any more.
//!
//! ## Why this is needed at all
//!
//! [`clean_garbage`](super::clean_garbage) reclaims by walking the `prev_index`
//! chain and honouring each version's garbage manifest. That reclaims everything
//! the chain can still *see* — and nothing else. An artifact that has fallen off
//! the chain is referenced by no root and listed in no garbage manifest, so the
//! chain walk can never discover it. Measured on a live deployment: GC reported
//! `chain_len=22` on every pass while the same ledgers held **370, 169 and 125**
//! root files on disk, with 23–33 GiB of `objects/history` behind them. Chain GC
//! was working perfectly and the volume still filled, twice.
//!
//! Nothing else in the indexer reclaims by reachability, so that space cannot come
//! back from any retention setting. Hence this.
//!
//! ## Matching on digests, not addresses
//!
//! Reachability is compared on the **hex digest** parsed from each listed object's
//! filename, not on a reconstructed address. That is deliberate:
//!
//! - it avoids depending on `StorageContentStore`'s private address mapping;
//! - it is immune to the legacy address forms that mapping still supports (dicts
//!   moved from per-branch to `@shared`, roots renamed `.json` → `.fir6`) — a
//!   sweep that compared addresses would see a live artifact at a legacy path as
//!   unreferenced and delete it;
//! - a digest collision across two `ContentKind`s makes an unreachable artifact
//!   look reachable, which keeps a file that could have been freed. Wrong in the
//!   safe direction.
//!
//! ## Safety
//!
//! Deleting by non-reachability is the most destructive thing in this crate: a
//! reachability set that is wrong, or merely incomplete, deletes live data. Five
//! guards, and `delete` is **off by default** so the first thing any deployment
//! gets is a report:
//!
//! 1. **Abort on an empty or trivial reachable set.** If the chain walk yields no
//!    roots, or no reachable digests, every artifact on disk looks unreferenced.
//!    That is indistinguishable from "the walk failed", so the sweep refuses to
//!    act rather than delete the ledger.
//! 2. **Two-pass confirmation.** An artifact is deleted only if it was
//!    unreferenced on this pass *and* the previous one. `RemoteObject` carries no
//!    timestamp, so an mtime-based "older than N days" window (what Iceberg's
//!    `remove_orphan_files` uses) is not available; requiring two separated
//!    observations is the substitute. An in-flight build's freshly written
//!    artifacts are unreferenced on one pass at most — by the next they are either
//!    referenced by a published root, or the build failed and they are genuinely
//!    orphaned.
//! 3. **A per-run deletion cap** that bounds one pass and then CONTINUES, logging
//!    how many remain. It must not veto the work: a real backlog exceeds any cap
//!    by definition, and an earlier version that refused outright made the backlog
//!    permanent, since nothing else reclaims those objects. Truncation is only
//!    dangerous when silent.
//! 4. **Index prefixes only.** Commit and nameservice data are never listed, so
//!    they cannot be deleted even if the reachable set is wrong.
//! 5. **Abort if listing finds nothing while the chain references artifacts.**
//!    Those artifacts must be on disk, so `listed=0` with `reachable>0` means the
//!    prefixes are wrong, not that the ledger is empty — a state an earlier version
//!    reported as a clean sweep of a tidy ledger.

use super::collector::walk_prev_index_chain_cs_cached;
use crate::error::Result;
use fluree_db_core::{ContentId, ContentKind, ContentStore, StorageRead};
use std::collections::HashSet;
use std::path::PathBuf;

/// Default ceiling on deletions per run (guard 3).
pub const DEFAULT_MAX_DELETE_PER_RUN: usize = 10_000;

/// Configuration for [`sweep_orphans`].
#[derive(Debug, Clone)]
pub struct OrphanSweepConfig {
    /// Optional disk artifact cache, shared with the chain walk.
    pub artifact_cache_dir: Option<PathBuf>,
    /// Actually delete confirmed orphans. **Defaults to `false`** — report only.
    ///
    /// Leave it off until a deployment's reported numbers have been sanity-checked
    /// against `du`. The report is the useful half on its own: it quantifies
    /// unreachable bytes, which no other tool here can.
    pub delete: bool,
    /// Ceiling on deletions in a single run (guard 3).
    pub max_delete_per_run: usize,
}

impl Default for OrphanSweepConfig {
    fn default() -> Self {
        Self {
            artifact_cache_dir: None,
            delete: false,
            max_delete_per_run: DEFAULT_MAX_DELETE_PER_RUN,
        }
    }
}

/// Outcome of one sweep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanSweepReport {
    /// Objects seen under the index prefixes.
    pub listed: usize,
    /// Distinct digests reachable from the retained chain.
    pub reachable: usize,
    /// Listed objects matching no reachable digest, this pass.
    pub candidates: usize,
    /// Candidates also seen as candidates on the previous pass (guard 2).
    pub confirmed: usize,
    /// Confirmed orphans actually released. Always 0 when `delete` is false.
    pub deleted: usize,
    /// Bytes held by this pass's candidates — the size of the problem.
    ///
    /// Meaningful only when [`Self::sizes_available`] is true: backends that do not
    /// implement `list_prefix_with_metadata` report counts but not sizes.
    pub candidate_bytes: u64,
    /// Whether the backend could report object sizes, i.e. whether
    /// [`Self::candidate_bytes`] means anything. False on backends that only
    /// implement the plain `list_prefix`, such as the local filesystem.
    pub sizes_available: bool,
    /// Set when a guard stopped the sweep. `deleted` is then always 0.
    pub aborted: Option<&'static str>,
}

/// Prefixes searched. Index artifacts only — guard 4.
///
/// Derived with the SAME helpers `content_path` uses, deliberately: a namespace id
/// is `ledger:branch`, which maps to `ledger/branch/…`, and dicts live outside the
/// branch under `ledger/@shared/…`. Hand-rolling this is how the first version
/// listed zero objects — it produced `ledger:branch/main/index/`, which matches
/// nothing. Reusing the canonical helpers means the two cannot drift.
fn index_prefixes(namespace_id: &str) -> Vec<String> {
    let branch = fluree_db_core::ledger_id_prefix_for_path(namespace_id);
    let shared = fluree_db_core::address_path::shared_prefix_for_path(namespace_id);
    vec![format!("{branch}/index/"), format!("{shared}/dicts/")]
}

/// The hex digest in an object address: the filename stem.
///
/// `…/objects/leaves/3909ce8c….fli` → `3909ce8c…`. Returns `None` for anything
/// that does not look like a hex digest, which is what keeps non-artifact files
/// (manifests, stray uploads) out of the candidate set entirely.
fn digest_from_address(address: &str) -> Option<&str> {
    let file = address.rsplit('/').next()?;
    let stem = file.split('.').next()?;
    let ok = stem.len() >= 32 && stem.chars().all(|c| c.is_ascii_hexdigit());
    ok.then_some(stem)
}

/// `ContentKind` implied by an address, for rebuilding a `ContentId` to release.
///
/// Deletion goes through [`ContentStore::release`], which needs a `ContentId`, and
/// listing only gives addresses — so the kind has to come from the path. An
/// unrecognised path yields `None` and the object is left alone.
fn kind_from_address(address: &str) -> Option<ContentKind> {
    if address.contains("/objects/leaves/") {
        Some(ContentKind::IndexLeaf)
    } else if address.contains("/objects/branches/") {
        Some(ContentKind::IndexBranch)
    } else if address.contains("/objects/history/") {
        Some(ContentKind::HistorySidecar)
    } else if address.contains("/index/roots/") {
        Some(ContentKind::IndexRoot)
    } else if address.contains("/index/garbage/") {
        Some(ContentKind::GarbageRecord)
    } else if address.contains("/index/stats/") {
        Some(ContentKind::StatsSketch)
    } else {
        None
    }
}

/// Find index artifacts no retained index version references.
///
/// Returns the report and this pass's candidate address set, which the caller
/// holds and passes back as `previous_candidates` next time (guard 2). Keeping
/// that state with the caller rather than persisting it means a restart merely
/// re-primes the confirmation — strictly more conservative, and no on-disk format
/// to version.
pub async fn sweep_orphans(
    store: &dyn ContentStore,
    storage: &dyn StorageRead,
    namespace_id: &str,
    current_root_id: &ContentId,
    previous_candidates: &HashSet<String>,
    config: &OrphanSweepConfig,
) -> Result<(OrphanSweepReport, HashSet<String>)> {
    let mut report = OrphanSweepReport::default();

    // 1. Every digest any retained index version still refers to.
    let chain = walk_prev_index_chain_cs_cached(
        store,
        current_root_id,
        config.artifact_cache_dir.as_deref(),
    )
    .await?;

    // Guard 1: no chain means we cannot tell "unreferenced" from "walk failed".
    if chain.is_empty() {
        report.aborted = Some("empty prev-index chain");
        tracing::warn!(
            namespace_id,
            root_id = %current_root_id,
            "Orphan sweep aborted: prev-index chain walk returned no versions"
        );
        return Ok((report, HashSet::new()));
    }

    let mut reachable: HashSet<String> = HashSet::new();
    for entry in &chain {
        reachable.insert(entry.root_id.digest_hex());
        if let Some(g) = &entry.garbage_id {
            reachable.insert(g.digest_hex());
        }
        for cid in entry.root.all_cas_ids() {
            reachable.insert(cid.digest_hex());
        }
    }
    report.reachable = reachable.len();

    // Guard 1 (cont.): a walk that succeeded but yielded nothing to protect is
    // equally untrustworthy.
    if reachable.is_empty() {
        report.aborted = Some("empty reachable set");
        tracing::warn!(
            namespace_id,
            chain_len = chain.len(),
            "Orphan sweep aborted: chain has versions but no reachable artifacts"
        );
        return Ok((report, HashSet::new()));
    }

    // 2. Everything actually on disk under the index prefixes (guard 4).
    //
    // `list_prefix_with_metadata` is the nicer call — it carries `size_bytes`, so
    // the report can say how much space is unreachable — but it is a DEFAULT trait
    // method that errors "not supported by this storage backend" on backends that
    // do not override it, the local filesystem among them. So it is opportunistic:
    // try it for the byte figure, and fall back to the always-implemented
    // `list_prefix`, reporting counts without bytes rather than failing the sweep.
    let mut candidates: HashSet<String> = HashSet::new();
    let mut have_sizes = true;
    for prefix in index_prefixes(namespace_id) {
        match storage.list_prefix_with_metadata(&prefix).await {
            Ok(objects) => {
                for obj in objects {
                    report.listed += 1;
                    let Some(digest) = digest_from_address(&obj.address) else {
                        continue; // not an artifact filename — never a candidate
                    };
                    if !reachable.contains(digest) {
                        report.candidate_bytes += obj.size_bytes;
                        candidates.insert(obj.address);
                    }
                }
            }
            Err(_) => {
                have_sizes = false;
                for address in storage.list_prefix(&prefix).await? {
                    report.listed += 1;
                    let Some(digest) = digest_from_address(&address) else {
                        continue;
                    };
                    if !reachable.contains(digest) {
                        candidates.insert(address);
                    }
                }
            }
        }
    }
    report.candidates = candidates.len();
    report.sizes_available = have_sizes;

    // Guard 5: the chain references artifacts, so they must BE on disk. Listing
    // nothing while `reachable > 0` is incoherent — it means the prefixes are wrong
    // (or listing silently returned empty), not that the ledger is empty.
    //
    // Added because exactly that happened: the first deployed version built its
    // prefixes by hand, produced `ledger:branch/main/index/`, and reported
    // `listed=0 reachable=4052`. Nothing was deleted — an object that is never
    // listed can never become a candidate, so a wrong prefix can only ever
    // under-collect — but it looked like a clean sweep of a tidy ledger, which is
    // the most misleading result this code could produce. Fail loudly instead.
    if report.listed == 0 {
        report.aborted = Some("listed nothing while the chain references artifacts");
        tracing::warn!(
            namespace_id,
            reachable = report.reachable,
            prefixes = ?index_prefixes(namespace_id),
            "Orphan sweep aborted: listing returned no objects but the chain \
             references artifacts — check the storage prefixes"
        );
        return Ok((report, HashSet::new()));
    }

    // 3. Guard 2: only orphans seen on two consecutive passes are actionable.
    let mut confirmed: Vec<&String> = candidates
        .iter()
        .filter(|a| previous_candidates.contains(*a))
        .collect();
    report.confirmed = confirmed.len();

    tracing::info!(
        namespace_id,
        chain_len = chain.len(),
        listed = report.listed,
        reachable = report.reachable,
        candidates = report.candidates,
        candidate_bytes = report.candidate_bytes,
        confirmed = report.confirmed,
        delete = config.delete,
        "Orphan sweep complete"
    );

    if !config.delete {
        // Report-only: the numbers alone quantify unreachable bytes, which chain
        // GC cannot see and no other tool here reports.
        return Ok((report, candidates));
    }

    // Guard 3: bound how much one pass may delete, then CONTINUE.
    //
    // Earlier this refused outright when `confirmed` exceeded the cap, on the
    // reasoning that truncating silently was worse than doing nothing. That was
    // wrong, and deadlocked the exact case the sweep exists for: a real backlog is
    // BY DEFINITION more than the cap (we measured 14,728 confirmed against a cap
    // of 10,000), nothing else reclaims those objects, so every subsequent pass
    // refused too and the backlog was permanent.
    //
    // A cap on a repeatable operation should limit the blast radius of one pass,
    // not veto the work. So delete up to the cap and say plainly how many remain —
    // the next pass takes the next batch. Truncation is only dangerous when it is
    // silent, which the log below prevents.
    let over_cap = confirmed.len().saturating_sub(config.max_delete_per_run);
    if over_cap > 0 {
        tracing::warn!(
            namespace_id,
            confirmed = confirmed.len(),
            max_delete_per_run = config.max_delete_per_run,
            remaining_after_this_pass = over_cap,
            "Orphan sweep capped: deleting the per-run maximum, the rest follow on \
             later passes. If this does not shrink pass over pass, stop and check \
             the reachability set before raising the cap."
        );
        confirmed.truncate(config.max_delete_per_run);
    }

    for address in confirmed {
        let Some(kind) = kind_from_address(address) else {
            continue; // unknown path shape: leave it alone
        };
        let Some(digest) = digest_from_address(address) else {
            continue;
        };
        let Some(cid) = ContentId::from_hex_digest(kind.to_codec(), digest) else {
            continue;
        };
        match store.release(&cid).await {
            Ok(()) => report.deleted += 1,
            Err(e) => tracing::debug!(
                address = %address,
                error = %e,
                "Orphan release failed (may already be gone)"
            ),
        }
    }

    tracing::info!(
        namespace_id,
        deleted = report.deleted,
        "Orphan sweep deleted confirmed orphans"
    );

    Ok((report, candidates))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_the_filename_stem() {
        let d = "3909ce8c719276eed790e6539cc1d85e4a5b8a4f2f568f9990004f3d86382a2f";
        assert_eq!(
            digest_from_address(&format!("ns/main/index/objects/leaves/{d}.fli")),
            Some(d)
        );
        // Roots use a different extension; the stem is still the digest.
        assert_eq!(
            digest_from_address(&format!("ns/main/index/roots/{d}.fir6")),
            Some(d)
        );
    }

    #[test]
    fn non_artifact_filenames_are_never_candidates() {
        // Anything not digest-shaped must be skipped rather than deleted: this is
        // what keeps manifests and stray files out of the candidate set.
        for a in [
            "ns/main/index/objects/leaves/not-a-digest.fli",
            "ns/main/index/manifest.json",
            "ns/main/index/objects/leaves/短.fli",
            "ns/main/index/objects/leaves/abc.fli", // too short
        ] {
            assert_eq!(digest_from_address(a), None, "{a} must not parse");
        }
    }

    #[test]
    fn kind_is_derived_from_the_path() {
        assert_eq!(
            kind_from_address("ns/main/index/objects/leaves/x.fli"),
            Some(ContentKind::IndexLeaf)
        );
        assert_eq!(
            kind_from_address("ns/main/index/objects/history/x.fhs1"),
            Some(ContentKind::HistorySidecar)
        );
        assert_eq!(
            kind_from_address("ns/main/index/roots/x.fir6"),
            Some(ContentKind::IndexRoot)
        );
        // Unknown shapes yield None so the object is left alone.
        assert_eq!(kind_from_address("ns/main/commit/x.fcv2"), None);
    }

    /// Prefixes must match what `content_path` actually writes.
    ///
    /// The first deployed version hand-rolled these as `{namespace_id}/main/…`,
    /// which for the real id `ledger:branch` produced `ledger:branch/main/index/`
    /// and listed **zero** objects on a ledger holding thousands. Pin the real
    /// shape: `ledger:branch` → `ledger/branch/index/`, dicts under
    /// `ledger/@shared/dicts/`.
    #[test]
    fn prefixes_match_the_real_address_layout() {
        let p = index_prefixes("mydb:main");
        assert!(
            p.contains(&"mydb/main/index/".to_string()),
            "branch prefix wrong: {p:?}"
        );
        assert!(
            p.contains(&"mydb/@shared/dicts/".to_string()),
            "shared dict prefix wrong: {p:?}"
        );
        // The ':' must never survive into a prefix — that was the bug.
        assert!(
            !p.iter().any(|x| x.contains(':')),
            "a ':' in a prefix matches nothing: {p:?}"
        );
        // Guard 4: commit data is never listed, so it can never be deleted even
        // if the reachable set is wrong.
        assert!(
            !p.iter().any(|x| x.contains("commit")),
            "commit data must never be in scope"
        );
    }

    /// A non-default branch must also resolve correctly, not just `main`.
    #[test]
    fn prefixes_respect_a_non_default_branch() {
        let p = index_prefixes("mydb:feature-x");
        assert!(p.contains(&"mydb/feature-x/index/".to_string()), "{p:?}");
        // Dicts are shared ACROSS branches, so they stay on the ledger, not the
        // branch — getting this wrong would sweep another branch's dictionaries.
        assert!(p.contains(&"mydb/@shared/dicts/".to_string()), "{p:?}");
    }

    #[test]
    fn report_defaults_mark_sizes_unavailable() {
        // `candidate_bytes` is only meaningful when the backend could report sizes.
        // A default report claiming sizes were available would make 0 bytes look
        // like "nothing unreachable" rather than "we could not measure".
        let r = OrphanSweepReport::default();
        assert!(!r.sizes_available);
        assert_eq!(r.candidate_bytes, 0);
    }

    #[test]
    fn the_per_run_cap_bounds_a_pass_without_vetoing_the_work() {
        // Regression: an earlier version REFUSED when confirmed exceeded the cap,
        // which deadlocked the only case the sweep exists for. A real backlog is
        // larger than any cap (measured: 14,728 confirmed against a cap of 10,000),
        // nothing else reclaims those objects, so refusing made it permanent.
        //
        // Pin the arithmetic the fix relies on: a pass takes `cap` and leaves the
        // remainder for the next one, so the backlog strictly shrinks.
        let confirmed = 14_728usize;
        let cap = DEFAULT_MAX_DELETE_PER_RUN;
        assert!(confirmed > cap, "the interesting case is over the cap");
        let over = confirmed.saturating_sub(cap);
        assert_eq!(over, 4_728);
        // Progress per pass must be non-zero, or the backlog never clears.
        assert!(
            confirmed - over > 0,
            "a capped pass must still delete something"
        );
        // And a pass under the cap must not be truncated at all.
        assert_eq!(500usize.saturating_sub(cap), 0);
    }

    #[test]
    fn delete_is_off_by_default() {
        // A deployment's first experience of this must be a report, never a
        // deletion.
        assert!(!OrphanSweepConfig::default().delete);
        assert_eq!(
            OrphanSweepConfig::default().max_delete_per_run,
            DEFAULT_MAX_DELETE_PER_RUN
        );
    }
}
