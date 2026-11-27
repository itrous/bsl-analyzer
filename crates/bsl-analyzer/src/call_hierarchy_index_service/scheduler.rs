use crate::call_hierarchy_index_overlay::CallHierarchyIndexFrozenSnapshot;
use crate::call_hierarchy_index_state::CallHierarchyIndexSnapshotId;
use crate::global_state::GlobalState;
use salsa::Database as _;

use super::BATCH_SIZE;

impl GlobalState {
    pub fn schedule_call_hierarchy_index_build(&mut self, source_root: base_db::SourceRootId) {
        if self.vfs_done {
            tracing::debug!(
                ?source_root,
                phase = "schedule",
                "call hierarchy compact index build scheduled"
            );
            self.call_hierarchy_index_rebuilds.insert(source_root);
        }
    }

    pub fn supersede_call_hierarchy_index(&mut self, source_root: base_db::SourceRootId) {
        if self
            .call_hierarchy_index
            .active()
            .is_some_and(|lifecycle| lifecycle.supersede(source_root))
        {
            tracing::debug!(
                ?source_root,
                phase = "supersession",
                supersession_reason = "structural_change",
                "call hierarchy compact index build superseded"
            );
            self.schedule_call_hierarchy_index_build(source_root);
        }
    }

    pub fn spawn_pending_call_hierarchy_index_builds(&mut self) {
        if !self.vfs_done || self.call_hierarchy_index_rebuilds.is_empty() {
            return;
        }

        let Some(lifecycle) = self.call_hierarchy_index.active().cloned() else {
            return;
        };

        let roots: Vec<_> =
            std::mem::take(&mut self.call_hierarchy_index_rebuilds).into_iter().collect();
        for source_root in roots {
            if !self.task_pool.pool.has_capacity() || lifecycle.has_active_build(source_root) {
                self.call_hierarchy_index_rebuilds.insert(source_root);
                continue;
            }

            let disk_read = match self.prepare_disk_read() {
                Ok(read) => read,
                Err(_) => {
                    self.call_hierarchy_index_rebuilds.insert(source_root);
                    continue;
                }
            };

            let Some(generation) = lifecycle.next_generation(source_root) else {
                tracing::error!(
                    ?source_root,
                    phase = "freeze",
                    failure_reason = "generation_overflow",
                    "call hierarchy index generation overflow"
                );
                continue;
            };
            let _build_span = tracing::info_span!(
                "call_hierarchy_index_build",
                ?source_root,
                generation,
                batch_size = BATCH_SIZE,
                implementation = "compact_reverse_index",
                workspace_call_graph = false,
            )
            .entered();
            tracing::debug!(
                phase = "freeze",
                "call hierarchy compact index metadata capture started"
            );
            let frozen = match salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                let analysis = self.analysis_host.analysis();
                disk_read.register_cancel(analysis.database().cancellation_token());
                CallHierarchyIndexFrozenSnapshot::capture(
                    analysis.database(),
                    source_root,
                    &self.mem_docs.freeze(),
                    generation,
                    disk_read.clone(),
                )
            })) {
                Ok(frozen) => frozen,
                Err(_) => {
                    self.call_hierarchy_index_rebuilds.insert(source_root);
                    continue;
                }
            };
            tracing::debug!(
                phase = "freeze",
                module_count = frozen.file_set.len(),
                "call hierarchy compact index metadata captured"
            );
            if !lifecycle.start_build(
                source_root,
                generation,
                CallHierarchyIndexSnapshotId(generation),
            ) {
                tracing::debug!(
                    phase = "supersession",
                    supersession_reason = "lifecycle_changed_before_start",
                    "call hierarchy compact index build start superseded"
                );
                self.call_hierarchy_index_rebuilds.insert(source_root);
                continue;
            }

            let worker_lifecycle = lifecycle.clone();
            let mem_docs = self.mem_docs.clone();
            let analysis_guard = self.note_analysis_spawned();
            let spawned = self.task_pool.pool.try_spawn(move || {
                let _analysis_guard = analysis_guard;
                let task = super::worker::run_build(worker_lifecycle, mem_docs, frozen);
                crate::global_state::Task::DiskReadCompleted {
                    epoch: disk_read.epoch(),
                    task: Box::new(task),
                    cancelled: Box::new(crate::global_state::Task::CallHierarchyIndexSuperseded {
                        source_root,
                        generation,
                        reason: "workspace_disk_epoch_changed",
                    }),
                }
            });
            if spawned.is_err() {
                tracing::warn!(
                    phase = "failure",
                    failure_reason = "task_pool_rejected_build",
                    "call hierarchy compact index build failed"
                );
                lifecycle.fail(
                    source_root,
                    generation,
                    "call hierarchy index task pool rejected build".to_owned(),
                );
                self.call_hierarchy_index_rebuilds.insert(source_root);
            }
        }
    }
}
