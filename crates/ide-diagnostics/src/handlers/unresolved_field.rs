use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::{Name, TypeId};

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
    receiver_ty: TypeId,
    field_name: &Name,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    let message = format!(
        "Поле '{}' не найдено у типа '{}'",
        field_name.as_str(),
        ctx.kernel_type_display(receiver_ty, ctx.locale())
    );
    crate::simple_hir_diagnostic(DiagnosticCode::UnresolvedField, message, range, ctx)
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_hir_diagnostic, check_hir_diagnostic_with_fixtures};
    use crate::DiagnosticCode;

    fn unresolved_fields(code: &str) -> Vec<crate::Diagnostic> {
        check_hir_diagnostic(code)
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedField)
            .collect()
    }

    #[test]
    fn emits_on_module_level_code_not_only_inside_methods() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
С = ОбщийМодуль.Ссылка();
Х = С.НесуществующееПоле;
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let unresolved: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::UnresolvedField).collect();
        assert_eq!(
            unresolved.len(),
            1,
            "UnresolvedField must surface for module-level expressions too, got: {diags:?}"
        );
    }

    #[test]
    fn emits_on_missing_field_of_known_catalog_ref() {
        let fixture = r#"
//- /CommonModules/ОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникСсылка.Справочник1
Функция Ссылка() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Функция Тест()
    С = ОбщийМодуль.Ссылка();
    Возврат С.НесуществующееПоле;
КонецФункции
"#;
        let diags = check_hir_diagnostic_with_fixtures(fixture);
        let unresolved: Vec<_> =
            diags.iter().filter(|d| d.code == DiagnosticCode::UnresolvedField).collect();
        assert_eq!(unresolved.len(), 1, "expected one UnresolvedField, got: {diags:?}");
        assert!(
            unresolved[0].message.contains("НесуществующееПоле"),
            "message must name the missing field, got: {}",
            unresolved[0].message
        );
    }

    #[test]
    fn emits_only_for_missing_fields_of_nonempty_closed_structure() {
        let code = r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Есть = С.СуществующееПоле;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#;

        let unresolved = unresolved_fields(code);
        assert_eq!(unresolved.len(), 1, "expected one closed-shape miss: {unresolved:?}");
        assert!(unresolved[0].message.contains("НесуществующееПоле"));
        let start: usize = unresolved[0].range.start().into();
        let end: usize = unresolved[0].range.end().into();
        assert_eq!(&code[start..end], "С.НесуществующееПоле");
    }

    #[test]
    fn literal_insert_closes_keyed_structure_but_dynamic_shapes_stay_soft() {
        let closed = r#"Процедура Тест()
    С = Новый Структура();
    С.Вставить("СуществующееПоле", 5);
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#;
        assert_eq!(unresolved_fields(closed).len(), 1);

        let soft_cases = [
            r#"Процедура Тест()
    С = Новый Структура();
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Ключ = "ДинамическоеПоле";
    С.Вставить(Ключ, 1);
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Псевдоним = С;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    С = ПолучитьСтруктуру();
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Изменить(С);
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    С.Очистить();
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Функция Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Нет = С.НесуществующееПоле;
    Возврат С;
КонецФункции"#,
            r#"// Параметры:
//   С - Структура:
//     * СуществующееПоле - Число
Процедура Тест(С)
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
        ];
        for code in soft_cases {
            assert!(
                unresolved_fields(code).is_empty(),
                "unsafe or keyless shape must stay soft:\n{code}"
            );
        }
    }

    #[test]
    fn only_proven_by_value_call_preserves_closed_shape() {
        let by_value = r#"Процедура Принять(Знач Параметр)
КонецПроцедуры

Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Принять(С);
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#;
        assert_eq!(unresolved_fields(by_value).len(), 1);

        let by_reference = by_value.replace("Знач Параметр", "Параметр");
        assert!(unresolved_fields(&by_reference).is_empty());
    }

    #[test]
    fn expression_context_escapes_keep_shape_soft() {
        let cases = [
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Если Проверить(С) Тогда КонецЕсли;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Пока Проверить(С) Цикл Прервать; КонецЦикла;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Для Индекс = Начало(С) По 1 Цикл КонецЦикла;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Для Каждого Элемент Из Получить(С) Цикл КонецЦикла;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Внешняя(Внутренняя(С));
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Выполнить С;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    ДобавитьОбработчик С, Обработчик;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    ВызватьИсключение С;
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Функция Тест(Условие)
    С = Новый Структура("СуществующееПоле", 5);
    Нет = С.НесуществующееПоле;
    Возврат ?(Условие, С, Неопределено);
КонецФункции"#,
            r#"Процедура Тест(Условие)
    С = Новый Структура("СуществующееПоле", 5);
    Псевдоним = ?(Условие, С, Неопределено);
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("СуществующееПоле", 5);
    Контейнер = Новый Массив(С);
    Нет = С.НесуществующееПоле;
КонецПроцедуры"#,
        ];

        for code in cases {
            assert!(
                unresolved_fields(code).is_empty(),
                "expression escape must keep the shape soft:\n{code}"
            );
        }
    }

    #[test]
    fn nested_structure_completeness_is_independent() {
        let code = r#"Процедура Тест()
    С = Новый Структура("Вложенная, Корневое", Новый Структура("Известное", 1), 2);
    Псевдоним = С.Вложенная;
    НетВКорне = С.НесуществующееКорневое;
    НетВнутри = С.Вложенная.НесуществующееВложенное;
КонецПроцедуры"#;

        let unresolved = unresolved_fields(code);
        assert_eq!(
            unresolved.len(),
            1,
            "only the still-closed outer shape must report: {unresolved:?}"
        );
        assert!(unresolved[0].message.contains("НесуществующееКорневое"));
    }

    /// Полнота состава ключей утверждается на месте чтения, а не «где-то в теле»: имя,
    /// жившее до своего конструктора, этим конструктором не описано.
    #[test]
    fn a_later_literal_does_not_close_a_name_that_lived_before_it() {
        let doc_typed_param = r#"// Параметры:
//   С - Структура:
//     * Б - Число
Процедура Тест(С)
    До = С.Б;
    С = Новый Структура("А", 1);
КонецПроцедуры"#;
        assert!(
            unresolved_fields(doc_typed_param).is_empty(),
            "чтение до присваивания закрыто литералом из будущего: {:?}",
            unresolved_fields(doc_typed_param)
        );

        let read_in_condition = r#"// Параметры:
//   С - Структура:
//     * Б - Число
Процедура Тест(С)
    Если С.Б = 1 Тогда
    КонецЕсли;
    С = Новый Структура("А", 1);
КонецПроцедуры"#;
        assert!(unresolved_fields(read_in_condition).is_empty());
    }

    /// Произвольный код меняет состав ключей, не называя структуру ни одним операндом.
    #[test]
    fn executing_arbitrary_code_opens_every_shape() {
        let cases = [
            r#"Процедура Тест()
    С = Новый Структура("А", 1);
    Выполнить "С.Вставить(""Б"", 1)";
    Нет = С.Б;
КонецПроцедуры"#,
            r#"Процедура Тест()
    С = Новый Структура("А", 1);
    Значение = Вычислить("Дополнить(С)");
    Нет = С.Б;
КонецПроцедуры"#,
        ];
        for code in cases {
            assert!(
                unresolved_fields(code).is_empty(),
                "произвольный код оставил форму закрытой:\n{code}"
            );
        }
    }

    /// Открытый корень не оставляет закрытой свою вложенную форму: о значении, которое
    /// могли подменить целиком, неизвестно и то, из чего состоят его поля.
    #[test]
    fn opening_a_root_opens_its_nested_shapes() {
        let code = r#"Процедура Тест()
    С = Новый Структура("Вложенная", Новый Структура("Известное", 1));
    Изменить(С);
    Нет = С.Вложенная.НеизвестноеВложенное;
КонецПроцедуры"#;
        assert!(unresolved_fields(code).is_empty(), "{:?}", unresolved_fields(code));
    }

    /// Запись под условием могла не состояться, поэтому составом ключей она не
    /// распоряжается — ни присваивание литерала, ни `Вставить`.
    #[test]
    fn a_conditionally_executed_write_does_not_close_the_shape() {
        let cases = [
            r#"// Параметры:
//   С - Структура:
//     * Б - Число
Процедура Тест(С, Условие)
    Если Условие Тогда
        С = Новый Структура("А", 1);
    КонецЕсли;
    Результат = С.Б;
КонецПроцедуры"#,
            r#"Процедура Тест(Условие)
    С = Новый Структура("K", Новый Структура("A", 1));
    Если Условие Тогда
        С.Вставить("K", Новый Структура("B", 1));
    КонецЕсли;
    Х = С.K.A;
КонецПроцедуры"#,
            r#"Процедура Тест(Коллекция)
    Для Каждого Элемент Из Коллекция Цикл
        С = Новый Структура("А", 1);
    КонецЦикла;
    Х = С.Б;
КонецПроцедуры"#,
            r#"Процедура Тест()
    Попытка
        С = Новый Структура("А", 1);
    Исключение
    КонецПопытки;
    Х = С.Б;
КонецПроцедуры"#,
        ];
        for code in cases {
            assert!(
                unresolved_fields(code).is_empty(),
                "запись под условием закрыла форму:\n{code}"
            );
        }
    }

    /// Безусловная запись состав ключей доказывает — иначе правило про условную запись
    /// закрыло бы диагностику вовсе.
    #[test]
    fn an_unconditional_write_still_closes_the_shape() {
        let code = r#"Процедура Тест()
    С = Новый Структура("А", 1);
    С.Вставить("Б", 2);
    Нет = С.В;
КонецПроцедуры"#;
        assert_eq!(unresolved_fields(code).len(), 1, "{:?}", unresolved_fields(code));
    }

    /// Платформенное имя разрешено переопределить: своя `Вычислить` строкового кода не
    /// исполняет и до чужой структуры не дотягивается.
    #[test]
    fn a_user_method_shadowing_eval_does_not_open_shapes() {
        let code = r#"Функция Вычислить()
    Возврат 0;
КонецФункции

Процедура Тест()
    С = Новый Структура("А", 1);
    Игнорировать = Вычислить();
    Нет = С.Б;
КонецПроцедуры"#;
        assert_eq!(unresolved_fields(code).len(), 1, "{:?}", unresolved_fields(code));
    }

    /// Методов у структуры пять, и два из них состава ключей не меняют: после них
    /// диагностика остаётся, после остальных — нет.
    #[test]
    fn only_a_key_changing_method_opens_the_shape() {
        let preserving = ["С.Количество();", "С.Свойство(\"А\", Значение);"];
        for call in preserving {
            let code = format!(
                "Процедура Тест()\n    С = Новый Структура(\"А\", 1);\n    {call}\n    Нет = С.Б;\nКонецПроцедуры"
            );
            assert_eq!(unresolved_fields(&code).len(), 1, "{call} потерял диагностику");
        }

        let changing = ["С.Удалить(\"А\");", "С.Очистить();", "С.НеизвестныйМетод();"];
        for call in changing {
            let code = format!(
                "Процедура Тест()\n    С = Новый Структура(\"А\", 1);\n    {call}\n    Нет = С.Б;\nКонецПроцедуры"
            );
            assert!(unresolved_fields(&code).is_empty(), "{call} оставил форму закрытой");
        }
    }

    /// Правая часть конструктора читает ПРЕЖНЕЕ значение имени, а значит, имя жило до
    /// этого литерала.
    #[test]
    fn a_read_in_the_constructor_arguments_counts_as_earlier_life() {
        let code = r#"// Параметры:
//   С - Структура:
//     * Б - Число
Процедура Тест(С)
    С = Новый Структура("А", С.Б);
КонецПроцедуры"#;
        assert!(unresolved_fields(code).is_empty(), "{:?}", unresolved_fields(code));
    }

    /// Приёмник, который не является цепочкой имён, всё равно называет корни — и мутация
    /// через него может достаться любому из них.
    #[test]
    fn a_mutation_through_a_computed_receiver_opens_the_named_roots() {
        let cases = [
            r#"Процедура Тест(Условие)
    С = Новый Структура("А", 1);
    ?(Условие, С, С).Вставить("Б", 2);
    Нет = С.Б;
КонецПроцедуры"#,
            r#"Процедура Тест(Условие)
    С = Новый Структура("А", 1);
    ?(Условие, С, С).Очистить();
    Нет = С.Б;
КонецПроцедуры"#,
        ];
        for code in cases {
            assert!(
                unresolved_fields(code).is_empty(),
                "мутация через вычисляемый приёмник оставила форму закрытой:\n{code}"
            );
        }

        // Приёмник, который корня не называет, диагностику не отменяет.
        let untouched = r#"Процедура Тест(Массив)
    С = Новый Структура("А", 1);
    Массив[0].Очистить();
    Нет = С.Б;
КонецПроцедуры"#;
        assert_eq!(unresolved_fields(untouched).len(), 1, "{:?}", unresolved_fields(untouched));
    }

    fn unresolved_in_fixture(fixture: &str) -> Vec<crate::Diagnostic> {
        check_hir_diagnostic_with_fixtures(fixture)
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedField)
            .collect()
    }

    #[test]
    fn loop_variable_over_a_metadata_collection_stays_soft() {
        // The second loop iterates an unknown collection, and the variable would keep the
        // first loop's kind if the element were typed by it: `Регистр.ТабличныеЧасти` below
        // is valid code on a catalog.
        let fixture = r#"
//- /test.bsl
Процедура Тест(Вид)
    Для Каждого Описание Из Метаданные.РегистрыСведений Цикл
        Нет = Описание.НетТакогоСвойства;
    КонецЦикла;
    Для Каждого Описание Из Метаданные[Вид] Цикл
        Есть = Описание.ТабличныеЧасти;
    КонецЦикла;
КонецПроцедуры
"#;
        let unresolved = unresolved_in_fixture(fixture);
        assert!(unresolved.is_empty(), "loop variables must stay soft: {unresolved:?}");
    }

    #[test]
    fn metadata_shapes_without_a_trusted_member_list_stay_silent() {
        // Nested collections, generic descriptions, the configuration root and the kinds
        // whose platform member lists are not trusted as complete must not accuse anything.
        let fixture = r#"
//- /test.bsl
Процедура Тест()
    А = Метаданные.Справочники.Справочник1.Реквизиты.НетТакогоРеквизита;
    Б = Метаданные.НайтиПоПолномуИмени("Справочник.Справочник1").НетТакогоСвойства;
    В = Метаданные.ОсновнаяРоль;
    Г = Метаданные.ОбщиеМодули.НетТакогоМодуля;
    Д = Метаданные.Справочники.Справочник1.ОсновнаяФормаОбъекта.НетТакогоСвойства;
    Для Каждого Описание Из Метаданные.Справочники.Справочник1.Реквизиты Цикл
        Е = Описание.НетТакогоСвойства;
    КонецЦикла;
КонецПроцедуры
"#;
        let unresolved = unresolved_in_fixture(fixture);
        assert!(unresolved.is_empty(), "untrusted metadata shapes must stay soft: {unresolved:?}");
    }

    #[test]
    fn metadata_collection_miss_is_silent_without_a_visible_configuration() {
        // An external processor or a loose module may name objects of a configuration it
        // cannot see; the miss proves nothing there.
        let code = r#"Процедура Тест()
    Нет = Метаданные.Справочники.НетТакогоСправочника;
КонецПроцедуры"#;
        assert!(unresolved_fields(code).is_empty(), "{:?}", unresolved_fields(code));
    }
}
