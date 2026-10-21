use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::TypeId;
use hir_def::module_interface::MethodDecl;
use hir_def::resolver::{QualifiedMethodError, Resolver};
use hir_def::ty::{DocSeeSlots, FunctionSignature};
use hir_def::{MethodId, Name};

use crate::db::HirDatabase;
use crate::lower::TyLoweringContext;
#[cfg(test)]
use vfs::FileId;

use crate::infer::UnresolvedMethodKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodResolution {
    pub method_id: MethodId,

    pub is_export: bool,

    pub signature: FunctionSignature,

    pub return_type: TypeId,
}

impl MethodResolution {
    pub fn new(method_id: MethodId, is_export: bool, signature: FunctionSignature) -> Self {
        let return_type = signature.ret;
        Self { method_id, is_export, signature, return_type }
    }
}

pub fn resolve_qualified_call(
    db: &dyn HirDatabase,
    module_name: &Name,
    method_name: &Name,
    resolver: &Resolver,
) -> Result<MethodResolution, UnresolvedMethodKind> {
    let resolution =
        resolver.resolve_qualified_method(db, module_name, method_name).map_err(|e| match e {
            QualifiedMethodError::NotVisibleInConfigs | QualifiedMethodError::NotFound => {
                UnresolvedMethodKind::MethodNotFound
            }
            QualifiedMethodError::BodyUnread => UnresolvedMethodKind::BodyUnread,
        })?;

    let method_symbol = db.interface_method(resolution.method_id).expect(
        "method_id returned by Resolver must exist in module_interface — \
         module_interface / Resolver are out of sync",
    );

    let signature = materialise_signature_enriched(db, resolution.method_id, &method_symbol);
    Ok(MethodResolution::new(resolution.method_id, resolution.is_export, signature))
}

/// The object family whose `ObjectModule` slot holds the user module of a value of
/// `kind`: its export methods and export variables are the value's user members.
pub(crate) fn object_kind_to_mdo(kind: hir_def::ty::MetadataKind) -> Option<bsl_metadata::MdoType> {
    use bsl_metadata::MdoType;
    use hir_def::ty::MetadataKind;
    Some(match kind {
        MetadataKind::CatalogObject => MdoType::Catalog,
        MetadataKind::DocumentObject => MdoType::Document,
        MetadataKind::ExchangePlanObject => MdoType::ExchangePlan,
        MetadataKind::ChartOfAccountsObject => MdoType::ChartOfAccounts,
        MetadataKind::TaskObject => MdoType::Task,
        MetadataKind::BusinessProcessObject => MdoType::BusinessProcess,
        MetadataKind::DataProcessorObject => MdoType::DataProcessor,
        MetadataKind::ReportObject => MdoType::Report,
        MetadataKind::ExternalDataProcessorObject => MdoType::ExternalDataProcessor,
        MetadataKind::ExternalReportObject => MdoType::ExternalReport,
        MetadataKind::ChartOfCharacteristicTypesObject => MdoType::ChartOfCharacteristicTypes,
        MetadataKind::ChartOfCalculationTypesObject => MdoType::ChartOfCalculationTypes,
        // The module index files a constant's value-manager module in its object slot.
        MetadataKind::ConstantValueManager => MdoType::Constant,
        MetadataKind::CatalogRef
        | MetadataKind::DocumentRef
        | MetadataKind::EnumRef
        | MetadataKind::TaskRef
        | MetadataKind::BusinessProcessRef
        | MetadataKind::ExchangePlanRef
        | MetadataKind::ChartOfAccountsRef
        | MetadataKind::ChartOfCharacteristicTypesRef
        | MetadataKind::ChartOfCalculationTypesRef
        | MetadataKind::InformationRegisterRef
        | MetadataKind::AccumulationRegisterRef
        | MetadataKind::AccountingRegisterRef
        | MetadataKind::CalculationRegisterRef => return None,
        MetadataKind::InformationRegisterRecordManager
        | MetadataKind::InformationRegisterRecordSet
        | MetadataKind::InformationRegisterRecord
        | MetadataKind::AccumulationRegisterRecordSet
        | MetadataKind::AccumulationRegisterRecord
        | MetadataKind::AccountingRegisterRecordSet
        | MetadataKind::AccountingRegisterRecord
        | MetadataKind::CalculationRegisterRecordSet
        | MetadataKind::CalculationRegisterRecord => return None,
        MetadataKind::RegisterDimension { .. }
        | MetadataKind::RegisterResource { .. }
        | MetadataKind::RegisterAttribute { .. }
        | MetadataKind::RegisterFilter { .. }
        | MetadataKind::TabularSection { .. }
        | MetadataKind::TabularSectionRow { .. } => return None,
    })
}

fn record_set_kind_to_mdo(kind: hir_def::ty::MetadataKind) -> Option<bsl_metadata::MdoType> {
    use bsl_metadata::MdoType;
    use hir_def::ty::MetadataKind;
    Some(match kind {
        MetadataKind::InformationRegisterRecordSet => MdoType::InformationRegister,
        MetadataKind::AccumulationRegisterRecordSet => MdoType::AccumulationRegister,
        MetadataKind::AccountingRegisterRecordSet => MdoType::AccountingRegister,
        MetadataKind::CalculationRegisterRecordSet => MdoType::CalculationRegister,
        MetadataKind::InformationRegisterRecordManager => return None,
        MetadataKind::CatalogObject
        | MetadataKind::DocumentObject
        | MetadataKind::ExchangePlanObject
        | MetadataKind::ChartOfAccountsObject
        | MetadataKind::TaskObject
        | MetadataKind::BusinessProcessObject
        | MetadataKind::DataProcessorObject
        | MetadataKind::ChartOfCharacteristicTypesObject
        | MetadataKind::ChartOfCalculationTypesObject
        | MetadataKind::ReportObject
        | MetadataKind::ExternalDataProcessorObject
        | MetadataKind::ExternalReportObject
        | MetadataKind::ConstantValueManager => return None,
        MetadataKind::CatalogRef
        | MetadataKind::DocumentRef
        | MetadataKind::EnumRef
        | MetadataKind::TaskRef
        | MetadataKind::BusinessProcessRef
        | MetadataKind::ExchangePlanRef
        | MetadataKind::ChartOfAccountsRef
        | MetadataKind::ChartOfCharacteristicTypesRef
        | MetadataKind::ChartOfCalculationTypesRef
        | MetadataKind::InformationRegisterRef
        | MetadataKind::AccumulationRegisterRef
        | MetadataKind::AccountingRegisterRef
        | MetadataKind::CalculationRegisterRef => return None,
        MetadataKind::InformationRegisterRecord
        | MetadataKind::AccumulationRegisterRecord
        | MetadataKind::AccountingRegisterRecord
        | MetadataKind::CalculationRegisterRecord => return None,
        MetadataKind::RegisterDimension { .. }
        | MetadataKind::RegisterResource { .. }
        | MetadataKind::RegisterAttribute { .. }
        | MetadataKind::RegisterFilter { .. }
        | MetadataKind::TabularSection { .. }
        | MetadataKind::TabularSectionRow { .. } => return None,
    })
}

pub fn resolve_record_set_module_call(
    db: &dyn HirDatabase,
    kind: hir_def::ty::MetadataKind,
    mdo_name: &Name,
    method_name: &Name,
    resolver: &Resolver,
) -> Result<MethodResolution, UnresolvedMethodKind> {
    let mdo_type = record_set_kind_to_mdo(kind).ok_or(UnresolvedMethodKind::MethodNotFound)?;

    let resolution = resolver
        .resolve_record_set_module_method(db, mdo_type, mdo_name, method_name)
        .map_err(|e| match e {
            QualifiedMethodError::NotVisibleInConfigs | QualifiedMethodError::NotFound => {
                UnresolvedMethodKind::MethodNotFound
            }
            QualifiedMethodError::BodyUnread => UnresolvedMethodKind::BodyUnread,
        })?;

    let method_symbol = db.interface_method(resolution.method_id).expect(
        "method_id returned by Resolver must exist in module_interface — \
         module_interface / Resolver are out of sync",
    );

    let signature = materialise_signature_enriched(db, resolution.method_id, &method_symbol);
    Ok(MethodResolution::new(resolution.method_id, resolution.is_export, signature))
}

pub fn resolve_object_module_call(
    db: &dyn HirDatabase,
    kind: hir_def::ty::MetadataKind,
    mdo_name: &Name,
    method_name: &Name,
    resolver: &Resolver,
) -> Result<MethodResolution, UnresolvedMethodKind> {
    let mdo_type = object_kind_to_mdo(kind).ok_or(UnresolvedMethodKind::MethodNotFound)?;

    let resolution = resolver
        .resolve_object_module_method(db, mdo_type, mdo_name, method_name)
        .map_err(|e| match e {
            QualifiedMethodError::NotVisibleInConfigs | QualifiedMethodError::NotFound => {
                UnresolvedMethodKind::MethodNotFound
            }
            QualifiedMethodError::BodyUnread => UnresolvedMethodKind::BodyUnread,
        })?;

    let method_symbol = db.interface_method(resolution.method_id).expect(
        "method_id returned by Resolver must exist in module_interface — \
         module_interface / Resolver are out of sync",
    );

    let signature = materialise_signature_enriched(db, resolution.method_id, &method_symbol);
    Ok(MethodResolution::new(resolution.method_id, resolution.is_export, signature))
}

pub fn resolve_aliased_manager_call(
    db: &dyn HirDatabase,
    mdo_type: bsl_metadata::MdoType,
    mdo_name: &Name,
    method_name: &Name,
    resolver: &Resolver,
) -> Result<MethodResolution, UnresolvedMethodKind> {
    let resolution = resolver
        .resolve_aliased_manager_method(db, mdo_type, mdo_name, method_name)
        .map_err(|e| match e {
        QualifiedMethodError::NotVisibleInConfigs | QualifiedMethodError::NotFound => {
            UnresolvedMethodKind::MethodNotFound
        }
        QualifiedMethodError::BodyUnread => UnresolvedMethodKind::BodyUnread,
    })?;

    let method_symbol = db.interface_method(resolution.method_id).expect(
        "method_id returned by Resolver must exist in module_interface — \
         module_interface / Resolver are out of sync",
    );

    let signature = materialise_signature_enriched(db, resolution.method_id, &method_symbol);
    Ok(MethodResolution::new(resolution.method_id, resolution.is_export, signature))
}

/// Which module answered a call on a typed receiver.
///
/// Carried out of [`resolve_user_call`] because inference names the receiver
/// differently per route in its not-exported diagnostic: the object route reports
/// the COERCED receiver, the other two the one as written. The two spellings only
/// diverge on `ЭтотОбъект`/`ЭтотМенеджер`. Navigation ignores this field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserCallRoute {
    ObjectModule,
    RecordSetModule,
    ManagerModule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCall {
    pub route: UserCallRoute,
    pub resolution: MethodResolution,
    /// The metadata object whose module answered, as the receiver type spelled
    /// it. Inference falls back to it when the receiver has no display name.
    pub mdo_name: Name,
}

/// The verdict of the user-method cascade. `NotUserMethod` and `BodyUnread` both
/// mean "ask the platform surface next"; they differ in whether a body that
/// nobody read could have answered, which only inference records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserCallTarget {
    Method(UserCall),
    BodyUnread,
    NotUserMethod,
}

/// Resolve a call on a typed receiver against the USER methods of the module the
/// receiver denotes — object module, record-set module, or manager module.
///
/// This is the single owner of two rules that used to live in inference alone,
/// and that every other consumer of the same question had to reproduce:
///
/// - the receiver is COERCED first, so `ЭтотОбъект` and `ЭтотМенеджер` reach the
///   same modules as a variable holding the object would;
/// - a user method is looked for BEFORE the platform surface, so a user method
///   spelled like a platform one (`Записать` on a catalog object, `НайтиПоКоду`
///   on its manager) is the one the call names.
///
/// Deliberately pure: no diagnostics, no interning of expression types, no
/// recording of call-argument bindings. Those belong to inference, which has the
/// expression ids this function is not given — and which would emit them twice if
/// a second caller could trigger them.
///
/// The platform surface is NOT consulted here. Resolving it needs a
/// [`crate::method_lookup::RefineCtx`] built from a body, which navigation has in
/// a different form; each caller keeps that step.
pub fn resolve_user_call(
    db: &dyn HirDatabase,
    receiver_ty: TypeId,
    method_name: &Name,
    resolver: &Resolver,
) -> UserCallTarget {
    use bsl_types::kind::TypeKind;

    let coerced =
        crate::this_object::coerce_to_metadata_ref_id(db, receiver_ty).unwrap_or(receiver_ty);

    // A body nobody read cannot answer, but it also cannot be reported as absent:
    // the route that hit it stays a "maybe" while the remaining routes are tried.
    let mut unread = false;

    let mut consider = |result: Result<MethodResolution, UnresolvedMethodKind>,
                        route: UserCallRoute,
                        origin: &str|
     -> Option<(UserCallRoute, MethodResolution)> {
        match result {
            Ok(resolution) => Some((route, resolution)),
            Err(UnresolvedMethodKind::MethodNotFound) => None,
            Err(UnresolvedMethodKind::BodyUnread) => {
                unread = true;
                None
            }
            Err(
                kind @ (UnresolvedMethodKind::MethodNotExport
                | UnresolvedMethodKind::SelfMethodNotExport
                | UnresolvedMethodKind::ReceiverNotResolved
                | UnresolvedMethodKind::ReceiverNameAbsent),
            ) => {
                unreachable!("{origin} returned unexpected kind: {kind:?}")
            }
        }
    };

    match db.lookup_type(coerced) {
        TypeKind::MetadataRef(facet) => {
            let mdo_name = Name::new(&facet.name);
            // The two kind mappers accept disjoint sets of `MetadataKind`, so the
            // order between these two decides nothing.
            if let Some((route, resolution)) = consider(
                resolve_object_module_call(db, facet.kind, &mdo_name, method_name, resolver),
                UserCallRoute::ObjectModule,
                "resolve_object_module_call",
            ) {
                return UserCallTarget::Method(UserCall { route, resolution, mdo_name });
            }
            if let Some((route, resolution)) = consider(
                resolve_record_set_module_call(db, facet.kind, &mdo_name, method_name, resolver),
                UserCallRoute::RecordSetModule,
                "resolve_record_set_module_call",
            ) {
                return UserCallTarget::Method(UserCall { route, resolution, mdo_name });
            }
        }
        TypeKind::ObjectManager(facet) => {
            let mdo_name = Name::new(&facet.name);
            if let Some((route, resolution)) = consider(
                resolve_aliased_manager_call(db, facet.mdo, &mdo_name, method_name, resolver),
                UserCallRoute::ManagerModule,
                "resolve_aliased_manager_call",
            ) {
                return UserCallTarget::Method(UserCall { route, resolution, mdo_name });
            }
        }
        _ => {}
    }

    if unread {
        UserCallTarget::BodyUnread
    } else {
        UserCallTarget::NotUserMethod
    }
}

pub(crate) fn materialise_signature(
    db: &dyn TypeKernelDb,
    method_symbol: &MethodDecl,
) -> FunctionSignature {
    let ctx = TyLoweringContext::new();
    // Absent documentation is the common case, and then nothing below changes a single type.
    let docs = method_symbol.docs.as_deref();

    let mut param_sees: Vec<bool> = Vec::with_capacity(method_symbol.params.len());
    let params: Box<[TypeId]> = method_symbol
        .params
        .iter()
        .map(|p| {
            let base =
                p.type_ref.as_ref().map(|t| ctx.lower_type_ref_id(db, t)).unwrap_or(db.unknown());
            let documented = docs
                .and_then(|docs| find_param_doc(docs, &p.name))
                .map(|param_doc| param_doc.types.as_slice());
            let (ty, sees_target) = enrich_with_documented_structure(db, base, documented);
            param_sees.push(sees_target);
            ty
        })
        .collect();
    let defaults: Box<[bool]> = method_symbol.params.iter().map(|p| p.has_default).collect();

    let ret = method_symbol
        .return_type_ref
        .as_ref()
        .map(|t| ctx.lower_type_ref_id(db, t))
        .unwrap_or_else(|| if method_symbol.is_function { db.unknown() } else { db.undefined() });
    let (ret, ret_sees_target) =
        enrich_with_documented_structure(db, ret, docs.map(|docs| docs.returned_value.as_slice()));

    let max_args = Some(params.len() as u32);
    let doc_see = DocSeeSlots { ret: ret_sees_target, params: param_sees.into() };
    FunctionSignature { params, defaults, ret, max_args, from_doc_comment: true, doc_see }
}

/// Puts the fields a doc-comment declares into the slot's already lowered type, and reports
/// whether the documentation pointed at another method (`см. Модуль.Метод`).
///
/// Documentation is advisory here: it may only fill in the fields of a structure the declared type
/// already carries. When the slot declares something else, or the documentation names no inline
/// structure, the lowered type is returned untouched.
///
/// The reference is reported from this one parse rather than re-read from the text later. Two
/// parsers already read these comments and have disagreed before; a third would disagree again,
/// silently, in whichever form only one of them recognises.
fn enrich_with_documented_structure(
    db: &dyn TypeKernelDb,
    base: TypeId,
    documented: Option<&[hir_def::docs::TypeDoc]>,
) -> (TypeId, bool) {
    let Some(documented) = documented else {
        return (base, false);
    };
    let mut enriched = base;
    let mut sees_target = false;
    for type_doc in documented {
        let Some(expr) = hir_def::docs::parse_type_expr(type_doc) else {
            continue;
        };
        sees_target |= expr.names_documentation_target();
        if let Some(documented) = crate::lower::doc_structure::doc_structure_ty(
            db,
            &expr,
            &crate::lower::doc_structure::SeePolicy::Permissive,
        ) {
            enriched = crate::lower::doc_structure::substitute(db, enriched, &documented);
        }
    }
    (enriched, sees_target)
}

/// Matched by name without regard to case, the same way the syntactic parameter hints are.
fn find_param_doc<'a>(
    docs: &'a hir_def::docs::MethodDocs,
    name: &hir_def::Name,
) -> Option<&'a hir_def::docs::ParameterDoc> {
    use stdx::case::CaseExt;
    let needle = name.as_str().fold_lower();
    docs.parameters.iter().find(|param| param.name.fold_lower() == needle)
}

pub(crate) fn materialise_signature_enriched(
    db: &dyn HirDatabase,
    method_id: hir_def::MethodId,
    method_symbol: &MethodDecl,
) -> FunctionSignature {
    let mut sig: FunctionSignature = materialise_signature(db, method_symbol);
    // A slot documented as a bare `Структура` says nothing the body does not already say, and the
    // keys the body proves are the only thing anyone can use. Reading the body for it keeps the
    // rule that documentation adds and never removes.
    let unknown = sig.ret == db.unknown();
    // The untyped structure may stand alone or in a union arm — `Неопределено, Структура` is how an
    // optional result is written — and both must be filled, or documenting the obvious still costs
    // the caller the keys.
    if unknown || crate::lower::doc_structure::has_bare_untyped_structure(db, sig.ret) {
        let method_input = hir_def::MethodIdInput::new(db, method_id);
        let inferred = crate::method_graph::method_return_type_query(db, method_input);
        if unknown {
            if inferred != db.unknown() {
                sig.ret = inferred;
            }
        } else {
            // The body proves its keys on the path that returns a structure; the other paths of an
            // optional result contribute nothing to the documented structure and are left out.
            let proven = crate::lower::doc_structure::structures_with_fields(db, inferred);
            if !proven.is_empty() {
                let structure = if proven.len() == 1 { proven[0] } else { db.union(proven) };
                // Only the untyped structure is replaced: an arm the documentation declares as
                // something else, and a structure documented under a collection, stay as declared.
                sig.ret = crate::lower::doc_structure::substitute_bare(db, sig.ret, structure);
            }
        }
    }

    sig
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::testing::InMemoryDb;

    #[test]
    fn test_method_resolution_new() {
        let db = InMemoryDb::new();
        let method_id = MethodId {
            module: hir_def::ModuleId { file_id: FileId(0) },
            local_id: hir_def::MethodKey::first("М"),
        };
        let signature = FunctionSignature {
            params: Box::new([db.string(None, false)]),
            defaults: Box::new([false]),
            ret: db.number(None, None),
            max_args: Some(1),
            from_doc_comment: true,
            doc_see: Default::default(),
        };

        let resolution = MethodResolution::new(method_id, true, signature.clone());

        assert_eq!(resolution.method_id, method_id);
        assert!(resolution.is_export);
        assert_eq!(resolution.return_type, db.number(None, None));
        assert_eq!(resolution.signature, signature);
    }

    #[test]
    fn test_method_resolution_not_export() {
        let db = InMemoryDb::new();
        let method_id = MethodId {
            module: hir_def::ModuleId { file_id: FileId(0) },
            local_id: hir_def::MethodKey::first("М"),
        };
        let signature = FunctionSignature {
            params: Box::new([]),
            defaults: Box::new([]),
            ret: db.undefined(),
            max_args: Some(0),
            from_doc_comment: true,
            doc_see: Default::default(),
        };

        let resolution = MethodResolution::new(method_id, false, signature);

        assert!(!resolution.is_export);
    }

    /// A method declaration shaped the way `module_interface` builds one: the parameter's `type_ref` is the
    /// hint the second doc-parser produced, which is what `materialise_signature` lowers.
    fn method_with_docs(
        docs: hir_def::docs::MethodDocs,
        params: &[(&str, Option<hir_def::TypeRef>)],
    ) -> MethodDecl {
        use hir_def::symbol_tree::ParamSymbol;
        MethodDecl {
            id: MethodId {
                module: hir_def::ModuleId { file_id: FileId(0) },
                local_id: hir_def::MethodKey::first("М"),
            },
            name: Name::new("Тест"),
            is_function: true,
            is_export: true,
            params: params
                .iter()
                .map(|(name, type_ref)| ParamSymbol {
                    name: Name::new(name),
                    is_val: false,
                    has_default: false,
                    type_ref: type_ref.clone(),
                })
                .collect(),
            directives: Box::new([]),
            preproc_env: hir_def::execution_env::EnvFlags::ALL,
            preproc_undecided: hir_def::execution_env::EnvFlags::EMPTY,
            docs: Some(std::sync::Arc::new(docs)),
            return_type_ref: None,
        }
    }

    fn slot(name: &str) -> hir_def::docs::TypeDoc {
        hir_def::docs::TypeDoc::simple(name.to_string(), None)
    }

    #[test]
    fn the_signature_records_which_slots_name_a_documentation_target() {
        // The record is what tier 2 is allowed to ask about, so it must separate a reference from
        // everything else that lowers to the same permissive type. `Произвольный` is the input
        // that makes the difference visible: it and an unresolved reference are the same type
        // afterwards, and only the parse can still tell them apart.
        let db = InMemoryDb::new();
        let mut docs = hir_def::docs::MethodDocs::empty();
        docs.parameters = vec![
            hir_def::docs::ParameterDoc::new("Ссылка".into(), vec![slot("см. База.Создать")]),
            hir_def::docs::ParameterDoc::new("Любой".into(), vec![slot("Произвольный")]),
            hir_def::docs::ParameterDoc::new("Проза".into(), vec![slot("см. в описании")]),
        ];
        docs.returned_value = vec![slot("см. База.Создать.Настройки")];
        // All three lower to `Any` — a reference, `Произвольный` and prose reach the same hint.
        let any = Some(hir_def::TypeRef::Any);
        let symbol = method_with_docs(
            docs,
            &[("Ссылка", any.clone()), ("Любой", any.clone()), ("Проза", any)],
        );

        let signature = materialise_signature(&db, &symbol);

        assert!(signature.doc_see.ret, "трёхсегментная ссылка на возврате");
        assert!(signature.doc_see.param(0), "ссылка в параметре");
        assert!(!signature.doc_see.param(1), "Произвольный целью не является");
        assert!(!signature.doc_see.param(2), "проза целью не является");
        // The positive control for the whole record: the three parameters lower to ONE type, so
        // the record demonstrably cannot be read back off the lowered types.
        assert_eq!(signature.params[0], db.any());
        assert_eq!(signature.params[1], db.any());
        assert_eq!(signature.params[2], db.any());
    }

    /// `Имя - Структура - см. Модуль.Метод`: the marker stands in the slot's DESCRIPTION, and
    /// the slot names a target all the same — the declared `Структура` is a placeholder for the
    /// keys documented at the target (1 244 such slots in one configuration).
    #[test]
    fn a_marker_in_the_description_is_recorded_as_a_target() {
        let db = InMemoryDb::new();
        let mut docs = hir_def::docs::MethodDocs::empty();
        docs.parameters = vec![hir_def::docs::ParameterDoc::new(
            "Параметры".into(),
            vec![hir_def::docs::TypeDoc::simple(
                "Структура".to_string(),
                Some("см. ОбщегоНазначенияКлиентСервер.ПараметрыЗаписи".to_string()),
            )],
        )];
        let symbol = method_with_docs(
            docs,
            &[(
                "Параметры",
                Some(hir_def::TypeRef::Builtin(hir_def::type_ref::BuiltinTypeRef::Structure)),
            )],
        );

        let signature = materialise_signature(&db, &symbol);

        assert!(
            signature.doc_see.param(0),
            "маркер в описании обязан назвать цель, как и форма с маркером в имени"
        );
    }

    #[test]
    fn a_signature_without_references_records_none() {
        // Tier 1 behaviour must be unchanged where no reference occurs, and the record must not
        // spread to slots that merely carry documentation.
        let db = InMemoryDb::new();
        let mut docs = hir_def::docs::MethodDocs::empty();
        docs.parameters = vec![hir_def::docs::ParameterDoc::new(
            "Данные".into(),
            vec![hir_def::docs::TypeDoc::structured(
                "Структура:".to_string(),
                None,
                vec![hir_def::docs::ParameterDoc::new("Имя".into(), vec![slot("Строка")])],
            )],
        )];
        let symbol = method_with_docs(
            docs,
            &[(
                "Данные",
                Some(hir_def::TypeRef::Builtin(hir_def::type_ref::BuiltinTypeRef::Structure)),
            )],
        );

        let signature = materialise_signature(&db, &symbol);

        assert!(signature.doc_see.is_empty(), "ни один слот не называет цель");
        // Positive control: the documented structure still carries its field, so the assertion
        // above is not green merely because the documentation stopped being read.
        let bsl_types::kind::TypeKind::Structure(facet) = db.lookup_type(signature.params[0])
        else {
            panic!("ожидалась структура, получено {:?}", db.lookup_type(signature.params[0]));
        };
        let fields = facet.fields.as_ref().expect("документированные поля должны сохраниться");
        assert_eq!(fields.fields.len(), 1);
        assert_eq!(fields.fields[0].name, "Имя");
    }

    #[test]
    fn a_field_documented_as_arbitrary_is_the_top_type() {
        // Teaching the structured doc-parser this word changed tier 1 too: the field used to lower
        // to `Unknown` through an unrecognised name. The direction is a widening — `Any` dominates
        // a union where `Unknown` was discarded — so no slot narrows, but the change is real and
        // is pinned here rather than left to be discovered.
        let db = InMemoryDb::new();
        let mut docs = hir_def::docs::MethodDocs::empty();
        docs.parameters = vec![hir_def::docs::ParameterDoc::new(
            "Данные".into(),
            vec![hir_def::docs::TypeDoc::structured(
                "Структура:".to_string(),
                None,
                vec![hir_def::docs::ParameterDoc::new(
                    "Значение".into(),
                    vec![slot("Произвольный")],
                )],
            )],
        )];
        let symbol = method_with_docs(
            docs,
            &[(
                "Данные",
                Some(hir_def::TypeRef::Builtin(hir_def::type_ref::BuiltinTypeRef::Structure)),
            )],
        );

        let signature = materialise_signature(&db, &symbol);

        let bsl_types::kind::TypeKind::Structure(facet) = db.lookup_type(signature.params[0])
        else {
            panic!("ожидалась структура");
        };
        let fields = facet.fields.as_ref().expect("поле документировано");
        assert_eq!(fields.fields[0].ty, db.any());
    }

    #[test]
    fn object_kind_to_mdo_accepts_only_object_variants() {
        use bsl_metadata::MdoType;
        use hir_def::ty::MetadataKind;
        assert_eq!(object_kind_to_mdo(MetadataKind::CatalogObject), Some(MdoType::Catalog));
        assert_eq!(object_kind_to_mdo(MetadataKind::DocumentObject), Some(MdoType::Document));
        assert_eq!(
            object_kind_to_mdo(MetadataKind::ExchangePlanObject),
            Some(MdoType::ExchangePlan),
        );
        assert_eq!(
            object_kind_to_mdo(MetadataKind::ChartOfAccountsObject),
            Some(MdoType::ChartOfAccounts),
        );
        assert_eq!(object_kind_to_mdo(MetadataKind::CatalogRef), None);
        assert_eq!(object_kind_to_mdo(MetadataKind::DocumentRef), None);
        assert_eq!(object_kind_to_mdo(MetadataKind::InformationRegisterRecordManager), None);
        assert_eq!(object_kind_to_mdo(MetadataKind::AccumulationRegisterRecordSet), None);
    }

    #[test]
    fn record_set_kind_to_mdo_accepts_only_register_set_variants() {
        use bsl_metadata::MdoType;
        use hir_def::ty::MetadataKind;
        assert_eq!(
            record_set_kind_to_mdo(MetadataKind::InformationRegisterRecordSet),
            Some(MdoType::InformationRegister),
        );
        assert_eq!(
            record_set_kind_to_mdo(MetadataKind::AccumulationRegisterRecordSet),
            Some(MdoType::AccumulationRegister),
        );
        assert_eq!(
            record_set_kind_to_mdo(MetadataKind::AccountingRegisterRecordSet),
            Some(MdoType::AccountingRegister),
        );
        assert_eq!(
            record_set_kind_to_mdo(MetadataKind::CalculationRegisterRecordSet),
            Some(MdoType::CalculationRegister),
        );
        assert_eq!(record_set_kind_to_mdo(MetadataKind::InformationRegisterRecordManager), None);
        assert_eq!(
            record_set_kind_to_mdo(MetadataKind::RegisterFilter {
                parent: MdoType::InformationRegister,
            }),
            None,
        );
        assert_eq!(record_set_kind_to_mdo(MetadataKind::CatalogObject), None);
        assert_eq!(record_set_kind_to_mdo(MetadataKind::DocumentObject), None);
        assert_eq!(record_set_kind_to_mdo(MetadataKind::CatalogRef), None);
        assert_eq!(record_set_kind_to_mdo(MetadataKind::InformationRegisterRef), None);
        assert_eq!(record_set_kind_to_mdo(MetadataKind::AccumulationRegisterRef), None);
        assert_eq!(record_set_kind_to_mdo(MetadataKind::AccountingRegisterRef), None);
        assert_eq!(record_set_kind_to_mdo(MetadataKind::CalculationRegisterRef), None);
    }
}
