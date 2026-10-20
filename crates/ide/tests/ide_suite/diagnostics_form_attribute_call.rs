use bsl_platform::PlatformDataInner;
use hir::{
    Builders, InferenceDiagnostic, MetadataKind, TypeId, TypeKernelDb, TypeKind,
    UnresolvedMethodKind,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn data_processor_module_path() -> PathBuf {
    designer_fixture_path().join("DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl")
}

fn setup_form_module(disk_path: PathBuf, bsl: &str) -> (RootDatabaseImpl, FileId) {
    assert!(disk_path.exists(), "fixture missing: {}", disk_path.display());
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut file_set = FileSet::default();
    file_set.insert(file_id, VfsPath::new(disk_path.to_string_lossy().as_ref()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, bsl);
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    (db, file_id)
}

fn has_platform_data() -> bool {
    !PlatformDataInner::instance().all_methods().is_empty()
}

fn unresolved_kinds(db: &RootDatabaseImpl, file_id: FileId) -> Vec<UnresolvedMethodKind> {
    use hir::HirDatabase;
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    use hir::HirDatabase;
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn is_metadata_ref(db: &RootDatabaseImpl, ty: TypeId, kind: MetadataKind, name: &str) -> bool {
    matches!(
        db.lookup_type(ty),
        TypeKind::MetadataRef(facet) if facet.kind == kind && facet.name.as_str() == name
    )
}

#[test]
fn chained_form_attribute_method_call_silent() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    Объект.НастройкиЭксель.Очистить();\nКонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "form-attribute method call must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}

#[test]
fn form_data_collection_find_by_id_silent_and_preserves_row_schema() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    \
        Строка = Объект.НастройкиЭксель.НайтиПоИдентификатору(1);\n    \
        ИтогАктивна = Строка.Активна;\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "FormDataCollection.НайтиПоИдентификатору must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );

    let row = var_ty(&db, file_id, "строка").expect("строка must be typed");
    assert!(
        matches!(db.lookup_type(row), TypeKind::Union(members)
            if members.contains(&db.undefined())
                && members.iter().any(|member| is_metadata_ref(
                    &db,
                    *member,
                    MetadataKind::TabularSectionRow {
                        parent: bsl_metadata::MdoType::DataProcessor
                    },
                    "ТестоваяОбработка.НастройкиЭксель",
                ))
        ),
        "FindByID must rebind FormDataCollectionItem to the concrete tabular-section row, got {:?}",
        db.lookup_type(row),
    );
    assert_eq!(
        var_ty(&db, file_id, "итогактивна"),
        Some(db.boolean()),
        "row column access after FindByID must keep the tabular-section schema",
    );
}

#[test]
fn chained_form_attribute_misspelled_method_emits_method_not_found() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    \
        Объект.НастройкиЭксель.СовершенноНетТакогоМетода();\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let kinds = unresolved_kinds(&db, file_id);
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::MethodNotFound],
        "misspelled tabular-section method must surface MethodNotFound exactly once \
         (proves the receiver positively resolves to its FormData type rather than \
          falling through silently as Ty::Unknown), got: {:?}",
        kinds
    );
}

#[test]
fn value_list_form_attribute_method_call_silent() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    \
        НоваяСтр = СписокЗначенийРеквизит.Добавить();\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "ValueList form-attribute method call must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}

#[test]
fn value_list_form_attribute_types_to_kernel_value_list() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    \
        Список = СписокЗначенийРеквизит;\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    assert_eq!(
        var_ty(&db, file_id, "список"),
        Some(db.value_list(None)),
        "v8:ValueListType form attribute must infer to the kernel ValueList",
    );
}

#[test]
fn untyped_form_attribute_method_call_not_resolved_as_module() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    // Designer emits an empty <Type/> for untyped form attributes. A method
    // call on such an attribute must not fall through to common-module
    // resolution: the receiver IS a form attribute, merely untyped, so no
    // UnresolvedMethodCall may surface.
    let bsl = "Процедура Тест()\n    \
        СтруктураРеквизитовФормы.Вставить(\"НастройкиВидимости\", 1);\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "method call on an untyped form attribute must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}

#[test]
fn value_tree_attribute_is_form_data_tree() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    \
        Дерево = ДеревоРазделов;\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let ty = var_ty(&db, file_id, "дерево").expect("дерево must be typed");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::FormData { kind: hir::FormDataFacet::Tree, .. }),
        "v8:ValueTree form attribute must lower to ДанныеФормыДерево, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn value_tree_find_by_id_returns_tree_item() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест()\n    \
        Строка = ДеревоРазделов.НайтиПоИдентификатору(1);\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "ДанныеФормыДерево.НайтиПоИдентификатору must resolve, got: {:?}",
        kinds
    );

    let row = var_ty(&db, file_id, "строка").expect("строка must be typed");
    let members: Vec<String> = match db.lookup_type(row) {
        TypeKind::Union(ms) => ms.iter().map(|m| format!("{:?}", db.lookup_type(*m))).collect(),
        other => vec![format!("{other:?}")],
    };
    assert!(
        members.iter().any(|m| m.contains("ДанныеФормыЭлементДерева")),
        "FindByID on a form data tree must return ДанныеФормыЭлементДерева, got {members:?}"
    );
    assert!(
        !members.iter().any(|m| m.contains("ДанныеФормыЭлементКоллекции")),
        "tree FindByID must not return a collection item, got {members:?}"
    );
}

#[test]
fn unknown_bare_receiver_still_unresolved() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    // A bare receiver that is neither declared nor a form attribute must keep
    // surfacing ReceiverNotResolved — the untyped-attribute arm is name-gated.
    let bsl = "Процедура Тест()\n    \
        НетТакогоРеквизитаИМодуля.Вставить(\"Ключ\", 1);\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let kinds = unresolved_kinds(&db, file_id);
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::ReceiverNotResolved],
        "unknown bare receiver must still be reported, got: {:?}",
        kinds
    );
}

#[test]
fn findrows_chain_no_unresolved_method_call() {
    if !has_platform_data() {
        eprintln!("Skipping: no platform data available");
        return;
    }

    let bsl = "Процедура Тест(Отбор)\n    \
        Х = Объект.НастройкиЭксель.НайтиСтроки(Отбор)[0].Значение;\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);

    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "chained НайтиСтроки result must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}
