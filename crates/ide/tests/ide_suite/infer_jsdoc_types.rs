use hir::{
    Builders, DefDatabase, HirDatabase, MetadataKind, ModuleId, TypeId, TypeKernelDb, TypeKind,
};
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

fn assert_metadata_ref(
    db: &RootDatabaseImpl,
    actual: Option<TypeId>,
    kind: MetadataKind,
    name: &str,
) {
    let actual = actual.expect("expected metadata ref type");
    assert!(
        matches!(
            db.lookup_type(actual),
            TypeKind::MetadataRef(facet)
                if facet.kind == kind && facet.name.as_str() == name
        ),
        "expected MetadataRef({kind:?}, {name}), got {:?}",
        db.lookup_type(actual)
    );
}

#[test]
fn jsdoc_return_type_primitive_flows_into_var_types() {
    let fixture = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
// Возвращаемое значение:
//   Строка - имя реквизита
Функция Имя() Экспорт
    Возврат "";
КонецФункции

//- /test.bsl
Функция Тест()
    Х = ОбщегоНазначения.Имя();
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.string(None, false)),
        "JSDoc `Возвращаемое значение: Строка` must lower into Ty::String"
    );
}

#[test]
fn jsdoc_catalog_ref_return_lowers_to_metadata_ref() {
    let fixture = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Номенклатура - ссылка на номенклатуру
Функция ПолучитьСсылку() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    Ссылка = ОбщегоНазначения.ПолучитьСсылку();
    Возврат Ссылка;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_metadata_ref(
        &db,
        var_ty(&db, file_id, "ссылка"),
        MetadataKind::CatalogRef,
        "Номенклатура",
    );
}

#[test]
fn missing_jsdoc_surfaces_body_inferred_return() {
    let fixture = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция БезКомментария() Экспорт
    Возврат Истина;
КонецФункции

//- /test.bsl
Функция Тест()
    Х = ОбщегоНазначения.БезКомментария();
    Возврат Х;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        var_ty(&db, file_id, "х"),
        Some(db.boolean()),
        "cascade typing must surface `Возврат Истина` as Ty::Boolean through `materialise_signature_enriched`"
    );
    let infer = db.infer(file_id);
    let unresolved: Vec<_> = infer
        .diagnostics
        .iter()
        .filter(|(_, d)| matches!(d, hir::InferenceDiagnostic::UnresolvedMethodCall { .. }))
        .collect();
    assert!(unresolved.is_empty(), "method must resolve even without JSDoc");
}

#[test]
fn jsdoc_union_return_lowers_to_ty_union() {
    let fixture = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
// Возвращаемое значение:
//   Число, Строка - результат
Функция Результат() Экспорт
    Возврат "";
КонецФункции

//- /test.bsl
Функция Тест()
    Р = ОбщегоНазначения.Результат();
    Возврат Р;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    let ty = var_ty(&db, file_id, "р").expect("var_types must track union return");
    match db.lookup_type(ty) {
        TypeKind::Union(parts) => {
            assert_eq!(parts.len(), 2, "Union should have exactly 2 members");
            assert!(parts.contains(&db.number(None, None)));
            assert!(parts.contains(&db.string(None, false)));
        }
        other => panic!("expected Ty::Union, got {other:?}"),
    }
}

#[test]
fn jsdoc_three_level_return_lowers_through_manager_chain() {
    let fixture = r#"
//- /Documents/ПКО/Ext/ManagerModule.bsl
// Возвращаемое значение:
//   ДокументСсылка.ПКО - ссылка на документ
Функция ПолучитьСсылку() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    Ссылка = Документы.ПКО.ПолучитьСсылку();
    Возврат Ссылка;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert_metadata_ref(&db, var_ty(&db, file_id, "ссылка"), MetadataKind::DocumentRef, "ПКО");
}
