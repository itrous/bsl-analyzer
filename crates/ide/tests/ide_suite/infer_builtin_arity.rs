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
fn nstr_two_args_is_accepted() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Сообщить(НСтр("ru = 'Привет'", "ru"));
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "NStr('ru = ...', 'ru') is two valid args (required=1, total=2), \
         no MismatchedArgCount expected, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn nstr_one_arg_is_accepted() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Сообщить(НСтр("ru = 'Привет'"));
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "NStr('ru = ...') with one arg satisfies required=1, no diag expected, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn nstr_zero_args_fires_mismatch() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Сообщить(НСтр());
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        arg_count_diags(&db, file_id),
        vec![(1, 2, 0)],
        "NStr() must emit (required=1, total=2, found=0) — required floor is preserved"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn nstr_three_args_fires_mismatch() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Сообщить(НСтр("ru = '...'", "ru", "extra"));
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        arg_count_diags(&db, file_id),
        vec![(1, 2, 3)],
        "NStr(arg, arg, arg) must emit (required=1, total=2, found=3) — \
         non-variadic upper bound is preserved"
    );
}

#[test]
fn strtemplate_variadic_accepts_many_args() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Сообщить(СтрШаблон("без подстановок"));
    Сообщить(СтрШаблон("%1 + %2 + %3 = %4", 1, 2, 3, 6));
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "СтрШаблон is variadic, none of the call sites should fire \
         MismatchedArgCount, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn strtemplate_zero_args_fires_mismatch() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Сообщить(СтрШаблон());
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        arg_count_diags(&db, file_id),
        vec![(1, 2, 0)],
        "СтрШаблон() with no args must emit (required=1, total=2, found=0) — \
         the variadic flag does NOT relax the lower bound"
    );
}

#[test]
fn currentdate_no_args_is_accepted() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = ТекущаяДата();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "ТекущаяДата() with zero args is correct, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
fn min_max_accept_many_args() {
    let fixture = r#"
//- /test.bsl
Процедура Тест(Мин, Макс)
    Х = Мин(Макс(1, 2), 3, 4, 5);
    У = Макс(10, 20, 30, 40, 50);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "Мин/Макс are unbounded variadic, multi-arg calls must NOT fire \
         MismatchedArgCount, got {:?}",
        arg_count_diags(&db, file_id)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn min_max_zero_args_fires_mismatch() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    Х = Мин();
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let diags = arg_count_diags(&db, file_id);
    assert_eq!(
        diags.len(),
        1,
        "Мин() with zero args must fire exactly one MismatchedArgCount, got {diags:?}"
    );
    assert_eq!(diags[0].2, 0, "found = 0");
    assert!(diags[0].0 >= 1, "required >= 1 for the leading param");
}

#[test]
fn user_repro_min_max_okr_intervals() {
    let fixture = r#"
//- /test.bsl
Процедура ВыбратьЧисло(Мин, Макс)
    Случ = 0.5;
    ЧислоИзИнтервала = Мин(Макс(Окр(Мин + (Макс - Мин) * Случ, 0, 1), Мин), Макс);
    Сообщить(ЧислоИзИнтервала);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        arg_count_diags(&db, file_id).is_empty(),
        "user repro line must produce zero MismatchedArgCount diagnostics, got {:?}",
        arg_count_diags(&db, file_id)
    );
}
