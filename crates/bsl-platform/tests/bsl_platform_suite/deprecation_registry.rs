use bsl_platform::deprecation::{
    registry, CompatibilityBucket, DeprecationEntry, DeprecationRegistry, DisplayKind, ElementKind,
    LifecycleGroup, Lookup, OwnerType, Replacement,
};
use std::collections::HashSet;

const STR_FIND_REPLACEMENT: Replacement = Replacement { ru: "СтрНайти", en: "StrFind" };
const OPEN_FORM_REPLACEMENT: Replacement =
    Replacement { ru: "ОткрытьФорму", en: "OpenForm" };
const CLIENT_APPLICATION_FORM: OwnerType = OwnerType {
    ru: "ФормаКлиентскогоПриложения",
    en: "ClientApplicationForm",
};

// Fixture-only entries for API coverage. Todo 9 owns production population.
const FIXTURE_ENTRIES: &[DeprecationEntry] = &[
    DeprecationEntry {
        ru: "Найти",
        en: "Find",
        element_kind: ElementKind::GlobalMethod,
        owner: None,
        group: LifecycleGroup::StringSearch,
        replacement: Some(STR_FIND_REPLACEMENT),
        compatibility: CompatibilityBucket::CompatibilityMode8_3_6,
        display: DisplayKind::Function,
    },
    DeprecationEntry {
        ru: "ПолучитьФорму",
        en: "GetForm",
        element_kind: ElementKind::Method,
        owner: Some(CLIENT_APPLICATION_FORM),
        group: LifecycleGroup::ManagedForm,
        replacement: Some(OPEN_FORM_REPLACEMENT),
        compatibility: CompatibilityBucket::CompatibilityMode8_3_17,
        display: DisplayKind::Method,
    },
    DeprecationEntry {
        ru: "УправляемаяФорма",
        en: "ManagedForm",
        element_kind: ElementKind::Type,
        owner: None,
        group: LifecycleGroup::ManagedForm,
        replacement: None,
        compatibility: CompatibilityBucket::Any,
        display: DisplayKind::Type,
    },
];

const DUPLICATE_ENTRIES: &[DeprecationEntry] = &[
    FIXTURE_ENTRIES[0],
    DeprecationEntry {
        ru: "ДубликатНайти",
        en: "find",
        element_kind: ElementKind::GlobalMethod,
        owner: None,
        group: LifecycleGroup::StringSearch,
        replacement: Some(STR_FIND_REPLACEMENT),
        compatibility: CompatibilityBucket::CompatibilityMode8_3_6,
        display: DisplayKind::Function,
    },
];

fn fixture_registry() -> DeprecationRegistry {
    DeprecationRegistry::from_entries(FIXTURE_ENTRIES)
}

#[test]
fn deprecation_entries_expose_typed_lifecycle_fields() {
    let reg = fixture_registry();

    let entry = reg.lookup(Lookup::global_method("Найти")).expect("global method lookup");

    assert_eq!(entry.element_kind, ElementKind::GlobalMethod);
    assert_eq!(entry.owner, None);
    assert_eq!(entry.group, LifecycleGroup::StringSearch);
    assert_eq!(entry.replacement, Some(STR_FIND_REPLACEMENT));
    assert_eq!(entry.compatibility, CompatibilityBucket::CompatibilityMode8_3_6);
    assert_eq!(entry.display, DisplayKind::Function);
}

#[test]
fn deprecation_lookup_matches_optional_owner_type() {
    let reg = fixture_registry();

    let method = reg
        .lookup(Lookup::method("ФормаКлиентскогоПриложения", "ПолучитьФорму"))
        .expect("owned method lookup");

    assert_eq!(method.owner, Some(CLIENT_APPLICATION_FORM));
    assert_eq!(method.display, DisplayKind::Method);
    assert!(reg.lookup(Lookup::global_method("ПолучитьФорму")).is_none());
    assert!(reg.lookup(Lookup::method("ДругойТип", "ПолучитьФорму")).is_none());
}

#[test]
fn deprecation_bilingual_lookup_folds_name_and_owner_aliases() {
    let reg = fixture_registry();

    let by_ru = reg
        .lookup(Lookup::method("ФормаКлиентскогоПриложения", "ПолучитьФорму"))
        .expect("RU owned method lookup");
    let by_en = reg
        .lookup(Lookup::method("ClientApplicationForm", "GetForm"))
        .expect("EN owned method lookup");
    let by_cross_alias = reg
        .lookup(Lookup::method("ClientApplicationForm", "ПолучитьФорму"))
        .expect("cross-alias owned method lookup");

    assert!(std::ptr::eq(by_ru, by_en));
    assert!(std::ptr::eq(by_ru, by_cross_alias));

    let owner_lc = "ФормаКлиентскогоПриложения".to_lowercase();
    let name_lc = "ПолучитьФорму".to_lowercase();
    let by_lc = reg
        .lookup_lc(Lookup::new(ElementKind::Method, Some(&owner_lc), &name_lc))
        .expect("folded owned method lookup");
    assert!(std::ptr::eq(by_ru, by_lc));
}

#[test]
fn deprecation_registry_contains_owner_qualified_http_connection_get() {
    let reg = registry();

    let by_ru = reg
        .lookup(Lookup::method("HTTPСоединение", "Получить"))
        .expect("RU owned HTTPConnection.Get lookup");
    let by_en = reg
        .lookup(Lookup::method("HTTPConnection", "Get"))
        .expect("EN owned HTTPConnection.Get lookup");

    assert!(std::ptr::eq(by_ru, by_en));
    assert_eq!(by_ru.element_kind, ElementKind::Method);
    assert_eq!(by_ru.owner, Some(OwnerType { ru: "HTTPСоединение", en: "HTTPConnection" }));
    assert_eq!(by_ru.display, DisplayKind::Method);
    assert_eq!(by_ru.replacement, Some(Replacement { ru: "ПолучитьАсинх", en: "GetAsync" }));
    assert_eq!(by_ru.compatibility, CompatibilityBucket::CompatibilityMode8_3_17);
}

#[test]
fn deprecation_registry_contains_owner_qualified_internet_proxy_credentials() {
    let reg = registry();

    let password_ru = reg
        .lookup(Lookup::property("ИнтернетПрокси", "Пароль"))
        .expect("RU owned InternetProxy.Password lookup");
    let password_en = reg
        .lookup(Lookup::property("InternetProxy", "Password"))
        .expect("EN owned InternetProxy.Password lookup");
    let user_ru = reg
        .lookup(Lookup::property("ИнтернетПрокси", "Пользователь"))
        .expect("RU owned InternetProxy.User lookup");
    let user_en = reg
        .lookup(Lookup::property("InternetProxy", "User"))
        .expect("EN owned InternetProxy.User lookup");

    assert!(std::ptr::eq(password_ru, password_en));
    assert!(std::ptr::eq(user_ru, user_en));
    assert_eq!(password_ru.element_kind, ElementKind::Property);
    assert_eq!(password_ru.owner, Some(OwnerType { ru: "ИнтернетПрокси", en: "InternetProxy" }));
    assert_eq!(password_ru.display, DisplayKind::Property);
    assert_eq!(password_ru.replacement, Some(Replacement { ru: "Пароль", en: "Password" }));
    assert_eq!(password_ru.compatibility, CompatibilityBucket::CompatibilityMode8_3_17);
    assert_eq!(user_ru.element_kind, ElementKind::Property);
    assert_eq!(user_ru.owner, Some(OwnerType { ru: "ИнтернетПрокси", en: "InternetProxy" }));
    assert_eq!(user_ru.display, DisplayKind::Property);
    assert_eq!(user_ru.replacement, Some(Replacement { ru: "Пользователь", en: "User" }));
    assert_eq!(user_ru.compatibility, CompatibilityBucket::CompatibilityMode8_3_17);
}

#[test]
fn deprecation_group_iteration_lists_populated_groups_once() {
    let reg = fixture_registry();

    let groups = reg.groups();

    assert!(groups.contains(&LifecycleGroup::StringSearch));
    assert!(groups.contains(&LifecycleGroup::ManagedForm));

    let mut unique = groups.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(groups.len(), unique.len(), "groups must be unique");

    for &group in groups {
        assert!(!reg.entries_by_group(group).is_empty(), "empty group: {group:?}");
    }
}

#[test]
fn deprecation_group_lookup_returns_only_requested_group() {
    let reg = fixture_registry();

    let string_search = reg.entries_by_group(LifecycleGroup::StringSearch);
    let managed_form = reg.entries_by_group(LifecycleGroup::ManagedForm);

    assert_eq!(string_search.len(), 1);
    assert_eq!(managed_form.len(), 2);
    assert!(string_search.iter().all(|entry| entry.group == LifecycleGroup::StringSearch));
    assert!(managed_form.iter().all(|entry| entry.group == LifecycleGroup::ManagedForm));
}

#[test]
fn deprecation_registry_fixture_has_no_duplicate_lookup_keys() {
    let mut seen: HashSet<(ElementKind, Option<String>, String)> = HashSet::new();

    for entry in FIXTURE_ENTRIES {
        let name_keys = [entry.ru.to_lowercase(), entry.en.to_lowercase()];
        let owner_keys = match entry.owner {
            Some(owner) => vec![Some(owner.ru.to_lowercase()), Some(owner.en.to_lowercase())],
            None => vec![None],
        };

        for name_key in name_keys.into_iter().filter(|key| !key.is_empty()) {
            for owner_key in &owner_keys {
                assert!(
                    seen.insert((entry.element_kind, owner_key.clone(), name_key.clone())),
                    "duplicate lookup key for {entry:?}",
                );
            }
        }
    }
}

#[test]
fn deprecation_empty_and_unknown_lookups_return_none() {
    let reg = fixture_registry();

    assert!(reg.lookup(Lookup::global_method("")).is_none());
    assert!(reg.lookup_lc(Lookup::global_method("")).is_none());
    assert!(reg.lookup(Lookup::global_method("__definitely_not_a_method__")).is_none());
    assert!(reg.lookup(Lookup::new(ElementKind::Property, None, "Найти")).is_none());
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "lookup_lc requires pre-lowercased input")]
fn deprecation_lc_lookup_debug_asserts_on_mixed_case_input() {
    let _ = fixture_registry().lookup_lc(Lookup::global_method("Find"));
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "duplicate deprecation-registry key")]
fn deprecation_registry_debug_asserts_on_duplicate_lookup_key() {
    let _ = DeprecationRegistry::from_entries(DUPLICATE_ENTRIES);
}
