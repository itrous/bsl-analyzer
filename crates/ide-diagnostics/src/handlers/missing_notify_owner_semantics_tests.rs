use super::missing_notify_handler;
use crate::DiagnosticsConfig;
use bsl_platform::PlatformDataInner;
use hir::{
    call_graph::{CallerId, NotifyTarget},
    DefDatabase, DefWithBodyId, Expr, ExprId, IdConversion, ModuleId, Stmt, TypeKernelDb, TypeKind,
};
use test_fixture::CfeFixtureBuilder;

const CALLER: &str = "CommonModules/Caller/Ext/Module.bsl";
const CALLBACKS: &str = r#"
&НаКлиентеНаСервере
Процедура Test()
BODY
КонецПроцедуры
"#;

#[derive(Clone, Copy)]
enum ExpectedOwner {
    CommonModule,
    Local,
    LocalWithoutReachingWrite,
    Parameter,
}

#[test]
fn regular_bare_reads_and_notify_receivers_share_the_owner_across_compile_environments() {
    let cases = [
        (
            "dead contradictory assignment",
            r#"#Если Сервер И Не Сервер Тогда
    Handlers = Новый Структура;
#КонецЕсли
Observed = Handlers;
О = Новый ОписаниеОповещения("MissingDirect", Handlers);
О = Новый CallbackDescription("MissingAlias", Handlers);"#,
            ExpectedOwner::CommonModule,
            2,
        ),
        (
            "server assignment is disjoint from thin-client use",
            r#"#Если Сервер Тогда
    Handlers = Новый Структура;
#КонецЕсли
#Если ТонкийКлиент Тогда
    Observed = Handlers;
    О = Новый ОписаниеОповещения("MissingDirect", Handlers);
    О = Новый CallbackDescription("MissingAlias", Handlers);
#КонецЕсли"#,
            ExpectedOwner::CommonModule,
            2,
        ),
        (
            "overlapping assignment before use",
            r#"#Если ТонкийКлиент Тогда
    handlers = Новый Структура;
    Observed = HANDLERS;
    О = Новый ОписаниеОповещения("MissingDirect", Handlers);
    О = Новый CallbackDescription("MissingAlias", Handlers);
#КонецЕсли"#,
            ExpectedOwner::Local,
            0,
        ),
        (
            "overlapping assignment after use shadows the whole body",
            r#"#Если ТонкийКлиент Тогда
    Observed = Handlers;
    О = Новый ОписаниеОповещения("MissingDirect", Handlers);
    О = Новый CallbackDescription("MissingAlias", Handlers);
    Handlers = Новый Структура;
#КонецЕсли"#,
            ExpectedOwner::LocalWithoutReachingWrite,
            0,
        ),
        (
            "parameter shadows a same-named common module",
            r#"Observed = Handlers;
О = Новый ОписаниеОповещения("MissingDirect", Handlers);
О = Новый CallbackDescription("MissingAlias", Handlers);"#,
            ExpectedOwner::Parameter,
            0,
        ),
    ];

    for (case, body, expected_owner, expected_diagnostics) in cases {
        let mut source = CALLBACKS
            .replace("BODY", body)
            .replace("Observed = Handlers;", "Observed = Handlers;\nHandlers.MissingMethod();");
        if matches!(expected_owner, ExpectedOwner::Parameter) {
            source = source.replace("Процедура Test()", "Процедура Test(Handlers)");
        }
        let mut builder = CfeFixtureBuilder::new("");
        builder.add_base_module("Handlers", "");
        let fixture = builder.build();
        let diagnostics = crate::test_utils::check_cfe_at_with_db(
            CALLER,
            &source,
            fixture,
            &[],
            DiagnosticsConfig::all_enabled(),
            configure_common_modules,
            |db, ctx| {
                let module_id = ModuleId::new(ctx.file_id);
                let summary = db.module_call_summary(module_id);
                assert_eq!(summary.notify_regs.len(), 2, "{case}: {:#?}", summary.notify_regs);
                let CallerId::Method(local_id) = summary.notify_regs[0].caller else {
                    panic!("{case}: expected a method caller");
                };
                let body =
                    db.module_bodies_ref(module_id).body(local_id).expect("caller method body");
                let inference = ctx.infer_owner(DefWithBodyId::Method(local_id));
                let dispatched_to_common_module = inference.diagnostics().iter().any(|diagnostic| {
                    matches!(
                        diagnostic,
                        hir::InferenceDiagnostic::MissedRequiredParameterCommonModule { module, callee, .. }
                            if module.as_str() == "Handlers" && callee.as_str() == "MissingMethod"
                    )
                });
                assert_eq!(
                    dispatched_to_common_module,
                    matches!(expected_owner, ExpectedOwner::CommonModule),
                    "{case}: call dispatch disagrees with the read's owner: {:#?}",
                    inference.diagnostics()
                );
                let ordinary_read = assigned_path_read(body, "Observed", "Handlers");
                let ordinary_ty = inference
                    .type_id_of_expr(ordinary_read)
                    .unwrap_or_else(|| panic!("{case}: ordinary read has no inferred type"));
                let receivers = notify_receivers(body);
                assert_eq!(receivers.len(), 2, "{case}: receiver expressions: {receivers:?}");
                for receiver in &receivers {
                    assert_eq!(
                        inference.type_id_of_expr(*receiver),
                        Some(ordinary_ty),
                        "{case}: ordinary read and notification receiver inferred differently"
                    );
                }

                match expected_owner {
                    ExpectedOwner::CommonModule => {
                        assert!(
                            matches!(db.lookup_type(ordinary_ty), TypeKind::CommonModule(_)),
                            "{case}: expected the common-module owner, got {:?}",
                            db.lookup_type(ordinary_ty)
                        );
                        assert!(
                            summary.notify_regs.iter().all(|reg| matches!(
                                &reg.target,
                                NotifyTarget::Module(name) if name.as_str() == "Handlers"
                            )),
                            "{case}: {:#?}",
                            summary.notify_regs
                        );
                    }
                    ExpectedOwner::Local => {
                        assert!(
                            matches!(db.lookup_type(ordinary_ty), TypeKind::Structure(_)),
                            "{case}: expected the overlapping local's structure type, got {:?}",
                            db.lookup_type(ordinary_ty)
                        );
                        // A local of a known data type is proven foreign: its value can
                        // never be the current module object.
                        assert!(
                            summary
                                .notify_regs
                                .iter()
                                .all(|reg| { matches!(reg.target, NotifyTarget::Unsupported) }),
                            "{case}: {:#?}",
                            summary.notify_regs
                        );
                    }
                    ExpectedOwner::LocalWithoutReachingWrite => {
                        assert!(matches!(db.lookup_type(ordinary_ty), TypeKind::Unknown),
                            "{case}: expected the not-yet-assigned local to have unknown type, got {:?}",
                            db.lookup_type(ordinary_ty));
                        assert!(
                            summary
                                .notify_regs
                                .iter()
                                .all(|reg| { matches!(reg.target, NotifyTarget::LocalBinding) }),
                            "{case}: {:#?}",
                            summary.notify_regs
                        );
                    }
                    ExpectedOwner::Parameter => {
                        assert!(
                            matches!(db.lookup_type(ordinary_ty), TypeKind::Unknown),
                            "{case}: expected an unknown parameter type, got {:?}",
                            db.lookup_type(ordinary_ty)
                        );
                        assert!(
                            summary
                                .notify_regs
                                .iter()
                                .all(|reg| { matches!(reg.target, NotifyTarget::LocalBinding) }),
                            "{case}: {:#?}",
                            summary.notify_regs
                        );
                    }
                }

                missing_notify_handler::check(ctx)
            },
        );
        assert_eq!(diagnostics.len(), expected_diagnostics, "{case}: {diagnostics:#?}");
    }
}

#[test]
fn bare_read_and_method_call_share_ownership_before_a_disjoint_late_assignment() {
    let source = CALLBACKS.replace(
        "BODY",
        r#"#Если ТонкийКлиент Тогда
    Observed = Ghost;
    Ghost.Missing();
#КонецЕсли
#Если Сервер Тогда
    Ghost = Новый Структура;
#КонецЕсли
О = Новый ОписаниеОповещения("Test", Ghost);"#,
    );
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_base_module("Handlers", "");
    let fixture = builder.build();
    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        &source,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        configure_common_modules,
        |db, ctx| {
            let module_id = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module_id);
            let CallerId::Method(local_id) = summary
                .notify_regs
                .first()
                .expect("registration provides its caller method")
                .caller
            else {
                panic!("expected a method caller");
            };
            let inference = ctx.infer_owner(DefWithBodyId::Method(local_id));
            assert!(
                inference.diagnostics().iter().any(|diagnostic| matches!(
                    diagnostic,
                    hir::InferenceDiagnostic::UnresolvedName { .. }
                )),
                "the simple read in the thin-client environment must remain unresolved: {:#?}",
                inference.diagnostics()
            );
            assert!(
                inference.diagnostics().iter().any(|diagnostic| matches!(
                    diagnostic,
                    hir::InferenceDiagnostic::UnresolvedMethodCall { receiver_name, method_name, .. }
                        if receiver_name.as_str() == "Ghost" && method_name.as_str() == "Missing"
                )),
                "the method call must use the same unowned-name verdict as the read: {:#?}",
                inference.diagnostics()
            );
            Vec::new()
        },
    );
    assert!(diagnostics.is_empty(), "diagnostics are checked in the inference result");
}

fn assigned_path_read(body: &hir::Body, target_name: &str, value_name: &str) -> ExprId {
    body.stmts_iter()
        .find_map(|(_, stmt)| {
            let Stmt::Assign { target, value } = stmt else { return None };
            let (Expr::Path(target_name_found), Expr::Path(value_name_found)) =
                (body.expr_idx(*target), body.expr_idx(*value))
            else {
                return None;
            };
            (target_name_found.as_str().eq_ignore_ascii_case(target_name)
                && value_name_found.as_str().eq_ignore_ascii_case(value_name))
            .then(|| ExprId::from_idx(*value))
        })
        .unwrap_or_else(|| panic!("missing `{target_name} = {value_name}` in body"))
}

#[test]
fn duplicate_foreign_notify_slots_do_not_leave_ordinary_local_name_references() {
    const SOURCE: &str = r#"&НаКлиентеНаСервере
Процедура Register()
    О = Новый ОписаниеОповещения("SharedHandler", Handlers, , "SharedHandler", Handlers);
КонецПроцедуры

&НаКлиентеНаСервере
Процедура SharedHandler() Экспорт
КонецПроцедуры
"#;
    let mut builder = CfeFixtureBuilder::new("");
    builder.add_base_module("Handlers", "Процедура SharedHandler() Экспорт\nКонецПроцедуры\n");
    let fixture = builder.build();
    let diagnostics = crate::test_utils::check_cfe_at_with_db(
        CALLER,
        SOURCE,
        fixture,
        &[],
        DiagnosticsConfig::all_enabled(),
        configure_common_modules,
        |db, ctx| {
            let module = ModuleId::new(ctx.file_id);
            let summary = db.module_call_summary(module);
            assert_eq!(summary.notify_regs.len(), 1, "the graph summary merges equal targets");
            let candidates = ctx.notify_candidates();
            assert_eq!(
                candidates.len(),
                2,
                "both callback slots retain provenance: {candidates:#?}"
            );
            assert!(candidates.iter().all(|candidate| matches!(
                &candidate.receiver,
                hir::NotifyCandidateReceiver::ForeignModule(name) if name.as_str() == "Handlers"
            )));
            assert_eq!(candidates[0].slot, hir::NotifyCallbackSlot::Handler);
            assert_eq!(candidates[1].slot, hir::NotifyCallbackSlot::ErrorHandler);
            assert!(
                summary.name_literal_refs.is_empty(),
                "a foreign callback name must not count as an ordinary local reference: {summary:#?}"
            );
            assert!(
                summary.name_literal_ref_sites.iter().all(|site| {
                    candidates.iter().all(|candidate| candidate.expression != site.expr)
                }),
                "foreign slots must be removed from source-exact local references: {:#?}",
                summary.name_literal_ref_sites
            );
            missing_notify_handler::check(ctx)
        },
    );
    assert!(diagnostics.is_empty(), "external handler is not checked locally: {diagnostics:#?}");
}

fn notify_receivers(body: &hir::Body) -> Vec<ExprId> {
    let platform = PlatformDataInner::instance();
    body.exprs_iter()
        .filter_map(|(_, node)| {
            let Expr::New { type_name, args } = node else { return None };
            let type_name = type_name.as_ref()?;
            platform
                .is_notify_description_type_name(type_name.as_str())
                .then(|| args.get(1).copied().map(ExprId::from_idx))
                .flatten()
        })
        .collect()
}

fn configure_common_modules(fixture: &test_fixture::CfeFixture) {
    for (_, root) in fixture.config_paths() {
        let path = root.join("CommonModules/Handlers.xml");
        if path.exists() {
            let xml = std::fs::read_to_string(&path).unwrap();
            let xml = xml.replace(
                "<ClientManagedApplication>false</ClientManagedApplication>",
                "<ClientManagedApplication>true</ClientManagedApplication>",
            );
            std::fs::write(path, xml).unwrap();
        }
    }

    let path = fixture.root().join("CommonModules/Caller.xml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="00000000-0000-0000-0000-000000000001">
        <Properties>
            <Name>Caller</Name>
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
</MetaDataObject>"#,
    )
    .unwrap();
}
