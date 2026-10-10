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
    let root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), root);
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

fn var_type(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn path_resolves_platform_builtin_via_resolver() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Х = СтрДлина("abc");
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_type(&db, file_id, "х"),
        Some(db.number(None, None)),
        "platform builtin call must type `Х` as Number (СтрДлина ret type)"
    );
}

#[test]
fn implicit_local_assignment_shadows_module_procedure() {
    let fixture = r#"//- /test.bsl
Процедура Данные() Экспорт
КонецПроцедуры

Функция Тест()
    Данные = 42;
    Рез = Данные;
    Возврат Рез;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_type(&db, file_id, "рез"),
        Some(db.number(None, None)),
        "implicit local must shadow module procedure; `Рез` should inherit Number"
    );
}

#[test]
fn implicit_local_assignment_shadows_builtin_in_value_position() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Строка = 42;
    Рез = Строка;
    Возврат Рез;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_type(&db, file_id, "рез"),
        Some(db.number(None, None)),
        "implicit local named like a builtin must win in value position"
    );
}

/// A local of unknown type owns the name: the platform steps of the bare-name
/// cascade must not hand back the same-named global. Re-typing a claimed name
/// as the global contradicts the availability diagnostic, which already treats
/// such a name as owned by the local.
#[test]
fn assignment_does_not_claim_a_manager_collection_name() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Справочники = НеизвестнаяФункция();
    Рез = Справочники;
    Возврат Рез;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    // The assignment writes to a Global-context property and declares no local, so
    // the later read is still the collection. `GlobalPropertyNotWritable` reports
    // the illegal write.
    assert_eq!(
        var_type(&db, file_id, "рез"),
        Some(db.manager_collection(bsl_metadata::MdoType::Catalog)),
        "the name still denotes the manager collection"
    );
}

#[test]
fn implicit_local_of_unknown_type_claims_platform_global_property_name() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    ОбработкаОшибок = НеизвестнаяФункция();
    Рез = ОбработкаОшибок;
    Возврат Рез;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_type(&db, file_id, "рез"),
        None,
        "a local owning the name must not be re-typed as the platform global property"
    );
}

/// Positive control for the two tests above: without a claiming local the very
/// same reads still resolve to the globals.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn unclaimed_bare_global_names_still_resolve() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Рез = Справочники;
    Рез2 = ОбработкаОшибок;
    Возврат Рез;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_type(&db, file_id, "рез"),
        Some(db.manager_collection(bsl_metadata::MdoType::Catalog)),
        "an unclaimed manager-collection name must still resolve"
    );
    assert!(
        var_type(&db, file_id, "рез2").is_some(),
        "an unclaimed platform global property must still resolve"
    );
}

/// Inside its own right-hand side the name still denotes the previous owner —
/// the claim starts only once the assignment has completed.
#[test]
fn name_still_denotes_global_inside_its_own_right_hand_side() {
    let fixture = r#"//- /test.bsl
Функция Тест()
    Справочники = Справочники;
    Рез = Справочники;
    Возврат Рез;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_type(&db, file_id, "рез"),
        Some(db.manager_collection(bsl_metadata::MdoType::Catalog)),
        "the right-hand side reads the global, so the local carries it forward"
    );
}
