use base_db::{SourceDatabase, SourceRoot, SourceRootId};
use hir::{Builders, HirDatabase, InferenceDiagnostic, TypeId, TypeKernelDb, TypeKind};
use test_fixture::Fixture;
use vfs::FileId;

use crate::RootDatabaseImpl;

fn setup(source: &str) -> (RootDatabaseImpl, FileId) {
    let fixture = Fixture::parse(&format!("//- /test.bsl\n{source}"));
    let mut db = RootDatabaseImpl::new();
    let mut files = vfs::FileSet::default();
    for (id, file) in &fixture.files {
        files.insert(*id, file.path.clone());
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    for (id, file) in &fixture.files {
        db.set_file_source_root(*id, SourceRootId(0));
        db.set_file_text(*id, &file.content);
    }
    (db, fixture.first_file().unwrap())
}

fn local(db: &RootDatabaseImpl, file: FileId, name: &str) -> TypeId {
    db.infer(file).var_types.get(name).copied().unwrap_or_else(|| db.unknown())
}

fn mismatches(db: &RootDatabaseImpl, file: FileId) -> Vec<InferenceDiagnostic> {
    db.arg_diagnostics(file)
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::TypeMismatch { .. } => Some(diagnostic.clone()),
            _ => None,
        })
        .collect()
}

fn mismatch_types(db: &RootDatabaseImpl, file: FileId) -> Vec<String> {
    mismatches(db, file)
        .iter()
        .map(|d| match d {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => format!(
                "expected {:?}, actual {:?}",
                db.lookup_type(*expected),
                db.lookup_type(*actual)
            ),
            _ => unreachable!(),
        })
        .collect()
}

#[test]
fn property_out_parameter_replaces_undefined_in_rhs_and_body() {
    let (db, file) = setup(
        r#"Процедура Тест()
    Параметры = Новый Структура("Причины", Новый Массив);
    Причины = Неопределено;
    Если Параметры.Свойство("Причины", Причины) И Причины.Количество() > 0 Тогда
        ВПравойВетви = Причины;
        Сообщить(СтрСоединить(Причины, Символы.ПС));
    КонецЕсли;
КонецПроцедуры"#,
    );
    assert!(matches!(db.lookup_type(local(&db, file, "вправойветви")), TypeKind::Array(_)));
    assert!(mismatches(&db, file).is_empty());
}

#[test]
fn property_known_wrong_value_keeps_type_error() {
    let (db, file) = setup(
        r#"Процедура Тест()
    Параметры = Новый Структура("Причины", 42);
    Причины = Неопределено;
    Если Параметры.Свойство("Причины", Причины) Тогда
        Сообщить(СтрСоединить(Причины, Символы.ПС));
    КонецЕсли;
КонецПроцедуры"#,
    );
    assert_eq!(local(&db, file, "причины"), db.number(None, None));
    assert_eq!(mismatches(&db, file).len(), 1);
}

#[test]
fn property_unknown_value_does_not_reuse_undefined() {
    let (db, file) = setup(
        r#"Процедура Тест(Параметры)
    Причины = Неопределено;
    Если Параметры.Свойство("Причины", Причины) И Причины.Количество() > 0 Тогда
        Сообщить(СтрСоединить(Причины, Символы.ПС));
    КонецЕсли;
КонецПроцедуры"#,
    );
    assert!(mismatches(&db, file).is_empty(), "{:?}", mismatches(&db, file));
}

#[test]
fn omitted_call_slots_preserve_the_sixth_argument() {
    let (db, file) = setup(
        r#"&НаСервере
Процедура Тест()
    ЗаписьЖурналаРегистрации("Test", УровеньЖурналаРегистрации.Ошибка,,,, "RunId=test");
    ЗаписьЖурналаРегистрации("Test", УровеньЖурналаРегистрации.Ошибка,,, "RunId=test");
    ЗаписьЖурналаРегистрации("Test", УровеньЖурналаРегистрации.Ошибка,,, "RunId=test", РежимТранзакцииЗаписиЖурналаРегистрации.Независимая);
КонецПроцедуры"#,
    );
    assert_eq!(mismatches(&db, file).len(), 1, "{:?}", mismatch_types(&db, file));
}

#[test]
fn xml_reader_compound_comparison_is_boolean() {
    let (db, file) = setup(
        r#"Процедура Тест(Запуск)
    Чтение = Новый ЧтениеXML;
    Чтение.ОткрытьФайл(Запуск.runRoot + "/run.xml");
    ОшибкаИтога = Ложь;
    Пока Чтение.Прочитать() Цикл
        Если Чтение.ТипУзла = ТипУзлаXML.НачалоЭлемента И Чтение.Имя = "run" Тогда
            ОшибкаИтога = Чтение.ПолучитьАтрибут("stage") = "failed"
                И Число(Чтение.ПолучитьАтрибут("errors")) > 0
                И Чтение.ПолучитьАтрибут("runId") = Запуск.runId;
            Прервать;
        КонецЕсли;
    КонецЦикла;
    ПроверитьУсловие(ОшибкаИтога, "XML_RESULT");
КонецПроцедуры
// Параметры:
//  Условие - Булево
Процедура ПроверитьУсловие(Условие, Сообщение)
КонецПроцедуры"#,
    );
    assert_eq!(local(&db, file, "ошибкаитога"), db.boolean());
    assert_eq!(mismatches(&db, file).len(), 1, "{:?}", mismatch_types(&db, file));
}

#[test]
fn xml_string_accepts_date_and_primitives_but_rejects_value_table() {
    let (db, file) = setup(
        r#"Процедура Тест()
    А = XMLСтрока('20261107');
    ДатаEN = XMLString('20261107');
    ТекстRU = XMLСтрока("text");
    ТекстEN = XMLString("text");
    ЧислоRU = XMLСтрока(42);
    ЧислоEN = XMLString(42);
    БулевоRU = XMLСтрока(Истина);
    БулевоEN = XMLString(True);
    ОшибкаRU = XMLСтрока(Новый ТаблицаЗначений);
    ОшибкаEN = XMLString(New ValueTable);
КонецПроцедуры"#,
    );
    assert_eq!(mismatches(&db, file).len(), 2, "{:?}", mismatches(&db, file));
    for diagnostic in mismatches(&db, file) {
        let InferenceDiagnostic::TypeMismatch { actual, .. } = diagnostic else { unreachable!() };
        assert!(
            matches!(db.lookup_type(actual), TypeKind::ValueTable(_)),
            "{:?}",
            db.lookup_type(actual)
        );
    }
    for name in ["а", "датаen", "текстru", "текстen", "числоru", "числоen", "булевоru", "булевоen"]
    {
        assert_eq!(local(&db, file, name), db.string(None, false), "{name}");
    }
}

#[test]
fn sequential_typed_loops_replace_the_same_binding_and_reset_unknown() {
    let (db, file) = setup(
        r#"Процедура Тест(Неизвестные)
    Первые = ПервыйНабор();
    Вторые = ВторойНабор();
    Для Каждого Строка Из Первые Цикл
        Первый = Строка;
    КонецЦикла;
    Для Каждого Строка Из Вторые Цикл
        Второй = Строка;
    КонецЦикла;
    Для Каждого ДругаяСтрока Из Вторые Цикл
        ВторойСДругимИменем = ДругаяСтрока;
    КонецЦикла;
    Для Каждого Строка Из Неизвестные Цикл
        Неизвестный = Строка;
    КонецЦикла;
    Для Каждого Строка Из Первые Цикл
        СледующийИзвестный = Строка;
    КонецЦикла;
КонецПроцедуры
// Возвращаемое значение:
//  Массив из Строка - первый набор.
Функция ПервыйНабор()
    Возврат Новый Массив;
КонецФункции
// Возвращаемое значение:
//  Массив из Число - второй набор.
Функция ВторойНабор()
    Возврат Новый Массив;
КонецФункции"#,
    );
    assert_eq!(local(&db, file, "первый"), db.string(None, false));
    assert_eq!(local(&db, file, "второй"), db.number(None, None));
    assert_eq!(local(&db, file, "второйсдругимименем"), db.number(None, None));
    assert_eq!(local(&db, file, "неизвестный"), db.unknown());
    assert_eq!(local(&db, file, "следующийизвестный"), db.string(None, false));
}

#[test]
fn sequential_register_sets_check_only_the_current_record_fields() {
    let (mut db, file) = setup(
        r#"Процедура Тест(Неизвестные)
    Первые = ПервыйНабор();
    Вторые = ВторойНабор();
    Для Каждого Запись Из Первые Цикл
        Первый = Запись.Справочник1;
    КонецЦикла;
    Для Каждого Запись Из Вторые Цикл
        Второй = Запись.Количество;
        СтароеПоле = Запись.Справочник1;
    КонецЦикла;
    Для Каждого ДругаяЗапись Из Вторые Цикл
        ДругойВторой = ДругаяЗапись.Количество;
    КонецЦикла;
    Для Каждого Запись Из Неизвестные Цикл
        Неизвестный = Запись.Ключ;
    КонецЦикла;
    Для Каждого Запись Из Первые Цикл
        СледующийИзвестный = Запись.НетПоля;
    КонецЦикла;
КонецПроцедуры
// Возвращаемое значение:
//  РегистрСведенийНаборЗаписей.РегистрСведений1
Функция ПервыйНабор()
    Возврат Неопределено;
КонецФункции
// Возвращаемое значение:
//  РегистрНакопленияНаборЗаписей.РегистрНакопления1
Функция ВторойНабор()
    Возврат Неопределено;
КонецФункции"#,
    );
    db.set_all_config_paths(
        vec![(
            None,
            std::path::PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../bsl-metadata/fixtures/designer"
            )),
        )],
        None,
    );
    assert_eq!(local(&db, file, "второй"), db.number(None, None));
    assert_eq!(local(&db, file, "другойвторой"), db.number(None, None));
    let missing: Vec<_> = db
        .infer(file)
        .diagnostics
        .iter()
        .filter_map(|(_, diagnostic)| match diagnostic {
            InferenceDiagnostic::UnresolvedField { field_name, .. } => {
                Some(field_name.as_str().to_owned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(missing, ["Справочник1", "НетПоля"]);
}

#[test]
fn xml_reader_required_attribute_guard_keeps_numeric_conversion_valid() {
    let (db, file) = setup(
        r#"Процедура Тест()
    Чтение = Новый ЧтениеXML;
    Ошибки = Чтение.ПолучитьАтрибут("errors");
    Если Ошибки <> Неопределено Тогда
        ОшибкаИтога = Чтение.ПолучитьАтрибут("stage") = "failed"
            И Число(Ошибки) > 0;
    КонецЕсли;
КонецПроцедуры"#,
    );
    assert_eq!(local(&db, file, "ошибкаитога"), db.boolean());
    assert!(mismatches(&db, file).is_empty(), "{:?}", mismatch_types(&db, file));
}

#[test]
fn property_absent_value_does_not_prove_an_error_in_the_true_branch() {
    let (db, file) = setup(
        r#"Процедура Тест()
    Параметры = Новый Структура("Другое", 42);
    Причины = Неопределено;
    Если Параметры.Property("Причины", Причины) И Причины.Количество() > 0 Тогда
        Сообщить(СтрСоединить(Причины, Символы.ПС));
    КонецЕсли;
    Причины = 42;
    Сообщить(СтрСоединить(Причины, Символы.ПС));
КонецПроцедуры"#,
    );
    assert_eq!(mismatches(&db, file).len(), 1, "{:?}", mismatch_types(&db, file));
}

#[test]
fn property_name_on_a_known_non_structure_does_not_supply_an_output_type() {
    let (db, file) = setup(
        r#"Процедура Тест()
    Приемник = Новый Массив;
    Причины = 42;
    Приемник.Свойство("Причины", Причины);
    Сообщить(СтрСоединить(Причины, Символы.ПС));
КонецПроцедуры"#,
    );
    assert_eq!(local(&db, file, "причины"), db.number(None, None));
    assert_eq!(mismatches(&db, file).len(), 1, "{:?}", mismatch_types(&db, file));
}

#[test]
fn ws_proxy_constructor_keeps_omitted_and_undefined_fifth_slots() {
    use hir::DefDatabase;
    use hir::{Expr, Literal};
    let (db, file) = setup(
        r#"Прокси1 = Новый WSПрокси(Определение, Пространство, Сервис, Порт,, 30);
Прокси2 = Новый WSПрокси(Определение, Пространство, Сервис, Порт, Неопределено, 30);
Прокси3 = Новый WSПрокси(Определение, Пространство, Сервис, Порт);"#,
    );
    let bodies = db.module_bodies(hir::ModuleId::new(file));
    let body = bodies.module_code().unwrap();
    let calls: Vec<_> = body
        .exprs_iter()
        .filter_map(|(_, expr)| match expr {
            Expr::New { args, .. } => Some(args),
            _ => None,
        })
        .collect();
    assert_eq!(calls.iter().map(|args| args.len()).collect::<Vec<_>>(), [6, 6, 4]);
    assert!(matches!(body.expr_idx(calls[0][4]), Expr::Missing));
    assert!(matches!(body.expr_idx(calls[1][4]), Expr::Literal(Literal::Undefined)));
    for args in &calls[..2] {
        assert!(matches!(body.expr_idx(args[5]), Expr::Literal(Literal::Number(_))));
    }
}
