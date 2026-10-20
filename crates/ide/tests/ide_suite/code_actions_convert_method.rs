//! `Analysis::code_actions`: `Процедура` ⇄ `Функция` on the declaration line.
//!
//! Each "offered" case carries the exact text after the edit; each "not offered" case keeps
//! a control in the same shape that is offered, so an assist that is never offered at all
//! cannot pass the negative assertions alone.

use ide::{Analysis, Assist};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use syntax::{TextRange, TextSize};
use vfs::{FileId, FileSet, VfsPath};

const FILE: FileId = FileId(0);

fn analysis(code: &str) -> Analysis {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(FILE, VfsPath::new("/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    db.set_file_source_root(FILE, SourceRootId(0));
    db.set_file_text(FILE, code);
    Analysis::from_database(db)
}

fn assists_at(code: &str, anchor: &str) -> Vec<Assist> {
    let offset = code.find(anchor).unwrap_or_else(|| panic!("anchor {anchor:?} not in source"));
    let offset = TextSize::from(offset as u32);
    analysis(code).code_actions(FILE, TextRange::empty(offset))
}

fn applied(code: &str, anchor: &str, label: &str) -> String {
    let assists = assists_at(code, anchor);
    let assist = assists
        .iter()
        .find(|assist| assist.label == label)
        .unwrap_or_else(|| panic!("{label:?} not offered at {anchor:?}: {assists:#?}"));
    let mut edits: Vec<_> = assist
        .source_change
        .edits
        .iter()
        .inspect(|file_edit| assert_eq!(file_edit.file_id, FILE))
        .flat_map(|file_edit| file_edit.edits.iter().cloned())
        .collect();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start()));
    let mut text = code.to_string();
    for edit in edits {
        text.replace_range(
            usize::from(edit.range.start())..usize::from(edit.range.end()),
            &edit.new_text,
        );
    }
    text
}

const TO_FUNCTION: &str = "Преобразовать в функцию";
const TO_PROCEDURE: &str = "Преобразовать в процедуру";

#[test]
fn procedure_becomes_function_without_a_return_added() {
    let code = "Процедура Тест(Знач А) Экспорт\n    Б = А;\nКонецПроцедуры\n";
    assert_eq!(
        applied(code, "Тест", TO_FUNCTION),
        "Функция Тест(Знач А) Экспорт\n    Б = А;\nКонецФункции\n"
    );
}

#[test]
fn function_with_bare_return_becomes_procedure() {
    let code =
        "Функция Тест()\n    Если Истина Тогда\n        Возврат;\n    КонецЕсли;\nКонецФункции\n";
    assert_eq!(
        applied(code, "Функция", TO_PROCEDURE),
        "Процедура Тест()\n    Если Истина Тогда\n        Возврат;\n    КонецЕсли;\nКонецПроцедуры\n"
    );
}

#[test]
fn function_called_only_as_a_statement_becomes_procedure() {
    let code = "Функция Записать(Данные)\n    Данные.Записать();\nКонецФункции\n\n\
                Процедура Сохранить(Данные)\n    Записать(Данные);\n    Если Истина Тогда\n        Записать(Данные);\n    КонецЕсли;\nКонецПроцедуры\n";
    assert_eq!(
        applied(code, "Записать(Данные)\n", TO_PROCEDURE),
        "Процедура Записать(Данные)\n    Данные.Записать();\nКонецПроцедуры\n\n\
         Процедура Сохранить(Данные)\n    Записать(Данные);\n    Если Истина Тогда\n        Записать(Данные);\n    КонецЕсли;\nКонецПроцедуры\n"
    );
}

/// The module with one more call to `Проверить` in `Вызов`, which uses its result.
fn with_caller(call: &str) -> String {
    format!(
        "Функция Проверить()\n    А = 1;\nКонецФункции\n\n\
         Процедура Вызов()\n    Проверить();\n    {call}\nКонецПроцедуры\n"
    )
}

#[test]
fn function_whose_result_is_used_is_not_offered() {
    for call in [
        "Итог = Проверить();",
        "Сообщить(Проверить());",
        "Итог = 1 + Проверить();",
        "Если Проверить() Тогда\n    КонецЕсли;",
    ] {
        let code = with_caller(call);
        assert!(
            assists_at(&code, "Проверить()\n    А").is_empty(),
            "offered although `{call}` uses the result"
        );
    }

    // Control: the same module where the extra call is a statement too is offered.
    let code = with_caller("Проверить();");
    assert_eq!(assists_at(&code, "Проверить()\n    А").len(), 1);
}

#[test]
fn function_whose_result_is_returned_is_not_offered() {
    let code = "Функция Проверить()\n    А = 1;\nКонецФункции\n\n\
                Функция Обертка()\n    Возврат Проверить();\nКонецФункции\n";
    assert!(assists_at(code, "Проверить()\n    А").is_empty());

    // Control: the caller itself returns a value but calls `Проверить` as a statement.
    let code = "Функция Проверить()\n    А = 1;\nКонецФункции\n\n\
                Функция Обертка()\n    Проверить();\n    Возврат 1;\nКонецФункции\n";
    assert_eq!(assists_at(code, "Проверить()\n    А").len(), 1);
}

#[test]
fn exported_function_is_not_offered() {
    // Callers in other modules are not searched, so `Экспорт` alone blocks the conversion,
    // even with no `Возврат <выражение>` and no caller in the module.
    let exported = "Функция Тест() Экспорт\n    А = 1;\nКонецФункции\n";
    assert!(assists_at(exported, "Тест").is_empty());

    // Control: the same function without `Экспорт` is offered.
    let local = "Функция Тест()\n    А = 1;\nКонецФункции\n";
    assert_eq!(assists_at(local, "Тест").len(), 1);
}

#[test]
fn function_returning_a_value_is_not_offered() {
    let returns =
        "Функция Тест()\n    Если Истина Тогда\n        Возврат 1;\n    КонецЕсли;\nКонецФункции\n";
    assert!(assists_at(returns, "Тест").is_empty(), "{:#?}", assists_at(returns, "Тест"));

    // Control: the same function with a bare return is offered.
    let bare =
        "Функция Тест()\n    Если Истина Тогда\n        Возврат;\n    КонецЕсли;\nКонецФункции\n";
    assert_eq!(assists_at(bare, "Тест").len(), 1);
}

#[test]
fn english_keywords_stay_english() {
    let procedure = "Procedure Test() Export\n    A = 1;\nEndProcedure\n";
    assert_eq!(
        applied(procedure, "Test", TO_FUNCTION),
        "Function Test() Export\n    A = 1;\nEndFunction\n"
    );

    let function = "Function Test()\n    Return;\nEndFunction\n";
    assert_eq!(
        applied(function, "Test", TO_PROCEDURE),
        "Procedure Test()\n    Return;\nEndProcedure\n"
    );

    let returns = "Function Test()\n    Return 1;\nEndFunction\n";
    assert!(assists_at(returns, "Test").is_empty());
}

#[test]
fn annotated_method_keeps_its_annotation_and_is_offered_only_on_the_declaration() {
    let code = "&НаСервере\nПроцедура Первая()\nКонецПроцедуры\n\n&НаСервере\nПроцедура Вторая() Экспорт\n    А = 1;\nКонецПроцедуры\n";
    assert_eq!(
        applied(code, "Вторая", TO_FUNCTION),
        "&НаСервере\nПроцедура Первая()\nКонецПроцедуры\n\n&НаСервере\nФункция Вторая() Экспорт\n    А = 1;\nКонецФункции\n"
    );

    // The annotation line belongs to the method node but is not its declaration.
    let annotation = code.rfind("&НаСервере").unwrap();
    let offset = TextSize::from(annotation as u32);
    assert!(analysis(code).code_actions(FILE, TextRange::empty(offset)).is_empty());
}

#[test]
fn async_procedure_keeps_async() {
    let code = "&НаКлиенте\nАсинх Процедура Тест()\n    Ждать Пауза();\nКонецПроцедуры\n";
    assert_eq!(
        applied(code, "Асинх", TO_FUNCTION),
        "&НаКлиенте\nАсинх Функция Тест()\n    Ждать Пауза();\nКонецФункции\n"
    );
}

#[test]
fn not_offered_inside_the_body_or_outside_methods() {
    let code = "Перем Модульная;\n\nПроцедура Тест()\n    А = 1;\nКонецПроцедуры\n";
    assert!(assists_at(code, "А = 1").is_empty());
    assert!(assists_at(code, "КонецПроцедуры").is_empty());
    assert!(assists_at(code, "Модульная").is_empty());

    // Control: the declaration line of the same method is offered.
    assert_eq!(assists_at(code, "Тест").len(), 1);
}

#[test]
fn incomplete_method_is_not_offered() {
    let code = "Процедура Тест()\n    А = 1;\n";
    assert!(assists_at(code, "Тест").is_empty());
}

#[test]
fn function_in_a_module_that_runs_dynamic_code_is_not_offered() {
    // The text `Вычислить` and `Выполнить` run is not known until run time, however it is
    // assembled, so any of them in the module may read the result.
    for call in [
        "Итог = Вычислить(\"Проверить()\");",
        "Выполнить(\"Итог = Проверить ();\");",
        "Итог = Вычислить(\"Провер\" + \"ить()\");",
        "Итог = Eval(Текст + \"()\");",
        "Если Истина Тогда\n        Выполнить Текст;\n    КонецЕсли;",
    ] {
        let code = with_caller(call);
        assert!(
            assists_at(&code, "Проверить()\n    А").is_empty(),
            "offered although `{call}` may call it"
        );
    }

    // Control: the call written in a string that nothing runs, and a method of an object
    // that is spelled like the global one.
    for call in [
        "Сообщить(\"Проверить()\");",
        "Запрос.Выполнить();",
        "Итог = Формула.Вычислить(\"Проверить()\");",
    ] {
        let code = with_caller(call);
        assert_eq!(assists_at(&code, "Проверить()\n    А").len(), 1, "not offered with `{call}`");
    }
}

const BASE: FileId = FileId(0);
const EXTENSION: FileId = FileId(1);

/// A common module `Общий` in the configuration and its body in an extension.
fn with_extension(base: &str, extension: &str) -> Analysis {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    file_set.insert(BASE, VfsPath::new("/base/CommonModules/Общий/Ext/Module.bsl"));
    file_set.insert(EXTENSION, VfsPath::new("/ext/CommonModules/Общий/Ext/Module.bsl"));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file, text) in [(BASE, base), (EXTENSION, extension)] {
        db.set_file_source_root(file, SourceRootId(0));
        db.set_file_text(file, text);
    }
    Analysis::from_database(db)
}

fn assists_in(analysis: &Analysis, file: FileId, code: &str, anchor: &str) -> Vec<Assist> {
    let offset = code.find(anchor).unwrap_or_else(|| panic!("anchor {anchor:?} not in source"));
    analysis.code_actions(file, TextRange::empty(TextSize::from(offset as u32)))
}

#[test]
fn method_an_extension_intercepts_is_not_offered() {
    let base = "Процедура Записать() Экспорт\n    А = 1;\nКонецПроцедуры\n\n\
                Процедура Прочитать() Экспорт\n    А = 1;\nКонецПроцедуры\n";
    for annotation in ["&После", "&Перед", "&Вместо", "&ИзменениеИКонтроль"]
    {
        let extension = format!(
            "#Область Перехват\n{annotation}(\"ЗАПИСАТЬ\")\n\
             Процедура Расш_Записать()\n    А = 2;\nКонецПроцедуры\n#КонецОбласти\n"
        );
        let analysis = with_extension(base, &extension);
        assert!(
            assists_in(&analysis, BASE, base, "Записать()").is_empty(),
            "offered although {annotation} intercepts it"
        );
        // The interceptor's own kind is bound to the method it intercepts.
        assert!(
            assists_in(&analysis, EXTENSION, &extension, "Расш_Записать").is_empty(),
            "offered on the {annotation} interceptor itself"
        );
        // Control: the method of the same module nobody intercepts is offered.
        assert_eq!(assists_in(&analysis, BASE, base, "Прочитать()").len(), 1);
    }

    // Control: a plain method of the extension body is offered.
    let extension = "Процедура Расш_Свой()\n    А = 2;\nКонецПроцедуры\n";
    let analysis = with_extension(base, extension);
    assert_eq!(assists_in(&analysis, EXTENSION, extension, "Расш_Свой").len(), 1);
    assert_eq!(assists_in(&analysis, BASE, base, "Записать()").len(), 1);
}

#[test]
fn interceptor_inside_a_preprocessor_branch_still_binds_the_method() {
    let base = "Процедура Записать() Экспорт\n    А = 1;\nКонецПроцедуры\n";
    for branch in
        ["#Если Сервер Тогда\n{}#КонецЕсли\n", "#Если Клиент Тогда\n#Иначе\n{}#КонецЕсли\n"]
    {
        let extension = branch.replace(
            "{}",
            "&Вместо(\"Записать\")\nПроцедура Расш_Записать()\n    А = 2;\nКонецПроцедуры\n",
        );
        let analysis = with_extension(base, &extension);
        assert!(
            assists_in(&analysis, BASE, base, "Записать()").is_empty(),
            "offered although an interceptor in `{branch}` binds it"
        );
    }
}

#[test]
fn function_an_extension_body_reads_is_not_offered() {
    // A method of the extension body resolves the base module's methods, `Экспорт` or not.
    let base = "Функция Проверить()\n    А = 1;\nКонецФункции\n";
    for body in [
        "    Значение = Проверить();\n",
        "    Если ПРОВЕРИТЬ() Тогда\n    КонецЕсли;\n",
        "    Итог = Вычислить(Текст);\n",
        "    Выполнить Текст;\n",
    ] {
        let extension = format!("Процедура Расш_Свой()\n{body}КонецПроцедуры\n");
        let analysis = with_extension(base, &extension);
        assert!(
            assists_in(&analysis, BASE, base, "Проверить()").is_empty(),
            "offered although the extension body has `{body}`"
        );
    }

    // Control: the extension body calls it as a statement.
    let extension = "Процедура Расш_Свой()\n    Проверить();\nКонецПроцедуры\n";
    let analysis = with_extension(base, extension);
    assert_eq!(assists_in(&analysis, BASE, base, "Проверить()").len(), 1);
}

#[test]
fn offered_anywhere_on_the_declaration_line() {
    let indented = "    Процедура Тест()\n    А = 1;\nКонецПроцедуры\n";
    let offset = TextSize::from(0);
    assert_eq!(analysis(indented).code_actions(FILE, TextRange::empty(offset)).len(), 1);

    let annotated = "&НаСервере Процедура Тест()\n    А = 1;\nКонецПроцедуры\n";
    assert_eq!(assists_at(annotated, "&НаСервере").len(), 1);

    // Control: the line before the declaration is not it.
    let code = format!("Перем А;\n{indented}");
    assert!(assists_at(&code, "Перем").is_empty());
}
