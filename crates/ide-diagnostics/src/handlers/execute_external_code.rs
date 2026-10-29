use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Vulnerability,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[
        bsl_metadata::ModuleType::CommandModule,
        bsl_metadata::ModuleType::ExternalConnectionModule,
        bsl_metadata::ModuleType::FormModule,
        bsl_metadata::ModuleType::ObjectModule,
        bsl_metadata::ModuleType::OrdinaryApplicationModule,
    ],
    minutes_to_fix: 1,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Standard],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(range: LocalRange, ctx: &AnalysisContext) -> Option<Diagnostic<LocalRange>> {
    // An ORDINARY form module is thick-client-only code: it has no server context, and the
    // standard exempts code running on the client. The `&НаКлиенте` exemption in lowering is
    // syntactic, and a compilation directive there does not move a method off the thick
    // client, so even `&НаСервере` code is client code.
    if ctx.is_ordinary_form_module() {
        return None;
    }
    crate::simple_hir_diagnostic(
        DiagnosticCode::ExecuteExternalCode,
        "Запрещено выполнять внешний код на сервере",
        range,
        ctx,
    )
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::ExecuteExternalCode, expected);
    }

    #[test]
    fn test_client_only_exemption() {
        let code = r#"&НаКлиенте
Процедура ПрименитьФормулуСкидки(ТекстФормулы)
	Выполнить(ТекстФормулы);
КонецПроцедуры

&НаКлиенте
Функция ЗначениеФормулы(ТекстФормулы)
	Возврат Вычислить(ТекстФормулы);
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }

    #[test]
    fn test_server_annotation() {
        let code = r#"&НаСервере
Процедура ПрименитьФормулуСкидки(ТекстФормулы)
	Выполнить(ТекстФормулы);
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            ExecuteExternalCode @ 3:2..3:26
              message: Запрещено выполнять внешний код на сервере
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_execute_on_server_without_context() {
        let code = r#"&НаСервереБезКонтекста
Процедура ПересчитатьПоФормуле(ТекстФормулы)
	Выполнить(ТекстФормулы);
КонецПроцедуры
"#;
        check(
            code,
            expect![[r#"
            ExecuteExternalCode @ 3:2..3:26
              message: Запрещено выполнять внешний код на сервере
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_eval_on_client_server_without_context() {
        let code = r#"&НаКлиентеНаСервереБезКонтекста
Функция ЗначениеФормулы(ТекстФормулы)
	Возврат Вычислить(ТекстФормулы);
КонецФункции
"#;
        check(
            code,
            expect![[r#"
            ExecuteExternalCode @ 3:10..3:33
              message: Запрещено выполнять внешний код на сервере
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_client_at_server_annotation() {
        let code = r#"&НаКлиентеНаСервере
Функция ЗначениеФормулы(ТекстФормулы)
	Возврат Вычислить(ТекстФормулы);
КонецФункции
"#;
        check(
            code,
            expect![[r#"
            ExecuteExternalCode @ 3:10..3:33
              message: Запрещено выполнять внешний код на сервере
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_methods_without_annotations() {
        let code = r#"Процедура ПрименитьФормулуСкидки(ТекстФормулы)
	Выполнить(ТекстФормулы);
КонецПроцедуры

Функция ЗначениеФормулы(ТекстФормулы)
	Возврат Вычислить(ТекстФормулы);
КонецФункции
"#;
        check(
            code,
            expect![[r#"
            ExecuteExternalCode @ 2:2..2:26
              message: Запрещено выполнять внешний код на сервере
              severity: Critical
            ExecuteExternalCode @ 6:10..6:33
              message: Запрещено выполнять внешний код на сервере
              severity: Critical"#]],
        );
    }

    #[test]
    fn test_qualified_and_similar_names_ignored() {
        let code = r#"&НаСервере
Функция ЗначениеФормулы(Калькулятор, ТекстФормулы)
	Промежуточное = Калькулятор.Вычислить(ТекстФормулы);
	Калькулятор.Выполнить();
	Возврат ВычислитьОкругленно(Промежуточное);
КонецФункции
"#;
        check(code, expect![[r#""#]]);
    }
}
