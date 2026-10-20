use hir::{HirDatabase, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use test_fixture::Fixture;
use vfs::FileId;

/// Build the fixture and return the `FileId` of `/test.bsl` specifically — selecting by path
/// rather than insertion order keeps the analysed file deterministic across hash ordering.
fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = vfs::FileSet::default();
    let mut test_file = None;
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
        if file.path.as_path().to_string_lossy().ends_with("/test.bsl") {
            test_file = Some(*file_id);
        }
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    (db, test_file.expect("fixture must contain /test.bsl"))
}

fn var_ty(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> TypeId {
    *db.infer(file_id).var_types.get(var_lower).unwrap_or_else(|| panic!("no var {var_lower}"))
}

/// `None` when the variable carries no recorded type (an `Unknown`-typed local is not
/// recorded) — which, for the negative cases, already means "not a common module".
fn var_ty_opt(db: &RootDatabaseImpl, file_id: FileId, var_lower: &str) -> Option<TypeId> {
    db.infer(file_id).var_types.get(var_lower).copied()
}

const RECEIVER: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция ОбщийМодуль(Имя) Экспорт
    Возврат Имя;
КонецФункции

//- /CommonModules/ПервыйМодуль/Ext/Module.bsl
Процедура НекийМетод() Экспорт
КонецПроцедуры
"#;

#[test]
fn by_name_call_types_variable_as_common_module() {
    let fixture = format!(
        "{RECEIVER}\n//- /test.bsl\n\
         Процедура Тест()\n    М = ОбщегоНазначения.ОбщийМодуль(\"ПервыйМодуль\");\nКонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    let ty = var_ty(&db, file_id, "м");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::CommonModule(facet) if facet.name == "ПервыйМодуль"),
        "expected CommonModule(ПервыйМодуль), got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn literal_case_canonicalises_to_declared_module_name() {
    let fixture = format!(
        "{RECEIVER}\n//- /test.bsl\n\
         Процедура Тест()\n    М = ОбщегоНазначения.ОбщийМодуль(\"первыймодуль\");\nКонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    let ty = var_ty(&db, file_id, "м");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::CommonModule(facet) if facet.name == "ПервыйМодуль"),
        "lower-case literal must canonicalise to the declared name, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn dotted_manager_form_is_not_narrowed() {
    let fixture = format!(
        "{RECEIVER}\n//- /test.bsl\n\
         Процедура Тест()\n    М = ОбщегоНазначения.ОбщийМодуль(\"Справочники.Контрагенты\");\nКонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    if let Some(ty) = var_ty_opt(&db, file_id, "м") {
        assert!(
            !matches!(db.lookup_type(ty), TypeKind::CommonModule(_)),
            "dotted manager form must not produce a CommonModule type, got {:?}",
            db.lookup_type(ty)
        );
    }
}

// Mirrors БСП's real `ОбщегоНазначения.ОбщийМодуль`: the `Иначе Модуль = Неопределено`
// branch makes the flow-insensitive return type Undefined (not Unknown), which is exactly
// what the narrowing gate must accept for the feature to fire on real configurations.
const RECEIVER_BSP: &str = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
Функция СерверныйМодульМенеджера(Имя)
    Возврат Вычислить(Имя);
КонецФункции

Функция ОбщийМодуль(Имя) Экспорт
    Если Истина Тогда
        Модуль = Вычислить(Имя);
    ИначеЕсли Ложь Тогда
        Возврат СерверныйМодульМенеджера(Имя);
    Иначе
        Модуль = Неопределено;
    КонецЕсли;
    Возврат Модуль;
КонецФункции

//- /CommonModules/ПервыйМодуль/Ext/Module.bsl
Процедура НекийМетод() Экспорт
КонецПроцедуры
"#;

#[test]
fn bsp_shaped_locator_still_types_variable() {
    let fixture = format!(
        "{RECEIVER_BSP}\n//- /test.bsl\n\
         Процедура Тест()\n    М = ОбщегоНазначения.ОбщийМодуль(\"ПервыйМодуль\");\nКонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    let ty = var_ty(&db, file_id, "м");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::CommonModule(facet) if facet.name == "ПервыйМодуль"),
        "a БСП-shaped locator (Undefined return) must still narrow, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn module_fetched_and_used_inside_guard_resolves_member() {
    // The real-world idiom: fetch and call the module inside the same guarded block. The
    // assignment types `М` as the module, so the in-block member call resolves against it.
    let fixture = format!(
        "{RECEIVER}\n//- /test.bsl\n\
         Процедура Тест()\n    Если Истина Тогда\n        \
         М = ОбщегоНазначения.ОбщийМодуль(\"ПервыйМодуль\");\n        \
         М.НекийМетод();\n    КонецЕсли;\nКонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    let ty = var_ty(&db, file_id, "м");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::CommonModule(facet) if facet.name == "ПервыйМодуль"),
        "guarded fetch must still type the variable as the module, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn bare_module_reference_is_typed_as_common_module() {
    // M2: a bare module name used directly is a value of that module's type.
    let fixture = format!(
        "{RECEIVER}\n//- /test.bsl\n\
         Процедура Тест()\n    М = ПервыйМодуль;\nКонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    let ty = var_ty(&db, file_id, "м");
    assert!(
        matches!(db.lookup_type(ty), TypeKind::CommonModule(facet) if facet.name == "ПервыйМодуль"),
        "bare module reference must type as CommonModule, got {:?}",
        db.lookup_type(ty)
    );
}

#[test]
fn local_assignment_shadows_bare_module_name() {
    // M2 BLOCKER-2: a local assignment to a module-shaped name (Unknown RHS) keeps the name a
    // local variable, never a module — otherwise a use before the reassignment misresolves.
    let fixture = format!(
        "{RECEIVER}\n//- /test.bsl\n\
         Процедура Тест()\n    ПервыйМодуль = Вычислить(\"Х\");\n    М = ПервыйМодуль;\nКонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    if let Some(ty) = var_ty_opt(&db, file_id, "м") {
        assert!(
            !matches!(db.lookup_type(ty), TypeKind::CommonModule(_)),
            "a locally-assigned module-shaped name must not type as CommonModule, got {:?}",
            db.lookup_type(ty)
        );
    }
}

#[test]
fn unknown_module_name_is_not_narrowed() {
    let fixture = format!(
        "{RECEIVER}\n//- /test.bsl\n\
         Процедура Тест()\n    М = ОбщегоНазначения.ОбщийМодуль(\"НетТакогоМодуля\");\nКонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    if let Some(ty) = var_ty_opt(&db, file_id, "м") {
        assert!(
            !matches!(db.lookup_type(ty), TypeKind::CommonModule(_)),
            "an unknown module name must not produce a CommonModule type, got {:?}",
            db.lookup_type(ty)
        );
    }
}
