//! Quick fixes of `DataExchangeLoading` (insert the `ОбменДанными.Загрузка` guard)
//! and `UnusedLocalMethod` (delete the method), through the public pipeline
//! (`file_diagnostics`). Each case prints the fix label, its `fix_all` flag and the
//! source after the fix, or `no fix` when the diagnostic must stay without one.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, TextEdit};
use vfs::{FileId, FileSet, VfsPath};

const OBJECT_MODULE: &str = "/test/Catalogs/Товары/Ext/ObjectModule.bsl";
const COMMON_MODULE: &str = "/test/CommonModules/Сервис/Ext/Module.bsl";

fn diagnostics_at(path: &str, source: &str) -> Vec<Diagnostic> {
    let mut db = RootDatabaseImpl::new();
    let file_id = FileId(0);
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new(path.to_string()));
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));
    db.set_file_text(file_id, source);
    ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
}

fn apply(source: &str, edits: &[TextEdit]) -> String {
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|edit| edit.range.start());
    let mut out = String::new();
    let mut cursor = 0usize;
    for edit in sorted {
        out.push_str(&source[cursor..usize::from(edit.range.start())]);
        out.push_str(&edit.new_text);
        cursor = usize::from(edit.range.end());
    }
    out.push_str(&source[cursor..]);
    out
}

fn line_col(source: &str, offset: usize) -> (usize, usize) {
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, col)
}

/// Every diagnostic of `code` with its fixes applied one at a time.
fn fixes_of(path: &str, source: &str, code: DiagnosticCode) -> String {
    let diagnostics = diagnostics_at(path, source);
    let mut out = Vec::new();
    for diagnostic in diagnostics.iter().filter(|d| d.code == code) {
        let (line, col) = line_col(source, usize::from(diagnostic.range.start()));
        let head = format!("{} @ {line}:{col}", code.as_str());
        if diagnostic.fixes.is_empty() {
            out.push(format!("{head} no fix"));
        }
        for fix in &diagnostic.fixes {
            out.push(format!(
                "{head} {} [fix_all={}]\n{}",
                fix.label,
                fix.safe_for_fix_all,
                apply(source, &fix.edits)
            ));
        }
    }
    out.join("\n---\n")
}

fn guard_fix(source: &str) -> String {
    fixes_of(OBJECT_MODULE, source, DiagnosticCode::DataExchangeLoading)
}

fn delete_fix(source: &str) -> String {
    fixes_of(COMMON_MODULE, source, DiagnosticCode::UnusedLocalMethod)
}

// --- DataExchangeLoading ------------------------------------------------------------

#[test]
fn guard_goes_after_local_variables_with_tab_indent() {
    let source = "Процедура ПередЗаписью(Отказ)\n\tПерем Итог;\n\tИтог = 1;\nКонецПроцедуры\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку ОбменДанными.Загрузка [fix_all=false]\n\
         Процедура ПередЗаписью(Отказ)\n\
         \tПерем Итог;\n\
         \tЕсли ОбменДанными.Загрузка Тогда\n\
         \t\tВозврат;\n\
         \tКонецЕсли;\n\
         \tИтог = 1;\n\
         КонецПроцедуры\n"
    );
}

#[test]
fn guard_follows_space_indent_and_leading_comment() {
    let source =
        "Процедура ПриЗаписи(Отказ)\n    // Пересчёт итогов.\n    Пересчитать();\nКонецПроцедуры\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку ОбменДанными.Загрузка [fix_all=false]\n\
         Процедура ПриЗаписи(Отказ)\n    // Пересчёт итогов.\n    \
         Если ОбменДанными.Загрузка Тогда\n        Возврат;\n    КонецЕсли;\n    \
         Пересчитать();\nКонецПроцедуры\n"
    );
}

#[test]
fn english_handler_gets_english_guard() {
    let source = "Procedure BeforeWrite(Cancel)\n    Total = 1;\nEndProcedure\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку DataExchange.Load [fix_all=false]\n\
         Procedure BeforeWrite(Cancel)\n    If DataExchange.Load Then\n        Return;\n    \
         EndIf;\n    Total = 1;\nEndProcedure\n"
    );
}

#[test]
fn empty_handler_gets_a_tab_indented_guard() {
    let source = "Процедура ПередУдалением(Отказ)\nКонецПроцедуры\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку ОбменДанными.Загрузка [fix_all=false]\n\
         Процедура ПередУдалением(Отказ)\n\tЕсли ОбменДанными.Загрузка Тогда\n\t\tВозврат;\n\
         \tКонецЕсли;\nКонецПроцедуры\n"
    );
}

#[test]
fn guard_keeps_crlf_line_breaks() {
    let source = "Процедура ПриЗаписи(Отказ)\r\n\tЗаписать();\r\nКонецПроцедуры\r\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку ОбменДанными.Загрузка [fix_all=false]\n\
         Процедура ПриЗаписи(Отказ)\r\n\tЕсли ОбменДанными.Загрузка Тогда\r\n\t\tВозврат;\r\n\
         \tКонецЕсли;\r\n\tЗаписать();\r\nКонецПроцедуры\r\n"
    );
}

#[test]
fn guarded_handler_gets_no_diagnostic() {
    let source =
        "Процедура ПередЗаписью(Отказ)\n\tЕсли ОбменДанными.Загрузка Тогда\n\t\tВозврат;\n\
                  \tКонецЕсли;\n\tИтог = 1;\nКонецПроцедуры\n";
    assert_eq!(guard_fix(source), "");
}

// --- UnusedLocalMethod --------------------------------------------------------------

#[test]
fn dead_method_goes_with_its_doc_comment_directive_and_one_blank_line() {
    let source = "Процедура Используемая() Экспорт\n\tВспомогательная();\nКонецПроцедуры\n\n\
                  // Удвоенное значение.\n//\n// Параметры:\n//  Значение - Число\n\
                  &НаСервере\nФункция Удвоить(Значение) // устарело\n\tВозврат Значение * 2;\n\
                  КонецФункции // Удвоить\n\n\
                  Процедура Вспомогательная()\nКонецПроцедуры\n";
    assert_eq!(
        delete_fix(source),
        "UnusedLocalMethod @ 10:9 Удалить неиспользуемый метод \"Удвоить\" [fix_all=false]\n\
         Процедура Используемая() Экспорт\n\tВспомогательная();\nКонецПроцедуры\n\n\
         Процедура Вспомогательная()\nКонецПроцедуры\n"
    );
}

#[test]
fn dead_method_without_blank_line_above_keeps_the_one_below() {
    let source =
        "Процедура Первая() Экспорт\nКонецПроцедуры\nПроцедура Лишняя()\nКонецПроцедуры\n\n\
                  Процедура Вторая() Экспорт\nКонецПроцедуры\n";
    assert_eq!(
        delete_fix(source),
        "UnusedLocalMethod @ 3:11 Удалить неиспользуемый метод \"Лишняя\" [fix_all=false]\n\
         Процедура Первая() Экспорт\nКонецПроцедуры\n\n\
         Процедура Вторая() Экспорт\nКонецПроцедуры\n"
    );
}

#[test]
fn dead_method_is_deleted_with_crlf_line_breaks() {
    let source = "Процедура Первая() Экспорт\r\nКонецПроцедуры\r\n\r\n// Не нужна.\r\n\
                  Процедура Лишняя()\r\nКонецПроцедуры\r\n\r\n\
                  Процедура Вторая() Экспорт\r\nКонецПроцедуры\r\n";
    assert_eq!(
        delete_fix(source),
        "UnusedLocalMethod @ 5:11 Удалить неиспользуемый метод \"Лишняя\" [fix_all=false]\n\
         Процедура Первая() Экспорт\r\nКонецПроцедуры\r\n\r\n\
         Процедура Вторая() Экспорт\r\nКонецПроцедуры\r\n"
    );
}

#[test]
fn english_dead_function_is_deleted() {
    let source = "Procedure Main() Export\nEndProcedure\n\n// Helper.\nFunction Helper()\n    \
                  Return 1;\nEndFunction\n";
    assert_eq!(
        delete_fix(source),
        "UnusedLocalMethod @ 5:10 Удалить неиспользуемый метод \"Helper\" [fix_all=false]\n\
         Procedure Main() Export\nEndProcedure\n\n"
    );
}

#[test]
fn method_named_in_a_string_literal_gets_no_deletion_fix() {
    let source = "Процедура Запуск() Экспорт\n\tВыполнить(\"Пересчитать()\");\nКонецПроцедуры\n\n\
                  Процедура Пересчитать()\nКонецПроцедуры\n";
    assert_eq!(delete_fix(source), "UnusedLocalMethod @ 5:11 no fix");
}

#[test]
fn a_longer_name_in_a_string_does_not_block_the_fix() {
    let source = "Процедура Запуск() Экспорт\n\tСообщить(\"ПересчитатьВсё\");\nКонецПроцедуры\n\n\
                  Процедура Пересчитать()\nКонецПроцедуры\n";
    assert_eq!(
        delete_fix(source),
        "UnusedLocalMethod @ 5:11 Удалить неиспользуемый метод \"Пересчитать\" [fix_all=false]\n\
         Процедура Запуск() Экспорт\n\tСообщить(\"ПересчитатьВсё\");\nКонецПроцедуры\n\n"
    );
}

#[test]
fn notification_handler_gets_no_diagnostic() {
    let source = "Процедура Спросить() Экспорт\n\
                  \tОповещение = Новый ОписаниеОповещения(\"ПослеОтвета\", Сервис);\n\
                  \tПоказатьВопрос(Оповещение, \"Продолжить?\", РежимДиалогаВопрос.ДаНет);\n\
                  КонецПроцедуры\n\n\
                  Процедура ПослеОтвета(Ответ, Параметры)\nКонецПроцедуры\n";
    assert_eq!(delete_fix(source), "");
}

#[test]
fn guard_goes_after_local_variables_inside_a_leading_region() {
    let source =
        "Процедура ПередЗаписью(Отказ)\n\t#Область Служебная\n\tПерем Итог;\n\tИтог = 1;\n\
                  \t#КонецОбласти\nКонецПроцедуры\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку ОбменДанными.Загрузка [fix_all=false]\n\
         Процедура ПередЗаписью(Отказ)\n\t#Область Служебная\n\tПерем Итог;\n\
         \tЕсли ОбменДанными.Загрузка Тогда\n\t\tВозврат;\n\tКонецЕсли;\n\
         \tИтог = 1;\n\t#КонецОбласти\nКонецПроцедуры\n"
    );
}

#[test]
fn declarations_sharing_a_directive_with_statements_get_no_guard_fix() {
    let source = "Процедура ПередЗаписью(Отказ)\n#Если Сервер Тогда\n\tПерем Итог;\n\tИтог = 1;\n\
                  #КонецЕсли\nКонецПроцедуры\n";
    assert_eq!(guard_fix(source), "DataExchangeLoading @ 1:11 no fix");
}

#[test]
fn dead_method_on_the_first_line_of_a_bom_file_is_deleted_with_its_doc_comment() {
    let source = "\u{FEFF}// Док Лишней\nПроцедура Лишняя()\nКонецПроцедуры\n\n\
                  Процедура Вторая() Экспорт\nКонецПроцедуры\n";
    assert_eq!(
        delete_fix(source),
        "UnusedLocalMethod @ 2:11 Удалить неиспользуемый метод \"Лишняя\" [fix_all=false]\n\
         \u{FEFF}Процедура Вторая() Экспорт\nКонецПроцедуры\n"
    );
    let source = "\u{FEFF}Процедура Лишняя()\nКонецПроцедуры\n\nПроцедура Вторая() Экспорт\nКонецПроцедуры\n";
    assert_eq!(
        delete_fix(source),
        "UnusedLocalMethod @ 1:12 Удалить неиспользуемый метод \"Лишняя\" [fix_all=false]\n\
         \u{FEFF}Процедура Вторая() Экспорт\nКонецПроцедуры\n"
    );
}

#[test]
fn guard_goes_after_a_directive_wrapping_only_local_variables() {
    let source = "Процедура ПередЗаписью(Отказ)\n#Если Сервер Тогда\n\tПерем Итог;\n#КонецЕсли\n\
                  \tИтог = 1;\nКонецПроцедуры\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку ОбменДанными.Загрузка [fix_all=false]\n\
         Процедура ПередЗаписью(Отказ)\n#Если Сервер Тогда\n\tПерем Итог;\n#КонецЕсли\n\
         \tЕсли ОбменДанными.Загрузка Тогда\n\t\tВозврат;\n\tКонецЕсли;\n\
         \tИтог = 1;\nКонецПроцедуры\n"
    );
}

#[test]
fn guard_stays_unconditional_before_a_leading_directive_without_declarations() {
    let source = "Процедура ПередЗаписью(Отказ)\n#Если Сервер Тогда\n\tИтог = 1;\n#КонецЕсли\n\
                  КонецПроцедуры\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку ОбменДанными.Загрузка [fix_all=false]\n\
         Процедура ПередЗаписью(Отказ)\nЕсли ОбменДанными.Загрузка Тогда\n\tВозврат;\n\
         КонецЕсли;\n#Если Сервер Тогда\n\tИтог = 1;\n#КонецЕсли\nКонецПроцедуры\n"
    );
}

#[test]
fn declarations_sharing_a_directive_with_statements_get_no_guard_fix_before_a_later_statement() {
    let source = "Процедура ПередЗаписью(Отказ)\n#Если Сервер Тогда\n\tПерем Итог;\n\tИтог = 1;\n\
                  #КонецЕсли\n\tЗаписать();\nКонецПроцедуры\n";
    assert_eq!(guard_fix(source), "DataExchangeLoading @ 1:11 no fix");
}

#[test]
fn guard_goes_before_a_block_holding_a_nested_local_variable() {
    let source = "Процедура ПередЗаписью(Отказ)\n\tЕсли Истина Тогда\n\t\tПерем Л;\n\t\tЛ = 1;\n\
                  \tКонецЕсли;\n\tСообщить(\"x\");\nКонецПроцедуры\n";
    assert_eq!(
        guard_fix(source),
        "DataExchangeLoading @ 1:11 Добавить проверку ОбменДанными.Загрузка [fix_all=false]\n\
         Процедура ПередЗаписью(Отказ)\n\tЕсли ОбменДанными.Загрузка Тогда\n\t\tВозврат;\n\
         \tКонецЕсли;\n\tЕсли Истина Тогда\n\t\tПерем Л;\n\t\tЛ = 1;\n\tКонецЕсли;\n\
         \tСообщить(\"x\");\nКонецПроцедуры\n"
    );
}
