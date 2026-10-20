use bsl_metadata::MdoType;
use hir::{HirDatabase, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }

    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
fn infer_manager_collection_promotes_to_object_manager() {
    let fixture = r#"
//- /test.bsl
Функция Тест()
    М = Справочники.Справочник1;
    Возврат М;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "м").expect("M must carry a type");
    assert!(
        matches!(
            db.lookup_type(ty),
            TypeKind::ObjectManager(facet)
                if facet.mdo == MdoType::Catalog && facet.name.as_str() == "Справочник1"
        ),
        "expected ObjectManager(Catalog, Справочник1), got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn infer_manager_collection_with_unknown_mdo_name_stays_unknown() {
    let fixture = r#"
//- /test.bsl
Функция Тест()
    М = Справочники.НеСуществует;
    Возврат М;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(var_ty(&db, file_id, "м"), None, "unknown MDO must stay Unknown");
}

#[test]
fn infer_manager_chain_catalog_ref_attribute() {
    let fixture = r#"
//- /test.bsl
Функция Тест()
    М = Справочники.Справочник1;
    Возврат М;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "м").expect("M must carry a type");
    match db.lookup_type(ty) {
        TypeKind::ObjectManager(facet) => {
            assert_eq!(facet.mdo, MdoType::Catalog);
            assert_eq!(facet.name.as_str(), "Справочник1");
        }
        other => panic!("expected ObjectManager(Catalog, Справочник1), got {other:?}"),
    }
}
