use bsl_metadata::MdoType;
use hir::{Builders, DefDatabase, HirDatabase, ModuleId, TypeId};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

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
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    let _ = db.module_bodies(ModuleId::new(test_file));
    (db, test_file)
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
fn bare_documenty_yields_manager_collection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    М = Документы;
    Возврат М;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "м"),
        Some(db.manager_collection(MdoType::Document)),
        "`Документы` must lower to Ty::ManagerCollection(Document)"
    );
}

#[test]
fn bare_spravochniki_yields_manager_collection() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    С = Справочники;
    Возврат С;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "с"),
        Some(db.manager_collection(MdoType::Catalog)),
        "`Справочники` must lower to Ty::ManagerCollection(Catalog)"
    );
}

#[test]
fn assignment_does_not_shadow_a_manager_plural() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Документы = 42;
    М = Документы;
    Возврат М;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    // `Документы = 42` writes to a Global-context property and declares no local, so
    // the read is still the collection; the illegal write is reported separately by
    // `GlobalPropertyNotWritable`.
    assert_eq!(
        var_ty(&db, file_id, "м"),
        Some(db.manager_collection(bsl_metadata::MdoType::Document)),
        "the name still denotes the manager collective"
    );
}
