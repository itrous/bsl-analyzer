//! A value whose union type the documentation gave fits a slot that admits one
//! of its members.
//!
//! Documentation lists what may come in (`Неопределено, Структура`); an author
//! who passes such a value where one of the listed variants is expected chose
//! that variant. A union the analyzer computed from the code carries no such
//! promise and is still judged by every member.

use hir::{HirDatabase, InferenceDiagnostic, TypeId, TypeKernelDb, TypeKind};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

const DOC_FN: &str = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   Неопределено, Структура - Параметры открытия:
Функция ПараметрыОткрытия() Экспорт
    Пар = Новый Структура("Код, ОтборСписка");
    Возврат Пар;
КонецФункции

// Параметры:
//   П - Структура - параметры.
Процедура ПринятьСтруктуру(П) Экспорт
КонецПроцедуры

Функция БезДокументации(Условие) Экспорт
    Если Условие Тогда
        Возврат Неопределено;
    КонецЕсли;
    Возврат Новый Структура("Код");
КонецФункции
"#;

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
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
    db.set_all_config_paths(vec![(None, designer_fixture_path())], None);
    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with("/test.bsl"))
        .map(|(id, _)| *id)
        .expect("fixture must contain /test.bsl");
    (db, test_file)
}

fn with_doc_fn(test_bsl: &str) -> String {
    format!("{DOC_FN}\n//- /test.bsl\n{test_bsl}")
}

fn render(db: &RootDatabaseImpl, ty: TypeId) -> String {
    match db.lookup_type(ty) {
        TypeKind::Union(parts) => {
            parts.iter().map(|part| render(db, *part)).collect::<Vec<_>>().join(" | ")
        }
        TypeKind::Structure(facet) => match &facet.keys {
            Some(keys) => format!(
                "Структура({})",
                keys.iter().map(|key| key.as_str()).collect::<Vec<_>>().join(", ")
            ),
            None => "Структура".to_owned(),
        },
        TypeKind::Undefined => "Неопределено".to_owned(),
        TypeKind::Number(_) => "Число".to_owned(),
        TypeKind::PlatformObject(facet) => facet.name.to_string(),
        other => format!("{other:?}"),
    }
}

/// `(expected, actual, from_doc_comment)` of every reported type mismatch.
fn mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(String, String, bool)> {
    db.arg_diagnostics(file_id)
        .iter()
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::TypeMismatch { expected, actual, from_doc_comment, .. } => {
                Some((render(db, *expected), render(db, *actual), *from_doc_comment))
            }
            _ => None,
        })
        .collect()
}

fn is_union_of_structure_and_undefined(db: &RootDatabaseImpl, ty: TypeId) -> bool {
    let TypeKind::Union(parts) = db.lookup_type(ty) else {
        return false;
    };
    parts.len() == 2
        && parts.iter().any(|part| matches!(db.lookup_type(*part), TypeKind::Structure(_)))
        && parts.iter().any(|part| matches!(db.lookup_type(*part), TypeKind::Undefined))
}

/// The type of argument `index` of the one call that passes more than `index` arguments.
fn argument_ty(db: &RootDatabaseImpl, file_id: FileId, index: usize) -> TypeId {
    let infer = db.infer(file_id);
    let mut arguments = infer.call_arg_bindings.iter().filter_map(|binding| {
        let arg = binding.args.get(index)?;
        infer.expr_types_by_body.get(&binding.owner)?.get(arg).copied()
    });
    let ty = arguments.next().unwrap_or_else(|| panic!("no call passes argument {index}"));
    assert!(arguments.next().is_none(), "more than one call passes argument {index}");
    ty
}

/// Positive control: the slot is typed, so a value of a foreign type still fails it.
fn assert_foreign_value_rejected() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
Процедура Тест()
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", 42);
КонецПроцедуры
"#,
    );
    assert_eq!(
        mismatches(&db, file_id),
        vec![("Структура".to_owned(), "Число".to_owned(), false)],
        "the `Параметры` slot must be typed `Структура`"
    );
}

#[test]
fn a_documented_union_parameter_fits_a_slot_of_one_member() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
// Параметры:
//  ПараметрыОткрытия - Неопределено, Структура - Параметры открытия:
//  *Код - Строка - Необязательно
//  *ОтборСписка - Соответствие из КлючИЗначение:
//  Владелец - Форма - Форма владелец
Процедура ОткрытьФормуВыбора(ПараметрыОткрытия, Владелец) Экспорт
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", ПараметрыОткрытия, Владелец);
КонецПроцедуры
"#,
    );
    let infer = db.infer(file_id);
    let parameter_types: Vec<TypeId> =
        infer.binding_types_by_body.values().flat_map(|types| types.values().copied()).collect();
    assert!(
        parameter_types.iter().any(|ty| is_union_of_structure_and_undefined(&db, *ty)),
        "the documented parameter must keep its union type, got {:?}",
        parameter_types.iter().map(|ty| render(&db, *ty)).collect::<Vec<_>>()
    );
    assert_eq!(mismatches(&db, file_id), Vec::new());
    assert_foreign_value_rejected();
}

#[test]
fn a_documented_union_return_fits_a_slot_of_one_member() {
    let (db, file_id) = setup(&with_doc_fn(
        r#"Процедура Тест()
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", ПервыйОбщийМодуль.ПараметрыОткрытия());
КонецПроцедуры
"#,
    ));
    let ty = argument_ty(&db, file_id, 1);
    assert!(is_union_of_structure_and_undefined(&db, ty), "got {}", render(&db, ty));
    assert_eq!(mismatches(&db, file_id), Vec::new());
    assert_foreign_value_rejected();
}

#[test]
fn a_documented_union_keeps_its_origin_through_locals() {
    for body in [
        r#"Процедура Тест()
    Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар);
КонецПроцедуры
"#,
        r#"Процедура Тест()
    Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Пар2 = Пар;
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар2);
КонецПроцедуры
"#,
    ] {
        let (db, file_id) = setup(&with_doc_fn(body));
        let ty = argument_ty(&db, file_id, 1);
        assert!(is_union_of_structure_and_undefined(&db, ty), "{body}: got {}", render(&db, ty));
        assert_eq!(mismatches(&db, file_id), Vec::new(), "{body}");
    }
    assert_foreign_value_rejected();
}

#[test]
fn a_documented_union_fits_a_documented_slot_of_one_member() {
    let (db, file_id) = setup(&with_doc_fn(
        r#"Процедура Тест()
    ПервыйОбщийМодуль.ПринятьСтруктуру(ПервыйОбщийМодуль.ПараметрыОткрытия());
КонецПроцедуры
"#,
    ));
    let ty = argument_ty(&db, file_id, 0);
    assert!(is_union_of_structure_and_undefined(&db, ty), "got {}", render(&db, ty));
    assert_eq!(mismatches(&db, file_id), Vec::new());

    let (db, file_id) = setup(&with_doc_fn(
        r#"Процедура Тест()
    ПервыйОбщийМодуль.ПринятьСтруктуру(42);
КонецПроцедуры
"#,
    ));
    assert_eq!(
        mismatches(&db, file_id),
        vec![("Структура".to_owned(), "Число".to_owned(), true)],
        "the documented slot must stay typed `Структура` and report from the documentation"
    );
}

#[test]
fn a_documented_union_of_regular_members_fits_a_slot_of_one_member() {
    let (db, file_id) = setup(
        r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   СправочникОбъект.Справочник1, Структура - данные
Функция Данные() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", ПервыйОбщийМодуль.Данные());
КонецПроцедуры
"#,
    );
    let ty = argument_ty(&db, file_id, 1);
    let TypeKind::Union(parts) = db.lookup_type(ty) else {
        panic!("the documented return must be a union, got {}", render(&db, ty));
    };
    assert!(
        parts.iter().any(|part| matches!(db.lookup_type(*part), TypeKind::Structure(_)))
            && parts.iter().any(|part| matches!(db.lookup_type(*part), TypeKind::MetadataRef(_)))
            && !parts.iter().any(|part| matches!(db.lookup_type(*part), TypeKind::Undefined)),
        "got {}",
        render(&db, ty)
    );
    assert_eq!(mismatches(&db, file_id), Vec::new());
    assert_foreign_value_rejected();
}

#[test]
fn a_documented_union_without_a_fitting_member_is_still_rejected() {
    let (db, file_id) = setup(
        r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   Неопределено, Число - код
Функция Код() Экспорт
    Возврат 1;
КонецФункции

//- /test.bsl
Процедура Тест()
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", ПервыйОбщийМодуль.Код());
КонецПроцедуры
"#,
    );
    assert_eq!(
        mismatches(&db, file_id),
        vec![("Структура".to_owned(), "Число | Неопределено".to_owned(), false)]
    );
}

#[test]
fn a_documented_value_of_a_foreign_type_is_still_rejected() {
    let (db, file_id) = setup(
        r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Возвращаемое значение:
//   Число - код
Функция Код() Экспорт
    Возврат 1;
КонецФункции

//- /test.bsl
Процедура Тест()
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", ПервыйОбщийМодуль.Код());
КонецПроцедуры
"#,
    );
    assert_eq!(mismatches(&db, file_id), vec![("Структура".to_owned(), "Число".to_owned(), false)]);
}

#[test]
fn a_computed_union_is_still_rejected() {
    let cases: [(&str, String, &str); 5] = [
        (
            "a return of an undocumented function",
            with_doc_fn(
                r#"Процедура Тест(У)
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", ПервыйОбщийМодуль.БезДокументации(У));
КонецПроцедуры
"#,
            ),
            "Структура | Неопределено",
        ),
        (
            "a documented parameter reassigned with a computed value",
            with_doc_fn(
                r#"// Параметры:
//  П - Неопределено, Структура - параметры:
//  *Код - Строка
//  У - Булево
Процедура Тест(П, У)
    П = ПервыйОбщийМодуль.БезДокументации(У);
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", П);
КонецПроцедуры
"#,
            ),
            "Структура | Неопределено",
        ),
        (
            "a documented value overwritten by a computed one on a branch",
            with_doc_fn(
                r#"Процедура Тест(У)
    Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Если У Тогда
        Пар = ПервыйОбщийМодуль.БезДокументации(У);
    КонецЕсли;
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар);
КонецПроцедуры
"#,
            ),
            "Структура | Неопределено",
        ),
        (
            "a computed value overwritten by a documented one on a branch",
            with_doc_fn(
                r#"Процедура Тест(У)
    Пар = ПервыйОбщийМодуль.БезДокументации(У);
    Если У Тогда
        Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    КонецЕсли;
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар);
КонецПроцедуры
"#,
            ),
            "Структура(Код, ОтборСписка) | Неопределено",
        ),
        (
            "a documented value on one branch and a literal on the other",
            with_doc_fn(
                r#"Процедура Тест(У)
    Если У Тогда
        Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Иначе
        Пар = Неопределено;
    КонецЕсли;
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар);
КонецПроцедуры
"#,
            ),
            "Неопределено",
        ),
    ];
    for (case, fixture, actual) in cases {
        let (db, file_id) = setup(&fixture);
        assert_eq!(
            mismatches(&db, file_id),
            vec![("Структура".to_owned(), actual.to_owned(), false)],
            "{case}"
        );
    }
}

#[test]
fn a_documented_value_a_callee_may_rewrite_is_still_rejected() {
    let parameter = r#"// Параметры:
//  Пар - Неопределено, Структура - параметры:
//  *Код - Строка - Необязательно
"#;
    let cases: [(&str, String); 4] = [
        (
            "a documented parameter passed by reference before the call",
            format!(
                r#"
//- /test.bsl
{parameter}Процедура Тест(Пар) Экспорт
    Испортить(Пар);
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар);
КонецПроцедуры

Процедура Испортить(П)
    П = 42;
КонецПроцедуры
"#
            ),
        ),
        (
            "a documented local passed by reference before the call",
            with_doc_fn(
                r#"Процедура Тест()
    Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Испортить(Пар);
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар);
КонецПроцедуры

Процедура Испортить(П)
    П = 42;
КонецПроцедуры
"#,
            ),
        ),
        (
            "a documented local the judged call itself may rewrite on the previous turn",
            with_doc_fn(
                r#"Процедура Тест(У)
    Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Пока У Цикл
        ПервыйОбщийМодуль.ПринятьСтруктуру(Пар);
    КонецЦикла;
КонецПроцедуры
"#,
            ),
        ),
        (
            "a documented local after `Выполнить`",
            with_doc_fn(
                r#"Процедура Тест(Код)
    Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Выполнить(Код);
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар);
КонецПроцедуры
"#,
            ),
        ),
    ];
    let wrong: Vec<_> = cases
        .iter()
        .filter_map(|(case, fixture)| {
            let (db, file_id) = setup(fixture);
            let got = mismatches(&db, file_id);
            (got.len() != 1).then(|| format!("{case}: {got:?}"))
        })
        .collect();
    assert!(wrong.is_empty(), "each case must keep exactly one mismatch: {wrong:#?}");
}

#[test]
fn a_documented_union_return_of_the_same_module_fits_a_slot_of_one_member() {
    let (db, file_id) = setup(
        r#"
//- /test.bsl
// Возвращаемое значение:
//   Неопределено, Структура - данные
Функция Данные() Экспорт
    Возврат Новый Структура("Код");
КонецФункции

Процедура Тест()
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Данные());
КонецПроцедуры
"#,
    );
    let ty = argument_ty(&db, file_id, 1);
    assert!(is_union_of_structure_and_undefined(&db, ty), "got {}", render(&db, ty));
    assert_eq!(mismatches(&db, file_id), Vec::new());
    assert_foreign_value_rejected();
}

#[test]
fn a_value_defined_through_itself_is_not_documented() {
    let (db, file_id) = setup(&with_doc_fn(
        r#"Процедура Тест(У)
    Пар = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Пока У Цикл
        Пар = Пар;
    КонецЦикла;
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", Пар);
КонецПроцедуры
"#,
    ));
    assert_eq!(
        mismatches(&db, file_id),
        vec![(
            "Структура".to_owned(),
            "Структура(Код, ОтборСписка) | Неопределено".to_owned(),
            false
        )]
    );
}

#[test]
fn a_value_that_may_be_an_undocumented_parameter_is_not_documented() {
    let (db, file_id) = setup(&with_doc_fn(
        r#"Процедура Тест(П, У)
    Если У Тогда
        П = ПервыйОбщийМодуль.ПараметрыОткрытия();
    КонецЕсли;
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", П);
КонецПроцедуры
"#,
    ));
    assert_eq!(
        mismatches(&db, file_id),
        vec![(
            "Структура".to_owned(),
            "Структура(Код, ОтборСписка) | Неопределено".to_owned(),
            false
        )]
    );
}

#[test]
fn a_value_defined_through_a_cycle_of_locals_is_not_documented() {
    for body in [
        r#"Процедура Тест(У)
    Б = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Пока У Цикл
        А = Б;
        Б = А;
    КонецЦикла;
    В = А;
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", В);
КонецПроцедуры
"#,
        r#"Процедура Тест(У)
    А = ПервыйОбщийМодуль.ПараметрыОткрытия();
    Пока У Цикл
        Б = А;
        А = Б;
    КонецЦикла;
    В = А;
    ОткрытьФорму("Справочник.Справочник1.ФормаЭлемента", В);
КонецПроцедуры
"#,
    ] {
        let (db, file_id) = setup(&with_doc_fn(body));
        assert_eq!(
            mismatches(&db, file_id),
            vec![(
                "Структура".to_owned(),
                "Структура(Код, ОтборСписка) | Неопределено".to_owned(),
                false
            )],
            "{body}"
        );
    }
}
