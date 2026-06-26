//! Send-safe scan planning for Iceberg tables.
//!
//! This module provides `SendScanPlanner` which mirrors `ScanPlanner` but uses
//! `SendIcebergStorage` for AWS SDK integration where futures must be `Send`.

use std::sync::Arc;

use crate::error::{IcebergError, Result};
use crate::io::SendIcebergStorage;
use crate::manifest::{parse_manifest, parse_manifest_list};
use crate::metadata::{Schema, Snapshot, TableMetadata};
use crate::scan::planner::{
    effective_sequence_number, FileScanTask, IncrementalScanPlan, ScanConfig, ScanPlan,
};
use crate::scan::pruning::can_contain_file;
use tracing::Instrument;

/// Send-safe scan planner for Iceberg tables.
///
/// This is identical to `ScanPlanner` but uses `SendIcebergStorage` instead of
/// `IcebergStorage`, producing `Send` futures for use with tokio::spawn and
/// async_trait without ?Send.
pub struct SendScanPlanner<'a, S: SendIcebergStorage> {
    storage: &'a S,
    metadata: &'a TableMetadata,
    config: ScanConfig,
}

impl<'a, S: SendIcebergStorage> SendScanPlanner<'a, S> {
    /// Create a new send-safe scan planner.
    pub fn new(storage: &'a S, metadata: &'a TableMetadata, config: ScanConfig) -> Self {
        Self {
            storage,
            metadata,
            config,
        }
    }

    /// Plan a scan for the current snapshot.
    pub async fn plan_scan(&self) -> Result<ScanPlan> {
        let snapshot = self
            .metadata
            .current_snapshot()
            .ok_or_else(|| IcebergError::SnapshotNotFound("No current snapshot".to_string()))?;

        self.plan_scan_for_snapshot(snapshot).await
    }

    /// Plan a scan for a specific snapshot.
    ///
    /// Wraps the planning work (manifest-list + manifest reads and file pruning)
    /// in an `iceberg.scan_plan` timing span and records the selected/pruned file
    /// counts and estimated row count on the span once they are known.
    pub async fn plan_scan_for_snapshot(&self, snapshot: &Snapshot) -> Result<ScanPlan> {
        let span = tracing::debug_span!(
            "iceberg.scan_plan",
            files_selected = tracing::field::Empty,
            files_pruned = tracing::field::Empty,
            estimated_row_count = tracing::field::Empty,
        );
        async move {
            let plan = self.plan_scan_for_snapshot_inner(snapshot).await?;
            let span = tracing::Span::current();
            span.record("files_selected", plan.files_selected);
            span.record("files_pruned", plan.files_pruned);
            span.record("estimated_row_count", plan.estimated_row_count);
            Ok(plan)
        }
        .instrument(span)
        .await
    }

    /// Inner planning implementation (see [`Self::plan_scan_for_snapshot`]).
    async fn plan_scan_for_snapshot_inner(&self, snapshot: &Snapshot) -> Result<ScanPlan> {
        let schema = self
            .metadata
            .current_schema()
            .ok_or_else(|| IcebergError::Metadata("No current schema".to_string()))?;

        // Clone schema into Arc for sharing with tasks
        let schema_arc = Arc::new(schema.clone());

        // Determine projection
        let (projected_field_ids, projected_columns) = self.resolve_projection(schema)?;

        // Load manifest list
        let manifest_list_path = snapshot.manifest_list.as_ref().ok_or_else(|| {
            IcebergError::Manifest(
                "Snapshot has no manifest list (v1 format not supported)".to_string(),
            )
        })?;

        let manifest_list_data = self.storage.read(manifest_list_path).await?;
        let manifest_entries = parse_manifest_list(&manifest_list_data)?;

        tracing::debug!(
            manifest_count = manifest_entries.len(),
            "Loaded manifest list"
        );

        // Collect data files from manifests, applying pruning
        let mut tasks = Vec::new();
        let mut files_selected = 0;
        let mut files_pruned = 0;
        let mut estimated_row_count = 0i64;

        for manifest_entry in &manifest_entries {
            // Skip delete manifests (already filtered by parse_manifest_list)
            if manifest_entry.is_deletes() {
                continue;
            }

            // Load and parse manifest file
            let manifest_data = self.storage.read(&manifest_entry.manifest_path).await?;
            let data_file_entries = parse_manifest(&manifest_data)?;

            for entry in data_file_entries {
                let data_file = entry.data_file;

                // Apply file-level pruning
                if let Some(filter) = &self.config.filter {
                    if !can_contain_file(filter, &data_file, schema) {
                        files_pruned += 1;
                        continue;
                    }
                }

                files_selected += 1;
                estimated_row_count += data_file.record_count;

                // Create file scan task with schema for correct field ID mapping
                let task = FileScanTask::for_whole_file_with_schema(
                    data_file,
                    projected_field_ids.clone(),
                    self.config.filter.clone(),
                    Arc::clone(&schema_arc),
                );
                tasks.push(task);
            }
        }

        tracing::info!(
            files_selected,
            files_pruned,
            estimated_row_count,
            "Scan planning complete"
        );

        Ok(ScanPlan {
            tasks,
            projected_columns,
            projected_field_ids,
            residual_filter: self.config.filter.clone(),
            estimated_row_count,
            files_selected,
            files_pruned,
        })
    }

    /// Plan a full scan for a snapshot chosen by `selection` (current / by id /
    /// as-of-time) rather than always the current snapshot.
    pub async fn plan_scan_with_selection(
        &self,
        selection: &crate::metadata::SnapshotSelection,
    ) -> Result<ScanPlan> {
        let snapshot =
            crate::metadata::select_snapshot(self.metadata, selection).ok_or_else(|| {
                IcebergError::SnapshotNotFound("No snapshot matches selection".to_string())
            })?;
        self.plan_scan_for_snapshot(snapshot).await
    }

    /// Plan an **incremental (append-only)** scan: the data files ADDED in the
    /// window `(from_snapshot_id, to_snapshot_id]`.
    ///
    /// A file is "added in the window" when its *effective data sequence number*
    /// (see [`effective_sequence_number`]) is strictly greater than the `from`
    /// snapshot's sequence number. `from_snapshot_id = None` means "since genesis"
    /// (the full live state of `to`) — used for the initial materialization.
    ///
    /// This captures **additions only**. The caller MUST first verify the window
    /// is append-only via [`TableMetadata::window_is_append_only`](crate::metadata::TableMetadata::window_is_append_only)
    /// and fall back to a full re-read otherwise — overwrite/delete/replace
    /// snapshots carry updates/deletions this scan cannot see.
    pub async fn plan_incremental(
        &self,
        from_snapshot_id: Option<i64>,
        to_snapshot_id: i64,
    ) -> Result<IncrementalScanPlan> {
        let to_snapshot = self.metadata.snapshot(to_snapshot_id).ok_or_else(|| {
            IcebergError::SnapshotNotFound(format!("to snapshot {to_snapshot_id} not found"))
        })?;
        let from_seq = match from_snapshot_id {
            Some(id) => {
                self.metadata
                    .snapshot(id)
                    .ok_or_else(|| {
                        IcebergError::SnapshotNotFound(format!("from snapshot {id} not found"))
                    })?
                    .sequence_number
            }
            None => 0,
        };
        let to_seq = to_snapshot.sequence_number;

        let schema = self
            .metadata
            .current_schema()
            .ok_or_else(|| IcebergError::Metadata("No current schema".to_string()))?;
        let schema_arc = Arc::new(schema.clone());
        let (projected_field_ids, projected_columns) = self.resolve_projection(schema)?;

        let manifest_list_path = to_snapshot.manifest_list.as_ref().ok_or_else(|| {
            IcebergError::Manifest(
                "Snapshot has no manifest list (v1 format not supported)".to_string(),
            )
        })?;
        let manifest_list_data = self.storage.read(manifest_list_path).await?;
        let manifest_entries = parse_manifest_list(&manifest_list_data)?;

        let mut added_tasks = Vec::new();
        let mut files_selected = 0;
        let mut estimated_row_count = 0i64;

        for manifest_entry in &manifest_entries {
            if manifest_entry.is_deletes() {
                continue; // append-only: data manifests only
            }
            // A manifest written at or before `from` cannot hold files newer than
            // `from` (every file it lists has data-seq <= the manifest's seq).
            if manifest_entry.sequence_number <= from_seq {
                continue;
            }

            let manifest_data = self.storage.read(&manifest_entry.manifest_path).await?;
            let data_file_entries = parse_manifest(&manifest_data)?;

            for entry in data_file_entries {
                let eff_seq = effective_sequence_number(
                    entry.sequence_number,
                    manifest_entry.sequence_number,
                );
                if eff_seq <= from_seq {
                    continue; // present at/before `from`
                }

                let data_file = entry.data_file;
                if let Some(filter) = &self.config.filter {
                    if !can_contain_file(filter, &data_file, schema) {
                        continue;
                    }
                }

                files_selected += 1;
                estimated_row_count += data_file.record_count;
                added_tasks.push(FileScanTask::for_whole_file_with_schema(
                    data_file,
                    projected_field_ids.clone(),
                    self.config.filter.clone(),
                    Arc::clone(&schema_arc),
                ));
            }
        }

        tracing::info!(
            ?from_snapshot_id,
            to_snapshot_id,
            from_seq,
            to_seq,
            files_selected,
            estimated_row_count,
            "Incremental scan planning complete (append-only)"
        );

        Ok(IncrementalScanPlan {
            added_tasks,
            projected_columns,
            projected_field_ids,
            from_sequence_number: from_seq,
            to_snapshot_id,
            to_sequence_number: to_seq,
            files_selected,
            estimated_row_count,
        })
    }

    /// Resolve projection to field IDs and column names.
    fn resolve_projection(&self, schema: &Schema) -> Result<(Vec<i32>, Vec<String>)> {
        match &self.config.projection {
            Some(field_ids) => {
                let mut names = Vec::with_capacity(field_ids.len());
                for &id in field_ids {
                    let field = schema.field(id).ok_or_else(|| {
                        IcebergError::Scan(format!("Field ID {id} not found in schema"))
                    })?;
                    names.push(field.name.clone());
                }
                Ok((field_ids.clone(), names))
            }
            None => {
                // Project all non-nested fields
                let field_ids: Vec<i32> = schema
                    .fields
                    .iter()
                    .filter(|f| !f.is_nested())
                    .map(|f| f.id)
                    .collect();
                let names: Vec<String> = schema
                    .fields
                    .iter()
                    .filter(|f| !f.is_nested())
                    .map(|f| f.name.clone())
                    .collect();
                Ok((field_ids, names))
            }
        }
    }
}
