use bsl_metadata::MdoType;
use hir::{HirDatabase, InferenceDiagnostic, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn catalog_manager_module_path() -> PathBuf {
    designer_fixture_path().join("Catalogs/Справочник1/Ext/ManagerModule.bsl")
}

fn register_manager_module_path() -> PathBuf {
    designer_fixture_path().join("InformationRegisters/РегистрСведений1/Ext/ManagerModule.bsl")
}

fn common_module_path() -> PathBuf {
    designer_fixture_path().join("CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl")
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

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn assert_this_manager(db: &RootDatabaseImpl, actual: TypeId, mdo_type: MdoType, name: &str) {
    assert!(
        matches!(
            db.lookup_type(actual),
            TypeKind::ThisManager { owner, .. }
                if owner.mdo_type == mdo_type && owner.name.as_str() == name
        ),
        "expected ThisManager({mdo_type:?}, {name}), got {:?}",
        db.lookup_type(actual)
    );
}

#[test]
fn infer_this_manager_resolves_to_catalog_owner() {
    let text = r#"
Функция Тест()
    Э = ЭтотОбъект;
    Возврат Э;
КонецФункции
"#;
    let (db, file_id) = setup_at(catalog_manager_module_path(), text);
    let actual = var_ty(&db, file_id, "э").expect("э must be inferred");
    assert_this_manager(&db, actual, MdoType::Catalog, "Справочник1");
}

#[test]
fn infer_this_manager_english_spelling() {
    let text = r#"
Функция Test()
    T = ThisObject;
    Возврат T;
КонецФункции
"#;
    let (db, file_id) = setup_at(catalog_manager_module_path(), text);
    let actual = var_ty(&db, file_id, "t").expect("t must be inferred");
    assert_this_manager(&db, actual, MdoType::Catalog, "Справочник1");
}

#[test]
fn infer_this_manager_resolves_in_information_register_module() {
    let text = r#"
Функция Тест()
    Э = ЭтотОбъект;
    Возврат Э;
КонецФункции
"#;
    let (db, file_id) = setup_at(register_manager_module_path(), text);
    let actual = var_ty(&db, file_id, "э").expect("э must be inferred");
    assert_this_manager(&db, actual, MdoType::InformationRegister, "РегистрСведений1");
}

#[test]
fn infer_this_manager_in_common_module_stays_unknown() {
    let text = r#"
Функция Тест() Экспорт
    Возврат ЭтотОбъект;
КонецФункции
"#;
    let (db, file_id) = setup_at(common_module_path(), text);

    let infer = db.infer(file_id);
    let has_this_manager = infer
        .var_types
        .values()
        .any(|tid| matches!(db.lookup_type(*tid), TypeKind::ThisManager { .. }));
    assert!(!has_this_manager, "common module must not produce Ty::ThisManager");
    let has_this_object = infer
        .var_types
        .values()
        .any(|tid| matches!(db.lookup_type(*tid), TypeKind::ThisObject { .. }));
    assert!(!has_this_object, "common module must not produce Ty::ThisObject either");
}

#[test]
fn infer_this_manager_unknown_field_does_not_escalate_to_unresolved_field() {
    let text = r#"
Функция Тест()
    Х = ЭтотОбъект.НесуществующееПоле;
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup_at(catalog_manager_module_path(), text);

    let infer = db.infer(file_id);
    let unresolved_count = infer
        .diagnostics
        .iter()
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::UnresolvedField { .. }))
        .count();
    assert_eq!(
        unresolved_count, 0,
        "ManagerModule's ЭтотОбъект.<missing field> must NOT escalate to \
         UnresolvedField — see Step J docs in `field_lookup.rs` for the boundary"
    );
}
