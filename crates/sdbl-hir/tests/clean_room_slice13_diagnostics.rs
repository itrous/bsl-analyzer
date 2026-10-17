//! Query-quality diagnostics of the HIR lowering, derived from 1C standards and
//! the local diagnostic API.
//!
//! Each input is written for this file from the cited 1C standard or methodology page.
//! The rendering keeps the diagnostic variant, its payload and the text it points at, so
//! a wrong range is as visible as a missing report.

use std::fmt::Write;

use expect_test::{expect, Expect};
use sdbl_hir::{lower_sdbl_to_hir, SdblDiagnostic};

fn render(queries: &[&str], keep: fn(&SdblDiagnostic) -> bool) -> String {
    let mut out = String::new();
    for query in queries {
        writeln!(out, "{query}").unwrap();
        let package = lower_sdbl_to_hir(&parser::parse_sdbl(query), None);
        for diagnostic in package.all_diagnostics().filter(|d| keep(d)) {
            let range = diagnostic.range();
            let text = &query[usize::from(range.start())..usize::from(range.end())];
            writeln!(out, "  {diagnostic:?} `{text}`").unwrap();
        }
    }
    out
}

fn check(queries: &[&str], keep: fn(&SdblDiagnostic) -> bool, expect: Expect) {
    expect.assert_eq(&render(queries, keep));
}

// v8std #435 §1.1: a full outer join is reported once per occurrence.
#[test]
fn full_outer_join() {
    check(
        &[
            "ВЫБРАТЬ П.Номенклатура КАК Н ИЗ План КАК П ПОЛНОЕ СОЕДИНЕНИЕ Факт КАК Ф ПО П.Номенклатура = Ф.Номенклатура",
            "SELECT P.Item AS I FROM Plan AS P FULL OUTER JOIN Fact AS F ON P.Item = F.Item",
            "ВЫБРАТЬ П.Номенклатура КАК Н ИЗ План КАК П ПОЛНОЕ СОЕДИНЕНИЕ Факт КАК Ф ПО П.Номенклатура = Ф.Номенклатура ПОЛНОЕ СОЕДИНЕНИЕ Прогноз КАК Пр ПО П.Номенклатура = Пр.Номенклатура",
            "ВЫБРАТЬ П.Номенклатура КАК Н ИЗ План КАК П ЛЕВОЕ СОЕДИНЕНИЕ Факт КАК Ф ПО П.Номенклатура = Ф.Номенклатура",
        ],
        |d| matches!(d, SdblDiagnostic::FullOuterJoin { .. }),
        expect![[r#"
            ВЫБРАТЬ П.Номенклатура КАК Н ИЗ План КАК П ПОЛНОЕ СОЕДИНЕНИЕ Факт КАК Ф ПО П.Номенклатура = Ф.Номенклатура
              FullOuterJoin { range: 77..192 } `ПОЛНОЕ СОЕДИНЕНИЕ Факт КАК Ф ПО П.Номенклатура = Ф.Номенклатура`
            SELECT P.Item AS I FROM Plan AS P FULL OUTER JOIN Fact AS F ON P.Item = F.Item
              FullOuterJoin { range: 34..78 } `FULL OUTER JOIN Fact AS F ON P.Item = F.Item`
            ВЫБРАТЬ П.Номенклатура КАК Н ИЗ План КАК П ПОЛНОЕ СОЕДИНЕНИЕ Факт КАК Ф ПО П.Номенклатура = Ф.Номенклатура ПОЛНОЕ СОЕДИНЕНИЕ Прогноз КАК Пр ПО П.Номенклатура = Пр.Номенклатура
              FullOuterJoin { range: 77..192 } `ПОЛНОЕ СОЕДИНЕНИЕ Факт КАК Ф ПО П.Номенклатура = Ф.Номенклатура`
              FullOuterJoin { range: 193..318 } `ПОЛНОЕ СОЕДИНЕНИЕ Прогноз КАК Пр ПО П.Номенклатура = Пр.Номенклатура`
            ВЫБРАТЬ П.Номенклатура КАК Н ИЗ План КАК П ЛЕВОЕ СОЕДИНЕНИЕ Факт КАК Ф ПО П.Номенклатура = Ф.Номенклатура
        "#]],
    );
}

// v8std #655 §1.1: a nested query in a join is reported on either side, a lone nested
// source is not.
#[test]
fn join_with_nested_query() {
    check(
        &[
            "ВЫБРАТЬ Д.Ссылка КАК Ссылка ИЗ Документ.Реализация КАК Д ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л) КАК Лим ПО ИСТИНА",
            "ВЫБРАТЬ Лим.Сумма КАК Сумма ИЗ (ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л) КАК Лим ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Реализация КАК Д ПО ИСТИНА",
            "ВЫБРАТЬ Лим.Сумма КАК Сумма ИЗ (ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л) КАК Лим",
            "ВЫБРАТЬ Лим.Сумма КАК Сумма ИЗ (ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л) КАК Лим, Документ.Реализация КАК Д",
        ],
        |d| matches!(d, SdblDiagnostic::JoinWithSubQuery { .. }),
        expect![[r#"
            ВЫБРАТЬ Д.Ссылка КАК Ссылка ИЗ Документ.Реализация КАК Д ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л) КАК Лим ПО ИСТИНА
              JoinWithSubQuery { range: 137..242 } `ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л`
            ВЫБРАТЬ Лим.Сумма КАК Сумма ИЗ (ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л) КАК Лим ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Реализация КАК Д ПО ИСТИНА
              JoinWithSubQuery { range: 57..162 } `ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л`
            ВЫБРАТЬ Лим.Сумма КАК Сумма ИЗ (ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л) КАК Лим
            ВЫБРАТЬ Лим.Сумма КАК Сумма ИЗ (ВЫБРАТЬ Л.Сумма КАК Сумма ИЗ РегистрСведений.Лимиты КАК Л) КАК Лим, Документ.Реализация КАК Д
        "#]],
    );
}

// v8std #655 §2: a virtual table in a join is reported with its kind, on either side.
#[test]
fn join_with_virtual_table() {
    check(
        &[
            "ВЫБРАТЬ Н.Ссылка КАК Ссылка ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата) КАК О ПО О.Номенклатура = Н.Ссылка",
            "ВЫБРАТЬ О.Номенклатура КАК Н ИЗ РегистрСведений.Цены.СрезПоследних(&Дата) КАК О ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Номенклатура КАК Н ПО О.Номенклатура = Н.Ссылка",
            "ВЫБРАТЬ О.Номенклатура КАК Н ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата) КАК О",
        ],
        |d| matches!(d, SdblDiagnostic::JoinWithVirtualTable { .. }),
        expect![[r#"
            ВЫБРАТЬ Н.Ссылка КАК Ссылка ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата) КАК О ПО О.Номенклатура = Н.Ссылка
              JoinWithVirtualTable { table_name: "РегистрНакопления.ТоварыНаСкладах.Остатки", virtual_table_type: "остатки", range: 144..235 } `РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата)`
            ВЫБРАТЬ О.Номенклатура КАК Н ИЗ РегистрСведений.Цены.СрезПоследних(&Дата) КАК О ЛЕВОЕ СОЕДИНЕНИЕ Справочник.Номенклатура КАК Н ПО О.Номенклатура = Н.Ссылка
              JoinWithVirtualTable { table_name: "РегистрСведений.Цены.СрезПоследних", virtual_table_type: "срезпоследних", range: 58..135 } `РегистрСведений.Цены.СрезПоследних(&Дата)`
            ВЫБРАТЬ О.Номенклатура КАК Н ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(&Дата) КАК О
        "#]],
    );
}

// v8std #658 §2: every disjunction of WHERE is a review point; nested queries report
// their own condition once.
#[test]
fn disjunction_in_where() {
    check(
        &[
            "ВЫБРАТЬ Т.Поле1 КАК П ИЗ Таблица КАК Т ГДЕ Т.Поле1 = &Значение1 ИЛИ Т.Поле2 = &Значение2",
            "ВЫБРАТЬ Т.Поле1 КАК П ИЗ Таблица КАК Т ГДЕ Т.Поле1 = 1 ИЛИ Т.Поле2 = 2 ИЛИ Т.Поле3 = 3",
            "ВЫБРАТЬ Т.Поле1 КАК П ИЗ Таблица КАК Т ГДЕ Т.Поле1 В (ВЫБРАТЬ Д.Поле1 ИЗ Другая КАК Д ГДЕ Д.Поле2 = 1 ИЛИ Д.Поле3 = 2)",
            "ВЫБРАТЬ Т.Поле1 КАК П ИЗ Таблица КАК Т ГДЕ Т.Поле1 = &Значение1 И Т.Поле2 = &Значение2",
        ],
        |d| matches!(d, SdblDiagnostic::LogicalOrInWhere { .. }),
        expect![[r#"
            ВЫБРАТЬ Т.Поле1 КАК П ИЗ Таблица КАК Т ГДЕ Т.Поле1 = &Значение1 ИЛИ Т.Поле2 = &Значение2
              LogicalOrInWhere { range: 109..115 } `ИЛИ`
            ВЫБРАТЬ Т.Поле1 КАК П ИЗ Таблица КАК Т ГДЕ Т.Поле1 = 1 ИЛИ Т.Поле2 = 2 ИЛИ Т.Поле3 = 3
              LogicalOrInWhere { range: 92..98 } `ИЛИ`
              LogicalOrInWhere { range: 116..122 } `ИЛИ`
            ВЫБРАТЬ Т.Поле1 КАК П ИЗ Таблица КАК Т ГДЕ Т.Поле1 В (ВЫБРАТЬ Д.Поле1 ИЗ Другая КАК Д ГДЕ Д.Поле2 = 1 ИЛИ Д.Поле3 = 2)
              LogicalOrInWhere { range: 172..178 } `ИЛИ`
            ВЫБРАТЬ Т.Поле1 КАК П ИЗ Таблица КАК Т ГДЕ Т.Поле1 = &Значение1 И Т.Поле2 = &Значение2
        "#]],
    );
}

// v8std #658 §2.1: a disjunction in a join condition is
// allowed when every operand constrains the same single column with values, so it can
// become one `В (...)` lookup. Comparing the column with fields of the joined table, or
// computing it through a function, involves more than one subject and is reported.
#[test]
fn disjunction_in_join_condition() {
    check(
        &[
            "ВЫБРАТЬ Т.Поле КАК П ИЗ Таблица КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Другая КАК Д ПО Т.Поле = Д.Поле И (Д.Вид = &Вид1 ИЛИ Д.Вид = &Вид2)",
            "ВЫБРАТЬ Т.Поле КАК П ИЗ Таблица КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Другая КАК Д ПО Т.Поле = Д.Поле ИЛИ Т.Поле = Д.Резерв",
            "ВЫБРАТЬ Т.Поле КАК П ИЗ Таблица КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Другая КАК Д ПО Д.Вид = &Вид1 ИЛИ ПОДСТРОКА(Д.Вид, 1, 2) = &Вид2",
        ],
        |d| matches!(d, SdblDiagnostic::LogicalOrInJoin { .. }),
        expect![[r#"
            ВЫБРАТЬ Т.Поле КАК П ИЗ Таблица КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Другая КАК Д ПО Т.Поле = Д.Поле И (Д.Вид = &Вид1 ИЛИ Д.Вид = &Вид2)
            ВЫБРАТЬ Т.Поле КАК П ИЗ Таблица КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Другая КАК Д ПО Т.Поле = Д.Поле ИЛИ Т.Поле = Д.Резерв
              LogicalOrInJoin { range: 153..159 } `ИЛИ`
            ВЫБРАТЬ Т.Поле КАК П ИЗ Таблица КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Другая КАК Д ПО Д.Вид = &Вид1 ИЛИ ПОДСТРОКА(Д.Вид, 1, 2) = &Вид2
              LogicalOrInJoin { range: 148..154 } `ИЛИ`
        "#]],
    );
}

// Expressions mini-spec «String literal — single vs multi»: every double-quoted literal
// with content is a candidate; the empty literal is not.
#[test]
fn string_literal_candidates() {
    check(
        &["ВЫБРАТЬ \"\" КАК Пусто, \"Текст\" КАК Текст"],
        |d| matches!(d, SdblDiagnostic::MultilineString { .. }),
        expect![[r#"
            ВЫБРАТЬ "" КАК Пусто, "Текст" КАК Текст
              MultilineString { range: 37..49 } `"Текст"`
        "#]],
    );
    let spanning = "ВЫБРАТЬ \"Первая строка\nвторая строка\" КАК Текст";
    let package = lower_sdbl_to_hir(&parser::parse_sdbl(spanning), None);
    let candidate = package
        .all_diagnostics()
        .filter(|d| matches!(d, SdblDiagnostic::MultilineString { .. }))
        .collect::<Vec<_>>();
    assert_eq!(candidate.len(), 1);
    let SdblDiagnostic::MultilineString { range } = candidate[0] else {
        unreachable!();
    };
    assert_eq!(usize::from(range.start()), spanning.find('\"').unwrap());
    assert_eq!(usize::from(range.end()), spanning.find(" КАК").unwrap());
    assert_eq!(
        &spanning[usize::from(range.start())..usize::from(range.end())],
        "\"Первая строка\nвторая строка\""
    );

    let escaped = "ВЫБРАТЬ \"Текст с \"\"кавычками\"\"\" КАК Значение";
    let package = lower_sdbl_to_hir(&parser::parse_sdbl(escaped), None);
    let candidates = package
        .all_diagnostics()
        .filter_map(|d| match d {
            SdblDiagnostic::MultilineString { range } => {
                Some((usize::from(range.start()), usize::from(range.end())))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let start = escaped.find('\"').unwrap();
    let end = escaped.find(" КАК").unwrap();
    assert_eq!(candidates, vec![(start, end)]);
    assert_eq!(&escaped[start..end], "\"Текст с \"\"кавычками\"\"\"");
}

// v8std #437 §2, §2б: a result column needs an explicit name given with КАК.
#[test]
fn field_alias_requirements() {
    check(
        &[
            "ВЫБРАТЬ Касса.Валюта, Касса.Валюта Валюта, Касса.Валюта КАК Валюта, Касса.* ИЗ Справочник.Кассы КАК Касса",
            "ВЫБРАТЬ Касса.Валюта.Наименование ИЗ Справочник.Кассы КАК Касса",
            "ВЫБРАТЬ К.Валюта КАК Валюта ИЗ Справочник.Кассы КАК К ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ К.Валюта ИЗ Справочник.Кассы КАК К",
            "ВЫБРАТЬ *, К.*, К., 1 КАК Н ИЗ Справочник.Кассы КАК К",
        ],
        |d| matches!(d, SdblDiagnostic::AliasWithoutAsKeyword { .. }),
        expect![[r#"
            ВЫБРАТЬ Касса.Валюта, Касса.Валюта Валюта, Касса.Валюта КАК Валюта, Касса.* ИЗ Справочник.Кассы КАК Касса
              AliasWithoutAsKeyword { field_name: None, raw_name: Some("Валюта"), range: 15..38 } `Касса.Валюта`
              AliasWithoutAsKeyword { field_name: Some("Валюта"), raw_name: Some("Валюта"), range: 40..76 } `Касса.Валюта Валюта`
            ВЫБРАТЬ Касса.Валюта.Наименование ИЗ Справочник.Кассы КАК Касса
              AliasWithoutAsKeyword { field_name: None, raw_name: Some("Наименование"), range: 15..63 } `Касса.Валюта.Наименование`
            ВЫБРАТЬ К.Валюта КАК Валюта ИЗ Справочник.Кассы КАК К ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ К.Валюта ИЗ Справочник.Кассы КАК К
            ВЫБРАТЬ *, К.*, К., 1 КАК Н ИЗ Справочник.Кассы КАК К
        "#]],
    );
}

// v8std #654 §1.1–1.2 and pubqlang «Использовать параметры виртуальных таблиц»: dotted
// paths carry their length; inside a virtual table condition the path has none.
#[test]
fn fields_through_a_dot() {
    check(
        &[
            "ВЫБРАТЬ ТоварныеЗапасы.Товар.Артикул КАК Артикул ИЗ РегистрНакопления.ТоварныеЗапасы КАК ТоварныеЗапасы",
            "ВЫБРАТЬ О.КоличествоОстаток КАК К ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(, Номенклатура.Родитель = &Группа) КАК О",
            "ВЫБРАТЬ 1 КАК Н ИЗ Справочник.Товары ГДЕ Справочник.Товары.Код = 1",
        ],
        |d| matches!(d, SdblDiagnostic::QueryNestedFieldsByDot { .. }),
        expect![[r#"
            ВЫБРАТЬ ТоварныеЗапасы.Товар.Артикул КАК Артикул ИЗ РегистрНакопления.ТоварныеЗапасы КАК ТоварныеЗапасы
              QueryNestedFieldsByDot { range: 15..69, parts_count: Some(3) } `ТоварныеЗапасы.Товар.Артикул`
            ВЫБРАТЬ О.КоличествоОстаток КАК К ИЗ РегистрНакопления.ТоварыНаСкладах.Остатки(, Номенклатура.Родитель = &Группа) КАК О
              QueryNestedFieldsByDot { range: 15..52, parts_count: Some(2) } `О.КоличествоОстаток`
              QueryNestedFieldsByDot { range: 151..192, parts_count: None } `Номенклатура.Родитель`
            ВЫБРАТЬ 1 КАК Н ИЗ Справочник.Товары ГДЕ Справочник.Товары.Код = 1
        "#]],
    );
}

// metod8dev «Использование функции ЕСТЬNULL()», pubqlang «Левое/Правое/Полное
// соединение»: fields of the optional side are NULL for unmatched rows.
#[test]
fn optional_side_fields_without_null_handling() {
    check(
        &[
            "ВЫБРАТЬ Н.Наименование КАК Н, О.КоличествоОстаток КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка",
            "ВЫБРАТЬ ЕСТЬNULL(О.КоличествоОстаток, 0) КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка",
            "ВЫБРАТЬ ВЫБОР КОГДА О.КоличествоОстаток ЕСТЬ NULL ТОГДА 0 ИНАЧЕ О.КоличествоОстаток КОНЕЦ КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка",
            "ВЫБРАТЬ ВЫБОР КОГДА О.Номенклатура ЕСТЬ NULL ТОГДА О.КоличествоОстаток ИНАЧЕ 0 КОНЕЦ КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка",
            "ВЫБРАТЬ О.КоличествоОстаток КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка ГДЕ О.Номенклатура ЕСТЬ НЕ NULL",
            "ВЫБРАТЬ Н.Наименование КАК Н, Ц.Цена КАК Ц ИЗ Справочник.Номенклатура КАК Н ПРАВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Н.Ссылка = Ц.Номенклатура",
            "ВЫБРАТЬ Н.Наименование КАК Н, Ц.Цена КАК Ц ИЗ Справочник.Номенклатура КАК Н ПОЛНОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Н.Ссылка = Ц.Номенклатура",
        ],
        |d| matches!(d, SdblDiagnostic::FieldsFromJoinWithoutNullCheck { .. }),
        expect![[r#"
            ВЫБРАТЬ Н.Наименование КАК Н, О.КоличествоОстаток КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка
              FieldsFromJoinWithoutNullCheck { join_type: Left, range: 163..314, unprotected_fields: [UnprotectedFieldRef { table_alias: "О", field_name: "КоличествоОстаток", range: 54..91 }] } `ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка`
            ВЫБРАТЬ ЕСТЬNULL(О.КоличествоОстаток, 0) КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка
            ВЫБРАТЬ ВЫБОР КОГДА О.КоличествоОстаток ЕСТЬ NULL ТОГДА 0 ИНАЧЕ О.КоличествоОстаток КОНЕЦ КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка
            ВЫБРАТЬ ВЫБОР КОГДА О.Номенклатура ЕСТЬ NULL ТОГДА О.КоличествоОстаток ИНАЧЕ 0 КОНЕЦ КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка
              FieldsFromJoinWithoutNullCheck { join_type: Left, range: 223..374, unprotected_fields: [UnprotectedFieldRef { table_alias: "О", field_name: "КоличествоОстаток", range: 90..127 }] } `ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка`
            ВЫБРАТЬ О.КоличествоОстаток КАК К ИЗ Справочник.Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ РегистрНакопления.Учет.Остатки КАК О ПО О.Номенклатура = Н.Ссылка ГДЕ О.Номенклатура ЕСТЬ НЕ NULL
            ВЫБРАТЬ Н.Наименование КАК Н, Ц.Цена КАК Ц ИЗ Справочник.Номенклатура КАК Н ПРАВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Н.Ссылка = Ц.Номенклатура
              FieldsFromJoinWithoutNullCheck { join_type: Right, range: 137..271, unprotected_fields: [UnprotectedFieldRef { table_alias: "Н", field_name: "Наименование", range: 15..42 }] } `ПРАВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Н.Ссылка = Ц.Номенклатура`
            ВЫБРАТЬ Н.Наименование КАК Н, Ц.Цена КАК Ц ИЗ Справочник.Номенклатура КАК Н ПОЛНОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Н.Ссылка = Ц.Номенклатура
              FieldsFromJoinWithoutNullCheck { join_type: Full, range: 137..271, unprotected_fields: [UnprotectedFieldRef { table_alias: "Ц", field_name: "Цена", range: 54..65 }, UnprotectedFieldRef { table_alias: "Н", field_name: "Наименование", range: 15..42 }] } `ПОЛНОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Н.Ссылка = Ц.Номенклатура`
        "#]],
    );
}

// A table joined with ВНУТРЕННЕЕ is never NULL, even when a left join hangs off it; the
// left-joined price table is optional but none of its fields is used, so nothing is
// reported at all.
#[test]
fn inner_joined_table_with_a_nested_left_join_is_not_optional() {
    let query = "ВЫБРАТЬ Д.Номер КАК Номер, Т.Количество КАК Количество ИЗ Документ.Заказ КАК Д ВНУТРЕННЕЕ СОЕДИНЕНИЕ Документ.Заказ.Товары КАК Т ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Ц ПО Ц.Номенклатура = Т.Номенклатура ПО Т.Ссылка = Д.Ссылка";
    let package = lower_sdbl_to_hir(&parser::parse_sdbl(query), None);
    let reported: Vec<_> = package
        .all_diagnostics()
        .filter(|d| matches!(d, SdblDiagnostic::FieldsFromJoinWithoutNullCheck { .. }))
        .collect();
    assert!(reported.is_empty(), "no optional-side field is used: {reported:?}");
}

#[test]
fn every_query_quality_diagnostic_has_a_message() {
    let query = "ВЫБРАТЬ Т.Поле.Вложенное, \"Текст\" КАК С ИЗ Таблица КАК Т ПОЛНОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ 1 КАК Поле) КАК Д ПО Т.Поле = Д.Поле ИЛИ Т.Х = Д.Поле ГДЕ Т.Поле = 1 ИЛИ Т.Поле ПОДОБНО Т.Шаблон";
    let package = lower_sdbl_to_hir(&parser::parse_sdbl(query), None);
    let mut seen = 0;
    for diagnostic in package.all_diagnostics() {
        assert!(!diagnostic.message().trim().is_empty(), "{diagnostic:?}");
        seen += 1;
    }
    assert!(seen >= 8, "the query is built to trigger every query-quality check, got {seen}");
}

#[test]
fn rewritten_messages_explain_each_diagnostic_and_keep_metadata_identity() {
    use sdbl_hir::LikeUsageKind;

    let range = text_size::TextRange::new(3.into(), 7.into());
    let diagnostics = [
        SdblDiagnostic::QueryToMissingMetadata {
            table_name: "Справочник.Неизвестный".to_string(),
            range,
        },
        SdblDiagnostic::MultilineString { range },
        SdblDiagnostic::QueryNestedFieldsByDot { range, parts_count: Some(3) },
        SdblDiagnostic::RefOveruse { range },
        SdblDiagnostic::LikeUsage { range, kind: LikeUsageKind::Allowed },
        SdblDiagnostic::LikeUsage { range, kind: LikeUsageKind::Incorrect },
    ];
    let mut messages = String::new();
    for diagnostic in diagnostics {
        assert_eq!(diagnostic.range(), range);
        writeln!(messages, "{}", diagnostic.message()).unwrap();
        writeln!(messages, "  code={:?}, error={}", diagnostic.code(), diagnostic.is_error())
            .unwrap();
    }
    expect![[r#"
        В конфигурации нет объекта метаданных для источника 'Справочник.Неизвестный'
          code=Some(122), error=true
        Строковый литерал запроса может продолжаться на следующей строке: перенос строки войдёт в значение литерала
          code=None, error=false
        Поле, полученное через точку от ссылочного поля, добавляет в запрос неявное соединение с таблицей, на которую указывает ссылка
          code=None, error=false
        Поле «Ссылка», полученное через точку от ссылочного поля, совпадает с самим этим полем: лишнее соединение можно убрать
          code=None, error=false
        Шаблон ПОДОБНО должен одинаково работать во всех СУБД: используйте только «%» и «_», а спецсимволы искомого текста экранируйте через СПЕЦСИМВОЛ
          code=None, error=false
        Шаблон ПОДОБНО задаётся строковым литералом или параметром запроса, а не вычисляется в запросе
          code=None, error=true
    "#]]
    .assert_eq(&messages);
}

// Dotted source paths carry their depth regardless of the clause that evaluates them.
#[test]
fn dotted_paths_in_grouping_ordering_and_nested_queries_keep_ranges_and_depth() {
    let query = "ВЫБРАТЬ 1 КАК Н ИЗ Таблица КАК Т СГРУППИРОВАТЬ ПО Т.Группа.Код ИМЕЮЩИЕ МАКСИМУМ(Т.Группа.Цена) > 0 УПОРЯДОЧИТЬ ПО Т.Группа.Имя";
    let package = lower_sdbl_to_hir(&parser::parse_sdbl(query), None);
    assert!(parser::parse_sdbl(query).errors().is_empty());
    let paths: Vec<_> = package
        .all_diagnostics()
        .filter_map(|d| match d {
            SdblDiagnostic::QueryNestedFieldsByDot { range, parts_count } => {
                Some((&query[usize::from(range.start())..usize::from(range.end())], *parts_count))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        paths,
        vec![("Т.Группа.Код", Some(3)), ("Т.Группа.Цена", Some(3)), ("Т.Группа.Имя", Some(3))]
    );

    // Both the nested query and its enclosing source expose this diagnostic to consumers.
    let query = "ВЫБРАТЬ 1 КАК Н ИЗ (ВЫБРАТЬ Т.Группа.Код КАК Код ИЗ Таблица КАК Т) КАК Вл";
    let package = lower_sdbl_to_hir(&parser::parse_sdbl(query), None);
    let paths: Vec<_> = package
        .all_diagnostics()
        .filter_map(|d| match d {
            SdblDiagnostic::QueryNestedFieldsByDot { range, parts_count } => {
                Some((&query[usize::from(range.start())..usize::from(range.end())], *parts_count))
            }
            _ => None,
        })
        .collect();
    assert_eq!(paths, vec![("Т.Группа.Код", Some(3)), ("Т.Группа.Код", Some(3))]);
}

#[test]
fn english_disjunctions_report_their_own_keywords_in_where_and_join() {
    let query = "SELECT T.Code AS Code FROM Goods AS T LEFT JOIN Prices AS P ON T.Code = P.Code OR T.Code = P.Reserve WHERE T.Code = 1 OR T.Code = 2";
    let parse = parser::parse_sdbl(query);
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());
    let package = lower_sdbl_to_hir(&parse, None);
    let operators: Vec<_> = package
        .all_diagnostics()
        .filter_map(|d| match d {
            SdblDiagnostic::LogicalOrInJoin { range } => Some(("join", *range)),
            SdblDiagnostic::LogicalOrInWhere { range } => Some(("where", *range)),
            _ => None,
        })
        .collect();
    let expected: Vec<_> = ["join", "where"]
        .into_iter()
        .zip(query.match_indices(" OR "))
        .map(|(clause, (start, _))| {
            let start = text_size::TextSize::try_from(start + 1).unwrap();
            (clause, text_size::TextRange::at(start, 2.into()))
        })
        .collect();
    assert_eq!(operators, expected);
    assert_eq!(operators.len(), 2);
}

#[test]
fn a_disjunction_of_different_subjects_inside_an_on_subquery_is_reported() {
    let query = "ВЫБРАТЬ Т.Код КАК Код ИЗ Таблица КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Другая КАК Д ПО Т.Код В (ВЫБРАТЬ Вл.Код ИЗ Вложенная КАК Вл ГДЕ Вл.Вид = 1 ИЛИ Вл.ДругойВид = 2)";
    let parse = parser::parse_sdbl(query);
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());
    let package = lower_sdbl_to_hir(&parse, None);
    let operators: Vec<_> = package
        .all_diagnostics()
        .filter_map(|d| match d {
            SdblDiagnostic::LogicalOrInJoin { range } => Some(*range),
            _ => None,
        })
        .collect();
    let start = text_size::TextSize::try_from(query.find("ИЛИ").unwrap()).unwrap();
    assert_eq!(operators, vec![text_size::TextRange::at(start, text_size::TextSize::of("ИЛИ"))]);
}

#[test]
fn an_unprotected_optional_side_field_in_where_keeps_its_identity_and_range() {
    let query = "ВЫБРАТЬ Н.Код КАК Код ИЗ Номенклатура КАК Н ЛЕВОЕ СОЕДИНЕНИЕ Остатки КАК О ПО О.Код = Н.Код ГДЕ О.КоличествоОстаток > 0";
    let package = lower_sdbl_to_hir(&parser::parse_sdbl(query), None);
    let uses: Vec<_> = package
        .all_diagnostics()
        .filter_map(|d| match d {
            SdblDiagnostic::FieldsFromJoinWithoutNullCheck {
                join_type,
                unprotected_fields,
                ..
            } => {
                let fields: Vec<_> = unprotected_fields
                    .iter()
                    .map(|f| (f.table_alias.as_str(), f.field_name.as_str(), f.range))
                    .collect();
                Some((*join_type, fields))
            }
            _ => None,
        })
        .collect();
    let text = "О.КоличествоОстаток";
    let start = text_size::TextSize::try_from(query.find(text).unwrap()).unwrap();
    let range = text_size::TextRange::at(start, text_size::TextSize::of(text));
    assert_eq!(uses, vec![(sdbl_hir::JoinType::Left, vec![("О", "КоличествоОстаток", range)])]);
}

#[test]
fn dotted_paths_in_join_where_and_membership_queries_are_not_skipped() {
    let query = "ВЫБРАТЬ 1 КАК Н ИЗ Таблица КАК Т ЛЕВОЕ СОЕДИНЕНИЕ Другая КАК Д ПО Т.Группа.Код = Д.Код ГДЕ Т.Группа.Цена > 0 И Т.Код В (ВЫБРАТЬ Вл.Группа.Имя ИЗ Вложенная КАК Вл)";
    let parse = parser::parse_sdbl(query);
    assert!(parse.errors().is_empty(), "{:?}", parse.errors());
    let package = lower_sdbl_to_hir(&parse, None);
    let mut paths: Vec<_> = package
        .all_diagnostics()
        .filter_map(|d| match d {
            SdblDiagnostic::QueryNestedFieldsByDot { range, parts_count } => {
                Some((*range, *parts_count))
            }
            _ => None,
        })
        .collect();
    paths.sort_by_key(|(range, depth)| (range.start(), range.end(), *depth));
    let mut expected: Vec<_> = [
        ("Т.Группа.Код", 3),
        ("Д.Код", 2),
        ("Т.Группа.Цена", 3),
        ("Т.Код", 2),
        ("Вл.Группа.Имя", 3),
        ("Вл.Группа.Имя", 3),
    ]
    .into_iter()
    .map(|(text, depth)| {
        let start = text_size::TextSize::try_from(query.find(text).unwrap()).unwrap();
        (text_size::TextRange::at(start, text_size::TextSize::of(text)), Some(depth))
    })
    .collect();
    expected.sort_by_key(|(range, depth)| (range.start(), range.end(), *depth));
    assert_eq!(paths, expected);
}
