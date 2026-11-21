use std::cell::RefCell;

use base_db::{content_revision, SourceDatabase, SourceRoot};
use ide::Analysis;
use vfs::{file_set::FileSet, VfsPath};

use super::*;
use crate::mem_docs::MemDocs;

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
    mem_docs.insert(uri, "Процедура Буфер() КонецПроцедуры".to_owned(), 1);
    let snapshot =
        CallHierarchyIndexFrozenSnapshot::capture(&db, source_root_id, &mem_docs.freeze(), 7)
            .materialize();

    // When: disk bytes change after the snapshot and a batch database is opened.
    std::fs::write(&path, "Процедура ПозжеНаДиске() КонецПроцедуры").expect("rewrite disk");
    let batch = snapshot.open_batch(&[ModuleId::new(file_id)]);

    // Then: the batch sees the captured open-buffer text, not either disk revision.
    let text = Analysis::from_database(batch).file_text(file_id);
    assert_eq!(&*text, "Процедура Буфер() КонецПроцедуры");
}

#[test]
fn call_hierarchy_index_batch_reads_cross_batch_targets_from_frozen_revisions() {
    // Given: two disk-backed common modules, with a call crossing their batches.
    let directory = tempfile::tempdir().expect("temporary directory");
    let caller_path = directory.path().join("src/CommonModules/Первый/Ext/Module.bsl");
    let target_path = directory.path().join("src/CommonModules/Второй/Ext/Module.bsl");
    std::fs::create_dir_all(caller_path.parent().unwrap()).expect("caller directory");
    std::fs::create_dir_all(target_path.parent().unwrap()).expect("target directory");
    let caller_text = "Процедура Начать()\nВторой.Продолжить();\nКонецПроцедуры";
    let target_text = "Процедура Продолжить() Экспорт\nКонецПроцедуры";
    std::fs::write(&caller_path, caller_text).expect("caller module");
    std::fs::write(&target_path, target_text).expect("target module");

    let source_root_id = SourceRootId(0);
    let caller_file = FileId(0);
    let target_file = FileId(1);
    let mut file_set = FileSet::new();
    file_set.insert(caller_file, VfsPath::new(caller_path));
    file_set.insert(target_file, VfsPath::new(target_path));
    let mut db = RootDatabaseImpl::default();
    db.set_source_root(source_root_id, SourceRoot::new_local(file_set));
    for (file_id, text) in [(caller_file, caller_text), (target_file, target_text)] {
        db.set_file_source_root(file_id, source_root_id);
        db.set_file_revision_from_disk(file_id, content_revision(text));
    }
    let snapshot = CallHierarchyIndexFrozenSnapshot::capture(
        &db,
        source_root_id,
        &MemDocs::default().freeze(),
        7,
    )
    .materialize();

    // When: semantic resolution reads the target while only the caller is in the batch.
    let read_target_text =
        Analysis::from_database(snapshot.open_batch(&[ModuleId::new(caller_file)]))
            .file_text(target_file);
    assert_eq!(&*read_target_text, target_text);

    let modules = snapshot.modules();
    let mut open_batch = |batch: &[ModuleId]| snapshot.open_batch(batch);

    // Then: the bounded builder can lower the caller in a database whose own batch
    // does not contain the target module.
    let built = ide::build_call_hierarchy_index(
        ide::CallHierarchyIndexBuildRequest::new(&modules, 1).with_concurrency(1),
        &mut open_batch,
    )
    .expect("bounded index build");

    // Then: bounded indexing completes with both modules' methods available.
    assert_eq!(built.method_count, 2);
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
    let snapshot =
        CallHierarchyIndexFrozenSnapshot::capture(&db, source_root_id, &mem_docs.freeze(), 7);

    // When: the buffer closes before catch-up refreshes the snapshot.
    let refreshed = snapshot.refresh(&MemDocs::default().freeze());
    let batch = refreshed.open_batch(&[ModuleId::new(file_id)]);

    // Then: the batch falls back to the captured disk revision, not empty text.
    let text = Analysis::from_database(batch).file_text(file_id);
    assert_eq!(&*text, disk_text);
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
