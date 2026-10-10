use hir::{Builders, HirDatabase, InferenceDiagnostic, TypeId};
use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide_db::RootDatabaseImpl;
use std::path::PathBuf;
use test_fixture::Fixture;
use vfs::FileId;

fn designer_fixture_path() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer"))
}

fn setup(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    setup_impl(fixture_text, true, "/test.bsl")
}

fn setup_vfs_only(fixture_text: &str) -> (RootDatabaseImpl, FileId) {
    setup_impl(fixture_text, false, "/test.bsl")
}

fn setup_impl(
    fixture_text: &str,
    attach_designer_config: bool,
    test_file_suffix: &str,
) -> (RootDatabaseImpl, FileId) {
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
    if attach_designer_config {
        db.set_all_config_paths(vec![(None, designer_fixture_path())]);
    }

    let test_file = fixture
        .files
        .iter()
        .find(|(_, f)| f.path.as_path().to_string_lossy().ends_with(test_file_suffix))
        .map(|(id, _)| *id)
        .expect("fixture must contain the requested test file");
    (db, test_file)
}

fn mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(TypeId, TypeId)> {
    db.infer(file_id)
        .diagnostics
        .iter()
        .chain(db.arg_diagnostics(file_id).iter())
        .filter_map(|(_, d)| match d {
            InferenceDiagnostic::TypeMismatch { expected, actual, .. } => {
                Some((*expected, *actual))
            }
            _ => None,
        })
        .collect()
}

const COMMON_MODULE_TYPED_NUMBER_PARAM: &str = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   П - Число - численный аргумент
// Возвращаемое значение:
//   Строка - описание
Функция Привет(П) Экспорт
    Возврат "привет";
КонецФункции
"#;

#[test]
fn type_mismatch_fires_on_concrete_mismatch() {
    let fixture = format!(
        "{COMMON_MODULE_TYPED_NUMBER_PARAM}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    А = ПервыйОбщийМодуль.Привет(\"строка\");\n\
КонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);

    let mm = mismatches(&db, file_id);
    assert_eq!(
        mm,
        vec![(db.number(None, None), db.string(None, false))],
        "concrete Number param + String arg must fire exactly one mismatch"
    );
}

#[test]
fn type_mismatch_silent_on_matching_arg() {
    let fixture = format!(
        "{COMMON_MODULE_TYPED_NUMBER_PARAM}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    А = ПервыйОбщийМодуль.Привет(42);\n\
КонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    assert!(mismatches(&db, file_id).is_empty(), "matching arg must not produce any TypeMismatch");
}

#[test]
fn type_mismatch_silent_when_param_type_is_unknown() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
Функция Привет(П) Экспорт
    Возврат П;
КонецФункции

//- /test.bsl
Процедура Тест()
    А = ПервыйОбщийМодуль.Привет("строка");
    Б = ПервыйОбщийМодуль.Привет(42);
    В = ПервыйОбщийМодуль.Привет(Истина);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        mismatches(&db, file_id).is_empty(),
        "untyped param (Unknown) must accept any arg type — gradual bottom rule"
    );
}

#[test]
fn type_mismatch_silent_when_arg_type_is_unknown() {
    let fixture = format!(
        "{COMMON_MODULE_TYPED_NUMBER_PARAM}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    Х = Опаковать();\n\
    А = ПервыйОбщийМодуль.Привет(Х);\n\
КонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    assert!(
        mismatches(&db, file_id).is_empty(),
        "Unknown arg must not trigger TypeMismatch — gradual top rule"
    );
}

#[test]
fn type_mismatch_respects_null_to_ref_rule() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   Ссылка - СправочникСсылка.Справочник1 - ссылка
Процедура СохранитьСсылку(Ссылка) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ПервыйОбщийМодуль.СохранитьСсылку(NULL);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        mismatches(&db, file_id).is_empty(),
        "Null arg to a ref-typed param must not fire TypeMismatch — matches `Null ≤ ref-type` subtype rule"
    );
}

#[test]
fn type_mismatch_fires_on_three_level_manager_call() {
    let fixture = r#"
//- /Documents/ПКО/Ext/ManagerModule.bsl
// Параметры:
//   Код - Число - первый
Функция ПолучитьСсылку(Код) Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Результат = Документы.ПКО.ПолучитьСсылку("not a number");
КонецПроцедуры
"#;
    let (db, file_id) = setup_vfs_only(fixture);
    assert_eq!(
        mismatches(&db, file_id),
        vec![(db.number(None, None), db.string(None, false))],
        "a three-level manager call must emit TypeMismatch like any other resolved call"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn type_mismatch_fires_on_fluent_method_call() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    ТЗ = Новый ТаблицаЗначений();
    ТЗ.Вставить("не число");
КонецПроцедуры
"#;
    let (db, file_id) = setup_vfs_only(fixture);
    assert_eq!(
        mismatches(&db, file_id),
        vec![(db.number(None, None), db.string(None, false))],
        "fluent method call with wrong-typed arg must fire TypeMismatch via Expr::Field callee branch"
    );
}

#[test]
fn type_mismatch_silent_on_fluent_method_call_matching_arg() {
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    ТЗ = Новый ТаблицаЗначений();
    ТЗ.Вставить(0);
КонецПроцедуры
"#;
    let (db, file_id) = setup_vfs_only(fixture);
    assert!(
        mismatches(&db, file_id).is_empty(),
        "fluent method call with matching arg must not produce any TypeMismatch"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn type_mismatch_fires_on_array_insert_string_index() {
    // Guard against over-suppression: a non-union Массив receiver must still
    // flag a String passed where a numeric index is required.
    let fixture = r#"
//- /test.bsl
Процедура Тест()
    М = Новый Массив;
    М.Вставить("Ключ", 1);
КонецПроцедуры
"#;
    let (db, file_id) = setup_vfs_only(fixture);
    assert_eq!(
        mismatches(&db, file_id),
        vec![(db.number(None, None), db.string(None, false))],
        "single Массив receiver must still fire on a String index"
    );
}

#[test]
fn type_mismatch_does_not_double_fire_on_arg_count_mismatch() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   П - Число - первый
//   С - Строка - второй
Процедура Двойной(П, С) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ПервыйОбщийМодуль.Двойной(42);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let infer_diags = db.infer(file_id).diagnostics.clone();
    let arg_diags = db.arg_diagnostics(file_id);

    let count_mismatches: Vec<_> = infer_diags
        .iter()
        .chain(arg_diags.iter())
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::MismatchedArgCount { .. }))
        .collect();
    assert_eq!(count_mismatches.len(), 1, "exactly one MismatchedArgCount expected");

    let type_mismatches: Vec<_> = infer_diags
        .iter()
        .chain(arg_diags.iter())
        .filter(|(_, d)| matches!(d, InferenceDiagnostic::TypeMismatch { .. }))
        .collect();
    assert!(
        type_mismatches.is_empty(),
        "no TypeMismatch must fire for the paired position or for the missing tail — \
         got {type_mismatches:?}"
    );
}

#[test]
fn type_mismatch_silent_on_coercion_to_string_param() {
    let fixture = format!(
        "{COMMON_MODULE_TYPED_NUMBER_PARAM}\n\
//- /CommonModules/ВторойОбщийМодуль/Ext/Module.bsl\n\
// Параметры:\n\
//   С - Строка - строковый аргумент\n\
Процедура Принимает(С) Экспорт\n\
КонецПроцедуры\n\
\n\
//- /test.bsl\n\
Процедура Тест()\n\
    ВторойОбщийМодуль.Принимает(42);\n\
    ВторойОбщийМодуль.Принимает(Истина);\n\
КонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    assert!(
        mismatches(&db, file_id).is_empty(),
        "non-String arg flowing into a String param must coerce silently — got {:?}",
        mismatches(&db, file_id)
    );
}

#[test]
fn type_mismatch_still_fires_on_string_to_number() {
    let fixture = format!(
        "{COMMON_MODULE_TYPED_NUMBER_PARAM}\n\
//- /test.bsl\n\
Процедура Тест()\n\
    ПервыйОбщийМодуль.Привет(\"не число\");\n\
КонецПроцедуры\n"
    );
    let (db, file_id) = setup(&fixture);
    assert_eq!(
        mismatches(&db, file_id),
        vec![(db.number(None, None), db.string(None, false))],
        "String arg flowing into a Number param must still fire — coercion is one-way"
    );
}

#[test]
fn type_mismatch_silent_on_any_ref_param() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   Значение - Строка, ЛюбаяСсылка - строка или любая ссылка
Процедура Записать(Значение) Экспорт
КонецПроцедуры

// Возвращаемое значение:
//   СправочникСсылка.Справочник1 - ссылка на элемент
Функция ПолучитьСсылку() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Ссылка = ПервыйОбщийМодуль.ПолучитьСсылку();
    ПервыйОбщийМодуль.Записать(Ссылка);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered: Vec<(String, String)> = mismatches(&db, file_id)
        .into_iter()
        .map(|(expected, actual)| {
            (
                hir::Type::from_id(&db, file_id, expected)
                    .display_name(ide_db::base_db::Locale::Ru),
                hir::Type::from_id(&db, file_id, actual).display_name(ide_db::base_db::Locale::Ru),
            )
        })
        .collect();
    assert!(
        rendered.is_empty(),
        "concrete `СправочникСсылка.X` is a subtype of `ЛюбаяСсылка` — passing it \
         into a `Строка | ЛюбаяСсылка` param must not fire TypeMismatch; got {rendered:?}"
    );
}

#[test]
fn type_mismatch_still_fires_on_number_to_any_ref_param() {
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   Значение - ЛюбаяСсылка - любая ссылка
Процедура Записать(Значение) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ПервыйОбщийМодуль.Записать(42);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert_eq!(
        mismatches(&db, file_id),
        vec![(db.any_ref(), db.number(None, None))],
        "Число into a `ЛюбаяСсылка` param must still fire — AnyRef is a ref supertype, not gradual"
    );
}

fn rendered_mismatches(db: &RootDatabaseImpl, file_id: FileId) -> Vec<(String, String)> {
    mismatches(db, file_id)
        .into_iter()
        .map(|(expected, actual)| {
            (
                hir::Type::from_id(db, file_id, expected).display_name(ide_db::base_db::Locale::Ru),
                hir::Type::from_id(db, file_id, actual).display_name(ide_db::base_db::Locale::Ru),
            )
        })
        .collect()
}

const ISSUE80_DOM_FIXTURE: &str = include_str!("fixtures/issue80/dom.fixture");
const ISSUE80_DOM_EXPECTED: &str =
    "АтрибутDOM | АтрибутHTML | ДокументDOM | НотацияDOM | ОпределениеТипаДокументаDOM | ЭлементDOM | ЭлементHTML | ЭлементВводаHTML | ЭлементЗаголовокHTML | ЭлементКнопкаHTML";
const ISSUE80_DOM_VALID_ACTUAL: &str = "ЭлементDOM | ЭлементHTML";

#[test]
fn issue80_dom_valid_child_is_accepted() {
    let (db, file_id) = setup_impl(ISSUE80_DOM_FIXTURE, false, "/valid.bsl");
    let rendered = rendered_mismatches(&db, file_id);

    assert!(
        rendered.is_empty(),
        "a valid HTML child must satisfy ДобавитьДочерний; expected `{ISSUE80_DOM_EXPECTED}` \
         and actual `{ISSUE80_DOM_VALID_ACTUAL}` must not mismatch, got {rendered:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn issue80_dom_invalid_scalar_child_is_rejected() {
    let (db, file_id) = setup_impl(ISSUE80_DOM_FIXTURE, false, "/invalid.bsl");

    assert_eq!(
        rendered_mismatches(&db, file_id),
        vec![(ISSUE80_DOM_EXPECTED.to_string(), "Число".to_string())],
        "a scalar child must produce exactly one TypeMismatch at the ДобавитьДочерний argument"
    );
}

#[test]
fn type_mismatch_fires_on_concrete_ref_where_other_concrete_ref_documented() {
    // A swapped-arguments bug: the callee documents a concrete catalog ref,
    // the caller passes a ref of a DIFFERENT catalog. Any widening of concrete
    // refs to generic supertypes must keep flagging concrete-vs-concrete.
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//		Счет - СправочникСсылка.Справочник1 - Банковский счет организации;
Процедура ЗаполнитьСчета(Счет = Неопределено) Экспорт
КонецПроцедуры

// Возвращаемое значение:
//   СправочникСсылка.СправочникСМенеджером - ссылка
Функция СчетКонтрагента() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    СчетКонтрагента = ПервыйОбщийМодуль.СчетКонтрагента();
    ПервыйОбщийМодуль.ЗаполнитьСчета(СчетКонтрагента);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered = rendered_mismatches(&db, file_id);
    assert_eq!(
        rendered.len(),
        1,
        "a wrong concrete ref must fire exactly one TypeMismatch, got {rendered:?}"
    );
    assert!(
        rendered[0].0.contains("Справочник1") && rendered[0].1.contains("СправочникСМенеджером"),
        "expected/actual must surface both concrete catalog refs, got {rendered:?}"
    );
}

#[test]
fn type_mismatch_fires_on_undefined_bearing_result_into_structure_param() {
    // A real-bug shape: a function returns Неопределено on several paths and a
    // value on the rest; its result flows unchecked into a doc-typed Структура
    // param. The Неопределено arm must keep this call flagged.
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   СтруктураДанных - Структура - данные объекта.
Процедура ОбработатьДанные(СтруктураДанных) Экспорт
КонецПроцедуры

//- /test.bsl
Функция ДанныеОбъекта(Режим)
    Если Режим = 1 Тогда
        Возврат Неопределено;
    ИначеЕсли Режим = 2 Тогда
        Возврат Неопределено;
    ИначеЕсли Режим = 3 Тогда
        Возврат Неопределено;
    КонецЕсли;
    Возврат Новый Структура;
КонецФункции

Процедура Тест()
    ПервыйОбщийМодуль.ОбработатьДанные(ДанныеОбъекта(2));
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered = rendered_mismatches(&db, file_id);
    assert!(
        !rendered.is_empty(),
        "Неопределено-bearing result into a Структура param must fire TypeMismatch"
    );
}

#[test]
fn type_mismatch_silent_on_vendor_doc_query_and_value_list_params() {
    // Vendor doc shape (БСП-проведение, tab-indented): `Запрос - Запрос - …`
    // and `ТекстыЗапроса - СписокЗначений ИЗ Строка - …`. Arguments built with
    // exactly those platform types must pass: the doc type and the inferred
    // type are the same platform type and must compare equal.
    let fixture = r#"
//- /CommonModules/ПроведениеДокументов/Ext/Module.bsl
// Инициализирует данные документа для проведения.
//
// Параметры:
//	Запрос - Запрос - запрос, хранящий параметры, используемые в списке запросов.
//	ТекстыЗапроса - СписокЗначений ИЗ Строка - список текстов запросов и их имен.
//	ДопПараметры - см. ПроведениеДокументов.ДопПараметрыИнициализироватьДанныеДокументаДляПроведения
//
// Возвращаемое значение:
//	Структура Из КлючИЗначение - Таблицы проведения:
//			* Ключ - Строка - Имя таблицы
//			* Значение - ТаблицаЗначений - Таблица данных проведения
//
Функция ИнициализироватьДанныеДокументаДляПроведения(Запрос, ТекстыЗапроса, ДопПараметры = Неопределено) Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Запрос = Новый Запрос;
    ТекстыЗапроса = Новый СписокЗначений;
    Результат = ПроведениеДокументов.ИнициализироватьДанныеДокументаДляПроведения(Запрос, ТекстыЗапроса);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered = rendered_mismatches(&db, file_id);
    assert!(
        rendered.is_empty(),
        "args matching the documented platform types must not fire, got {rendered:?}"
    );
}

#[test]
fn type_mismatch_silent_when_doc_alternatives_follow_prose_colon() {
    // Vendor doc shape: prose, then a colon, then comma-separated alternatives
    // with a trailing parenthetical. An argument matching the FIRST listed
    // alternative must pass.
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   ОтменяемыйДокумент - Ссылка на документ видов: ДокументСсылка.Документ1, СправочникСсылка.Справочник1 (Необязательный)
Процедура ОтменитьДокумент(ОтменяемыйДокумент) Экспорт
КонецПроцедуры

// Возвращаемое значение:
//   ДокументСсылка.Документ1 - ссылка
Функция ТекущийДокумент() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Документ = ПервыйОбщийМодуль.ТекущийДокумент();
    ПервыйОбщийМодуль.ОтменитьДокумент(Документ);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered = rendered_mismatches(&db, file_id);
    assert!(
        rendered.is_empty(),
        "arg matching the first documented alternative must not fire, got {rendered:?}"
    );
}

#[test]
fn doc_return_arbitrary_keeps_call_result_unconstrained() {
    // Vendor doc shape: «Возвращаемое значение: Произвольный - …» with prose
    // continuation lines. Произвольный must stay sticky (Any): the call result
    // must satisfy any documented param type, and body inference must not
    // replace the documented Произвольный with a narrower type.
    let fixture = r#"
//- /CommonModules/ОбщегоНазначения/Ext/Module.bsl
// Параметры:
//  Ссылка    - ЛюбаяСсылка - объект, значения реквизитов которого необходимо получить.
//            - Строка      - полное имя предопределенного элемента, значения реквизитов которого необходимо получить.
//  ИмяРеквизита       - Строка - имя получаемого реквизита.
//
// Возвращаемое значение:
//  Произвольный - если в параметр Ссылка передана пустая ссылка, то возвращается Неопределено.
//                 Если в параметр Ссылка передана ссылка несуществующего объекта (битая ссылка),
//                 то возвращается Неопределено.
//
Функция ЗначениеРеквизитаОбъекта(Ссылка, ИмяРеквизита) Экспорт
    Возврат "строковое значение";
КонецФункции

//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   Склад - СправочникСсылка.Справочник1 - склад-отправитель.
Процедура ПринятьСклад(Склад) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест(Распоряжение)
    СкладОтправитель = ОбщегоНазначения.ЗначениеРеквизитаОбъекта(Распоряжение, "СкладОтправитель");
    ПервыйОбщийМодуль.ПринятьСклад(СкладОтправитель);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered = rendered_mismatches(&db, file_id);
    assert!(
        rendered.is_empty(),
        "result of a Произвольный-documented function must satisfy any param type, got {rendered:?}"
    );
}

#[test]
fn type_mismatch_silent_on_get_area_result_into_area_param() {
    // Canonical БСП print pattern: ТабличныйДокумент.ПолучитьОбласть(имя)
    // returns ТабличныйДокумент (platform data), while vendor docs annotate the
    // receiving param as ОбластьЯчеекТабличногоДокумента. Both sides describe
    // the same canonical flow — it must not fire.
    let fixture = r#"
//- /CommonModules/ШтрихкодированиеПечатныхФорм/Ext/Module.bsl
// Вывести штрихкод в табличный документ
//
// Параметры:
//  ТабличныйДокумент - ТабличныйДокумент - Табличный документ
//  Макет - ТабличныйДокумент
//  ОбластьМакета - ОбластьЯчеекТабличногоДокумента - Область
//  Ссылка - ЛюбаяСсылка - Ссылка на документ из которого будет вычислен штрихкод.
//
Процедура ВывестиШтрихкодВТабличныйДокумент(ТабличныйДокумент, Макет, Знач ОбластьМакета, Ссылка) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест(Ссылка)
    ТабличныйДокумент = Новый ТабличныйДокумент;
    Макет = Новый ТабличныйДокумент;
    ОбластьЗаголовок = Макет.ПолучитьОбласть("Заголовок");
    ШтрихкодированиеПечатныхФорм.ВывестиШтрихкодВТабличныйДокумент(ТабличныйДокумент, Макет, ОбластьЗаголовок, Ссылка);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered = rendered_mismatches(&db, file_id);
    assert!(
        rendered.is_empty(),
        "ПолучитьОбласть result into a documented area param is the canonical \
         print flow and must not fire, got {rendered:?}"
    );
}

#[test]
fn type_mismatch_silent_on_concrete_manager_into_generic_manager_param() {
    // Vendor doc shape: the param is annotated with the GENERIC platform name
    // (ДокументМенеджер) while the call passes a concrete manager
    // (Документы.Документ1) — by platform semantics the concrete IS the
    // generic.
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   МенеджерДокумента - ДокументМенеджер - менеджер документа.
Процедура ПараметрыУказанияСерий(МенеджерДокумента) Экспорт
КонецПроцедуры

//- /test.bsl
Процедура Тест()
    ПервыйОбщийМодуль.ПараметрыУказанияСерий(Документы.Документ1);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered = rendered_mismatches(&db, file_id);
    assert!(
        rendered.is_empty(),
        "a concrete document manager satisfies a ДокументМенеджер-documented param, \
         got {rendered:?}"
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn type_mismatch_fires_on_area_value_into_spreadsheet_param() {
    // The spreadsheet-area bridge is one-way: a value documented as
    // ОбластьЯчеекТабличногоДокумента must NOT be admitted where a full
    // ТабличныйДокумент is required (e.g. СкомпоноватьРезультат, Присоединить).
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   Док - ТабличныйДокумент - целевой табличный документ.
Процедура Присоединить(Док) Экспорт
КонецПроцедуры

// Возвращаемое значение:
//   ОбластьЯчеекТабличногоДокумента - область.
Функция ТекущаяОбласть() Экспорт
    Возврат Неопределено;
КонецФункции

//- /test.bsl
Процедура Тест()
    Область = ПервыйОбщийМодуль.ТекущаяОбласть();
    ПервыйОбщийМодуль.Присоединить(Область);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    let rendered = rendered_mismatches(&db, file_id);
    assert_eq!(
        rendered.len(),
        1,
        "an area value into a full-document param must keep firing, got {rendered:?}"
    );
    assert!(
        rendered[0].0.contains("ТабличныйДокумент")
            && rendered[0].1.contains("ОбластьЯчеекТабличногоДокумента"),
        "expected the full document on the expected side and the area on the actual side, \
         got {rendered:?}"
    );
}

#[test]
fn bare_identifier_matching_global_function_is_not_function_typed() {
    let fixture = r#"
//- /test.bsl
Функция ИнформационнаяБазаФайловая(Знач СтрокаСоединенияИнформационнойБазы = "") Экспорт
    Если ПустаяСтрока(СтрокаСоединенияИнформационнойБазы) Тогда
        Возврат Истина;
    КонецЕсли;
    Возврат Ложь;
КонецФункции
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        mismatches(&db, file_id).is_empty(),
        "bare identifier shadowing a platform global must not produce TypeMismatch — got {:?}",
        mismatches(&db, file_id)
    );
}

#[test]
fn type_mismatch_silent_on_keyed_structure_to_structure_param() {
    // Regression: literal-key inference types `С` as `Структура(Ключ1, Ключ2)`, a distinct interned
    // type from the param's plain `Структура`. Structure keys are a soft completion aid, never a
    // subtyping constraint, so this must NOT fire a TypeMismatch (found via ERP: ×3.6 FP blowup).
    let fixture = r#"
//- /CommonModules/ПервыйОбщийМодуль/Ext/Module.bsl
// Параметры:
//   П - Структура - параметры соединения
Функция Привет(П) Экспорт
    Возврат "привет";
КонецФункции

//- /test.bsl
Процедура Тест()
    С = Новый Структура("Ключ1, Ключ2");
    А = ПервыйОбщийМодуль.Привет(С);
КонецПроцедуры
"#;
    let (db, file_id) = setup(fixture);
    assert!(
        mismatches(&db, file_id).is_empty(),
        "a keyed literal structure must satisfy a `Структура` parameter without a TypeMismatch"
    );
}
