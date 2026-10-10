//! A local re-bound from a value inference cannot type stops reading as the type an earlier
//! write gave it: the walk used to keep that type, and every member check made through
//! the local judged a value it no longer held.

use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use vfs::{FileId, VfsPath};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

/// The object module of a data processor whose tabular section `НастройкиЭксель` has the
/// columns `Значение` and `Активна`.
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
    db.set_all_config_paths(vec![(None, designer_fixture_path())]);
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
fn loop_over_an_untyped_collection_drops_the_previous_row_type() {
    // The shape of a report module applying parameters passed by the caller: the first
    // loop walks the report's tabular section, the next ones reuse the variable `Строка`
    // over a `Соответствие`/`Структура` taken from `Параметры[...]`, whose type inference
    // cannot know. The pairs have `Ключ`/`Значение`; the tabular-section row has no `Ключ`.
    let found = unresolved_fields(
        r#"Процедура ПрименитьПараметры(Параметры, Поля) Экспорт
	Для Каждого Строка Из НастройкиЭксель Цикл
		Строка.Активна = Истина;
	КонецЦикла;

	ВыбранныеПоля = Параметры["ВыбранныеПоля"];
	Если ТипЗнч(ВыбранныеПоля) = Тип("Соответствие")
		ИЛИ ТипЗнч(ВыбранныеПоля) = Тип("Структура") Тогда
		Для каждого Строка Из ВыбранныеПоля Цикл
			Поля.Добавить(Строка.Ключ);
		КонецЦикла;
	КонецЕсли;

	СтрокиОтбора = Параметры["Отбор"];
	Для каждого Строка Из СтрокиОтбора Цикл
		Если Строка.Ключ = "" Тогда
			Поля.Добавить(Строка.ВидСравнения);
		КонецЕсли;
	КонецЦикла;
КонецПроцедуры
"#,
    );
    assert!(
        found.is_empty(),
        "the rows of the later loops are not tabular-section rows: {found:?}"
    );
}

#[test]
fn assignment_of_an_untyped_value_drops_the_previous_type() {
    let found = unresolved_fields(
        r#"Процедура Тест(Параметры)
	Запись = НастройкиЭксель.Добавить();
	Запись = Параметры["Запись"];
	Ключ = Запись.Ключ;
КонецПроцедуры
"#,
    );
    assert!(found.is_empty(), "the local no longer holds a tabular-section row: {found:?}");
}

#[test]
fn a_typed_write_after_the_reset_is_checked_again() {
    // The reset lasts until the next typed write: a later loop over the tabular section
    // types the variable again, and a column the section lacks is still reported.
    let found = unresolved_fields(
        r#"Процедура Тест(Параметры)
	Для каждого Строка Из Параметры["Отбор"] Цикл
		Ключ = Строка.Ключ;
	КонецЦикла;
	Для Каждого Строка Из НастройкиЭксель Цикл
		Нет = Строка.НетТакойКолонки;
	КонецЦикла;
	Запись = Параметры["Запись"];
	Запись = НастройкиЭксель.Добавить();
	Тоже = Запись.ИТакойНет;
КонецПроцедуры
"#,
    );
    assert_eq!(found, vec!["НетТакойКолонки".to_string(), "ИТакойНет".to_string()]);
}
