//! An unqualified column that more than one source offers.
//!
//! The rule existed in the SDBL lowering long before it reached anyone: it was constructed,
//! carried a message, and had no `DiagnosticCode` and no dispatch entry, so no consumer could
//! ever see it. These tests pin the route from the lowering to a user-visible finding, and —
//! just as importantly — the cases where it must stay quiet.

use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
use std::path::PathBuf;

const DESIGNER_FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");

fn codes(query: &str) -> Vec<String> {
    let config = DiagnosticsConfig::all_enabled();
    let configuration = bsl_metadata::load_from_directory(PathBuf::from(DESIGNER_FIXTURE))
        .expect("the designer fixture loads");

    ide_diagnostics::validate_query_text(&config, Some(&configuration), query)
        .into_iter()
        .map(|d| d.code.as_str().to_string())
        .collect()
}

fn is_ambiguous(query: &str) -> bool {
    codes(query).iter().any(|c| c == DiagnosticCode::AmbiguousFieldInQuery.as_str())
}

#[test]
fn a_bare_column_offered_by_two_sources_is_reported() {
    let query = "ВЫБРАТЬ Наименование КАК П \
                 ИЗ Справочник.Справочник1 КАК А \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Б ПО ИСТИНА";

    assert!(is_ambiguous(query), "two sources offer `Наименование`: {:?}", codes(query));
}

#[test]
fn the_message_names_the_candidate_sources() {
    let config = DiagnosticsConfig::all_enabled();
    let configuration = bsl_metadata::load_from_directory(PathBuf::from(DESIGNER_FIXTURE))
        .expect("the designer fixture loads");
    let query = "ВЫБРАТЬ Наименование КАК П \
                 ИЗ Справочник.Справочник1 КАК А \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Б ПО ИСТИНА";

    let message = ide_diagnostics::validate_query_text(&config, Some(&configuration), query)
        .into_iter()
        .find(|d| d.code == DiagnosticCode::AmbiguousFieldInQuery)
        .expect("the ambiguity is reported")
        .message;

    // Without the candidates the reader cannot pick a qualifier, which is the only fix.
    assert!(message.contains('А') && message.contains('Б'), "must name both sources: {message}");
}

/// The negative controls. A rule that fired on these would be worse than no rule: qualifying
/// the column is exactly the fix the message asks for, and a single-source query has nothing
/// to be ambiguous about.
#[test]
fn a_qualified_column_and_a_single_source_stay_quiet() {
    for (label, query) in [
        (
            "qualified",
            "ВЫБРАТЬ А.Наименование КАК П \
             ИЗ Справочник.Справочник1 КАК А \
             ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Б ПО ИСТИНА",
        ),
        ("single source", "ВЫБРАТЬ Наименование КАК П ИЗ Справочник.Справочник1 КАК А"),
    ] {
        assert!(
            !is_ambiguous(query),
            "[{label}] must not be reported as ambiguous: {:?}",
            codes(query),
        );
    }
}

/// Turning the rule off must actually turn it off — it is new, and a rule that ignores the
/// project's configuration is a rule nobody can live with.
#[test]
fn the_rule_obeys_the_configuration() {
    let query = "ВЫБРАТЬ Наименование КАК П \
                 ИЗ Справочник.Справочник1 КАК А \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Б ПО ИСТИНА";

    let config = DiagnosticsConfig {
        disabled: vec![DiagnosticCode::AmbiguousFieldInQuery],
        ..DiagnosticsConfig::all_enabled()
    };
    let configuration = bsl_metadata::load_from_directory(PathBuf::from(DESIGNER_FIXTURE))
        .expect("the designer fixture loads");

    let found = ide_diagnostics::validate_query_text(&config, Some(&configuration), query);
    assert!(
        !found.iter().any(|d| d.code == DiagnosticCode::AmbiguousFieldInQuery),
        "a disabled rule must not report: {:?}",
        found.iter().map(|d| d.code.as_str()).collect::<Vec<_>>(),
    );
}

/// A second source under a name already taken used to evict the first from the scope without
/// a word — so every later reference to that name resolved against the wrong table and the
/// evicted source's fields were never checked at all. The finding matters less than what it
/// stands for: the eviction is a resolution bug, not just a missing message.
#[test]
fn two_sources_sharing_one_alias_are_reported() {
    let query = "ВЫБРАТЬ Т.Наименование КАК П \
                 ИЗ Справочник.Справочник1 КАК Т \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Т ПО ИСТИНА";

    assert!(
        codes(query).iter().any(|c| c == DiagnosticCode::DuplicateAliasInQuery.as_str()),
        "a colliding source alias must be reported: {:?}",
        codes(query),
    );
}

/// The control: distinct aliases over the same two tables must stay quiet, otherwise the test
/// above would pass on a rule that fires on every join.
#[test]
fn distinct_aliases_over_the_same_tables_stay_quiet() {
    let query = "ВЫБРАТЬ А.Наименование КАК П \
                 ИЗ Справочник.Справочник1 КАК А \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Б ПО ИСТИНА";

    assert!(
        !codes(query).iter().any(|c| c == DiagnosticCode::DuplicateAliasInQuery.as_str()),
        "distinct aliases are not a collision: {:?}",
        codes(query),
    );
}

/// Fields that exist only under settings the metadata model does not read.
///
/// An information register is listed with `Активность`/`НомерСтроки`/`Регистратор` whether or
/// not it is subordinate to a recorder — twice over, in fact: the XML reader injects them as
/// attributes and the query layer adds them again as standard fields. Over-listing is
/// deliberate and safe for a rule that reports a MISSING field. It is the opposite of safe
/// here: counting such a field as an occurrence invents a collision the platform never sees,
/// and this rule is a Blocker.
mod conditional_register_fields {
    use super::*;

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../bsl-metadata/fixtures/register_conditional_fields"
    );

    fn codes_over_fixture(query: &str) -> Vec<String> {
        let config = DiagnosticsConfig::all_enabled();
        let configuration =
            bsl_metadata::load_from_directory(PathBuf::from(FIXTURE)).expect("the fixture loads");

        ide_diagnostics::validate_query_text(&config, Some(&configuration), query)
            .into_iter()
            .map(|d| d.code.as_str().to_string())
            .collect()
    }

    /// Every shape a register is read through, not just the main table.
    ///
    /// The first attempt at this fix marked the query layer's own standard-field set, which the
    /// virtual-table transform discards when it rebuilds a slice's field list from the
    /// register's attributes — so the slice, the commonest way to read a periodic information
    /// register, kept the false positive the fix was written to remove. Marking at the
    /// attribute source covers every shape; this table is what says so.
    #[test]
    fn a_conditional_register_field_is_not_an_occurrence_in_any_register_shape() {
        // `Активность` is a real attribute of the catalog and a conditional field of the
        // independent register, so the platform sees exactly one source for the bare name.
        for (shape, source) in [
            ("main table", "РегистрСведений.Курсы"),
            ("slice last", "РегистрСведений.Курсы.СрезПоследних(&Дата)"),
            ("slice first", "РегистрСведений.Курсы.СрезПервых(&Дата)"),
        ] {
            let query = format!(
                "ВЫБРАТЬ Активность КАК П ИЗ {source} КАК А \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Б ПО ИСТИНА"
            );

            assert!(
                !codes_over_fixture(&query)
                    .iter()
                    .any(|c| c == DiagnosticCode::AmbiguousFieldInQuery.as_str()),
                "[{shape}] a field that may not exist cannot make a name ambiguous: {:?}",
                codes_over_fixture(&query),
            );
        }
    }

    /// `Период` is the conditional field of the other kind: absent from a NON-PERIODIC
    /// information register, and the periodicity is knowable. It gets its own case because the
    /// verdict is made on the main table — this asserts the slice inherits it end to end,
    /// through the real lowering rather than the unit-level shape table.
    #[test]
    fn a_non_periodic_register_does_not_make_period_ambiguous_through_a_slice() {
        let designer = PathBuf::from(DESIGNER_FIXTURE);
        let config = DiagnosticsConfig::all_enabled();
        let configuration =
            bsl_metadata::load_from_directory(designer).expect("the designer fixture loads");

        for (shape, source) in [
            ("main table", "РегистрСведений.РегистрСведений1"),
            ("slice last", "РегистрСведений.РегистрСведений1.СрезПоследних(&Дата)"),
        ] {
            let query = format!(
                "ВЫБРАТЬ Период КАК П ИЗ {source} КАК А \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ РегистрНакопления.РегистрНакопления1 КАК Б ПО ИСТИНА"
            );
            let found: Vec<String> =
                ide_diagnostics::validate_query_text(&config, Some(&configuration), &query)
                    .into_iter()
                    .map(|d| d.code.as_str().to_string())
                    .collect();

            assert!(
                !found.iter().any(|c| c == DiagnosticCode::AmbiguousFieldInQuery.as_str()),
                "[{shape}] a non-periodic register has no `Период` to be ambiguous about: \
                 {found:?}",
            );
        }
    }

    /// The control, over the same fixture: two sources that both really offer `Ссылка` must
    /// still be reported. Without it, silencing the rule outright would pass the test above.
    #[test]
    fn two_real_occurrences_are_still_reported() {
        let query = "ВЫБРАТЬ Ссылка КАК П \
                     ИЗ Справочник.Валюты КАК А \
                     ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Валюты КАК Б ПО ИСТИНА";

        assert!(
            codes_over_fixture(query)
                .iter()
                .any(|c| c == DiagnosticCode::AmbiguousFieldInQuery.as_str()),
            "a genuinely ambiguous name must still be reported: {:?}",
            codes_over_fixture(query),
        );
    }
}

/// A subquery is its own scope.
///
/// Resolution used to search every live frame at once, so a self-contained subquery collided
/// with its enclosing query over any shared field name — and in 1C the shared names are `Код`,
/// `Ссылка`, `Наименование`, which is to say almost all of them. The rule is the SQL one: a
/// name resolved on the current level shadows the same name outside, and only sources of one
/// level compete for it.
mod nested_scopes {
    use super::*;

    #[test]
    fn a_name_resolved_inside_a_subquery_does_not_collide_with_the_outer_query() {
        let query = "ВЫБРАТЬ Т.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Т \
                     ГДЕ Т.Ссылка В (ВЫБРАТЬ Код ИЗ Справочник.СправочникСМенеджером КАК В)";

        assert!(
            !is_ambiguous(query),
            "the inner `Код` names the subquery's only source: {:?}",
            codes(query),
        );
    }

    /// The control: two sources on ONE level still collide. Without it, a rule that never fired
    /// would pass the test above.
    #[test]
    fn two_sources_on_the_same_level_still_collide() {
        let query = "ВЫБРАТЬ Код КАК П ИЗ Справочник.Справочник1 КАК А \
                     ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Б ПО ИСТИНА";

        assert!(is_ambiguous(query), "same-level sources are ambiguous: {:?}", codes(query));
    }

    /// And the candidates named to the reader come from the level that owns the name — pointing
    /// at a source the reference could not have meant is worse than naming none.
    #[test]
    fn the_candidates_come_from_the_level_that_owns_the_name() {
        let config = DiagnosticsConfig::all_enabled();
        let configuration = bsl_metadata::load_from_directory(PathBuf::from(DESIGNER_FIXTURE))
            .expect("the designer fixture loads");
        let query = "ВЫБРАТЬ Код КАК П ИЗ Справочник.Справочник1 КАК А \
                     ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Б ПО ИСТИНА";

        let message = ide_diagnostics::validate_query_text(&config, Some(&configuration), query)
            .into_iter()
            .find(|d| d.code == DiagnosticCode::AmbiguousFieldInQuery)
            .expect("the ambiguity is reported")
            .message;

        assert!(
            message.contains('А') && message.contains('Б'),
            "both same-level sources must be named: {message}",
        );
    }
}

/// The head of a qualified reference that names both a source and a field.
///
/// The rule was established against 8.3.27 on a live base, not inferred: twelve shapes were
/// sent to the platform's validator and the analyzer now agrees with it on all twelve, six
/// accepted and six rejected. What the probe settled, and what the issue's wording got wrong:
///
/// * it is not about tabular sections — a plain String attribute collides identically, so the
///   field's type never enters into it;
/// * declaring such an alias is legal; only REFERENCING through it is not;
/// * one source is enough — an alias may collide with a field of its own table;
/// * levels do not mix: the same collision inside a subquery is accepted.
mod qualified_head {
    use super::*;

    fn message_for(query: &str) -> Option<String> {
        let config = DiagnosticsConfig::all_enabled();
        let configuration = bsl_metadata::load_from_directory(PathBuf::from(DESIGNER_FIXTURE))
            .expect("the designer fixture loads");

        ide_diagnostics::validate_query_text(&config, Some(&configuration), query)
            .into_iter()
            .find(|d| d.code == DiagnosticCode::AmbiguousFieldInQuery)
            .map(|d| d.message)
    }

    #[test]
    fn an_alias_colliding_with_a_field_is_reported_when_referenced_through() {
        let query = "ВЫБРАТЬ Т.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Т \
                     ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Справочник1.ТабличнаяЧасть1 \
                     КАК ТабличнаяЧасть1 ПО Т.Ссылка = ТабличнаяЧасть1.Ссылка";

        let message = message_for(query).expect("the platform rejects this query");
        assert!(
            message.contains("переименуйте источник"),
            "the fix is to rename the source, not to add a qualifier: {message}",
        );
    }

    #[test]
    fn one_source_is_enough_when_the_alias_collides_with_its_own_field() {
        let query = "ВЫБРАТЬ Реквизит1.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Реквизит1";

        let message = message_for(query).expect("the platform rejects this query too");
        assert!(
            message.contains("того же источника"),
            "the collision is with the source's own field and should say so: {message}",
        );
    }

    /// The negatives, each one a shape the platform accepts. Reporting any of them would be a
    /// false Blocker on code that works.
    #[test]
    fn the_shapes_the_platform_accepts_stay_quiet() {
        for (label, query) in [
            (
                "alias renamed",
                "ВЫБРАТЬ Т.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Т \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Справочник1.ТабличнаяЧасть1 КАК Строки \
                 ПО Т.Ссылка = Строки.Ссылка",
            ),
            (
                "alias declared but never referenced",
                "ВЫБРАТЬ Т.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Т \
                 ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Справочник1.ТабличнаяЧасть1 \
                 КАК ТабличнаяЧасть1 ПО Т.Ссылка = Т.Ссылка",
            ),
            (
                "alias matches no field of any source",
                "ВЫБРАТЬ Нечто.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Нечто",
            ),
        ] {
            assert!(
                message_for(query).is_none(),
                "[{label}] the platform accepts this — reporting it is a false Blocker",
            );
        }
    }

    /// Levels do not mix — asserted together with the control that lets it fail.
    ///
    /// `Реквизит3` is a field of `Справочник1` and of nothing else in the fixture, so inside a
    /// subquery over `СправочникСМенеджером` the name collides only if resolution reaches the
    /// enclosing frame. The same pair placed on ONE level must be reported: without that half,
    /// the negative passes on any rule that never fires, and the earlier version of this case
    /// used an alias matching no field at all — it could not have failed.
    #[test]
    fn a_subquery_alias_may_name_an_outer_field_though_the_same_pair_on_one_level_may_not() {
        let across_levels = "ВЫБРАТЬ Внеш.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Внеш \
                             ГДЕ Внеш.Ссылка В (ВЫБРАТЬ Реквизит3.Ссылка \
                             ИЗ Справочник.СправочникСМенеджером КАК Реквизит3)";
        let one_level = "ВЫБРАТЬ Реквизит3.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Внеш \
                         ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.СправочникСМенеджером КАК Реквизит3 \
                         ПО Внеш.Ссылка = Реквизит3.Ссылка";

        assert!(
            message_for(across_levels).is_none(),
            "the platform accepts a subquery alias that names a field of the outer query",
        );
        assert!(
            message_for(one_level).is_some(),
            "the same alias and field on one level is the collision the platform rejects",
        );
    }

    /// The query language is case-insensitive, so the reference and the `КАК ...` declaration
    /// may be spelled differently. An ASCII-only comparison folds no Cyrillic and reported the
    /// source's own field as a foreign source's — a wrong explanation of a correct verdict.
    #[test]
    fn a_self_collision_is_named_as_such_whatever_the_case_of_the_reference() {
        let query = "ВЫБРАТЬ реквизит1.Ссылка КАК П ИЗ Справочник.Справочник1 КАК Реквизит1";

        let message = message_for(query).expect("the platform rejects this query");
        assert!(
            message.contains("того же источника"),
            "the alias and the field belong to one source, however it is spelled: {message}",
        );
    }

    /// A collision with the source's own field does not hide the others: renaming the alias is
    /// the fix either way, but the reader is told what actually carries the name.
    #[test]
    fn the_message_names_the_other_sources_even_when_the_alias_collides_with_itself() {
        let query = "ВЫБРАТЬ Реквизит1.Ссылка КАК П \
                     ИЗ Справочник.Справочник1 КАК Реквизит1 \
                     ВНУТРЕННЕЕ СОЕДИНЕНИЕ Справочник.Справочник1.ТабличнаяЧасть1 КАК СтрокиТЧ \
                     ПО Реквизит1.Ссылка = СтрокиТЧ.Ссылка";

        let message = message_for(query).expect("the platform rejects this query");
        assert!(
            message.contains("того же источника") && message.contains("СтрокиТЧ"),
            "both the own field and the second source offering the name: {message}",
        );
    }
}

/// An output alias named in `ИТОГИ` is a column of the result, not a bare name to look up in
/// the sources.
///
/// Both `Ключ` and `Количество` are output aliases here, and `ВТ2` offers a `Количество` of its
/// own: the totals clause names the result column, so resolving it against the sources made
/// the alias collide with the columns it was derived from. No metadata is loaded, which is the
/// case the temporary tables of one batch are in.
///
/// Every route is asserted twice: the batch from the report stays quiet, and the same batch
/// with the bare name moved into the select list — where it really is offered by both tables —
/// is reported. The second half is what makes the first one able to fail: a route that
/// analysed nothing would otherwise stay quiet on both.
mod output_alias_in_totals {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use ide_diagnostics::{DiagnosticCode, DiagnosticsConfig};
    use vfs::{FileId, FileSet, VfsPath};

    const TEMPORARY_TABLES: &str = "ВЫБРАТЬ\n\
        \t1 КАК Ключ,\n\
        \t10 КАК Количество\n\
        ПОМЕСТИТЬ ВТ1\n\
        ;\n\
        ВЫБРАТЬ\n\
        \t1 КАК Ключ,\n\
        \t20 КАК Количество\n\
        ПОМЕСТИТЬ ВТ2\n\
        ;\n";

    const SOURCES: &str = "ИЗ\n\
        \tВТ1 КАК ВТ1\n\
        \t\tЛЕВОЕ СОЕДИНЕНИЕ ВТ2 КАК ВТ2\n\
        \t\tПО ВТ1.Ключ = ВТ2.Ключ";

    /// The query of the report: the alias is named only in the totals clause.
    fn reported_query() -> String {
        format!(
            "{TEMPORARY_TABLES}ВЫБРАТЬ\n\
             \tВТ1.Ключ КАК Ключ,\n\
             \tЕСТЬNULL(ВТ2.Количество, 0) КАК Количество\n\
             {SOURCES}\n\
             ИТОГИ\n\
             \tСУММА(Количество)\n\
             ПО\n\
             \tКлюч"
        )
    }

    /// The control: no totals, and the bare name stands in the select list, where both tables
    /// offer it.
    fn control_query() -> String {
        format!(
            "{TEMPORARY_TABLES}ВЫБРАТЬ\n\
             \tВТ1.Ключ КАК Ключ,\n\
             \tЕСТЬNULL(Количество, 0) КАК Количество\n\
             {SOURCES}"
        )
    }

    fn module_codes(query: &str) -> Vec<DiagnosticCode> {
        let body = query.replace('\n', "\n\t|");
        let source = format!(
            "Процедура ИтогиПоПсевдониму() Экспорт\n\
             \tЗапрос = Новый Запрос;\n\
             \tЗапрос.Текст =\n\
             \t\"{body}\";\n\
             \tЗапрос.Выполнить();\n\
             КонецПроцедуры\n"
        );
        let mut db = RootDatabaseImpl::new();
        let file_id = FileId(0);
        let mut files = FileSet::default();
        files.insert(file_id, VfsPath::new("/test/Module.bsl".to_string()));
        db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
        db.set_file_source_root(file_id, SourceRootId(0));
        db.set_file_text(file_id, &source);
        ide_diagnostics::file_diagnostics(&db, file_id, &DiagnosticsConfig::all_enabled())
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    fn bare_codes(query: &str) -> Vec<DiagnosticCode> {
        ide_diagnostics::validate_query_text(&DiagnosticsConfig::all_enabled(), None, query)
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    fn codes_over_empty_configuration(query: &str) -> Vec<DiagnosticCode> {
        let configuration = bsl_metadata::Configuration::new("Test");
        ide_diagnostics::validate_query_text(
            &DiagnosticsConfig::all_enabled(),
            Some(&configuration),
            query,
        )
        .into_iter()
        .map(|d| d.code)
        .collect()
    }

    fn assert_alias_quiet_and_control_reported(route: fn(&str) -> Vec<DiagnosticCode>) {
        let reported = route(&reported_query());
        assert!(
            !reported.contains(&DiagnosticCode::AmbiguousFieldInQuery),
            "the totals clause names result columns, not source columns: {reported:?}",
        );

        let control = route(&control_query());
        assert!(
            control.contains(&DiagnosticCode::AmbiguousFieldInQuery),
            "the same batch with the bare name in the select list is ambiguous, so this \
             route must be able to say so: {control:?}",
        );
    }

    #[test]
    fn an_output_alias_in_totals_is_not_ambiguous_in_a_module_literal() {
        assert_alias_quiet_and_control_reported(module_codes);
    }

    #[test]
    fn an_output_alias_in_totals_is_not_ambiguous_in_a_bare_query() {
        assert_alias_quiet_and_control_reported(bare_codes);
    }

    /// The same batch with a resolver present that holds none of its sources.
    #[test]
    fn an_output_alias_in_totals_is_not_ambiguous_over_an_empty_configuration() {
        assert_alias_quiet_and_control_reported(codes_over_empty_configuration);
    }
}
