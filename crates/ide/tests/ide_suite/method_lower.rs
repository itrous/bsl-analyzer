use hir::{DefDatabase, MethodIdInput, ModuleId, Name};
use ide_db::base_db::RootQueryDb;
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::sync::Arc;
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

#[test]
fn method_lower_matches_module_bodies_slice() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура Foo()
    А = 1;
    Б = 2;
КонецПроцедуры
"#,
    );

    let module_id = ModuleId::new(fid);
    let symbol_tree = db.symbol_tree(module_id);
    let mid = symbol_tree.find_method(&Name::new("Foo")).expect("Foo declared").id;
    let method_input = MethodIdInput::new(&db, mid);

    let lowered = db.method_lower(method_input).expect("Foo lowers");
    let body = &lowered.body;

    let module_bodies = db.module_bodies(module_id);
    let whole_body = module_bodies.body(mid.local_id).expect("module_bodies has Foo");
    let whole_map = module_bodies.source_map(mid.local_id).expect("module_bodies has source_map");

    assert_eq!(
        body.stmt_count(),
        whole_body.stmt_count(),
        "per-method body must match module_bodies slice"
    );
    assert!(body.expr_count() > 0, "two assignments must yield expressions");

    // The per-method map speaks method-relative positions; the file view lifts
    // them by the method's offset, and the two must agree through that lift.
    let (id, range) = whole_body
        .exprs_iter()
        .find_map(|(id, _)| whole_map.expr_range(id).map(|r| (id, r)))
        .expect("an expression with a range");
    let base = module_bodies.method_offset(mid.local_id).expect("Foo has an offset");
    assert_eq!(lowered.source_map.expr_range(id).map(|r| r.lift(base)), Some(range));
    assert_eq!(
        lowered.source_map.expr_at_range(base.lower(range).expect("inside the method")),
        Some(id),
        "per-method source map must agree with module_bodies on reverse-range lookup"
    );
    assert!(u32::from(range.start()) > 0, "the probe must not sit at offset zero");
}

#[test]
fn method_lower_caches_via_salsa() {
    let (db, fid) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    А = 1;
КонецПроцедуры
"#,
    );

    let module_id = ModuleId::new(fid);
    let symbol_tree = db.symbol_tree(module_id);
    let mid = symbol_tree.find_method(&Name::new("Тест")).expect("Тест declared").id;
    let method_input = MethodIdInput::new(&db, mid);

    let first = db.method_lower(method_input).expect("Тест lowers");
    let second = db.method_lower(method_input).expect("Тест lowers");
    assert!(
        Arc::ptr_eq(&first, &second),
        "salsa-cached lowering must return the same Arc on a cache hit"
    );
}

/// The pure builder and the query fold are two routes to one value: a
/// consumer that assembles text (effective modules) must see exactly what the
/// database sees, positions included.
#[test]
fn pure_builder_and_query_fold_produce_the_same_module_bodies() {
    let text = r#"Перем МодульнаяА Экспорт;
#Если Сервер Тогда
Перем ПодУсловием;
#КонецЕсли

// Описание.
&НаСервере
Процедура Первая(Пар1, Знач Пар2 = 0) Экспорт
    Локальная = Пар1 + 1;
    Запрос = Новый Запрос("ВЫБРАТЬ 1 КАК Поле");
#Если Сервер Тогда
    Вторая(Локальная);
#КонецЕсли
КонецПроцедуры

Функция Вторая(Х)
    Если Х > 0 Тогда
        Возврат Х;
    КонецЕсли;
    Возврат 0;
КонецФункции

МодульнаяА = Первая(1);
"#;
    let (db, fid) = setup(&format!("//- /test.bsl\n{text}"));
    let module_id = ModuleId::new(fid);

    let from_db = db.module_bodies(module_id);
    let parse = db.parse(fid);
    let pure = hir::ModuleBodies::from_parse_with_text(&parse, text);

    assert_eq!(*from_db, pure);
    assert!(from_db.len() == 2 && from_db.module_code().is_some());
    assert_eq!(
        from_db.module_vars().iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
        ["МодульнаяА", "ПодУсловием"],
        "a module variable under a module-level `#Если` is still a module variable"
    );
    assert!(
        from_db.all_diagnostics().iter().all(|(_, d)| u32::from(d.range().start()) > 0),
        "lifted diagnostics carry file positions, not method-relative ones"
    );
}
