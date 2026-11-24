use hir::{
    graph_index::{project_batch_method_call_pairs_checked, GraphIndex},
    MethodCallPair, ModuleId,
};

use super::CallHierarchyIndexBuildError;
use crate::graph::{run_batch_db, BatchDbOpener};

#[derive(Debug)]
pub struct CallHierarchyIndexModuleProjection {
    pub module: ModuleId,
    pub layout_hash: u64,
    pub pairs: Vec<MethodCallPair>,
}

pub fn reproject_call_hierarchy_index_modules(
    graph_index: &GraphIndex,
    batch_size: usize,
    changed_modules: &[ModuleId],
    open_batch: &mut BatchDbOpener<'_>,
) -> Result<Vec<CallHierarchyIndexModuleProjection>, CallHierarchyIndexBuildError> {
    let batch_size = batch_size.max(1);
    let pool = rayon::ThreadPoolBuilder::new().build()?;
    let mut projections = Vec::with_capacity(changed_modules.len());
    for batch in changed_modules.chunks(batch_size) {
        let (pairs, refreshed_layouts) = run_batch_db(
            batch,
            open_batch,
            &pool,
            |db| {
                let pairs = project_batch_method_call_pairs_checked(&pool, db, graph_index, batch)
                    .map_err(CallHierarchyIndexBuildError::IncompleteNotifyProjection)?;
                let mut refreshed_layouts = GraphIndex::new();
                refreshed_layouts.add_batch(&pool, db, batch);
                Ok::<_, CallHierarchyIndexBuildError>((pairs, refreshed_layouts))
            },
            |_release| {},
        )?;
        let by_module = MethodCallPair::group_by_caller_module(&pairs);
        for &module in batch {
            let layout_hash = refreshed_layouts
                .module_layout_hash(module)
                .ok_or(CallHierarchyIndexBuildError::MissingLayoutHash(module))?;
            let pairs = by_module.get(&module).cloned().unwrap_or_default();
            projections.push(CallHierarchyIndexModuleProjection { module, layout_hash, pairs });
        }
    }

    Ok(projections)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use vfs::{file_set::FileSet, FileId, VfsPath};

    use super::*;
    use crate::RootDatabaseImpl;
    use hir::{DefDatabase, MethodId};

    const ROOT: SourceRootId = SourceRootId(0);

    #[test]
    fn call_hierarchy_index_catch_up_minimal_graph_index() {
        // Given: a body edit to one of two modules during a build.
        let changed_module = ModuleId::new(FileId(0));
        let unchanged_module = ModuleId::new(FileId(1));
        let modules = [changed_module, unchanged_module];
        let initial_text =
            "Процедура Первый()\nВторой();\nКонецПроцедуры\n\nПроцедура Второй()\nКонецПроцедуры";
        let changed_text =
            "Процедура Первый()\nКонецПроцедуры\n\nПроцедура Второй()\nКонецПроцедуры";
        let unchanged_text = "Процедура Второй()\nКонецПроцедуры";
        let mut target_index = GraphIndex::new();
        let base = batch_database(&modules, initial_text, unchanged_text);
        for &module in &modules {
            target_index.add_module(&base, module);
        }
        let batches = RefCell::new(Vec::new());
        let mut open_batch = |batch: &[ModuleId]| {
            batches.borrow_mut().push(batch.to_vec());
            batch_database(batch, changed_text, unchanged_text)
        };

        // When: catch-up reprojects only the edited module.
        let projections = reproject_call_hierarchy_index_modules(
            &target_index,
            1,
            &[changed_module],
            &mut open_batch,
        )
        .expect("catch-up projection");

        // Then: the unchanged module is not opened to rebuild a GraphIndex.
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].module, changed_module);
        assert!(projections[0].pairs.is_empty());
        assert_eq!(*batches.borrow(), vec![vec![changed_module]]);
    }

    #[test]
    fn catch_up_accepts_named_unknown_array_gap_without_marking_projection_truncated() {
        // Given: a caller whose incoming array has a known local callback plus an
        // unknown input-value alternative, which is an intentional analysis boundary.
        let changed_module = ModuleId::new(FileId(0));
        let unchanged_module = ModuleId::new(FileId(1));
        let modules = [changed_module, unchanged_module];
        let source = r#"
Процедура Первый(А, Флаг)
    А.Добавить("H");
    А.Добавить(ЭтотОбъект);
    Если Флаг Тогда
        А = Новый Массив;
        А.Добавить("Другой");
        А.Добавить(ЭтотОбъект);
    КонецЕсли;
    О = Новый("CallbackDescription", А);
КонецПроцедуры
Процедура H(Параметр) Экспорт
КонецПроцедуры
"#;
        let target = "Процедура Второй()\nКонецПроцедуры";
        let db = batch_database(&modules, source, target);
        let summary = db.module_call_summary(changed_module);
        assert!(!summary.notify_projection_incomplete);
        assert!(!summary.notify_dynamic_signature_analysis_incomplete.is_empty());
        let mut graph_index = GraphIndex::new();
        for &module in &modules {
            graph_index.add_module(&db, module);
        }
        let mut open_batch = |_batch: &[ModuleId]| batch_database(&modules, source, target);

        // When: catch-up reprojects the changed module through the checked path.
        let projections = reproject_call_hierarchy_index_modules(
            &graph_index,
            1,
            &[changed_module],
            &mut open_batch,
        )
        .expect("named unknown-array uncertainty is not budget truncation");

        // Then: the generation remains projectable despite the descriptive gap.
        assert_eq!(projections.len(), 1);
        assert_eq!(projections[0].module, changed_module);
    }

    #[test]
    fn call_hierarchy_index_catch_up_groups_pairs_per_module_and_preserves_order() {
        // Given: two modules, each with one internal call, both edited.
        let first_module = ModuleId::new(FileId(0));
        let second_module = ModuleId::new(FileId(1));
        let modules = [first_module, second_module];
        let text =
            "Процедура Первый()\nВторой();\nКонецПроцедуры\n\nПроцедура Второй()\nКонецПроцедуры";
        let mut target_index = GraphIndex::new();
        let base = batch_database(&modules, text, text);
        for &module in &modules {
            target_index.add_module(&base, module);
        }
        let batches = RefCell::new(Vec::new());
        let mut open_batch = |batch: &[ModuleId]| {
            batches.borrow_mut().push(batch.to_vec());
            batch_database(batch, text, text)
        };

        // When: catch-up reprojects both modules in one batch, in reverse order.
        let changed_modules = [second_module, first_module];
        let projections = reproject_call_hierarchy_index_modules(
            &target_index,
            changed_modules.len(),
            &changed_modules,
            &mut open_batch,
        )
        .expect("catch-up projection");

        // Then: both modules are opened together, projections follow changed_modules
        // order, and each module receives only its own caller-target pair.
        assert_eq!(*batches.borrow(), vec![vec![second_module, first_module]]);
        assert_eq!(projections.len(), 2);
        assert_eq!(projections[0].module, second_module);
        assert_eq!(projections[1].module, first_module);
        assert_eq!(
            projections[0].pairs,
            vec![MethodCallPair::new(
                MethodId { module: second_module, local_id: hir::MethodKey::first("Первый") },
                MethodId { module: second_module, local_id: hir::MethodKey::first("Второй") }
            )]
        );
        assert_eq!(
            projections[1].pairs,
            vec![MethodCallPair::new(
                MethodId { module: first_module, local_id: hir::MethodKey::first("Первый") },
                MethodId { module: first_module, local_id: hir::MethodKey::first("Второй") }
            )]
        );
    }

    fn batch_database(
        batch: &[ModuleId],
        changed_text: &str,
        unchanged_text: &str,
    ) -> RootDatabaseImpl {
        let mut db = RootDatabaseImpl::new();
        let mut file_set = FileSet::new();
        file_set.insert(FileId(0), VfsPath::new("/src/Changed.bsl"));
        file_set.insert(FileId(1), VfsPath::new("/src/Unchanged.bsl"));
        db.set_source_root(ROOT, SourceRoot::new_local(file_set));
        for module in batch {
            let text = match module.file_id {
                FileId(0) => changed_text,
                FileId(1) => unchanged_text,
                _ => unreachable!("fixture contains two modules"),
            };
            db.set_file_source_root(module.file_id, ROOT);
            db.set_file_text(module.file_id, text);
        }
        db
    }
}
