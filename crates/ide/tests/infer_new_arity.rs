use hir::{HirDatabase, InferenceDiagnostic};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn arg_count_diags(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(usize, usize, usize)> {
    db.arg_diagnostics(file_id)
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::MismatchedArgCount {
                required_count, total_count, found, ..
            } => Some((*required_count, *total_count, *found)),
            _ => None,
        })
        .collect()
}

#[test]
fn array_variadic_accepts_many_args() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = Новый Массив(10, 20, 30, 40, 50);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "Новый Массив with variadic count list must accept any arity, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn array_one_arg_accepts() {
    let fixture = r#"
//- /test.bsl
Процедура Тест(ИсхМассив)
    Х = Новый Массив(ИсхМассив);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "Новый Массив(x) must accept (1-arg fixed overload), got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn array_no_parens_accepts() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = Новый Массив;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "Новый Массив (no parens) must accept (zero-arity overload), got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn structure_keys_and_values_accepts_variadic() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Отбор = Новый Структура("Дата, Клиент", ТекущаяДата(), "Иванов");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "Новый Структура('К1, К2', З1, З2) must accept via PR3 overlay, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn structure_no_parens_accepts() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = Новый Структура;
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "Новый Структура (no parens) must accept, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn fixed_structure_keys_and_values_accepts_variadic() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = Новый ФиксированнаяСтруктура("К1, К2", "Знач1", "Знач2");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "ФиксированнаяСтруктура with keys+values must accept via PR3 overlay, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn dynamic_list_row_key_accepts_variadic() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Ключ = Новый КлючСтрокиДинамическогоСписка("Поле1, Поле2", 1, "два");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "КлючСтрокиДинамическогоСписка with paths+values must accept, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn formatted_string_strings_accepts_variadic() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Стр = Новый ФорматированнаяСтрока("a", "b", "c", "d");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "ФорматированнаяСтрока with multiple strings must accept via PR3 Step 1, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn fixed_array_required_floor_fires() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = Новый ФиксированныйМассив();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let diags = arg_count_diags(&db, file_id);
    assert_eq!(diags.len(), 1, "expected exactly one diagnostic, got {diags:?}");
    assert_eq!(diags[0].2, 0, "found = 0");
    assert_eq!(diags[0].0, 1, "required = 1 (Массив is mandatory)");
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn query_upper_bound_fires() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = Новый Запрос("текст", "extra1", "extra2");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let diags = arg_count_diags(&db, file_id);
    assert_eq!(diags.len(), 1, "expected exactly one diagnostic, got {diags:?}");
    assert_eq!(diags[0].2, 3, "found = 3");
    assert_eq!(diags[0].1, 1, "total = 1 (single param)");
}

#[test]
fn unresolved_type_does_not_fire_arity() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = Новый НесуществующийТип(1, 2, 3, "anything");
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "unknown ctor type must NOT fire MismatchedArgCount (skipped on empty ctor list), got {:?}",
        arg_count_diags(&db, file_id)
    );
}

/// У динамической формы список аргументов — `(<Тип>, <МассивПараметров>)`, а не
/// позиционные аргументы конструктора: сверять их с перегрузками нельзя, иначе
/// само имя типа будет разобрано как первый параметр.
#[test]
fn dynamic_ctor_name_does_not_fire_arity() {
    let fixture = r#"
//- /test.bsl
Процедура Тест(Параметры)
    А = Новый("Массив", Параметры);
    Б = Новый(Тип("Массив"), Параметры);
    В = Новый(Тип("ФиксированныйМассив"));
    Г = Новый("Структура", Параметры);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "dynamic `Новый(<Тип>, <Параметры>)` must not be matched against constructor overloads, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn user_repro_structure_filter() {
    let fixture = r#"
//- /test.bsl
Процедура ПрименитьОтбор(Дата, Клиент, Сумма)
    Отбор = Новый Структура("Дата, Клиент, Сумма", Дата, Клиент, Сумма);
    Сообщить(Отбор);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "user-style 4-arg Структура call must produce zero diagnostics, got {:?}",
        arg_count_diags(&db, file_id)
    );
}
