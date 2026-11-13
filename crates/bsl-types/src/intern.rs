use std::sync::Arc;

use crate::facet::{
    DateFacet, FormBindingFacet, FormBindingTargetFacet, FunctionFacet, MdoRefFacet, NumberFacet,
    PlatformObjectFacet, ProjectionFacet, StringFacet, TableFacet,
};
use crate::kind::{
    Projection, ProjectionField, ProjectionFieldSource, ProjectionOrigin, TypeId, TypeKind,
};

pub trait TypeKernelDb {
    fn intern_type(&self, kind: TypeKind) -> TypeId;

    fn lookup_type(&self, id: TypeId) -> &TypeKind;
}

pub fn canonicalise(db: &dyn TypeKernelDb, kind: TypeKind) -> TypeKind {
    match kind {
        TypeKind::Number(NumberFacet { precision, scale, .. }) => {
            TypeKind::Number(NumberFacet { precision, scale, origin: None })
        }
        TypeKind::String(StringFacet { length, fixed, .. }) => {
            TypeKind::String(StringFacet { length, fixed, origin: None })
        }
        TypeKind::Date(DateFacet { component, .. }) => {
            TypeKind::Date(DateFacet { component, origin: None })
        }

        TypeKind::ValueTable(TableFacet { projection, .. }) => TypeKind::ValueTable(TableFacet {
            projection: strip_projection(projection),
            source: crate::facet::TableSource::Unknown,
        }),
        TypeKind::ValueTableRow(TableFacet { projection, .. }) => {
            TypeKind::ValueTableRow(TableFacet {
                projection: strip_projection(projection),
                source: crate::facet::TableSource::Unknown,
            })
        }
        TypeKind::QueryResult(ProjectionFacet { projection, .. }) => {
            TypeKind::QueryResult(ProjectionFacet {
                projection: strip_projection(projection),
                source: crate::facet::ProjectionSource::Unknown,
            })
        }
        TypeKind::QueryResultSelection(ProjectionFacet { projection, .. }) => {
            TypeKind::QueryResultSelection(ProjectionFacet {
                projection: strip_projection(projection),
                source: crate::facet::ProjectionSource::Unknown,
            })
        }
        TypeKind::Query { projections } => {
            TypeKind::Query { projections: strip_projection_slice(&projections) }
        }
        TypeKind::QueryBatchResult { per_query } => {
            TypeKind::QueryBatchResult { per_query: strip_projection_slice(&per_query) }
        }

        TypeKind::Function(FunctionFacet {
            params, defaults, min_args, max_args, returns, ..
        }) => TypeKind::Function(FunctionFacet {
            params,
            defaults,
            min_args,
            max_args,
            returns,
            origin: crate::facet::FunctionOrigin::Unknown,
        }),

        TypeKind::FormData { kind, underlying } => {
            TypeKind::FormData { kind, underlying: underlying.map(canonicalise_mdo_ref) }
        }
        TypeKind::FormControl { kind, binding } => TypeKind::FormControl {
            kind,
            binding: binding.map(|b| canonicalise_form_binding(db, b)),
        },
        TypeKind::ThisObject { config_id, owner } => {
            TypeKind::ThisObject { config_id, owner: canonicalise_mdo_ref(owner) }
        }
        TypeKind::ThisManager { config_id, owner } => {
            TypeKind::ThisManager { config_id, owner: canonicalise_mdo_ref(owner) }
        }

        TypeKind::Union(members) => canonicalise_union(db, members),

        TypeKind::PlatformObject(PlatformObjectFacet { name, described }) => {
            TypeKind::PlatformObject(PlatformObjectFacet {
                name: canonical_platform_name(name),
                described,
            })
        }

        other => other,
    }
}

/// Имена в BSL складывают регистр, а русское и английское написания одного
/// платформенного типа обозначают один тип. Номинальный тип сравнивается по
/// имени и интернируется по нему же, поэтому написание обязано быть
/// каноническим: иначе `Файл` и `файл` становятся двумя несовместимыми типами и
/// дают ложное несоответствие типов там, где его нет.
///
/// Канон берётся из корпуса платформы. Имя, которого корпус не знает
/// (внекорпусные генерики вроде `ДокументМенеджер`, фантом от неизвестного
/// `Новый X`), остаётся как записано; регистр таких имён складывает
/// `subtype::is_assignable`, а не интернирование.
///
/// Отдельный случай — тёзки: `ЭлементыФормы` в корпусе дважды, как `FormItems`
/// и как `Controls`, с разным API. Русское имя у них общее, английские разные,
/// поэтому свести `Controls` к `ЭлементыФормы` значило бы объявить его тем же
/// типом, что `FormItems`. Неоднозначное имя канонизируется только по регистру —
/// то есть когда на входе оно само, а не другой алиас одного из тёзок.
fn canonical_platform_name(name: String) -> String {
    let platform = bsl_platform::PlatformData::instance();
    let Some(ty) = platform.get_type(&name) else {
        return name;
    };

    // У тёзок общее только русское имя, поэтому сводить их к нему нельзя. Но
    // регистр сложить всё равно надо: канон здесь — то из написаний записи, под
    // которое имя подошло, а разные написания тёзок так и остаются разными.
    let canonical = if platform.is_ambiguous_type_name(&ty.name) {
        [ty.name.as_str(), ty.english_name.as_str()]
            .into_iter()
            .chain(ty.xdto_name.as_deref())
            .find(|alias| !alias.is_empty() && stdx::case::eq_ignore_case(alias, &name))
    } else {
        Some(ty.name.as_str())
    };

    match canonical {
        Some(canonical) if canonical != name => canonical.to_string(),
        _ => name,
    }
}

fn canonicalise_mdo_ref(owner: MdoRefFacet) -> MdoRefFacet {
    owner
}

fn canonicalise_form_binding(
    db: &dyn TypeKernelDb,
    FormBindingFacet { path, target }: FormBindingFacet,
) -> FormBindingFacet {
    FormBindingFacet { path, target: canonicalise_form_binding_target(db, target) }
}

fn canonicalise_form_binding_target(
    db: &dyn TypeKernelDb,
    target: FormBindingTargetFacet,
) -> FormBindingTargetFacet {
    match target {
        FormBindingTargetFacet::TabularSection { mdo_ref, section } => {
            FormBindingTargetFacet::TabularSection {
                mdo_ref: canonicalise_mdo_ref(mdo_ref),
                section,
            }
        }
        FormBindingTargetFacet::Attribute { ty } => {
            let ty = db.intern_type(db.lookup_type(ty).clone());
            FormBindingTargetFacet::Attribute { ty }
        }
    }
}

fn strip_projection(p: Option<Arc<Projection>>) -> Option<Arc<Projection>> {
    p.map(|arc| {
        let fields: Arc<[ProjectionField]> = arc
            .fields
            .iter()
            .map(|f| ProjectionField {
                name: f.name.clone(),
                ty: f.ty,
                source: ProjectionFieldSource::Unknown,
            })
            .collect();
        Arc::new(Projection {
            fields,
            origin: ProjectionOrigin::Unknown,
            raw_sdbl_types: arc.raw_sdbl_types.clone(),
        })
    })
}

fn strip_projection_slice(
    slice: &Arc<[Option<Arc<Projection>>]>,
) -> Arc<[Option<Arc<Projection>>]> {
    slice.iter().map(|p| strip_projection(p.clone())).collect()
}

fn canonicalise_union(db: &dyn TypeKernelDb, members: Arc<[TypeId]>) -> TypeKind {
    let mut flat: Vec<TypeId> = Vec::with_capacity(members.len());
    for &m in members.iter() {
        match db.lookup_type(m) {
            TypeKind::Union(inner) => flat.extend(inner.iter().copied()),
            _ => flat.push(m),
        }
    }

    let has_non_never = flat.iter().any(|&m| !matches!(db.lookup_type(m), TypeKind::Never));
    if has_non_never {
        flat.retain(|&m| !matches!(db.lookup_type(m), TypeKind::Never));
    }

    if flat.iter().any(|&m| matches!(db.lookup_type(m), TypeKind::Any)) {
        return TypeKind::Any;
    }

    let has_non_unknown = flat.iter().any(|&m| !matches!(db.lookup_type(m), TypeKind::Unknown));
    if has_non_unknown {
        flat.retain(|&m| !matches!(db.lookup_type(m), TypeKind::Unknown));
    }

    // Order members by a deterministic structural key, NOT the raw `TypeId`
    // (whose value follows interning order, which is scheduler-dependent under
    // parallel analysis). This keeps a union's canonical member order — and thus
    // its display text and the arm picked by union method/overload resolution —
    // stable across runs. Stable sort preserves construction order on key ties.
    flat.sort_by_cached_key(|&id| union_order_key(db, id));
    flat.dedup();

    if flat.len() == 1 {
        return db.lookup_type(flat[0]).clone();
    }

    if flat.is_empty() {
        return TypeKind::Unknown;
    }

    TypeKind::Union(flat.into())
}

/// Deterministic structural ordering key for union members, independent of
/// `TypeId` assignment order. Variants without nested `TypeId`s key off their
/// `Debug` (only names / enums / config-ids — no ids); the variants that carry
/// nested `TypeId`s recurse so the key reflects structure, not interning order.
fn union_order_key(db: &dyn TypeKernelDb, id: TypeId) -> String {
    let mut out = String::new();
    write_type_order_key(db, id, 0, &mut out);
    out
}

fn write_type_order_key(db: &dyn TypeKernelDb, id: TypeId, depth: u8, out: &mut String) {
    use std::fmt::Write as _;

    if depth >= 24 {
        out.push('~');
        return;
    }
    let d = depth + 1;
    let opt = |db: &dyn TypeKernelDb, t: Option<TypeId>, out: &mut String| match t {
        Some(t) => write_type_order_key(db, t, d, out),
        None => out.push('_'),
    };

    match db.lookup_type(id) {
        TypeKind::Array(f) => {
            out.push_str("Array<");
            opt(db, f.element, out);
            out.push('>');
        }
        TypeKind::Map(f) => {
            out.push_str("Map<");
            opt(db, f.key, out);
            out.push(',');
            opt(db, f.value, out);
            out.push(',');
            write_projection_order_key(db, &f.entries, d, out);
            out.push('>');
        }
        TypeKind::ValueList(el) => {
            out.push_str("ValueList<");
            opt(db, *el, out);
            out.push('>');
        }
        TypeKind::ValueTable(f) => {
            out.push_str("ValueTable");
            write_projection_order_key(db, &f.projection, d, out);
        }
        TypeKind::ValueTableRow(f) => {
            out.push_str("ValueTableRow");
            write_projection_order_key(db, &f.projection, d, out);
        }
        TypeKind::QueryResult(f) => {
            out.push_str("QueryResult");
            write_projection_order_key(db, &f.projection, d, out);
        }
        TypeKind::QueryResultSelection(f) => {
            out.push_str("QueryResultSelection");
            write_projection_order_key(db, &f.projection, d, out);
        }
        TypeKind::QueryBatchResult { per_query } => {
            out.push_str("QueryBatchResult");
            for proj in per_query.iter() {
                write_projection_order_key(db, proj, d, out);
            }
        }
        TypeKind::Query { projections } => {
            out.push_str("Query");
            for proj in projections.iter() {
                write_projection_order_key(db, proj, d, out);
            }
        }
        TypeKind::Function(f) => {
            out.push_str("Function(");
            for p in f.params.iter() {
                let _ = write!(out, "{:?}{:?}:", p.name, (&p.passing, p.variadic));
                write_type_order_key(db, p.ty, d, out);
                out.push(',');
            }
            out.push_str("->");
            write_type_order_key(db, f.returns, d, out);
            let _ = write!(out, ";{:?}{:?}{:?}{:?}", f.min_args, f.max_args, f.origin, f.defaults);
            out.push(')');
        }
        TypeKind::FormControl { kind, binding } => {
            let _ = write!(out, "FormControl{{{kind:?},");
            match binding {
                Some(b) => {
                    let _ = write!(out, "{:?}:", b.path);
                    match &b.target {
                        crate::facet::FormBindingTargetFacet::Attribute { ty } => {
                            out.push_str("Attr(");
                            write_type_order_key(db, *ty, d, out);
                            out.push(')');
                        }
                        other => {
                            let _ = write!(out, "{other:?}");
                        }
                    }
                }
                None => out.push('_'),
            }
            out.push('}');
        }
        TypeKind::Union(members) => {
            // A union nested inside another type (Array<A|B>, a projection field,
            // …) must recurse, not hit the `Debug` fallback (which embeds raw
            // member `TypeId`s). Sort the member keys so the nested-union key is
            // itself member-order-independent.
            out.push_str("Union(");
            let mut keys: Vec<String> = members
                .iter()
                .map(|&m| {
                    let mut s = String::new();
                    write_type_order_key(db, m, d, &mut s);
                    s
                })
                .collect();
            keys.sort();
            for k in &keys {
                out.push_str(k);
                out.push('|');
            }
            out.push(')');
        }
        // Every remaining variant carries only names / enums / config-ids (verified
        // against `kind.rs`), so `Debug` is already interning-order-independent.
        other => {
            let _ = write!(out, "{other:?}");
        }
    }
}

fn write_projection_order_key(
    db: &dyn TypeKernelDb,
    projection: &Option<Arc<crate::kind::Projection>>,
    depth: u8,
    out: &mut String,
) {
    use std::fmt::Write as _;
    out.push('[');
    if let Some(p) = projection {
        for fld in p.fields.iter() {
            let _ = write!(out, "{:?}=", fld.name);
            write_type_order_key(db, fld.ty, depth, out);
            out.push(';');
        }
    }
    out.push(']');
}

#[cfg(test)]
mod tests {
    use crate::builders::Builders;
    use crate::intern::TypeKernelDb;
    use crate::kind::TypeKind;
    use crate::testing::InMemoryDb;

    /// Имена в BSL складывают регистр, а русское и английское написания одного
    /// платформенного типа обозначают один тип. Номинальный тип интернируется по
    /// имени, поэтому разные написания обязаны давать один `TypeId` — иначе
    /// `Файл` и `файл` несовместимы и дают ложное несоответствие типов.
    #[test]
    fn corpus_spellings_of_one_type_intern_to_one_id() {
        let data = bsl_platform::PlatformData::instance();
        let Some(file) = data.get_type("Файл") else {
            return;
        };
        let db = InMemoryDb::new();
        let canonical = db.platform_object(file.name.to_string());
        assert_eq!(db.platform_object("файл".to_string()), canonical, "регистр");
        assert_eq!(db.platform_object("ФАЙЛ".to_string()), canonical, "регистр");
        if !file.english_name.is_empty() {
            assert_eq!(
                db.platform_object(file.english_name.to_string()),
                canonical,
                "английское имя обозначает тот же тип"
            );
        }
    }

    /// Канонизировать нечем — имя остаётся как записано, и различие написаний
    /// по-прежнему держится на `name_eq_ci` в hir-ty.
    #[test]
    fn uncorpused_name_is_kept_as_written() {
        let data = bsl_platform::PlatformData::instance();
        if data.get_type("ДокументМенеджер").is_some() {
            return;
        }
        let db = InMemoryDb::new();
        assert_ne!(
            db.platform_object("ДокументМенеджер".to_string()),
            db.platform_object("документменеджер".to_string())
        );
    }

    /// У тёзок русское имя общее, а английские разные. Свести `Controls` к
    /// `ЭлементыФормы` значило бы объявить его тем же типом, что `FormItems`,
    /// хотя у них разный API, — поэтому чужой алиас не канонизируется.
    #[test]
    fn twin_aliases_stay_distinct_types() {
        let data = bsl_platform::PlatformData::instance();
        if !data.is_ambiguous_type_name("ЭлементыФормы") {
            return;
        }
        let db = InMemoryDb::new();
        assert_ne!(
            db.platform_object("FormItems".to_string()),
            db.platform_object("Controls".to_string()),
            "разные платформенные типы не должны склеиваться в один"
        );
    }

    /// `canonicalise` зовётся на КАЖДОМ интернировании, поэтому канонизация
    /// обязана быть идемпотентной: иначе повторный проход менял бы имя и `TypeId`
    /// поплыл бы в зависимости от порядка обращений. Существующие
    /// `canon_tests::intern_is_idempotent*` номинальных типов не касаются.
    #[test]
    fn canonicalisation_is_idempotent() {
        let db = InMemoryDb::new();
        for raw in [
            "Файл",
            "файл",
            "ФАЙЛ",
            "TextReader",
            "ЧтениеТекста",
            "ЭлементыФормы",
            "элементыформы",
            "Controls",
            "controls",
            "FormItems",
            "ДокументМенеджер",
            "документменеджер",
            "ЗаведомоНесуществующийТип",
            "",
        ] {
            let once = db.platform_object(raw.to_string());
            let TypeKind::PlatformObject(facet) = db.lookup_type(once).clone() else {
                panic!("{raw:?} должно оставаться PlatformObject");
            };
            let twice = db.platform_object(facet.name.clone());
            assert_eq!(once, twice, "повторная канонизация {raw:?} изменила тип");
        }
    }

    /// Различать тёзок нужно, а хранить их алиасы в произвольном регистре — нет:
    /// канон для такого имени — то из написаний записи, под которое оно подошло.
    #[test]
    fn twin_alias_still_folds_case() {
        let data = bsl_platform::PlatformData::instance();
        if !data.is_ambiguous_type_name("ЭлементыФормы") {
            return;
        }
        let db = InMemoryDb::new();
        assert_eq!(
            db.platform_object("Controls".to_string()),
            db.platform_object("controls".to_string()),
            "два регистра одного английского алиаса — один тип"
        );
        assert_ne!(
            db.platform_object("Controls".to_string()),
            db.platform_object("FormItems".to_string()),
            "но разные алиасы — по-прежнему разные типы"
        );
    }

    /// А вот регистр самого неоднозначного имени свести можно и нужно: тёзки
    /// пишут его одинаково, выбирать между ними тут не приходится.
    #[test]
    fn ambiguous_name_still_folds_case() {
        let data = bsl_platform::PlatformData::instance();
        if !data.is_ambiguous_type_name("ЭлементыФормы") {
            return;
        }
        let db = InMemoryDb::new();
        assert_eq!(
            db.platform_object("ЭлементыФормы".to_string()),
            db.platform_object("элементыформы".to_string())
        );
    }

    // ---- keyed maps -----------------------------------------------------------------------

    fn keyed(db: &InMemoryDb, key: &str, ty: crate::kind::TypeId) -> crate::kind::TypeId {
        use crate::kind::{Projection, ProjectionField, ProjectionFieldSource, ProjectionOrigin};
        let field = ProjectionField::new(key.to_string(), ty, ProjectionFieldSource::DocComment);
        db.map_keyed(std::sync::Arc::new(Projection::new(
            vec![field].into(),
            ProjectionOrigin::DocComment,
            None,
        )))
    }

    fn first_key(db: &InMemoryDb, ty: crate::kind::TypeId) -> String {
        let TypeKind::Map(facet) = db.lookup_type(ty).clone() else { panic!("a map expected") };
        facet.entries.expect("entries").fields[0].name.to_string()
    }

    #[test]
    fn maps_with_different_entries_are_different_types_and_equal_entries_intern_once() {
        let db = InMemoryDb::new();
        let number = db.number(None, None);
        let string = db.string(None, false);

        let a = keyed(&db, "сумма", number);
        assert_eq!(a, keyed(&db, "сумма", number), "equal entries intern to one id");
        assert_ne!(a, keyed(&db, "итог", number), "another key");
        assert_ne!(a, keyed(&db, "сумма", string), "another value type");
        assert_ne!(a, db.map(None, None), "a keyed map is not the plain map");
        let TypeKind::Map(facet) = db.lookup_type(a).clone() else { panic!("a map expected") };
        assert!(facet.key.is_none() && facet.value.is_none() && facet.entries.is_some());
    }

    #[test]
    fn the_plain_map_builder_still_builds_a_map_without_entries() {
        let db = InMemoryDb::new();
        let TypeKind::Map(facet) = db.lookup_type(db.map(None, None)).clone() else {
            panic!("a map expected")
        };
        assert!(facet.entries.is_none());
    }

    #[test]
    fn a_union_of_keyed_maps_has_one_order_whatever_order_the_members_are_given_in() {
        // The order key is structural. If the entries were left out of it, two keyed maps would
        // tie and the union would keep the order its members were handed in, so the same two
        // members given in opposite orders would build two different unions.
        let members_in_union = |db: &InMemoryDb, given: [crate::kind::TypeId; 2]| -> Vec<String> {
            let TypeKind::Union(members) = db.lookup_type(db.union(given.to_vec())).clone() else {
                panic!("a union expected");
            };
            members.iter().map(|member| first_key(db, *member)).collect()
        };
        for created_a_first in [true, false] {
            let db = InMemoryDb::new();
            let (a, b) = if created_a_first {
                let a = keyed(&db, "а", db.number(None, None));
                let b = keyed(&db, "б", db.string(None, false));
                (a, b)
            } else {
                let b = keyed(&db, "б", db.string(None, false));
                let a = keyed(&db, "а", db.number(None, None));
                (a, b)
            };
            let forward = members_in_union(&db, [a, b]);
            let backward = members_in_union(&db, [b, a]);
            assert_eq!(forward, backward, "created `а` first: {created_a_first}");
            assert_eq!(forward.len(), 2);
            assert_eq!(db.union(vec![a, b]), db.union(vec![b, a]));
        }
    }
}
