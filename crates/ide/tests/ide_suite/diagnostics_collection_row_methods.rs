//! Row methods of form collections and selections must resolve without
//! `UnresolvedMethodCall`: `ПолучитьИдентификатор` on a form-collection row,
//! `НайтиПоИдентификатору` on a tabular section, `ПолучитьОбъект` on a ref,
//! `Выбрать` on a nested-table query field.

use bsl_platform::PlatformDataInner;
use hir::{
    Builders, HirDatabase, InferenceDiagnostic, MetadataKind, TypeId, TypeKernelDb, TypeKind,
    UnresolvedMethodKind,
};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::{FileId, FileSet, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn data_processor_module_path() -> PathBuf {
    designer_fixture_path().join("DataProcessors/ТестоваяОбработка/Forms/Форма/Ext/Form/Module.bsl")
}

fn document_form_module_path() -> PathBuf {
    designer_fixture_path().join("Documents/Документ1/Forms/ФормаДокумента/Ext/Form/Module.bsl")
}

fn has_platform_data() -> bool {
    !PlatformDataInner::instance().all_methods().is_empty()
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

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
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

fn unresolved(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(String, UnresolvedMethodKind)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall {
                method_name, receiver_name, kind, ..
            } => Some((format!("{} у {}", method_name.as_str(), receiver_name.as_str()), *kind)),
            _ => None,
        })
        .collect()
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

fn is_row_ref(
    db: &RootDatabaseImpl,
    ty: TypeId,
    parent: bsl_metadata::MdoType,
    name: &str,
) -> bool {
    matches!(
        db.lookup_type(ty),
        TypeKind::MetadataRef(facet)
            if facet.kind == MetadataKind::TabularSectionRow { parent }
                && facet.name.as_str() == name
    )
}

#[test]
fn form_table_current_data_get_id_returns_number() {
    if !has_platform_data() {
        return;
    }
    let bsl = "Процедура Тест()\n    \
        ТекущиеДанные = Элементы.ТабличнаяЧасть1.ТекущиеДанные;\n    \
        Ид = ТекущиеДанные.ПолучитьИдентификатор();\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(document_form_module_path(), bsl);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "ТекущиеДанные.ПолучитьИдентификатор FP: {kinds:?}");
    assert_eq!(
        var_ty(&db, file_id, "ид"),
        Some(db.number(None, None)),
        "ПолучитьИдентификатор must return Число",
    );
}

#[test]
fn form_collection_added_row_get_id() {
    if !has_platform_data() {
        return;
    }
    let bsl = "Процедура Тест()\n    \
        НоваяСтрока = Объект.НастройкиЭксель.Добавить();\n    \
        Ид = НоваяСтрока.ПолучитьИдентификатор();\n\
        КонецПроцедуры\n";
    let (db, file_id) = setup_form_module(data_processor_module_path(), bsl);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "Добавить().ПолучитьИдентификатор FP: {kinds:?}");
    assert_eq!(
        var_ty(&db, file_id, "ид"),
        Some(db.number(None, None)),
        "ПолучитьИдентификатор must return Число",
    );
}

#[test]
fn object_tabular_section_find_by_id_keeps_row_schema() {
    if !has_platform_data() {
        return;
    }
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Спр = Справочники.Справочник1.СоздатьЭлемент();
    Строка = Спр.ТабличнаяЧасть1.НайтиПоИдентификатору(1);
    Реквизит = Строка.Реквизит1;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "ТЧ.НайтиПоИдентификатору FP: {kinds:?}");
    let row = var_ty(&db, file_id, "строка").expect("строка must be typed");
    assert!(
        matches!(db.lookup_type(row), TypeKind::Union(members)
            if members.contains(&db.undefined())
                && members.iter().any(|m| is_row_ref(
                    &db,
                    *m,
                    bsl_metadata::MdoType::Catalog,
                    "Справочник1.ТабличнаяЧасть1",
                ))
        ),
        "НайтиПоИдентификатору must return the tabular-section row (or Неопределено), got {:?}",
        db.lookup_type(row),
    );
}

#[test]
fn catalog_selection_next_and_get_object() {
    if !has_platform_data() {
        return;
    }
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Выборка = Справочники.Справочник1.Выбрать();
    Пока Выборка.Следующий() Цикл
        Объект = Выборка.ПолучитьОбъект();
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "selection methods FP: {kinds:?}");
}

#[test]
fn catalog_ref_from_find_get_object() {
    if !has_platform_data() {
        return;
    }
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Ссылка = Справочники.Справочник1.НайтиПоНаименованию("а");
    Объект = Ссылка.ПолучитьОбъект();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "НайтиПоНаименованию→ПолучитьОбъект FP: {kinds:?}");
}

// A variable reassigned in a sibling branch must not poison method lookup in
// the other branch: sequential inference sees the Тогда-branch type at the
// Иначе-branch use, so lookup falls back to the other recorded assignments.
#[test]
fn branch_reassigned_var_get_object_not_flagged() {
    if !has_platform_data() {
        return;
    }
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Папка = Справочники.Справочник1.НайтиПоКоду("01", Истина);
    Если НЕ ЗначениеЗаполнено(Папка) Тогда
        Папка = Справочники.Справочник1.СоздатьГруппу();
        Папка.Записать();
    Иначе
        Папка = Папка.ПолучитьОбъект();
        Папка.Записать();
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "cross-branch ПолучитьОбъект FP: {kinds:?}");
}

// The cross-branch suppression must not swallow a real error when the stale
// assignment is on the same straight-line path: after an unconditional
// reassignment only the last type reaches the call.
#[test]
fn straight_line_reassignment_keeps_diagnostic() {
    if !has_platform_data() {
        return;
    }
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Об = Справочники.Справочник1.СоздатьЭлемент();
    Об = Справочники.Справочник1.НайтиПоКоду("01");
    Об.Записать();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds = unresolved(&db, file_id);
    assert_eq!(
        kinds.len(),
        1,
        "Записать у СправочникСсылка must stay flagged after straight-line reassignment, got {kinds:?}",
    );
}

#[test]
fn query_selection_nested_table_select() {
    if !has_platform_data() {
        return;
    }
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст =
        "ВЫБРАТЬ
        |   Справочник1.Ссылка КАК Ссылка,
        |   Справочник1.ТабличнаяЧасть1.(
        |       Реквизит1 КАК Реквизит1) КАК ТабличнаяЧасть1
        |ИЗ
        |   Справочник.Справочник1 КАК Справочник1";
    Выборка = Запрос.Выполнить().Выбрать();
    Пока Выборка.Следующий() Цикл
        Вложенная = Выборка.ТабличнаяЧасть1.Выбрать();
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "nested selection Выбрать FP: {kinds:?}");
}
