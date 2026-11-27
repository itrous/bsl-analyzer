//! Quick fix of `UnusedParameters`: remove the parameter and the argument at its position
//! from every call of the method in the module, through the public pipeline
//! (`file_diagnostics`). Each case prints the fix label, its `fix_all` flag and the source
//! after the fix, or `no fix` when the diagnostic must stay without one. Every "no fix"
//! case keeps a control of the same shape that does get the fix, so a fix that is never
//! offered cannot pass the negative cases alone.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{Diagnostic, DiagnosticCode, DiagnosticsConfig, TextEdit};
use vfs::{FileId, FileSet, VfsPath};

const COMMON_MODULE: &str = "/test/CommonModules/Сервис/Ext/Module.bsl";
const OBJECT_MODULE: &str = "/test/Catalogs/Товары/Ext/ObjectModule.bsl";
const BASE_MODULE: &str = "/base/CommonModules/Общий/Ext/Module.bsl";
const EXTENSION_MODULE: &str = "/ext/CommonModules/Общий/Ext/Module.bsl";

/// Diagnostics of the first file, analysed together with the rest in one source root.
fn diagnostics_of(files: &[(&str, &str)]) -> Vec<Diagnostic> {
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (index, (path, _)) in files.iter().enumerate() {
        file_set.insert(FileId(index as u32), VfsPath::new(path.to_string()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (index, (_, source)) in files.iter().enumerate() {
        db.set_file_source_root(FileId(index as u32), SourceRootId(0));
        db.set_file_text(FileId(index as u32), source);
    }
    ide_diagnostics::file_diagnostics(&db, FileId(0), &DiagnosticsConfig::all_enabled())
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

/// Every `UnusedParameters` diagnostic of the first file with its fixes applied one at a
/// time.
fn fixes_in(files: &[(&str, &str)]) -> String {
    let source = files[0].1;
    let diagnostics = diagnostics_of(files);
    let mut out = Vec::new();
    for diagnostic in diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnusedParameters) {
        let (line, col) = line_col(source, usize::from(diagnostic.range.start()));
        let head = format!("UnusedParameters @ {line}:{col}");
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

fn fixes(path: &str, source: &str) -> String {
    fixes_in(&[(path, source)])
}

#[test]
fn greek_sigma_method_name_call_gets_parameter_quick_fix() {
    let source = "Процедура ΟΣ(Лишний)\n    Сообщить(\"тело\");\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    οσ(1);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:14 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]\n\
         Процедура ΟΣ()\n    Сообщить(\"тело\");\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    οσ();\nКонецПроцедуры\n"
    );
}

#[test]
fn greek_sigma_parameter_gets_removed_by_quick_fix() {
    let source = "Процедура Метод(ΟΣ)\n    Сообщить(\"тело\");\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Метод(1);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:17 Удалить неиспользуемый параметр \"ΟΣ\" и его аргумент в вызовах [fix_all=false]\n\
         Процедура Метод()\n    Сообщить(\"тело\");\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Метод();\nКонецПроцедуры\n"
    );
}

#[test]
fn greek_sigma_method_name_string_prevents_parameter_quick_fix() {
    let source = "Процедура ΟΣ(Лишний)\n    Сообщить(\"οσ\");\nКонецПроцедуры\n";
    assert_eq!(fixes(COMMON_MODULE, source), "UnusedParameters @ 1:14 no fix");
}

#[test]
fn last_parameter_goes_with_the_argument_of_each_call() {
    let source = "Процедура Записать(Данные, Лишний)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Записать(1, 2);\n    Записать(3, \"а\");\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:28 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]\n\
         Процедура Записать(Данные)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
         Процедура Обработать()\n    Записать(1);\n    Записать(3);\nКонецПроцедуры\n"
    );
}

#[test]
fn middle_and_first_parameters_keep_the_commas_right() {
    let source = "Процедура Сложить(А, Лишний, В)\n    Сообщить(А + В);\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Сложить(1, 2, 3);\n    Сложить(1, , 3);\n    Сложить(1,2,3);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:22 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]\n\
         Процедура Сложить(А, В)\n    Сообщить(А + В);\nКонецПроцедуры\n\n\
         Процедура Обработать()\n    Сложить(1, 3);\n    Сложить(1, 3);\n    Сложить(1,3);\nКонецПроцедуры\n"
    );

    let source = "Процедура Показать(Лишний, А)\n    Сообщить(А);\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Показать(1, 2);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:20 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]\n\
         Процедура Показать(А)\n    Сообщить(А);\nКонецПроцедуры\n\n\
         Процедура Обработать()\n    Показать(2);\nКонецПроцедуры\n"
    );
}

#[test]
fn only_parameter_leaves_empty_parentheses() {
    let source = "Функция Итог(Лишний)\n    Возврат 1;\nКонецФункции\n\n\
                  Процедура Обработать()\n    А = Итог(Истина);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:14 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]\n\
         Функция Итог()\n    Возврат 1;\nКонецФункции\n\n\
         Процедура Обработать()\n    А = Итог();\nКонецПроцедуры\n"
    );
}

#[test]
fn default_valued_parameter_and_a_caller_passing_fewer_arguments() {
    let source = "Процедура Записать(Данные, Знач Лишний = 0, Флаг = Ложь)\n    Сообщить(Данные);\n    Сообщить(Флаг);\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Записать(1);\n    Записать(1, -5, Истина);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:33 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]\n\
         Процедура Записать(Данные, Флаг = Ложь)\n    Сообщить(Данные);\n    Сообщить(Флаг);\nКонецПроцедуры\n\n\
         Процедура Обработать()\n    Записать(1);\n    Записать(1, Истина);\nКонецПроцедуры\n"
    );

    // No caller passes the parameter: only the declaration changes.
    let source =
        "Процедура Записать(Данные, Лишний = 0)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Записать(1);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:28 Удалить неиспользуемый параметр \"Лишний\" [fix_all=false]\n\
         Процедура Записать(Данные)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
         Процедура Обработать()\n    Записать(1);\nКонецПроцедуры\n"
    );
}

#[test]
fn calls_are_matched_whatever_the_letter_case_and_through_this_object() {
    let source = "Процедура Записать(Данные, Лишний)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    записать(1, 2);\n    ЗАПИСАТЬ(3, 4);\n    ЭтотОбъект.Записать(5, 6);\n    Строка.Записать(7, 8);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(OBJECT_MODULE, source),
        "UnusedParameters @ 1:28 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]\n\
         Процедура Записать(Данные)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
         Процедура Обработать()\n    записать(1);\n    ЗАПИСАТЬ(3);\n    ЭтотОбъект.Записать(5);\n    Строка.Записать(7, 8);\nКонецПроцедуры\n"
    );
}

#[test]
fn multiline_list_keeps_the_comment_of_the_previous_parameter() {
    let source = "Процедура Записать(Данные, // что записать\n    Лишний, // не нужен\n    Флаг)\n    Сообщить(Данные);\n    Сообщить(Флаг);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 2:5 Удалить неиспользуемый параметр \"Лишний\" [fix_all=false]\n\
         Процедура Записать(Данные, // что записать\n    Флаг)\n    Сообщить(Данные);\n    Сообщить(Флаг);\nКонецПроцедуры\n"
    );
}

/// `Записать(Данные, Лишний)` with `Лишний` unused, and the given call of it.
fn with_call(declaration_suffix: &str, call: &str) -> String {
    format!(
        "Процедура Записать(Данные, Лишний){declaration_suffix}\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
         Процедура Обработать()\n    {call}\nКонецПроцедуры\n"
    )
}

const CONTROL_FIX: &str = "UnusedParameters @ 1:28 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]";

#[test]
fn exported_method_gets_no_fix() {
    let source = with_call(" Экспорт", "Записать(1, 2);");
    assert_eq!(fixes(COMMON_MODULE, &source), "UnusedParameters @ 1:28 no fix");

    let control = with_call("", "Записать(1, 2);");
    assert!(fixes(COMMON_MODULE, &control).starts_with(CONTROL_FIX), "control must be fixed");
}

#[test]
fn argument_with_an_effect_blocks_the_fix() {
    for call in [
        "Записать(1, Вычисленное());",
        "Записать(1, Новый Массив);",
        "Записать(1, Строка.Поле);",
        "Записать(1, Список[0]);",
        "Записать(1, -Счетчик());",
    ] {
        let source = with_call("", call);
        assert_eq!(
            fixes(COMMON_MODULE, &source),
            "UnusedParameters @ 1:28 no fix",
            "fixed although `{call}` evaluates more than a value"
        );
    }

    for call in [
        "Записать(1, 2);",
        "Записать(1, Значение);",
        "Записать(1, \"а\");",
        "Записать(1, (НЕ Флаг));",
    ] {
        let source = with_call("", call);
        assert!(fixes(COMMON_MODULE, &source).starts_with(CONTROL_FIX), "`{call}` must be fixed");
    }
}

#[test]
fn method_bound_by_name_gets_no_fix() {
    for call in [
        "ПодключитьОбработчикОжидания(\"Записать\", 1);",
        "Обработчик = Новый ОписаниеОповещения(\"Записать\", ЭтотОбъект);",
        "ДобавитьОбработчик Источник.ПриИзменении, Записать;",
        "Записать = 1;",
    ] {
        let source = with_call("", &format!("Записать(1, 2);\n    {call}"));
        let found = fixes(OBJECT_MODULE, &source);
        // Either the diagnostic already knows the binding, or the fix stays away.
        assert!(!found.contains("Удалить"), "fixed although `{call}` binds the method: {found}");
    }

    let control = with_call("", "Записать(1, 2);\n    Сообщить(\"Записать данные\");");
    assert_eq!(fixes(OBJECT_MODULE, &control), "UnusedParameters @ 1:28 no fix");
    let control = with_call("", "Записать(1, 2);\n    Сообщить(\"Записатьданные\");");
    assert!(
        fixes(OBJECT_MODULE, &control).starts_with(CONTROL_FIX),
        "a longer word must not block"
    );
}

#[test]
fn module_running_dynamic_code_gets_no_fix() {
    for call in ["Выполнить(Текст);", "А = Вычислить(Текст);"] {
        let source = with_call("", &format!("Записать(1, 2);\n    {call}"));
        assert_eq!(fixes(COMMON_MODULE, &source), "UnusedParameters @ 1:28 no fix", "`{call}`");
    }

    let control = with_call("", "Записать(1, 2);\n    Сообщить(Текст);");
    assert!(fixes(COMMON_MODULE, &control).starts_with(CONTROL_FIX), "control must be fixed");
}

#[test]
fn parameter_named_in_a_preprocessor_branch_or_string_keeps_its_declaration() {
    let source = "Процедура Записать(Данные, Лишний)\n    Сообщить(Данные);\n\
                  #Если Клиент Тогда\n    Сообщить(Лишний);\n#КонецЕсли\nКонецПроцедуры\n";
    let found = fixes(COMMON_MODULE, source);
    assert!(!found.contains("Удалить"), "{found}");

    // The name in a string of the method's body: the diagnostic reports the parameter,
    // the fix leaves it.
    let source = "Процедура Записать(Данные, Лишний)\n    Сообщить(Данные);\n    Сообщить(\"Лишний: \" + Данные);\nКонецПроцедуры\n";
    assert_eq!(fixes(COMMON_MODULE, source), "UnusedParameters @ 1:28 no fix");

    // A field of something else with the parameter's name is not the parameter.
    let source =
        "Процедура Записать(Данные, Лишний)\n    Сообщить(Данные.Лишний);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 1:28 Удалить неиспользуемый параметр \"Лишний\" [fix_all=false]\n\
         Процедура Записать(Данные)\n    Сообщить(Данные.Лишний);\nКонецПроцедуры\n"
    );
}

#[test]
fn method_an_extension_body_mentions_gets_no_fix() {
    let base = with_call("", "Записать(1, 2);");
    let extension = "&После(\"Записать\")\nПроцедура Расш_Записать(Данные, Лишний)\n    Сообщить(Данные);\nКонецПроцедуры\n";
    assert_eq!(
        fixes_in(&[(BASE_MODULE, &base), (EXTENSION_MODULE, extension)]),
        "UnusedParameters @ 1:28 no fix"
    );

    let extension = "Процедура Другая()\n    Записать(1, 2);\nКонецПроцедуры\n";
    assert_eq!(
        fixes_in(&[(BASE_MODULE, &base), (EXTENSION_MODULE, extension)]),
        "UnusedParameters @ 1:28 no fix"
    );

    let extension = "Процедура Другая()\n    Сообщить(1);\nКонецПроцедуры\n";
    assert!(
        fixes_in(&[(BASE_MODULE, &base), (EXTENSION_MODULE, extension)]).starts_with(CONTROL_FIX),
        "an extension body that does not mention the method must not block"
    );
}

#[test]
fn method_defined_in_several_preprocessor_branches_gets_no_fix() {
    // A call shared by both branches must keep matching both signatures.
    let source = "#Если Сервер Тогда\nПроцедура Записать(Данные, Лишний)\n    Сообщить(Данные);\nКонецПроцедуры\n\
                  #Иначе\nПроцедура Записать(Данные, Флаг)\n    Сообщить(Данные + Флаг);\nКонецПроцедуры\n#КонецЕсли\n\n\
                  Процедура Обработать()\n    Записать(1, 2);\nКонецПроцедуры\n";
    assert_eq!(fixes(COMMON_MODULE, source), "UnusedParameters @ 2:28 no fix");

    let control = "#Если Сервер Тогда\nПроцедура Записать(Данные, Лишний)\n    Сообщить(Данные);\nКонецПроцедуры\n#КонецЕсли\n\n\
                   Процедура Обработать()\n    Записать(1, 2);\nКонецПроцедуры\n";
    assert!(
        fixes(COMMON_MODULE, control).starts_with(
            "UnusedParameters @ 2:28 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах"
        ),
        "a single definition inside a branch must be fixed"
    );
}

#[test]
fn last_position_keeps_the_comment_of_the_previous_one() {
    let source = "Процедура Записать(Данные, // что записать\n    Лишний)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Записать(1, // единица\n        2);\nКонецПроцедуры\n";
    assert_eq!(
        fixes(COMMON_MODULE, source),
        "UnusedParameters @ 2:5 Удалить неиспользуемый параметр \"Лишний\" и его аргумент в вызовах [fix_all=false]\n\
         Процедура Записать(Данные // что записать\n)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
         Процедура Обработать()\n    Записать(1 // единица\n);\nКонецПроцедуры\n"
    );
}

#[test]
fn notify_description_with_a_computed_name_blocks_the_fix() {
    for construction in [
        "Новый ОписаниеОповещения(\"Запи\" + \"сать\", ЭтотОбъект)",
        "Новый ОписаниеОповещения(ИмяМетода, ЭтотОбъект)",
        "Новый NotifyDescription(ИмяМетода, ЭтотОбъект)",
        "Новый CallbackDescription(ИмяМетода, ЭтотОбъект)",
        "Новый(\"ОписаниеОповещения\", ИмяМетода, ЭтотОбъект)",
        "Новый(\"CallbackDescription\", ИмяМетода, ЭтотОбъект)",
        "Новый ОписаниеОповещения(\"Другой\", ЭтотОбъект, , ИмяОбработкиОшибки, ЭтотОбъект)",
        "Новый CallbackDescription(\"Другой\", ЭтотОбъект, , ИмяОбработкиОшибки, ЭтотОбъект)",
        "Новый(\"CallbackDescription\", \"Другой\", ЭтотОбъект, , ИмяОбработкиОшибки, ЭтотОбъект)",
    ] {
        let source = with_call("", &format!("Записать(1, 2);\n    Описание = {construction};"));
        assert_eq!(
            fixes(OBJECT_MODULE, &source),
            "UnusedParameters @ 1:28 no fix",
            "`{construction}`"
        );
    }

    for construction in [
        "Новый ОписаниеОповещения(\"Другой\", ЭтотОбъект)",
        "Новый CallbackDescription(\"Другой\", ЭтотОбъект)",
        "Новый(\"ОписаниеОповещения\", \"Другой\", ЭтотОбъект)",
        "Новый(\"CallbackDescription\", \"Другой\", ЭтотОбъект)",
        "Новый ОписаниеОповещения(\"Другой\", ЭтотОбъект, , \"ДругаяОшибка\", ЭтотОбъект)",
        "Новый CallbackDescription(\"Другой\", ЭтотОбъект, , \"ДругаяОшибка\", ЭтотОбъект)",
        "Новый Структура(Ключи)",
    ] {
        let source = with_call("", &format!("Записать(1, 2);\n    Описание = {construction};"));
        assert!(
            fixes(OBJECT_MODULE, &source).starts_with(CONTROL_FIX),
            "`{construction}` names no method at run time and must not block"
        );
    }
}

#[test]
fn default_value_with_an_effect_blocks_the_fix() {
    let source = "Процедура Записать(Данные, Журнал = ЗаписатьВЖурнал())\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
                  Процедура Обработать()\n    Записать(1);\nКонецПроцедуры\n";
    assert_eq!(fixes(COMMON_MODULE, source), "UnusedParameters @ 1:28 no fix");

    let control =
        "Процедура Записать(Данные, Журнал = -1)\n    Сообщить(Данные);\nКонецПроцедуры\n\n\
                   Процедура Обработать()\n    Записать(1);\nКонецПроцедуры\n";
    assert!(
        fixes(COMMON_MODULE, control)
            .starts_with("UnusedParameters @ 1:28 Удалить неиспользуемый параметр \"Журнал\""),
        "a constant default must not block"
    );
}

#[test]
fn set_action_with_a_computed_name_blocks_the_fix() {
    for binding in [
        "Элементы.Склад.УстановитьДействие(\"НачалоВыбора\", \"Запи\" + \"сать\");",
        "Элементы.Склад.УстановитьДействие(\"НачалоВыбора\", ИмяОбработчика);",
        "Items.Warehouse.SetAction(\"StartChoice\", HandlerName);",
    ] {
        let source = with_call("", &format!("Записать(1, 2);\n    {binding}"));
        assert_eq!(fixes(OBJECT_MODULE, &source), "UnusedParameters @ 1:28 no fix", "`{binding}`");
    }

    let control = with_call(
        "",
        "Записать(1, 2);\n    Элементы.Склад.УстановитьДействие(\"НачалоВыбора\", \"Другой\");",
    );
    assert!(
        fixes(OBJECT_MODULE, &control).starts_with(CONTROL_FIX),
        "a literal name of another method must not block"
    );
}
