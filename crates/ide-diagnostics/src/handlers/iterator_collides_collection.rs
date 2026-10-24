use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 2,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    iterator_name: &str,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::IteratorCollidesCollection;
    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    Some(Diagnostic {
        code,
        message: format!(
            "Переменная «{iterator_name}» одновременно коллекция и итератор: после первой итерации коллекция будет перезаписана элементом"
        ),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use crate::test_utils::check_diagnostics_snapshot_for;
    use crate::DiagnosticCode;
    use expect_test::expect;

    fn check(code: &str, expected: expect_test::Expect) {
        check_diagnostics_snapshot_for(code, DiagnosticCode::IteratorCollidesCollection, expected);
    }

    #[test]
    fn issue_pair_with_used_iterator() {
        check(
            r#"
Процедура Тест()
    Коллекция = Новый Массив;
    Коллекция.Добавить(1);
    Для Каждого Коллекция Из Коллекция Цикл
        Сообщить(Коллекция);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#"
                IteratorCollidesCollection @ 5:17..5:26
                  message: Переменная «Коллекция» одновременно коллекция и итератор: после первой итерации коллекция будет перезаписана элементом
                  severity: Critical"#]],
        );
    }

    #[test]
    fn unused_iterator() {
        check(
            r#"
Процедура Тест()
    Коллекция = Новый Массив;
    Для Каждого Коллекция Из Коллекция Цикл
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#"
                IteratorCollidesCollection @ 4:17..4:26
                  message: Переменная «Коллекция» одновременно коллекция и итератор: после первой итерации коллекция будет перезаписана элементом
                  severity: Critical"#]],
        );
    }

    #[test]
    fn english_keywords() {
        check(
            r#"
Procedure Test()
    Items = New Array;
    For Each Items In Items Do
        Message(Items);
    EndDo;
EndProcedure
"#,
            expect![[r#"
                IteratorCollidesCollection @ 4:14..4:19
                  message: Переменная «Items» одновременно коллекция и итератор: после первой итерации коллекция будет перезаписана элементом
                  severity: Critical"#]],
        );
    }

    #[test]
    fn different_name_case() {
        check(
            r#"
Процедура Тест()
    Коллекция = Новый Массив;
    Для Каждого коллекция Из КОЛЛЕКЦИЯ Цикл
        Сообщить(коллекция);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#"
                IteratorCollidesCollection @ 4:17..4:26
                  message: Переменная «коллекция» одновременно коллекция и итератор: после первой итерации коллекция будет перезаписана элементом
                  severity: Critical"#]],
        );
    }

    #[test]
    fn module_variable_collection() {
        check(
            r#"
Перем Коллекция;

Процедура Тест()
    Для Каждого Коллекция Из Коллекция Цикл
        Сообщить(Коллекция);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#"
                IteratorCollidesCollection @ 5:17..5:26
                  message: Переменная «Коллекция» одновременно коллекция и итератор: после первой итерации коллекция будет перезаписана элементом
                  severity: Critical"#]],
        );
    }

    #[test]
    fn parenthesized_source() {
        check(
            r#"
Процедура Тест()
    Коллекция = Новый Массив;
    Для Каждого Коллекция Из (Коллекция) Цикл
        Сообщить(Коллекция);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#"
                IteratorCollidesCollection @ 4:17..4:26
                  message: Переменная «Коллекция» одновременно коллекция и итератор: после первой итерации коллекция будет перезаписана элементом
                  severity: Critical"#]],
        );
    }

    #[test]
    fn fixed_variant() {
        check(
            r#"
Процедура Тест()
    Коллекция = Новый Массив;
    Для Каждого Элемент Из Коллекция Цикл
        Сообщить(Элемент);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn qualified_source() {
        check(
            r#"
Процедура Тест(Объект)
    Для Каждого Коллекция Из Объект.Коллекция Цикл
        Сообщить(Коллекция);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn alias_is_a_different_symbol() {
        check(
            r#"
Процедура Тест()
    А = Новый Массив;
    Б = А;
    Для Каждого А Из Б Цикл
        Сообщить(А);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn call_source() {
        check(
            r#"
Процедура Тест()
    Для Каждого Коллекция Из Коллекция() Цикл
        Сообщить(Коллекция);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn index_source() {
        check(
            r#"
Процедура Тест(Список)
    Для Каждого Коллекция Из Список[0] Цикл
        Сообщить(Коллекция);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }

    #[test]
    fn method_call_source() {
        check(
            r#"
Процедура Тест(Список)
    Для Каждого Коллекция Из Коллекция.Получить() Цикл
        Сообщить(Коллекция);
    КонецЦикла;
КонецПроцедуры
"#,
            expect![[r#""#]],
        );
    }
}
