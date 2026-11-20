use super::*;

fn deprecated(host: &AnalysisHost, file: FileId) -> Vec<ide::Diagnostic> {
    host.analysis()
        .diagnostics(file, &DiagnosticsConfig::all_enabled())
        .into_iter()
        .filter(|diagnostic| diagnostic.code == DiagnosticCode::DeprecatedPlatformApi)
        .collect()
}

fn assert_replacements(diagnostics: &[ide::Diagnostic], names: &[&str], applicable: bool) {
    assert_eq!(diagnostics.len(), names.len(), "{diagnostics:?}");
    // The message for a replacement proven absent names it only to say it is missing.
    for (diagnostic, name) in diagnostics.iter().zip(names) {
        let recommends = diagnostic.message.contains(name)
            && !diagnostic.message.contains("в этой конфигурации нет");
        assert_eq!(recommends, applicable, "{diagnostic:?}");
    }
}

pub(super) fn library(root: &Path, name: &str, source: &str, flags: &str) -> PathBuf {
    let xml = format!(
        r#"<MetaDataObject><CommonModule uuid="00000000-0000-0000-0000-000000000005"><Properties><Name>{name}</Name>{flags}</Properties></CommonModule></MetaDataObject>"#
    );
    assert!(
        bsl_metadata::parse_common_module_from_text(&xml).is_some(),
        "fixture common module must parse: {xml}"
    );
    write(root, &format!("CommonModules/{name}.xml"), &xml);
    let path = format!("CommonModules/{name}/Ext/Module.bsl");
    write(root, &path, source);
    root.join(path)
}

#[test]
fn replacement_find_uses_the_selected_owner_for_message_and_fix() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Процедура Проверить()\n    А = Найти(\"a\", \"b\");\n    B = Find(\"a\", \"b\");\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    let project = project(root);
    let (host, _, ids) = host(&project, &[(legacy, code), (modern, code)]);
    for (file, applicable) in [(ids[0], false), (ids[1], true)] {
        let diagnostics = deprecated(&host, file);
        assert_replacements(&diagnostics, &["СтрНайти", "StrFind"], applicable);
        assert!(
            diagnostics.iter().all(|diagnostic| diagnostic.fixes.len() == usize::from(applicable)),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn replacement_unknown_owner_compatibility_is_not_a_dont_use_mode() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Процедура Проверить()\n    А = Найти(\"a\", \"b\");\n    B = Find(\"a\", \"b\");\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    let project = project(root);
    let (mut host, vfs, ids) = host(&project, &[(legacy, code), (modern, code)]);
    let modern_header = root.join("modern/Configuration.xml");
    let legacy_header = root.join("legacy/Configuration.xml");
    fs::write(
        &modern_header,
        fs::read_to_string(&modern_header).unwrap().replace("Version8_3_27", "UnsupportedMode"),
    )
    .unwrap();
    fs::write(
        &legacy_header,
        fs::read_to_string(&legacy_header).unwrap().replace("Version8_2_16", ""),
    )
    .unwrap();
    host.refresh_metadata_substrate(&vfs, &[modern_header, legacy_header]);
    for file in ids {
        let diagnostics = deprecated(&host, file);
        assert_replacements(&diagnostics, &["СтрНайти", "StrFind"], false);
        assert!(
            diagnostics.iter().all(|diagnostic| diagnostic.fixes.is_empty()),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn replacement_explicit_dont_use_mode_keeps_available_replacements() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "DontUse");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "НеИспользовать");
    let code = "Процедура Проверить()\n    А = Найти(\"a\", \"b\");\n    B = Find(\"a\", \"b\");\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    let project = project(root);
    let (host, _, ids) = host(&project, &[(legacy, code), (modern, code)]);
    for file in ids {
        let diagnostics = deprecated(&host, file);
        assert_replacements(&diagnostics, &["СтрНайти", "StrFind"], true);
        assert!(
            diagnostics.iter().all(|diagnostic| diagnostic.fixes.len() == 1),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn replacement_session_date_uses_owner_runtime_and_method_directive() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let ordinary =
        "Процедура Проверить()\n    А = ТекущаяДата();\n    B = CurrentDate();\nКонецПроцедуры";
    let client = format!("&НаКлиенте\n{ordinary}");
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", ordinary);
    external(&root.join("modern-external"), "ModernProcessor", "");
    let modern = managed_form(&root.join("modern-external"), "ModernProcessor", &client);
    let project = project(root);
    let (host, _, ids) = host(&project, &[(legacy, ordinary), (modern, &client)]);
    for (file, applicable) in [(ids[0], true), (ids[1], false)] {
        let diagnostics = deprecated(&host, file);
        assert_replacements(&diagnostics, &["ТекущаяДатаСеанса", "CurrentSessionDate"], applicable);
        assert!(
            diagnostics.iter().all(|diagnostic| diagnostic.fixes.len() == usize::from(applicable)),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn replacement_error_processing_is_not_hidden_by_an_unattested_compatibility_threshold() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Процедура Проверить()\n    А = ПодробноеПредставлениеОшибки(ИнформацияОбОшибке());\n    B = DetailErrorDescription(ErrorInfo());\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    let modern_code = format!("&НаСервере\n{code}");
    let modern = external(&root.join("modern-external"), "ModernProcessor", &modern_code);
    let project = project(root);
    let (host, _, ids) = host(&project, &[(legacy, code), (modern, &modern_code)]);
    for file in ids {
        assert_replacements(
            &deprecated(&host, file),
            &["ОбработкаОшибок.", "ErrorProcessing."],
            true,
        );
    }
}

#[test]
fn replacement_error_processing_respects_explicit_minimum_platform() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "&НаСервере\nПроцедура Проверить()\n    А = ПодробноеПредставлениеОшибки(ИнформацияОбОшибке());\n    B = DetailErrorDescription(ErrorInfo());\nКонецПроцедуры";
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    external(&root.join("legacy-external"), "LegacyProcessor", "");
    let _ = project(root);
    let path = root.join("bsl-analyzer.toml");
    fs::write(
        &path,
        format!("min_platform_version = \"8.3.16\"\n{}", fs::read_to_string(&path).unwrap()),
    )
    .unwrap();
    let project = Project::new(root).unwrap();
    let (host, _, ids) = host(&project, &[(modern, code)]);
    assert_replacements(
        &deprecated(&host, ids[0]),
        &["ОбработкаОшибок.", "ErrorProcessing."],
        false,
    );
}

#[test]
fn replacement_bsp_requires_an_export_in_the_selected_owner() {
    for (name, method, call, replacement) in [
        (
            "ОбщегоНазначения",
            "СообщитьПользователю",
            "Сообщить",
            "ОбщегоНазначения.СообщитьПользователю",
        ),
        ("CommonUse", "MessageToUser", "Message", "CommonUse.MessageToUser"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
        configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
        let library_source = format!("Процедура {method}(Текст) Экспорт\nКонецПроцедуры");
        let library = library(
            &root.join("modern"),
            name,
            &library_source,
            "<Server>true</Server><ClientOrdinaryApplication>true</ClientOrdinaryApplication>",
        );
        let code =
            format!("&НаСервере\nПроцедура Проверить()\n    {call}(\"text\");\nКонецПроцедуры");
        let legacy = external(&root.join("legacy-external"), "LegacyProcessor", &code);
        let modern = external(&root.join("modern-external"), "ModernProcessor", &code);
        let project = project(root);
        let (host, _, ids) =
            host(&project, &[(legacy, &code), (modern, &code), (library, &library_source)]);
        assert_replacements(&deprecated(&host, ids[0]), &[replacement], false);
        assert_replacements(&deprecated(&host, ids[1]), &[replacement], true);
    }
}

#[test]
fn replacement_bsp_rejects_private_wrong_environment_guard_and_shadow() {
    for (source, flags, parameter) in [
        ("Процедура СообщитьПользователю(Текст)\nКонецПроцедуры", "<Server>true</Server>", ""),
        ("Процедура СообщитьПользователю(Текст) Экспорт\nКонецПроцедуры", "<ClientManagedApplication>true</ClientManagedApplication>", ""),
        ("#Если Клиент Тогда\nПроцедура СообщитьПользователю(Текст) Экспорт\nКонецПроцедуры\n#КонецЕсли", "<Server>true</Server><ClientManagedApplication>true</ClientManagedApplication>", ""),
        ("#Если НеизвестныйСимвол Тогда\nПроцедура СообщитьПользователю(Текст) Экспорт\nКонецПроцедуры\n#КонецЕсли", "<Server>true</Server>", ""),
        ("Процедура СообщитьПользователю(Текст) Экспорт\nКонецПроцедуры", "<Server>true</Server>", "ОбщегоНазначения"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
        configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
        let library = library(&root.join("modern"), "ОбщегоНазначения", source, flags);
        let code = format!("&НаСервере\nПроцедура Проверить({parameter})\n    Сообщить(\"text\");\nКонецПроцедуры");
        let modern = external(&root.join("modern-external"), "ModernProcessor", &code);
        external(&root.join("legacy-external"), "LegacyProcessor", "");
        let project = project(root);
        let (host, _, ids) = host(&project, &[(modern, &code), (library, source)]);
        assert_replacements(&deprecated(&host, ids[0]), &["ОбщегоНазначения.СообщитьПользователю"], false);
    }
}

#[test]
fn replacement_unknown_owner_does_not_inherit_main_compatibility_or_environment() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Процедура Проверить()\n    А = Найти(\"a\", \"b\");\n    B = CurrentDate();\nКонецПроцедуры";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    external(&root.join("modern-external"), "ModernProcessor", "");
    let project = project(root);
    let (mut host, vfs, ids) = host(&project, &[(legacy, code)]);
    write(&root.join("legacy"), "Configuration.xml", "<broken");
    host.refresh_metadata_substrate(&vfs, &[root.join("legacy/Configuration.xml")]);
    let diagnostics = deprecated(&host, ids[0]);
    assert_replacements(&diagnostics, &["СтрНайти", "CurrentSessionDate"], false);
    assert!(diagnostics.iter().all(|diagnostic| diagnostic.fixes.is_empty()), "{diagnostics:?}");
}

#[test]
fn replacement_same_module_callable_shadow_withholds_message_and_fix() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Процедура Проверить()\n    А = Найти(\"a\", \"b\");\n    B = CurrentDate();\nКонецПроцедуры\nФункция СтрНайти(А, Б)\n    Возврат 1;\nКонецФункции\nФункция CurrentSessionDate()\n    Возврат 1;\nКонецФункции";
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    external(&root.join("legacy-external"), "LegacyProcessor", "");
    let project = project(root);
    let (host, _, ids) = host(&project, &[(modern, code)]);
    let diagnostics = deprecated(&host, ids[0]);
    assert_replacements(&diagnostics, &["СтрНайти", "CurrentSessionDate"], false);
    assert!(diagnostics.iter().all(|diagnostic| diagnostic.fixes.is_empty()), "{diagnostics:?}");
}

#[test]
fn replacement_bare_calls_keep_the_separate_variable_namespace() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Перем СтрНайти;\n&НаСервере\nПроцедура Проверить(CurrentSessionDate)\n    А = Найти(\"a\", \"b\");\n    B = CurrentDate();\nКонецПроцедуры";
    let modern = external(&root.join("modern-external"), "ModernProcessor", code);
    external(&root.join("legacy-external"), "LegacyProcessor", "");
    let project = project(root);
    let (host, _, ids) = host(&project, &[(modern, code)]);
    let diagnostics = deprecated(&host, ids[0]);
    assert_replacements(&diagnostics, &["СтрНайти", "CurrentSessionDate"], true);
    assert!(diagnostics.iter().all(|diagnostic| diagnostic.fixes.len() == 1), "{diagnostics:?}");
}

#[test]
fn replacement_bare_calls_require_a_complete_unshadowed_global_surface() {
    for unread in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
        configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
        let code = "&НаСервере\nПроцедура Проверить()\n    А = Найти(\"a\", \"b\");\n    B = CurrentDate();\nКонецПроцедуры";
        let modern = external(&root.join("modern-external"), "ModernProcessor", code);
        external(&root.join("legacy-external"), "LegacyProcessor", "");
        let source = "Функция СтрНайти(А, Б) Экспорт\n    Возврат 1;\nКонецФункции\nФункция CurrentSessionDate() Экспорт\n    Возврат 1;\nКонецФункции";
        let library = library(
            &root.join("modern"),
            "Overrides",
            source,
            "<Global>true</Global><Server>true</Server>",
        );
        let project = project(root);
        let files =
            if unread { vec![(modern, code)] } else { vec![(modern, code), (library, source)] };
        let (host, _, ids) = host(&project, &files);
        let diagnostics = deprecated(&host, ids[0]);
        assert_replacements(&diagnostics, &["СтрНайти", "CurrentSessionDate"], false);
        assert!(
            diagnostics.iter().all(|diagnostic| diagnostic.fixes.is_empty()),
            "unread={unread}: {diagnostics:?}"
        );
    }
}

#[test]
fn replacement_platform_namespace_is_not_a_same_named_local_or_common_module() {
    for local in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
        configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
        let parameter = if local { "ErrorProcessing" } else { "" };
        let code = format!("&НаСервере\nПроцедура Проверить({parameter})\n    А = DetailErrorDescription(ErrorInfo());\nКонецПроцедуры");
        let modern = external(&root.join("modern-external"), "ModernProcessor", &code);
        external(&root.join("legacy-external"), "LegacyProcessor", "");
        let source =
            "Функция DetailErrorDescription(Ошибка) Экспорт\n    Возврат \"custom\";\nКонецФункции";
        let library =
            library(&root.join("modern"), "ErrorProcessing", source, "<Server>true</Server>");
        let project = project(root);
        let files = if local {
            vec![(modern, code.as_str())]
        } else {
            vec![(modern, code.as_str()), (library, source)]
        };
        let (host, _, ids) = host(&project, &files);
        assert_replacements(&deprecated(&host, ids[0]), &["ErrorProcessing."], false);
    }
}

#[test]
fn replacement_bsp_server_call_keeps_method_guard_requirement() {
    use hir::ConfigsDatabase;
    for (guard, applicable) in [("Сервер", true), ("Клиент", false)] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
        configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
        let source = format!("#Если {guard} Тогда\nПроцедура СообщитьПользователю(Текст) Экспорт\nКонецПроцедуры\n#КонецЕсли");
        let library = library(
            &root.join("modern"),
            "ОбщегоНазначения",
            &source,
            "<Server>true</Server><ServerCall>true</ServerCall>",
        );
        let code = "&НаКлиенте\nПроцедура Проверить()\n    Сообщить(\"text\");\nКонецПроцедуры";
        external(&root.join("modern-external"), "ModernProcessor", "");
        let modern = managed_form(&root.join("modern-external"), "ModernProcessor", code);
        external(&root.join("legacy-external"), "LegacyProcessor", "");
        let project = project(root);
        let (host, _, ids) = host(&project, &[(modern, code), (library, &source)]);
        let module = host.raw_database().resolve_common_module(ids[0], "ОбщегоНазначения");
        assert!(module.expect("fixture common module is visible").is_server_call());
        assert_replacements(
            &deprecated(&host, ids[0]),
            &["ОбщегоНазначения.СообщитьПользователю"],
            applicable,
        );
    }
}
