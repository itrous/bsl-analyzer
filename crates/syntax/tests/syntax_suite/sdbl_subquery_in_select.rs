//! The select-list position of a nested query, judged where both routes read parse errors.
//!
//! An integration test because the check needs the parser, and the parser depends on this
//! crate: inside a unit test the two would be different copies of the same types.

use syntax::sdbl_query::collect_query_parse_errors;

const MESSAGE: &str = "Вложенный запрос в списке полей выборки недопустим: он возможен только \
                       как источник в ИЗ и справа от В (...)";

/// Every parse error of `query` as the text it covers and the message a user sees. Comparing
/// all of them, not only the synthetic ones, also proves that each query parses cleanly
/// otherwise: a broken input would make a silent control silent for the wrong reason.
fn errors(query: &str) -> Vec<(String, String)> {
    collect_query_parse_errors(&parser::parse_sdbl(query))
        .into_iter()
        .map(|(range, error)| (query[range].to_string(), error.format_ru()))
        .collect()
}

fn reported(slices: &[&str]) -> Vec<(String, String)> {
    slices.iter().map(|slice| (slice.to_string(), MESSAGE.to_string())).collect()
}

#[test]
fn the_minimal_select_list_subquery_is_reported_over_its_parentheses() {
    assert_eq!(errors("ВЫБРАТЬ (ВЫБРАТЬ 1) КАК А"), reported(&["(ВЫБРАТЬ 1)"]));
    assert_eq!(errors("SELECT (SELECT 1) AS A"), reported(&["(SELECT 1)"]));
}

#[test]
fn a_correlated_subquery_in_the_select_list_is_reported() {
    let subquery = "(ВЫБРАТЬ КОЛИЧЕСТВО(З2.Ссылка) КАК К \
                    ИЗ Справочник.Задачи КАК З2 ГДЕ З2.Родитель = Задачи.Ссылка)";
    let query = format!(
        "ВЫБРАТЬ Задачи.Ссылка КАК Ссылка, {subquery} КАК Дочерних ИЗ Справочник.Задачи КАК Задачи"
    );
    assert_eq!(errors(&query), reported(&[subquery]));
}

/// The position is the select list, not the top of a field: an operand of a function, of
/// arithmetic or of a `ВЫБОР` branch is still inside it.
#[test]
fn a_subquery_deep_inside_a_field_expression_is_reported() {
    assert_eq!(
        errors(
            "ВЫБРАТЬ ЕСТЬNULL((ВЫБРАТЬ ПЕРВЫЕ 1 Т.Код КАК Код ИЗ Справочник.Т КАК Т), 0) КАК Код"
        ),
        reported(&["(ВЫБРАТЬ ПЕРВЫЕ 1 Т.Код КАК Код ИЗ Справочник.Т КАК Т)"]),
    );
    assert_eq!(
        errors("ВЫБРАТЬ ВЫБОР КОГДА ИСТИНА ТОГДА 1 + (ВЫБРАТЬ 2) ИНАЧЕ 0 КОНЕЦ КАК А"),
        reported(&["(ВЫБРАТЬ 2)"]),
    );
}

#[test]
fn every_offending_field_is_reported_on_its_own() {
    assert_eq!(
        errors("ВЫБРАТЬ (ВЫБРАТЬ 1) КАК А, 2 КАК Б, (ВЫБРАТЬ 3) КАК Вл"),
        reported(&["(ВЫБРАТЬ 1)", "(ВЫБРАТЬ 3)"]),
    );
}

/// Each `ОБЪЕДИНИТЬ` branch has a select list of its own, and the first one is not the only one
/// looked at.
#[test]
fn a_subquery_in_the_select_list_of_any_union_branch_is_reported() {
    assert_eq!(
        errors("ВЫБРАТЬ 1 КАК А ОБЪЕДИНИТЬ ВСЕ ВЫБРАТЬ (ВЫБРАТЬ 2)"),
        reported(&["(ВЫБРАТЬ 2)"]),
    );
    assert_eq!(
        errors("ВЫБРАТЬ (ВЫБРАТЬ 1) КАК А ОБЪЕДИНИТЬ ВЫБРАТЬ 2"),
        reported(&["(ВЫБРАТЬ 1)"]),
    );
}

/// An allowed nested query has a select list of its own, and the rule holds there as well:
/// being inside `ИЗ (...)` or `В (...)` legalises that nested query, not what its fields contain.
#[test]
fn a_select_list_subquery_inside_an_allowed_nested_query_is_reported() {
    assert_eq!(
        errors("ВЫБРАТЬ Вл.А КАК А ИЗ (ВЫБРАТЬ (ВЫБРАТЬ 1) КАК А) КАК Вл"),
        reported(&["(ВЫБРАТЬ 1)"]),
    );
    assert_eq!(
        errors(
            "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т \
             ГДЕ Т.Ссылка В (ВЫБРАТЬ (ВЫБРАТЬ 1) КАК А ИЗ Справочник.Т КАК Т2)"
        ),
        reported(&["(ВЫБРАТЬ 1)"]),
    );
    assert_eq!(
        errors(
            "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т \
             ГДЕ НЕ Т.Ссылка В (ВЫБРАТЬ (ВЫБРАТЬ 1) КАК А ИЗ Справочник.Т КАК Т2)"
        ),
        reported(&["(ВЫБРАТЬ 1)"]),
    );
}

/// A nested query is judged by the query it belongs to. One sitting in a condition of an
/// offending select-list subquery is not a second offence of that select list, so the finding
/// is the outer subquery alone.
#[test]
fn a_subquery_in_a_condition_of_an_offending_one_is_not_reported_again() {
    let outer = "(ВЫБРАТЬ 1 КАК Ч ИЗ Справочник.Т КАК Т2 \
                 ГДЕ Т2.Ссылка В (ВЫБРАТЬ Т3.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т3) \
                 И Т2.Ссылка = (ВЫБРАТЬ ПЕРВЫЕ 1 Т4.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т4))";
    assert_eq!(errors(&format!("ВЫБРАТЬ {outer} КАК А")), reported(&[outer]));
}

/// A select-list subquery inside another one's select list is an offence of its own.
#[test]
fn a_select_list_subquery_nested_in_another_is_reported_at_both_levels() {
    assert_eq!(
        errors("ВЫБРАТЬ (ВЫБРАТЬ (ВЫБРАТЬ 1) КАК Б) КАК А"),
        reported(&["(ВЫБРАТЬ (ВЫБРАТЬ 1) КАК Б)", "(ВЫБРАТЬ 1)"]),
    );
}

/// The forms the platform accepts: a nested query as a source, joined or not, and on the right
/// of `В` / `НЕ В` — in a condition and in a select-list expression alike, and with nested
/// queries of their own that keep to the same places.
#[test]
fn a_nested_query_where_the_platform_accepts_one_is_not_reported() {
    for query in [
        "ВЫБРАТЬ Вл.Ссылка КАК Ссылка ИЗ (ВЫБРАТЬ З.Ссылка КАК Ссылка ИЗ Справочник.Задачи КАК З) КАК Вл",
        "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т \
         ЛЕВОЕ СОЕДИНЕНИЕ (ВЫБРАТЬ Т2.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т2) КАК Вл \
         ПО Т.Ссылка = Вл.Ссылка",
        "ВЫБРАТЬ Задачи.Ссылка КАК Ссылка ИЗ Справочник.Задачи КАК Задачи \
         ГДЕ Задачи.Ссылка В (ВЫБРАТЬ З2.Родитель КАК Родитель ИЗ Справочник.Задачи КАК З2)",
        "ВЫБРАТЬ Задачи.Ссылка КАК Ссылка ИЗ Справочник.Задачи КАК Задачи \
         ГДЕ НЕ Задачи.Ссылка В (ВЫБРАТЬ З2.Родитель КАК Родитель ИЗ Справочник.Задачи КАК З2)",
        "ВЫБРАТЬ ВЫБОР КОГДА Задачи.Ссылка В (ВЫБРАТЬ З2.Родитель КАК Родитель \
         ИЗ Справочник.Задачи КАК З2) ТОГДА 1 ИНАЧЕ 0 КОНЕЦ КАК Признак \
         ИЗ Справочник.Задачи КАК Задачи",
        "ВЫБРАТЬ ВЫБОР КОГДА НЕ Задачи.Ссылка В (ВЫБРАТЬ З2.Родитель КАК Родитель \
         ИЗ Справочник.Задачи КАК З2) ТОГДА 1 ИНАЧЕ 0 КОНЕЦ КАК Признак \
         ИЗ Справочник.Задачи КАК Задачи",
        "ВЫБРАТЬ ВЫБОР КОГДА Задачи.Ссылка В ИЕРАРХИИ (ВЫБРАТЬ З2.Родитель КАК Родитель \
         ИЗ Справочник.Задачи КАК З2) ТОГДА 1 ИНАЧЕ 0 КОНЕЦ КАК Признак \
         ИЗ Справочник.Задачи КАК Задачи",
        "ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т \
         ГДЕ Т.Ссылка В (ВЫБРАТЬ Вл.Ссылка КАК Ссылка \
         ИЗ (ВЫБРАТЬ Т2.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т2 \
         ГДЕ Т2.Родитель В (ВЫБРАТЬ Т3.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т3)) КАК Вл)",
        "ВЫБРАТЬ Вл.Ссылка КАК Ссылка ИЗ (ВЫБРАТЬ Т.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т) КАК Вл \
         ОБЪЕДИНИТЬ ВСЕ \
         ВЫБРАТЬ Т.Ссылка ИЗ Справочник.Т КАК Т \
         ГДЕ Т.Ссылка В (ВЫБРАТЬ Т2.Ссылка КАК Ссылка ИЗ Справочник.Т КАК Т2)",
    ] {
        assert_eq!(errors(query), Vec::new(), "{query}");
    }
}

/// `В` legalises only its right side. A nested query on the left of `В` / `В ИЕРАРХИИ` in a
/// select list is still a select-list operand, while the right side next to it stays silent.
#[test]
fn a_subquery_on_the_left_of_in_in_the_select_list_is_reported() {
    assert_eq!(errors("ВЫБРАТЬ (ВЫБРАТЬ 1) В (1) КАК А"), reported(&["(ВЫБРАТЬ 1)"]));
    assert_eq!(errors("ВЫБРАТЬ (ВЫБРАТЬ 1) НЕ В (1) КАК А"), reported(&["(ВЫБРАТЬ 1)"]));
    assert_eq!(
        errors("ВЫБРАТЬ (ВЫБРАТЬ 1) В ИЕРАРХИИ (&Группы) КАК А"),
        reported(&["(ВЫБРАТЬ 1)"]),
    );
    assert_eq!(
        errors("ВЫБРАТЬ (ВЫБРАТЬ 1) В (ВЫБРАТЬ Т.Код КАК Код ИЗ Справочник.Т КАК Т) КАК А"),
        reported(&["(ВЫБРАТЬ 1)"]),
    );
}

/// A parenthesised nested query inside the value list of `В` / `В ИЕРАРХИИ` is not attested
/// either way, so the whole right side of `В` is left unreported rather than guessed at.
#[test]
fn the_value_list_of_in_is_left_unreported() {
    for query in [
        "ВЫБРАТЬ ВЫБОР КОГДА 1 В ((ВЫБРАТЬ 1 КАК Ч)) ТОГДА 1 ИНАЧЕ 0 КОНЕЦ КАК А",
        "ВЫБРАТЬ ВЫБОР КОГДА &Ссылка В ИЕРАРХИИ ((ВЫБРАТЬ 1 КАК Ч)) ТОГДА 1 ИНАЧЕ 0 КОНЕЦ КАК А",
    ] {
        assert_eq!(errors(query), Vec::new(), "{query}");
    }
}
