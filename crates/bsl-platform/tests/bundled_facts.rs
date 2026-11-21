//! The corpus compiled into the analyzer holds interface facts only. The check
//! is structural: every key of every record is on an allow-list kept here,
//! independently of the generator script, so a text field of any name — a
//! description, a parameter note, an example — fails the build's tests rather
//! than entering the binary.

use bsl_platform::{
    MemberSurface, PlatformData, PlatformHelp, PlatformHelpRequest, PlatformHelpSourceKind,
    PlatformHelpStatus, BUNDLED_PLATFORM_VERSION,
};
use serde_json::Value;

const FACTS: &str = include_str!("../data/platform_facts.json");

/// The type the curated property correction and the first attestation name.
const RANGE_TYPE: &str = "ОбластьЯчеекТабличногоДокумента";
const RANGE_EN: &str = "SpreadsheetDocumentRange";

/// What the gate answers for the methods and for the fields of `name`, each
/// asked on its own.
fn surfaces_of(data: &PlatformData, name: &str) -> (bool, bool) {
    (
        data.platform_type_member_surface_verified(name, MemberSurface::Methods),
        data.platform_type_member_surface_verified(name, MemberSurface::Fields),
    )
}

const BOTH: (bool, bool) = (true, true);
const NEITHER: (bool, bool) = (false, false);
const METHODS_ONLY: (bool, bool) = (true, false);

const RECORD_KEYS: &[(&str, &[&str])] = &[
    (
        "types",
        &["name", "english_name", "min_version", "context", "iter_element_types", "xdto_name"],
    ),
    (
        "methods",
        &[
            "id",
            "type_name",
            "name",
            "english_name",
            "return_type",
            "parameters",
            "variants",
            "min_version",
            "context",
            "documentation",
        ],
    ),
    (
        "global_functions",
        &[
            "id",
            "name",
            "english_name",
            "return_type",
            "parameters",
            "variants",
            "min_version",
            "context",
            "documentation",
        ],
    ),
    (
        "constructors",
        &[
            "id",
            "type_name",
            "variant_name",
            "parameters",
            "min_version",
            "context",
            "documentation",
        ],
    ),
    (
        "properties",
        &[
            "id",
            "type_name",
            "name",
            "english_name",
            "property_types",
            "is_readonly",
            "min_version",
            "context",
            "documentation",
        ],
    ),
];
const DOCUMENTATION_KEYS: &[&str] = &["syntax"];
const PARAMETER_KEYS: &[&str] = &["name", "param_type", "is_optional", "is_variadic"];
const VARIANT_KEYS: &[&str] = &["variant_name", "parameters"];
const CONTEXT_KEYS: &[&str] = &[
    "thick_client",
    "thin_client",
    "web_client",
    "server",
    "mobile_client",
    "external_connection",
];

fn only_keys(where_: &str, value: &Value, allowed: &[&str], problems: &mut Vec<String>) {
    let Some(object) = value.as_object() else {
        problems.push(format!("{where_} must be an object"));
        return;
    };
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            problems.push(format!("{where_}.{key} is not an interface fact"));
        }
    }
}

fn parameters(where_: &str, value: Option<&Value>, problems: &mut Vec<String>) {
    for (index, parameter) in value.and_then(Value::as_array).into_iter().flatten().enumerate() {
        only_keys(&format!("{where_}.parameters[{index}]"), parameter, PARAMETER_KEYS, problems);
    }
}

#[test]
fn the_built_in_corpus_holds_interface_facts_and_no_texts() {
    let facts: Value = serde_json::from_str(FACTS).expect("the built-in corpus is JSON");
    let root = facts.as_object().expect("the corpus root is an object");
    let mut problems = Vec::new();
    for section in root.keys() {
        if !RECORD_KEYS.iter().any(|(name, _)| name == section) {
            problems.push(format!("unexpected section {section}"));
        }
    }
    for (section, allowed) in RECORD_KEYS {
        let records = root.get(*section).and_then(Value::as_array);
        let records = records.unwrap_or_else(|| panic!("section {section} is an array"));
        assert!(!records.is_empty(), "section {section} is not empty");
        for (index, record) in records.iter().enumerate() {
            let where_ = format!("{section}[{index}]");
            only_keys(&where_, record, allowed, &mut problems);
            if let Some(documentation) = record.get("documentation") {
                only_keys(
                    &format!("{where_}.documentation"),
                    documentation,
                    DOCUMENTATION_KEYS,
                    &mut problems,
                );
                if !documentation.get("syntax").is_some_and(Value::is_string) {
                    problems.push(format!("{where_}.documentation.syntax must be a string"));
                }
            }
            if let Some(context) = record.get("context").filter(|c| !c.is_null()) {
                only_keys(&format!("{where_}.context"), context, CONTEXT_KEYS, &mut problems);
            }
            parameters(&where_, record.get("parameters"), &mut problems);
            for (variant_index, variant) in
                record.get("variants").and_then(Value::as_array).into_iter().flatten().enumerate()
            {
                let variant_where = format!("{where_}.variants[{variant_index}]");
                only_keys(&variant_where, variant, VARIANT_KEYS, &mut problems);
                parameters(&variant_where, variant.get("parameters"), &mut problems);
            }
        }
    }
    assert!(
        problems.is_empty(),
        "{} problem(s), first: {:#?}",
        problems.len(),
        &problems[..problems.len().min(10)]
    );
}

/// The allow-list is what rejects a text: a record with a description must fail it.
#[test]
fn the_structural_check_rejects_a_description() {
    let mut problems = Vec::new();
    let record = serde_json::json!({
        "id": 1, "type_name": "Array", "name": "Добавить", "english_name": "Add",
        "documentation": {"syntax": "Добавить(<Значение>)", "description": "Adds."}
    });
    let (_, allowed) = RECORD_KEYS.iter().find(|(name, _)| *name == "methods").unwrap();
    only_keys("methods[0]", &record, allowed, &mut problems);
    only_keys(
        "methods[0].documentation",
        &record["documentation"],
        DOCUMENTATION_KEYS,
        &mut problems,
    );
    assert_eq!(problems, vec!["methods[0].documentation.description is not an interface fact"]);
}

#[test]
fn the_built_in_facts_serve_signatures_without_texts() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let origin = help.origin.clone().unwrap_or_else(|| panic!("{:?}", help.missing_reason));
    assert_eq!(origin.source, PlatformHelpSourceKind::Bundled);
    assert_eq!(origin.platform_version.as_deref(), Some(BUNDLED_PLATFORM_VERSION));
    assert!(help.missing_reason.is_none());
    let data = PlatformData::from_help(help);
    assert_eq!(data.help_status_for_target(Some("8.3.27")), PlatformHelpStatus::Available);
    assert_eq!(data.help_status_for_target(Some("8.3.28")), PlatformHelpStatus::UnsupportedTarget);

    // Facts: names in both languages, signature, parameters, return type,
    // version and context, and the curated overlays on top.
    let add = data.get_method("Массив", "Добавить").expect("RU lookup");
    assert_eq!(data.get_method("Array", "Add").map(|m| m.id), Some(add.id));
    assert_eq!(add.parameters.len(), 1);
    assert_eq!(add.parameters[0].name, "Значение");
    assert!(add.min_version.is_some() && add.context.is_some());
    let docs = data.get_method_docs(add.id).expect("the signature line is kept");
    assert!(docs.syntax.starts_with("Добавить("), "{}", docs.syntax);
    assert!(docs.description.is_empty() && docs.params.is_empty() && docs.examples.is_empty());
    assert!(docs.notes.is_none() && docs.see_also.is_empty());

    let strlen = data.get_global_function("СтрДлина").expect("global function");
    assert_eq!(strlen.english_name, "StrLen");
    assert_eq!(strlen.return_type.as_deref(), Some("Число"));
    let ctor = data.get_constructors("Массив");
    assert!(!ctor.is_empty(), "constructors are facts");
    let count = data.get_property("Массив", "Количество");
    assert!(count.is_none(), "a method is not a property");
    assert!(data.get_type("ТаблицаЗначений").is_some() && data.get_type("ValueTable").is_some());

    // Texts of every record kind are empty, not just the method above.
    assert!(data.all_methods().iter().all(|m| data.get_method_docs(m.id).is_none_or(|d| d
        .description
        .is_empty()
        && d.params.is_empty()
        && d.examples.is_empty())));
    assert!(data.all_properties().iter().all(|p| data
        .get_property_docs(p.id)
        .is_none_or(|d| d.description.is_empty() && d.notes.is_none())));
    assert!(data.all_constructors().iter().all(|c| data
        .get_constructor_docs(c.id)
        .is_none_or(|d| d.description.is_empty() && d.params.is_empty() && d.examples.is_empty())));
}

/// The curated corrections and attestations of `platform_overlays.json` are
/// authored against the built-in facts, and a typo in either would leave them
/// silently unapplied — the overlay skips a target its corpus does not have.
/// Every fact here is therefore asserted against the built-in corpus: the
/// targets exist, and the values the entries name are the ones that come out.
#[test]
fn the_curated_overlays_reach_the_built_in_facts() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);

    // The correction's target exists and carries the corrected fact; the record
    // keeps its identity, so documentation lookups still find it.
    let value = data
        .get_property("SpreadsheetDocumentRange", "Значение")
        .expect("the correction's target must exist in the built-in facts");
    assert_eq!(value.id, 12748);
    assert!(value.is_readonly, "ОбластьЯчеекТабличногоДокумента.Значение is read-only at run time");
    assert_eq!(
        data.get_property("ОбластьЯчеекТабличногоДокумента", "Value").map(|p| p.id),
        Some(12748)
    );

    // Only the named property moved: its neighbours and the identically named
    // property of the drawing keep their extracted flags.
    assert!(!data.get_property("SpreadsheetDocumentRange", "Текст").unwrap().is_readonly);
    assert!(
        !data.get_property("SpreadsheetDocumentDrawing", "Значение").unwrap().is_readonly,
        "the drawing's Значение is not part of the correction"
    );

    // The attested surfaces, under both spellings of their names.
    for name in [RANGE_TYPE, RANGE_EN] {
        assert!(
            surfaces_of(&data, name) == BOTH,
            "{name} is attested and the built-in facts are of the attested release"
        );
    }
    for name in ["ОбъектМетаданных: Реквизит", "MetadataObject: Attribute"]
    {
        assert!(surfaces_of(&data, name) == BOTH, "{name} is attested");
    }
    for name in ["ЗаписьJSON", "JSONWriter", "ТаблицаЗначений", "ValueTable"] {
        assert!(surfaces_of(&data, name) == BOTH, "{name} is attested");
    }
    // The collections that address their elements by name are attested for the
    // methods alone: an element name is answered as a property, so the property
    // surface stays open.
    for name in [
        "SpreadsheetDocumentDrawingCollection",
        "КоллекцияРисунковТабличногоДокумента",
        "StandardAttributeDescriptions",
        "ОписанияСтандартныхРеквизитов",
    ] {
        assert!(
            surfaces_of(&data, name) == METHODS_ONLY,
            "{name} is attested for its methods and not for its properties"
        );
    }
    assert_eq!(data.get_type_methods("SpreadsheetDocumentDrawingCollection").len(), 6);
    assert!(data.get_type_properties("SpreadsheetDocumentDrawingCollection").is_empty());
    assert_eq!(data.get_type_methods("StandardAttributeDescriptions").len(), 1);
    assert!(data.get_type_properties("StandardAttributeDescriptions").is_empty());
    // ...and the types nobody attested stay open, however complete they look.
    assert!(surfaces_of(&data, "ТабличныйДокумент") == NEITHER);
    assert!(surfaces_of(&data, "Запрос") == NEITHER);
    assert!(surfaces_of(&data, "ЗаписьJSONНеТакой") == NEITHER);

    // The members the attestation covers are the ones the positive tests call
    // absent: were the corpus to gain them, those tests would have to change.
    assert_eq!(
        data.get_type_methods("JSONWriter").len(),
        11,
        "the attested surface is the 8.3 one"
    );
    assert_eq!(data.get_type_properties("SpreadsheetDocumentRange").len(), 63);
    assert!(data.get_property("SpreadsheetDocumentRange", "ТекущаяОбласть").is_none());
    assert!(data.get_method("JSONWriter", "ЗаписатьНачалоТаблицы").is_none());
    assert_eq!(data.get_type_methods("ValueTable").len(), 19);
    assert_eq!(data.get_type_properties("ValueTable").len(), 2);
    assert!(data.get_method("ТаблицаЗначений", "Прочитать").is_none());
}

/// The curated return-type correction reaches the built-in facts under both
/// spellings of its target, keeps the record's identity, and leaves the
/// neighbouring factory methods as extracted.
#[test]
fn the_curated_return_type_correction_reaches_the_built_in_facts() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);

    let create = data
        .get_method("ФабрикаXDTO", "Создать")
        .expect("the correction's target must exist in the built-in facts");
    assert_eq!(create.id, 2070);
    assert_eq!(create.return_type.as_deref(), Some("ОбъектXDTO, ЗначениеXDTO"));
    assert_eq!(data.get_method("XDTOFactory", "Create").map(|m| m.id), Some(2070));

    assert_eq!(
        data.get_method("XDTOFactory", "ПрочитатьXML").and_then(|m| m.return_type.as_deref()),
        Some("ОбъектXDTO"),
        "only the named method moved"
    );
}

/// Lowercased alphanumeric words of a type name: `Form extension for a HTML document
/// field` is five words, `ConfigurationExtension` is one.
fn name_words(name: &str) -> Vec<String> {
    name.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

fn normalized(name: &str) -> String {
    name.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect()
}

/// An extension type: a type of the corpus that widens another type by a configuration
/// (`Client application form extension for documents`, `Form field extension for a text box`,
/// `Filter table box extension`). It is read off the corpus, not guessed: the word
/// `extension` (`расширение`) stands on its own in the name of such a type, while the
/// configuration-extension types (`ConfigurationExtension`, `ConfigurationExtensionsManager`)
/// carry it inside a longer word and are not extensions of anything.
fn is_extension_type(ty: &bsl_platform::PlatformType) -> bool {
    name_words(&ty.english_name).iter().any(|word| word == "extension")
        || name_words(&ty.name).iter().any(|word| word == "расширение")
}

/// The data-composition family is extended by a configuration in the same way, but it has
/// 257 types and no extension type points at them one by one, so it is carried by name. The
/// corpus spells it both with and without a space (`DataComposition…`, `…КомпоновкиДанных`),
/// so the comparison drops whitespace on both sides.
fn is_data_composition_type(ty: &bsl_platform::PlatformType) -> bool {
    [ty.name.as_str(), ty.english_name.as_str()].iter().any(|alias| {
        let folded = normalized(alias);
        folded.contains("datacomposition") || folded.contains("компоновкаданных")
    })
}

/// The base types the extension families of the corpus widen, each with the family that
/// widens it, counted on the bundled 8.3.27 corpus by the extension types' English names
/// (see [`is_extension_type`]). English canonical names. The list lives here, in the
/// curating test, and not in the overlay document: it is a curating aid, not a fact the
/// analyzer reads.
const FORM_EXTENSION_TARGETS: &[(&str, &str)] = &[
    // 13 `Client application form extension for ...` / `Managed form extension for ...` types.
    ("ClientApplicationForm", "client application / managed form extensions"),
    // 31 thick-client `<owner> form extension` types (`Catalog item form extension`, ...).
    ("Form", "<owner> form extensions of the ordinary form"),
    // 18 `Form field extension for a ...`, 11 `Tested form field extension for a ...`.
    ("FormField", "form field extensions"),
    // 7 `Form group extension for a ...`.
    ("FormGroup", "form group extensions"),
    // 8 `Form table extension for ...`.
    ("FormTable", "form table extensions"),
    // 2 `Form decoration extension for ...`.
    ("FormDecoration", "form decoration extensions"),
    // 3 `Form item addition extension for ...`.
    ("FormItemAddition", "form item addition extensions"),
    // `Form extension for a HTML document field` (12 properties) widens this field type.
    // The PDF counterpart has no field type of its own in the corpus to list.
    ("HTMLDocumentField", "form extension for a HTML document field"),
];

/// Whether the extract can call the property surface of `ty` complete: an extension type
/// is a surface a configuration widens, the data-composition family is extended by a
/// configuration, and a listed target is the type an extension family widens. None of those
/// may be attested for properties.
fn properties_stay_open(ty: &bsl_platform::PlatformType) -> bool {
    is_extension_type(ty)
        || is_data_composition_type(ty)
        || FORM_EXTENSION_TARGETS.iter().any(|(target, _)| *target == ty.english_name.as_str())
}

/// The curating rule of an attestation, held by a test rather than by prose in
/// `OVERLAYS.md`, and it is a rule about surfaces. A type whose extract had to
/// be patched has no surface the extract may call complete, so it is not
/// attested at all. A type a configuration extends and a type that addresses its
/// elements by name keep their properties open, while their methods are still the
/// platform's own, so such a type may be attested for `["methods"]` only.
///
/// "A configuration extends" is three things, none of them a substring guess at a
/// name. A type that IS an extension type (the corpus names it so, see
/// [`is_extension_type`]) widens another type and has no complete surface of its own.
/// A type an extension type WIDENS is listed in [`FORM_EXTENSION_TARGETS`]
/// with the family that widens it: a managed form gains properties from its
/// per-MDO extensions (`form_extension_property_names` in `crates/bsl-platform/src/db.rs`),
/// and the extract knows nothing of them. The data-composition family is the third and is
/// carried by name ([`is_data_composition_type`]). Neither a bare `form` in a name nor a
/// type no extension family names is extended: a form button is a type of its own that no
/// extension widens, so it may be attested for both surfaces.
///
/// The ceiling of this rule is the corpus: an extension no page describes would still slip
/// through, which is why `FORM_EXTENSION_TARGETS` is a hand-kept list and not a
/// proof, and why the corpus numbers behind it are pinned by
/// [`the_form_extension_families_of_the_corpus_are_pinned`].
///
/// The second list is the same kind of ceiling for the name-addressed collections:
/// the corpus does not say that `КоллекцияРисунковТабличногоДокумента` answers
/// `Рисунки.<имя рисунка>` as a property, a live run does, so the names are kept here by
/// hand. Each entry carries the type's English canonical name.
#[test]
fn curated_attestations_stay_off_types_the_extract_cannot_complete() {
    const ELEMENTS_ADDRESSED_BY_NAME: &[&str] =
        &["SpreadsheetDocumentDrawingCollection", "StandardAttributeDescriptions"];

    let document: Value =
        serde_json::from_str(include_str!("../data/platform_overlays.json")).expect("overlay JSON");
    let entries = document["type_member_surface_attestations"]
        .as_array()
        .expect("the curated document carries the attestations");
    assert!(!entries.is_empty(), "the rule would be vacuous without curated attestations");

    let synthesized = bsl_platform::docs_gap_synthesized_member_types();
    assert!(
        !synthesized.is_empty(),
        "an empty synthesis list would let this check pass on any attestation"
    );

    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);
    for name in ELEMENTS_ADDRESSED_BY_NAME {
        assert!(
            data.get_type(name).is_some(),
            "{name} of the name-addressed list must exist in the built-in facts"
        );
    }
    for entry in entries {
        let name = entry["type"].as_str().expect("an attested type is named");
        let ty = data
            .get_type(name)
            .unwrap_or_else(|| panic!("attested type {name} must exist in the built-in facts"));
        assert!(
            !synthesized.iter().any(|patched| *patched == ty.english_name.as_str()),
            "{name} is a type whose member list the docs-gap overlays synthesize: \
             its extract is patched, so it has no attested surface of either kind"
        );
        let surfaces: Option<Vec<&str>> = entry.get("verified_surface").map(|value| {
            value
                .as_array()
                .expect("verified_surface is a list")
                .iter()
                .map(|surface| surface.as_str().expect("a surface is named by a string"))
                .collect()
        });
        let addressed_by_name = ELEMENTS_ADDRESSED_BY_NAME.contains(&ty.english_name.as_str());
        if properties_stay_open(ty) || addressed_by_name {
            assert_eq!(
                surfaces.as_deref(),
                Some(&["methods"][..]),
                "{name} is extended by a configuration or addresses its elements by name, so \
                 its properties are open: it may be attested for [\"methods\"] only"
            );
        }
    }
}

/// The extension rules are only worth their names if they catch the corpus spellings and
/// leave the types they must not touch alone. They are exercised on the corpus's own names,
/// not on invented ones, and the controls are types that are NOT extended.
#[test]
fn the_extension_rules_catch_the_corpus_spellings() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);

    // The data-composition family is the biggest extended one, and its names are written
    // without a space.
    let composition = data.all_types().iter().filter(|ty| is_data_composition_type(ty)).count();
    assert!(
        composition > 250,
        "the corpus names a whole data-composition family, found {composition}"
    );

    for name in [
        "DataCompositionSettings",
        "НастройкиКомпоновкиДанных",
        "Расширение табличного поля отбора",
        "Filter table box extension",
        "Client application form extension for documents",
        "Form field extension for a text box",
        "Form group extension for a command bar",
        "Form extension for a HTML document field",
    ] {
        let ty = data.get_type(name).unwrap_or_else(|| {
            panic!("{name} must exist in the built-in facts for this check to mean anything")
        });
        assert!(
            properties_stay_open(ty),
            "{name} must be recognized as a type a configuration extends"
        );
    }

    // Every listed target is a type of the corpus and is named by its English canonical
    // spelling, so a Russian spelling or a typo cannot silently stop matching.
    for (target, family) in FORM_EXTENSION_TARGETS {
        let ty = data
            .get_type(target)
            .unwrap_or_else(|| panic!("target {target} must exist in the built-in facts"));
        assert_eq!(ty.english_name.as_str(), *target, "{target} is not the canonical spelling");
        assert!(properties_stay_open(ty), "{target} must stay open for properties");
        assert!(!family.is_empty(), "{target} names the family that widens it");
    }
    let mut seen = std::collections::HashSet::new();
    assert!(
        FORM_EXTENSION_TARGETS.iter().all(|(target, _)| seen.insert(target.to_lowercase())),
        "a target is listed once"
    );

    // The controls: a type that merely has `form` in its name, a type no extension family
    // widens and a type that has `extension` only inside a longer word are not extended.
    for name in
        ["FormButton", "JSONWriter", "ConfigurationExtension", "ConfigurationExtensionsManager"]
    {
        let ty = data.get_type(name).unwrap_or_else(|| panic!("{name} must exist"));
        assert!(
            !properties_stay_open(ty),
            "{name} is not extended by a configuration and must stay attestable"
        );
    }
}

/// The numbers the form-button attestation rests on, pinned so that a corpus that grows a
/// button extension (or a field extension) breaks this test instead of silently widening
/// what the attestation claims. `Form extension for a … field` is the family that widens a
/// single element, so it is counted on its own.
#[test]
fn the_form_extension_families_of_the_corpus_are_pinned() {
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);
    let spaced = |ty: &bsl_platform::PlatformType| {
        ty.english_name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let has_properties = |ty: &bsl_platform::PlatformType| {
        data.all_properties().iter().any(|prop| prop.type_name == ty.english_name)
    };

    let form_extensions: Vec<_> =
        data.all_types().iter().filter(|ty| spaced(ty).contains("form extension")).collect();
    assert_eq!(form_extensions.len(), 46, "`form extension` types of the corpus");
    assert_eq!(
        form_extensions.iter().filter(|ty| has_properties(ty)).count(),
        36,
        "`form extension` types that carry properties"
    );
    let field_family: Vec<_> = form_extensions
        .iter()
        .filter(|ty| {
            spaced(ty).starts_with("form extension for a ") && spaced(ty).ends_with(" field")
        })
        .map(|ty| ty.english_name.as_str())
        .collect();
    assert_eq!(
        field_family,
        ["Form extension for a HTML document field", "Form extension for a PDF document field"],
        "the `Form extension for a … field` family"
    );

    // No extension type of the corpus is a form button's: the only extension types that
    // mention a button belong to a radio button field and to a button group.
    let mut button_extensions: Vec<_> = data
        .all_types()
        .iter()
        .filter(|ty| is_extension_type(ty))
        .filter(|ty| name_words(&ty.english_name).iter().any(|word| word == "button"))
        .map(|ty| ty.english_name.as_str())
        .collect();
    button_extensions.sort_unstable();
    assert_eq!(
        button_extensions,
        [
            "Form field extension for a radio button field",
            "Form group extension for a button group",
            "Tested form field extension for a radio button field",
        ],
        "an extension type that names a form button would widen the attested surface"
    );
}

/// Every read-only correction must hit exactly one property of the built-in facts
/// whose extracted flag it changes: a correction of an absent property is skipped
/// at load time (older corpora lack newer properties), so a misspelt or stale entry
/// would otherwise go unnoticed, and an entry restating the extracted flag would
/// correct nothing.
#[test]
fn every_readonly_correction_changes_a_built_in_property() {
    let overlays: Value =
        serde_json::from_str(include_str!("../data/platform_overlays.json")).unwrap();
    let facts: Value = serde_json::from_str(FACTS).unwrap();
    let properties = facts["properties"].as_array().unwrap();
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);
    let corrections: Vec<&Value> = overlays["type_property_corrections"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|correction| correction.get("is_readonly").is_some())
        .collect();
    assert!(corrections.iter().any(|correction| correction["is_readonly"] == false));
    for correction in corrections {
        let type_name = correction["type"].as_str().unwrap();
        let property_name = correction["name"].as_str().unwrap();
        let corrected = correction["is_readonly"].as_bool().unwrap();
        let canonical_type = data
            .get_type(type_name)
            .unwrap_or_else(|| panic!("{type_name} is a built-in type"))
            .english_name
            .clone();
        let extracted: Vec<&Value> = properties
            .iter()
            .filter(|prop| {
                prop["type_name"] == canonical_type.as_str()
                    && (prop["name"] == property_name || prop["english_name"] == property_name)
            })
            .collect();
        assert_eq!(extracted.len(), 1, "{type_name}.{property_name}: one extracted property");
        assert_eq!(
            extracted[0]["is_readonly"].as_bool().unwrap_or(false),
            !corrected,
            "{type_name}.{property_name}: the extracted flag is the one the entry corrects"
        );
        for name in [&extracted[0]["name"], &extracted[0]["english_name"]] {
            let name = name.as_str().unwrap();
            let served = data
                .get_property(&canonical_type, name)
                .unwrap_or_else(|| panic!("{canonical_type}.{name} is served"));
            assert_eq!(served.is_readonly, corrected, "{canonical_type}.{name}");
        }
    }
}

/// The folded names of the functions the shipped passing attestation leaves out.
fn shipped_unverified_functions() -> std::collections::BTreeSet<String> {
    use stdx::case::CaseExt;
    let document: Value =
        serde_json::from_str(include_str!("../data/platform_overlays.json")).expect("overlay JSON");
    let attestations = document["parameter_passing_attestations"]
        .as_array()
        .expect("the curated document carries the passing attestation");
    assert_eq!(attestations.len(), 1, "the guards below are written for one global attestation");
    attestations[0]["unverified_functions"]
        .as_array()
        .expect("the attestation lists its unverified functions")
        .iter()
        .map(|name| name.as_str().expect("a function is named").fold_lower())
        .collect()
}

/// The exclusion list must be exactly the functions that carry call variants. The extract
/// flattens a function's variants into one parameter list and leaves the merged ones
/// undescribed, so a function that gains variants later must be added to the list, or the
/// attestation would start claiming "by value" for parameters nothing ever verified.
#[test]
fn the_unverified_functions_are_exactly_the_global_functions_with_call_variants() {
    use stdx::case::CaseExt;
    let facts: Value = serde_json::from_str(FACTS).expect("the built-in corpus is JSON");
    let with_variants: std::collections::BTreeSet<String> = facts["global_functions"]
        .as_array()
        .expect("global functions are an array")
        .iter()
        .filter(|function| {
            function["variants"].as_array().is_some_and(|variants| !variants.is_empty())
        })
        .map(|function| function["name"].as_str().expect("a function is named").fold_lower())
        .collect();
    let listed = shipped_unverified_functions();
    assert!(!listed.is_empty(), "the guard would be vacuous without listed functions");

    let missing: Vec<_> = with_variants.difference(&listed).collect();
    let superfluous: Vec<_> = listed.difference(&with_variants).collect();
    assert!(
        missing.is_empty() && superfluous.is_empty(),
        "the extract flattens a function's call variants into one parameter list and leaves the \
         merged ones undescribed, so every global function with variants must be listed in \
         unverified_functions, or the attestation would claim \"by value\" for parameters \
         nothing ever verified. With variants but not listed: {missing:?}; listed but without \
         variants: {superfluous:?}"
    );
}

/// The attestation's effect on the shipped corpus, computed from the data: outside the list
/// every parameter has a verdict, inside it none has.
#[test]
fn the_passing_attestation_covers_every_parameter_outside_its_exclusion_list() {
    use stdx::case::CaseExt;
    let help = PlatformHelp::without_io(&PlatformHelpRequest::Bundled).unwrap();
    let data = PlatformData::from_help(help);
    let listed = shipped_unverified_functions();

    let mut attested = 0;
    for function in data.all_global_functions() {
        let excluded = listed.contains(&function.name.fold_lower());
        for parameter in &function.parameters {
            if excluded {
                assert_eq!(
                    parameter.passing, None,
                    "{}: parameter {} of an unverified function must stay unknown",
                    function.name, parameter.name
                );
            } else {
                attested += 1;
                assert!(
                    parameter.passing.is_some(),
                    "{}: parameter {} has no passing verdict although the function is attested",
                    function.name,
                    parameter.name
                );
            }
        }
    }
    assert!(attested > 0, "the guard would be vacuous without attested parameters");
}
