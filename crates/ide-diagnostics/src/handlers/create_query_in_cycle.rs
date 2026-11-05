use crate::define_metadata;
use crate::metadata::*;
use crate::suppression::SuppressionLookup;
use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};
use hir::{query_in_loop_candidates, DefWithBodyId, QueryCallKey, QueryInLoopCandidate};
use rustc_hash::FxHashSet;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 20,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Performance],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const DIRECT_MESSAGE: &str =
    "Выполнение запроса в цикле приводит к деградации производительности. \
     Создайте запрос один раз до цикла и изменяйте только параметры внутри цикла";

/// Judged per file, not per body: the facts come from the fold of the module that the base-aware
/// passes also read, and their ranges are placed by that same provider's source maps.
pub fn check(ctx: &DiagnosticsContext) -> Vec<Diagnostic> {
    let code = DiagnosticCode::CreateQueryInCycle;
    let local_call_code = DiagnosticCode::LocalCallWithQueryInCycle;
    // Each code is disabled on its own; `simple_hir_diagnostic` drops the findings of a disabled
    // one, so the whole pass is skipped only when both are off.
    if ctx.is_disabled_with_metadata(code) && ctx.is_disabled_with_metadata(local_call_code) {
        return Vec::new();
    }
    let infer = ctx.infer();
    let bodies = ctx.module_bodies();
    let site_range = |site: &QueryCallKey| {
        let source_map = match site.owner {
            DefWithBodyId::Method(key) => bodies.source_map(key),
            DefWithBodyId::ModuleCode => {
                bodies.module_code_result().map(|lower| lower.source_map())
            }
        }?;
        source_map.expr_range(site.call)
    };

    // A suppressed execution is no ground for any caller, so it is cut before reachability. A
    // suppressed call is another matter: the finalizer hides that finding alone, and its callee
    // stays a ground for the other callers.
    let suppressions = SuppressionLookup::new(
        &ctx.parse().syntax_node(),
        &ctx.file_text(),
        ctx.config.bslls_suppression_compat,
    );
    let active: FxHashSet<QueryCallKey> = infer
        .query_facts_by_body
        .iter()
        .flat_map(|(&owner, facts)| {
            facts
                .executions
                .iter()
                .map(move |execution| QueryCallKey { owner, call: execution.call })
        })
        .filter(|site| {
            site_range(site).is_some_and(|range| !suppressions.is_suppressed(range.start(), code))
        })
        .collect();

    query_in_loop_candidates(&infer.query_facts_by_body, &active)
        .into_iter()
        .filter_map(|candidate| {
            let (site, finding_code, message) = match candidate {
                QueryInLoopCandidate::Execution(site) => (site, code, DIRECT_MESSAGE.to_string()),
                QueryInLoopCandidate::LocalCall { site, target } => {
                    let name = ctx.interface_method(target)?.name.clone();
                    (
                        site,
                        local_call_code,
                        format!(
                            "Вызов локального метода «{name}» в теле цикла приводит к выполнению \
                             запроса непосредственно или через локальные методы этого модуля. \
                             Получите необходимые данные одним запросом до цикла"
                        ),
                    )
                }
            };
            crate::simple_hir_diagnostic(finding_code, message, site_range(&site)?, ctx.analysis())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::check_diagnostics_snapshot_for;
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::CreateQueryInCycle, expected);
    }

    #[test]
    fn test_query_executed_outside_loop() {
        let code = r#"Функция ЦеныТарифов(Тарифы)
	Запрос = Новый Запрос("ВЫБРАТЬ Т.Цена ИЗ Справочник.Тарифы КАК Т ГДЕ Т.Ссылка В (&Тарифы)");
	Запрос.УстановитьПараметр("Тарифы", Тарифы);
	Выборка = Запрос.Выполнить().Выбрать();
	Цены = Новый Массив;
	Пока Выборка.Следующий() Цикл
		Цены.Добавить(Выборка.Цена);
	КонецЦикла;
	Возврат Цены;
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_query_created_outside_loop_but_executed_inside_loop() {
        let code = r#"Функция ЦеныТарифов(Тарифы)
	Запрос = Новый Запрос("ВЫБРАТЬ Т.Цена ИЗ Справочник.Тарифы КАК Т ГДЕ Т.Ссылка = &Тариф");
	Цены = Новый Массив;
	Для Каждого Тариф Из Тарифы Цикл
		Запрос.УстановитьПараметр("Тариф", Тариф);
		Цены.Добавить(Запрос.Выполнить().Выгрузить());
	КонецЦикла;
	Возврат Цены;
КонецФункции
"#;
        check(
            code,
            expect![[r#"
            CreateQueryInCycle @ 6:17..6:35
              message: Выполнение запроса в цикле приводит к деградации производительности. Создайте запрос один раз до цикла и изменяйте только параметры внутри цикла
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_query_created_and_executed_in_while_loop() {
        let code = r#"Процедура ОбойтиСтраницы(Курсор)
	Пока Курсор.ЕстьЕще() Цикл
		Страница = Новый Запрос;
		Страница.Текст = Курсор.ТекстСтраницы();
		Страница.Выполнить();
	КонецЦикла;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            CreateQueryInCycle @ 5:3..5:23
              message: Выполнение запроса в цикле приводит к деградации производительности. Создайте запрос один раз до цикла и изменяйте только параметры внутри цикла
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_english_keywords() {
        let code = r#"Procedure WalkPages(Pages)
	For Each Page In Pages Do
		Request = New Query(Page.Text);
		Request.Execute();
	EndDo;
EndProcedure
"#;
        check(
            code,
            expect![[r#"
            CreateQueryInCycle @ 4:3..4:20
              message: Выполнение запроса в цикле приводит к деградации производительности. Создайте запрос один раз до цикла и изменяйте только параметры внутри цикла
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_case_insensitive() {
        let code = r#"Процедура ОбойтиСтраницы(ЧислоСтраниц)
	Для Номер = 1 По ЧислоСтраниц Цикл
		Страница = Новый запрос;
		Страница.выполнить();
	КонецЦикла;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            CreateQueryInCycle @ 4:3..4:23
              message: Выполнение запроса в цикле приводит к деградации производительности. Создайте запрос один раз до цикла и изменяйте только параметры внутри цикла
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_query_builder() {
        let code = r#"Процедура ПостроитьОтчеты(Периоды)
	Построитель = Новый ПостроительЗапроса;
	Для Каждого Период Из Периоды Цикл
		Построитель.Выполнить();
	КонецЦикла;
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            CreateQueryInCycle @ 4:3..4:26
              message: Выполнение запроса в цикле приводит к деградации производительности. Создайте запрос один раз до цикла и изменяйте только параметры внутри цикла
              severity: Critical"#]],
        );
    }
}
