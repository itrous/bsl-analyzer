use hir::{execution_env::EnvFlags, Builders, DefDatabase, HirDatabase, ModuleId};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    setup_for(fixture_text, "/test.bsl")
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn constructor_call_attaches_candidate_resolution() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    Массив = Новый Массив();
КонецПроцедуры
"#,
    );

    let inference = db.infer(file_id);
    let binding = inference
        .call_arg_bindings
        .iter()
        .find(|binding| binding.candidate.candidates.as_slice().iter().all(|s| s.id.is_platform()))
        .expect("constructor call must attach candidate semantics");
    assert!(binding.candidate.resolution.unique_candidate().is_some());
}

fn setup_for(fixture_text: &str, target_path: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();
    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(file_set));
    for (file_id, file) in &fixture.files {
        db.set_file_source_root(*file_id, SourceRootId(0));
        db.set_file_text(*file_id, &file.content);
    }
    let test_file = fixture
        .files
        .iter()
        .find(|(_, file)| file.path.as_path().to_string_lossy().ends_with(target_path))
        .map(|(file_id, _)| *file_id)
        .unwrap_or_else(|| panic!("fixture must contain {target_path}"));
    let _ = db.module_bodies(ModuleId::new(test_file));
    (db, test_file)
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn builtin_call_attaches_candidate_resolution() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    Результат = СтрДлина("строка");
КонецПроцедуры
"#,
    );

    let inference = db.infer(file_id);
    let binding = inference
        .call_arg_bindings
        .iter()
        .find(|binding| {
            binding
                .candidate
                .candidates
                .as_slice()
                .iter()
                .all(|signature| signature.id.is_builtin())
        })
        .expect("builtin call must attach candidate semantics");
    let candidate = &binding.candidate;
    assert!(candidate.candidates.as_slice().iter().all(|signature| signature.id.is_builtin()));
    assert!(candidate.resolution.unique_candidate().is_some());
    assert_eq!(candidate.resolution.return_ty, db.number(None, None));
}

#[test]
fn user_method_call_attaches_candidate_resolution() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
// Параметры:
//   Значение - Число - входное значение
// Возвращаемое значение:
//   Строка - результат
Функция Преобразовать(Значение)
    Возврат "готово";
КонецФункции

Процедура Тест()
    Результат = Преобразовать(1);
КонецПроцедуры
"#,
    );

    let inference = db.infer(file_id);
    let binding = inference
        .call_arg_bindings
        .iter()
        .find(|binding| {
            binding.candidate.candidates.as_slice().iter().all(|signature| signature.id.is_user())
        })
        .expect("user method call must attach candidate semantics");
    let candidate = &binding.candidate;
    assert_eq!(candidate.candidates.as_slice().len(), 1);
    assert!(candidate.candidates.as_slice()[0].id.is_user());
    assert!(candidate.resolution.unique_candidate().is_some());
    assert_eq!(candidate.resolution.return_ty, db.string(None, false));
}

#[test]
fn user_method_candidate_preserves_effective_environment() {
    let (db, file_id) = setup(
        r#"
//- /Documents/ПКО/Ext/ManagerModule.bsl
#Если Сервер Тогда
&НаСервере
Функция ТолькоНаСервере() Экспорт
    Возврат 1;
КонецФункции
#КонецЕсли

//- /test.bsl
Процедура Тест()
    Результат = Документы.ПКО.ТолькоНаСервере();
КонецПроцедуры
"#,
    );

    let inference = db.infer(file_id);
    let candidate = inference
        .call_arg_bindings
        .iter()
        .map(|binding| &binding.candidate)
        .find(|candidate| {
            candidate.candidates.as_slice().iter().all(|signature| signature.id.is_user())
        })
        .expect("user method call must attach candidate semantics");
    assert_eq!(candidate.candidates.as_slice()[0].environment, EnvFlags::SERVER);
}

#[test]
fn user_method_candidate_preserves_client_preprocessor_environment() {
    let target = "/Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl";
    let (db, file_id) = setup_for(
        r#"
//- /Catalogs/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl
#Если ТонкийКлиент Тогда
&НаКлиенте
Функция ТолькоНаТонкомКлиенте()
    Возврат 1;
КонецФункции
#КонецЕсли

&НаКлиенте
Процедура Тест()
    Результат = ТолькоНаТонкомКлиенте();
КонецПроцедуры
"#,
        target,
    );

    let inference = db.infer(file_id);
    let candidate = inference
        .call_arg_bindings
        .iter()
        .map(|binding| &binding.candidate)
        .find(|candidate| {
            candidate.candidates.as_slice().iter().all(|signature| signature.id.is_user())
        })
        .expect("user method call must attach candidate semantics");
    assert_eq!(candidate.candidates.as_slice()[0].environment, EnvFlags::THIN_CLIENT);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn platform_manager_call_attaches_complete_candidate_resolution() {
    let (mut db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    Результат = Справочники.Справочник1.НайтиПоКоду("001");
КонецПроцедуры
"#,
    );
    db.set_all_config_paths(vec![(
        None,
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer")),
    )]);

    let inference = db.infer(file_id);
    let binding = inference
        .call_arg_bindings
        .iter()
        .find(|binding| {
            binding
                .candidate
                .candidates
                .as_slice()
                .iter()
                .all(|signature| signature.id.is_platform())
        })
        .expect("platform manager call must attach candidate semantics");
    let candidate = &binding.candidate;
    assert!(!candidate.candidates.as_slice().is_empty());
    assert!(candidate.candidates.as_slice().iter().all(|signature| signature.id.is_platform()));
    assert!(candidate.resolution.unique_candidate().is_some());
    assert_eq!(candidate.resolution.return_ty, inference.var_types["результат"]);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn constant_refinement_updates_candidate_truth_before_resolution() {
    let (mut db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    Результат = Константы.СтрокаКонст.Получить();
    Константы.СтрокаКонст.Установить("значение");
КонецПроцедуры
"#,
    );
    db.set_all_config_paths(vec![(
        None,
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer")),
    )]);

    let inference = db.infer(file_id);
    let candidate_bindings = inference.call_arg_bindings.iter().collect::<Vec<_>>();
    assert_eq!(candidate_bindings.len(), 2);
    let get = candidate_bindings
        .iter()
        .find(|binding| binding.args.is_empty())
        .map(|binding| &binding.candidate)
        .expect("Получить binding must have no arguments");
    assert_eq!(get.resolution.return_ty, db.string(None, false));
    assert!(get
        .candidates
        .as_slice()
        .iter()
        .all(|candidate| candidate.return_ty == db.string(None, false)));
    let set = candidate_bindings
        .iter()
        .find(|binding| binding.args.len() == 1)
        .map(|binding| &binding.candidate)
        .expect("Установить binding must have one argument");
    assert!(set.candidates.as_slice().iter().all(|candidate| {
        candidate.params.first().is_some_and(|param| param.ty == db.string(None, false))
    }));
}
