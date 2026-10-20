//! `UnusedLocalMethod` / `UnusedParameters` in a form module whose procedure is bound
//! as a handler by string from ANOTHER module: a helper in a common module (or another
//! form) is handed the form and wires `Форма.ПодключитьОбработчикОжидания("Имя", …)`,
//! `Форма.Элементы.Поле.УстановитьДействие(…, "Имя")` or
//! `Форма.ПодключитьОбработчикИзмененияДанных(…, "Имя")`. The form module itself holds
//! no trace of the name; the platform calls the procedure all the same.

use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use vfs::{FileId, FileSet, VfsPath};

const FORM_MODULE: &str = "/test/Documents/Заказ/Forms/ФормаДокумента/Ext/Form/Module.bsl";

/// `UnusedLocalMethod` and `UnusedParameters` messages of `FORM_MODULE`, analysed
/// together with `others` in one source root.
fn form_findings(form_source: &str, others: &[(&str, &str)]) -> Vec<String> {
    let mut db = RootDatabaseImpl::new();
    let mut files = FileSet::default();
    let all: Vec<(&str, &str)> =
        std::iter::once((FORM_MODULE, form_source)).chain(others.iter().copied()).collect();
    for (index, (path, source)) in all.iter().enumerate() {
        let file_id = FileId(index as u32);
        files.insert(file_id, VfsPath::new(path.to_string()));
        db.set_file_text(file_id, source);
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    for index in 0..all.len() {
        db.set_file_source_root(FileId(index as u32), SourceRootId(0));
    }
    let mut findings: Vec<String> =
        ide_diagnostics::file_diagnostics(&db, FileId(0), &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|d| {
                matches!(
                    d.code,
                    DiagnosticCode::UnusedLocalMethod | DiagnosticCode::UnusedParameters
                )
            })
            .map(|d| format!("{}: {}", d.code.as_str(), d.message))
            .collect();
    findings.sort();
    findings
}

const FORM: &str = r#"
Процедура ОбработчикОжидания1()
    Сообщить(1);
КонецПроцедуры

Процедура Поле1ПриИзменении(Элемент)
    Сообщить(2);
КонецПроцедуры

Процедура ОбработчикИзмененияДанных(ИмяДанных)
    Сообщить(3);
КонецПроцедуры

Процедура ДействительноНеИспользуется(Параметр)
    Сообщить(4);
КонецПроцедуры

Процедура Подготовить() Экспорт
    ОбщийМодуль1.Подключить(ЭтотОбъект);
КонецПроцедуры
"#;

const HELPER: &str = r#"
Процедура Подключить(Форма) Экспорт
    Форма.ПодключитьОбработчикОжидания("ОбработчикОжидания1", 0.1, Истина);
    Форма.Элементы.Поле1.УстановитьДействие("ПриИзменении", "Поле1ПриИзменении");
    Форма.ПодключитьОбработчикИзмененияДанных("Реквизит1", "ОбработчикИзмененияДанных", Ложь);
КонецПроцедуры
"#;

#[test]
fn handlers_bound_by_string_from_a_common_module_are_used() {
    let findings =
        form_findings(FORM, &[("/test/CommonModules/ОбщийМодуль1/Ext/Module.bsl", HELPER)]);
    assert_eq!(
        findings,
        vec![
            "UnusedLocalMethod: Неиспользуемый локальный метод \"ДействительноНеИспользуется\""
                .to_string(),
            "UnusedParameters: Уберите неиспользуемый параметр \"Параметр\"".to_string(),
        ],
        "{findings:#?}"
    );
}

/// The same name in a message text, a binding to the binder's own module, or a call of
/// a common module's export that merely shares the platform method's name does not
/// reach the form.
#[test]
fn unrelated_strings_and_own_module_bindings_do_not_suppress() {
    let unrelated = r#"
Процедура Подключить(Форма) Экспорт
    Сообщить("ОбработчикОжидания1");
    ПодключитьОбработчикОжидания("ОбработчикОжидания1", 1);
    ЭтаФорма.Элементы.Поле1.УстановитьДействие("ПриИзменении", "Поле1ПриИзменении");
    ОбщийМодуль2.ПодключитьОбработчикОжидания("ОбработчикИзмененияДанных", 1);
КонецПроцедуры
"#;
    let findings = form_findings(
        FORM,
        &[
            ("/test/DataProcessors/Обработка1/Forms/Форма/Ext/Form/Module.bsl", unrelated),
            (
                "/test/CommonModules/ОбщийМодуль2/Ext/Module.bsl",
                "Процедура ПодключитьОбработчикОжидания(Имя, Интервал) Экспорт\nКонецПроцедуры\n",
            ),
        ],
    );
    let unused_methods: Vec<&String> =
        findings.iter().filter(|f| f.starts_with("UnusedLocalMethod")).collect();
    assert_eq!(unused_methods.len(), 4, "{findings:#?}");
}

/// A bound handler keeps its platform-fixed signature; an unbound procedure's ignored
/// parameter is still reported, and without the helper every handler's is.
#[test]
fn parameters_of_handlers_bound_from_another_module_are_not_reported() {
    let unused_parameters = |findings: Vec<String>| -> Vec<String> {
        findings.into_iter().filter(|f| f.starts_with("UnusedParameters")).collect()
    };
    let bound = unused_parameters(form_findings(
        FORM,
        &[("/test/CommonModules/ОбщийМодуль1/Ext/Module.bsl", HELPER)],
    ));
    assert_eq!(
        bound,
        vec!["UnusedParameters: Уберите неиспользуемый параметр \"Параметр\"".to_string()],
        "{bound:#?}"
    );

    let alone = unused_parameters(form_findings(FORM, &[]));
    assert!(alone.iter().any(|f| f.contains("\"ИмяДанных\"")), "{alone:#?}");
}

const COMMON_MODULE_1: &str = "/test/CommonModules/ОбщийМодуль1/Ext/Module.bsl";

fn only_unused_dead_procedure(findings: &[String]) -> bool {
    findings
        == [
            "UnusedLocalMethod: Неиспользуемый локальный метод \"ДействительноНеИспользуется\"",
            "UnusedParameters: Уберите неиспользуемый параметр \"Параметр\"",
        ]
}

/// The parser joins a member access across whitespace, a line break and a comment after
/// the dot; the binding written that way is the same binding.
#[test]
fn binding_split_after_the_dot_is_seen() {
    let helper = r#"
Процедура Подключить(Форма) Экспорт
    Форма.
        ПодключитьОбработчикОжидания("ОбработчикОжидания1", 0.1, Истина);
    Форма.Элементы.Поле1. // поле формы
        УстановитьДействие("ПриИзменении", "Поле1ПриИзменении");
    Форма .	ПодключитьОбработчикИзмененияДанных("Реквизит1", "ОбработчикИзмененияДанных", Ложь);
КонецПроцедуры
"#;
    let findings = form_findings(FORM, &[(COMMON_MODULE_1, helper)]);
    assert!(only_unused_dead_procedure(&findings), "{findings:#?}");
}

/// Outside the form, `Элементы` is just a name: a helper parameter or local holding
/// another form's items is a foreign receiver.
#[test]
fn items_named_local_of_a_helper_is_a_foreign_receiver() {
    let helper = r#"
Процедура Подключить(Элементы, Форма) Экспорт
    Перем ЭлементыФормы;
    Элементы.Поле1.УстановитьДействие("ПриИзменении", "Поле1ПриИзменении");
    ЭлементыФормы = Форма.Элементы;
    ЭлементыФормы.Поле2.УстановитьДействие("ПриИзменении", "ОбработчикОжидания1");
    Items = Форма.Элементы;
    Items.Поле3.УстановитьДействие("ПриИзменении", "ОбработчикИзмененияДанных");
КонецПроцедуры
"#;
    let findings = form_findings(FORM, &[(COMMON_MODULE_1, helper)]);
    assert!(only_unused_dead_procedure(&findings), "{findings:#?}");
}

/// A receiver produced by a call is an object, not the binder's own module, unless the
/// call is a method of the current form's items.
#[test]
fn binding_on_a_call_result_is_seen() {
    let helper = r#"
Процедура Подключить(Форма) Экспорт
    Форма.ПолучитьФормуВладельца().ПодключитьОбработчикОжидания("ОбработчикОжидания1", 0.1, Истина);
    Форма.Элементы.Найти("Поле1").УстановитьДействие("ПриИзменении", "Поле1ПриИзменении");
    ПолучитьФорму("Обработка.Обработка1.Форма").ПодключитьОбработчикИзмененияДанных("Реквизит1", "ОбработчикИзмененияДанных");
    Элементы.Найти("Поле2").УстановитьДействие("ПриИзменении", "ДействительноНеИспользуется");
КонецПроцедуры
"#;
    let findings = form_findings(
        FORM,
        &[("/test/DataProcessors/Обработка1/Forms/Форма/Ext/Form/Module.bsl", helper)],
    );
    assert!(only_unused_dead_procedure(&findings), "{findings:#?}");
}

/// Detaching an idle handler does not make the platform call it.
#[test]
fn detaching_a_handler_does_not_mark_it_used() {
    let helper = r#"
Процедура Отключить(Форма) Экспорт
    Форма.ОтключитьОбработчикОжидания("ОбработчикОжидания1");
КонецПроцедуры
"#;
    let findings = form_findings(FORM, &[(COMMON_MODULE_1, helper)]);
    assert!(findings.iter().any(|f| f.contains("метод \"ОбработчикОжидания1\"")), "{findings:#?}");
}
