//! A receiver whose type nothing established must not be reported as missing a
//! method — whether the branch guard rules the type out, or the type was a
//! wildcard from the start.
//!
//! A function whose result inference could not type ends up carrying a confident
//! `Неопределено` — the lossy path (a dynamically concatenated query text) drops
//! the real arm instead of recording that it is unknown. At a use inside the
//! `Иначе` branch that value is proven not to be `Неопределено`, so the only
//! definition reaching the call contradicts the guard and nothing about the
//! receiver is provable: reporting "method not found" there is a false positive.
//!
//! The tests pin both polarities and both operand orders, and keep the
//! diagnostic alive where no guard proves anything.

use hir::{HirDatabase, InferenceDiagnostic, UnresolvedMethodKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::{FileId, FileSet};

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn has_platform_data() -> bool {
    !bsl_platform::PlatformDataInner::instance().all_methods().is_empty()
}

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn unresolved(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(String, UnresolvedMethodKind)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::UnresolvedMethodCall {
                method_name, receiver_name, kind, ..
            } => Some((format!("{} у {}", method_name.as_str(), receiver_name.as_str()), *kind)),
            _ => None,
        })
        .collect()
}

/// The minimal repro from the issue: the query text is glued together at run
/// time, so SDBL cannot type the selection and `НайтиСсылку` materialises as a
/// confident `Неопределено`.
const DYNAMIC_QUERY_FINDER: &str = r#"
Функция НайтиСсылку(Имя)
    Результат = Неопределено;
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ ИскСправочник.Ссылка КАК Ссылка ИЗ Справочник." + Имя + " КАК ИскСправочник";
    Выборка = Запрос.Выполнить().Выбрать();
    Если Выборка.Следующий() Тогда
        Результат = Выборка.Ссылка;
    КонецЕсли;
    Возврат Результат;
КонецФункции
"#;

fn fixture_with_finder(test_proc: &str) -> String {
    format!("\n//- /test.bsl\n{DYNAMIC_QUERY_FINDER}\n{test_proc}")
}

#[test]
fn else_branch_of_is_undefined_guard_not_flagged() {
    if !has_platform_data() {
        return;
    }
    let fixture = fixture_with_finder(
        r#"
Процедура Тест()
    Счет = Неопределено;
    Счет = НайтиСсылку("Справочник1");
    Если Счет = Неопределено Тогда
        Счет = Справочники.Справочник1.СоздатьЭлемент();
    Иначе
        Счет = Счет.ПолучитьОбъект();
    КонецЕсли;
КонецПроцедуры
"#,
    );
    let (db, file_id) = setup(&fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "guarded Иначе branch FP: {kinds:?}");
}

/// The `<>` polarity, on the shape where it is observable: a receiver typed
/// `Неопределено` has no name to report, so the false positive needs an earlier
/// assignment that sequential inference mistakes for the receiver's type. That
/// assignment sits in the sibling branch, which no path takes to the call.
#[test]
fn then_branch_of_is_not_undefined_guard_not_flagged() {
    if !has_platform_data() {
        return;
    }
    let fixture = fixture_with_finder(
        r#"
Процедура Тест(Флаг)
    Счет = НайтиСсылку("Справочник1");
    Если Флаг Тогда
        Счет = Справочники.Справочник1.СоздатьЭлемент();
    Иначе
        Если Счет <> Неопределено Тогда
            Счет = Счет.ПолучитьОбъект();
        КонецЕсли;
    КонецЕсли;
КонецПроцедуры
"#,
    );
    let (db, file_id) = setup(&fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "guarded Тогда branch FP: {kinds:?}");
}

#[test]
fn undefined_as_left_operand_narrows_the_same_way() {
    if !has_platform_data() {
        return;
    }
    let fixture = fixture_with_finder(
        r#"
Процедура Тест()
    Счет = Неопределено;
    Счет = НайтиСсылку("Справочник1");
    Если Неопределено = Счет Тогда
        Счет = Справочники.Справочник1.СоздатьЭлемент();
    Иначе
        Счет = Счет.ПолучитьОбъект();
    КонецЕсли;
КонецПроцедуры
"#,
    );
    let (db, file_id) = setup(&fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "reversed operands FP: {kinds:?}");
}

#[test]
fn elsif_branch_inherits_the_negated_first_condition() {
    if !has_platform_data() {
        return;
    }
    let fixture = fixture_with_finder(
        r#"
Процедура Тест()
    Счет = Неопределено;
    Счет = НайтиСсылку("Справочник1");
    Если Счет = Неопределено Тогда
        Счет = Справочники.Справочник1.СоздатьЭлемент();
    ИначеЕсли Истина Тогда
        Счет = Счет.ПолучитьОбъект();
    КонецЕсли;
КонецПроцедуры
"#,
    );
    let (db, file_id) = setup(&fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "ИначеЕсли branch FP: {kinds:?}");
}

/// Anti-FN: the guard proves the value is not `Неопределено`, and the reaching
/// definition is a ref that survives that proof — the missing method is real.
#[test]
fn guard_does_not_swallow_a_real_miss_on_a_surviving_type() {
    if !has_platform_data() {
        return;
    }
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Об = Справочники.Справочник1.СоздатьЭлемент();
    Об = Справочники.Справочник1.НайтиПоКоду("01");
    Если Об = Неопределено Тогда
        Об = Справочники.Справочник1.СоздатьЭлемент();
    Иначе
        Об.НесуществующийМетод();
    КонецЕсли;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds = unresolved(&db, file_id);
    assert_eq!(kinds.len(), 1, "real miss must stay flagged, got {kinds:?}");
}

/// Anti-FN: a guard on another variable proves nothing about the receiver.
#[test]
fn guard_on_another_variable_keeps_the_diagnostic() {
    if !has_platform_data() {
        return;
    }
    let fixture = fixture_with_finder(
        r#"
Процедура Тест()
    Флаг = Неопределено;
    Об = Справочники.Справочник1.СоздатьЭлемент();
    Об = Справочники.Справочник1.НайтиПоКоду("01");
    Если Флаг = Неопределено Тогда
        Об = Справочники.Справочник1.СоздатьЭлемент();
    Иначе
        Об.НесуществующийМетод();
    КонецЕсли;
КонецПроцедуры
"#,
    );
    let (db, file_id) = setup(&fixture);
    let kinds = unresolved(&db, file_id);
    assert_eq!(kinds.len(), 1, "unrelated guard must not suppress, got {kinds:?}");
}

/// The shape reported on ERP: the finder documents its result as a bare
/// `СправочникСсылка`, which lowering cannot tie to a catalog and turns into the
/// wildcard `Произвольный`. That wildcard is the only definition reaching the
/// `Иначе` branch, and a wildcard cannot witness that `ПолучитьОбъект` is absent.
#[test]
fn documented_wildcard_ref_is_not_a_provable_receiver() {
    if !has_platform_data() {
        return;
    }
    let fixture = r#"
//- /test.bsl
// Возвращаемое значение:
//  - СправочникСсылка - найденное значение
//
Функция НайтиСсылкуНаОбъектПоРеквизиту(ИмяСправочника, ИмяРеквизита, ЗначРеквизита) Экспорт
    Результат = Неопределено;
    Запрос = Новый Запрос;
    Запрос.Текст = "ВЫБРАТЬ ИскСправочник.Ссылка КАК Ссылка ИЗ Справочник." + ИмяСправочника + " КАК ИскСправочник";
    Выборка = Запрос.Выполнить().Выбрать();
    Если Выборка.Следующий() Тогда
        Результат = Выборка.Ссылка;
    КонецЕсли;
    Возврат Результат;
КонецФункции

Процедура Тест(МассивСчетов, КонтрагентСсылка) Экспорт
    Для Каждого ЭлементМассива Из МассивСчетов Цикл
        НомерСчета = "1";
        Если Не ЗначениеЗаполнено(НомерСчета) Тогда
            Продолжить;
        КонецЕсли;
        БанковскийСчет = Неопределено;
        БанковскийСчет = НайтиСсылкуНаОбъектПоРеквизиту("Справочник1", "НомерСчета", НомерСчета);
        Если БанковскийСчет = Неопределено Тогда
            БанковскийСчет = Справочники.Справочник1.СоздатьЭлемент();
            БанковскийСчет.Владелец = КонтрагентСсылка;
        Иначе
            БанковскийСчет = БанковскийСчет.ПолучитьОбъект();
            БанковскийСчет.Владелец = КонтрагентСсылка;
        КонецЕсли;
    КонецЦикла;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let kinds = unresolved(&db, file_id);
    assert!(kinds.is_empty(), "documented wildcard receiver FP: {kinds:?}");
}

/// A wildcard definition vouches for a call only while it still reaches it.
///
/// Reaching definitions kill the overwritten one, so a wildcard assigned and
/// then replaced by a concrete type says nothing here: the miss on the live
/// definition stays reported.
#[test]
fn overwritten_wildcard_definition_does_not_swallow_a_real_miss() {
    if !has_platform_data() {
        return;
    }
    let (db, file_id) = setup(WILDCARD_THEN_CONCRETE);
    assert_eq!(
        unresolved(&db, file_id).into_iter().map(|(what, _)| what).collect::<Vec<_>>(),
        vec!["НесуществующийМетод у Справочники.Справочник1".to_string()],
    );
}

/// The control for the test above: the same two assignments the other way round,
/// so the wildcard is the definition that reaches the call and nothing is
/// reported. Without this pair the test above would pass on an input that never
/// reaches the wildcard at all. What silences this one is that the receiver type
/// is itself the wildcard, which has no name to report — not the vouching rule;
/// the rule is loaded by the two join tests below.
#[test]
fn live_wildcard_definition_leaves_the_call_unprovable() {
    if !has_platform_data() {
        return;
    }
    let (db, file_id) = setup(CONCRETE_THEN_WILDCARD);
    assert_eq!(unresolved(&db, file_id), vec![]);
}

const WILDCARD_THEN_CONCRETE: &str = r#"
//- /test.bsl
// Возвращаемое значение:
//  - СправочникСсылка - найденное значение
//
Функция НайтиСсылкуПоРеквизиту(ИмяСправочника, ИмяРеквизита, ЗначРеквизита) Экспорт
    Результат = Неопределено;
    Возврат Результат;
КонецФункции

Процедура Тест()
    Счет = НайтиСсылкуПоРеквизиту("Справочник1", "НомерСчета", "1");
    Счет = Справочники.Справочник1.НайтиПоКоду("01");
    Счет.НесуществующийМетод();
КонецПроцедуры
"#;

const CONCRETE_THEN_WILDCARD: &str = r#"
//- /test.bsl
// Возвращаемое значение:
//  - СправочникСсылка - найденное значение
//
Функция НайтиСсылкуПоРеквизиту(ИмяСправочника, ИмяРеквизита, ЗначРеквизита) Экспорт
    Результат = Неопределено;
    Возврат Результат;
КонецФункции

Процедура Тест()
    Счет = Справочники.Справочник1.НайтиПоКоду("01");
    Счет = НайтиСсылкуПоРеквизиту("Справочник1", "НомерСчета", "1");
    Счет.НесуществующийМетод();
КонецПроцедуры
"#;

/// A wildcard reaching the call alongside a concrete type does not hide the miss
/// the concrete type proves.
///
/// The `Иначе` path really does carry `Справочники.Справочник1`, and the method
/// really is absent there. A wildcard on the sibling path says nothing about
/// members, but it says nothing about that path either.
#[test]
fn a_wildcard_at_a_join_does_not_hide_the_miss_on_the_other_path() {
    if !has_platform_data() {
        return;
    }
    let (db, file_id) = setup(WILDCARD_JOINS_CONCRETE);
    assert_eq!(
        unresolved(&db, file_id).into_iter().map(|(what, _)| what).collect::<Vec<_>>(),
        vec!["НесуществующийМетод у Справочники.Справочник1".to_string()],
    );
}

/// The control for the test above: a join where the only definition reaching the
/// call is the wildcard, and the receiver type is the sibling branch's leftover.
/// Nothing proves a miss here, so nothing is reported. Without this pair the test
/// above would pass on a build that never suppresses anything.
#[test]
fn a_wildcard_reaching_alone_leaves_the_sibling_type_unprovable() {
    if !has_platform_data() {
        return;
    }
    let (db, file_id) = setup(WILDCARD_REACHES_ALONE);
    assert_eq!(unresolved(&db, file_id), vec![]);
}

const WILDCARD_JOINS_CONCRETE: &str = r#"
//- /test.bsl
// Возвращаемое значение:
//  - СправочникСсылка
Функция ПолучитьЛюбую()
    Возврат Неопределено;
КонецФункции

Процедура Тест(Флаг)
    Если Флаг Тогда
        С = ПолучитьЛюбую();
    Иначе
        С = Справочники.Справочник1.НайтиПоКоду("01");
    КонецЕсли;
    С.НесуществующийМетод();
КонецПроцедуры
"#;

const WILDCARD_REACHES_ALONE: &str = r#"
//- /test.bsl
// Возвращаемое значение:
//  - СправочникСсылка
Функция ПолучитьЛюбую()
    Возврат Неопределено;
КонецФункции

Процедура Тест(Флаг)
    С = ПолучитьЛюбую();
    Если Флаг Тогда
        С = Справочники.Справочник1.СоздатьЭлемент();
        С.Владелец = Неопределено;
    Иначе
        С.НесуществующийМетод();
    КонецЕсли;
КонецПроцедуры
"#;

/// A parameter reaching the guarded branch establishes nothing to survive with.
///
/// `Если Счет = Неопределено Тогда Счет = … Иначе Счет = Счет.ПолучитьОбъект()`
/// is the ordinary "create it or load it" shape. In the `Иначе` branch the only
/// definition reaching the call is the parameter, which carries no declared type
/// in BSL — so the receiver type there is the sibling branch's leftover, and the
/// miss is not provable.
#[test]
fn a_parameter_reaching_the_guarded_branch_is_not_a_provable_receiver() {
    if !has_platform_data() {
        return;
    }
    let (db, file_id) = setup(PARAMETER_UNDER_ITS_OWN_GUARD);
    assert_eq!(unresolved(&db, file_id), vec![]);
}

/// The control for the test above: the same shape with the condition testing
/// another variable, so nothing rules the parameter path out and the diagnostic
/// stays. Without this pair the test above would pass on a build that silences
/// every call whose receiver is a parameter.
#[test]
fn a_parameter_under_an_unrelated_condition_keeps_the_diagnostic() {
    if !has_platform_data() {
        return;
    }
    assert_eq!(
        unresolved_names(PARAMETER_UNDER_ANOTHER_GUARD),
        vec!["НесуществующийМетод у Справочники.Справочник1".to_string()],
    );
}

/// A branch holding a label proves nothing about the statements after it.
///
/// `Перейти` draws a CFG edge straight to the label, so the call is reached on a
/// path where the condition was never evaluated — and on that path the value the
/// guard would have excluded is exactly the one that arrives.
#[test]
fn a_label_in_the_guarded_branch_proves_nothing() {
    if !has_platform_data() {
        return;
    }
    assert_eq!(
        unresolved_names(GUARDED_BRANCH_WITH_A_LABEL),
        vec!["НесуществующийМетод у Справочники.Справочник1".to_string()],
    );
}

/// The control for the test above: the same body with the jump and the label
/// removed. Now the branch really does dominate its statements, the guard holds,
/// and nothing is reported. Without this pair the test above would pass on a
/// build whose branch guards never fire at all.
#[test]
fn the_same_branch_without_a_label_is_proven_by_its_condition() {
    if !has_platform_data() {
        return;
    }
    let (db, file_id) = setup(GUARDED_BRANCH_WITHOUT_A_LABEL);
    assert_eq!(unresolved(&db, file_id), vec![]);
}

fn unresolved_names(fixture: &str) -> Vec<String> {
    let (db, file_id) = setup(fixture);
    unresolved(&db, file_id).into_iter().map(|(what, _)| what).collect()
}

const PARAMETER_UNDER_ITS_OWN_GUARD: &str = r#"
//- /test.bsl
Процедура Тест(Счет) Экспорт
    Если Счет = Неопределено Тогда
        Счет = Справочники.Справочник1.СоздатьЭлемент();
    Иначе
        Счет = Счет.ПолучитьОбъект();
    КонецЕсли;
КонецПроцедуры
"#;

const PARAMETER_UNDER_ANOTHER_GUARD: &str = r#"
//- /test.bsl
Процедура Тест(Счет, Флаг) Экспорт
    Если Флаг = Неопределено Тогда
        Счет = Справочники.Справочник1.НайтиПоКоду("01");
    Иначе
        Счет.НесуществующийМетод();
    КонецЕсли;
КонецПроцедуры
"#;

const GUARDED_BRANCH_WITH_A_LABEL: &str = r#"
//- /test.bsl
Процедура Тест(Флаг) Экспорт
    Х = Неопределено;
    Если Флаг Тогда
        Перейти ~Метка;
    КонецЕсли;
    Если Х = Неопределено Тогда
        Х = Справочники.Справочник1.НайтиПоКоду("01");
    Иначе
        ~Метка:
        Х.НесуществующийМетод();
    КонецЕсли;
КонецПроцедуры
"#;

const GUARDED_BRANCH_WITHOUT_A_LABEL: &str = r#"
//- /test.bsl
Процедура Тест(Флаг) Экспорт
    Х = Неопределено;
    Если Флаг Тогда
        Х = Неопределено;
    КонецЕсли;
    Если Х = Неопределено Тогда
        Х = Справочники.Справочник1.НайтиПоКоду("01");
    Иначе
        Х.НесуществующийМетод();
    КонецЕсли;
КонецПроцедуры
"#;
