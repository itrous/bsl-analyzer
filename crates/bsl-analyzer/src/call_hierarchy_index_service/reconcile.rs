use std::sync::Arc;
use std::time::Instant;

use hir::{graph_index::GraphIndex, CallHierarchyReverseIndex};

use super::worker::{superseded, BuildContext};
use super::{CatchUpBudget, BATCH_SIZE};
use crate::call_hierarchy_index_state::CallHierarchyIndexState;
use crate::global_state::Task;

pub(super) fn reconcile(
    context: BuildContext,
    mut index: CallHierarchyReverseIndex,
    target_index: GraphIndex,
) -> Task {
    let BuildContext { lifecycle, mem_docs, frozen, budget } = context;
    let source_root = frozen.source_root_id;
    let generation = frozen.creation_generation;
    let started = Instant::now();
    let mut passes = 0usize;
    let _catch_up_span = tracing::info_span!(
        "call_hierarchy_index_catch_up",
        ?source_root,
        generation,
        batch_size = BATCH_SIZE,
        implementation = "compact_reverse_index",
        workspace_call_graph = false,
    )
    .entered();

    loop {
        let Some(edited_files) = lifecycle.drain_journal(source_root, generation) else {
            return superseded(
                &lifecycle,
                source_root,
                generation,
                "generation_not_building_during_catch_up",
            );
        };
        if edited_files.is_empty() {
            if passes != 0 && catch_up_exhausted(passes, started, budget) {
                return catch_up_superseded(&lifecycle, source_root, generation, passes);
            }
            tracing::debug!(
                phase = "catch_up",
                catch_up_passes = passes,
                "call hierarchy compact index catch-up completed"
            );
            let pair_count = index.len();
            let estimated_heap_bytes = index.estimated_heap_bytes();
            let candidate = Arc::new(index);
            if lifecycle.publish(source_root, generation, Arc::clone(&candidate)) {
                tracing::info!(
                    phase = "publish",
                    pair_count,
                    estimated_heap_bytes,
                    "call hierarchy compact index published"
                );
                return Task::CallHierarchyIndexBuilt { source_root, generation, index: candidate };
            }
            index = match Arc::try_unwrap(candidate) {
                Ok(index) if lifecycle.is_building(source_root, generation) => index,
                Ok(_) | Err(_) => {
                    return superseded(
                        &lifecycle,
                        source_root,
                        generation,
                        "publication_lifecycle_changed",
                    );
                }
            };
            continue;
        }
        if catch_up_exhausted(passes, started, budget) {
            return catch_up_superseded(&lifecycle, source_root, generation, passes);
        }
        passes += 1;
        let changed_count = edited_files.len();
        tracing::debug!(
            phase = "catch_up",
            catch_up_pass = passes,
            edited_file_count = changed_count,
            "call hierarchy compact index catch-up started"
        );

        let refreshed = frozen.refresh(&mem_docs.freeze());
        let changed_modules: Vec<_> = edited_files
            .into_iter()
            .map(ide::ModuleId::new)
            .filter(|module| refreshed.file_set.contains_key(&module.file_id))
            .collect();
        if changed_modules.len() != changed_count {
            tracing::debug!(
                phase = "supersession",
                edited_file_count = changed_count,
                changed_module_count = changed_modules.len(),
                "an edited file left the frozen snapshot"
            );
            return superseded(
                &lifecycle,
                source_root,
                generation,
                "edited_file_left_frozen_snapshot",
            );
        }
        let mut open_batch = |batch: &[ide::ModuleId]| refreshed.open_batch(batch);
        let projections = ide::reproject_call_hierarchy_index_modules(
            &target_index,
            BATCH_SIZE,
            &changed_modules,
            &mut open_batch,
        );
        let projections = match projections {
            Ok(projections) => projections,
            Err(error) => {
                tracing::debug!(
                    phase = "supersession",
                    failure_reason = %error,
                    "call hierarchy compact index catch-up projection failed"
                );
                return superseded(
                    &lifecycle,
                    source_root,
                    generation,
                    "catch_up_projection_failed",
                );
            }
        };
        let projection_pair_count =
            projections.iter().map(|projection| projection.pairs.len()).sum::<usize>();
        tracing::debug!(
            phase = "catch_up",
            catch_up_pass = passes,
            changed_module_count = projections.len(),
            pair_count = projection_pair_count,
            "call hierarchy compact index catch-up projection completed"
        );
        for projection in projections {
            if index.layout_hash(projection.module) != Some(projection.layout_hash) {
                tracing::debug!(
                    phase = "supersession",
                    ?projection.module,
                    "a caught-up module changed its layout"
                );
                return superseded(&lifecycle, source_root, generation, "module_layout_changed");
            }
            index.replace_module(projection.module, projection.pairs, projection.layout_hash);
        }
        if catch_up_exhausted(passes, started, budget) {
            return catch_up_superseded(&lifecycle, source_root, generation, passes);
        }
    }
}

fn catch_up_superseded(
    lifecycle: &CallHierarchyIndexState,
    source_root: base_db::SourceRootId,
    generation: u64,
    passes: usize,
) -> Task {
    tracing::debug!(phase = "supersession", catch_up_passes = passes, "catch-up budget exhausted");
    superseded(lifecycle, source_root, generation, "catch_up_budget_exhausted")
}

pub(super) fn catch_up_exhausted(passes: usize, started: Instant, budget: CatchUpBudget) -> bool {
    passes >= budget.passes || started.elapsed() >= budget.limit
}
