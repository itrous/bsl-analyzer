use hir::{Builders, HirDatabase, MetadataKind, TypeId, TypeKernelDb, TypeKind};
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
fn indexing_typed_array_of_string_returns_string() {
    let (db, file_id) = setup(
        r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
// Возвращаемое значение:
//   Массив из Строка - элементы коллекции
Функция Получить() Экспорт
    Возврат Новый Массив;
КонецФункции

//- /test.bsl
Процедура Тест()
    Массив = ОбщегоНазначения.Получить();
    Элемент = Массив[0];
КонецПроцедуры
"#,
    );

    let ty = var_ty(&db, file_id, "элемент");
    assert_eq!(
        ty,
        Some(db.string(None, false)),
        "TypedArray(String)[i] must yield String, got {:?}",
        ty
    );
}

#[test]
fn indexing_typed_array_of_metadata_ref_returns_ref() {
    let (db, file_id) = setup(
        r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары - подобранная номенклатура
Функция Получить() Экспорт
    Возврат Новый Массив;
КонецФункции

//- /test.bsl
Процедура Тест()
    Массив = ОбщегоНазначения.Получить();
    Элемент = Массив[0];
КонецПроцедуры
"#,
    );

    let ty = var_ty(&db, file_id, "элемент");
    assert_metadata_ref(&db, ty, MetadataKind::CatalogRef, "Товары");
}

#[test]
fn indexing_unparameterised_array_stays_unknown() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    Массив = Новый Массив;
    Массив.Добавить(1);
    Элемент = Массив[0];
КонецПроцедуры
"#,
    );

    let actual = var_ty(&db, file_id, "элемент");
    assert!(
        actual.is_none_or(|ty| matches!(db.lookup_type(ty), TypeKind::Unknown)),
        "bare Новый Массив indexing must not invent an element type"
    );
}
