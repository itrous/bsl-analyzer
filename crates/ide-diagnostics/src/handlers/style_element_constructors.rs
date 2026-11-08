use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Minor,
    scope: DiagnosticScope::Bsl,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Badpractice],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

pub fn from_hir(
    type_name: &str,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let code = DiagnosticCode::StyleElementConstructors;

    if ctx.is_disabled_with_metadata(code) {
        return None;
    }

    Some(Diagnostic {
        code,
        message: format!("Замените конструктор {} на получение элемента стиля", type_name),
        severity: ctx.severity(code),
        range,
        tags: ctx.tags(code),
        fixes: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::check_diagnostics_snapshot_for;
    use expect_test::expect;

    #[test]
    fn test_style_items_and_other_constructors_are_silent() {
        let code = r#"Процедура ОформитьПоле(Поле)
	Поле.ЦветТекста = ЦветаСтиля.ЦветОсобогоТекста;
	Поле.Шрифт = ШрифтыСтиля.ШрифтЗаголовка;
	Параметры = Новый Структура("Цвет, Шрифт", Поле.ЦветТекста, Поле.Шрифт);
	Отбор = Новый("Массив");
	Текст = Новый ТекстовыйДокумент;
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_direct_constructor_russian() {
        let code = r#"Процедура ОформитьПоле(Поле)
	Поле.ЦветТекста = Новый Цвет(200, 30, 30);
	Поле.Шрифт = Новый Шрифт(, 12, Истина);
	Поле.Рамка = Новый Рамка(ТипРамкиЭлементаУправления.Одинарная, 1);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#"
                StyleElementConstructors @ 2:20..2:43
                  message: Замените конструктор Цвет на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 3:15..3:40
                  message: Замените конструктор Шрифт на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 4:15..4:67
                  message: Замените конструктор Рамка на получение элемента стиля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_string_constructor_russian() {
        let code = r#"ЦветПредупреждения = Новый("Цвет", 250, 180, 0);
ШрифтПримечания = Новый("Шрифт", , 8);
РамкаКарточки = Новый("Рамка", ТипРамкиЭлементаУправления.Двойная);
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#"
                StyleElementConstructors @ 1:22..1:48
                  message: Замените конструктор Цвет на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 2:19..2:38
                  message: Замените конструктор Шрифт на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 3:17..3:67
                  message: Замените конструктор Рамка на получение элемента стиля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_english_constructors() {
        let code = r#"Procedure Decorate(Field)
	Field.TextColor = New Color(10, 120, 60);
	Field.Font = New Font(, 10);
	Field.Border = New Border(ControlBorderType.Single);
EndProcedure
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#"
                StyleElementConstructors @ 2:20..2:42
                  message: Замените конструктор Color на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 3:15..3:29
                  message: Замените конструктор Font на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 4:17..4:53
                  message: Замените конструктор Border на получение элемента стиля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_auto_value_constructors_are_silent() {
        let code = r#"Процедура СброситьОформление(Надпись, Ячейка)
	Надпись.ЦветТекста = Новый Цвет;
	Надпись.ЦветФона = Новый Цвет();
	Надпись.Шрифт = Новый Шрифт;
	Ячейка.Шрифт = Новый Шрифт();
	Ячейка.ЦветРамки = Новый("Цвет");
	Ячейка.Шрифт = Новый("Шрифт");
	ЗапомнитьОформление(Новый Цвет, Новый Шрифт);
КонецПроцедуры

Procedure ResetLook(Caption)
	Caption.TextColor = New Color;
	Caption.BackColor = New Color();
	Caption.Font = New Font;
	Caption.Font = New("Font");
EndProcedure
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_comparison_operands_are_silent() {
        let code = r#"Функция ЯчейкаОтмечена(Ячейка)
	Если Ячейка.ЦветФона = Новый Цвет(250, 230, 10) Тогда
		Возврат Истина;
	ИначеЕсли Ячейка.ЦветТекста <> Новый Цвет(120, 120, 120) Тогда
		Возврат Ложь;
	КонецЕсли;
	Отметка = (Ячейка.ЦветФона = Новый Цвет(5, 15, 25));
	Возврат Отметка Или Новый Шрифт("Courier New", 11) = Ячейка.Шрифт
		Или Ячейка.ЦветРамки = (Новый("Цвет", 40, 50, 60));
КонецФункции

Function IsMarked(Cell)
	Return Cell.BackColor = New Color(1, 99, 199);
EndFunction
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#""#]],
        );
    }

    #[test]
    fn test_ternary_branch_compared_is_silent_but_condition_is_reported() {
        let code = r#"Функция ЯчейкаОтмечена(Ячейка, Выделить, А, Б)
	Если ?(Выделить, Новый Цвет(1, 2, 3), Новый Цвет(4, 5, 6)) = Ячейка.ЦветФона Тогда
		Возврат Истина;
	КонецЕсли;
	Возврат Ячейка.Шрифт <> ?(Выделить, (Новый Шрифт("Arial", 10)), Ячейка.Шрифт)
		Или ?(Новый Цвет(7, 8, 9), А, Б) = А;
КонецФункции
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#"
                StyleElementConstructors @ 6:9..6:28
                  message: Замените конструктор Цвет на получение элемента стиля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_torn_if_inside_arguments_is_reported() {
        let code = r#"Процедура Оформить(Надпись)
	Надпись.ЦветТекста = Новый Цвет(#Если Сервер Тогда
#Иначе
255, 0, 0
#КонецЕсли);
КонецПроцедуры
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#"
                StyleElementConstructors @ 2:23..5:12
                  message: Замените конструктор Цвет на получение элемента стиля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_constructors_with_values_are_still_reported() {
        let code = r#"Функция ОформитьЯчейку(Надпись, Ячейка, Выделить)
	Надпись.ЦветТекста = Новый Цвет(130, 0, 70);
	Ячейка.Шрифт = Новый Шрифт(, , Истина);
	Ячейка.Шрифт = Новый Шрифт(,);
	Надпись.Рамка = Новый Рамка;
	ЗапомнитьОформление(Новый Цвет(1, 2, 3));
	Ячейка.ЦветФона = ?(Выделить, Новый Цвет(7, 8, 9), Новый Цвет);
	Возврат Новый Color(64, 64, 64);
КонецФункции
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#"
                StyleElementConstructors @ 2:23..2:45
                  message: Замените конструктор Цвет на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 3:17..3:40
                  message: Замените конструктор Шрифт на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 4:17..4:31
                  message: Замените конструктор Шрифт на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 5:18..5:29
                  message: Замените конструктор Рамка на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 6:22..6:41
                  message: Замените конструктор Цвет на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 7:32..7:51
                  message: Замените конструктор Цвет на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 8:10..8:33
                  message: Замените конструктор Color на получение элемента стиля
                  severity: Warning"#]],
        );
    }

    #[test]
    fn test_nested_constructors() {
        let code = r#"Настройки = Новый Соответствие;
Настройки.Вставить("Акцент", Новый ХранилищеЗначения(Новый Цвет(0, 90, 160)));
Настройки.Вставить("Подпись", Новый ХранилищеЗначения(Новый("Шрифт", , 9)));
Настройки.Вставить("Окантовка", Новый("ХранилищеЗначения", Новый Рамка(ТипРамкиЭлементаУправления.Выпуклая)));
"#;
        check_diagnostics_snapshot_for(
            code,
            DiagnosticCode::StyleElementConstructors,
            expect![[r#"
                StyleElementConstructors @ 2:54..2:76
                  message: Замените конструктор Цвет на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 3:55..3:74
                  message: Замените конструктор Шрифт на получение элемента стиля
                  severity: Warning
                StyleElementConstructors @ 4:60..4:108
                  message: Замените конструктор Рамка на получение элемента стиля
                  severity: Warning"#]],
        );
    }
}
