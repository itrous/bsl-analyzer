use crate::test_utils::{
    check_config_files_with_unreadable, check_hir_diagnostic_with_unreadable, format_diags,
};
use crate::{Diagnostic, DiagnosticCode};
use expect_test::{expect, Expect};

const MODULE_PATH: &str = "CommonModules/Проба/Ext/Module.bsl";
const MANAGER_MODULE_PATH: &str = "Catalogs/Товары/Ext/ManagerModule.bsl";

const CONFIGURATION_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Configuration uuid="00000000-0000-0000-0000-000000000001">
        <Properties><Name>Тест</Name></Properties>
        <ChildObjects>
            <Catalog>Товары</Catalog>
            <Document>Заказ</Document>
            <Enum>Статусы</Enum>
            <CommonModule>Проба</CommonModule>
            <CommonModule>МойМодуль</CommonModule>
        </ChildObjects>
    </Configuration>
</MetaDataObject>"#;

const CATALOG_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
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
            <Attribute uuid="00000000-0000-0000-0000-00000000000a">
                <Properties>
                    <Name>ЛюбаяСсылкаТовара</Name>
                    <Type>
                        <v8:TypeSet>cfg:CatalogRef</v8:TypeSet>
                    </Type>
                </Properties>
            </Attribute>
        </ChildObjects>
    </Catalog>
</MetaDataObject>"#;

const DOCUMENT_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Document uuid="00000000-0000-0000-0000-000000000004">
        <Properties>
            <Name>Заказ</Name>
            <NumberLength>9</NumberLength>
        </Properties>
        <ChildObjects/>
    </Document>
</MetaDataObject>"#;

const ENUM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <Enum uuid="00000000-0000-0000-0000-000000000005">
        <Properties><Name>Статусы</Name></Properties>
        <ChildObjects>
            <EnumValue uuid="00000000-0000-0000-0000-000000000006">
                <Properties><Name>Активный</Name></Properties>
            </EnumValue>
        </ChildObjects>
    </Enum>
</MetaDataObject>"#;

const PREDEFINED_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<PredefinedData xmlns="http://v8.1c.ru/8.3/xcf/predef"
                xsi:type="CatalogPredefinedItems"
                version="2.20"
                xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
    <Item id="00000000-0000-0000-0000-000000000007">
        <Name>Основной</Name>
        <Code/>
        <Description/>
        <IsFolder>false</IsFolder>
    </Item>
</PredefinedData>"#;

const PROBE_MODULE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <CommonModule uuid="00000000-0000-0000-0000-000000000008">
        <Properties>
            <Name>Проба</Name>
            <Global>true</Global>
            <Server>true</Server>
            <ServerCall>true</ServerCall>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

const OTHER_MODULE_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.20">
    <CommonModule uuid="00000000-0000-0000-0000-000000000009">
        <Properties>
            <Name>МойМодуль</Name>
            <Global>true</Global>
            <Server>true</Server>
            <ServerCall>true</ServerCall>
        </Properties>
    </CommonModule>
</MetaDataObject>"#;

const OTHER_MODULE_BSL: &str = r#"Функция НайтиПоКоду(Код) Экспорт
    Возврат Неопределено;
КонецФункции
"#;

fn diagnostics(source: &str) -> Vec<Diagnostic> {
    diagnostics_with_bodies(source, &[], &[])
}

/// `extra_bodies` are further `.bsl` files of the configuration; `unreadable` names those among
/// them (or any other body) that exist but could not be read.
fn diagnostics_with_bodies(
    source: &str,
    extra_bodies: &[(&str, &str)],
    unreadable: &[&str],
) -> Vec<Diagnostic> {
    let mut files = vec![
        ("Configuration.xml", CONFIGURATION_XML),
        ("Catalogs/Товары.xml", CATALOG_XML),
        ("Catalogs/Товары/Ext/Predefined.xml", PREDEFINED_XML),
        ("Documents/Заказ.xml", DOCUMENT_XML),
        ("Enums/Статусы.xml", ENUM_XML),
        ("CommonModules/Проба.xml", PROBE_MODULE_XML),
        ("CommonModules/МойМодуль.xml", OTHER_MODULE_XML),
        (MODULE_PATH, source),
        ("CommonModules/МойМодуль/Ext/Module.bsl", OTHER_MODULE_BSL),
    ];
    files.extend_from_slice(extra_bodies);
    check_config_files_with_unreadable(&files, MODULE_PATH, unreadable)
}

fn diagnostics_for(source: &str, code: DiagnosticCode) -> Vec<Diagnostic> {
    diagnostics(source).into_iter().filter(|diag| diag.code == code).collect()
}

fn candidates_with_manager_module(
    source: &str,
    manager_module: &str,
    unreadable: &[&str],
) -> Vec<Diagnostic> {
    diagnostics_with_bodies(source, &[(MANAGER_MODULE_PATH, manager_module)], unreadable)
        .into_iter()
        .filter(|diag| diag.code == DiagnosticCode::ImplicitDatabaseReadInCycle)
        .collect()
}

fn assert_snapshot(source: &str, code: DiagnosticCode, expected: Expect) {
    let selected = diagnostics_for(source, code);
    expected.assert_eq(&format_diags(source, &selected));
}

fn assert_count(source: &str, code: DiagnosticCode, expected: usize) -> Vec<Diagnostic> {
    let selected = diagnostics_for(source, code);
    assert_eq!(selected.len(), expected, "unexpected {code:?} diagnostics: {selected:#?}");
    selected
}

fn range_text<'a>(source: &'a str, diag: &Diagnostic) -> &'a str {
    let start: usize = diag.range.start().into();
    let end: usize = diag.range.end().into();
    &source[start..end]
}

fn assert_static_candidate_messages(diags: &[Diagnostic]) {
    for diag in diags {
        assert!(
            diag.message.starts_with("Возможное чтение из базы в цикле"),
            "candidate message should be qualified as possible: {}",
            diag.message
        );
        assert!(
            !diag.message.contains("один запрос"),
            "candidate message must not promise exactly one query: {}",
            diag.message
        );
    }
}

#[test]
fn implicit_database_read_in_cycle_reference_attributes_and_query_projection() {
    let source = r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Ссылка КАК Номенклатура ИЗ Справочник.Товары КАК Т";
    Выборка = Запрос.Выполнить().Выбрать();
    Пока Выборка.Следующий() Цикл
        Наименование = Выборка.Номенклатура.Наименование;
        Артикул = Выборка.Номенклатура.Код;
    КонецЦикла;
КонецПроцедуры
"#;
    assert_snapshot(
        source,
        DiagnosticCode::ImplicitDatabaseReadInCycle,
        expect![[r#"
            ImplicitDatabaseReadInCycle @ 6:24..6:57
              message: Возможное чтение из базы в цикле: обращение к реквизиту «Наименование» через точку от ссылки загружает объект. Получите реквизит в запросе до цикла
              severity: Warning
            ImplicitDatabaseReadInCycle @ 7:19..7:43
              message: Возможное чтение из базы в цикле: обращение к реквизиту «Код» через точку от ссылки загружает объект. Получите реквизит в запросе до цикла
              severity: Warning"#]],
    );
}

#[test]
fn implicit_database_read_in_cycle_query_projection_is_not_a_reference_read() {
    let source = r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Наименование КАК Наименование, Т.Код КАК Артикул ИЗ Справочник.Товары КАК Т";
    Выборка = Запрос.Выполнить().Выбрать();
    Пока Выборка.Следующий() Цикл
        Наименование = Выборка.Наименование;
        Артикул = Выборка.Артикул;
    КонецЦикла;
КонецПроцедуры
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_predefined_reference_exemption_keeps_type_control() {
    let source = r#"Процедура Тест()
    Для Индекс = 1 По 1 Цикл
        Артикул = Справочники.Товары.Основной.Артикул;
    КонецЦикла;
    Значение = Справочники.Товары.Основной.НетТакого;
КонецПроцедуры
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
    let unresolved = assert_count(source, DiagnosticCode::UnresolvedField, 1);
    assert!(unresolved[0].message.contains("СправочникСсылка.Товары"), "{unresolved:#?}");
    assert!(unresolved[0].message.contains("НетТакого"), "{unresolved:#?}");
}

#[test]
fn implicit_database_read_in_cycle_string_of_reference_in_loops() {
    let source = r#"Процедура Тест()
    Для Каждого Ссылка Из Ссылки() Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
    Для Каждого Ссылка Из Ссылки() Цикл
        Представление = строка(Ссылка);
    КонецЦикла;
    Ссылка = Справочники.Товары.НайтиПоКоду("1");
    Для Индекс = 1 По 3 Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    assert_snapshot(
        source,
        DiagnosticCode::ImplicitDatabaseReadInCycle,
        expect![[r#"
            ImplicitDatabaseReadInCycle @ 3:25..3:39
              message: Возможное чтение из базы в цикле: Строка() от ссылки получает её представление. Получите представление в запросе (ПРЕДСТАВЛЕНИЕ) до цикла
              severity: Warning
            ImplicitDatabaseReadInCycle @ 6:25..6:39
              message: Возможное чтение из базы в цикле: Строка() от ссылки получает её представление. Получите представление в запросе (ПРЕДСТАВЛЕНИЕ) до цикла
              severity: Warning
            ImplicitDatabaseReadInCycle @ 10:25..10:39
              message: Возможное чтение из базы в цикле: Строка() от ссылки получает её представление. Получите представление в запросе (ПРЕДСТАВЛЕНИЕ) до цикла
              severity: Warning"#]],
    );
}

#[test]
fn implicit_database_read_in_cycle_rejects_non_reference_types() {
    let source = r#"Процедура Тест()
    Для Каждого Элемент Из Числа() Цикл
        Представление = Строка(Элемент);
    КонецЦикла;
    Для Каждого Элемент Из Тексты() Цикл
        Представление = Строка(Элемент);
    КонецЦикла;
    Для Каждого Элемент Из Статусы() Цикл
        Представление = Строка(Элемент);
    КонецЦикла;
    Статус = Перечисления.Статусы.Активный;
    Значение = Статус.НетТакого;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из Число
Функция Числа() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из Строка
Функция Тексты() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из ПеречислениеСсылка.Статусы
Функция Статусы() Экспорт
    Возврат Новый Массив;
КонецФункции

"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
    let unresolved = assert_count(source, DiagnosticCode::UnresolvedField, 1);
    assert!(unresolved[0].message.contains("ПеречислениеСсылка.Статусы"), "{unresolved:#?}");
}

#[test]
fn implicit_database_read_in_cycle_accepts_any_metadata_reference_from_query_projection() {
    let source = r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.ЛюбаяСсылкаТовара КАК Ссылка ИЗ Справочник.Товары КАК Т";
    Выборка = Запрос.Выполнить().Выбрать();
    Пока Выборка.Следующий() Цикл
        Представление = Строка(Выборка.Ссылка);
    КонецЦикла;
КонецПроцедуры
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Строка(Выборка.Ссылка)");
}

#[test]
fn implicit_database_read_in_cycle_mixed_reference_and_undefined_union_is_not_candidate() {
    let source = r#"Процедура Тест()
    Для Индекс = 1 По 1 Цикл
        Представление = Строка(СсылкаИлиПусто());
        Представление = Строка(СсылкаТовара());
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   СправочникСсылка.Товары, Неопределено
Функция СсылкаИлиПусто() Экспорт
    Возврат Неопределено;
КонецФункции

// Возвращаемое значение:
//   СправочникСсылка.Товары
Функция СсылкаТовара() Экспорт
    Возврат Неопределено;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Строка(СсылкаТовара())");
}

#[test]
fn implicit_database_read_in_cycle_reference_array_is_a_positive_control() {
    let source = r#"Процедура Тест()
    Для Каждого Ссылка Из Ссылки() Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Строка(Ссылка)");
}

#[test]
fn implicit_database_read_in_cycle_accepts_unions_of_object_references_from_doc_returns() {
    let source = r#"Процедура Тест()
    Для Каждого Ссылка Из СсылкиРазныхВидов() Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   - Массив из СправочникСсылка.Товары
//   - Массив из ДокументСсылка.Заказ
Функция СсылкиРазныхВидов() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Строка(Ссылка)");
}

#[test]
fn implicit_database_read_in_cycle_union_array_with_non_reference_arm_is_not_candidate() {
    let source = r#"Процедура Тест()
    Для Каждого Элемент Из СсылкиИлиТекст() Цикл
        Представление = Строка(Элемент);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   - Массив из СправочникСсылка.Товары
//   - Массив из Строка
Функция СсылкиИлиТекст() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_any_and_unknown_are_not_candidates() {
    let source = r#"Процедура Тест(Значения)
    Массив = Новый Массив;
    Массив.Добавить(Справочники.Товары.НайтиПоКоду("1"));
    Для Каждого Элемент Из Массив Цикл
        Представление = Строка(Элемент);
    КонецЦикла;
    Для Каждого Ссылка Из Значения Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
КонецПроцедуры
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_shadowed_string_is_not_a_candidate() {
    let source = r#"Процедура Тест()
    Для Каждого Ссылка Из Ссылки() Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции

Функция Строка(Значение)
    Возврат "пользовательская функция";
КонецФункции
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_selected_presentation_and_outside_string_are_not_candidates() {
    let source = r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ ПРЕДСТАВЛЕНИЕ(Т.Ссылка) КАК ТоварПредставление ИЗ Справочник.Товары КАК Т";
    Выборка = Запрос.Выполнить().Выбрать();
    Пока Выборка.Следующий() Цикл
        Представление = Выборка.ТоварПредставление;
    КонецЦикла;
    Ссылка = Справочники.Товары.НайтиПоКоду("1");
    Представление = Строка(Ссылка);
КонецПроцедуры
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_manager_search_methods() {
    let source = r#"Процедура Тест(Коды, Код, Имя, Значение, Номер)
    Для Каждого КодЦикла Из Коды Цикл
        Товар = Справочники.Товары.НайтиПоКоду(Код);
        Товар = Справочники.Товары.НайтиПоНаименованию(Имя);
        Товар = Справочники.Товары.НайтиПоРеквизиту("Артикул", Значение);
        Документ = Документы.Заказ.НайтиПоНомеру(Номер);
    КонецЦикла;
КонецПроцедуры
"#;
    assert_snapshot(
        source,
        DiagnosticCode::ImplicitDatabaseReadInCycle,
        expect![[r#"
            ImplicitDatabaseReadInCycle @ 3:17..3:52
              message: Возможное чтение из базы в цикле: НайтиПоКоду() менеджера выполняет поиск в базе. Найдите значения одним запросом до цикла
              severity: Warning
            ImplicitDatabaseReadInCycle @ 4:17..4:60
              message: Возможное чтение из базы в цикле: НайтиПоНаименованию() менеджера выполняет поиск в базе. Найдите значения одним запросом до цикла
              severity: Warning
            ImplicitDatabaseReadInCycle @ 5:17..5:73
              message: Возможное чтение из базы в цикле: НайтиПоРеквизиту() менеджера выполняет поиск в базе. Найдите значения одним запросом до цикла
              severity: Warning
            ImplicitDatabaseReadInCycle @ 6:20..6:56
              message: Возможное чтение из базы в цикле: НайтиПоНомеру() менеджера выполняет поиск в базе. Найдите значения одним запросом до цикла
              severity: Warning"#]],
    );
}

#[test]
fn implicit_database_read_in_cycle_manager_search_outside_loop_is_not_a_candidate() {
    let source = r#"Процедура Тест(Код)
    Товар = Справочники.Товары.НайтиПоКоду(Код);
КонецПроцедуры
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_shadowed_manager_method_is_not_a_candidate() {
    let source = r#"Процедура Тест(Коды, Код)
    Для Каждого КодЦикла Из Коды Цикл
        Товар = МойМодуль.НайтиПоКоду(Код);
        Товар = НайтиПоКоду(Код);
    КонецЦикла;
КонецПроцедуры

Функция НайтиПоКоду(Код)
    Возврат Неопределено;
КонецФункции
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_manager_search_query_replacement_has_no_query_diagnostic() {
    let source = r#"Процедура Тест(Коды)
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Ссылка КАК Товар ИЗ Справочник.Товары КАК Т ГДЕ Т.Код В (&Коды)";
    Запрос.УстановитьПараметр("Коды", Коды);
    Выборка = Запрос.Выполнить().Выбрать();
    Пока Выборка.Следующий() Цикл
        ОбработатьТовар(Выборка.Товар);
    КонецЦикла;
КонецПроцедуры
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
    assert_count(source, DiagnosticCode::CreateQueryInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_coexists_with_explicit_query_diagnostic_without_overlap() {
    let source = r#"Процедура Тест(Код)
    Запрос = Новый Запрос;
    Для Каждого Ссылка Из Ссылки() Цикл
        Выборка = Запрос.Выполнить();
        Артикул = Ссылка.Артикул;
        Представление = Строка(Ссылка);
        Товар = Справочники.Товары.НайтиПоКоду(Код);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let implicit = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 3);
    let explicit = assert_count(source, DiagnosticCode::CreateQueryInCycle, 1);
    let expressions = ["Ссылка.Артикул", "Строка(Ссылка)", "Справочники.Товары.НайтиПоКоду(Код)"];
    for (diag, expression) in implicit.iter().zip(expressions) {
        assert_eq!(range_text(source, diag), expression);
    }
    let explicit_start: usize = explicit[0].range.start().into();
    let explicit_end: usize = explicit[0].range.end().into();
    for diag in &implicit {
        let start: usize = diag.range.start().into();
        let end: usize = diag.range.end().into();
        assert!(end <= explicit_start || explicit_end <= start, "overlapping ranges: {diag:#?}");
    }
}

#[test]
fn implicit_database_read_in_cycle_nested_families_have_separate_diagnostics() {
    let source = r#"Процедура Тест(Код)
    Для Каждого Элемент Из Ссылки() Цикл
        Представление = Строка(Справочники.Товары.НайтиПоКоду(Код));
        Артикул = Справочники.Товары.НайтиПоКоду(Код).Артикул;
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = diagnostics_for(source, DiagnosticCode::ImplicitDatabaseReadInCycle);
    assert_eq!(selected.len(), 4, "{selected:#?}");
    assert_eq!(
        selected.iter().map(|diag| diag.message.as_str()).collect::<Vec<_>>(),
        vec![
            "Возможное чтение из базы в цикле: Строка() от ссылки получает её представление. Получите представление в запросе (ПРЕДСТАВЛЕНИЕ) до цикла",
            "Возможное чтение из базы в цикле: НайтиПоКоду() менеджера выполняет поиск в базе. Найдите значения одним запросом до цикла",
            "Возможное чтение из базы в цикле: НайтиПоКоду() менеджера выполняет поиск в базе. Найдите значения одним запросом до цикла",
            "Возможное чтение из базы в цикле: обращение к реквизиту «Артикул» через точку от ссылки загружает объект. Получите реквизит в запросе до цикла",
        ]
    );
    assert_eq!(range_text(source, &selected[0]), "Строка(Справочники.Товары.НайтиПоКоду(Код))");
    assert_eq!(range_text(source, &selected[1]), "Справочники.Товары.НайтиПоКоду(Код)");
    assert_eq!(range_text(source, &selected[2]), "Справочники.Товары.НайтиПоКоду(Код)");
    assert_eq!(range_text(source, &selected[3]), "Справочники.Товары.НайтиПоКоду(Код).Артикул");
}

#[test]
fn implicit_database_read_in_cycle_nested_loops_emit_once_per_expression() {
    let source = r#"Процедура Тест()
    Для Каждого Ссылка Из Ссылки() Цикл
        Пока Истина Цикл
            Для Индекс = 1 По 1 Цикл
                Артикул = Ссылка.Артикул;
            КонецЦикла;
        КонецЦикла;
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Ссылка.Артикул");
}

#[test]
fn implicit_database_read_in_cycle_loop_headers_are_not_loop_bodies() {
    let source = r#"Процедура Тест()
    Ссылка = Справочники.Товары.НайтиПоКоду("1");
    Пока Ссылка.Артикул <> "" Цикл
        Прервать;
    КонецЦикла;
    Для Индекс = 1 По СтрДлина(Строка(Ссылка)) Цикл
    КонецЦикла;
    Для Каждого Символ Из СтрРазделить(Строка(Справочники.Товары.НайтиПоКоду("1")), ",") Цикл
    КонецЦикла;
    Для Каждого Элемент Из Ссылки() Цикл
        Артикул = Ссылка.Артикул;
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Ссылка.Артикул");
}

#[test]
fn implicit_database_read_in_cycle_helper_bodies_are_not_expanded_at_call_sites() {
    let source = r#"Процедура Тест(Код)
    Для Каждого Значение Из Коды() Цикл
        Обработать(Код);
        А();
    КонецЦикла;
КонецПроцедуры

Процедура Обработать(Код)
    Ссылка = Справочники.Товары.НайтиПоКоду(Код);
    Представление = Строка(Ссылка);
    Артикул = Ссылка.Артикул;
КонецПроцедуры

Процедура А()
    Б();
КонецПроцедуры

Процедура Б()
    А();
КонецПроцедуры

// Возвращаемое значение:
//   Массив из Строка
Функция Коды() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
}

#[test]
fn implicit_database_read_in_cycle_unreadable_helper_body_is_not_expanded() {
    let fixture = r#"
//- /CommonModules/Сервер/Ext/Module.bsl
// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции

Процедура Обработать() Экспорт
    Для Каждого Ссылка Из Ссылки() Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Для Каждого Код Из Коды Цикл
        Сервер.Обработать();
    КонецЦикла;
КонецПроцедуры
"#;
    let readable = check_hir_diagnostic_with_unreadable(fixture, &[]);
    assert!(
        readable.iter().all(|diag| diag.code != DiagnosticCode::ImplicitDatabaseReadInCycle),
        "the helper call itself is not an implicit database read: {readable:#?}"
    );
    let unreadable =
        check_hir_diagnostic_with_unreadable(fixture, &["/CommonModules/Сервер/Ext/Module.bsl"]);
    assert!(
        unreadable.iter().all(|diag| diag.code != DiagnosticCode::ImplicitDatabaseReadInCycle),
        "an unreadable helper body is not analyzed at its call site: {unreadable:#?}"
    );
}

#[test]
fn implicit_database_read_in_cycle_helper_with_its_own_loop_is_analyzed() {
    let source = r#"Процедура Тест()
    ОбработатьВЦикле();
КонецПроцедуры

Процедура ОбработатьВЦикле()
    Для Каждого Ссылка Из Ссылки() Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Строка(Ссылка)");
}

#[test]
fn implicit_database_read_in_cycle_english_keywords_and_manager_methods() {
    let source = r#"Procedure Test(Codes, Code, Name, Value, Number)
    For Each Ref In Refs() Do
        View = String(Ref);
    EndDo;
    For Each CurrentCode In Codes Do
        Item = Catalogs.Товары.FindByCode(Code);
        Item = Catalogs.Товары.FindByDescription(Name);
        Item = Catalogs.Товары.FindByAttribute("Артикул", Value);
        Document = Documents.Заказ.FindByNumber(Number);
    EndDo;
EndProcedure

// Returns:
//   Array of CatalogRef.Товары
Function Refs() Export
    Return New Array;
EndFunction
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 5);
    assert_eq!(selected.iter().filter(|diag| diag.message.contains("Строка()")).count(), 1);
    assert_eq!(selected.iter().filter(|diag| diag.message.contains("менеджера")).count(), 4);
}

#[test]
fn implicit_database_read_in_cycle_messages_and_documentation_describe_static_candidates() {
    let reference_source = r#"Процедура Тест()
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ Т.Ссылка КАК Номенклатура ИЗ Справочник.Товары КАК Т";
    Выборка = Запрос.Выполнить().Выбрать();
    Пока Выборка.Следующий() Цикл
        Наименование = Выборка.Номенклатура.Наименование;
    КонецЦикла;
КонецПроцедуры
"#;
    let string_source = r#"Процедура Тест()
    Для Каждого Ссылка Из Ссылки() Цикл
        Представление = Строка(Ссылка);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let manager_source = r#"Процедура Тест(Коды, Код)
    Для Каждого КодЦикла Из Коды Цикл
        Товар = Справочники.Товары.НайтиПоКоду(Код);
    КонецЦикла;
КонецПроцедуры
"#;
    let reference = assert_count(reference_source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    let string = assert_count(string_source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    let manager = assert_count(manager_source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_static_candidate_messages(&reference);
    assert_static_candidate_messages(&string);
    assert_static_candidate_messages(&manager);

    let ru = include_str!("../../docs/ru/ImplicitDatabaseReadInCycle.md");
    let en = include_str!("../../docs/en/ImplicitDatabaseReadInCycle.md");
    assert!(ru.contains("Это статический кандидат: диагностика не утверждает, что платформа выполнит ровно один запрос к базе на каждой итерации — кэширование и реальные планы запросов требуют измерений."));
    assert!(en.contains("This is a static candidate: the diagnostic does not claim that the platform executes exactly one database query on every iteration; caching and actual query plans require measurement."));
}

#[test]
fn implicit_database_read_in_cycle_diagnostics_after_loop_are_not_candidates() {
    let source = r#"Процедура Тест(Код)
    Для Каждого Ссылка Из Ссылки() Цикл
        Артикул = Ссылка.Артикул;
        Представление = Строка(Ссылка);
        Товар = Справочники.Товары.НайтиПоКоду(Код);
    КонецЦикла;
    Артикул = Ссылка.Артикул;
    Представление = Строка(Ссылка);
    Товар = Справочники.Товары.НайтиПоКоду(Код);
    Пока Истина Цикл
        Для Каждого Элемент Из Ссылки() Цикл
            Артикул = Элемент.Артикул;
            Представление = Строка(Элемент);
            Товар = Справочники.Товары.НайтиПоКоду(Код);
        КонецЦикла;
    КонецЦикла;
    Артикул = Элемент.Артикул;
    Представление = Строка(Элемент);
    Товар = Справочники.Товары.НайтиПоКоду(Код);
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 6);
    assert_eq!(
        selected.iter().filter(|diag| range_text(source, diag) == "Ссылка.Артикул").count(),
        1
    );
    assert_eq!(
        selected.iter().filter(|diag| range_text(source, diag) == "Строка(Ссылка)").count(),
        1
    );
    assert_eq!(
        selected
            .iter()
            .filter(|diag| range_text(source, diag) == "Справочники.Товары.НайтиПоКоду(Код)")
            .count(),
        2,
        "the manager call occurs once in each loop, but not after either loop"
    );
    for expression in ["Элемент.Артикул", "Строка(Элемент)"] {
        assert_eq!(
            selected.iter().filter(|diag| range_text(source, diag) == expression).count(),
            1
        );
    }
}

#[test]
fn implicit_database_read_in_cycle_rejected_manager_binding_has_no_candidate() {
    let source = r#"Процедура Тест(Код)
    Для Каждого Элемент Из Коды() Цикл
        Товар = Справочники.Товары.НайтиПоКоду(Код, 1, 2, 3, 4);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из Строка
Функция Коды() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
    assert_count(source, DiagnosticCode::MismatchedArgCount, 1);
}

#[test]
fn implicit_database_read_in_cycle_keeps_using_find_element_by_string_behavior() {
    let source = r#"Процедура Тест(Коды)
    Для Каждого Код Из Коды Цикл
        Товар = Справочники.Товары.НайтиПоКоду("840");
    КонецЦикла;
    Товар = Справочники.Товары.НайтиПоКоду("840");
КонецПроцедуры
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_count(source, DiagnosticCode::UsingFindElementByString, 2);
}

const MANAGER_FIND_AFTER_REFERENCE_SOURCE: &str = r#"Процедура Тест(Коды, Код)
    Ссылка = Справочники.Товары.НайтиПоКоду(Код);
    Для Каждого КодЦикла Из Коды Цикл
        Представление = Строка(Ссылка);
        Товар = Справочники.Товары.НайтиПоКоду(Код);
    КонецЦикла;
КонецПроцедуры
"#;

const UNRELATED_MANAGER_MODULE: &str = r#"Процедура Прочее() Экспорт
КонецПроцедуры
"#;

#[test]
fn implicit_database_read_in_cycle_readable_manager_module_without_the_method_keeps_candidates() {
    let selected = candidates_with_manager_module(
        MANAGER_FIND_AFTER_REFERENCE_SOURCE,
        UNRELATED_MANAGER_MODULE,
        &[],
    );
    let expressions: Vec<_> =
        selected.iter().map(|diag| range_text(MANAGER_FIND_AFTER_REFERENCE_SOURCE, diag)).collect();
    assert_eq!(
        expressions,
        ["Строка(Ссылка)", "Справочники.Товары.НайтиПоКоду(Код)"],
        "{selected:#?}"
    );
}

#[test]
fn implicit_database_read_in_cycle_unreadable_manager_module_withholds_only_the_manager_find() {
    let selected = candidates_with_manager_module(
        MANAGER_FIND_AFTER_REFERENCE_SOURCE,
        UNRELATED_MANAGER_MODULE,
        &[MANAGER_MODULE_PATH],
    );
    let expressions: Vec<_> =
        selected.iter().map(|diag| range_text(MANAGER_FIND_AFTER_REFERENCE_SOURCE, diag)).collect();
    assert_eq!(
        expressions,
        ["Строка(Ссылка)"],
        "an unread manager module may declare the method, so its call is no candidate, while \
         the platform surface still types the receiver: {selected:#?}"
    );
}

#[test]
fn implicit_database_read_in_cycle_manager_module_method_with_a_search_name_is_not_a_candidate() {
    let manager_module = r#"Функция НайтиПоКоду(Код) Экспорт
    Возврат Неопределено;
КонецФункции
"#;
    let selected =
        candidates_with_manager_module(MANAGER_FIND_AFTER_REFERENCE_SOURCE, manager_module, &[]);
    assert!(
        selected.is_empty(),
        "the user method answers the call before the platform one: {selected:#?}"
    );
}

#[test]
fn implicit_database_read_in_cycle_other_manager_methods_are_not_candidates() {
    let source = r#"Процедура Тест(Коды, Код)
    Для Каждого КодЦикла Из Коды Цикл
        Пустая = Справочники.Товары.ПустаяСсылка();
        Новый = Справочники.Товары.СоздатьЭлемент();
        Выборка = Справочники.Товары.Выбрать();
    КонецЦикла;
    Для Каждого КодЦикла Из Коды Цикл
        Товар = Справочники.Товары.НайтиПоКоду(Код);
    КонецЦикла;
КонецПроцедуры
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Справочники.Товары.НайтиПоКоду(Код)");
    assert_count(source, DiagnosticCode::UnresolvedMethodCall, 0);
}

#[test]
fn implicit_database_read_in_cycle_rejected_string_binding_has_no_candidate() {
    let source = r#"Процедура Тест()
    Для Каждого Ссылка Из Ссылки() Цикл
        Представление = Строка(Ссылка, 1, 2, 3);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 0);
    assert_count(source, DiagnosticCode::MismatchedArgCount, 1);
}

#[test]
fn implicit_database_read_in_cycle_reference_methods_are_not_attribute_reads() {
    let source = r#"Процедура Тест()
    Для Каждого Ссылка Из Ссылки() Цикл
        Пустая = Ссылка.Пустая();
        Идентификатор = Ссылка.УникальныйИдентификатор();
        Мета = Ссылка.Метаданные();
        Заполнена = ЗначениеЗаполнено(Ссылка);
        Тип = Строка(ТипЗнч(Ссылка));
        Артикул = Ссылка.Артикул;
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция Ссылки() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 1);
    assert_eq!(range_text(source, &selected[0]), "Ссылка.Артикул");
}

#[test]
fn implicit_database_read_in_cycle_every_object_reference_kind_is_a_candidate() {
    let source = r#"Процедура Тест()
    Для Каждого Эл1 Из СсылкиСправочник() Цикл
        Представление = Строка(Эл1);
    КонецЦикла;
    Для Каждого Эл2 Из СсылкиДокумент() Цикл
        Представление = Строка(Эл2);
    КонецЦикла;
    Для Каждого Эл3 Из СсылкиПланОбмена() Цикл
        Представление = Строка(Эл3);
    КонецЦикла;
    Для Каждого Эл4 Из СсылкиПланСчетов() Цикл
        Представление = Строка(Эл4);
    КонецЦикла;
    Для Каждого Эл5 Из СсылкиПланВидовХарактеристик() Цикл
        Представление = Строка(Эл5);
    КонецЦикла;
    Для Каждого Эл6 Из СсылкиПланВидовРасчета() Цикл
        Представление = Строка(Эл6);
    КонецЦикла;
    Для Каждого Эл7 Из СсылкиБизнесПроцесс() Цикл
        Представление = Строка(Эл7);
    КонецЦикла;
    Для Каждого Эл8 Из СсылкиЗадача() Цикл
        Представление = Строка(Эл8);
    КонецЦикла;
    Для Каждого Эл9 Из СсылкиЛюбаяСсылка() Цикл
        Представление = Строка(Эл9);
    КонецЦикла;
КонецПроцедуры

// Возвращаемое значение:
//   Массив из СправочникСсылка.Товары
Функция СсылкиСправочник() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из ДокументСсылка.Заказ
Функция СсылкиДокумент() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из ПланОбменаСсылка.Обмен
Функция СсылкиПланОбмена() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из ПланСчетовСсылка.Основной
Функция СсылкиПланСчетов() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из ПланВидовХарактеристикСсылка.Виды
Функция СсылкиПланВидовХарактеристик() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из ПланВидовРасчетаСсылка.Начисления
Функция СсылкиПланВидовРасчета() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из БизнесПроцессСсылка.Процесс
Функция СсылкиБизнесПроцесс() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из ЗадачаСсылка.Поручение
Функция СсылкиЗадача() Экспорт
    Возврат Новый Массив;
КонецФункции

// Возвращаемое значение:
//   Массив из ЛюбаяСсылка
Функция СсылкиЛюбаяСсылка() Экспорт
    Возврат Новый Массив;
КонецФункции
"#;
    let selected = assert_count(source, DiagnosticCode::ImplicitDatabaseReadInCycle, 8);
    let expressions: Vec<_> = selected.iter().map(|diag| range_text(source, diag)).collect();
    assert_eq!(
        expressions,
        [
            "Строка(Эл1)",
            "Строка(Эл2)",
            "Строка(Эл3)",
            "Строка(Эл4)",
            "Строка(Эл5)",
            "Строка(Эл6)",
            "Строка(Эл7)",
            "Строка(Эл8)"
        ],
        "any-reference (Эл9) is not an object reference"
    );
}
