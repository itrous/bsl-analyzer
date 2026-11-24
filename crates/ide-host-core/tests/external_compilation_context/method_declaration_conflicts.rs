use super::*;

#[test]
fn unconditional_duplicate_is_a_blocker_even_when_the_external_owner_becomes_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    let code = "Процедура Применить(Область)\nКонецПроцедуры\nProcedure ПРИМЕНИТЬ(Area, Value)\nEndProcedure";
    let legacy = external(&root.join("legacy-external"), "LegacyProcessor", code);
    external(&root.join("modern-external"), "ModernProcessor", "");
    let project = project(root);
    let (mut host, vfs, ids) = host(&project, &[(legacy, code)]);
    write(&root.join("legacy"), "Configuration.xml", "<broken");
    host.refresh_metadata_substrate(&vfs, &[root.join("legacy/Configuration.xml")]);
    assert_eq!(
        host.raw_database().file_compilation_context(ids[0]).unwrap().origin,
        hir::CompilationContextOrigin::Unknown
    );
    let diagnostics = host.analysis().diagnostics(ids[0], &DiagnosticsConfig::all_enabled());
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::DuplicateMethodDeclaration
                && diagnostic.severity == ide::Severity::Blocker)
            .count(),
        2,
        "{diagnostics:?}"
    );
}

#[test]
fn managed_form_methods_in_distinct_contexts_do_not_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    external(&root.join("legacy-external"), "LegacyProcessor", "");
    external(&root.join("modern-external"), "ModernProcessor", "");
    write(
        &root.join("modern-external"),
        "ModernProcessor/Forms/Main.xml",
        r#"<MetaDataObject><Form uuid="00000000-0000-0000-0000-000000000004"><Properties><Name>Main</Name><FormType>Managed</FormType></Properties></Form></MetaDataObject>"#,
    );
    let code="&AtClient\nProcedure Shared()\nEndProcedure\n&НаСервере\nПроцедура SHARED(Value)\nКонецПроцедуры";
    let path = "ModernProcessor/Forms/Main/Ext/Form/Module.bsl";
    write(&root.join("modern-external"), path, code);
    let project = project(root);
    let (host, _, ids) = host(&project, &[(root.join("modern-external").join(path), code)]);
    let diagnostics = host.analysis().diagnostics(ids[0], &DiagnosticsConfig::all_enabled());
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == DiagnosticCode::DuplicateMethodDeclaration),
        "{diagnostics:?}"
    );
}

#[test]
fn ordinary_form_directives_do_not_create_distinct_method_namespaces() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    configuration(&root.join("modern"), "ModernOnly", "ManagedApplication", "Version8_3_27");
    configuration(&root.join("legacy"), "LegacyOnly", "OrdinaryApplication", "Version8_2_16");
    external(&root.join("legacy-external"), "LegacyProcessor", "");
    external(&root.join("modern-external"), "ModernProcessor", "");
    write(
        &root.join("legacy-external"),
        "LegacyProcessor/Forms/Форма.xml",
        r#"<MetaDataObject><Form uuid="00000000-0000-0000-0000-000000000004"><Properties><Name>Форма</Name><FormType>Ordinary</FormType></Properties></Form></MetaDataObject>"#,
    );
    let code = "&AtClient\nProcedure Shared()\nEndProcedure\n&НаСервере\nПроцедура SHARED(Value)\nКонецПроцедуры";
    let path = "LegacyProcessor/Forms/Форма/Ext/Form/Module.bsl";
    write(&root.join("legacy-external"), path, code);
    let project = project(root);
    let (host, _, ids) = host(&project, &[(root.join("legacy-external").join(path), code)]);
    let diagnostics = host.analysis().diagnostics(ids[0], &DiagnosticsConfig::all_enabled());
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == DiagnosticCode::DuplicateMethodDeclaration)
            .count(),
        2,
        "{diagnostics:?}"
    );
}
