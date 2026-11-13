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
    fn loop_variable_over_a_metadata_collection_is_checked_until_rebound() {
        // The first loop's variable is an information-register description, so a member the
        // description lacks is reported. The second loop iterates an unknown collection and
        // rebinds the name: `ТабличныеЧасти` there is valid code on a catalog, and judging it
        // by the first loop's kind would accuse it.
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
        assert_eq!(unresolved.len(), 1, "{unresolved:?}");
        assert!(unresolved[0].message.contains("НетТакогоСвойства"), "{unresolved:?}");
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

    const DESCRIPTION_MODULE: &str = "CommonModules/Проба/Ext/Module.bsl";

    const DESCRIPTION_CONFIGURATION_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Configuration uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Тест</Name></Properties>
        <ChildObjects>
            <Catalog>Товары</Catalog>
            <CommonModule>Проба</CommonModule>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#;

    const DESCRIPTION_CATALOG_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.20">
    <Catalog uuid="00000000-0000-0000-0000-000000000002">
        <Properties>
            <Name>Товары</Name>
            <CodeLength>9</CodeLength>
            <DescriptionLength>100</DescriptionLength>
        </Properties>
        <ChildObjects>
            <Attribute uuid="00000000-0000-0000-0000-000000000003">
                <Properties>
                    <Name>Артикул</Name>
                    <Type>
                        <v8:Type>xs:string</v8:Type>
                        <v8:StringQualifiers>
                            <v8:Length>50</v8:Length>
                            <v8:AllowedLength>Variable</v8:AllowedLength>
                        </v8:StringQualifiers>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

    const DESCRIPTION_MODULE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <CommonModule uuid="00000000-0000-0000-0000-000000000008">
        <Properties>
            <Name>Проба</Name>
            <Server>true</Server>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

    /// Findings of `code` over a body checked in a configuration that holds the catalog
    /// `Товары` with one attribute, `Артикул`.
    fn description_findings(body: &str, code: DiagnosticCode) -> Vec<String> {
        let source = format!("Процедура Тест(Имя)\n{body}КонецПроцедуры\n");
        let files = [
            ("Configuration.xml", DESCRIPTION_CONFIGURATION_XML),
            ("Catalogs/Товары.xml", DESCRIPTION_CATALOG_XML),
            ("CommonModules/Проба.xml", DESCRIPTION_MODULE_XML),
            (DESCRIPTION_MODULE, source.as_str()),
        ];
        crate::test_utils::check_config_files(&files, DESCRIPTION_MODULE)
            .into_iter()
            .filter(|diag| diag.code == code)
            .map(|diag| diag.message)
            .collect()
    }

    fn description_misses(body: &str) -> Vec<String> {
        description_findings(body, DiagnosticCode::UnresolvedField)
    }

    #[test]
    fn a_declared_attribute_of_a_catalog_description_resolves_silently() {
        let found = description_misses(
            "    А = Метаданные.Справочники.Товары.Реквизиты.Артикул;\n    \
             Б = Метаданные.Справочники.товары.Реквизиты.артикул;\n    \
             В = Метаданные.Справочники.Товары.Реквизиты.Артикул.Имя;\n",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_misspelled_attribute_of_a_catalog_description_is_reported() {
        let found = description_misses("    А = Метаданные.Справочники.Товары.Реквизиты.Артикл;\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("'Артикл'"), "{found:?}");
        assert!(found[0].contains("КоллекцияОбъектовМетаданных"), "{found:?}");
    }

    #[test]
    fn a_misspelled_attribute_is_reported_through_a_local_description() {
        let found = description_misses(
            "    Описание = Метаданные.Справочники.Товары;\n    Нет = Описание.Реквизиты.Артикл;\n",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("'Артикл'"), "{found:?}");
    }

    #[test]
    fn the_description_keeps_answering_for_its_own_members_as_the_bare_one_did() {
        // An own member resolves; an unknown one is reported with the platform description
        // type in the message, exactly as before the description remembered its object.
        let found = description_misses(
            "    А = Метаданные.Справочники.Товары.Имя;\n    \
             Б = Метаданные.Справочники.Товары.Синоним;\n    \
             Нет = Метаданные.Справочники.Товары.ТакогоСвойстваНет;\n",
        );
        assert_eq!(
            found,
            vec!["Поле 'ТакогоСвойстваНет' не найдено у типа 'ОбъектМетаданных: Справочник'"
                .to_string()]
        );
    }

    #[test]
    fn a_description_rebound_on_another_path_does_not_judge_its_attributes() {
        // After the `Если` the local may hold a description of an object the walk cannot
        // name, and that object may well own the attribute.
        let found = description_misses(
            "    Описание = Метаданные.Справочники.Товары;\n    \
             Если Имя = 1 Тогда\n        Описание = Метаданные.Справочники[Имя];\n    \
             КонецЕсли;\n    Нет = Описание.Реквизиты.Артикл;\n",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    /// The member surface of the attribute description is attested at 8.3.27, so a member
    /// the platform does not have is a defect of the code. The verdict is of a known release:
    /// a versionless corpus keeps the gate shut by contract, and the member is then silent.
    #[test]
    #[cfg_attr(
        corpus_contract,
        ignore = "the corpus profile serves a versionless corpus: the completeness gate is shut by contract"
    )]
    fn an_unknown_member_of_an_attribute_description_is_reported() {
        let found = description_misses(
            "    А = Метаданные.Справочники.Товары.Реквизиты.Артикул.ИспользованиеРеквизита;
                 Описание = Метаданные.Справочники.Товары.Реквизиты.Артикул;
                 Б = Описание.НетТакогоПоляВовсе;
",
        );
        if crate::test_utils::built_in_facts_are_served() {
            assert_eq!(found.len(), 2, "{found:?}");
            assert!(found[0].contains("'ИспользованиеРеквизита'"), "{found:?}");
            assert!(found[0].contains("ОбъектМетаданных: Реквизит"), "{found:?}");
            assert!(found[1].contains("'НетТакогоПоляВовсе'"), "{found:?}");
        } else {
            assert!(
                found.is_empty(),
                "an unknown platform version keeps the miss silent: {found:?}"
            );
        }
    }

    /// The method surface of the attribute description is attested too, so a call the
    /// platform does not have is reported against the description type. As with the
    /// fields, a versionless corpus keeps the gate shut by contract.
    #[test]
    #[cfg_attr(
        corpus_contract,
        ignore = "the corpus profile serves a versionless corpus: the completeness gate is shut by contract"
    )]
    fn an_unknown_method_of_an_attribute_description_is_reported() {
        let found = description_findings(
            "    Метаданные.Справочники.Товары.Реквизиты.Артикул.НетТакогоМетода();\n",
            DiagnosticCode::UnresolvedMethodCall,
        );
        if crate::test_utils::built_in_facts_are_served() {
            assert_eq!(found.len(), 1, "{found:?}");
            assert!(found[0].contains("'НетТакогоМетода'"), "{found:?}");
            assert!(found[0].contains("ОбъектМетаданных: Реквизит"), "{found:?}");
        } else {
            assert!(
                found.is_empty(),
                "an unknown platform version keeps the miss silent: {found:?}"
            );
        }
    }

    /// The methods the 8.3.27 extract lists for the attribute description never report,
    /// whether called on the chain itself or through a local.
    #[test]
    fn each_method_of_an_attribute_description_resolves_silently() {
        let found = description_findings(
            "    А = Метаданные.Справочники.Товары.Реквизиты.Артикул.ПолноеИмя();
                 Описание = Метаданные.Справочники.Товары.Реквизиты.Артикул;
                 Б = Описание.Представление();
                 В = Описание.Родитель();
                 Г = Описание.FullName();
",
            DiagnosticCode::UnresolvedMethodCall,
        );
        assert!(found.is_empty(), "{found:?}");
    }

    /// The ten members the corpus scan found on attribute descriptions, in both spellings
    /// of the type's own members, resolve silently: each is a member of the extract.
    #[test]
    fn each_member_the_corpus_uses_on_an_attribute_description_resolves_silently() {
        let found = description_misses(
            "    Описание = Метаданные.Справочники.Товары.Реквизиты.Артикул;
                 А = Описание.Тип;
                 Б = Описание.ПараметрыВыбора;
                 В = Описание.Имя;
                 Г = Описание.Подсказка;
                 Д = Описание.Синоним;
                 Е = Описание.ЗначениеЗаполнения;
                 Ж = Описание.ПолноеИмя();
                 З = Описание.Представление();
                 И = Описание.СвязиПараметровВыбора;
                 К = Описание.МаксимальноеЗначение;
                 Л = Описание.Использование;
                 М = Описание.Name;
",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_unknown_object_name_gives_no_finding_on_its_attribute_access() {
        // The missing catalog is the one finding; its attributes are not judged. A name
        // computed at runtime leaves the object unknown, and nothing is reported at all.
        let found = description_misses(
            "    А = Метаданные.Справочники.НетТакогоСправочника.Реквизиты.Артикл;\n    \
             Б = Метаданные.Справочники[Имя].Реквизиты.Артикл;\n    \
             В = Метаданные.НайтиПоПолномуИмени(Имя).Реквизиты.Артикл;\n",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("'НетТакогоСправочника'"), "{found:?}");
    }

    #[test]
    fn a_description_whose_object_is_not_visible_does_not_judge_its_attributes() {
        // Outside a configuration the description does not remember an object, so the
        // attribute collection stays the bare platform one.
        let code = r#"Процедура Тест()
    Нет = Метаданные.Справочники.Товары.Реквизиты.Артикл;
КонецПроцедуры"#;
        assert!(unresolved_fields(code).is_empty(), "{:?}", unresolved_fields(code));
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

    /// The drawings collection of a spreadsheet document is attested for its methods only:
    /// it addresses its elements by name, so `Рисунки.<имя рисунка>` is a property read the
    /// platform answers, and the absent-field check must stay silent on it.
    #[test]
    fn a_name_read_from_a_methods_only_attested_collection_stays_silent() {
        let unresolved = unresolved_fields(
            r#"Процедура Тест()
    ТабДок = Новый ТабличныйДокумент;
    Рисунок = ТабДок.Рисунки.Диаграмма1;
КонецПроцедуры"#,
        );
        assert!(
            unresolved.is_empty(),
            "an element name of a name-addressed collection is no miss: {unresolved:?}"
        );
    }

    /// `ТекущаяОбласть` belongs to the spreadsheet document, not to the range it returns.
    /// The range is a platform type whose member surface the curated overlays attest as
    /// complete (`type_member_surface_attestations`), so the miss is a defect of the code
    /// and not a gap in the extract — which is what makes it reportable at all.
    ///
    /// The assertion describes a source of a known release. The documented corpus profile
    /// serves a corpus file, which carries no platform version, so there the contract keeps
    /// the check silent by design; the silence is asserted by
    /// [`a_versionless_corpus_keeps_the_platform_miss_silent`] instead.
    #[test]
    #[cfg_attr(
        corpus_contract,
        ignore = "the corpus profile serves a versionless corpus: the completeness gate is shut by contract"
    )]
    fn a_miss_on_an_attested_platform_type_is_reported() {
        let code = r#"Процедура Тест()
    ТабДок = Новый ТабличныйДокумент;
    Область = ТабДок.ТекущаяОбласть;
    Нет = Область.ТекущаяОбласть;
КонецПроцедуры"#;
        if crate::test_utils::built_in_facts_are_served() {
            let unresolved = unresolved_fields(code);
            assert_eq!(unresolved.len(), 1, "expected one miss on the range, got: {unresolved:?}");
            assert!(
                unresolved[0].message.contains("ТекущаяОбласть"),
                "message must name the missing field, got: {}",
                unresolved[0].message
            );
            assert!(
                unresolved[0].message.contains("ОбластьЯчеекТабличногоДокумента"),
                "message must name the receiver type, got: {}",
                unresolved[0].message
            );
        } else {
            // A corpus run: the served file carries no platform version, so the
            // completeness gate is shut by contract (see the class docs) and the very
            // same miss is silent. The control says the silence is that gate and not a
            // corpus that went missing — which is what a failure here would mean.
            assert!(
                !bsl_platform::PlatformData::instance().platform_type_member_surface_verified(
                    "ОбластьЯчеекТабличногоДокумента",
                    bsl_platform::MemberSurface::Fields,
                ),
                "a corpus without a version must leave the gate shut"
            );
            let unresolved = unresolved_fields(code);
            assert!(
                unresolved.is_empty(),
                "an unknown platform version keeps the miss silent: {unresolved:?}"
            );
        }
    }

    /// The other half of the contract, on the profile that exercises it: with a corpus
    /// whose platform version cannot be determined the completeness gate stays shut, so
    /// the very same miss is silent. The controls say why that silence is the contract
    /// and not a lost corpus: the served data knows the range, the member really is
    /// absent from it, and the gate really is the thing that is closed.
    #[test]
    #[cfg_attr(
        not(corpus_contract),
        ignore = "corpus contract: needs a corpus that carries no platform version"
    )]
    fn a_versionless_corpus_keeps_the_platform_miss_silent() {
        let data = bsl_platform::PlatformData::instance();
        assert!(
            data.get_type("ОбластьЯчеекТабличногоДокумента").is_some(),
            "the corpus profile must serve the pinned corpus"
        );
        assert!(
            data.get_property("SpreadsheetDocumentRange", "ТекущаяОбласть").is_none(),
            "the member the positive test misses must still be absent from the corpus"
        );
        assert!(
            !data.platform_type_member_surface_verified(
                "ОбластьЯчеекТабличногоДокумента",
                bsl_platform::MemberSurface::Fields
            ),
            "a corpus without a version must leave the gate shut"
        );

        let code = r#"Процедура Тест()
    ТабДок = Новый ТабличныйДокумент;
    Область = ТабДок.ТекущаяОбласть;
    Нет = Область.ТекущаяОбласть;
    Есть = Область.Значение;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(code).is_empty(),
            "an unknown platform version keeps the miss silent: {:?}",
            unresolved_fields(code)
        );
    }

    /// A system enumeration lists exactly the values the platform defines, so a value it
    /// does not have fails at run time ("Поле объекта не обнаружено") and is a defect of the
    /// checked code, not a gap in the corpus. The valid values, the English spelling and a
    /// different letter case must stay silent.
    #[test]
    fn a_miss_on_a_system_enumeration_value_is_reported() {
        if !crate::test_utils::built_in_facts_are_served() {
            return;
        }
        let code = r#"Процедура Тест()
    Р = РежимДиалогаВопрос.НетТакогоЗначения;
КонецПроцедуры"#;
        let unresolved = unresolved_fields(code);
        assert_eq!(unresolved.len(), 1, "expected one miss on the enum, got: {unresolved:?}");
        assert!(
            unresolved[0].message.contains("НетТакогоЗначения"),
            "message must name the missing value, got: {}",
            unresolved[0].message
        );

        let valid = r#"Процедура Тест()
    А = РежимДиалогаВопрос.ДаНет;
    Б = режимдиалогавопрос.даНет;
    В = QuestionDialogMode.YesNo;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(valid).is_empty(),
            "existing values must never be reported: {:?}",
            unresolved_fields(valid)
        );
    }

    /// `Символы` is a closed set of characters like a system enumeration: a name it does
    /// not have is a run-time failure.
    #[test]
    fn a_miss_on_the_characters_collection_is_reported() {
        if !crate::test_utils::built_in_facts_are_served() {
            return;
        }
        let code = r#"Процедура Тест()
    С = Символы.НетТакогоСимвола;
КонецПроцедуры"#;
        let unresolved = unresolved_fields(code);
        assert_eq!(unresolved.len(), 1, "expected one miss on the characters, got: {unresolved:?}");
        assert!(
            unresolved[0].message.contains("НетТакогоСимвола"),
            "message must name the missing member, got: {}",
            unresolved[0].message
        );

        let valid = r#"Процедура Тест()
    А = Символы.ПС;
    Б = Символы.ВК;
    В = символы.пс;
    Г = Chars.LF;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(valid).is_empty(),
            "existing members must never be reported: {:?}",
            unresolved_fields(valid)
        );
    }

    /// A global export shadows the platform's global names, so while a global module body
    /// is unread the bare name may denote that export instead of the enumeration, and a
    /// miss on it proves nothing. The readable control shows the same read is reported once
    /// the whole global surface is known.
    #[test]
    fn an_unread_global_body_leaves_a_system_enumeration_miss_unjudged() {
        use crate::test_utils::check_with_cfe_unreadable;
        if !crate::test_utils::built_in_facts_are_served() {
            return;
        }
        let code = "Процедура Тест() Экспорт\n    С = Символы.НетТакогоСимвола;\nКонецПроцедуры";
        let fixture = || {
            let mut builder = test_fixture::CfeFixtureBuilder::new("");
            builder.add_base_module_global("Глобальный", "Процедура П() Экспорт КонецПроцедуры");
            builder.build()
        };
        let misses = |unreadable: &[&str]| -> Vec<_> {
            check_with_cfe_unreadable(code, fixture(), unreadable)
                .into_iter()
                .filter(|d| d.code == DiagnosticCode::UnresolvedField)
                .collect()
        };

        let control = misses(&[]);
        assert_eq!(
            control.len(),
            1,
            "control: a known global surface judges the miss: {control:?}"
        );

        let unread = misses(&["CommonModules/Глобальный/Ext/Module.bsl"]);
        assert!(unread.is_empty(), "an unread global body may export the name: {unread:?}");
    }

    /// Only an attested enumeration is judged: the configuration's style items are values of
    /// `ЦветаСтиля`, `ШрифтыСтиля` and `РамкиСтиля` that no platform list can contain, so a
    /// read of one is never a miss.
    #[test]
    fn style_item_collections_stay_open() {
        let code = r#"Процедура Тест()
    А = ЦветаСтиля.ЛюбойЭлементСтиляКонфигурации;
    Б = ШрифтыСтиля.ЛюбойШрифтКонфигурации;
    В = РамкиСтиля.ЛюбаяРамкаКонфигурации;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(code).is_empty(),
            "style items belong to the configuration: {:?}",
            unresolved_fields(code)
        );
    }

    /// The gate: without an attestation the extract is not known to list every member,
    /// so the very same kind of miss proves nothing and stays silent. The receiver here
    /// is the spreadsheet document itself, whose surface is deliberately not attested —
    /// and reading a member that does exist is silent on the attested type, so the
    /// check does not fire on ordinary code either.
    #[test]
    fn a_miss_without_an_attestation_and_known_members_stay_silent() {
        let unattested = r#"Процедура Тест()
    ТабДок = Новый ТабличныйДокумент;
    Нет = ТабДок.ТекущаяОбластьНеТак;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(unattested).is_empty(),
            "a type nobody attested must stay soft: {:?}",
            unresolved_fields(unattested)
        );

        let known_members = r#"Процедура Тест()
    ТабДок = Новый ТабличныйДокумент;
    Область = ТабДок.ТекущаяОбласть;
    Есть = Область.Значение;
    Ещё = Область.Текст;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(known_members).is_empty(),
            "members the extract has must never be reported: {:?}",
            unresolved_fields(known_members)
        );

        let undefined_receiver = r#"Процедура Тест()
    Н = Неопределено;
    Нет = Н.НетТакогоПоля;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(undefined_receiver).is_empty(),
            "Неопределено is no platform type and must stay soft: {:?}",
            unresolved_fields(undefined_receiver)
        );
    }

    /// The walk types a local by its textually latest write, so at a read reached from a
    /// path where the local holds something else the attested type is one branch's view
    /// only. The member exists on that other path, and nothing proves it missing here.
    #[test]
    fn a_platform_miss_on_one_path_only_stays_silent() {
        if !crate::test_utils::built_in_facts_are_served() {
            return;
        }
        let merged = r#"Процедура Тест(Условие)
    ТабДок = Новый ТабличныйДокумент;
    Обл = Новый Структура("Ключ", 1);
    Если Условие Тогда
        Обл = ТабДок.ТекущаяОбласть;
    КонецЕсли;
    Х = Обл.Ключ;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(merged).is_empty(),
            "the structure path owns the key: {:?}",
            unresolved_fields(merged)
        );

        let sibling = r#"Процедура Тест(Условие)
    ТабДок = Новый ТабличныйДокумент;
    Пишущий = Новый ЗаписьJSON;
    Если Условие Тогда
        Пишущий = ТабДок.ТекущаяОбласть;
    Иначе
        Х = Пишущий.ПроверятьСтруктуру;
    КонецЕсли;
КонецПроцедуры"#;
        assert!(
            unresolved_fields(sibling).is_empty(),
            "the sibling branch holds the writer: {:?}",
            unresolved_fields(sibling)
        );

        let every_path = r#"Процедура Тест(Условие)
    ТабДок = Новый ТабличныйДокумент;
    Другой = Новый ТабличныйДокумент;
    Если Условие Тогда
        Обл = ТабДок.ТекущаяОбласть;
    Иначе
        Обл = Другой.ТекущаяОбласть;
    КонецЕсли;
    Х = Обл.ТекущаяОбласть;
КонецПроцедуры"#;
        let unresolved = unresolved_fields(every_path);
        assert_eq!(
            unresolved.len(),
            1,
            "a range on every path lacks the member on every path: {unresolved:?}"
        );
    }

    const ADDITIONAL_COLUMNS_CONFIGURATION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>TestConfiguration</Name>
        </Properties>
        <ChildObjects>
            <Catalog>Товары</Catalog>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#;

    const ADDITIONAL_COLUMNS_CATALOG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <Catalog uuid="7e1c0000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Товары</Name>
        </Properties>
        <ChildObjects>
            <TabularSection uuid="7e1c0000-0000-0000-0000-000000000002">
                <Properties>
                    <Name>Состав</Name>
                </Properties>
                <ChildObjects>
                    <Attribute uuid="7e1c0000-0000-0000-0000-000000000003">
                        <Properties>
                            <Name>Количество</Name>
                            <Type><v8:Type>xs:decimal</v8:Type></Type>
                        </Properties>
                    </Attribute>
                </ChildObjects>
            </TabularSection>
            <TabularSection uuid="7e1c0000-0000-0000-0000-000000000004">
                <Properties>
                    <Name>Цены</Name>
                </Properties>
                <ChildObjects>
                    <Attribute uuid="7e1c0000-0000-0000-0000-000000000005">
                        <Properties>
                            <Name>Цена</Name>
                            <Type><v8:Type>xs:decimal</v8:Type></Type>
                        </Properties>
                    </Attribute>
                </ChildObjects>
            </TabularSection>
            <Form>ФормаЭлемента</Form>
            <Form>ФормаВыбора</Form>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

    /// The item form adds `ЭтоНабор` to the rows of `Объект.Состав` and binds a table
    /// to that tabular section.
    const ITEM_FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <ChildItems>
        <Table name="Состав" id="1">
            <DataPath>Объект.Состав</DataPath>
        </Table>
    </ChildItems>
    <Attributes>
        <Attribute name="Объект" id="1">
            <Type>
                <v8:Type>cfg:CatalogObject.Товары</v8:Type>
            </Type>
            <MainAttribute>true</MainAttribute>
            <SavedData>true</SavedData>
            <Columns>
                <AdditionalColumns table="Объект.Состав">
                    <Column name="ЭтоНабор" id="1">
                        <Type>
                            <v8:Type>xs:boolean</v8:Type>
                        </Type>
                    </Column>
                </AdditionalColumns>
            </Columns>
        </Attribute>
    </Attributes>
</Form>"#;

    /// Another form of the same catalog: the same main attribute, no additional columns.
    const CHOICE_FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:v8="http://v8.1c.ru/8.1/data/core" version="2.10">
    <Attributes>
        <Attribute name="Объект" id="1">
            <Type>
                <v8:Type>cfg:CatalogObject.Товары</v8:Type>
            </Type>
            <MainAttribute>true</MainAttribute>
        </Attribute>
    </Attributes>
</Form>"#;

    const ITEM_FORM_MODULE: &str = "Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
    const CHOICE_FORM_MODULE: &str = "Catalogs/Товары/Forms/ФормаВыбора/Ext/Form/Module.bsl";
    const OBJECT_MODULE: &str = "Catalogs/Товары/Ext/ObjectModule.bsl";

    const EMPTY_FORM_MODULE: &str = "&НаКлиенте\nПроцедура Пусто()\nКонецПроцедуры\n";
    const EMPTY_OBJECT_MODULE: &str = "Процедура Пусто()\nКонецПроцедуры\n";

    /// The messages of `UnresolvedField` in `checked`, with the item form, the choice
    /// form and the object module in one configuration.
    fn unresolved_with_additional_columns(
        item_form: &str,
        choice_form: &str,
        object_module: &str,
        checked: &str,
    ) -> Vec<String> {
        let files = [
            ("Configuration.xml", ADDITIONAL_COLUMNS_CONFIGURATION),
            ("Catalogs/Товары.xml", ADDITIONAL_COLUMNS_CATALOG),
            ("Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form.xml", ITEM_FORM_XML),
            ("Catalogs/Товары/Forms/ФормаВыбора/Ext/Form.xml", CHOICE_FORM_XML),
            (ITEM_FORM_MODULE, item_form),
            (CHOICE_FORM_MODULE, choice_form),
            (OBJECT_MODULE, object_module),
        ];
        crate::test_utils::check_config_files(&files, checked)
            .into_iter()
            .filter(|diag| diag.code == DiagnosticCode::UnresolvedField)
            .map(|diag| diag.message)
            .collect()
    }

    #[test]
    fn a_form_additional_column_resolves_on_the_rows_of_its_tabular_section() {
        // The rows come through the main attribute and through the bound table; the
        // column name is matched regardless of case, as the platform does.
        let item_form = "&НаКлиенте
Процедура Тест()
    Строка = Объект.Состав.Добавить();
    Строка.ЭтоНабор = Истина;
    Текущие = Элементы.Состав.ТекущиеДанные;
    Флаг = Текущие.этонабор;
КонецПроцедуры
";
        let unresolved = unresolved_with_additional_columns(
            item_form,
            EMPTY_FORM_MODULE,
            EMPTY_OBJECT_MODULE,
            ITEM_FORM_MODULE,
        );
        assert!(unresolved.is_empty(), "additional column must resolve: {unresolved:?}");
    }

    #[test]
    fn a_misspelled_or_misplaced_additional_column_is_still_reported() {
        // The misspelling is not a column; the right name on the rows of another tabular
        // section of the same object is not one either.
        let item_form = "&НаКлиенте
Процедура Тест()
    Строка = Объект.Состав.Добавить();
    Строка.ЭтоНобор = Истина;
    Цена = Объект.Цены.Добавить();
    Цена.ЭтоНабор = Истина;
КонецПроцедуры
";
        let unresolved = unresolved_with_additional_columns(
            item_form,
            EMPTY_FORM_MODULE,
            EMPTY_OBJECT_MODULE,
            ITEM_FORM_MODULE,
        );
        assert_eq!(unresolved.len(), 2, "{unresolved:?}");
        assert!(unresolved[0].contains("'ЭтоНобор'"), "{unresolved:?}");
        assert!(unresolved[1].contains("'ЭтоНабор'"), "{unresolved:?}");
        assert!(unresolved[1].contains("Цены"), "{unresolved:?}");
    }

    #[test]
    fn an_additional_column_does_not_leak_into_other_forms_or_the_object_module() {
        let choice_form = "&НаКлиенте
Процедура Тест()
    Строка = Объект.Состав.Добавить();
    Строка.ЭтоНабор = Истина;
КонецПроцедуры
";
        let object_module = "Процедура Тест()
    Строка = Состав.Добавить();
    Строка.ЭтоНабор = Истина;
КонецПроцедуры
";
        let in_choice_form = unresolved_with_additional_columns(
            EMPTY_FORM_MODULE,
            choice_form,
            EMPTY_OBJECT_MODULE,
            CHOICE_FORM_MODULE,
        );
        assert_eq!(in_choice_form.len(), 1, "another form: {in_choice_form:?}");
        assert!(in_choice_form[0].contains("'ЭтоНабор'"), "{in_choice_form:?}");

        let in_object_module = unresolved_with_additional_columns(
            EMPTY_FORM_MODULE,
            EMPTY_FORM_MODULE,
            object_module,
            OBJECT_MODULE,
        );
        assert_eq!(in_object_module.len(), 1, "object module: {in_object_module:?}");
        assert!(in_object_module[0].contains("'ЭтоНабор'"), "{in_object_module:?}");
    }
}
