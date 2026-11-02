//! The findings of every rule except `FieldsFromJoinsWithoutIsNull` on the BSL fixtures of the
//! crate: the rule moves only its own findings, and the rest stay as they were.
//!
//! Each line is `fixture | code | range | severity | tags | message`, sorted, over
//! `file_diagnostics` with every rule enabled. The snapshot was taken on the revision before
//! the rule changed and must not be rebased by the change that follows it.

use expect_test::{expect, Expect};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use vfs::{FileId, FileSet, VfsPath};

const RULE: DiagnosticCode = DiagnosticCode::FieldsFromJoinsWithoutIsNull;

const FIXTURES: [(&str, &str); 4] = [
    (
        "conditional_begin_before_preproc_try.bsl",
        include_str!("../../src/handlers/fixtures/conditional_begin_before_preproc_try.bsl"),
    ),
    (
        "fields_from_joins_issue_768.bsl",
        include_str!("../../src/handlers/fixtures/fields_from_joins_issue_768.bsl"),
    ),
    (
        "fields_from_joins_issue_773.bsl",
        include_str!("../../src/handlers/fixtures/fields_from_joins_issue_773.bsl"),
    ),
    (
        "query_parse_error_complex_valid.bsl",
        include_str!("../../src/handlers/fixtures/query_parse_error_complex_valid.bsl"),
    ),
];

/// `line:column` (from 1, in characters) of the byte offset.
fn position(source: &str, offset: usize) -> String {
    let before = &source[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |last| last.chars().count()) + 1;
    format!("{line}:{column}")
}

/// The findings of `source` as sorted lines. `with_rule` keeps the findings of the rule that
/// changes, which the snapshot leaves out and a control puts back.
fn projection(name: &str, source: &str, with_rule: bool) -> Vec<String> {
    let file_id = FileId(0);
    let mut db = RootDatabaseImpl::new();
    let mut files = FileSet::default();
    files.insert(file_id, VfsPath::new("/test.bsl"));
    db.set_file_text(file_id, source);
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(file_id, SourceRootId(0));

    let mut lines: Vec<String> =
        ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
            .into_iter()
            .filter(|d| with_rule || d.code != RULE)
            .map(|d| {
                format!(
                    "{name} | {} | {}..{} | {:?} | {:?} | {}",
                    d.code.as_str(),
                    position(source, usize::from(d.range.start())),
                    position(source, usize::from(d.range.end())),
                    d.severity,
                    d.tags,
                    d.message,
                )
            })
            .collect();
    lines.sort();
    lines
}

fn all_fixtures(with_rule: bool) -> String {
    let mut lines: Vec<String> =
        FIXTURES.iter().flat_map(|(name, source)| projection(name, source, with_rule)).collect();
    lines.sort();
    lines.join("\n")
}

fn assert_projection(expected: Expect) {
    expected.assert_eq(&all_fixtures(false));
}

#[test]
fn other_codes_on_the_bsl_fixtures_are_unchanged() {
    assert_projection(expect![[r#"
        conditional_begin_before_preproc_try.bsl | CodeOutOfRegion | 1:11..1:15 | Hint | [] | Процедура находится вне области (#Область/#Region). Весь код модуля должен быть организован в области для лучшей структуры.
        conditional_begin_before_preproc_try.bsl | PairingBrokenTransaction | 10:9..10:29 | Major | [] | Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
        conditional_begin_before_preproc_try.bsl | PairingBrokenTransaction | 16:9..16:34 | Major | [] | Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
        conditional_begin_before_preproc_try.bsl | PairingBrokenTransaction | 18:9..18:29 | Major | [] | Нарушена парность использования метода 'НачатьТранзакцию' и 'ОтменитьТранзакцию'
        conditional_begin_before_preproc_try.bsl | PairingBrokenTransaction | 8:9..8:34 | Major | [] | Нарушена парность использования метода 'НачатьТранзакцию' и 'ЗафиксироватьТранзакцию'
        fields_from_joins_issue_768.bsl | CodeOutOfRegion | 2:9..2:36 | Hint | [] | Функция находится вне области (#Область/#Region). Весь код модуля должен быть организован в области для лучшей структуры.
        fields_from_joins_issue_768.bsl | MissingParameterDescription | 34:9..34:24 | Warning | [] | Необходимо добавить описание всех параметров метода
        fields_from_joins_issue_768.bsl | MissingReturnedValueDescription | 16:9..16:23 | Warning | [] | Добавьте описание возвращаемого значения функции
        fields_from_joins_issue_768.bsl | MissingReturnedValueDescription | 2:9..2:36 | Warning | [] | Добавьте описание возвращаемого значения функции
        fields_from_joins_issue_768.bsl | MissingReturnedValueDescription | 34:9..34:24 | Warning | [] | Добавьте описание возвращаемого значения функции
        fields_from_joins_issue_768.bsl | MissingReturnedValueDescription | 50:9..50:22 | Warning | [] | Добавьте описание возвращаемого значения функции
        fields_from_joins_issue_768.bsl | MissingReturnedValueDescription | 63:9..63:30 | Warning | [] | Добавьте описание возвращаемого значения функции
        fields_from_joins_issue_773.bsl | CodeOutOfRegion | 2:11..2:21 | Hint | [] | Процедура находится вне области (#Область/#Region). Весь код модуля должен быть организован в области для лучшей структуры.
        query_parse_error_complex_valid.bsl | CodeOutOfRegion | 1:11..1:15 | Hint | [] | Процедура находится вне области (#Область/#Region). Весь код модуля должен быть организован в области для лучшей структуры.
        query_parse_error_complex_valid.bsl | DeprecatedPlatformApi | 90:36..90:47 | Warning | [Deprecated] | Используйте "ТекущаяДатаСеанса" вместо устаревшего "ТекущаяДата"
        query_parse_error_complex_valid.bsl | InvalidCharacterInFile | 29:3..29:67 | Major | [] | Нужно исправить на правильный символ "-"
        query_parse_error_complex_valid.bsl | InvalidCharacterInFile | 60:3..60:73 | Major | [] | Нужно исправить на правильный символ "-"
        query_parse_error_complex_valid.bsl | JoinWithVirtualTable | 20:5..20:54 | Warning | [] | Не следует использовать соединения с виртуальными таблицами
        query_parse_error_complex_valid.bsl | JoinWithVirtualTable | 21:23..21:90 | Warning | [] | Не следует использовать соединения с виртуальными таблицами
        query_parse_error_complex_valid.bsl | JoinWithVirtualTable | 24:23..24:73 | Warning | [] | Не следует использовать соединения с виртуальными таблицами
        query_parse_error_complex_valid.bsl | JoinWithVirtualTable | 54:23..54:73 | Warning | [] | Не следует использовать соединения с виртуальными таблицами
        query_parse_error_complex_valid.bsl | ProcedureReturnsValue | 96:2..96:41 | Blocker | [] | Процедура не должна возвращать значение
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 62:3..62:14 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 63:3..63:68 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 64:3..64:44 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 65:3..65:66 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 66:3..66:21 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 67:3..67:12 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 79:3..79:20 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 80:3..80:20 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 81:3..81:13 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 82:3..82:12 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 83:3..83:53 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 84:3..84:16 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 85:3..85:15 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 86:3..86:13 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | SpaceAtStartComment | 87:3..87:16 | Hint | [] | Комментарий должен иметь пробел после //
        query_parse_error_complex_valid.bsl | UnresolvedField | 91:37..91:55 | Major | [] | Поле 'Склады' не найдено у типа 'СправочникМенеджер'
        query_parse_error_complex_valid.bsl | UnresolvedField | 92:45..92:63 | Major | [] | Поле 'Склады' не найдено у типа 'СправочникМенеджер'
        query_parse_error_complex_valid.bsl | UnusedLocalMethod | 1:11..1:15 | Warning | [Unnecessary] | Неиспользуемый локальный метод "Тест"
        query_parse_error_complex_valid.bsl | VirtualTableCallWithoutParameters | 24:23..24:73 | Major | [] | Не следует использовать виртуальные таблицы без параметров
        query_parse_error_complex_valid.bsl | VirtualTableCallWithoutParameters | 54:23..54:73 | Major | [] | Не следует использовать виртуальные таблицы без параметров"#]]);
}

/// The control of the snapshot above: a projection that saw no findings at all would match any
/// snapshot. A field of a query without an alias is reported by `AssignAliasFieldsInQuery`
/// and the alias takes the finding away, so the projection does see other codes and does
/// follow the text.
#[test]
fn the_projection_sees_the_findings_of_other_rules() {
    let module = |select: &str| {
        format!(
            "Процедура Выбрать(Запрос)\n\tЗапрос.Текст = \"ВЫБРАТЬ {select} ИЗ Справочник.Товары КАК Товары\";\nКонецПроцедуры\n"
        )
    };
    let alias_code = DiagnosticCode::AssignAliasFieldsInQuery.as_str();
    let has_alias_finding = |source: &str| {
        projection("module.bsl", source, false).iter().any(|line| line.contains(alias_code))
    };

    assert!(has_alias_finding(&module("Товары.Код")));
    assert!(!has_alias_finding(&module("Товары.Код КАК Код")));
}

/// The other half of the control: the rule that changes is the one the projection leaves out,
/// and putting it back shows its finding, which the snapshot must not contain.
#[test]
fn the_projection_leaves_out_only_the_rule_that_changes() {
    let source = "Процедура Выбрать(Запрос)\n\tЗапрос.Текст = \"ВЫБРАТЬ Цены.Цена КАК Цена ИЗ Документ.Заказ КАК Док ЛЕВОЕ СОЕДИНЕНИЕ РегистрСведений.Цены КАК Цены ПО Док.Ключ = Цены.Ключ\";\nКонецПроцедуры\n";
    let rule = RULE.as_str();

    assert!(projection("module.bsl", source, true).iter().any(|line| line.contains(rule)));
    assert!(!projection("module.bsl", source, false).iter().any(|line| line.contains(rule)));
}
