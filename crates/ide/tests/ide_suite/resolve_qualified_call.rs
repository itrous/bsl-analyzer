use hir::{HirDatabase, InferenceDiagnostic, UnresolvedMethodKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

fn setup_fixture(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    let test_file = *fixture.files.keys().last().expect("fixture must have at least one file");
    (db, test_file)
}

fn unresolved_kinds(db: &RootDatabaseImpl, file_id: FileId) -> Vec<UnresolvedMethodKind> {
    let infer = db.infer(file_id);
    infer
        .diagnostics
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::UnresolvedMethodCall { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

fn mismatched_arg_counts(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(usize, usize, usize)> {
    db.arg_diagnostics(file_id)
        .iter()
        .filter_map(|(_, diag)| match diag {
            InferenceDiagnostic::MismatchedArgCount {
                required_count, total_count, found, ..
            } => Some((*required_count, *total_count, *found)),
            _ => None,
        })
        .collect()
}

const EXPORTED_METHOD_FIXTURE: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ЗначениеРеквизитаОбъекта() Экспорт
    Возврат Истина;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщегоНазначения.ЗначениеРеквизитаОбъекта();
КонецПроцедуры
"#;

#[test]
fn exported_method_resolves_without_diagnostic() {
    let (db, file_id) = setup_fixture(EXPORTED_METHOD_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert!(kinds.is_empty(), "Expected no UnresolvedMethodCall diagnostics, got: {:?}", kinds);
}

const POSITIVE_RESOLUTION_FIXTURE: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ЗначениеРеквизитаОбъекта(Объект, ИмяРеквизита) Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщегоНазначения.ЗначениеРеквизитаОбъекта();
КонецПроцедуры
"#;

#[test]
fn successful_resolution_triggers_arg_count_check() {
    let (db, file_id) = setup_fixture(POSITIVE_RESOLUTION_FIXTURE);
    let unresolved = unresolved_kinds(&db, file_id);
    assert!(unresolved.is_empty(), "Resolution must succeed, got unresolved: {:?}", unresolved);

    let mismatches = mismatched_arg_counts(&db, file_id);
    assert_eq!(
        mismatches,
        vec![(2, 2, 0)],
        "Expected MismatchedArgCount(required=2, total=2, found=0) as positive proof of resolution"
    );
}

const NON_EXPORTED_METHOD_FIXTURE: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция СкрытыйМетод()
    Возврат Ложь;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщегоНазначения.СкрытыйМетод();
КонецПроцедуры
"#;

#[test]
fn non_exported_method_reports_method_not_export() {
    let (db, file_id) = setup_fixture(NON_EXPORTED_METHOD_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::MethodNotExport],
        "Expected MethodNotExport for non-exported method"
    );
}

const MISSING_MODULE_FIXTURE: &str = r#"
//- /test.bsl
Процедура Тест()
    Результат = НесуществующийМодуль.Метод();
КонецПроцедуры
"#;

#[test]
fn missing_module_reports_receiver_not_resolved() {
    let (db, file_id) = setup_fixture(MISSING_MODULE_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::ReceiverNotResolved],
        "Expected ReceiverNotResolved when module_index has no entry"
    );
}

const MISSING_METHOD_FIXTURE: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Процедура СуществующийМетод() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Результат = ОбщегоНазначения.НесуществующийМетод();
КонецПроцедуры
"#;

#[test]
fn missing_method_reports_method_not_found() {
    let (db, file_id) = setup_fixture(MISSING_METHOD_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::MethodNotFound],
        "Expected MethodNotFound when module exists but method does not"
    );
}

const SHADOWING_FIXTURE: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ЗначениеРеквизитаОбъекта() Экспорт
    Возврат Истина;
КонецФункции

//- /test.bsl
Процедура Тест()
    Перем ОбщегоНазначения;
    ОбщегоНазначения = Новый Массив;
    ОбщегоНазначения.Добавить(1);
КонецПроцедуры
"#;

#[test]
fn local_shadowing_skips_qualified_resolution() {
    let (db, file_id) = setup_fixture(SHADOWING_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "Shadowed receiver must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}

const FOR_EACH_PROPERTY_CHAIN_FIXTURE: &str = r#"
//- /test.bsl
Процедура Тест(Коллекция)
    Для Каждого ТекЭлемент Из Коллекция Цикл
        Значение = ТекЭлемент.СохраненныеНастройки.Получить();
    КонецЦикла;
КонецПроцедуры
"#;

#[test]
fn for_each_iterator_property_chain_does_not_emit_unresolved() {
    let (db, file_id) = setup_fixture(FOR_EACH_PROPERTY_CHAIN_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "ForEach iterator chain must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}

const FOR_ITERATOR_PROPERTY_CHAIN_FIXTURE: &str = r#"
//- /test.bsl
Процедура Тест(Коллекция)
    Для Сч = 1 По 10 Цикл
        Значение = Сч.Свойство.Метод();
    КонецЦикла;
КонецПроцедуры
"#;

#[test]
fn for_iterator_property_chain_does_not_emit_unresolved() {
    let (db, file_id) = setup_fixture(FOR_ITERATOR_PROPERTY_CHAIN_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "For iterator chain must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}

const IMPLICIT_LOCAL_UNKNOWN_RHS_FIXTURE: &str = r#"
//- /test.bsl
Процедура Тест()
    Х = НеизвестнаяФункция();
    Х.Метод();
КонецПроцедуры
"#;

#[test]
fn implicit_local_with_unknown_rhs_is_not_misclassified_as_common_module() {
    let (db, file_id) = setup_fixture(IMPLICIT_LOCAL_UNKNOWN_RHS_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "implicit local must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}

const NON_MDO_LEADING_IDENT_FIXTURE: &str = r#"
//- /test.bsl
Процедура Тест()
    Значение = НеобъявленнаяПеременная.Подсвойство.Метод();
КонецПроцедуры
"#;

#[test]
fn non_mdo_leading_ident_does_not_emit_unresolved() {
    let (db, file_id) = setup_fixture(NON_MDO_LEADING_IDENT_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert!(
        kinds.is_empty(),
        "Non-MDO leading IDENT must not produce UnresolvedMethodCall, got: {:?}",
        kinds
    );
}

const CASE_INSENSITIVE_FIXTURE: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ЗначениеРеквизитаОбъекта(Параметр) Экспорт
    Возврат Истина;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = общегоназначения.ЗначениеРеквизитаОбъекта();
КонецПроцедуры
"#;

#[test]
fn case_insensitive_module_name_resolves() {
    let (db, file_id) = setup_fixture(CASE_INSENSITIVE_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert!(kinds.is_empty(), "Expected case-insensitive module lookup, got: {:?}", kinds);
    let mismatches = mismatched_arg_counts(&db, file_id);
    assert_eq!(
        mismatches,
        vec![(1, 1, 0)],
        "Case-insensitive lookup must reach arg-count check; got: {:?}",
        mismatches
    );
}

const RUSSIAN_LAYOUT_FIXTURE: &str = r#"
//- /ОбщиеМодули/ОбщегоНазначения/Ext/Module.bsl
Функция ЗначениеРеквизитаОбъекта(Параметр) Экспорт
    Возврат Истина;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = ОбщегоНазначения.ЗначениеРеквизитаОбъекта();
КонецПроцедуры
"#;

#[test]
fn russian_layout_resolves() {
    let (db, file_id) = setup_fixture(RUSSIAN_LAYOUT_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert!(kinds.is_empty(), "Russian-layout CommonModule must resolve, got: {:?}", kinds);
    let mismatches = mismatched_arg_counts(&db, file_id);
    assert_eq!(
        mismatches,
        vec![(1, 1, 0)],
        "Russian-layout lookup must reach arg-count check; got: {:?}",
        mismatches
    );
}

const USER_MODULE_SHADOWS_PLATFORM_GLOBAL_FIXTURE: &str = r#"
//- /CommonModules/Метаданные/Ext/Module.bsl
Процедура ОдинЭкспортируемыйМетод() Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    Метаданные.ЗаведомоОтсутствующийМетод();
КонецПроцедуры
"#;

#[test]
fn user_common_module_shadows_same_named_platform_global() {
    let (db, file_id) = setup_fixture(USER_MODULE_SHADOWS_PLATFORM_GLOBAL_FIXTURE);
    let kinds = unresolved_kinds(&db, file_id);
    assert_eq!(
        kinds,
        vec![UnresolvedMethodKind::MethodNotFound],
        "user CommonModule named like a platform global must keep its missing-method \
         diagnostic; if this fails, `infer_path_name` step 5 retyped the receiver as \
         PlatformObject before the cascade gate could route it through the workspace. \
         got: {:?}",
        kinds
    );
}
