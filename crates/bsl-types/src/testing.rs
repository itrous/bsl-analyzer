use std::cell::RefCell;

use elsa::FrozenVec;
use rustc_hash::FxHashMap;

use crate::intern::{canonicalise, TypeKernelDb};
use crate::kind::{TypeId, TypeKind};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Sentinels {
    pub unknown: u64,
    pub never: u64,
    pub any: u64,
    pub null: u64,
    pub undefined: u64,
    pub boolean: u64,
}

pub struct InMemoryDb {
    kinds: FrozenVec<Box<TypeKind>>,
    intern: RefCell<FxHashMap<TypeKind, u64>>,
    sentinel: Sentinels,
}

impl Default for InMemoryDb {
    fn default() -> Self {
        Self::new()
    }
}

impl InMemoryDb {
    pub fn new() -> Self {
        let kinds = FrozenVec::new();
        let mut intern = FxHashMap::default();

        let mut seed = |kind: TypeKind| -> u64 {
            let id = kinds.len() as u64;
            kinds.push(Box::new(kind.clone()));
            intern.insert(kind, id);
            id
        };

        let sentinel = Sentinels {
            unknown: seed(TypeKind::Unknown),
            never: seed(TypeKind::Never),
            any: seed(TypeKind::Any),
            null: seed(TypeKind::Null),
            undefined: seed(TypeKind::Undefined),
            boolean: seed(TypeKind::Boolean),
        };

        Self { kinds, intern: RefCell::new(intern), sentinel }
    }

    pub fn unknown(&self) -> TypeId {
        TypeId(self.sentinel.unknown)
    }

    pub fn never(&self) -> TypeId {
        TypeId(self.sentinel.never)
    }

    pub fn any(&self) -> TypeId {
        TypeId(self.sentinel.any)
    }

    pub fn null(&self) -> TypeId {
        TypeId(self.sentinel.null)
    }

    pub fn undefined(&self) -> TypeId {
        TypeId(self.sentinel.undefined)
    }

    pub fn boolean(&self) -> TypeId {
        TypeId(self.sentinel.boolean)
    }

    pub fn len(&self) -> usize {
        self.kinds.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kinds.len() == self.sentinel_count()
    }

    pub(crate) fn sentinel_count(&self) -> usize {
        6
    }
}

impl TypeKernelDb for InMemoryDb {
    fn intern_type(&self, kind: TypeKind) -> TypeId {
        let canon = canonicalise(self, kind);

        if let Some(&id) = self.intern.borrow().get(&canon) {
            return TypeId(id);
        }

        let id = self.kinds.len() as u64;
        self.kinds.push(Box::new(canon.clone()));
        self.intern.borrow_mut().insert(canon, id);
        TypeId(id)
    }

    fn lookup_type(&self, id: TypeId) -> &TypeKind {
        self.kinds
            .get(id.raw() as usize)
            .expect("TypeId out of range: caller mixed IDs across db instances or fabricated a TypeId directly")
    }
}

pub struct RootConfigCtx;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facet::NumberFacet;

    #[test]
    fn sentinels_are_distinct_and_preseeded() {
        let db = InMemoryDb::new();
        let ids = [db.unknown(), db.never(), db.any(), db.null(), db.undefined(), db.boolean()];
        for (i, &a) in ids.iter().enumerate() {
            for (j, &b) in ids.iter().enumerate().skip(i + 1) {
                assert_ne!(a, b, "sentinel {} and sentinel {} aliased to the same id", i, j);
            }
        }
        assert_eq!(db.len(), 6, "exactly six sentinels preseeded");
        assert!(db.is_empty(), "no non-sentinel types yet");
    }

    #[test]
    fn lookup_sentinels_round_trip() {
        let db = InMemoryDb::new();
        assert_eq!(db.lookup_type(db.unknown()), &TypeKind::Unknown);
        assert_eq!(db.lookup_type(db.never()), &TypeKind::Never);
        assert_eq!(db.lookup_type(db.any()), &TypeKind::Any);
        assert_eq!(db.lookup_type(db.null()), &TypeKind::Null);
        assert_eq!(db.lookup_type(db.undefined()), &TypeKind::Undefined);
        assert_eq!(db.lookup_type(db.boolean()), &TypeKind::Boolean);
    }

    #[test]
    fn intern_dedupes_equal_kinds() {
        let db = InMemoryDb::new();
        let a = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        let b = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        assert_eq!(a, b);
    }

    #[test]
    fn intern_returns_preseeded_sentinel_for_unknown() {
        let db = InMemoryDb::new();
        let from_intern = db.intern_type(TypeKind::Unknown);
        assert_eq!(from_intern, db.unknown());
        assert_eq!(db.len(), 6, "no new slot allocated");
    }

    #[test]
    fn distinct_kinds_get_distinct_ids() {
        let db = InMemoryDb::new();
        let a = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        let b = db.intern_type(TypeKind::Number(NumberFacet::with_scale(20, 4)));
        assert_ne!(a, b);
    }

    #[test]
    fn lookup_borrow_stays_valid_across_pushes() {
        let db = InMemoryDb::new();
        let id_a = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        let ref_a = db.lookup_type(id_a);

        for p in 0..16 {
            db.intern_type(TypeKind::Number(NumberFacet::with_precision(p)));
        }

        assert_eq!(ref_a, &TypeKind::Number(NumberFacet::with_scale(15, 2)));
    }
}

#[cfg(test)]
mod canon_tests {
    use std::sync::Arc;

    use bsl_metadata::MdoType;

    use super::*;
    use crate::facet::{
        FormBindingFacet, FormBindingTargetFacet, FormDataFacet, FormElementFacet, MdoRefFacet,
        NumberFacet,
    };
    use crate::kind::TypeOrigin;

    #[test]
    fn provenance_stripped_on_number() {
        let db = InMemoryDb::new();
        let a = db.intern_type(TypeKind::Number(NumberFacet {
            precision: Some(15),
            scale: Some(2),
            origin: Some(TypeOrigin::SdblCast),
        }));
        let b = db.intern_type(TypeKind::Number(NumberFacet {
            precision: Some(15),
            scale: Some(2),
            origin: Some(TypeOrigin::BslLiteral),
        }));
        assert_eq!(a, b);
        let c = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        assert_eq!(a, c);
    }

    #[test]
    fn intern_is_idempotent() {
        let db = InMemoryDb::new();
        let kind = TypeKind::Number(NumberFacet::with_scale(15, 2));
        let a = db.intern_type(kind.clone());
        let b = db.intern_type(kind.clone());
        let c = db.intern_type(kind);
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn form_and_this_variants_intern_idempotently() {
        let db = InMemoryDb::new();
        let owner =
            MdoRefFacet { mdo_type: MdoType::Catalog, name: "Контрагенты".to_string() };
        let attr_ty = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        let binding = FormBindingFacet {
            path: Arc::from(["Объект".to_string(), "Цена".to_string()]),
            target: FormBindingTargetFacet::Attribute { ty: attr_ty },
        };

        let kinds = [
            TypeKind::FormData { kind: FormDataFacet::Collection, underlying: Some(owner.clone()) },
            TypeKind::FormControl { kind: FormElementFacet::Field, binding: Some(binding) },
            TypeKind::ThisObject { config_id: crate::kind::ConfigId::Root, owner: owner.clone() },
            TypeKind::ThisManager { config_id: crate::kind::ConfigId::Root, owner },
        ];

        for kind in kinds {
            let a = db.intern_type(kind.clone());
            let b = db.intern_type(kind);
            assert_eq!(a, b);
        }
    }

    #[test]
    fn union_with_one_distinct_member_unwraps() {
        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::unsized_()));
        let u = db.intern_type(TypeKind::Union(Arc::from([n, n])));
        assert_eq!(u, n, "Union([X, X]) must collapse to X");
    }

    #[test]
    fn empty_union_collapses_to_unknown() {
        let db = InMemoryDb::new();
        let u = db.intern_type(TypeKind::Union(Arc::from([])));
        assert_eq!(u, db.unknown());
    }

    #[test]
    fn union_absorbs_unknown() {
        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::unsized_()));
        let u = db.intern_type(TypeKind::Union(Arc::from([db.unknown(), n])));
        assert_eq!(u, n);
    }

    #[test]
    fn union_drops_never() {
        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::unsized_()));
        let u = db.intern_type(TypeKind::Union(Arc::from([db.never(), n])));
        assert_eq!(u, n);
    }

    #[test]
    fn union_dominated_by_any() {
        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::unsized_()));
        let u = db.intern_type(TypeKind::Union(Arc::from([db.any(), n])));
        assert_eq!(u, db.any());
    }

    #[test]
    fn union_sort_canonicalises_order() {
        let db = InMemoryDb::new();
        let a = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        let b = db.intern_type(TypeKind::String(crate::facet::StringFacet::unsized_()));
        let u1 = db.intern_type(TypeKind::Union(Arc::from([a, b])));
        let u2 = db.intern_type(TypeKind::Union(Arc::from([b, a])));
        assert_eq!(u1, u2);
    }

    #[test]
    fn union_flatten_nested() {
        let db = InMemoryDb::new();
        let a = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        let b = db.intern_type(TypeKind::String(crate::facet::StringFacet::unsized_()));
        let c = db.intern_type(TypeKind::Date(crate::facet::DateFacet::datetime()));
        let inner = db.intern_type(TypeKind::Union(Arc::from([a, b])));
        let outer = db.intern_type(TypeKind::Union(Arc::from([inner, c])));
        let direct = db.intern_type(TypeKind::Union(Arc::from([a, b, c])));
        assert_eq!(outer, direct, "Union([Union([A, B]), C]) must equal Union([A, B, C])");
    }

    #[test]
    fn union_of_only_unknown_collapses_to_unknown() {
        let db = InMemoryDb::new();
        let u = db.intern_type(TypeKind::Union(Arc::from([db.unknown(), db.unknown()])));
        assert_eq!(u, db.unknown());
    }

    #[test]
    fn union_of_only_never_stays_never() {
        let db = InMemoryDb::new();
        let u = db.intern_type(TypeKind::Union(Arc::from([db.never(), db.never()])));
        assert_eq!(u, db.never());
    }

    #[test]
    fn union_of_only_any_stays_any() {
        let db = InMemoryDb::new();
        let u = db.intern_type(TypeKind::Union(Arc::from([db.any(), db.any()])));
        assert_eq!(u, db.any());
    }

    #[test]
    fn union_absorbs_both_unknown_and_never_with_concrete_arm() {
        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::unsized_()));
        let u = db.intern_type(TypeKind::Union(Arc::from([db.unknown(), db.never(), n])));
        assert_eq!(u, n);
    }

    #[test]
    fn union_unknown_plus_never_collapses_to_unknown() {
        let db = InMemoryDb::new();
        let u = db.intern_type(TypeKind::Union(Arc::from([db.unknown(), db.never()])));
        assert_eq!(u, db.unknown());
    }

    #[test]
    fn provenance_stripped_on_query_variant() {
        use crate::kind::{Projection, ProjectionField, ProjectionFieldSource, ProjectionOrigin};

        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));

        let make = |field_src, proj_origin| {
            Some(Arc::new(Projection {
                fields: Arc::from([ProjectionField {
                    name: "Цена".to_string(),
                    ty: n,
                    source: field_src,
                }]),
                origin: proj_origin,
                raw_sdbl_types: None,
            }))
        };

        let a = db.intern_type(TypeKind::Query {
            projections: Arc::from([make(
                ProjectionFieldSource::Cast,
                ProjectionOrigin::SdblQuery,
            )]),
        });
        let b = db.intern_type(TypeKind::Query {
            projections: Arc::from([make(
                ProjectionFieldSource::Column,
                ProjectionOrigin::Unknown,
            )]),
        });
        assert_eq!(a, b);
    }

    #[test]
    fn provenance_stripped_on_query_batch() {
        use crate::kind::{Projection, ProjectionField, ProjectionFieldSource, ProjectionOrigin};

        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));

        let make = |field_src, proj_origin| {
            Some(Arc::new(Projection {
                fields: Arc::from([ProjectionField {
                    name: "Цена".to_string(),
                    ty: n,
                    source: field_src,
                }]),
                origin: proj_origin,
                raw_sdbl_types: None,
            }))
        };

        let a = db.intern_type(TypeKind::QueryBatchResult {
            per_query: Arc::from([make(ProjectionFieldSource::Cast, ProjectionOrigin::SdblQuery)]),
        });
        let b = db.intern_type(TypeKind::QueryBatchResult {
            per_query: Arc::from([make(ProjectionFieldSource::Column, ProjectionOrigin::Unknown)]),
        });
        assert_eq!(a, b);
    }

    #[test]
    fn intern_is_idempotent_across_kind_variety() {
        use crate::facet::{ArrayFacet, DateFacet, MapFacet, StringFacet};

        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));
        let owner =
            MdoRefFacet { mdo_type: MdoType::Catalog, name: "Контрагенты".to_string() };

        let kinds = [
            TypeKind::Unknown,
            TypeKind::Never,
            TypeKind::Any,
            TypeKind::Boolean,
            TypeKind::Null,
            TypeKind::Undefined,
            TypeKind::Number(NumberFacet::with_scale(15, 2)),
            TypeKind::Number(NumberFacet::with_precision(10)),
            TypeKind::Number(NumberFacet::unsized_()),
            TypeKind::String(StringFacet::with_length(50)),
            TypeKind::String(StringFacet::unsized_()),
            TypeKind::Date(DateFacet::datetime()),
            TypeKind::Array(ArrayFacet { element: None }),
            TypeKind::Array(ArrayFacet { element: Some(n) }),
            TypeKind::Map(MapFacet { key: None, value: Some(n), entries: None }),
            TypeKind::FormData { kind: FormDataFacet::Structure, underlying: Some(owner.clone()) },
            TypeKind::FormControl {
                kind: FormElementFacet::Table,
                binding: Some(FormBindingFacet {
                    path: Arc::from(["Объект".to_string(), "Товары".to_string()]),
                    target: FormBindingTargetFacet::TabularSection {
                        mdo_ref: owner.clone(),
                        section: "Товары".to_string(),
                    },
                }),
            },
            TypeKind::ThisObject { config_id: crate::kind::ConfigId::Root, owner: owner.clone() },
            TypeKind::ThisManager { config_id: crate::kind::ConfigId::Root, owner },
            TypeKind::Union(Arc::from([n, db.boolean()])),
        ];

        for kind in &kinds {
            let a = db.intern_type(kind.clone());
            let b = db.intern_type(kind.clone());
            let c = db.intern_type(kind.clone());
            assert_eq!(a, b, "intern(x) != intern(x) for {:?}", kind);
            assert_eq!(b, c, "intern(x) != intern(x) for {:?}", kind);
        }
    }

    #[test]
    fn provenance_stripped_on_query_result() {
        use crate::facet::{ProjectionFacet, ProjectionSource};
        use crate::kind::{Projection, ProjectionField, ProjectionFieldSource, ProjectionOrigin};

        let db = InMemoryDb::new();
        let n = db.intern_type(TypeKind::Number(NumberFacet::with_scale(15, 2)));

        let proj_a = Arc::new(Projection {
            fields: Arc::from([ProjectionField {
                name: "Цена".to_string(),
                ty: n,
                source: ProjectionFieldSource::Cast,
            }]),
            origin: ProjectionOrigin::SdblQuery,
            raw_sdbl_types: None,
        });
        let proj_b = Arc::new(Projection {
            fields: Arc::from([ProjectionField {
                name: "Цена".to_string(),
                ty: n,
                source: ProjectionFieldSource::Column,
            }]),
            origin: ProjectionOrigin::Unknown,
            raw_sdbl_types: None,
        });

        let a = db.intern_type(TypeKind::QueryResult(ProjectionFacet {
            projection: Some(proj_a),
            source: ProjectionSource::Sdbl,
        }));
        let b = db.intern_type(TypeKind::QueryResult(ProjectionFacet {
            projection: Some(proj_b),
            source: ProjectionSource::Unknown,
        }));
        assert_eq!(a, b, "provenance differences must not produce distinct TypeIds");
    }
}
