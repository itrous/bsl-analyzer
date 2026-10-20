//! The deprecation table is hand-written while the platform database is
//! generated from the HBK dumps, so the two can drift apart without any other
//! test noticing: the remaining registry tests restate the registry's own
//! literals and stay green on a typo. These checks compare the table against
//! the generated data instead.

use bsl_platform::deprecation::{registry, DeprecationEntry, ElementKind};
use bsl_platform::PlatformData;
use stdx::case::CaseExt;

/// English name of the same element as the platform database knows it, or
/// `None` when the element is not described there at all.
fn platform_english_name(entry: &DeprecationEntry) -> Option<String> {
    let Some(owner) = entry.owner else {
        return platform_english_name_under_owner(entry, None);
    };
    // Members are indexed under whichever spelling of the owner the data itself
    // carries, and an owner type missing from the type table is reachable only
    // by that spelling.
    platform_english_name_under_owner(entry, Some(owner.ru)).or_else(|| {
        (!owner.en.is_empty())
            .then(|| platform_english_name_under_owner(entry, Some(owner.en)))
            .flatten()
    })
}

fn platform_english_name_under_owner(
    entry: &DeprecationEntry,
    owner: Option<&str>,
) -> Option<String> {
    let data = PlatformData::instance();
    let english = match (entry.element_kind, owner) {
        (ElementKind::Type | ElementKind::EnumName, None) => {
            data.get_type(entry.ru)?.english_name.to_string()
        }
        (ElementKind::GlobalMethod, None) => {
            data.get_global_function(entry.ru)?.english_name.to_string()
        }
        (ElementKind::GlobalProperty, None) => {
            data.get_global_property(entry.ru)?.english_name.to_string()
        }
        (ElementKind::Method | ElementKind::Constructor, Some(owner)) => {
            data.get_method(owner, entry.ru)?.english_name.to_string()
        }
        // An enum member is spelled out as a property of the enum type, so a
        // named enum value shares the lookup with ordinary members.
        (
            ElementKind::Property
            | ElementKind::Attribute
            | ElementKind::EnumValue
            | ElementKind::EnumName,
            Some(owner),
        ) => data.get_property(owner, entry.ru)?.english_name.to_string(),
        _ => return None,
    };
    Some(english)
}

/// Global-context property holding a value of `type_name`, e.g. `ОбработкаОшибок`
/// for `МенеджерОбработкиОшибок`.
fn global_property_of_type(type_name: &str) -> Option<String> {
    let data = PlatformData::instance();
    // Declared property types are spelled in Russian, so an English receiver has
    // to be folded back to the canonical name before the scan.
    let canonical = data.get_type(type_name)?.name.fold_lower();
    data.all_global_properties()
        .into_iter()
        .find(|prop| prop.property_types.iter().any(|ty| ty.fold_lower() == canonical))
        .map(|prop| prop.name.to_string())
}

/// What a replacement text claims. A replacement is a hint for a human, so a
/// bare name or a whole phrase ("одно из свойств …") is legitimate — but the
/// moment it carries a dot it claims a receiver, and a claim that is not a
/// well-formed path is a typo shown verbatim to the user.
enum ReplacementShape<'a> {
    /// Nothing that names a receiver: a bare name or prose.
    Unqualified,
    Path {
        receiver: &'a str,
        member: &'a str,
    },
    Malformed,
}

fn replacement_shape(text: &str) -> ReplacementShape<'_> {
    if !text.contains('.') {
        return ReplacementShape::Unqualified;
    }
    let mut segments = text.split('.');
    let (Some(receiver), Some(member), None) = (segments.next(), segments.next(), segments.next())
    else {
        return ReplacementShape::Malformed;
    };
    let broken = |part: &str| part.is_empty() || part.contains(char::is_whitespace);
    if broken(receiver) || broken(member) {
        return ReplacementShape::Malformed;
    }
    ReplacementShape::Path { receiver, member }
}

/// Deprecation facts whose element is absent from the generated platform data —
/// APIs the 1C help dropped once they were superseded. Their English names have
/// no source to be compared against.
const PLATFORM_DATA_HAS_NO_ENTRY: &[&str] = &[
    "Диаграмма.МаксимальноеКоличествоЦветовГрадиентнойПалитры",
    "Диаграмма.ОтображатьЗаголовок",
    "Диаграмма.ОтображатьЛегенду",
    "ДиаграммаГанта.МаксимальноеКоличествоЦветовГрадиентнойПалитры",
    "ДиаграммаГанта.ОтображатьЗаголовок",
    "ДиаграммаГанта.ОтображатьЛегенду",
    "ДиаграммаГанта.ПалитраЦветов",
    "ДиаграммаГанта.ПолучитьПалитру",
    "ДиаграммаГанта.УстановитьПалитру",
    "ДиаграммаГанта.ЦветКонцаГрадиентнойПалитры",
    "ДиаграммаГанта.ЦветНачалаГрадиентнойПалитры",
    "ОбластьПостроенияДиаграммы.ОриентацияМеток",
    "ОбластьПостроенияДиаграммы.ОтображатьЛинииЗначенийШкалы",
    "ОбластьПостроенияДиаграммы.ОтображатьПодписиШкалыЗначений",
    "ОбластьПостроенияДиаграммы.ОтображатьПодписиШкалыСерий",
    "ОбластьПостроенияДиаграммы.ОтображатьПодписиШкалыТочек",
    "ОбластьПостроенияДиаграммы.ОтображатьШкалу",
    "ОбластьПостроенияДиаграммы.ФорматШкалыЗначений",
    "ПолучитьЗаголовокКлиентскогоПриложения",
    "ПолучитьКраткийЗаголовокПриложения",
    "СводнаяДиаграмма.МаксимальноеКоличествоЦветовГрадиентнойПалитры",
    "СводнаяДиаграмма.ОтображатьЗаголовок",
    "СводнаяДиаграмма.ОтображатьЛегенду",
    "СводнаяДиаграмма.ПалитраЦветов",
    "СводнаяДиаграмма.ПолучитьПалитру",
    "СводнаяДиаграмма.УстановитьПалитру",
    "СводнаяДиаграмма.ЦветКонцаГрадиентнойПалитры",
    "СводнаяДиаграмма.ЦветНачалаГрадиентнойПалитры",
    "ТекущийВариантОсновногоШрифтаКлиентскогоПриложения",
    "УправляемаяФорма",
    "УстановитьЗаголовокКлиентскогоПриложения",
    "УстановитьКраткийЗаголовокПриложения",
];

/// Qualified replacements whose receiver the platform data cannot describe: a
/// library module that ships with the configuration rather than the platform,
/// and owner members the help dropped along with the deprecated API.
const RECEIVER_NOT_IN_PLATFORM_DATA: &[&str] = &[
    "(global): CommonUse.MessageToUser",
    "(global): ОбщегоНазначения.СообщитьПользователю",
    "ДиаграммаГанта: ColorPaletteDescription.ColorPalette",
    "ДиаграммаГанта: ColorPaletteDescription.GetPalette",
    "ДиаграммаГанта: ColorPaletteDescription.GradientPaletteEndColor",
    "ДиаграммаГанта: ColorPaletteDescription.GradientPaletteMaxColors",
    "ДиаграммаГанта: ColorPaletteDescription.GradientPaletteStartColor",
    "ДиаграммаГанта: ColorPaletteDescription.SetPalette",
    "ДиаграммаГанта: ОписаниеПалитрыЦветов.МаксимальноеКоличествоЦветовГрадиентнойПалитры",
    "ДиаграммаГанта: ОписаниеПалитрыЦветов.ПалитраЦветов",
    "ДиаграммаГанта: ОписаниеПалитрыЦветов.ПолучитьПалитру",
    "ДиаграммаГанта: ОписаниеПалитрыЦветов.УстановитьПалитру",
    "ДиаграммаГанта: ОписаниеПалитрыЦветов.ЦветКонцаГрадиентнойПалитры",
    "ДиаграммаГанта: ОписаниеПалитрыЦветов.ЦветНачалаГрадиентнойПалитры",
    "ОбластьПостроенияДиаграммы: SeriesScale.ScaleLabelLocation",
    "ОбластьПостроенияДиаграммы: ШкалаСерий.ПоложениеПодписейШкалы",
    "СводнаяДиаграмма: ColorPaletteDescription.ColorPalette",
    "СводнаяДиаграмма: ColorPaletteDescription.GetPalette",
    "СводнаяДиаграмма: ColorPaletteDescription.GradientPaletteEndColor",
    "СводнаяДиаграмма: ColorPaletteDescription.GradientPaletteMaxColors",
    "СводнаяДиаграмма: ColorPaletteDescription.GradientPaletteStartColor",
    "СводнаяДиаграмма: ColorPaletteDescription.SetPalette",
    "СводнаяДиаграмма: ОписаниеПалитрыЦветов.МаксимальноеКоличествоЦветовГрадиентнойПалитры",
    "СводнаяДиаграмма: ОписаниеПалитрыЦветов.ПалитраЦветов",
    "СводнаяДиаграмма: ОписаниеПалитрыЦветов.ПолучитьПалитру",
    "СводнаяДиаграмма: ОписаниеПалитрыЦветов.УстановитьПалитру",
    "СводнаяДиаграмма: ОписаниеПалитрыЦветов.ЦветКонцаГрадиентнойПалитры",
    "СводнаяДиаграмма: ОписаниеПалитрыЦветов.ЦветНачалаГрадиентнойПалитры",
];

/// Facts spelled only in Russian on both sides: the platform data offers no
/// English name to adopt, so these stay out of the bilingual index by necessity
/// rather than by omission. Listing them is what makes a *new* Russian-only
/// entry — a dropped alias — visible instead of silently skipped.
const RUSSIAN_ONLY_ENTRIES: &[&str] = &[
    "ОбластьПостроенияДиаграммы.ЛинииШкалы",
    "ОбластьПостроенияДиаграммы.ЦветШкалы",
    "ОриентацияМетокДиаграммы",
    "ОриентацияМетокДиаграммы.Авто",
];

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn deprecation_registry_english_names_match_platform_data() {
    let mut mismatched = Vec::new();
    let mut checked = 0usize;
    let mut absent = Vec::new();
    let mut alias_dropped = Vec::new();
    let mut russian_only = Vec::new();

    for entry in registry().entries() {
        let qualified = match entry.owner {
            Some(owner) => format!("{}.{}", owner.ru, entry.ru),
            None => entry.ru.to_string(),
        };
        let english = platform_english_name(entry);

        // An empty `en` keeps the entry out of the bilingual index, so dropping
        // an alias the platform does define is a silent loss of coverage rather
        // than a mismatch — it has to be caught here, not skipped.
        if entry.en.is_empty() {
            match english {
                Some(english) if !english.is_empty() => {
                    alias_dropped.push(format!("{qualified}: platform data defines {english:?}"));
                }
                _ => russian_only.push(qualified),
            }
            continue;
        }

        match english {
            Some(english) => {
                checked += 1;
                if english.fold_lower() != entry.en.fold_lower() {
                    mismatched.push(format!(
                        "{}: registry says {:?}, platform data says {:?}",
                        entry.ru, entry.en, english
                    ));
                }
            }
            None => absent.push(qualified),
        }
    }

    assert!(
        mismatched.is_empty(),
        "registry English names disagree with the generated platform data:\n{}",
        mismatched.join("\n"),
    );

    // Without a non-trivial number of comparisons the check above passes on any
    // registry content whatsoever; the floor guards against entries silently
    // sliding out of the comparable set.
    assert!(checked >= 17, "gate compared only {checked} entries against the platform data");

    // Elements the platform help no longer describes cannot be compared at all;
    // naming them keeps the uncovered set from growing unnoticed.
    absent.sort();
    assert_eq!(absent, PLATFORM_DATA_HAS_NO_ENTRY, "entries missing from the platform data");

    assert!(
        alias_dropped.is_empty(),
        "registry carries no English name where the platform data defines one:\n{}",
        alias_dropped.join("\n"),
    );

    russian_only.sort();
    assert_eq!(russian_only, RUSSIAN_ONLY_ENTRIES, "entries with no English name on either side",);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn deprecation_registry_replacements_name_a_reachable_receiver() {
    let mut unreachable = Vec::new();
    let mut unrecognized = Vec::new();
    let mut checked = 0usize;

    for entry in registry().entries() {
        let Some(replacement) = entry.replacement else {
            continue;
        };
        for text in [replacement.ru, replacement.en] {
            let (receiver, member) = match replacement_shape(text) {
                ReplacementShape::Unqualified => continue,
                ReplacementShape::Malformed => {
                    unreachable.push(format!("{text:?}: not a well-formed qualified name"));
                    continue;
                }
                ReplacementShape::Path { receiver, member } => (receiver, member),
            };
            let data = PlatformData::instance();
            if data.get_global_property(receiver).is_some() {
                checked += 1;
                if data.resolve_global_member(receiver, member).is_none() {
                    unreachable.push(format!(
                        "{text:?}: {receiver} is a global property, but it has no member {member}"
                    ));
                }
                continue;
            }
            // A receiver that only names a type cannot be written in BSL: the
            // platform exposes such a manager through a global property, and
            // suggesting the type name yields code that does not compile.
            if data.get_type(receiver).is_some() {
                checked += 1;
                let hint = match global_property_of_type(receiver) {
                    Some(prop) => format!("use the global property {prop}"),
                    None => "no global property exposes this type".to_string(),
                };
                unreachable
                    .push(format!("{text:?}: {receiver} is a type name, not a value — {hint}"));
                continue;
            }
            // A member replacement is written relative to the owner, so its
            // receiver is one of the owner's own properties
            // (`Диаграмма.ОписаниеПалитрыЦветов` → `ОписаниеПалитрыЦветов.ПалитраЦветов`).
            // The member is only verifiable when the data declares the receiver's
            // type; a receiver without one would otherwise be counted as checked
            // while nothing about its member was proven.
            if let Some(owner) = entry.owner {
                if let Some(declared) = data
                    .get_property(owner.ru, receiver)
                    .and_then(|prop| prop.property_types.first())
                {
                    checked += 1;
                    if data.get_property(declared, member).is_none()
                        && data.get_method(declared, member).is_none()
                    {
                        unreachable.push(format!(
                            "{text:?}: {owner}.{receiver} is a {declared}, which has no member {member}",
                            owner = owner.ru
                        ));
                    }
                    continue;
                }
            }
            // Everything left is a receiver the platform data does not describe:
            // a library module, or an owner member the help dropped. A typo
            // lands here too, so the remainder is named rather than skipped.
            unrecognized.push(match entry.owner {
                Some(owner) => format!("{}: {text}", owner.ru),
                None => format!("(global): {text}"),
            });
        }
    }

    assert!(
        unreachable.is_empty(),
        "replacements name a receiver that cannot be written in BSL:\n{}",
        unreachable.join("\n"),
    );

    assert!(checked >= 18, "gate resolved only {checked} replacement receivers");

    // Naming the remainder is what keeps a typo from passing as a library path:
    // a new unresolvable receiver shows up here instead of being skipped.
    unrecognized.sort();
    assert_eq!(
        unrecognized, RECEIVER_NOT_IN_PLATFORM_DATA,
        "qualified replacements whose receiver the platform data does not describe",
    );
}
