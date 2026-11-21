use super::missing_notify_handler;
use crate::{Diagnostic, DiagnosticsConfig, DiagnosticsContext};
use hir::call_graph::{CallerId, EdgeKind, EdgeProvenance, NotifyTarget, ResolvedTarget};
use hir::{ConfigsDatabase, DefDatabase, ModuleId};
use test_fixture::CfeFixtureBuilder;

#[test]
fn common_module_handler_selection_matches_the_graph() {
    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";
    const SOURCE: &str = "&НаКлиенте\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", Модуль);\nКонецПроцедуры\n";

    for (base_export, extension_export, expected) in [(false, true, false), (true, false, true)] {
        let method = |export| {
            format!("Процедура Готово(){}\nКонецПроцедуры\n", if export { " Экспорт" } else { "" })
        };
        let mut builder = CfeFixtureBuilder::new("");
        builder
            .add_base_module("Модуль", &method(base_export))
            .add_extension("Расш", "")
            .add_extension_module("Расш", "Модуль", &method(extension_export));
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            SOURCE,
            builder.build(),
            &[],
            DiagnosticsConfig::all_enabled(),
            |fixture| {
                for (_, root) in fixture.config_paths() {
                    let form = root.join("CommonForms/Форма1/Ext/Form.xml");
                    std::fs::create_dir_all(form.parent().unwrap()).unwrap();
                    std::fs::write(
                        form,
                        r#"<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.20"><Events/></Form>"#,
                    )
                    .unwrap();
                    let path = root.join("CommonModules/Модуль.xml");
                    let xml = std::fs::read_to_string(&path).unwrap().replace(
                        "<ClientManagedApplication>false</ClientManagedApplication>",
                        "<ClientManagedApplication>true</ClientManagedApplication>",
                    );
                    std::fs::write(path, xml).unwrap();
                }
            },
            |db, ctx| {
                let summary = db.resolved_module_summary(ModuleId::new(ctx.file_id));
                let edges: Vec<_> =
                    summary.edges.iter().filter(|edge| edge.kind == EdgeKind::NotifyRef).collect();
                assert_eq!(edges.len(), 1, "{edges:#?}");
                if expected {
                    assert_eq!(edges[0].provenance, EdgeProvenance::StringResolved);
                    assert!(matches!(edges[0].target, ResolvedTarget::Method(_)));
                } else {
                    assert_eq!(edges[0].provenance, EdgeProvenance::VisibilityBlocked);
                    assert!(matches!(edges[0].target, ResolvedTarget::Unresolved(_)));
                }
                missing_notify_handler::check(ctx)
            },
        );
        if expected {
            assert!(diagnostics.is_empty(), "{diagnostics:#?}");
        } else {
            assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
            assert!(diagnostics[0].message.contains("Добавьте \"Экспорт\""));
        }
    }
}

fn write_client_common_module_metadata(root: &std::path::Path, name: &str) {
    let path = root.join(format!("CommonModules/{name}.xml"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>{name}</Name>
            <Global>false</Global>
            <ClientManagedApplication>true</ClientManagedApplication>
            <ClientOrdinaryApplication>false</ClientOrdinaryApplication>
            <Server>true</Server>
            <ExternalConnection>false</ExternalConnection>
            <ServerCall>false</ServerCall>
            <Privileged>false</Privileged>
            <ReturnValuesReuse>DontUse</ReturnValuesReuse>
        </Properties>
    </CommonModule>
</MetaDataObject>"#
        ),
    )
    .unwrap();
}

fn enable_client_common_module(fixture: &test_fixture::CfeFixture, name: &str) {
    let mut found = false;
    for (_, root) in fixture.config_paths() {
        let path = root.join(format!("CommonModules/{name}.xml"));
        if !path.exists() {
            continue;
        }
        let xml = std::fs::read_to_string(&path).unwrap();
        let updated = xml.replace(
            "<ClientManagedApplication>false</ClientManagedApplication>",
            "<ClientManagedApplication>true</ClientManagedApplication>",
        );
        assert_ne!(updated, xml, "{name} fixture must start client-disabled");
        std::fs::write(path, updated).unwrap();
        found = true;
    }
    assert!(found, "no CFE metadata for common module {name}");
}

#[test]
fn this_form_receiver_can_resolve_to_a_client_common_module() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    const SOURCE: &str = "&НаКлиенте\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", ThisForm);\nКонецПроцедуры\n\nПроцедура Готово(НеИспользуется)\n    Сообщить(\"локальный метод\");\nКонецПроцедуры\n";
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_base_module("ThisForm", "Процедура Готово() Экспорт\nКонецПроцедуры\n");
    let fixture = builder.build();

    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| {
            enable_client_common_module(fixture, "ThisForm");
            write_client_common_module_metadata(fixture.root(), "Caller");
        },
        |db, ctx| {
            let module = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module);
            assert!(matches!(
                &summary.notify_regs[0].target,
                NotifyTarget::Module(name) if name.as_str() == "ThisForm"
            ));
            let graph = db.resolved_module_summary(module);
            let edges: Vec<_> =
                graph.edges.iter().filter(|edge| edge.kind == EdgeKind::NotifyRef).collect();
            assert_eq!(edges.len(), 1, "{edges:#?}");
            let ResolvedTarget::Method(method) = &edges[0].target else {
                panic!("expected the callback edge to resolve: {edges:#?}");
            };
            assert_ne!(method.module.file_id, ctx.file_id, "edge must target ThisForm");

            let mut diagnostics = missing_notify_handler::check(ctx);
            diagnostics.extend(super::unused_parameters::check(ctx));
            diagnostics
        },
    );

    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code != crate::DiagnosticCode::MissingNotifyHandler),
        "external ThisForm handler should satisfy the registration: {diagnostics:#?}"
    );
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.code == crate::DiagnosticCode::UnusedParameters
                && diagnostic.message.contains("НеИспользуется")
        }),
        "the local same-named method is not the registered handler: {diagnostics:#?}"
    );
}

#[test]
fn notify_registrations_use_live_branches_and_type_availability() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    const SOURCE: &str = r#"
&НаКлиентеНаСервере
Процедура Тест()
    О = Новый ОписаниеОповещения("Mixed", Handlers);
#Если Сервер Тогда
    О = Новый ОписаниеОповещения("ServerBranch", Handlers);
#КонецЕсли
#Если Сервер И Не ТолстыйКлиентУправляемоеПриложение Тогда
    О = Новый ОписаниеОповещения("ServerOnlyTypeBranch", Handlers);
#КонецЕсли
#Если Клиент Тогда
    О = Новый ОписаниеОповещения("ClientBranch", Handlers);
#КонецЕсли
#Если Сервер И Не Сервер Тогда
    Handlers = Неопределено;
    О = Новый ОписаниеОповещения("DeadBranch", Handlers);
    О = Новый ОписаниеОповещения("DeadNonBareBranch", ПолучитьОбъект());
#КонецЕсли
#Если Сервер И Не Сервер Тогда
    Handlers = Неопределено;
#КонецЕсли
    О = Новый ОписаниеОповещения("AfterDeadAssignment", Handlers);
#Если Клиент Тогда
    LocalHandlers = Неопределено;
#КонецЕсли
    О = Новый ОписаниеОповещения("AfterLiveAssignment", LocalHandlers);
КонецПроцедуры

&НаСервере
Процедура Сервер()
    О = Новый ОписаниеОповещения("ServerOnly", Handlers);
КонецПроцедуры

&НаСервереБезКонтекста
Процедура СерверБезКонтекста()
    О = Новый ОписаниеОповещения("Contextless", Handlers);
КонецПроцедуры
"#;
    let handlers = [
        "Mixed",
        "ServerBranch",
        "ServerOnlyTypeBranch",
        "ClientBranch",
        "DeadBranch",
        "DeadNonBareBranch",
        "AfterDeadAssignment",
        "AfterLiveAssignment",
        "ServerOnly",
        "Contextless",
    ]
    .into_iter()
    .map(|name| format!("Процедура {name}() Экспорт\nКонецПроцедуры\n"))
    .collect::<String>();
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_base_module("Handlers", &handlers);
    let fixture = builder.build();

    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| {
            enable_client_common_module(fixture, "Handlers");
            write_client_common_module_metadata(fixture.root(), "Caller");
        },
        |db, ctx| {
            let metadata = ctx.module_metadata();
            assert_eq!(metadata.module_type, bsl_metadata::ModuleType::CommonModule);
            let common_module =
                metadata.common_module.as_ref().expect("caller common module metadata");
            assert!(common_module.is_client_managed_application());
            assert!(common_module.is_server());
            let module = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module);
            let caller = summary.notify_regs[0].caller;
            let CallerId::Method(local_id) = caller else { panic!("expected method caller") };
            let inferred = hir::infer_owner(db, ctx.file_id, hir::DefWithBodyId::Method(local_id));
            assert!(
                !inferred.dead_stmts().is_empty(),
                "dead branch was not recorded: {:?}",
                inferred.dead_stmts()
            );
            let target_for = |name: &str| {
                summary
                    .notify_regs
                    .iter()
                    .find(|reg| reg.callback_name.as_str() == name)
                    .map(|reg| &reg.target)
                    .unwrap_or_else(|| {
                        panic!("missing registration for {name}: {:#?}", summary.notify_regs)
                    })
            };
            for name in ["Mixed", "ServerBranch", "ClientBranch", "AfterDeadAssignment"] {
                assert!(
                    matches!(target_for(name), NotifyTarget::Module(module) if module.as_str() == "Handlers"),
                    "{name}: {:#?}",
                    target_for(name)
                );
            }
            for name in [
                "DeadBranch",
                "DeadNonBareBranch",
                "ServerOnly",
                "Contextless",
                "ServerOnlyTypeBranch",
            ] {
                assert!(
                    matches!(target_for(name), NotifyTarget::NotCompiled { .. }),
                    "{name}: {:#?}",
                    target_for(name)
                );
            }
            assert_eq!(target_for("AfterLiveAssignment"), &NotifyTarget::Unsupported);

            let graph = db.resolved_module_summary(module);
            let edges: Vec<_> =
                graph.edges.iter().filter(|edge| edge.kind == EdgeKind::NotifyRef).collect();
            assert_eq!(edges.len(), 4, "{edges:#?}");
            for edge in edges {
                let ResolvedTarget::Method(method) = &edge.target else {
                    panic!("valid client registration must resolve: {edge:#?}");
                };
                assert_ne!(method.module.file_id, ctx.file_id, "{edge:#?}");
            }
            assert!(missing_notify_handler::check(ctx).is_empty());
            Vec::new()
        },
    );
    assert!(diagnostics.is_empty());
}

#[test]
fn notify_receiver_assignments_shadow_only_overlapping_environments() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    let cases = [
        (
            "no assignment",
            r#"#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "server assignment and thin-client registration",
            r#"#Если Сервер И Не ТолстыйКлиентУправляемоеПриложение Тогда
    Handlers = Неопределено;
#КонецЕсли
#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "overlapping assignment and registration",
            r#"#Если ТонкийКлиент Тогда
    Handlers = Неопределено;
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            0,
        ),
        (
            "assignment in then branch does not shadow else registration",
            r#"#Если ТонкийКлиент Тогда
    Handlers = Неопределено;
    О = Новый ОписаниеОповещения("ВТонком", Handlers);
#Иначе
    О = Новый ОписаниеОповещения("ВДругом", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "assignment in else branch does not shadow then registration",
            r#"#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("ВТонком", Handlers);
#Иначе
    Handlers = Неопределено;
    О = Новый ОписаниеОповещения("ВДругом", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "dead assignment does not shadow registration",
            r#"#Если Сервер И Не Сервер Тогда
    Handlers = Неопределено;
#КонецЕсли
#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            1,
        ),
        (
            "unknown assignment environment conservatively shadows",
            r#"#Если НеизвестнаяПлатформа Тогда
    Handlers = Неопределено;
#КонецЕсли
#Если ТонкийКлиент Тогда
    О = Новый ОписаниеОповещения("Один", Handlers);
#КонецЕсли"#,
            0,
        ),
        (
            "English directives and CallbackDescription",
            r#"#If ThinClient Then
    hAnDlErS = Undefined;
    О = Новый("CallbackDescription", "Один", Handlers);
#EndIf"#,
            0,
        ),
    ];

    for (case, body, expected_module_targets) in cases {
        let source = format!("&НаКлиентеНаСервере\nПроцедура Тест()\n{body}\nКонецПроцедуры\n");
        let mut builder = CfeFixtureBuilder::new("");
        builder.add_base_module("Handlers", "Процедура Другая() Экспорт\nКонецПроцедуры\n");
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            &source,
            builder.build(),
            &[],
            DiagnosticsConfig::all_enabled(),
            |fixture| {
                enable_client_common_module(fixture, "Handlers");
                write_client_common_module_metadata(fixture.root(), "Caller");
            },
            |db, ctx| {
                let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
                let module_targets: Vec<_> = summary
                    .notify_regs
                    .iter()
                    .filter_map(|reg| match &reg.target {
                        NotifyTarget::Module(module) => Some(module),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    module_targets.len(),
                    expected_module_targets,
                    "{case}: {:#?}",
                    summary.notify_regs
                );
                assert!(module_targets.iter().all(|module| module.as_str() == "Handlers"));
                missing_notify_handler::check(ctx)
            },
        );
        let missing: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::MissingNotifyHandler)
            .collect();
        assert_eq!(missing.len(), expected_module_targets, "{case}: {diagnostics:#?}");
    }
}

#[test]
fn server_only_notify_constructors_are_not_diagnosed() {
    const CALLER: &str = "CommonForms/Форма1/Ext/Form/Module.bsl";
    const SOURCE: &str = r#"
&НаСервере
Процедура Тест()
    О = Новый ОписаниеОповещения("Статический", ЭтотОбъект);
    О = Новый("CallbackDescription", "Динамический", ЭтотОбъект);
КонецПроцедуры
"#;
    let fixture = CfeFixtureBuilder::new("").build();
    crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |_| {},
        |db, ctx| {
            let module = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module);
            assert_eq!(summary.notify_regs.len(), 2, "{:#?}", summary.notify_regs);
            assert!(
                summary
                    .notify_regs
                    .iter()
                    .all(|reg| { matches!(reg.target, NotifyTarget::NotCompiled { .. }) }),
                "{:#?}",
                summary.notify_regs
            );
            assert!(db
                .resolved_module_summary(module)
                .edges
                .iter()
                .all(|edge| edge.kind != EdgeKind::NotifyRef));
            assert!(missing_notify_handler::check(ctx).is_empty());
            Vec::new()
        },
    );
}

#[test]
fn unavailable_notify_registrations_do_not_keep_local_parameters_live() {
    const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
    const SOURCE: &str = r#"
&НаКлиентеНаСервере
Процедура Тест() Экспорт
#Если Сервер И Не Сервер Тогда
    О = Новый ОписаниеОповещения("DeadHandler", ПолучитьОбъект());
#КонецЕсли
#Если Клиент Тогда
    О = Новый ОписаниеОповещения("LiveHandler", ПолучитьОбъект());
#КонецЕсли
КонецПроцедуры

&НаКлиентеНаСервере
Процедура DeadHandler(МёртвыйПараметр)
    Сообщить("handler");
КонецПроцедуры

&НаКлиентеНаСервере
Процедура LiveHandler(ЖивойПараметр)
    Сообщить("handler");
КонецПроцедуры
"#;
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_base_module("Caller", "");
    let fixture = builder.build();
    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        |fixture| write_client_common_module_metadata(fixture.root(), "Caller"),
        |db, ctx| {
            let summary = db.module_call_summary(ModuleId::new(ctx.file_id));
            let target_for = |name: &str| {
                summary
                    .notify_regs
                    .iter()
                    .find(|reg| reg.callback_name.as_str() == name)
                    .map(|reg| &reg.target)
                    .unwrap_or_else(|| panic!("missing {name}: {:#?}", summary.notify_regs))
            };
            assert!(matches!(target_for("DeadHandler"), NotifyTarget::NotCompiled { .. }));
            assert_eq!(target_for("LiveHandler"), &NotifyTarget::Unsupported);
            super::unused_parameters::check(ctx)
        },
    );
    let unused_parameters: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == crate::DiagnosticCode::UnusedParameters)
        .collect();
    assert_eq!(unused_parameters.len(), 1, "{diagnostics:#?}");
    assert!(unused_parameters[0].message.contains("МёртвыйПараметр"), "{diagnostics:#?}");
}

fn form_with_unread_dialog(extension: bool, base_mapped: bool, malformed: bool) -> Vec<Diagnostic> {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use vfs::{FileId, FileSet, VfsPath};

    const REL: &str = "Documents/Заказ/Forms/Форма/Ext/Form/Module.bsl";
    const SOURCE: &str = "&НаКлиенте\nПроцедура Тест()\n    О = Новый ОписаниеОповещения(\"Готово\", ЭтотОбъект);\nКонецПроцедуры\n";
    const BASE: &str = "Процедура Готово() Экспорт\nКонецПроцедуры\n";

    let mut builder = CfeFixtureBuilder::new("");
    if extension {
        builder.add_extension("Расш", "");
    }
    let fixture = builder.build();
    let caller_path =
        if extension { fixture.extensions()[0].root().join(REL) } else { fixture.root().join(REL) };
    let base_path = fixture.root().join(REL);
    std::fs::create_dir_all(caller_path.parent().unwrap()).unwrap();
    std::fs::write(&caller_path, SOURCE).unwrap();
    if base_mapped {
        std::fs::create_dir_all(base_path.parent().unwrap()).unwrap();
        std::fs::write(&base_path, BASE).unwrap();
    }
    if malformed {
        std::fs::write(caller_path.parent().unwrap().parent().unwrap().join("Form.xml"), "<")
            .unwrap();
    }

    let mut db = ide_db::RootDatabaseImpl::new();
    db.set_all_config_paths(fixture.config_paths(), Some(bsl_metadata::ExtensionScope::InfoBase));
    let caller = FileId(0);
    let mut files = FileSet::default();
    files.insert(caller, VfsPath::new(caller_path.to_string_lossy().into_owned()));
    if base_mapped {
        files.insert(FileId(1), VfsPath::new(base_path.to_string_lossy().into_owned()));
    }
    db.set_source_root(SourceRootId(0), SourceRoot::new_local(files));
    db.set_file_source_root(caller, SourceRootId(0));
    db.set_file_text(caller, SOURCE);
    if base_mapped {
        db.set_file_source_root(FileId(1), SourceRootId(0));
        db.set_file_text(FileId(1), BASE);
    }
    assert_eq!(ide_db::weaving_target(&db, caller).is_some(), base_mapped);
    let provider = ide_db::SalsaProvider::new(&db, None);
    let config = DiagnosticsConfig::all_enabled();
    let ctx = DiagnosticsContext::new(&config, caller, &provider);
    assert!(ctx.module_metadata().form.is_none());
    missing_notify_handler::check(&ctx)
}

#[test]
fn unread_form_metadata_never_proves_the_form_is_managed() {
    for malformed in [false, true] {
        for base_mapped in [true, false] {
            let diagnostics = form_with_unread_dialog(true, base_mapped, malformed);
            assert!(diagnostics.is_empty(), "a base form can own the handler: {diagnostics:#?}");
        }
        let diagnostics = form_with_unread_dialog(false, false, malformed);
        assert!(diagnostics.is_empty(), "the base form kind is also unknown: {diagnostics:#?}");
    }
}
