use bsl_metadata::MdoType;
use hir::{
    Builders, HirDatabase, InferenceDiagnostic, MetadataKind, TypeId, TypeKernelDb, TypeKind,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn catalog_object_module_path() -> PathBuf {
    designer_fixture_path().join("Catalogs/Справочник1/Ext/ObjectModule.bsl")
}

fn common_module_path() -> PathBuf {
    designer_fixture_path().join("CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl")
}

fn task_object_module_path() -> PathBuf {
    designer_fixture_path().join("Tasks/ТестоваяЗадача/Ext/ObjectModule.bsl")
}

fn setup_at(path: PathBuf, text: &str) -> (RootDatabaseImpl, FileId) {
    let file_id = FileId::from_raw(1);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    file_set.insert(file_id, VfsPath::new(path.to_string_lossy().to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, text);
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    (db, file_id)
}

fn setup(text: &str) -> (RootDatabaseImpl, FileId) {
    setup_at(catalog_object_module_path(), text)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn assert_this_object(db: &RootDatabaseImpl, actual: TypeId, mdo_type: MdoType, name: &str) {
    assert!(
        matches!(
            db.lookup_type(actual),
            TypeKind::ThisObject { owner, .. }
                if owner.mdo_type == mdo_type && owner.name.as_str() == name
        ),
        "expected ThisObject({mdo_type:?}, {name}), got {:?}",
        db.lookup_type(actual)
    );
}

#[test]
fn infer_this_object_resolves_to_catalog_owner() {
    let text = r#"
Функция Тест()
    Э = ЭтотОбъект;
    Возврат Э;
КонецФункции
"#;
    let (db, file_id) = setup(text);
    let actual = var_ty(&db, file_id, "э").expect("э must be inferred");
    assert_this_object(&db, actual, MdoType::Catalog, "Справочник1");
}

#[test]
fn unknown_typed_assignment_does_not_hide_object_attribute_type() {
    let text = r#"
Процедура Тест(НеизвестныйПараметр)
    Реквизит1 = НеизвестныйПараметр;
    Результат = Реквизит1;
КонецПроцедуры
"#;
    let (db, file_id) = setup(text);
    let actual = var_ty(&db, file_id, "результат").expect("attribute type must reach result");
    assert!(
        matches!(db.lookup_type(actual), TypeKind::String(_)),
        "an Unknown assignment value must not replace the declared attribute type: {:?}",
        db.lookup_type(actual)
    );
}

#[test]
fn infer_this_object_english_spelling() {
    let text = r#"
Функция Test()
    T = ThisObject;
    Возврат T;
КонецФункции
"#;
    let (db, file_id) = setup(text);
    let actual = var_ty(&db, file_id, "t").expect("t must be inferred");
    assert_this_object(&db, actual, MdoType::Catalog, "Справочник1");
}

#[test]
fn infer_this_object_field_access_resolves_via_coercion() {
    let text = r#"
Функция Тест()
    Ч = ЭтотОбъект.Реквизит2;
    Возврат Ч;
КонецФункции
"#;
    let (db, file_id) = setup(text);
    assert_eq!(
        var_ty(&db, file_id, "ч"),
        Some(db.number(None, None)),
        "ЭтотОбъект.Реквизит2 must coerce to CatalogObject and resolve to Number",
    );
}

#[test]
fn infer_this_object_unknown_field_stays_unknown() {
    let text = r#"
Функция Тест()
    Х = ЭтотОбъект.НесуществующийРеквизит;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(text);
    assert_eq!(var_ty(&db, file_id, "х"), None);

    let infer = db.infer(file_id);
    let unresolved: Vec<_> = infer
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedField { receiver_ty, field_name, .. } => {
                Some((*receiver_ty, field_name.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        unresolved.len(),
        1,
        "exactly one UnresolvedField must fire on ЭтотОбъект miss, got {unresolved:?}"
    );
    let (receiver_ty, field_name) = &unresolved[0];
    assert_this_object(&db, *receiver_ty, MdoType::Catalog, "Справочник1");
    assert_eq!(field_name.as_str(), "НесуществующийРеквизит");
}

#[test]
fn infer_this_object_in_common_module_stays_unknown() {
    let text = r#"
Функция Тест() Экспорт
    Возврат ЭтотОбъект;
КонецФункции
"#;
    let (db, file_id) = setup_at(common_module_path(), text);

    let infer = db.infer(file_id);
    let has_this_object = infer
        .var_types
        .values()
        .any(|tid| matches!(db.lookup_type(*tid), TypeKind::ThisObject { .. }));
    assert!(!has_this_object, "common module must not produce Ty::ThisObject");
}

#[test]
fn infer_this_object_coercion_pins_object_kind() {
    let text = r#"
Функция Тест()
    С = ЭтотОбъект.Ссылка;
    Возврат С;
КонецФункции
"#;
    let (db, file_id) = setup(text);
    let ty = var_ty(&db, file_id, "с").expect("с must be inferred");
    assert!(
        matches!(
            db.lookup_type(ty),
            TypeKind::MetadataRef(facet)
                if facet.kind == MetadataKind::CatalogRef
                    && facet.name.as_str() == "Справочник1"
        ),
        "expected CatalogRef.Справочник1, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn infer_this_object_resolves_in_task_object_module() {
    let text = r#"
Функция Тест()
    Э = ЭтотОбъект;
    К = ЭтотОбъект.Комментарий;
    Возврат Э;
КонецФункции
"#;
    let (db, file_id) = setup_at(task_object_module_path(), text);
    let actual = var_ty(&db, file_id, "э").expect("э must be inferred");
    assert_this_object(&db, actual, MdoType::Task, "ТестоваяЗадача");
    assert_eq!(
        var_ty(&db, file_id, "к"),
        Some(db.string(None, false)),
        "ЭтотОбъект.Комментарий in TaskObject must coerce to MetadataRef and resolve to String",
    );
}
