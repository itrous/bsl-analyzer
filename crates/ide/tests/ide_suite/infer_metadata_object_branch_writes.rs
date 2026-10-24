//! A local written with objects of different kinds in the branches of `Если … ИначеЕсли` is read
//! after the branches as the type of its textually latest write. A field the other branches'
//! objects own is not a defect: the miss is reported only when every write reaching the read
//! lacks the field.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

/// The object module of a data processor of a configuration with the catalog `Справочник1`
/// (attributes `Реквизит1`..`Реквизит3`), the document `Документ1` (attribute `Реквизит1`) and the
/// information register `РегистрСведений1` (dimension `Справочник1`).
fn setup(text: &str) -> (RootDatabaseImpl, FileId) {
    let path =
        designer_fixture_path().join("DataProcessors/ТестоваяОбработка/Ext/ObjectModule.bsl");
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

fn unresolved_fields(text: &str) -> Vec<String> {
    let (db, file_id) = setup(text);
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn field_of_an_earlier_branch_object_is_not_reported() {
    let found = unresolved_fields(
        r#"Процедура Тест(Вид) Экспорт
	Если Вид = 1 Тогда
		Приемник = Справочники.Справочник1.СоздатьЭлемент();
	ИначеЕсли Вид = 2 Тогда
		Приемник = Документы.Документ1.СоздатьДокумент();
	ИначеЕсли Вид = 3 Тогда
		Приемник = РегистрыСведений.РегистрСведений1.СоздатьМенеджерЗаписи();
	КонецЕсли;
	Приемник.Реквизит2 = 1;
	Если Вид = 1 Тогда
		Х = Приемник.Реквизит3;
	КонецЕсли;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "the catalog written in the first branch has the fields: {found:?}");
}

#[test]
fn field_no_written_object_has_is_still_reported() {
    let found = unresolved_fields(
        r#"Процедура Тест(Вид) Экспорт
	Если Вид = 1 Тогда
		Приемник = Справочники.Справочник1.СоздатьЭлемент();
	Иначе
		Приемник = Документы.Документ1.СоздатьДокумент();
	КонецЕсли;
	Приемник.Реквизит1 = 1;
	Приемник.НетТакогоРеквизита = 1;

	Запись = РегистрыСведений.РегистрСведений1.СоздатьМенеджерЗаписи();
	Запись.ИТакогоНет = 1;

	Элемент = Неопределено;
	Если Вид = 1 Тогда
		Элемент = Справочники.Справочник1.СоздатьЭлемент();
	КонецЕсли;
	Элемент.НиТакого = 1;
КонецПроцедуры
"#,
    );
    assert_eq!(
        found,
        vec!["НетТакогоРеквизита".to_string(), "ИТакогоНет".to_string(), "НиТакого".to_string()]
    );
}
