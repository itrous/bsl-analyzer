//! The security and capability registries are hand-written while the platform
//! database is generated from the HBK dumps, so a name can drift out from under
//! an entry without any registry test noticing: those tests restate the
//! registry's own literals and stay green on a ghost entry. These checks compare
//! each entry against the generated data, by the surface the entry declares.

use bsl_platform::capability;
use bsl_platform::security;
use bsl_platform::PlatformData;
use stdx::case::CaseExt;

/// Some platform type declaring one method under BOTH spellings of the pair.
/// Checking the halves apart would accept a pair glued from two different real
/// methods, which is the same wrong lookup as a typo, only harder to see.
fn some_type_declares_pair(ru: &str, en: &str) -> bool {
    let (ru, en) = (ru.fold_lower(), en.fold_lower());
    PlatformData::instance().all_methods().iter().any(|m| {
        let (m_ru, m_en) = (m.name.fold_lower(), m.english_name.fold_lower());
        (ru.is_empty() || m_ru == ru || m_en == ru) && (en.is_empty() || m_en == en || m_ru == en)
    })
}

fn is_global_function(name: &str) -> bool {
    PlatformData::instance().get_global_function(name).is_some()
}

/// Both spellings must land on the SAME global function.
fn global_function_pair_agrees(ru: &str, en: &str) -> bool {
    let data = PlatformData::instance();
    match (data.get_global_function(ru), data.get_global_function(en)) {
        (Some(by_ru), Some(by_en)) => by_ru.id == by_en.id,
        _ => false,
    }
}

fn is_known_type(name: &str) -> bool {
    PlatformData::instance().get_type(name).is_some()
}

/// A constructor entry claims `Новый <Тип>` works, and plenty of known types
/// cannot be constructed at all — checking the type table alone would accept
/// them.
fn is_constructible(name: &str) -> bool {
    !PlatformData::instance().get_constructors(name).is_empty()
}

/// Both spellings must land on the SAME platform type.
fn type_pair_agrees(ru: &str, en: &str) -> bool {
    let data = PlatformData::instance();
    match (data.get_type(ru), data.get_type(en)) {
        (Some(by_ru), Some(by_en)) => by_ru.name == by_en.name,
        _ => false,
    }
}

/// Entries the platform data contradicts and that are knowingly left alone.
/// Each costs a wrong attribution, so the list must not grow silently.
///
/// `СообщитьПользователю` is a BSP library method — the deprecation registry
/// itself recommends it as `ОбщегоНазначения.СообщитьПользователю` — yet it is
/// registered as a platform global. Narrowing it to its owners changes how
/// `catch_class` classifies recovery in `Попытка`, which is a separate decision
/// with its own measure.
const KNOWN_DEVIATIONS: &[&str] = &["СообщитьПользователю"];

/// Both spellings of an entry are callable, so both are checked apart. A pair
/// where only one half exists is the drift this gate is for: an English synonym
/// that no longer resolves silently drops every English call from the registry
/// while the Russian half keeps the entry looking healthy. An empty half means
/// the surface has no such spelling and is skipped.
fn spellings(ru: &'static str, en: &'static str) -> impl Iterator<Item = &'static str> {
    [ru, en].into_iter().filter(|name| !name.is_empty())
}

/// A guarded name is worth guarding only where the platform actually has it.
#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn security_entries_match_platform_surface() {
    let mut wrong = Vec::new();

    for entry in security::registry().entries() {
        if KNOWN_DEVIATIONS.contains(&entry.ru) {
            continue;
        }
        match entry.kind {
            security::EntryKind::GlobalMethod => {
                for name in spellings(entry.ru, entry.en) {
                    if !is_global_function(name) {
                        wrong.push(format!("{name}: global method absent from platform data"));
                    }
                }
                if !entry.en.is_empty() && !global_function_pair_agrees(entry.ru, entry.en) {
                    wrong.push(format!(
                        "{} / {}: spellings name different global functions",
                        entry.ru, entry.en
                    ));
                }
            }
            security::EntryKind::Constructor => {
                for name in spellings(entry.ru, entry.en) {
                    if !is_constructible(name) {
                        wrong.push(format!("{name}: no constructor in platform data"));
                    }
                }
                if !entry.en.is_empty() && !type_pair_agrees(entry.ru, entry.en) {
                    wrong.push(format!(
                        "{} / {}: spellings name different types",
                        entry.ru, entry.en
                    ));
                }
            }
            // The owners are library common modules — BSP ships them, the
            // platform does not. A method name colliding with a platform one is
            // expected and is the very reason this kind carries owners:
            // `ОткрытьФайл` is also spelled by nine serializers.
            security::EntryKind::ModuleMethod { owners } => {
                for owner in owners {
                    if is_known_type(owner) {
                        wrong.push(format!(
                            "{}: owner {owner} is a platform type, not a library module",
                            entry.ru
                        ));
                    }
                }
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "security registry disagrees with platform data:\n{}",
        wrong.join("\n")
    );
}

/// A deviation that stopped deviating is a stale excuse: it would hide the next
/// drift of the same name.
#[test]
fn known_deviations_are_still_deviating() {
    for name in KNOWN_DEVIATIONS {
        let entry = security::registry()
            .lookup_global(name)
            .unwrap_or_else(|| panic!("{name}: no longer a registry global, drop the deviation"));
        assert!(
            spellings(entry.ru, entry.en).all(|spelling| !is_global_function(spelling)),
            "{name}: platform data now has it, drop the deviation"
        );
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn capability_entries_match_platform_surface() {
    let mut wrong = Vec::new();

    for entry in capability::registry().entries() {
        match entry.kind {
            capability::EntryKind::GlobalMethod => {
                for name in spellings(entry.ru, entry.en) {
                    if !is_global_function(name) {
                        wrong.push(format!("{name}: global method absent from platform data"));
                    }
                }
                if !entry.en.is_empty() && !global_function_pair_agrees(entry.ru, entry.en) {
                    wrong.push(format!(
                        "{} / {}: spellings name different global functions",
                        entry.ru, entry.en
                    ));
                }
            }
            // Spelling alone matches this kind, so at least one type must own
            // the name — otherwise every match is provably something else. A
            // same-named global function does not save the entry: the global
            // surface is declared by a separate `GlobalMethod` entry.
            capability::EntryKind::AnyReceiverMethod => {
                if !some_type_declares_pair(entry.ru, entry.en) {
                    wrong.push(format!(
                        "{} / {}: no platform type declares this method under both spellings",
                        entry.ru, entry.en
                    ));
                }
            }
            capability::EntryKind::Constructor => {
                for name in spellings(entry.ru, entry.en) {
                    if !is_constructible(name) {
                        wrong.push(format!("{name}: no constructor in platform data"));
                    }
                }
                if !entry.en.is_empty() && !type_pair_agrees(entry.ru, entry.en) {
                    wrong.push(format!(
                        "{} / {}: spellings name different types",
                        entry.ru, entry.en
                    ));
                }
            }
            capability::EntryKind::Type => {
                for name in spellings(entry.ru, entry.en) {
                    if !is_known_type(name) {
                        wrong.push(format!("{name}: type absent from platform data"));
                    }
                }
                if !entry.en.is_empty() && !type_pair_agrees(entry.ru, entry.en) {
                    wrong.push(format!(
                        "{} / {}: spellings name different types",
                        entry.ru, entry.en
                    ));
                }
            }
            capability::EntryKind::GlobalProperty => {}
        }
    }

    assert!(
        wrong.is_empty(),
        "capability registry disagrees with platform data:\n{}",
        wrong.join("\n")
    );
}
