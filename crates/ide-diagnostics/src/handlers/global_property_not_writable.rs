use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::Name;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    name: &Name,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::GlobalPropertyNotWritable;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    Some(Diagnostic {
        code,
        message: format!(
            "Свойство глобального контекста '{}' недоступно для записи. \
             Присваивание не создаёт переменную — выберите другое имя",
            name.as_str()
        ),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use crate::test_utils::*;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn writes(code: &str) -> Vec<crate::Diagnostic> {
        check_hir_diagnostic(code)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::GlobalPropertyNotWritable)
            .collect()
    }

    #[test]
    fn assigning_to_a_collection_name_is_flagged() {
        let code = r#"
Процедура Тест()
    Справочники = Новый Структура;
КонецПроцедуры
"#;
        expect![[r#"
            GlobalPropertyNotWritable @ 3:5..3:16
              message: Свойство глобального контекста 'Справочники' недоступно для записи. Присваивание не создаёт переменную — выберите другое имя
              severity: Major"#]]
        .assert_eq(&format_diags(code, &writes(code)));
    }

    #[test]
    fn english_collection_name_is_flagged_too() {
        let code = r#"
Процедура Тест()
    Catalogs = Новый Структура;
КонецПроцедуры
"#;
        assert_eq!(writes(code).len(), 1, "the English spelling names the same property");
    }

    #[test]
    fn a_declared_owner_takes_the_name_and_is_not_flagged() {
        // Every declared owner makes the name its own, so the assignment stores
        // into that owner and is perfectly legal.
        for code in [
            "Перем Справочники;\n\nПроцедура Тест()\n    Справочники = Новый Структура;\nКонецПроцедуры\n",
            "Процедура Тест(Справочники)\n    Справочники = Новый Структура;\nКонецПроцедуры\n",
            "Процедура Тест()\n    Перем Справочники;\n    Справочники = Новый Структура;\nКонецПроцедуры\n",
        ] {
            assert!(
                writes(code).is_empty(),
                "a declared owner owns the name, assignment is legal:\n{code}"
            );
        }
    }

    #[test]
    fn a_name_that_is_not_a_collection_is_not_flagged() {
        let code = r#"
Процедура Тест()
    МояПеременная = Новый Структура;
КонецПроцедуры
"#;
        assert!(writes(code).is_empty(), "an ordinary name declares a local as always");
    }

    /// Одно нарушение — одна диагностика. Синтаксическая проверка самоприсваивания
    /// видит только равные стороны и называет цель переменной; здесь переменной нет,
    /// поэтому точный вердикт её вытесняет.
    ///
    /// Спрашивать надо весь конвейер: вытеснение — свойство ИТОГОВОГО набора, и до
    /// выхода вердикт-победитель ещё может быть отозван базовым модулем пары.
    #[test]
    fn a_refused_self_assignment_is_reported_once() {
        let code = "Процедура Тест()\n    Справочники = Справочники;\n    А = А;\nКонецПроцедуры\n";
        let diags = crate::test_utils::check_file_diagnostics(code);
        assert_eq!(
            diags.iter().filter(|d| d.code == DiagnosticCode::GlobalPropertyNotWritable).count(),
            1,
            "{diags:#?}"
        );
        let self_assigns: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::SelfAssign).collect();
        assert_eq!(
            self_assigns.len(),
            1,
            "only the ordinary self-assignment survives:\n{self_assigns:#?}"
        );
    }

    /// Подавление скрывает готовый итог, а не участвует в выборе победителя:
    /// заглушив точную диагностику, пользователь не должен получить взамен ту,
    /// которую она вытеснила как неверную.
    #[test]
    fn suppressing_the_winner_does_not_resurrect_the_loser() {
        let code = "Процедура Тест()\n    // bsl-analyzer:disable-next-line \
                    GlobalPropertyNotWritable\n    Справочники = Справочники;\nКонецПроцедуры\n";
        let diags = crate::test_utils::check_file_diagnostics(code);
        let pair: Vec<_> = diags
            .iter()
            .filter(|d| {
                matches!(
                    d.code,
                    DiagnosticCode::GlobalPropertyNotWritable | DiagnosticCode::SelfAssign
                )
            })
            .collect();
        assert!(pair.is_empty(), "suppression hides the pair, it does not swap it:\n{pair:#?}");
    }

    #[test]
    fn repeated_assignment_is_flagged_once_per_statement() {
        let code = r#"
Процедура Тест()
    Справочники = Новый Структура;
    Справочники = Новый Массив;
КонецПроцедуры
"#;
        assert_eq!(writes(code).len(), 2, "each illegal write is its own defect");
    }

    /// Fixture write in a managed-form method behind the given compilation directive.
    fn form_writes(directive: &str, statement: &str) -> Vec<crate::Diagnostic> {
        let fixture = format!(
            "//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
             {directive}
Процедура Тест()
    {statement}
КонецПроцедуры
"
        );
        check_hir_diagnostic_with_fixtures(&fixture)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::GlobalPropertyNotWritable)
            .collect()
    }

    #[test]
    fn a_collection_name_is_an_ordinary_local_in_client_code() {
        // The thin client does not define the name at all, so the platform declares a local.
        let found = form_writes("&НаКлиенте", "Справочники = Новый Структура;");
        assert!(found.is_empty(), "client code declares a local: {found:#?}");
    }

    #[test]
    fn a_collection_name_is_still_refused_in_server_code() {
        let found = form_writes("&НаСервере", "Справочники = Новый Структура;");
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn a_non_collection_global_property_is_refused_in_server_code() {
        let found = form_writes("&НаСервере", "Метаданные = Новый Структура;");
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn an_assignable_global_property_is_never_reported() {
        for directive in ["&НаСервере", "&НаКлиенте"] {
            let found = form_writes(directive, "РабочаяДата = Дата(2026, 1, 1);");
            assert!(found.is_empty(), "РабочаяДата accepts the write ({directive}): {found:#?}");
        }
    }

    #[test]
    fn a_client_global_property_is_refused_in_client_code() {
        let found = form_writes("&НаКлиенте", "ПараметрЗапуска = \"x\";");
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn a_collection_name_is_still_refused_where_the_module_type_is_unknown() {
        let code = "Процедура Тест()
    Справочники = Новый Структура;
КонецПроцедуры
";
        assert_eq!(writes(code).len(), 1, "an unknown environment keeps the old verdict");
        let code = "Процедура Тест()
    Метаданные = Новый Структура;
КонецПроцедуры
";
        assert!(writes(code).is_empty(), "the wider rule needs a known environment");
    }
}
