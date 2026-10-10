use std::hash::{Hash, Hasher};

use bsl_metadata::MdoType;
use bsl_platform::{
    platform_method_query, prefixed_method_query, MethodLookupInput, PlatformMethod,
    PrefixedMethodLookupInput,
};
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{MetadataKind, TypeId, TypeKind};
use hir_def::execution_env::EnvFlags;
use hir_def::Name;
use smol_str::SmolStr;

use crate::call_resolution::CallCandidateSet;
use crate::method_lookup::{
    build_tabular_section_method_info, platform_type_key_id, to_method_info, MethodInfo,
};
use crate::platform_manager_lookup::{
    build_resolution, metadata_kind_to_prefix_and_mdo, object_kind_to_parent_mdo,
};

#[derive(Debug, Clone)]
pub struct PlatformMethodHandle {
    pub method_id: u32,
    pub origin: PlatformMethodOrigin,
}

impl PartialEq for PlatformMethodHandle {
    fn eq(&self, other: &Self) -> bool {
        self.method_id == other.method_id
    }
}

impl Eq for PlatformMethodHandle {}

impl Hash for PlatformMethodHandle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.method_id.hash(state);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlatformMethodOrigin {
    Scalar { type_name: SmolStr },
    Prefixed { prefix: SmolStr, mdo_type: MdoType, mdo_name: Name },
}

impl PlatformMethodHandle {
    pub fn lookup(&self, db: &dyn salsa::Database) -> Option<PlatformMethod> {
        let _ = db;
        match &self.origin {
            PlatformMethodOrigin::Scalar { type_name } => {
                let method = bsl_platform::PlatformDataInner::instance()
                    .get_type_methods(type_name.as_str())
                    .into_iter()
                    .find(|m| m.id == self.method_id)
                    .cloned();
                if method.is_some() {
                    return method;
                }
                bsl_platform::PlatformDataInner::instance()
                    .all_methods()
                    .iter()
                    .find(|m| m.id == self.method_id)
                    .cloned()
            }
            PlatformMethodOrigin::Prefixed { prefix, .. } => {
                bsl_platform::PlatformDataInner::instance()
                    .get_manager_methods(prefix.as_str())
                    .into_iter()
                    .find(|m| m.id == self.method_id)
                    .cloned()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPlatformMethod {
    pub handle: PlatformMethodHandle,
    pub return_ty: TypeId,
    pub candidates: CallCandidateSet,
    pub env: EnvFlags,
}

impl ResolvedPlatformMethod {
    pub fn into_method_info(self) -> MethodInfo {
        MethodInfo { return_ty: self.return_ty, candidates: self.candidates, env: self.env }
    }
}

pub fn resolve_method(
    db: &dyn crate::db::HirDatabase,
    receiver: TypeId,
    method_name: &Name,
) -> Option<ResolvedPlatformMethod> {
    resolve_method_inner(db, db, receiver, method_name)
}

fn resolve_method_inner(
    salsa_db: &dyn salsa::Database,
    kernel_db: &dyn TypeKernelDb,
    receiver: TypeId,
    method_name: &Name,
) -> Option<ResolvedPlatformMethod> {
    let receiver =
        crate::this_object::coerce_to_metadata_ref_id(kernel_db, receiver).unwrap_or(receiver);

    if let TypeKind::Union(members) = kernel_db.lookup_type(receiver) {
        let live: Vec<TypeId> = members
            .iter()
            .copied()
            .filter(|m| !matches!(kernel_db.lookup_type(*m), TypeKind::Undefined | TypeKind::Null))
            .collect();
        let mut returns: Vec<TypeId> = Vec::with_capacity(live.len());
        let mut signatures = Vec::new();
        let mut chosen: Option<ResolvedPlatformMethod> = None;
        let mut env = EnvFlags::EMPTY;
        for member in live {
            if let Some(res) = resolve_method_inner(salsa_db, kernel_db, member, method_name) {
                returns.push(res.return_ty);
                env = env | res.env;
                signatures.extend(res.candidates.as_slice().iter().cloned());
                if chosen.is_none() {
                    chosen = Some(res);
                }
            }
        }
        let mut result = chosen?;
        result.return_ty = kernel_db.union(returns);
        result.candidates = CallCandidateSet::merge_by_id(kernel_db, signatures).ok()?;
        result.env = env;
        return Some(result);
    }

    match kernel_db.lookup_type(receiver) {
        TypeKind::ObjectManager(facet) => {
            let prefix = facet.mdo.manager_type_prefix()?;
            let method = lookup_prefixed(salsa_db, prefix, method_name.as_str())?;
            let mdo_name = Name::new(facet.name.as_str());
            let resolution = build_resolution(kernel_db, method, facet.mdo, &mdo_name);
            Some(ResolvedPlatformMethod {
                handle: PlatformMethodHandle {
                    method_id: method.id,
                    origin: PlatformMethodOrigin::Prefixed {
                        prefix: SmolStr::from(prefix),
                        mdo_type: facet.mdo,
                        mdo_name,
                    },
                },
                return_ty: resolution.return_ty,
                candidates: resolution.candidates,
                env: resolution.env,
            })
        }
        TypeKind::MetadataRef(facet) => resolve_metadata_ref(
            salsa_db,
            kernel_db,
            facet.kind,
            &Name::new(facet.name.as_str()),
            method_name,
        ),
        _ => {
            let key = platform_type_key_id(kernel_db, receiver)?;
            let method = lookup_scalar(salsa_db, &key, method_name.as_str())?;
            let mut info = to_method_info(kernel_db, method);
            // Same homonym caveat as `lookup_scalar_receiver`: availability
            // resolved through an ambiguous name is unreliable.
            if bsl_platform::PlatformDataInner::instance().is_ambiguous_type_name(&key) {
                info.env = EnvFlags::ALL;
            }
            Some(ResolvedPlatformMethod {
                handle: PlatformMethodHandle {
                    method_id: method.id,
                    origin: PlatformMethodOrigin::Scalar { type_name: SmolStr::from(key) },
                },
                return_ty: info.return_ty,
                candidates: info.candidates,
                env: info.env,
            })
        }
    }
}

fn resolve_metadata_ref(
    salsa_db: &dyn salsa::Database,
    kernel_db: &dyn TypeKernelDb,
    kind: MetadataKind,
    mdo_name: &Name,
    method_name: &Name,
) -> Option<ResolvedPlatformMethod> {
    if let MetadataKind::TabularSection { parent } = kind {
        let method = lookup_scalar(salsa_db, "Tabular section", method_name.as_str())?;
        let info = build_tabular_section_method_info(kernel_db, method, parent, mdo_name);
        return Some(ResolvedPlatformMethod {
            handle: PlatformMethodHandle {
                method_id: method.id,
                origin: PlatformMethodOrigin::Scalar {
                    type_name: SmolStr::from("Tabular section"),
                },
            },
            return_ty: info.return_ty,
            candidates: info.candidates,
            env: info.env,
        });
    }

    // An external object's methods live under its bare platform type, which the
    // prefix index of dotted `type_name`s never sees; inference takes the same
    // detour, and navigation must land on the same method.
    if let Some(type_name) = kind.bare_platform_type() {
        let parent_mdo = object_kind_to_parent_mdo(kind)?;
        let method = lookup_scalar(salsa_db, type_name, method_name.as_str())?;
        let resolution = build_resolution(kernel_db, method, parent_mdo, mdo_name);
        return Some(ResolvedPlatformMethod {
            handle: PlatformMethodHandle {
                method_id: method.id,
                origin: PlatformMethodOrigin::Scalar { type_name: SmolStr::from(type_name) },
            },
            return_ty: resolution.return_ty,
            candidates: resolution.candidates,
            env: resolution.env,
        });
    }

    if let Some((prefix, parent_mdo)) = metadata_kind_to_prefix_and_mdo(kind) {
        if let Some(method) = lookup_prefixed(salsa_db, prefix, method_name.as_str()) {
            let resolution = build_resolution(kernel_db, method, parent_mdo, mdo_name);
            return Some(ResolvedPlatformMethod {
                handle: PlatformMethodHandle {
                    method_id: method.id,
                    origin: PlatformMethodOrigin::Prefixed {
                        prefix: SmolStr::from(prefix),
                        mdo_type: parent_mdo,
                        mdo_name: mdo_name.clone(),
                    },
                },
                return_ty: resolution.return_ty,
                candidates: resolution.candidates,
                env: resolution.env,
            });
        }
    }

    if let Some(scalar_key) = kind.scalar_platform_key() {
        if let Some(method) = lookup_scalar(salsa_db, scalar_key, method_name.as_str()) {
            let info = to_method_info(kernel_db, method);
            return Some(ResolvedPlatformMethod {
                handle: PlatformMethodHandle {
                    method_id: method.id,
                    origin: PlatformMethodOrigin::Scalar { type_name: SmolStr::from(scalar_key) },
                },
                return_ty: info.return_ty,
                candidates: info.candidates,
                env: info.env,
            });
        }
    }

    None
}

fn lookup_scalar<'db>(
    db: &'db dyn salsa::Database,
    type_name: &str,
    method_name: &str,
) -> Option<&'db PlatformMethod> {
    let input = MethodLookupInput::new(db, type_name.to_string(), method_name.to_string());
    platform_method_query(db, input)
}

fn lookup_prefixed<'db>(
    db: &'db dyn salsa::Database,
    prefix: &str,
    method_name: &str,
) -> Option<&'db PlatformMethod> {
    let input = PrefixedMethodLookupInput::new(db, prefix.to_string(), method_name.to_string());
    prefixed_method_query(db, input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::testing::{InMemoryDb, RootConfigCtx};

    #[salsa::db]
    #[derive(Clone, Default)]
    struct TestDatabase {
        storage: salsa::Storage<Self>,
    }

    impl salsa::Database for TestDatabase {}

    fn db() -> TestDatabase {
        TestDatabase::default()
    }

    fn resolve_for_test(
        db: &TestDatabase,
        kdb: &InMemoryDb,
        receiver: TypeId,
        method_name: &Name,
    ) -> Option<ResolvedPlatformMethod> {
        resolve_method_inner(db, kdb, receiver, method_name)
    }

    #[test]
    fn metadata_ref_record_set_resolves_with_prefixed_handle() {
        let db = db();
        let kdb = InMemoryDb::new();
        let receiver = kdb.metadata_ref(
            MetadataKind::InformationRegisterRecordSet,
            "Курсы".to_string(),
            &RootConfigCtx,
        );
        let res = resolve_for_test(&db, &kdb, receiver, &Name::new("Прочитать"));
        let Some(res) = res else {
            println!("Skipping: no platform data available");
            return;
        };
        assert_ne!(res.handle.method_id, 0);
        match &res.handle.origin {
            PlatformMethodOrigin::Prefixed { prefix, mdo_type, mdo_name } => {
                assert_eq!(prefix.as_str(), "InformationRegisterRecordSet");
                assert_eq!(*mdo_type, MdoType::InformationRegister);
                assert_eq!(mdo_name.as_str(), "Курсы");
            }
            PlatformMethodOrigin::Scalar { .. } => {
                panic!("composite prefix kind must use Prefixed origin");
            }
        }
        assert_eq!(res.return_ty, kdb.undefined());
    }

    #[test]
    fn metadata_ref_register_filter_resolves_with_scalar_handle() {
        let db = db();
        let kdb = InMemoryDb::new();
        let receiver = kdb.metadata_ref(
            MetadataKind::RegisterFilter { parent: MdoType::InformationRegister },
            "Курсы".to_string(),
            &RootConfigCtx,
        );
        let res = resolve_for_test(&db, &kdb, receiver, &Name::new("Сбросить"));
        let Some(res) = res else {
            println!("Skipping: no platform data available");
            return;
        };
        assert_ne!(res.handle.method_id, 0);
        match &res.handle.origin {
            PlatformMethodOrigin::Scalar { type_name } => {
                assert_eq!(type_name.as_str(), "Filter");
            }
            PlatformMethodOrigin::Prefixed { .. } => {
                panic!("RegisterFilter must reach Scalar fallback");
            }
        }
    }

    #[test]
    fn handle_eq_keys_on_method_id_only() {
        let h1 = PlatformMethodHandle {
            method_id: 42,
            origin: PlatformMethodOrigin::Scalar { type_name: SmolStr::from("Foo") },
        };
        let h2 = PlatformMethodHandle {
            method_id: 42,
            origin: PlatformMethodOrigin::Prefixed {
                prefix: SmolStr::from("BarPrefix"),
                mdo_type: MdoType::Catalog,
                mdo_name: Name::new("Bar"),
            },
        };
        assert_eq!(h1, h2);
        let mut h1_h = std::collections::hash_map::DefaultHasher::new();
        let mut h2_h = std::collections::hash_map::DefaultHasher::new();
        h1.hash(&mut h1_h);
        h2.hash(&mut h2_h);
        assert_eq!(h1_h.finish(), h2_h.finish());
    }

    #[test]
    fn unknown_method_returns_none_not_panic() {
        let db = db();
        let kdb = InMemoryDb::new();
        let receiver = kdb.metadata_ref(
            MetadataKind::InformationRegisterRecordSet,
            "Курсы".to_string(),
            &RootConfigCtx,
        );
        let res = resolve_for_test(&db, &kdb, receiver, &Name::new("НесуществующийМетод"));
        assert!(res.is_none());
    }

    #[test]
    fn english_name_resolves_bilingually() {
        let db = db();
        let kdb = InMemoryDb::new();
        let receiver = kdb.metadata_ref(
            MetadataKind::InformationRegisterRecordSet,
            "Курсы".to_string(),
            &RootConfigCtx,
        );
        let ru = resolve_for_test(&db, &kdb, receiver, &Name::new("Прочитать"));
        let en = resolve_for_test(&db, &kdb, receiver, &Name::new("Read"));
        match (ru, en) {
            (Some(r), Some(e)) => assert_eq!(r.handle.method_id, e.handle.method_id),
            (None, None) => println!("Skipping: no platform data available"),
            _ => panic!("Russian and English lookups must both succeed or both fail"),
        }
    }

    #[test]
    fn composite_multi_overload_method_populates_candidates() {
        let db = db();
        let kdb = InMemoryDb::new();
        let receiver =
            kdb.object_manager(MdoType::InformationRegister, "Курсы".to_string(), &RootConfigCtx);
        let res = resolve_for_test(&db, &kdb, receiver, &Name::new("Получить"));
        let Some(res) = res else {
            println!("Skipping: no platform data available");
            return;
        };
        assert!(
            res.candidates.as_slice().len() > 1,
            "InformationRegisterManager.Получить must surface all candidates: {:?}",
            res.candidates,
        );
    }

    #[test]
    fn object_manager_resolves_with_prefixed_handle() {
        let db = db();
        let kdb = InMemoryDb::new();
        let receiver =
            kdb.object_manager(MdoType::Catalog, "Номенклатура".to_string(), &RootConfigCtx);
        let res = resolve_for_test(&db, &kdb, receiver, &Name::new("СоздатьЭлемент"));
        let Some(res) = res else {
            println!("Skipping: no platform data available");
            return;
        };
        match &res.handle.origin {
            PlatformMethodOrigin::Prefixed { prefix, mdo_type, mdo_name } => {
                assert_eq!(prefix.as_str(), "CatalogManager");
                assert_eq!(*mdo_type, MdoType::Catalog);
                assert_eq!(mdo_name.as_str(), "Номенклатура");
            }
            PlatformMethodOrigin::Scalar { .. } => {
                panic!("ObjectManager must use Prefixed origin");
            }
        }
        assert_eq!(
            res.return_ty,
            kdb.metadata_ref(
                MetadataKind::CatalogObject,
                "Номенклатура".to_string(),
                &RootConfigCtx,
            )
        );
    }

    #[test]
    fn union_first_branch_owns_handle_returns_unioned() {
        let db = db();
        let kdb = InMemoryDb::new();
        let happy = kdb.platform_object("РезультатЗапроса".to_string());
        let union = kdb.union(vec![happy, kdb.undefined()]);
        let direct = resolve_for_test(&db, &kdb, happy, &Name::new("Выгрузить"));
        let through_union = resolve_for_test(&db, &kdb, union, &Name::new("Выгрузить"));
        match (direct, through_union) {
            (Some(direct_res), Some(union_res)) => {
                assert_eq!(direct_res.handle.method_id, union_res.handle.method_id);
                assert_eq!(union_res.return_ty, direct_res.return_ty);
            }
            (None, None) => {
                println!("Skipping: no platform data available");
            }
            _ => panic!("Direct and union routes must agree on success/failure"),
        }
    }

    #[test]
    fn union_strips_undefined_and_null_sentinels() {
        let db = db();
        let kdb = InMemoryDb::new();
        let dead = kdb.union(vec![kdb.undefined(), kdb.null()]);
        let res = resolve_for_test(&db, &kdb, dead, &Name::new("ЛюбоеИмя"));
        assert!(res.is_none());
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn into_method_info_preserves_candidates_and_drops_handle() {
        let salsa_db = db();
        let db = InMemoryDb::new();
        let receiver = db.platform_object("XBase".to_string());
        let res = resolve_for_test(&salsa_db, &db, receiver, &Name::new("Найти"))
            .expect("XBase.Find must resolve");
        let return_ty = res.return_ty;
        let candidates = res.candidates.clone();
        let info = res.into_method_info();
        assert_eq!(info.return_ty, return_ty);
        assert_eq!(info.candidates, candidates);
    }
}
