use bsl_platform::capability::{
    registry, CapabilityEntry, CapabilityRegistry, Category, EntryKind, Replacement,
};
use std::collections::HashSet;

type CapabilityFact = (Category, EntryKind, &'static str, &'static str);

fn bilingual_entry(reg: &CapabilityRegistry, fact: CapabilityFact) -> &'static CapabilityEntry {
    let (category, kind, ru, en) = fact;
    let by_ru = reg.lookup(category, kind, ru).unwrap();
    let by_en = reg.lookup(category, kind, en).unwrap();

    assert!(std::ptr::eq(by_ru, by_en));
    by_ru
}

#[test]
fn capability_registry_entries_have_no_duplicate_lookup_keys_per_category_and_kind() {
    let reg = registry();
    let mut seen: HashSet<(Category, EntryKind, String)> = HashSet::new();

    for entry in reg.entries() {
        let ru_key = entry.ru.to_lowercase();
        assert!(
            seen.insert((entry.category, entry.kind, ru_key.clone())),
            "duplicate RU key: {entry:?}",
        );
        if !entry.en.is_empty() {
            let en_key = entry.en.to_lowercase();
            if en_key != ru_key {
                assert!(
                    seen.insert((entry.category, entry.kind, en_key)),
                    "duplicate EN key: {entry:?}",
                );
            }
        }
    }
}

#[test]
fn capability_category_iteration_lists_populated_categories_once() {
    let reg = registry();

    let categories = reg.categories();

    assert!(categories.contains(&Category::ModalWindow));
    assert!(categories.contains(&Category::SynchronousCall));
    assert!(categories.contains(&Category::AsyncCall));
    assert!(categories.contains(&Category::SystemInformation));
    assert!(categories.contains(&Category::UnixUnavailableObject));
    assert!(categories.contains(&Category::TemporaryFilesDirectory));
    assert!(categories.contains(&Category::FormDataToValue));
    assert!(categories.contains(&Category::GetForm));

    let mut unique = categories.to_vec();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(categories.len(), unique.len(), "categories must be unique");

    for &category in categories {
        assert!(!reg.entries_by_category(category).is_empty(), "empty category: {category:?}");
    }
}

#[test]
fn capability_category_lookup_returns_only_requested_category() {
    let reg = registry();

    let modal = reg.entries_by_category(Category::ModalWindow);
    let sync = reg.entries_by_category(Category::SynchronousCall);

    assert!(!modal.is_empty(), "modal fixture category must be populated");
    assert!(!sync.is_empty(), "sync fixture category must be populated");
    assert!(modal.iter().all(|entry| entry.category == Category::ModalWindow));
    assert!(sync.iter().all(|entry| entry.category == Category::SynchronousCall));
}

#[test]
fn capability_bilingual_lookup_folds_ru_and_en_by_category_and_kind() {
    let reg = registry();

    let by_ru = reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "Вопрос")
        .expect("RU modal lookup must resolve");
    let by_ru_upper = reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "ВОПРОС")
        .expect("uppercase RU modal lookup must resolve");
    let by_en = reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "DoQueryBox")
        .expect("EN modal lookup must resolve");
    let by_en_lower = reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "doquerybox")
        .expect("lowercase EN modal lookup must resolve");

    assert!(std::ptr::eq(by_ru, by_ru_upper));
    assert!(std::ptr::eq(by_ru, by_en));
    assert!(std::ptr::eq(by_ru, by_en_lower));
}

#[test]
fn capability_folded_lc_lookup_matches_base_lookup_for_ru_and_en() {
    let reg = registry();

    let ru = reg
        .lookup(Category::AsyncCall, EntryKind::GlobalMethod, "ПоказатьВопрос")
        .expect("base RU async lookup must resolve");
    let ru_lc_name = "ПоказатьВопрос".to_lowercase();
    let ru_lc = reg
        .lookup_lc(Category::AsyncCall, EntryKind::GlobalMethod, &ru_lc_name)
        .expect("folded RU async lookup must resolve");
    assert!(std::ptr::eq(ru, ru_lc));

    let en = reg
        .lookup(Category::AsyncCall, EntryKind::GlobalMethod, "ShowQueryBox")
        .expect("base EN async lookup must resolve");
    let en_lc = reg
        .lookup_lc(Category::AsyncCall, EntryKind::GlobalMethod, "showquerybox")
        .expect("folded EN async lookup must resolve");
    assert!(std::ptr::eq(en, en_lc));
}

#[test]
fn capability_lookup_key_is_scoped_by_category() {
    let reg = registry();

    let modal = reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "Вопрос")
        .expect("modal lookup must resolve");
    let sync = reg
        .lookup(Category::SynchronousCall, EntryKind::GlobalMethod, "Вопрос")
        .expect("sync lookup must resolve");

    assert_eq!(modal.ru, sync.ru);
    assert_eq!(modal.en, sync.en);
    assert_ne!(modal.category, sync.category);
}

#[test]
fn capability_platform_ui_categories_cover_current_hardcoded_table_sizes() {
    let reg = registry();

    assert_eq!(reg.entries_by_category(Category::ModalWindow).len(), 12);
    assert_eq!(reg.entries_by_category(Category::SynchronousCall).len(), 26);
    assert_eq!(reg.entries_by_category(Category::AsyncCall).len(), 25);
}

#[test]
fn capability_modal_only_lookup_selects_ru_and_en_replacement() {
    let reg = registry();

    let by_ru = reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "Предупреждение")
        .expect("RU modal lookup must resolve");
    let by_en = reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "DoMessageBox")
        .expect("EN modal lookup must resolve");

    assert!(std::ptr::eq(by_ru, by_en));
    assert_eq!(
        by_ru.replacement,
        Some(Replacement {
            ru: "ПоказатьПредупреждение", en: "ShowMessageBox"
        })
    );
}

#[test]
fn capability_sync_only_lookup_keeps_delete_files_replacement() {
    let reg = registry();

    let by_ru = reg
        .lookup(Category::SynchronousCall, EntryKind::GlobalMethod, "УдалитьФайлы")
        .expect("RU sync lookup must resolve");
    let by_en = reg
        .lookup(Category::SynchronousCall, EntryKind::GlobalMethod, "DeleteFiles")
        .expect("EN sync lookup must resolve");

    assert!(std::ptr::eq(by_ru, by_en));
    assert_eq!(by_ru.ru, "УдалитьФайлы");
    assert_eq!(by_ru.en, "DeleteFiles");
    assert_eq!(
        by_ru.replacement,
        Some(Replacement {
            ru: "НачатьУдалениеФайлов", en: "BeginDeletingFiles"
        })
    );
}

#[test]
fn capability_async_begin_lookup_keeps_delete_files_begin_entry() {
    let reg = registry();

    let by_ru = reg
        .lookup(Category::AsyncCall, EntryKind::GlobalMethod, "НачатьУдалениеФайлов")
        .expect("RU async begin lookup must resolve");
    let by_en = reg
        .lookup(Category::AsyncCall, EntryKind::GlobalMethod, "BeginDeletingFiles")
        .expect("EN async begin lookup must resolve");

    assert!(std::ptr::eq(by_ru, by_en));
    assert_eq!(by_ru.replacement, None);
}

#[test]
fn capability_system_information_lookup_covers_ru_and_en_types() {
    let reg = registry();

    bilingual_entry(
        reg,
        (Category::SystemInformation, EntryKind::Type, "СистемнаяИнформация", "SystemInfo"),
    );
}

#[test]
fn capability_unix_unavailable_lookup_covers_current_ru_and_en_types() {
    let reg = registry();

    for fact in [
        (Category::UnixUnavailableObject, EntryKind::Type, "COMОбъект", "COMObject"),
        (Category::UnixUnavailableObject, EntryKind::Type, "Почта", "Mail"),
    ] {
        let entry = bilingual_entry(reg, fact);
        assert_eq!((entry.ru, entry.en), (fact.2, fact.3));
    }
}

#[test]
fn capability_temp_files_dir_lookup_is_global_method_only() {
    let reg = registry();

    bilingual_entry(
        reg,
        (
            Category::TemporaryFilesDirectory,
            EntryKind::GlobalMethod,
            "КаталогВременныхФайлов",
            "TempFilesDir",
        ),
    );

    assert!(reg
        .lookup(Category::TemporaryFilesDirectory, EntryKind::AnyReceiverMethod, "TempFilesDir")
        .is_none());
}

#[test]
fn capability_form_data_to_value_and_get_form_lookup_covers_declared_surfaces() {
    let reg = registry();

    for (category, kind, ru, en) in [
        (
            Category::FormDataToValue,
            EntryKind::GlobalMethod,
            "ДанныеФормыВЗначение",
            "FormDataToValue",
        ),
        (Category::GetForm, EntryKind::GlobalMethod, "ПолучитьФорму", "GetForm"),
        (Category::GetForm, EntryKind::AnyReceiverMethod, "ПолучитьФорму", "GetForm"),
    ] {
        let entry = bilingual_entry(reg, (category, kind, ru, en));
        assert_eq!(entry.replacement, None);
    }

    // `ДанныеФормыВЗначение` belongs to the global context alone: no platform
    // type declares it, so a qualified call names something else entirely.
    assert!(reg
        .lookup(Category::FormDataToValue, EntryKind::AnyReceiverMethod, "ДанныеФормыВЗначение")
        .is_none());
}

#[test]
fn capability_pure_membership_excludes_security_and_control_flow_categories() {
    let reg = registry();

    for category in [
        Category::SystemInformation,
        Category::UnixUnavailableObject,
        Category::TemporaryFilesDirectory,
        Category::FormDataToValue,
        Category::GetForm,
    ] {
        assert!(reg.lookup(category, EntryKind::GlobalMethod, "FileCopy").is_none());
        assert!(reg.lookup(category, EntryKind::GlobalMethod, "ValueToFile").is_none());
        assert!(reg.lookup(category, EntryKind::Type, "HTTPConnection").is_none());
        assert!(reg.lookup(category, EntryKind::Type, "InternetMail").is_none());
        assert!(reg.lookup(category, EntryKind::GlobalMethod, "Eval").is_none());
        assert!(reg.lookup(category, EntryKind::GlobalMethod, "Return").is_none());
        assert!(reg.lookup(category, EntryKind::GlobalMethod, "Возврат").is_none());
    }
}

#[test]
fn capability_duplicate_modal_and_sync_names_are_category_scoped() {
    let reg = registry();

    let modal = reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "ПоместитьФайл")
        .expect("modal PutFile lookup must resolve");
    let sync = reg
        .lookup(Category::SynchronousCall, EntryKind::GlobalMethod, "ПоместитьФайл")
        .expect("sync PutFile lookup must resolve");

    assert_eq!(modal.ru, sync.ru);
    assert_eq!(modal.en, sync.en);
    assert_eq!(
        modal.replacement,
        Some(Replacement { ru: "НачатьПомещениеФайла", en: "BeginPutFile" })
    );
    assert_eq!(modal.replacement, sync.replacement);
    assert_ne!(modal.category, sync.category);
}

#[test]
fn capability_empty_and_unknown_lookups_return_none() {
    let reg = registry();

    assert!(reg.lookup(Category::ModalWindow, EntryKind::GlobalMethod, "").is_none());
    assert!(reg.lookup_lc(Category::ModalWindow, EntryKind::GlobalMethod, "").is_none());
    assert!(reg
        .lookup(Category::ModalWindow, EntryKind::GlobalMethod, "__definitely_not_a_method__")
        .is_none());
    assert!(reg.lookup(Category::ModalWindow, EntryKind::Type, "Вопрос").is_none());
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "lookup_lc requires pre-lowercased input")]
fn capability_lc_lookup_debug_asserts_on_mixed_case_input() {
    let _ = registry().lookup_lc(Category::ModalWindow, EntryKind::GlobalMethod, "DoQueryBox");
}
