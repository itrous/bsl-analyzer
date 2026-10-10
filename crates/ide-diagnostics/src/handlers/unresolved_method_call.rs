use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::{Name, UnresolvedMethodKind};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    receiver_name: &Name,
    method_name: &Name,
    kind: UnresolvedMethodKind,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let message = match kind {
        UnresolvedMethodKind::MethodNotFound => {
            format!("Метод '{}' не найден у '{}'", method_name.as_str(), receiver_name.as_str())
        }
        UnresolvedMethodKind::MethodNotExport => {
            format!("Метод '{}.{}' не экспортирован", receiver_name.as_str(), method_name.as_str())
        }
        UnresolvedMethodKind::SelfMethodNotExport => format!(
            "Метод '{}' не экспортный: через '{}' доступны только экспортные методы модуля; \
             вызовите его без квалификатора или объявите Экспорт",
            method_name.as_str(),
            receiver_name.as_str()
        ),
        // Nothing is said about this call. The module's surface is unknown, so any
        // verdict would be a guess, and it would be filed against the calling file —
        // which is not the one with the problem. The unreadable file is reported at
        // its own address, once, by the host that failed to read it.
        UnresolvedMethodKind::BodyUnread => return None,
        UnresolvedMethodKind::ReceiverNotResolved | UnresolvedMethodKind::ReceiverNameAbsent => {
            unresolved_receiver_message(receiver_name, kind)
        }
    };
    crate::simple_hir_diagnostic(DiagnosticCode::UnresolvedMethodCall, message, range, ctx)
}

fn unresolved_receiver_message(receiver_name: &Name, kind: UnresolvedMethodKind) -> String {
    debug_assert!(matches!(
        kind,
        UnresolvedMethodKind::ReceiverNotResolved | UnresolvedMethodKind::ReceiverNameAbsent
    ));
    format!("Не удалось разрешить получателя вызова '{}'", receiver_name.as_str())
}

#[cfg(test)]
mod tests {
    use hir::{Name, UnresolvedMethodKind};

    use super::unresolved_receiver_message;
    use crate::test_utils::check_hir_diagnostic_with_fixtures;
    use crate::DiagnosticCode;

    #[test]
    fn this_object_and_this_form_self_calls_resolve_in_form_module() {
        // `ЭтотОбъект.Метод()` / `ЭтаФорма.Метод()` in a form module are self-references
        // to the current module, not a module named `ЭтотОбъект`/`ЭтаФорма`. Neither may
        // fire the "не удалось разрешить модуль" (ReceiverNotResolved) diagnostic.
        let fixture = r#"
//- /Catalogs/Тест/Forms/Форма/Ext/Form/Module.bsl
Процедура МояПроцедура() Экспорт
КонецПроцедуры

Процедура Обработчик()
    ЭтотОбъект.МояПроцедура();
    ЭтаФорма.МояПроцедура();
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let umc: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert!(
            umc.is_empty(),
            "ЭтотОбъект/ЭтаФорма self-calls must resolve, not fire UnresolvedMethodCall: {umc:?}"
        );
    }

    #[test]
    fn this_object_self_calls_keep_call_checks_in_form_module_without_form_metadata() {
        // Resolving the self receiver must not drop what the managed-form path checks:
        // argument count, the export rule, and silence on unknown platform form members.
        let fixture = r#"
//- /Catalogs/Тест/Forms/Форма/Ext/Form/Module.bsl
Процедура Экспортная(А, Б) Экспорт
КонецПроцедуры

Процедура Закрытая()
КонецПроцедуры

Процедура Обработчик()
    ЭтотОбъект.Экспортная(1);
    ЭтаФорма.Закрытая();
    ЭтаФорма.ОбновитьОтображениеДанных();
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let arg_count: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::MismatchedArgCount).collect();
        assert_eq!(arg_count.len(), 1, "expected one MismatchedArgCount, got: {diags:?}");
        let umc: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert_eq!(umc.len(), 1, "expected one UnresolvedMethodCall, got: {umc:?}");
        assert!(umc[0].message.contains("Закрытая"), "expected the non-export call: {umc:?}");
    }

    #[test]
    fn form_module_untyped_receiver_is_silent_not_unresolved_module() {
        // In a form module a bare-identifier receiver that resolves to nothing is almost
        // always a form attribute / element / the main object `Объект`. Half the config's
        // forms are binary (`Form.bin`, no `Form.xml`), so their attribute list cannot be
        // read at all — such a call must not be reported as "не удалось разрешить модуль".
        // (The harness never loads `Form.xml`, so this models exactly the binary-form case.)
        let fixture = r#"
//- /Catalogs/Тест/Forms/ФормаЭлемента/Ext/Form/Module.bsl
Процедура Обработчик()
    Объект.Пустая();
    ТЗВходит.Добавить();
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let umc: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert!(
            umc.is_empty(),
            "untyped form-module receivers must stay silent, not fire UnresolvedMethodCall: {umc:?}"
        );
    }

    #[test]
    fn unresolved_receiver_messages_are_neutral_for_both_reasons() {
        let receiver = Name::new("НеизвестныйПолучатель");
        for kind in
            [UnresolvedMethodKind::ReceiverNotResolved, UnresolvedMethodKind::ReceiverNameAbsent]
        {
            let message = unresolved_receiver_message(&receiver, kind);
            assert_eq!(message, "Не удалось разрешить получателя вызова 'НеизвестныйПолучатель'");
            assert!(!message.contains("модул"));
        }
    }

    #[test]
    fn emits_when_method_not_found_on_existing_module() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Процедура СуществующийМетод() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ОбщийМодуль.НесуществующийМетод();
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let unresolved: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert_eq!(unresolved.len(), 1, "expected one UnresolvedMethodCall, got: {diags:?}");
        assert!(
            unresolved[0].message.contains("НесуществующийМетод"),
            "message must name the missing method, got: {}",
            unresolved[0].message
        );
    }

    /// The body of the callee module could not be read, so its empty text is
    /// ignorance rather than an empty API — and the caller, which did not change,
    /// must not be blamed for a method the unread body may well export.
    ///
    /// The readable half is the positive control: without it the test would pass on
    /// an implementation that never reports an unresolved call at all.
    #[test]
    fn an_unread_callee_body_silences_the_call_where_an_empty_one_reports_it() {
        use crate::test_utils::check_hir_diagnostic_with_unreadable;

        let fixture = r#"
//- /CommonModules/Сервер/Ext/Module.bsl

//- /test.bsl
Процедура Тест()
    Сервер.П();
КонецПроцедуры
"#;

        let readable = check_hir_diagnostic_with_unreadable(fixture, &[]);
        let reported: Vec<_> =
            readable.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert_eq!(
            reported.len(),
            1,
            "an honestly empty module really does lack the method: {readable:?}"
        );

        let unread = check_hir_diagnostic_with_unreadable(
            fixture,
            &["/CommonModules/Сервер/Ext/Module.bsl"],
        );
        let silenced: Vec<_> =
            unread.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert!(silenced.is_empty(), "the caller is not the file with the problem: {silenced:?}");
    }

    /// A common module can have more than one body — the base declaration plus an
    /// extension's adoption of the same name — and the method may live in either. When
    /// one of them cannot be read, the readable one missing the method proves nothing,
    /// even though the candidate list is not empty. This is the input on which a rule
    /// phrased as "refuse once the list is empty" would silently do nothing.
    #[test]
    fn a_readable_body_without_the_method_is_no_verdict_while_a_sibling_body_is_unread() {
        use crate::test_utils::{check_with_cfe, check_with_cfe_unreadable};

        let source = r#"
Процедура Тест() Экспорт
    Сервер.П();
КонецПроцедуры
"#;
        let build = || {
            let mut builder = test_fixture::CfeFixtureBuilder::new("");
            builder.add_base_module("Сервер", "Процедура Другой() Экспорт КонецПроцедуры");
            builder.add_extension("Расш", "");
            builder.add_extension_module("Расш", "Сервер", "Процедура П() Экспорт КонецПроцедуры");
            builder.build()
        };

        // Positive control: both bodies readable and neither exports `П` — the verdict
        // is earned, and the test would be green on "suppress always" without this.
        let mut readable_builder = test_fixture::CfeFixtureBuilder::new("");
        readable_builder.add_base_module("Сервер", "Процедура Другой() Экспорт КонецПроцедуры");
        readable_builder.add_extension("Расш", "");
        readable_builder.add_extension_module(
            "Расш",
            "Сервер",
            "Процедура Третий() Экспорт КонецПроцедуры",
        );
        let control = check_with_cfe(source, readable_builder.build());
        assert!(
            control.iter().any(|d| d.code == DiagnosticCode::UnresolvedMethodCall),
            "with every body readable the method really is missing: {control:?}"
        );

        let unread = check_with_cfe_unreadable(
            source,
            build(),
            &["Extensions/Расш/CommonModules/Сервер/Ext/Module.bsl"],
        );
        assert!(
            !unread.iter().any(|d| d.code == DiagnosticCode::UnresolvedMethodCall),
            "the method may well live in the body nobody could read: {unread:?}"
        );
    }

    /// Priority runs base-before-extension, so a hit in the extension body is the
    /// answer only when the base body was readable and did not have the method. With
    /// the base unread, the extension's non-exported `П` must not produce a verdict:
    /// the base may well export one, and its declaration would have won.
    #[test]
    fn an_unread_base_body_bars_a_verdict_from_the_extension_body_behind_it() {
        use crate::test_utils::{check_with_cfe, check_with_cfe_unreadable};

        let source = r#"
Процедура Тест() Экспорт
    Сервер.П();
КонецПроцедуры
"#;
        let build = || {
            let mut builder = test_fixture::CfeFixtureBuilder::new("");
            builder.add_base_module("Сервер", "Процедура П() Экспорт КонецПроцедуры");
            builder.add_extension("Расш", "");
            builder.add_extension_module("Расш", "Сервер", "Процедура П() КонецПроцедуры");
            builder.build()
        };

        // Control: with the base readable its exported `П` wins and nothing is reported,
        // so the assertion below is about the unread base and not about the fixture.
        let control = check_with_cfe(source, build());
        assert!(
            !control.iter().any(|d| d.code == DiagnosticCode::UnresolvedMethodCall),
            "the base body exports П and has priority: {control:?}"
        );

        let unread =
            check_with_cfe_unreadable(source, build(), &["CommonModules/Сервер/Ext/Module.bsl"]);
        assert!(
            !unread.iter().any(|d| d.code == DiagnosticCode::UnresolvedMethodCall),
            "an unread base body bars the extension from answering for it: {unread:?}"
        );
    }

    /// The route that produced the defect in the first place: with no configuration
    /// root the resolver never asks the substrate at all and takes the candidate
    /// straight from the path-derived module index, which holds the unread body's id
    /// like any other file's.
    #[test]
    fn the_path_index_route_is_the_one_under_test_here() {
        use crate::test_utils::check_hir_diagnostic_with_unreadable;

        let fixture = r#"
//- /CommonModules/Сервер/Ext/Module.bsl

//- /test.bsl
Процедура Тест()
    Сервер.П();
КонецПроцедуры
"#;
        // No `set_all_config_paths` runs in this fixture, so `has_config_root` is
        // false and the substrate branch is unreachable — the assertion below can
        // only be satisfied by the check on the path-index candidate.
        let unread = check_hir_diagnostic_with_unreadable(
            fixture,
            &["/CommonModules/Сервер/Ext/Module.bsl"],
        );
        assert!(
            !unread.iter().any(|d| d.code == DiagnosticCode::UnresolvedMethodCall),
            "path-index candidates must be checked for readability too: {unread:?}"
        );
    }

    #[test]
    fn stays_silent_when_method_exists_and_is_exported() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
Процедура Метод() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ОбщийМодуль.Метод();
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        assert!(
            diags.iter().all(|d| d.code != DiagnosticCode::UnresolvedMethodCall),
            "resolved call must not fire UnresolvedMethodCall: {diags:?}"
        );
    }

    use crate::test_utils::check_hir_diagnostic;

    #[test]
    fn parameter_shadows_qualified_call() {
        let code = r#"
Процедура Тест(М)
    М.КакойТоМетод();
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let umc: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert!(
            umc.is_empty(),
            "parameter receiver must not fire UnresolvedMethodCall, got: {umc:?}"
        );
    }

    #[test]
    fn declared_local_var_shadows_qualified_call() {
        let code = r#"
Процедура Тест()
    Перем М;
    М.Метод();
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let umc: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert!(
            umc.is_empty(),
            "declared `Перем` receiver must not fire UnresolvedMethodCall, got: {umc:?}"
        );
    }

    // `Перем = ОбщегоНазначения.ОбщийМодуль("Имя")` types `Перем` as the named common module,
    // so member calls resolve against it. Two-file fixtures (the harness analyses the last file
    // by hash order; a self-referential module name keeps it to two files, like the tests above).
    #[test]
    fn common_module_by_name_missing_method_fires() {
        let fixture = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ОбщийМодуль(Имя) Экспорт
    Возврат Имя;
КонецФункции

//- /test.bsl
Процедура Тест()
    Модуль = ОбщегоНазначения.ОбщийМодуль("ОбщегоНазначения");
    Модуль.НесуществующийМетод();
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let umc: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert_eq!(umc.len(), 1, "expected one UnresolvedMethodCall, got: {diags:?}");
        assert!(
            umc[0].message.contains("НесуществующийМетод"),
            "message must name the missing method, got: {}",
            umc[0].message
        );
    }

    #[test]
    fn common_module_by_name_existing_method_silent() {
        let fixture = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ОбщийМодуль(Имя) Экспорт
    Возврат Имя;
КонецФункции

//- /test.bsl
Процедура Тест()
    Модуль = ОбщегоНазначения.ОбщийМодуль("ОбщегоНазначения");
    Модуль.ОбщийМодуль("Прочее");
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        assert!(
            diags.iter().all(|d| d.code != DiagnosticCode::UnresolvedMethodCall),
            "resolved common-module method via ОбщийМодуль must stay silent: {diags:?}"
        );
    }

    #[test]
    fn module_comma_var_receiver_under_pre_if_stays_silent() {
        let fixture = r#"
//- /test.bsl
#Если Сервер Тогда
Перем Запрос, Выборка;
#КонецЕсли

Процедура Инициализировать()
    Запрос = Новый Запрос;
    Выборка = Запрос.Выполнить().Выбрать();
КонецПроцедуры

Процедура Использовать()
    Выборка.Сбросить();
КонецПроцедуры
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let unresolved: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert!(
            unresolved.is_empty(),
            "declared receiver from comma `Перем` under `#Если Сервер` must stay silent, got: {diags:?}"
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn platform_global_method_call_resolves_silently() {
        let code = r#"
Процедура Тест()
    ОбработкаОшибок.КраткоеПредставлениеОшибки(ИнформацияОбОшибке());
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let umc: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert!(
            umc.is_empty(),
            "valid platform-global member call must not fire UnresolvedMethodCall, got: {umc:?}"
        );
    }

    #[test]
    fn unregistered_module_emits_receiver_not_resolved() {
        let code = r#"
Процедура Тест()
    ЗаведомоНесуществующийПрефикс.КакойТоМетод();
КонецПроцедуры
"#;
        let diagnostics = check_hir_diagnostic(code);
        let umc: Vec<_> =
            diagnostics.iter().filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall).collect();
        assert_eq!(
            umc.len(),
            1,
            "unresolved receiver must surface exactly one UnresolvedMethodCall, got: {umc:?}"
        );
        let callee = "ЗаведомоНесуществующийПрефикс.КакойТоМетод";
        let start = code.find(callee).expect("callee in fixture") as u32;
        assert_eq!(umc[0].code, DiagnosticCode::UnresolvedMethodCall);
        assert_eq!(
            umc[0].range,
            ide_db::TextRange::new(start.into(), (start + callee.len() as u32).into())
        );
        assert!(
            umc[0].message.contains("Не удалось разрешить получателя вызова")
                && umc[0].message.contains("ЗаведомоНесуществующийПрефикс")
                && !umc[0].message.contains("модул"),
            "message must use the ReceiverNotResolved phrasing and name the receiver; \
             a MethodNotFound-shaped phrasing here would be a regression. got: {}",
            umc[0].message
        );
    }

    fn form_umc(form_source: &str) -> Vec<String> {
        crate::test_utils::check_form_with_common_modules(form_source, &[])
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::UnresolvedMethodCall)
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn managed_form_with_readable_metadata_still_reports_unknown_receiver() {
        // The silence for unresolved bare receivers holds only where the form's attribute
        // list cannot be read. A managed form with readable metadata enumerates its
        // attributes, so a name matching none of them is still reported — as an
        // undefined name, which subsumes the unresolved-receiver verdict.
        let diags = crate::test_utils::check_form_with_common_modules(
            "&НаКлиенте\nПроцедура Сохранить()\n    НеизвестныйМодуль.Метод();\nКонецПроцедуры\n",
            &[],
        );
        let reported: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(
                    d.code,
                    DiagnosticCode::UnresolvedName | DiagnosticCode::UnresolvedMethodCall
                ) && d.message.contains("НеизвестныйМодуль")
            })
            .collect();
        assert_eq!(
            reported.len(),
            1,
            "expected the unknown receiver to be reported once, got: {diags:?}"
        );
    }

    #[test]
    fn form_self_call_to_missing_method_emits() {
        let umc = form_umc(
            "&НаКлиенте\nПроцедура Сохранить()\n    ЭтотОбъект.НетТакогоМетода();\nКонецПроцедуры\n",
        );
        assert_eq!(
            umc.len(),
            1,
            "the form itself is a closed surface — a missing self method must surface, got: {umc:?}"
        );
        assert!(
            umc[0].contains("НетТакогоМетода"),
            "message must name the missing method, got: {}",
            umc[0]
        );
    }

    #[test]
    fn form_self_call_to_own_method_is_silent() {
        let umc = form_umc(
            "&НаКлиенте\nПроцедура Показать() Экспорт\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Сохранить()\n    ЭтотОбъект.Показать();\nКонецПроцедуры\n",
        );
        assert!(umc.is_empty(), "the module declares and exports the method, got: {umc:?}");
    }

    #[test]
    fn form_self_call_to_own_exported_server_method_is_silent() {
        let umc = form_umc(
            "&НаСервере\nФункция Посчитать() Экспорт\n    Возврат 1;\nКонецФункции\n\n&НаСервере\nПроцедура Обработать()\n    Итог = ЭтаФорма.Посчитать();\n    Итог = ЭтотОбъект.Посчитать();\nКонецПроцедуры\n",
        );
        assert!(umc.is_empty(), "an exported server method is a member of the form, got: {umc:?}");
    }

    /// Checked on 8.3.17.1549 and 8.3.27.2214: the module compiles, and the call fails
    /// at run time with «Метод объекта не обнаружен (Показать)» — on the client and on
    /// the server, through `ЭтотОбъект` and through `ЭтаФорма` alike. The same method
    /// declared `Экспорт` answers (the tests above), and so does the bare call (below).
    #[test]
    fn form_self_call_to_non_exported_method_reports_it() {
        let umc = form_umc(
            "&НаКлиенте\nПроцедура Показать()\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Сохранить()\n    ЭтотОбъект.Показать();\n    ЭтаФорма.Показать();\nКонецПроцедуры\n",
        );
        assert_eq!(
            umc,
            vec![
                "Метод 'Показать' не экспортный: через 'ЭтотОбъект' доступны только экспортные \
                 методы модуля; вызовите его без квалификатора или объявите Экспорт"
                    .to_string(),
                "Метод 'Показать' не экспортный: через 'ЭтаФорма' доступны только экспортные \
                 методы модуля; вызовите его без квалификатора или объявите Экспорт"
                    .to_string(),
            ],
        );
    }

    #[test]
    fn form_self_call_to_non_exported_server_method_reports_it() {
        let umc = form_umc(
            "&НаСервере\nФункция Посчитать()\n    Возврат 1;\nКонецФункции\n\n&НаСервере\nПроцедура Обработать()\n    Итог = ЭтаФорма.Посчитать();\nКонецПроцедуры\n",
        );
        assert_eq!(umc.len(), 1, "got: {umc:?}");
        assert!(umc[0].starts_with("Метод 'Посчитать' не экспортный"), "got: {}", umc[0]);
    }

    #[test]
    fn bare_call_to_non_exported_form_method_is_silent() {
        let umc = form_umc(
            "&НаКлиенте\nПроцедура Показать()\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Сохранить()\n    Показать();\nКонецПроцедуры\n",
        );
        assert!(umc.is_empty(), "a bare call needs no Экспорт, got: {umc:?}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn form_self_call_to_platform_form_method_is_silent() {
        let umc = form_umc(
            "&НаКлиенте\nПроцедура Сохранить()\n    ЭтотОбъект.Закрыть();\nКонецПроцедуры\n",
        );
        assert!(
            umc.is_empty(),
            "a platform member of the form type resolves before the module's own methods, \
             got: {umc:?}"
        );
    }

    #[test]
    fn this_form_alias_resolves_the_form_instead_of_an_unknown_module() {
        let umc = form_umc(
            "&НаКлиенте\nПроцедура Показать() Экспорт\nКонецПроцедуры\n\n&НаКлиенте\nПроцедура Сохранить()\n    ЭтаФорма.Показать();\nКонецПроцедуры\n",
        );
        assert!(
            umc.is_empty(),
            "`ЭтаФорма` is the form, not an unresolved module receiver, got: {umc:?}"
        );
    }

    #[test]
    fn parameter_shadowing_a_self_name_keeps_its_own_type() {
        let umc = form_umc(
            "&НаКлиенте\nПроцедура Тест(ЭтаФорма)\n    ЭтаФорма = Новый Массив;\n    ЭтаФорма.Вставить(0, 1);\nКонецПроцедуры\n",
        );
        assert!(
            umc.is_empty(),
            "the parameter shadows the predefined name and carries an array, got: {umc:?}"
        );
    }
}
