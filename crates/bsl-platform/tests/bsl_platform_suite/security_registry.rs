use bsl_platform::security::{registry, Category, EntryKind, SecurityEntry};
use std::collections::HashSet;

#[test]
fn every_entry_has_russian_name() {
    for entry in registry().entries() {
        assert!(!entry.ru.is_empty(), "entry {entry:?} has empty `ru`",);
    }
}

#[test]
fn no_duplicate_lookup_keys() {
    let mut seen: HashSet<(String, EntryKind)> = HashSet::new();
    for entry in registry().entries() {
        let ru_key = entry.ru.to_lowercase();
        assert!(seen.insert((ru_key.clone(), entry.kind)), "duplicate RU key: {entry:?}",);
        if !entry.en.is_empty() {
            let en_key = entry.en.to_lowercase();
            if en_key != ru_key {
                assert!(seen.insert((en_key, entry.kind)), "duplicate EN key: {entry:?}",);
            }
        }
    }
}

#[test]
fn bilingual_lookup_round_trip() {
    let reg = registry();
    for entry in reg.entries() {
        let by_ru = lookup_by_kind(reg, entry.ru, entry.kind)
            .unwrap_or_else(|| panic!("RU lookup failed for {entry:?}"));
        assert_eq!(by_ru.ru, entry.ru, "RU lookup returned wrong entry");

        let by_ru_upper = lookup_by_kind(reg, &entry.ru.to_uppercase(), entry.kind);
        assert!(by_ru_upper.is_some(), "uppercase RU lookup failed for {entry:?}");

        if !entry.en.is_empty() {
            let by_en = lookup_by_kind(reg, entry.en, entry.kind)
                .unwrap_or_else(|| panic!("EN lookup failed for {entry:?}"));
            assert_eq!(by_en.ru, entry.ru, "EN lookup returned wrong entry");
            assert!(
                lookup_by_kind(reg, &entry.en.to_lowercase(), entry.kind).is_some(),
                "lowercase EN lookup failed for {entry:?}",
            );
        }
    }
}

#[test]
fn empty_and_unknown_lookups_return_none() {
    let reg = registry();
    assert!(reg.lookup_global("").is_none());
    assert!(reg.lookup_constructor("").is_none());
    assert!(reg.lookup_global("__definitely_not_a_method__").is_none());
    assert!(reg.lookup_constructor("__definitely_not_a_type__").is_none());
}

#[test]
fn lc_global_lookup_matches_base_for_ascii_case_insensitive_name() {
    let reg = registry();
    let base = reg.lookup_global("SetPrivilegedMode").expect("base lookup failed");
    let lc = reg.lookup_global_lc("setprivilegedmode").expect("_lc lookup failed");
    assert!(std::ptr::eq(base, lc), "_lc lookup returned a different entry");
}

#[test]
fn lc_global_lookup_matches_base_for_cyrillic_name() {
    let reg = registry();
    let base = reg.lookup_global("ОтменитьТранзакцию").expect("base lookup failed");
    let lc_name = "ОтменитьТранзакцию".to_lowercase();
    let lc = reg.lookup_global_lc(&lc_name).expect("_lc lookup failed");
    assert!(std::ptr::eq(base, lc), "_lc lookup returned a different entry");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "lookup_global_lc requires pre-lowercased input")]
fn lc_lookup_debug_asserts_on_mixed_case_input() {
    let _ = registry().lookup_global_lc("SetPrivilegedMode");
}

#[test]
fn lc_empty_string_returns_none() {
    let reg = registry();
    assert!(reg.lookup_global_lc("").is_none());
    assert!(reg.lookup_constructor_lc("").is_none());
}

#[test]
fn each_handler_category_has_entries() {
    let reg = registry();
    for category in [
        Category::FileSystem,
        Category::Internet,
        Category::ExternalApp,
        Category::OsUsers,
        Category::ExecuteExternalCode,
        Category::PrivilegedMode,
        Category::PrivilegedModeQuery,
        Category::SafeMode,
        Category::SafeModeQuery,
        Category::Logging,
        Category::Transaction,
    ] {
        assert!(!reg.entries_by_category(category).is_empty(), "no entries for {category:?}",);
    }
}

#[test]
fn constructors_are_fully_bilingual() {
    for entry in registry().entries() {
        if entry.kind == EntryKind::Constructor {
            assert!(!entry.en.is_empty(), "constructor entry {entry:?} is missing English alias",);
        }
    }
}

#[test]
fn mode_bool_role_only_on_mode_categories() {
    use bsl_platform::security::Role;
    for entry in registry().entries() {
        let mode_bool_polarity = entry.params.iter().find_map(|p| match p.role {
            Role::ModeBool { opens_unsafe_when } => Some(opens_unsafe_when),
            _ => None,
        });
        if let Some(opens_unsafe_when) = mode_bool_polarity {
            assert!(
                matches!(entry.category, Category::PrivilegedMode | Category::SafeMode),
                "ModeBool param on non-mode category: {entry:?}",
            );
            let expected_open_on_true = matches!(
                entry.ru,
                "УстановитьПривилегированныйРежим" | "УстановитьОтключениеБезопасногоРежима",
            );
            let expected_open_on_false = matches!(entry.ru, "УстановитьБезопасныйРежим");
            assert!(
                expected_open_on_true || expected_open_on_false,
                "ModeBool entry not in expected polarity table: {entry:?}",
            );
            if expected_open_on_true {
                assert!(opens_unsafe_when, "expected opens_unsafe_when=true for {entry:?}",);
            }
            if expected_open_on_false {
                assert!(!opens_unsafe_when, "expected opens_unsafe_when=false for {entry:?}",);
            }
        }
    }
}

#[test]
fn legacy_recognizer_parity() {
    let reg = registry();

    // `МенеджерФайловыхПотоков` / `FileStreamsManager` is missing on purpose:
    // the legacy list called it constructible, but the platform ships no
    // constructor for it — the manager is reached through the global property
    // `ФайловыеПотоки`. The entry could only ever have matched invalid code.

    for &name in &[
        "командасистемы",
        "system",
        "запуститьсистему",
        "runsystem",
        "запуститьприложение",
        "runapp",
        "начатьзапускприложения",
        "beginrunningapplication",
        "запуститьприложениеасинх",
        "runappasync",
    ] {
        assert!(
            reg.lookup_global(name).is_some(),
            "legacy is_external_app_method name not in registry: {name}",
        );
    }

    for &(owner, name) in &[
        ("файловаясистема", "запуститьпрограмму"),
        ("файловаясистемаклиент", "запуститьпрограмму"),
        ("файловаясистемаклиент", "открытьпроводник"),
        ("файловаясистемаклиент", "открытьфайл"),
        ("работасфайламиклиент", "открытьфайл"),
    ] {
        assert!(
            reg.lookup_module_method_lc(owner, name).is_some(),
            "library module method not in registry: {owner}.{name}",
        );
        assert!(
            reg.lookup_global(name).is_none(),
            "{name} is a library module method, not a global one",
        );
        assert!(
            reg.lookup_module_method_lc("записьxml", name).is_none(),
            "{name} matched a receiver outside its owners",
        );
    }

    for &name in &["пользователиос", "osusers"] {
        assert!(reg.lookup_global(name).is_some(), "missing OS-users name: {name}");
    }

    for &name in &[
        "file",
        "файл",
        "xbase",
        "htmlwriter",
        "записьhtml",
        "htmlreader",
        "чтениеhtml",
        "fastinfosetreader",
        "чтениеfastinfoset",
        "fastinfosetwriter",
        "записьfastinfoset",
        "xsltransform",
        "преобразованиеxsl",
        "zipfilewriter",
        "записьzipфайла",
        "zipfilereader",
        "чтениеzipфайла",
        "textreader",
        "чтениетекста",
        "textwriter",
        "записьтекста",
        "textextraction",
        "извлечениетекста",
        "binarydata",
        "двоичныеданные",
        "filestream",
        "файловыйпоток",
        "datawriter",
        "записьданных",
        "datareader",
        "чтениеданных",
    ] {
        assert!(
            reg.lookup_constructor(name).is_some(),
            "legacy is_file_system_type ctor not in registry: {name}",
        );
    }

    for &name in &[
        "значениевфайл",
        "valuetofile",
        "копироватьфайл",
        "copyfile",
        "filecopy",
        "объединитьфайлы",
        "mergefiles",
        "переместитьфайл",
        "movefile",
        "разделитьфайл",
        "splitfile",
        "создатькаталог",
        "createdirectory",
        "удалитьфайлы",
        "deletefiles",
        "каталогпрограммы",
        "bindir",
        "каталогвременныхфайлов",
        "tempfilesdir",
        "каталогдокументов",
        "documentsdir",
        "рабочийкаталогданныхпользователя",
        "userdataworkdir",
        "начатьподключениерасширенияработысфайлами",
        "beginattachingfilesystemextension",
        "начатьустановкурасширенияработысфайлами",
        "begininstallfilesystemextension",
        "установитьрасширениеработысфайлами",
        "installfilesystemextension",
        "установитьрасширениеработысфайламиасинх",
        "installfilesystemextensionasync",
        "подключитьрасширениеработысфайламиасинх",
        "attachfilesystemextensionasync",
        "каталогвременныхфайловасинх",
        "tempfilesdirasync",
        "каталогдокументовасинх",
        "documentsdirasync",
        "рабочийкаталогданныхпользователяасинх",
        "userdataworkdirasync",
        "копироватьфайласинх",
        "copyfileasync",
        "найтифайлыасинх",
        "findfilesasync",
        "начатькопированиефайла",
        "begincopyingfile",
        "начатьперемещениефайла",
        "beginmovingfile",
        "начатьпоискфайлов",
        "beginfindingfiles",
        "начатьсозданиедвоичныхданныхизфайла",
        "begincreatebinarydatafromfile",
        "начатьсозданиекаталога",
        "begincreatingdirectory",
        "начатьудалениефайлов",
        "begindeletingfiles",
        "переместитьфайласинх",
        "movefileasync",
        "создатьдвоичныеданныеизфайлаасинх",
        "createbinarydatafromfileasync",
        "создатькаталогасинх",
        "createdirectoryasync",
        "удалитьфайлыасинх",
        "deletefilesasync",
        "начатьполучениекаталогавременныхфайлов",
        "begingettingtempfilesdir",
        "начатьполучениекаталогадокументов",
        "begingettingdocumentsdir",
        "начатьполучениерабочегокаталогаданныхпользователя",
        "begingettinguserdataworkdir",
    ] {
        assert!(
            reg.lookup_global(name).is_some(),
            "legacy is_file_system_method (canonical) name not in registry: {name}",
        );
    }

    for &name in &[
        "ftpсоединение",
        "ftpconnection",
        "httpсоединение",
        "httpconnection",
        "wsопределения",
        "wsdefinitions",
        "wsпрокси",
        "wsproxy",
        "интернетпочтовыйпрофиль",
        "internetmailprofile",
        "интернетпочта",
        "internetmail",
        "почта",
        "mail",
        "httpзапрос",
        "httprequest",
        "интернетпрокси",
        "internetproxy",
    ] {
        assert!(
            reg.lookup_constructor(name).is_some(),
            "legacy internet ctor not in registry: {name}",
        );
    }

    for &name in &["записьжурналарегистрации", "writelogevent"] {
        assert!(reg.lookup_global(name).is_some(), "WriteLogEvent name not in registry: {name}",);
    }

    for &name in &[
        "установитьбезопасныйрежим",
        "setsafemode",
        "установитьотключениебезопасногорежима",
        "setsafemodedisabled",
        "безопасныйрежим",
        "safemode",
        "установитьпривилегированныйрежим",
        "setprivilegedmode",
        "привилегированныйрежим",
        "privilegedmode",
        "вычислить",
        "eval",
    ] {
        assert!(
            reg.lookup_global(name).is_some(),
            "legacy mode/eval method not in registry: {name}",
        );
    }
}

#[test]
fn legacy_recognizer_category_parity() {
    let reg = registry();

    let global_cases: &[(&str, &str, Category)] = &[
        ("командасистемы", "is_external_app_method", Category::ExternalApp),
        ("system", "is_external_app_method", Category::ExternalApp),
        ("запуститьсистему", "is_external_app_method", Category::ExternalApp),
        ("runsystem", "is_external_app_method", Category::ExternalApp),
        ("запуститьприложение", "is_external_app_method", Category::ExternalApp),
        ("runapp", "is_external_app_method", Category::ExternalApp),
        ("начатьзапускприложения", "is_external_app_method", Category::ExternalApp),
        ("beginrunningapplication", "is_external_app_method", Category::ExternalApp),
        ("запуститьприложениеасинх", "is_external_app_method", Category::ExternalApp),
        ("runappasync", "is_external_app_method", Category::ExternalApp),
        ("пользователиос", "is_os_users_method", Category::OsUsers),
        ("osusers", "is_os_users_method", Category::OsUsers),
        ("вычислить", "is_global_eval_call", Category::ExecuteExternalCode),
        ("eval", "is_global_eval_call", Category::ExecuteExternalCode),
        ("установитьбезопасныйрежим", "is_safe_mode_method", Category::SafeMode),
        ("setsafemode", "is_safe_mode_method", Category::SafeMode),
        ("установитьотключениебезопасногорежима", "is_safe_mode_method", Category::SafeMode),
        ("setsafemodedisabled", "is_safe_mode_method", Category::SafeMode),
        ("безопасныйрежим", "is_safe_mode_query", Category::SafeModeQuery),
        ("safemode", "is_safe_mode_query", Category::SafeModeQuery),
        ("установитьпривилегированныйрежим", "is_set_privileged_mode", Category::PrivilegedMode),
        ("setprivilegedmode", "is_set_privileged_mode", Category::PrivilegedMode),
        ("значениевфайл", "is_file_system_method", Category::FileSystem),
        ("valuetofile", "is_file_system_method", Category::FileSystem),
        ("копироватьфайл", "is_file_system_method", Category::FileSystem),
        ("copyfile", "is_file_system_method", Category::FileSystem),
        ("filecopy", "is_file_system_method", Category::FileSystem),
        ("объединитьфайлы", "is_file_system_method", Category::FileSystem),
        ("mergefiles", "is_file_system_method", Category::FileSystem),
        ("переместитьфайл", "is_file_system_method", Category::FileSystem),
        ("movefile", "is_file_system_method", Category::FileSystem),
        ("разделитьфайл", "is_file_system_method", Category::FileSystem),
        ("splitfile", "is_file_system_method", Category::FileSystem),
        ("создатькаталог", "is_file_system_method", Category::FileSystem),
        ("createdirectory", "is_file_system_method", Category::FileSystem),
        ("удалитьфайлы", "is_file_system_method", Category::FileSystem),
        ("deletefiles", "is_file_system_method", Category::FileSystem),
        ("каталогпрограммы", "is_file_system_method", Category::FileSystem),
        ("bindir", "is_file_system_method", Category::FileSystem),
        ("каталогвременныхфайлов", "is_file_system_method", Category::FileSystem),
        ("tempfilesdir", "is_file_system_method", Category::FileSystem),
        ("каталогдокументов", "is_file_system_method", Category::FileSystem),
        ("documentsdir", "is_file_system_method", Category::FileSystem),
        ("рабочийкаталогданныхпользователя", "is_file_system_method", Category::FileSystem),
        ("userdataworkdir", "is_file_system_method", Category::FileSystem),
        (
            "начатьподключениерасширенияработысфайлами",
            "is_file_system_method",
            Category::FileSystem,
        ),
        ("beginattachingfilesystemextension", "is_file_system_method", Category::FileSystem),
        ("начатьустановкурасширенияработысфайлами", "is_file_system_method", Category::FileSystem),
        ("begininstallfilesystemextension", "is_file_system_method", Category::FileSystem),
        ("установитьрасширениеработысфайлами", "is_file_system_method", Category::FileSystem),
        ("installfilesystemextension", "is_file_system_method", Category::FileSystem),
        ("установитьрасширениеработысфайламиасинх", "is_file_system_method", Category::FileSystem),
        ("installfilesystemextensionasync", "is_file_system_method", Category::FileSystem),
        ("подключитьрасширениеработысфайламиасинх", "is_file_system_method", Category::FileSystem),
        ("attachfilesystemextensionasync", "is_file_system_method", Category::FileSystem),
        ("каталогвременныхфайловасинх", "is_file_system_method", Category::FileSystem),
        ("tempfilesdirasync", "is_file_system_method", Category::FileSystem),
        ("каталогдокументовасинх", "is_file_system_method", Category::FileSystem),
        ("documentsdirasync", "is_file_system_method", Category::FileSystem),
        ("рабочийкаталогданныхпользователяасинх", "is_file_system_method", Category::FileSystem),
        ("userdataworkdirasync", "is_file_system_method", Category::FileSystem),
        ("копироватьфайласинх", "is_file_system_method", Category::FileSystem),
        ("copyfileasync", "is_file_system_method", Category::FileSystem),
        ("найтифайлыасинх", "is_file_system_method", Category::FileSystem),
        ("findfilesasync", "is_file_system_method", Category::FileSystem),
        ("начатькопированиефайла", "is_file_system_method", Category::FileSystem),
        ("begincopyingfile", "is_file_system_method", Category::FileSystem),
        ("начатьперемещениефайла", "is_file_system_method", Category::FileSystem),
        ("beginmovingfile", "is_file_system_method", Category::FileSystem),
        ("начатьпоискфайлов", "is_file_system_method", Category::FileSystem),
        ("beginfindingfiles", "is_file_system_method", Category::FileSystem),
        ("начатьсозданиедвоичныхданныхизфайла", "is_file_system_method", Category::FileSystem),
        ("begincreatebinarydatafromfile", "is_file_system_method", Category::FileSystem),
        ("начатьсозданиекаталога", "is_file_system_method", Category::FileSystem),
        ("begincreatingdirectory", "is_file_system_method", Category::FileSystem),
        ("начатьудалениефайлов", "is_file_system_method", Category::FileSystem),
        ("begindeletingfiles", "is_file_system_method", Category::FileSystem),
        ("переместитьфайласинх", "is_file_system_method", Category::FileSystem),
        ("movefileasync", "is_file_system_method", Category::FileSystem),
        ("создатьдвоичныеданныеизфайлаасинх", "is_file_system_method", Category::FileSystem),
        ("createbinarydatafromfileasync", "is_file_system_method", Category::FileSystem),
        ("создатькаталогасинх", "is_file_system_method", Category::FileSystem),
        ("createdirectoryasync", "is_file_system_method", Category::FileSystem),
        ("удалитьфайлыасинх", "is_file_system_method", Category::FileSystem),
        ("deletefilesasync", "is_file_system_method", Category::FileSystem),
        ("начатьполучениекаталогавременныхфайлов", "is_file_system_method", Category::FileSystem),
        ("begingettingtempfilesdir", "is_file_system_method", Category::FileSystem),
        ("начатьполучениекаталогадокументов", "is_file_system_method", Category::FileSystem),
        ("begingettingdocumentsdir", "is_file_system_method", Category::FileSystem),
        (
            "начатьполучениерабочегокаталогаданныхпользователя",
            "is_file_system_method",
            Category::FileSystem,
        ),
        ("begingettinguserdataworkdir", "is_file_system_method", Category::FileSystem),
    ];
    for (name, recognizer, expected) in global_cases {
        let entry = reg.lookup_global(name).unwrap_or_else(|| {
            panic!(
                "{recognizer}: registry has no global entry for {name:?} \
                 (run `cargo test legacy_recognizer_parity` first to localise)"
            )
        });
        assert!(
            std::mem::discriminant(&entry.category) == std::mem::discriminant(expected),
            "{recognizer}: name {name:?} has category {:?}, expected {expected:?} — \
             a category reassignment would silently drop detection",
            entry.category,
        );
    }

    let module_method_cases: &[(&str, &str, Category)] = &[
        ("файловаясистема", "запуститьпрограмму", Category::ExternalApp),
        ("файловаясистемаклиент", "открытьпроводник", Category::ExternalApp),
        ("файловаясистемаклиент", "открытьфайл", Category::ExternalApp),
        ("работасфайламиклиент", "открытьфайл", Category::ExternalApp),
    ];
    for (owner, name, expected) in module_method_cases {
        let entry = reg.lookup_module_method_lc(owner, name).unwrap_or_else(|| {
            panic!("registry has no module-method entry for {owner:?}.{name:?}")
        });
        assert!(
            std::mem::discriminant(&entry.category) == std::mem::discriminant(expected),
            "module method {owner:?}.{name:?} has category {:?}, expected {expected:?} — \
             a category reassignment would silently drop detection",
            entry.category,
        );
    }

    let constructor_cases: &[(&str, &str, Category)] = &[
        ("file", "is_file_system_type", Category::FileSystem),
        ("файл", "is_file_system_type", Category::FileSystem),
        ("xbase", "is_file_system_type", Category::FileSystem),
        ("htmlwriter", "is_file_system_type", Category::FileSystem),
        ("записьhtml", "is_file_system_type", Category::FileSystem),
        ("htmlreader", "is_file_system_type", Category::FileSystem),
        ("чтениеhtml", "is_file_system_type", Category::FileSystem),
        ("fastinfosetreader", "is_file_system_type", Category::FileSystem),
        ("чтениеfastinfoset", "is_file_system_type", Category::FileSystem),
        ("fastinfosetwriter", "is_file_system_type", Category::FileSystem),
        ("записьfastinfoset", "is_file_system_type", Category::FileSystem),
        ("xsltransform", "is_file_system_type", Category::FileSystem),
        ("преобразованиеxsl", "is_file_system_type", Category::FileSystem),
        ("zipfilewriter", "is_file_system_type", Category::FileSystem),
        ("записьzipфайла", "is_file_system_type", Category::FileSystem),
        ("zipfilereader", "is_file_system_type", Category::FileSystem),
        ("чтениеzipфайла", "is_file_system_type", Category::FileSystem),
        ("textreader", "is_file_system_type", Category::FileSystem),
        ("чтениетекста", "is_file_system_type", Category::FileSystem),
        ("textwriter", "is_file_system_type", Category::FileSystem),
        ("записьтекста", "is_file_system_type", Category::FileSystem),
        ("textextraction", "is_file_system_type", Category::FileSystem),
        ("извлечениетекста", "is_file_system_type", Category::FileSystem),
        ("binarydata", "is_file_system_type", Category::FileSystem),
        ("двоичныеданные", "is_file_system_type", Category::FileSystem),
        ("filestream", "is_file_system_type", Category::FileSystem),
        ("файловыйпоток", "is_file_system_type", Category::FileSystem),
        ("datawriter", "is_file_system_type", Category::FileSystem),
        ("записьданных", "is_file_system_type", Category::FileSystem),
        ("datareader", "is_file_system_type", Category::FileSystem),
        ("чтениеданных", "is_file_system_type", Category::FileSystem),
        ("ftpсоединение", "is_internet_constructor", Category::Internet),
        ("ftpconnection", "is_internet_constructor", Category::Internet),
        ("httpсоединение", "is_internet_constructor", Category::Internet),
        ("httpconnection", "is_internet_constructor", Category::Internet),
        ("wsопределения", "is_internet_constructor", Category::Internet),
        ("wsdefinitions", "is_internet_constructor", Category::Internet),
        ("wsпрокси", "is_internet_constructor", Category::Internet),
        ("wsproxy", "is_internet_constructor", Category::Internet),
        ("интернетпочтовыйпрофиль", "is_internet_constructor", Category::Internet),
        ("internetmailprofile", "is_internet_constructor", Category::Internet),
        ("интернетпочта", "is_internet_constructor", Category::Internet),
        ("internetmail", "is_internet_constructor", Category::Internet),
        ("почта", "is_internet_constructor", Category::Internet),
        ("mail", "is_internet_constructor", Category::Internet),
        ("httpзапрос", "is_internet_constructor", Category::Internet),
        ("httprequest", "is_internet_constructor", Category::Internet),
        ("интернетпрокси", "is_internet_constructor", Category::Internet),
        ("internetproxy", "is_internet_constructor", Category::Internet),
    ];
    for (name, recognizer, expected) in constructor_cases {
        let entry = reg.lookup_constructor(name).unwrap_or_else(|| {
            panic!("{recognizer}: registry has no constructor entry for {name:?}")
        });
        assert!(
            std::mem::discriminant(&entry.category) == std::mem::discriminant(expected),
            "{recognizer}: ctor {name:?} has category {:?}, expected {expected:?}",
            entry.category,
        );
    }
}

fn lookup_by_kind(
    reg: &bsl_platform::security::SecurityRegistry,
    name: &str,
    kind: EntryKind,
) -> Option<&'static SecurityEntry> {
    match kind {
        EntryKind::GlobalMethod => reg.lookup_global(name),
        EntryKind::Constructor => reg.lookup_constructor(name),
        EntryKind::ModuleMethod { owners } => {
            reg.lookup_module_method(owners.first().expect("module method without an owner"), name)
        }
    }
}
