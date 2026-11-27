use std::cell::RefCell;

use base_db::{content_revision, SourceDatabase, SourceRoot};
use ide::Analysis;
use vfs::{file_set::FileSet, VfsPath};

use super::*;
use crate::mem_docs::MemDocs;

fn disk_read() -> ide_host_core::WorkspaceDiskRead {
    ide_host_core::WorkspaceDiskGate::default().try_read().unwrap()
}

#[test]
fn workspace_disk_background_frozen_derivatives_retain_ticket_and_register_tokens() {
    use salsa::Database as _;
    use std::time::{Duration, Instant};
    let gate = ide_host_core::WorkspaceDiskGate::default();
    let mut db = RootDatabaseImpl::default();
    let root = SourceRootId(0);
    db.set_source_root(root, SourceRoot::new_local(FileSet::new()));
    let frozen = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        root,
        &MemDocs::default().freeze(),
        1,
        gate.try_read().unwrap(),
    );
    let materialized = frozen.materialize();
    let refreshed = materialized.refresh(&MemDocs::default().freeze());
    let batch = refreshed.open_batch(&[]);
    let token = batch.cancellation_token();
    let writing = gate.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        tx.send(writing.begin_write(Duration::from_secs(5), &|| false)).unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !token.is_cancelled() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(!refreshed.disk_read.is_current());
    assert!(rx.try_recv().is_err());
    drop(batch);
    drop(frozen);
    drop(materialized);
    assert!(rx.try_recv().is_err());
    drop(refreshed);
    rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap().finish();
    worker.join().unwrap();
    assert!(gate.try_read().is_ok());
}

#[test]
fn call_hierarchy_index_overlay_prefers_frozen_open_buffer_text() {
    // Given: a disk module with an unsaved editor buffer.
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("Module.bsl");
    std::fs::write(&path, "Процедура НаДиске() КонецПроцедуры").expect("disk module");
    let file_id = FileId(0);
    let source_root_id = SourceRootId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(path.clone()));
    let mut db = RootDatabaseImpl::default();
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, source_root_id);
    db.set_file_revision_from_disk(file_id, content_revision("Процедура НаДиске() КонецПроцедуры"));
    let uri = Url::from_file_path(&path).expect("file URL");
    let mut mem_docs = MemDocs::default();
    mem_docs.insert(uri.clone(), "Процедура Буфер() КонецПроцедуры".to_owned(), 1);
    let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        source_root_id,
        &mem_docs.freeze(),
        7,
        disk_read(),
    )
    .materialize();

    // When: disk bytes change after the snapshot and a batch database is opened.
    std::fs::write(&path, "Процедура ПозжеНаДиске() КонецПроцедуры").expect("rewrite disk");
    let batch = snapshot.open_batch(&[ModuleId::new(file_id)]);

    // Then: the batch sees the captured open-buffer text, not either disk revision.
    let text = Analysis::from_database(batch).file_text(file_id);
    assert_eq!(&*text, "Процедура Буфер() КонецПроцедуры");

    let refreshed_text = "Процедура ОбновлённыйБуфер() КонецПроцедуры";
    let mut refreshed_docs = MemDocs::default();
    refreshed_docs.insert(uri, refreshed_text.to_owned(), 2);
    let refreshed = snapshot.refresh(&refreshed_docs.freeze());
    let refreshed_batch = refreshed.open_batch(&[ModuleId::new(file_id)]);
    let text = Analysis::from_database(refreshed_batch).file_text(file_id);
    assert_eq!(&*text, refreshed_text);
}

#[test]
fn call_hierarchy_index_batch_reads_cross_batch_targets_from_frozen_revisions() {
    // Given: a caller, an exported common-module target, and an unrelated module.
    let directory = tempfile::tempdir().expect("temporary directory");
    let config_root = directory.path().join("src");
    let caller_path = config_root.join("CommonModules/Первый/Ext/Module.bsl");
    let target_path = config_root.join("CommonModules/Второй/Ext/Module.bsl");
    let unrelated_path = config_root.join("CommonModules/Третий/Ext/Module.bsl");
    std::fs::create_dir_all(caller_path.parent().unwrap()).expect("caller directory");
    std::fs::create_dir_all(target_path.parent().unwrap()).expect("target directory");
    std::fs::create_dir_all(unrelated_path.parent().unwrap()).expect("unrelated directory");
    let caller_text = "Процедура Начать()\nВторой.Продолжить();\nКонецПроцедуры";
    let target_disk_text = "Процедура Продолжить() Экспорт\nКонецПроцедуры";
    let target_text = "Процедура Продолжить() Экспорт\nВызовИзБуфера();\nКонецПроцедуры";
    let unrelated_text = "Процедура Независимый() КонецПроцедуры";
    std::fs::write(&caller_path, caller_text).expect("caller module");
    std::fs::write(&target_path, target_disk_text).expect("target module");
    std::fs::write(&unrelated_path, unrelated_text).expect("unrelated module");
    // CommonModules marks a configuration root, so declare the target module as a
    // real project would; otherwise the resolver correctly rejects this fixture.
    std::fs::write(config_root.join("Configuration.xml"), "<Configuration/>")
        .expect("configuration metadata");
    std::fs::write(
        config_root.join("CommonModules/Второй.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000031">
        <Properties><Name>Второй</Name><Server>true</Server></Properties>
    </CommonModule>
</MetaDataObject>"#,
    )
    .expect("target common-module metadata");

    let source_root_id = SourceRootId(0);
    let caller_file = FileId(0);
    let target_file = FileId(1);
    let unrelated_file = FileId(2);
    let mut file_set = FileSet::new();
    file_set.insert(caller_file, VfsPath::new(caller_path));
    file_set.insert(target_file, VfsPath::new(target_path.clone()));
    file_set.insert(unrelated_file, VfsPath::new(unrelated_path));
    let mut db = RootDatabaseImpl::default();
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    db.set_workspace_configs_snapshot(ide::WorkspaceConfigsSnapshot::from_paths(vec![(
        None,
        config_root,
    )]));
    for (file_id, text) in [
        (caller_file, caller_text),
        (target_file, target_disk_text),
        (unrelated_file, unrelated_text),
    ] {
        db.set_file_source_root(file_id, source_root_id);
        db.set_file_revision_from_disk(file_id, content_revision(text));
    }
    let target_uri = Url::from_file_path(&target_path).expect("target URL");
    let mut mem_docs = MemDocs::default();
    mem_docs.insert(target_uri, target_text.to_owned(), 1);
    let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        source_root_id,
        &mem_docs.freeze(),
        7,
        disk_read(),
    )
    .materialize();

    // When: the disk file changes after capture and only the caller is opened.
    std::fs::write(&target_path, "Процедура ИзмененаНаДиске() Экспорт\nКонецПроцедуры")
        .expect("rewrite target module");
    let caller_batch = snapshot.open_batch(&[ModuleId::new(caller_file)]);

    // Then: an unregistered cross-batch target reads from the frozen overlay,
    // while an unrelated closed module remains disk-backed and also stays unregistered.
    assert!(caller_batch.try_file_revision_input(target_file).is_none());
    assert!(caller_batch.try_file_revision_input(unrelated_file).is_none());
    assert_eq!(
        caller_batch.file_source_root_input(target_file).source_root_id(&caller_batch),
        source_root_id,
    );
    let analysis = Analysis::from_database(caller_batch.clone());
    assert_eq!(&*analysis.file_text(target_file), target_text);
    assert_eq!(&*analysis.file_text(unrelated_file), unrelated_text);

    let modules = snapshot.modules();
    let mut open_batch = |batch: &[ModuleId]| snapshot.open_batch(batch);

    // The bounded builder lowers the caller with its cross-batch target available.
    let built = ide::build_call_hierarchy_index(
        ide::CallHierarchyIndexBuildRequest::new(&modules, 1).with_concurrency(1),
        &mut open_batch,
    )
    .expect("bounded index build");

    assert_eq!(built.method_count, 3);
    assert_eq!(built.pair_count, 1);
    assert_eq!(
        built.index.callers(hir::MethodId {
            module: ModuleId::new(target_file),
            local_id: hir::MethodKey::first("Продолжить"),
        }),
        &[hir::MethodId {
            module: ModuleId::new(caller_file),
            local_id: hir::MethodKey::first("Начать"),
        }],
    );
}

#[test]
fn call_hierarchy_index_batch_revision_registration_is_linear() {
    fn registration_counts(module_count: usize) -> (usize, usize) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_root_id = SourceRootId(0);
        let mut file_set = FileSet::new();
        let mut contents = Vec::new();
        for index in 0..module_count {
            let module_name = format!("Модуль{index}");
            let next_module = format!("Модуль{}", (index + 1) % module_count);
            let path =
                directory.path().join(format!("src/CommonModules/{module_name}/Ext/Module.bsl"));
            std::fs::create_dir_all(path.parent().unwrap()).expect("module directory");
            let text = format!(
                "Процедура Вызов{index}()\n{next_module}.Цель();\nКонецПроцедуры\n\
                 Процедура Цель() Экспорт\nКонецПроцедуры"
            );
            std::fs::write(&path, &text).expect("module");
            file_set.insert(FileId(index as u32), VfsPath::new(path));
            contents.push(text);
        }

        let mut db = RootDatabaseImpl::default();
        db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
        for (index, text) in contents.iter().enumerate() {
            let file_id = FileId(index as u32);
            db.set_file_source_root(file_id, source_root_id);
            db.set_file_revision_from_disk(file_id, content_revision(text));
        }
        let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
            &db,
            source_root_id,
            &MemDocs::default().freeze(),
            7,
            disk_read(),
        )
        .materialize();
        let modules = snapshot.modules();
        let mut explicit_registrations = Vec::new();
        let mut open_batch = |batch: &[ModuleId]| {
            let db = snapshot.open_batch(batch);
            explicit_registrations.push(
                snapshot
                    .file_set
                    .keys()
                    .filter(|&&file_id| db.try_file_revision_input(file_id).is_some())
                    .count(),
            );
            db
        };
        let built = ide::build_call_hierarchy_index(
            ide::CallHierarchyIndexBuildRequest::new(&modules, 1).with_concurrency(1),
            &mut open_batch,
        )
        .expect("bounded index build");
        drop(open_batch);

        let started_batches: Vec<_> = built
            .batch_events
            .iter()
            .filter(|event| event.kind == ide::CallHierarchyBatchEventKind::Started)
            .collect();
        let index_batch_sizes: Vec<_> = started_batches
            .iter()
            .filter(|event| event.phase == ide::CallHierarchyBatchPhase::Index)
            .map(|event| event.module_count)
            .collect();
        let pair_batch_sizes: Vec<_> = started_batches
            .iter()
            .filter(|event| event.phase == ide::CallHierarchyBatchPhase::MethodPairs)
            .map(|event| event.module_count)
            .collect();
        let index_batch_count = index_batch_sizes.len();
        let (index_registrations, pair_registrations) =
            explicit_registrations.split_at(index_batch_count);
        assert_eq!(index_registrations, index_batch_sizes);
        assert_eq!(pair_registrations, pair_batch_sizes);

        let index_total = index_registrations.iter().sum();
        let pair_total = pair_registrations.iter().sum();
        assert_eq!(index_total, module_count);
        assert_eq!(pair_total, module_count);
        (index_total, pair_total)
    }

    // Pass 1 opens extraction batches; pass 2 opens larger resolution chunks.
    // Both paths register only their caller modules, so their total explicit
    // Salsa inputs scale linearly rather than multiplying by the whole workspace.
    let (small_index, small_pairs) = registration_counts(4);
    let (large_index, large_pairs) = registration_counts(8);
    assert_eq!((small_index, small_pairs), (4, 4));
    assert_eq!((large_index, large_pairs), (8, 8));
    assert_eq!(large_index + large_pairs, 2 * (small_index + small_pairs));
}

#[test]
fn call_hierarchy_index_overlay_uses_captured_disk_revision_after_buffer_closes() {
    // Given: a frozen open buffer backed by a registered disk revision.
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("Module.bsl");
    let disk_text = "Процедура НаДиске() КонецПроцедуры";
    std::fs::write(&path, disk_text).expect("disk module");
    let file_id = FileId(0);
    let source_root_id = SourceRootId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(path.clone()));
    let mut db = RootDatabaseImpl::default();
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, source_root_id);
    db.set_file_revision_from_disk(file_id, content_revision(disk_text));
    let uri = Url::from_file_path(path).expect("file URL");
    let mut mem_docs = MemDocs::default();
    mem_docs.insert(uri, "Процедура ВБуфере() КонецПроцедуры".to_owned(), 1);
    let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        source_root_id,
        &mem_docs.freeze(),
        7,
        disk_read(),
    );

    // When: the buffer closes before catch-up refreshes the snapshot.
    let refreshed = snapshot.refresh(&MemDocs::default().freeze());
    let batch = refreshed.open_batch(&[ModuleId::new(file_id)]);

    // Then: the batch falls back to the captured disk revision, not empty text.
    let text = Analysis::from_database(batch).file_text(file_id);
    assert_eq!(&*text, disk_text);
}

#[test]
fn call_hierarchy_index_overlay_refresh_after_close_rejects_disk_drift() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let caller_path = directory.path().join("Caller.bsl");
    let target_path = directory.path().join("Target.bsl");
    let caller_text = "Процедура Начать() КонецПроцедуры";
    let target_disk_text = "Процедура НаДиске() КонецПроцедуры";
    let target_buffer_text = "Процедура ВБуфере() КонецПроцедуры";
    std::fs::write(&caller_path, caller_text).expect("caller module");
    std::fs::write(&target_path, target_disk_text).expect("target module");

    let caller_file = FileId(0);
    let target_file = FileId(1);
    let source_root_id = SourceRootId(0);
    let mut file_set = FileSet::new();
    file_set.insert(caller_file, VfsPath::new(caller_path));
    file_set.insert(target_file, VfsPath::new(target_path.clone()));
    let mut db = RootDatabaseImpl::default();
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    for (file_id, text) in [(caller_file, caller_text), (target_file, target_disk_text)] {
        db.set_file_source_root(file_id, source_root_id);
        db.set_file_revision_from_disk(file_id, content_revision(text));
    }
    let target_uri = Url::from_file_path(&target_path).expect("target URL");
    let mut mem_docs = MemDocs::default();
    mem_docs.insert(target_uri, target_buffer_text.to_owned(), 1);
    let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        source_root_id,
        &mem_docs.freeze(),
        7,
        disk_read(),
    );

    std::fs::write(&target_path, "Процедура ПослеСнимка() КонецПроцедуры")
        .expect("rewrite target after capture");
    let refreshed = snapshot.refresh(&MemDocs::default().freeze());
    let caller_batch = refreshed.open_batch(&[ModuleId::new(caller_file)]);
    assert!(caller_batch.try_file_revision_input(target_file).is_none());

    let drift = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Analysis::from_database(caller_batch).file_text(target_file);
    }))
    .expect_err("closing the overlay must not hide later disk drift");
    assert!(matches!(
        drift.downcast_ref::<salsa::Cancelled>(),
        Some(salsa::Cancelled::PendingWrite)
    ));
}

#[test]
fn call_hierarchy_index_overlay_materializes_captured_disk_revision() {
    // Given: metadata captured while its disk module still exists.
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("Module.bsl");
    std::fs::write(&path, "Процедура НаДиске() КонецПроцедуры").expect("disk module");
    let file_id = FileId(0);
    let source_root_id = SourceRootId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(path.clone()));
    let mut db = RootDatabaseImpl::default();
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, source_root_id);
    db.set_file_revision_from_disk(file_id, content_revision("Процедура НаДиске() КонецПроцедуры"));
    let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        source_root_id,
        &MemDocs::default().freeze(),
        7,
        disk_read(),
    );
    std::fs::remove_file(path).expect("remove module after capture");

    // When: a worker materializes the captured snapshot.
    let result = snapshot.materialize();

    // Then: materialization uses the captured disk revision without rereading the module.
    assert_eq!(result.disk_revisions.get(&file_id), snapshot.disk_revisions.get(&file_id));
}

#[test]
fn call_hierarchy_index_non_bsl_filter() {
    // Given: a source root containing one BSL module and unrelated metadata files.
    let directory = tempfile::tempdir().expect("temporary directory");
    let bsl_path = directory.path().join("Module.bsl");
    let xml_path = directory.path().join("Configuration.xml");
    let json_path = directory.path().join("manifest.json");
    std::fs::write(&bsl_path, "Процедура Модуль() КонецПроцедуры").expect("BSL module");
    std::fs::write(&xml_path, "<Configuration/>").expect("metadata file");
    std::fs::write(&json_path, "{}").expect("JSON file");
    let source_root_id = SourceRootId(0);
    let bsl_file_id = FileId(0);
    let xml_file_id = FileId(1);
    let json_file_id = FileId(2);
    let mut file_set = FileSet::new();
    file_set.insert(bsl_file_id, VfsPath::new(bsl_path));
    file_set.insert(xml_file_id, VfsPath::new(xml_path));
    file_set.insert(json_file_id, VfsPath::new(json_path));
    let mut db = RootDatabaseImpl::default();
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    db.set_file_source_root(bsl_file_id, source_root_id);
    db.set_file_revision_from_disk(
        bsl_file_id,
        content_revision("Процедура Модуль() КонецПроцедуры"),
    );

    // When: the frozen snapshot drives a bounded index build.
    let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        source_root_id,
        &MemDocs::default().freeze(),
        7,
        disk_read(),
    )
    .materialize();
    let modules = snapshot.modules();
    let opened_batches = RefCell::new(Vec::new());
    let mut open_batch = |batch: &[ModuleId]| {
        opened_batches.borrow_mut().push(batch.to_vec());
        snapshot.open_batch(batch)
    };
    ide::build_call_hierarchy_index(
        ide::CallHierarchyIndexBuildRequest::new(&modules, 1),
        &mut open_batch,
    )
    .expect("bounded index build");

    // Then: only the BSL module remains frozen and reaches both build passes.
    assert_eq!(modules, vec![ModuleId::new(bsl_file_id)]);
    assert_eq!(snapshot.file_set.len(), 1);
    assert!(snapshot.file_set.contains_key(&bsl_file_id));
    assert_eq!(
        *opened_batches.borrow(),
        vec![vec![ModuleId::new(bsl_file_id)], vec![ModuleId::new(bsl_file_id)]],
    );
}

/// A batch database carries the workspace's roots as they were registered —
/// kinds and closures included. Re-reading them from labels would turn an
/// external object into an independent extension: visible to the designer's
/// view, blind to the extensions it must see.
#[test]
fn call_hierarchy_index_overlay_keeps_root_kinds_and_closures() {
    use ide_db::RootDatabase as _;

    let directory = tempfile::tempdir().expect("temporary directory");
    let cf = directory.path().join("cf");
    let ext = directory.path().join("ext");
    let epf = directory.path().join("epf");
    std::fs::create_dir_all(epf.join("АРМ/Ext")).expect("export layout");
    std::fs::create_dir_all(&cf).expect("base");
    std::fs::create_dir_all(&ext).expect("extension");
    let module = epf.join("АРМ/Ext/ObjectModule.bsl");
    std::fs::write(&module, "Процедура П() КонецПроцедуры").expect("object module");

    let file_id = FileId(0);
    let source_root_id = SourceRootId(0);
    let mut file_set = FileSet::new();
    file_set.insert(file_id, VfsPath::new(module.clone()));
    let mut db = RootDatabaseImpl::default();
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, source_root_id);
    db.set_file_revision_from_disk(file_id, content_revision("Процедура П() КонецПроцедуры"));
    let paths = vec![(None, cf), (Some("EXT".to_owned()), ext), (Some("EPF".to_owned()), epf)];
    db.set_workspace_configs_snapshot(ide::WorkspaceConfigsSnapshot {
        external_owners: Vec::new(),
        compilation_overrides: Default::default(),
        extension_scopes: vec![None, Some(bsl_metadata::ExtensionScope::InfoBase), None],
        canonical_paths: paths.iter().map(|(_, path)| path.clone()).collect(),
        kinds: vec![
            ide::WorkspaceRootKind::Base,
            ide::WorkspaceRootKind::Extension,
            ide::WorkspaceRootKind::External(bsl_metadata::ExternalObjectKind::DataProcessor),
        ],
        paths,
        closures: vec![vec![], vec![], vec![1]],
        topological_order: vec![0, 1, 2],
        fingerprint: Some("external".to_owned()),
        source_exclusions: Default::default(),
    });
    let registered = db.workspace_configs_snapshot();

    let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        source_root_id,
        &MemDocs::default().freeze(),
        7,
        disk_read(),
    );
    let batch = snapshot.open_batch(&[ModuleId::new(file_id)]);

    let carried = batch.workspace_configs_snapshot();
    assert_eq!(carried.kinds, registered.kinds, "the batch keeps every root's kind");
    assert_eq!(carried.closures, registered.closures, "and every root's closure");
    assert!(
        batch.external_root_of_path(&module).is_some(),
        "so the export's module still belongs to an external root in the batch"
    );
}
