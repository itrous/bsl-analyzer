use crate::define_metadata;
use crate::metadata::*;
use crate::{AnalysisContext, BodyContext};
use crate::{Diagnostic, DiagnosticCode};
use bsl_platform::PlatformVersion;
use hir::LocalRange;
use hir::Name;
use syntax::{SyntaxKind, SyntaxNode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 10,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

fn message(
    name: &str,
    introduced: PlatformVersion,
    minimum: PlatformVersion,
    ctx: &AnalysisContext,
) -> String {
    match ctx.locale() {
        base_db::Locale::Ru => format!(
            "'{name}' доступен с версии платформы {introduced}, а минимальная платформа проекта {minimum}: на ней этого ещё нет"
        ),
        base_db::Locale::En => format!(
            "'{name}' is available since platform {introduced}, but the project's minimum platform is {minimum}: that platform does not have it yet"
        ),
    }
}

/// A platform member resolved by inference (global function or property, type,
/// method or property of a platform-typed value).
pub fn from_hir(
    name: &Name,
    introduced: PlatformVersion,
    minimum: PlatformVersion,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    crate::simple_hir_diagnostic(
        DiagnosticCode::PlatformMemberNewerThanMinVersion,
        message(name.as_str(), introduced, minimum, ctx),
        range,
        ctx,
    )
}

/// The language part, which no member lookup sees: an `Асинх` method declaration
/// and a `Ждать` operator. Both are syntax, so they are judged here, on the
/// tokens of the body, and dated [`hir::min_platform::ASYNC_INTRODUCED`]. Silent
/// where the member checks are: in a module compiled nowhere, and for `Ждать` in
/// a statement inference walked as an uncompiled `#Если` branch.
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::PlatformMemberNewerThanMinVersion;
    if ctx.is_disabled_with_metadata(code) {
        return;
    }
    let Some(minimum) = ctx.min_platform_version() else {
        return;
    };
    let introduced = hir::min_platform::ASYNC_INTRODUCED;
    if !introduced.release_newer_than(minimum) {
        return;
    }
    if ctx.module_metadata().compiles_nowhere() {
        return;
    }
    let infer = ctx.infer();
    for token in ctx.tokens() {
        let Some(parent) = token.parent() else {
            continue;
        };
        let owned = match token.kind() {
            SyntaxKind::KW_ASYNC => crate::body_context::is_method_node(&parent),
            SyntaxKind::KW_AWAIT => {
                parent.kind() == SyntaxKind::AWAIT_EXPR
                    && !in_uncompiled_stmt(ctx, &parent, infer.uncompiled_stmts())
            }
            _ => false,
        };
        if !owned {
            continue;
        }
        if let Some(diagnostic) = crate::simple_hir_diagnostic(
            code,
            message(token.text(), introduced, minimum, ctx),
            ctx.token_range(&token),
            ctx,
        ) {
            acc.push(diagnostic);
        }
    }
}

/// Whether the innermost statement around `node` is one inference walked inside
/// an uncompiled `#Если` branch.
fn in_uncompiled_stmt(ctx: &BodyContext, node: &SyntaxNode, uncompiled: &[hir::StmtId]) -> bool {
    if uncompiled.is_empty() {
        return false;
    }
    node.ancestors()
        .find_map(|ancestor| ctx.source_map().stmt_at_range(ctx.range_of(&ancestor)))
        .is_some_and(|stmt| uncompiled.contains(&stmt))
}

#[cfg(test)]
mod tests {
    use crate::{Diagnostic, DiagnosticCode, DiagnosticsConfig};

    fn run(
        source: &str,
        minimum: Option<&str>,
        builder: test_fixture::CfeFixtureBuilder,
    ) -> Vec<Diagnostic> {
        let minimum = minimum.map(std::sync::Arc::<str>::from);
        crate::test_utils::check_cfe_at_with_db_setup(
            "CommonModules/Caller/Ext/Module.bsl",
            source,
            builder.build(),
            &[],
            DiagnosticsConfig::default(),
            |_| {},
            |db| db.set_min_platform_version(minimum),
            |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
        )
        .into_iter()
        .filter(|diag| diag.code == DiagnosticCode::PlatformMemberNewerThanMinVersion)
        .collect()
    }

    fn newer(source: &str, minimum: Option<&str>) -> Vec<String> {
        run(source, minimum, test_fixture::CfeFixtureBuilder::new(""))
            .into_iter()
            .map(|diag| diag.message)
            .collect()
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn global_function_newer_than_the_minimum_is_reported() {
        let source = r#"
Процедура Тест()
    Результат = СтрЗаменитьПоРегулярномуВыражению("abc", "b", "x");
КонецПроцедуры
"#;
        let reported = newer(source, Some("8.3.17"));
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(reported[0].contains("СтрЗаменитьПоРегулярномуВыражению"), "{reported:?}");
        assert!(reported[0].contains("8.3.23"), "{reported:?}");
        assert!(reported[0].contains("8.3.17"), "{reported:?}");

        assert!(newer(source, Some("8.3.23")).is_empty(), "8.3.23 has the function");
    }

    #[test]
    fn global_functions_old_enough_stay_silent() {
        let source = r#"
Процедура Тест()
    Позиция = Найти("abc", "b");
    Позиция = СтрНайти("abc", "b");
    Части = СтрРазделить("a,b", ",");
КонецПроцедуры
"#;
        let reported = newer(source, Some("8.3.17"));
        assert!(reported.is_empty(), "{reported:?}");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn the_range_covers_the_called_name() {
        let source = r#"
Процедура Тест()
    Результат = СтрЗаменитьПоРегулярномуВыражению("abc", "b", "x");
КонецПроцедуры
"#;
        let diagnostics = run(source, Some("8.3.17"), test_fixture::CfeFixtureBuilder::new(""));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let range = diagnostics[0].range;
        let text = &source[usize::from(range.start())..usize::from(range.end())];
        assert_eq!(text, "СтрЗаменитьПоРегулярномуВыражению");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn method_of_a_platform_typed_value_is_dated_by_member_and_owner() {
        let source = r#"
Процедура Тест()
    Запрос = Новый HTTPЗапрос("/");
    Запрос.ДобавитьТокенДоступа(Неопределено);
    Запрос.УстановитьТелоИзСтроки("x");
    Информация = Новый СистемнаяИнформация;
    Вариант = Информация.ВариантПриложения;
КонецПроцедуры
"#;
        let reported = newer(source, Some("8.3.17"));
        assert_eq!(reported.len(), 2, "{reported:?}");
        assert!(
            reported.iter().any(|m| m.contains("ДобавитьТокенДоступа") && m.contains("8.3.21")),
            "{reported:?}"
        );
        assert!(
            reported.iter().any(|m| m.contains("ВариантПриложения") && m.contains("8.3.22")),
            "{reported:?}"
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn constructed_type_newer_than_the_minimum_is_reported() {
        let source = r#"
Процедура Тест()
    Генератор = Новый ГенераторСлучайныхПаролей;
    Список = Новый Массив;
КонецПроцедуры
"#;
        let reported = newer(source, Some("8.3.17"));
        assert_eq!(reported.len(), 1, "{reported:?}");
        assert!(reported[0].contains("ГенераторСлучайныхПаролей"), "{reported:?}");
        assert!(reported[0].contains("8.3.22"), "{reported:?}");
    }

    #[test]
    fn async_declaration_and_await_need_8_3_18() {
        let source = r#"
Асинх Процедура Тест()
    Ждать Пауза();
КонецПроцедуры

Асинх Функция Пауза()
    Возврат Неопределено;
КонецФункции
"#;
        let on_17 = newer(source, Some("8.3.17"));
        assert_eq!(on_17.len(), 3, "two Асинх and one Ждать: {on_17:?}");
        assert_eq!(on_17.iter().filter(|m| m.contains("'Асинх'")).count(), 2, "{on_17:?}");
        assert_eq!(on_17.iter().filter(|m| m.contains("'Ждать'")).count(), 1, "{on_17:?}");
        assert!(on_17.iter().all(|m| m.contains("8.3.18")), "{on_17:?}");

        let on_18 = newer(source, Some("8.3.18"));
        assert!(on_18.is_empty(), "8.3.18 compiles Асинх: {on_18:?}");
    }

    #[test]
    fn unset_minimum_is_silent() {
        let source = r#"
Асинх Процедура Тест()
    Результат = СтрЗаменитьПоРегулярномуВыражению("abc", "b", "x");
    Генератор = Новый ГенераторСлучайныхПаролей;
КонецПроцедуры
"#;
        assert!(newer(source, None).is_empty());
        assert!(newer(source, Some("not a version")).is_empty(), "an unparseable floor is off");
        assert!(newer(source, Some("8.3.17 (typo)")).is_empty(), "a suffixed floor is off");
    }

    #[test]
    fn a_local_procedure_of_the_same_name_shadows_the_global() {
        let source = r#"
Функция СтрЗаменитьПоРегулярномуВыражению(Строка, Шаблон, Замена)
    Возврат Строка;
КонецФункции

Процедура Тест()
    Результат = СтрЗаменитьПоРегулярномуВыражению("abc", "b", "x");
КонецПроцедуры
"#;
        let reported = newer(source, Some("8.3.17"));
        assert!(reported.is_empty(), "the call reaches the module's own function: {reported:?}");
    }

    #[test]
    fn a_global_common_module_export_of_the_same_name_shadows_the_global() {
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module_global(
            "Глобальный",
            "Функция СтрЗаменитьПоРегулярномуВыражению(А, Б, В) Экспорт Возврат А; КонецФункции",
        );
        let source = r#"
Процедура Тест()
    Результат = СтрЗаменитьПоРегулярномуВыражению("abc", "b", "x");
КонецПроцедуры
"#;
        let reported: Vec<_> =
            run(source, Some("8.3.17"), builder).into_iter().map(|d| d.message).collect();
        assert!(reported.is_empty(), "{reported:?}");
    }

    /// One undated member exists in the bundled catalog
    /// (`InAppPurchasesManager.ПоддерживаетсяИсторияПриобретений`); its absence of a
    /// date must never read as "new". The lookup layer returns `None` for it, which
    /// is what the inference check needs to stay silent.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_member_without_a_version_is_silent() {
        let data = bsl_platform::PlatformDataInner::instance();
        let undated = data
            .all_methods()
            .iter()
            .find(|method| method.min_version.is_none())
            .expect("the catalog keeps at least one undated method");
        assert_eq!(
            hir::min_platform::type_member(
                undated.type_name.as_str(),
                undated.name.as_str(),
                false
            ),
            None
        );
    }

    /// The rule's messages for `body` as the whole of the common module `Проба`:
    /// a server module, or — `compiled_nowhere` — one with every environment off.
    fn in_common_module(body: &str, compiled_nowhere: bool) -> Vec<String> {
        let mut builder = test_fixture::CfeFixtureBuilder::new("");
        builder.add_base_module("Проба", body);
        crate::test_utils::check_cfe_at_with_db_setup(
            "CommonModules/Проба/Ext/Module.bsl",
            body,
            builder.build(),
            &[],
            DiagnosticsConfig::default(),
            |fixture| {
                if !compiled_nowhere {
                    return;
                }
                let path = fixture.root().join("CommonModules/Проба.xml");
                let xml = std::fs::read_to_string(&path).expect("read module metadata");
                std::fs::write(
                    &path,
                    xml.replace("<Server>true</Server>", "<Server>false</Server>"),
                )
                .expect("write module metadata");
            },
            |db| db.set_min_platform_version(Some("8.3.17".into())),
            |db, ctx| crate::file_diagnostics(db, ctx.file_id, ctx.config),
        )
        .into_iter()
        .filter(|diag| diag.code == DiagnosticCode::PlatformMemberNewerThanMinVersion)
        .map(|diag| diag.message)
        .collect()
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn an_uncompiled_branch_stays_silent() {
        const CALL: &str =
            "    Результат = СтрЗаменитьПоРегулярномуВыражению(\"abc\", \"b\", \"x\");\n";
        let plain =
            in_common_module(&format!("Процедура Тест() Экспорт\n{CALL}КонецПроцедуры\n"), false);
        assert_eq!(plain.len(), 1, "the server compiles the plain call: {plain:?}");
        let branch = in_common_module(
            &format!(
                "Процедура Тест() Экспорт\n#Если ТолстыйКлиентОбычноеПриложение Тогда\n{CALL}#КонецЕсли\nКонецПроцедуры\n"
            ),
            false,
        );
        assert!(branch.is_empty(), "a server module never compiles that branch: {branch:?}");
    }

    /// The input that tells a token walk from a compiled-branch walk: the same
    /// `Ждать` once in the plain body and once under a branch the server skips.
    #[test]
    fn await_in_an_uncompiled_branch_stays_silent() {
        const AWAIT: &str = "    Ждать Пауза();\n";
        let awaits = |messages: Vec<String>| {
            messages.into_iter().filter(|m| m.contains("'Ждать'")).collect::<Vec<_>>()
        };
        let plain = awaits(in_common_module(
            &format!("Процедура Тест() Экспорт\n{AWAIT}КонецПроцедуры\n"),
            false,
        ));
        assert_eq!(plain.len(), 1, "the server compiles the plain `Ждать`: {plain:?}");
        let branch = awaits(in_common_module(
            &format!(
                "Процедура Тест() Экспорт\n#Если ТолстыйКлиентОбычноеПриложение Тогда\n    Если Истина Тогда\n    {AWAIT}    КонецЕсли;\n#КонецЕсли\nКонецПроцедуры\n"
            ),
            false,
        ));
        assert!(branch.is_empty(), "a server module never compiles that branch: {branch:?}");
    }

    #[test]
    fn async_in_a_module_compiled_nowhere_stays_silent() {
        const BODY: &str = "Асинх Процедура Тест() Экспорт\n    Ждать Пауза();\nКонецПроцедуры\n";
        let server = in_common_module(BODY, false);
        assert_eq!(server.len(), 2, "`Асинх` and `Ждать` in a server module: {server:?}");
        let nowhere = in_common_module(BODY, true);
        assert!(nowhere.is_empty(), "a module compiled nowhere cannot fail: {nowhere:?}");
    }

    #[test]
    fn metadata_is_an_active_major_error() {
        let metadata =
            crate::handlers::get_metadata(DiagnosticCode::PlatformMemberNewerThanMinVersion)
                .unwrap();
        assert_eq!(metadata.severity, DiagnosticSeverityLevel::Major);
        assert!(metadata.activated_by_default);
        assert!(!DiagnosticsConfig::default()
            .is_disabled(DiagnosticCode::PlatformMemberNewerThanMinVersion));
    }

    use crate::metadata::DiagnosticSeverityLevel;
}
