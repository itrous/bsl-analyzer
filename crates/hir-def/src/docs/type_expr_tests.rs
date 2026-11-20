use super::{parse_type_expr, DocTypeExpr};
use crate::docs::{parse_method_docs, TypeDoc};

fn parse_docs(lines: &[&str]) -> crate::docs::MethodDocs {
    let comments = lines.iter().map(|line| (*line).to_string()).collect::<Vec<_>>();
    parse_method_docs(&comments).expect("test documentation must parse")
}

#[test]
fn parses_russian_and_english_method_collection_see_returns() {
    let russian = parse_docs(&[
        "Параметры:",
        "  Данные - см. ОбщийМодуль.ДанныеФайла",
        "Возвращаемое значение:",
        "  Массив из см. РаботаСФайлами.ДанныеФайла",
    ]);
    let english = parse_docs(&[
        "Parameters:",
        "  Data - See CommonModule.FileData",
        "Returns:",
        "  Array of See FileService.FileData",
    ]);

    assert_eq!(
        russian.returned_value[0].description.as_deref(),
        Some("из см. РаботаСФайлами.ДанныеФайла")
    );
    assert_eq!(
        english.returned_value[0].description.as_deref(),
        Some("of See FileService.FileData")
    );
    assert!(matches!(parse_type_expr(&russian.parameters[0].types[0]), Some(DocTypeExpr::See(_))));
    assert!(matches!(
        parse_type_expr(&russian.returned_value[0]),
        Some(DocTypeExpr::Array(element)) if matches!(*element, DocTypeExpr::See(_))
    ));
    assert!(matches!(parse_type_expr(&english.parameters[0].types[0]), Some(DocTypeExpr::See(_))));
    assert!(matches!(
        parse_type_expr(&english.returned_value[0]),
        Some(DocTypeExpr::Array(element)) if matches!(*element, DocTypeExpr::See(_))
    ));
}

#[test]
fn parses_structure_fields_recursively() {
    let docs = parse_docs(&[
        "Возвращаемое значение:",
        "  Структура:",
        "    * Адрес - Структура:",
        "    ** Город - Строка - город.",
    ]);

    let Some(DocTypeExpr::Structure { fields }) = parse_type_expr(&docs.returned_value[0]) else {
        panic!("expected documented structure");
    };

    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "Адрес");
    let [DocTypeExpr::Structure { fields }] = fields[0].types.as_slice() else {
        panic!("expected nested documented structure");
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "Город");
    assert!(matches!(fields[0].types.as_slice(), [DocTypeExpr::TypeRef(_)]));
}

#[test]
fn parses_full_name_collections_and_merged_descriptions() {
    let docs = parse_docs(&[
        "Параметры:",
        "  Русские - МаСсИв ИЗ Строка",
        "  English - aRrAy OF Number",
        "Возвращаемое значение:",
        "  МаСсИв ИЗ Структура - элементы результата:",
        "    * РусскоеПоле - Массив из Строка",
        "    * EnglishField - Array of Number",
    ]);

    assert!(matches!(
        parse_type_expr(&docs.parameters[0].types[0]),
        Some(DocTypeExpr::Array(element)) if matches!(*element, DocTypeExpr::TypeRef(_))
    ));
    assert!(matches!(
        parse_type_expr(&docs.parameters[1].types[0]),
        Some(DocTypeExpr::Array(element)) if matches!(*element, DocTypeExpr::TypeRef(_))
    ));

    let Some(DocTypeExpr::Array(element)) = parse_type_expr(&docs.returned_value[0]) else {
        panic!("expected documented array");
    };
    let DocTypeExpr::Structure { fields } = *element else {
        panic!("expected array element structure");
    };
    assert_eq!(
        docs.returned_value[0].description.as_deref(),
        Some("из Структура - элементы результата:")
    );
    assert_eq!(fields.len(), 2);
    assert!(fields.iter().all(|field| {
        matches!(field.types.as_slice(), [DocTypeExpr::Array(element)] if matches!(**element, DocTypeExpr::TypeRef(_)))
    }));
}

#[test]
fn rejects_prose_and_malformed_see_targets() {
    for invalid in [
        "См. 5",
        "See Service.Type trailing prose",
        "5 см.",
        "(см. Модуль.Тип)",
        "описание полей см. Модуль.Тип. Иначе текст",
    ] {
        assert_eq!(parse_type_expr(&TypeDoc::simple(invalid.to_string(), None)), None);
    }

    for type_name in ["СметнаяСтрока", "Смещение"] {
        assert!(matches!(
            parse_type_expr(&TypeDoc::simple(type_name.to_string(), None)),
            Some(DocTypeExpr::TypeRef(_))
        ));
    }
}

#[test]
fn structure_of_a_value_type_is_a_structure_with_its_documented_fields() {
    // A parameter keeps the `из <Тип>` tail inside the type name, and the tail names the values,
    // not the container: the slot is still the structure its bullets describe.
    let docs = parse_docs(&[
        "Параметры:",
        "  Данные - Структура из КлючИЗначение:",
        "   * Ключ - Строка - имя таблицы.",
        "Возвращаемое значение:",
        "  Structure of KeyAndValue - таблицы:",
        "   * Значение - Число - число строк.",
    ]);

    let parameter = parse_type_expr(&docs.parameters[0].types[0]);
    let Some(DocTypeExpr::Structure { fields }) = parameter else {
        panic!("parameter must lower to a structure, got {parameter:?}");
    };
    assert_eq!(fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Ключ"]);

    let returned = parse_type_expr(&docs.returned_value[0]);
    let Some(DocTypeExpr::Structure { fields }) = returned else {
        panic!("return must lower to a structure, got {returned:?}");
    };
    assert_eq!(fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Значение"]);
}

#[test]
fn a_collection_that_is_not_a_structure_keeps_its_own_lowering() {
    // The `из` tail alone must not turn every collection into a structure.
    assert!(matches!(
        parse_type_expr(&TypeDoc::simple("Массив из Строка".to_string(), None)),
        Some(DocTypeExpr::Array(_))
    ));
    assert!(parse_type_expr(&TypeDoc::simple("Соответствие из Строка".to_string(), None)).is_none());
}

/// The slot's alternatives, in the order the documentation declares them.
fn param_types(docs: &crate::docs::MethodDocs, name: &str) -> Vec<DocTypeExpr> {
    docs.parameters
        .iter()
        .find(|param| param.name == name)
        .expect("test documentation must declare the parameter")
        .types
        .iter()
        .filter_map(parse_type_expr)
        .collect()
}

fn names_target(docs: &crate::docs::MethodDocs, param: &str) -> bool {
    param_types(docs, param).iter().any(DocTypeExpr::names_documentation_target)
}

#[test]
fn a_reference_is_found_wherever_the_slot_puts_it() {
    // A reference is not only ever the whole slot: an array holds it as its element, and a
    // documented structure holds it in a field. A predicate reading only the root of the
    // expression answers `false` for both, and the slots that need resolving most go unnoticed.
    let docs = parse_docs(&[
        "Параметры:",
        "  Прямая - см. База.Создать",
        "  Параметр - см. База.Создать.Настройки",
        "  ВМассиве - Массив из см. База.Создать",
        "  ВПоле - Структура:",
        "   * Ключ - см. База.Создать - вложенная ссылка.",
    ]);

    assert!(names_target(&docs, "Прямая"), "две части — метод и его возврат");
    assert!(names_target(&docs, "Параметр"), "три части — параметр названного метода");
    assert!(names_target(&docs, "ВМассиве"), "ссылка под массивом");
    assert!(names_target(&docs, "ВПоле"), "ссылка в поле документированной структуры");
}

/// `Имя - Структура - см. Модуль.Метод`: the marker stands in the description, and the slot
/// must name the same target the plain `- см. ...` form names — 1 244 such slots in one
/// configuration alone.
#[test]
fn a_marker_in_the_description_names_the_target_too() {
    let docs = parse_docs(&[
        "Параметры:",
        "  Параметры - Структура - см. ОбщегоНазначенияКлиентСервер.ПараметрыЗаписи",
    ]);

    assert!(
        names_target(&docs, "Параметры"),
        "the marker in the description was lost: {:?}",
        param_types(&docs, "Параметры")
    );
    let parsed = param_types(&docs, "Параметры");
    let [DocTypeExpr::See(target)] = parsed.as_slice() else {
        panic!("expected the documented reference, got {parsed:?}");
    };
    assert_eq!(
        target.segments().iter().map(|segment| segment.as_str()).collect::<Vec<_>>(),
        ["ОбщегоНазначенияКлиентСервер", "ПараметрыЗаписи"]
    );
}

/// A slot that documents its own bullets keeps them: a marker in the description refines a
/// PLACEHOLDER, and a structure with local fields is not one.
#[test]
fn a_marker_beside_local_fields_keeps_them() {
    let docs = parse_docs(&[
        "Параметры:",
        "  Данные - Структура - см. База.Создать",
        "   * Ключ - Строка - локальное поле.",
    ]);

    let parsed = parse_type_expr(&docs.parameters[0].types[0]);
    let Some(DocTypeExpr::Structure { fields }) = parsed else {
        panic!("expected the locally documented structure, got {parsed:?}");
    };
    assert_eq!(fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Ключ"]);
}

/// A return union may declare a `см.` member. Every order and both shapes from the
/// measurement keep two alternatives, one of them naming a target; today the slot is lost
/// whole (0 alternatives, or 1 with the marker as the type name).
#[test]
fn a_see_member_of_a_return_union_survives_in_every_order() {
    for line in [
        "  Неопределено, см. База.Создать",
        "  Неопределено, см. База.Создать - описание.",
        "  Строка, см. База.Создать",
        "  см. База.Создать, Неопределено",
    ] {
        let docs = parse_docs(&["Возвращаемое значение:", line]);
        let returned: Vec<DocTypeExpr> =
            docs.returned_value.iter().filter_map(parse_type_expr).collect();
        assert_eq!(returned.len(), 2, "{line}: {returned:?}");
        assert!(
            returned.iter().any(DocTypeExpr::names_documentation_target),
            "{line}: the reference member was lost: {returned:?}"
        );
        assert!(
            returned.iter().any(|expr| matches!(expr, DocTypeExpr::TypeRef(_))),
            "{line}: the plain member was lost: {returned:?}"
        );
    }
}

/// A continuation of a field's type list belongs to the FIELD, and the bullets after it are
/// still parsed. The shape is live: an ERP method documents its returned structure this way.
#[test]
fn a_field_continuation_stays_with_its_field() {
    let docs = parse_docs(&[
        "Возвращаемое значение:",
        "  Структура:",
        "   * Ссылка - Неопределено",
        "            - ДокументСсылка.ЭлектронныйДокументИсходящийЭДО",
        "            - ДокументСсылка.ЭлектронныйДокументВходящийЭДО",
        "   * ПометкаУдаления - Булево",
        "   * ВидДокумента - СправочникСсылка.ВидыДокументовЭДО",
    ]);

    assert_eq!(docs.returned_value.len(), 1, "{:?}", docs.returned_value);
    let Some(DocTypeExpr::Structure { fields }) = parse_type_expr(&docs.returned_value[0]) else {
        panic!(
            "expected one documented structure, got {:?}",
            parse_type_expr(&docs.returned_value[0])
        );
    };
    assert_eq!(
        fields.iter().map(|field| field.name.as_str()).collect::<Vec<_>>(),
        ["Ссылка", "ПометкаУдаления", "ВидДокумента"]
    );
    assert_eq!(fields[0].types.len(), 3, "{:?}", fields[0].types);
}

/// One-word prose after the marker (`см. также`, `см. ниже`) names no target wherever it stands:
/// in a placeholder's description it must not replace the declared structure, and as a union
/// member it must not turn a prose line into a type alternative.
#[test]
fn one_word_prose_beside_a_type_stays_prose() {
    let docs = parse_docs(&[
        "Параметры:",
        "  Параметры - Структура - см. также",
        "Возвращаемое значение:",
        "  Структура:",
        "   * Данные - Структура - см. ниже",
    ]);

    let parsed = parse_type_expr(&docs.parameters[0].types[0]);
    assert!(
        matches!(parsed, Some(DocTypeExpr::Structure { .. })),
        "проза в описании заменила объявленную структуру: {parsed:?}"
    );
    let Some(DocTypeExpr::Structure { fields }) = parse_type_expr(&docs.returned_value[0]) else {
        panic!("expected the documented structure, got {:?}", docs.returned_value);
    };
    assert!(
        matches!(fields[0].types.as_slice(), [DocTypeExpr::Structure { .. }]),
        "проза в описании поля заменила объявленную структуру: {:?}",
        fields[0].types
    );

    let docs = parse_docs(&["Возвращаемое значение:", "  Строка, см. ниже."]);
    assert!(
        docs.returned_value.iter().all(|ty| !is_see_candidate_name(&ty.name)),
        "проза стала членом союза: {:?}",
        docs.returned_value
    );
}

fn is_see_candidate_name(name: &str) -> bool {
    name.trim().to_lowercase().starts_with("см.")
}

#[test]
fn prose_and_plain_types_name_no_target() {
    // `см. в`, `см. также` and their kin parse into a one-segment reference — there are 16 291 of
    // them in one configuration alone. Counting those as cross-references would claim a target for
    // slots that never had one. `Произвольный` matters for the opposite reason: it lowers to the
    // same permissive top type a reference does, so a predicate that looked at the lowered type
    // instead of the parse could not tell the two apart.
    let docs = parse_docs(&[
        "Параметры:",
        "  Проза - см. в описании",
        "  Ещё - см. также",
        "  Любой - Произвольный",
        "  Обычный - Строка",
        "  Своя - Структура:",
        "   * Ключ - Строка - без ссылок.",
    ]);

    for param in ["Проза", "Ещё", "Любой", "Обычный", "Своя"] {
        assert!(!names_target(&docs, param), "{param} не называет цель");
    }
}

// ---- documented map keys --------------------------------------------------------------------

use super::DocMapEntry;
use crate::type_ref::BuiltinTypeRef;
use crate::TypeRef;

fn returned_keys(lines: &[&str]) -> Vec<(String, Vec<DocTypeExpr>)> {
    let docs = parse_docs(lines);
    let Some(DocTypeExpr::Map { entries }) = parse_type_expr(&docs.returned_value[0]) else {
        panic!("expected a documented map for {lines:?}");
    };
    entries.into_iter().map(|DocMapEntry { key, types }| (key, types)).collect()
}

fn number() -> DocTypeExpr {
    DocTypeExpr::TypeRef(TypeRef::from_bare_name("Число").expect("Число is a platform type"))
}

/// Whether the slot parses as a keyed map at all.
fn parses_as_keyed_map(slot: &TypeDoc) -> bool {
    matches!(parse_type_expr(slot), Some(DocTypeExpr::Map { .. }))
}

#[test]
fn quoted_items_give_the_keys_with_dashes_spaces_separators_and_doubled_quotes() {
    let docs = parse_docs(&[
        "Возвращаемое значение:",
        "  Соответствие:",
        "    * \"а-б\" - Число",
        "    * \"а б\" - Строка",
        "    * \"а - б\" - Булево - описание",
        "    * \"а\"\"б\" - Дата",
    ]);
    let Some(DocTypeExpr::Map { entries }) = parse_type_expr(&docs.returned_value[0]) else {
        panic!("expected a documented map");
    };
    let keys: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(keys, ["а-б", "а б", "а - б", "а\"б"]);
    let names: Vec<String> = entries
        .iter()
        .map(|entry| match entry.types.as_slice() {
            [DocTypeExpr::TypeRef(TypeRef::Builtin(BuiltinTypeRef::Number))] => "Число".to_owned(),
            [DocTypeExpr::TypeRef(TypeRef::Builtin(BuiltinTypeRef::String))] => "Строка".to_owned(),
            [DocTypeExpr::TypeRef(TypeRef::Builtin(BuiltinTypeRef::Boolean))] => {
                "Булево".to_owned()
            }
            [DocTypeExpr::TypeRef(TypeRef::Builtin(BuiltinTypeRef::Date))] => "Дата".to_owned(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(names, ["Число", "Строка", "Булево", "Дата"]);
    assert!(docs.returned_value[0].parameters.is_empty(), "no ordinary fields appear");
}

#[test]
fn a_parameter_slot_takes_the_same_items_in_either_language_and_case() {
    for head in ["Соответствие:", "Map:", "СоОтВеТсТвИе:", "соответствие", "MAP"]
    {
        let docs =
            parse_docs(&["Параметры:", &format!("  Данные - {head}"), "    * \"сумма\" - Число"]);
        let slot = &docs.parameters[0].types[0];
        let Some(DocTypeExpr::Map { entries }) = parse_type_expr(slot) else {
            panic!("`{head}` must be a documented map: {slot:?}");
        };
        assert_eq!(entries.len(), 1, "{head}");
        assert_eq!(entries[0].key, "сумма");
        assert_eq!(entries[0].types, [number()]);
    }
}

#[test]
fn items_that_are_not_a_quoted_key_with_a_separator_give_no_key() {
    // Each of these keeps the old reading: the slot is not a keyed map, and `TypeDoc.parameters`
    // is what it was before keys existed.
    let unquoted =
        parse_docs(&["Возвращаемое значение:", "  Соответствие:", "    * сумма - Число"]);
    assert!(!parses_as_keyed_map(&unquoted.returned_value[0]));
    assert_eq!(unquoted.returned_value[0].parameters.len(), 1, "an unquoted name stays a field");
    assert_eq!(unquoted.returned_value[0].parameters[0].name, "сумма");

    for item in ["* \"а - Число", "* \"а\"-Число", "* \"а\" Число", "* \"а\" -", "* \"\""]
    {
        let docs =
            parse_docs(&["Возвращаемое значение:", "  Соответствие:", &format!("    {item}")]);
        let slot = &docs.returned_value[0];
        assert!(slot.map_entries.is_empty(), "`{item}` must give no key: {slot:?}");
        assert!(slot.parameters.is_empty(), "`{item}` must not become a field: {slot:?}");
        assert!(!parses_as_keyed_map(slot), "`{item}`");
    }
}

#[test]
fn the_separator_needs_a_space_after_the_dash_and_the_key_keeps_its_tabs() {
    let docs = parse_docs(&[
        "Возвращаемое значение:",
        "  Соответствие:",
        "    * \"а\" - Число",
        "    * \"б\" -Число",
        "    * \"в\tг\" -\tСтрока",
    ]);
    let slot = &docs.returned_value[0];
    let keys: Vec<&str> = slot.map_entries.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(keys, ["а", "в\tг"], "`-Число` is no separator, a tab after the dash is one");
}

#[test]
fn a_general_map_declaration_takes_no_keys() {
    // `Соответствие из КлючИЗначение:` documents key and value types with `* Ключ` / `* Значение`;
    // those items are not keys, and neither is a quoted item beside them.
    for lines in [
        vec![
            "Возвращаемое значение:",
            "  Соответствие из КлючИЗначение:",
            "    * Ключ - Строка",
            "    * Значение - Число",
        ],
        vec!["Возвращаемое значение:", "  Соответствие из КлючИЗначение:", "    * \"а\" - Число"],
        vec!["Возвращаемое значение:", "  Map of KeyAndValue:", "    * \"а\" - Число"],
        vec!["Возвращаемое значение:", "  Соответствие - из КлючИЗначение:", "    * \"а\" - Число"],
    ] {
        let docs = parse_docs(&lines);
        let slot = &docs.returned_value[0];
        assert!(!parses_as_keyed_map(slot), "{lines:?} -> {slot:?}");
    }
    let general = parse_docs(&[
        "Параметры:",
        "  Данные - Соответствие из КлючИЗначение:",
        "    * Ключ - Строка",
        "    * Значение - Число",
    ]);
    assert!(!parses_as_keyed_map(&general.parameters[0].types[0]));
    let tail_in_description = parse_docs(&[
        "Параметры:",
        "  Данные - Соответствие - из КлючИЗначение:",
        "    * \"а\" - Число",
    ]);
    assert!(!parses_as_keyed_map(&tail_in_description.parameters[0].types[0]));
}

#[test]
fn nested_items_and_continuations_under_a_key_are_not_attached() {
    let docs = parse_docs(&[
        "Возвращаемое значение:",
        "  Соответствие:",
        "    * \"а\" - Соответствие:",
        "      ** \"б\" - Число",
        "    * \"в\" - Число - описание",
        "      продолжение описания",
    ]);
    let Some(DocTypeExpr::Map { entries }) = parse_type_expr(&docs.returned_value[0]) else {
        panic!("expected a documented map");
    };
    let keys: Vec<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(keys, ["а", "в"], "a nested item is not a key of the outer map");
    assert_eq!(
        entries[0].types,
        [DocTypeExpr::TypeRef(TypeRef::Map(None))],
        "the nested map has no keys of its own"
    );
    let value = &docs.returned_value[0].map_entries[1];
    assert_eq!(value.types[0].description.as_deref(), Some("описание"), "{value:?}");
}

#[test]
fn an_unreadable_alternative_stays_in_the_key_as_an_unknown_type() {
    let entries =
        returned_keys(&["Возвращаемое значение:", "  Соответствие:", "    * \"а\" - Число, см. "]);
    let [(key, types)] = entries.as_slice() else { panic!("one key expected: {entries:?}") };
    assert_eq!(key, "а");
    assert_eq!(
        types,
        &[number(), DocTypeExpr::TypeRef(TypeRef::Unknown)],
        "the alternative is kept as an unknown type, not dropped"
    );
}

#[test]
fn keys_under_an_array_stay_with_the_collection_and_not_its_element() {
    let docs = parse_docs(&[
        "Возвращаемое значение:",
        "  Массив из Соответствие:",
        "    * \"сумма\" - Число",
    ]);
    let Some(DocTypeExpr::Array(element)) = parse_type_expr(&docs.returned_value[0]) else {
        panic!("expected an array");
    };
    assert_eq!(*element, DocTypeExpr::TypeRef(TypeRef::Map(None)));
}

#[test]
fn a_field_of_a_structure_never_carries_keys() {
    let docs = parse_docs(&[
        "Возвращаемое значение:",
        "  Структура:",
        "    * Поле - Соответствие:",
        "      ** \"сумма\" - Число",
    ]);
    let Some(DocTypeExpr::Structure { fields }) = parse_type_expr(&docs.returned_value[0]) else {
        panic!("expected a documented structure");
    };
    assert_eq!(fields[0].types, [DocTypeExpr::TypeRef(TypeRef::Map(None))]);
}

#[test]
fn without_map_keys_reads_a_map_as_plain_at_every_depth() {
    let plain = DocTypeExpr::TypeRef(TypeRef::Map(None));
    let keyed = || DocTypeExpr::Map {
        entries: vec![DocMapEntry { key: "а".to_owned(), types: vec![number()] }],
    };
    assert_eq!(keyed().without_map_keys(), plain);
    assert_eq!(
        DocTypeExpr::Array(Box::new(keyed())).without_map_keys(),
        DocTypeExpr::Array(Box::new(plain.clone())),
        "under an array"
    );
    let in_field = DocTypeExpr::Structure {
        fields: vec![super::DocField { name: "П".to_owned(), types: vec![keyed()] }],
    };
    assert_eq!(
        in_field.without_map_keys(),
        DocTypeExpr::Structure {
            fields: vec![super::DocField { name: "П".to_owned(), types: vec![plain.clone()] }]
        },
        "in a field of a structure"
    );
    assert_eq!(number().without_map_keys(), number(), "everything else is untouched");
}

#[test]
fn a_documented_map_without_items_is_the_plain_map_it_always_was() {
    let docs = parse_docs(&["Параметры:", "  Данные - Соответствие - значение."]);
    assert_eq!(
        parse_type_expr(&docs.parameters[0].types[0]),
        Some(DocTypeExpr::TypeRef(TypeRef::Map(None)))
    );
}

#[test]
fn a_nested_item_is_attached_only_to_an_ordinary_field() {
    // Under an ordinary field the nested key items belong to the field's type; under a key item
    // there is no field to hold them and they are dropped.
    let docs = parse_docs(&[
        "Возвращаемое значение:",
        "  Структура:",
        "    * Поле - Соответствие:",
        "      ** \"сумма\" - Число",
    ]);
    let field = &docs.returned_value[0].parameters[0];
    let [field_type] = field.types.as_slice() else { panic!("one type expected: {field:?}") };
    let keys: Vec<&str> = field_type.map_entries.iter().map(|entry| entry.key.as_str()).collect();
    assert_eq!(keys, ["сумма"]);

    let under_key = parse_docs(&[
        "Возвращаемое значение:",
        "  Соответствие:",
        "    * \"а\" - Соответствие:",
        "      ** \"б\" - Число",
    ]);
    let outer = &under_key.returned_value[0].map_entries;
    assert_eq!(outer.len(), 1);
    assert!(outer[0].types[0].map_entries.is_empty(), "nothing is attached under a key item");
}
