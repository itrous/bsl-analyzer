use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{MetadataKind, TypeId, TypeKind};
use stdx::case::CaseExt;

pub fn is_assignable(db: &dyn TypeKernelDb, from: TypeId, to: TypeId) -> bool {
    if from == to {
        return true;
    }

    let from_kind = db.lookup_type(from);
    let to_kind = db.lookup_type(to);

    if matches!(from_kind, TypeKind::Unknown | TypeKind::Any)
        || matches!(to_kind, TypeKind::Unknown | TypeKind::Any)
    {
        return true;
    }

    if matches!(from_kind, TypeKind::Never) {
        return true;
    }

    // Имена в BSL складывают регистр. Корпусные имена сводит к канону
    // интернирование, но внекорпусному номиналу (`ДокументМенеджер`, фантом от
    // `Новый X`) канона взять неоткуда, и без этой ветки два его написания —
    // два несовместимых типа, то есть ложное несоответствие на ровном месте.
    if let (TypeKind::PlatformObject(from_facet), TypeKind::PlatformObject(to_facet)) =
        (from_kind, to_kind)
    {
        if stdx::case::eq_ignore_case(&from_facet.name, &to_facet.name) {
            return true;
        }
    }

    // Cold-kind metadata references (`Метаданные.Роли.X`, `.РегламентныеЗадания.X`, `.HTTPСервисы.X`,
    // …) are a soft, navigation-only inference. Before this type existed the expression was
    // `Unknown`, which assigns freely; keep them Unknown-like for assignability so a cold-kind
    // reference passed where a metadata object is expected (e.g.
    // `РегламентныеЗадания.НайтиПредопределенное(Метаданные.РегламентныеЗадания.X)`) never
    // manufactures a TypeMismatch. They ARE metadata objects; a precise per-kind lattice for the
    // non-manager kinds is deliberately out of scope for this soft surface.
    // A kind-aware metadata collection (`Метаданные.Справочники`) gets the same latitude: it
    // was a plain platform object before it learned its element kind, and learning it must not
    // turn into a mismatch wherever a `КоллекцияОбъектовМетаданных` is passed around.
    if matches!(
        from_kind,
        TypeKind::MetadataReference { .. }
            | TypeKind::MetadataReferenceCollection(_)
            | TypeKind::MetadataObjectCollection(_)
    ) || matches!(
        to_kind,
        TypeKind::MetadataReference { .. }
            | TypeKind::MetadataReferenceCollection(_)
            | TypeKind::MetadataObjectCollection(_)
    ) {
        return true;
    }

    if let TypeKind::Union(parts) = from_kind {
        return parts.iter().all(|p| is_assignable(db, *p, to));
    }
    if let TypeKind::Union(parts) = to_kind {
        return parts.iter().any(|p| is_assignable(db, from, *p));
    }

    // Any structure is assignable to any structure annotation regardless of inferred keys: BSL
    // structures are dynamic dictionaries, so a key set is a soft completion aid, never a subtyping
    // constraint. (Without this, an inferred `Структура(k1, k2)` would not match a plain `Структура`
    // parameter — they are distinct interned types — manufacturing TypeMismatch false positives.)
    if matches!(from_kind, TypeKind::Structure(_)) && matches!(to_kind, TypeKind::Structure(_)) {
        return true;
    }

    if matches!(from_kind, TypeKind::Null) && is_ref_kind(to_kind) {
        return true;
    }

    if matches!(to_kind, TypeKind::AnyRef) && is_ref_kind(from_kind) {
        return true;
    }
    if let TypeKind::AnyMetadataRef { mdo_type } = to_kind {
        if let TypeKind::MetadataRef(facet) = from_kind {
            if facet.kind.ref_mdo_type() == Some(*mdo_type) {
                return true;
            }
        }
    }

    if is_tabular_row_bridge(from_kind, to_kind) {
        return true;
    }

    if is_spreadsheet_area_bridge(from_kind, to_kind) {
        return true;
    }

    if is_value_table_bridge(from_kind, to_kind) {
        return true;
    }

    if is_concrete_to_generic_platform_bridge(from_kind, to_kind) {
        return true;
    }

    if is_platform_object_supertype_bridge(from_kind, to_kind) {
        return true;
    }

    // A specific common module is a `ОбщийМодуль` — the platform supertype of every common
    // module, which carries no fixed interface of its own. Passing a concrete module where a
    // `ОбщийМодуль` parameter is annotated (the SSL delegation idiom: a module handed to a
    // client/server helper as the call source) is sound Liskov substitution.
    if matches!(from_kind, TypeKind::CommonModule(_)) {
        if let TypeKind::PlatformObject(facet) = to_kind {
            if stdx::case::eq_ignore_case(&facet.name, "ОбщийМодуль")
                || stdx::case::eq_ignore_case(&facet.name, "CommonModule")
            {
                return true;
            }
        }
    }

    if is_array_bridge(db, from_kind, to_kind) {
        return true;
    }

    if matches!(from_kind, TypeKind::ThisObject { .. } | TypeKind::ThisManager { .. }) {
        if let Some(coerced) = crate::this_object::coerce_to_metadata_ref_id(db, from) {
            if is_assignable(db, coerced, to) {
                return true;
            }
        }
    }

    if let (TypeKind::Function(from_fn), TypeKind::Function(to_fn)) = (from_kind, to_kind) {
        if from_fn.params.len() != to_fn.params.len() {
            return false;
        }
        let params_ok = from_fn
            .params
            .iter()
            .zip(to_fn.params.iter())
            .all(|(from_p, to_p)| is_assignable(db, to_p.ty, from_p.ty));
        return params_ok && is_assignable(db, from_fn.returns, to_fn.returns);
    }

    false
}

fn is_ref_kind(kind: &TypeKind) -> bool {
    match kind {
        TypeKind::MetadataRef(facet) => facet.kind.ref_mdo_type().is_some(),
        TypeKind::AnyMetadataRef { .. } | TypeKind::AnyRef => true,
        _ => false,
    }
}

pub fn is_ref_ty(db: &dyn TypeKernelDb, ty: TypeId) -> bool {
    is_ref_kind(db.lookup_type(ty))
}

fn is_tabular_row_bridge(a: &TypeKind, b: &TypeKind) -> bool {
    fn is_row_metadata_ref(ty: &TypeKind) -> bool {
        matches!(
            ty,
            TypeKind::MetadataRef(facet)
                if matches!(facet.kind, MetadataKind::TabularSectionRow { .. })
        )
    }
    fn is_row_platform_object(ty: &TypeKind) -> bool {
        matches!(ty, TypeKind::PlatformObject(facet)
            if crate::platform_type_name::is_tabular_row_name(&facet.name))
    }
    (is_row_metadata_ref(a) && is_row_platform_object(b))
        || (is_row_platform_object(a) && is_row_metadata_ref(b))
}

/// Both sides are `ТаблицаЗначений`. The projection — best-effort column tracking
/// recovered from query/form inference — is metadata, not a distinct type: a value
/// with a concrete projection IS a `ТаблицаЗначений`, so it satisfies a parameter
/// documented as the bare type (projection `None`), which is how `Выгрузить()`
/// feeding a `Неопределено | ТаблицаЗначений` slot must be admitted. When BOTH
/// sides carry a projection, require equality so genuinely different inferred
/// shapes still surface. Soundness is symmetric (one BSL type), so this lives in
/// `is_assignable`, not just argument-position coercion.
fn is_value_table_bridge(from: &TypeKind, to: &TypeKind) -> bool {
    let (TypeKind::ValueTable(a), TypeKind::ValueTable(b)) = (from, to) else {
        return false;
    };
    match (&a.projection, &b.projection) {
        (None, _) | (_, None) => true,
        (Some(pa), Some(pb)) => pa == pb,
    }
}

/// `ТабличныйДокумент.ПолучитьОбласть(…)` returns `ТабличныйДокумент` (so the
/// platform documents it), while callees document the receiving parameter as
/// `ОбластьЯчеекТабличногоДокумента` — the canonical print pattern passes one
/// into the other. One direction only: an area value flows into an
/// area-documented slot; nothing area-typed is admitted where a full
/// spreadsheet document is required.
fn is_spreadsheet_area_bridge(from: &TypeKind, to: &TypeKind) -> bool {
    fn is_spreadsheet(t: &TypeKind) -> bool {
        matches!(t, TypeKind::PlatformObject(facet)
            if platform_name_eq_ci(&facet.name, "ТабличныйДокумент", "SpreadsheetDocument"))
    }
    fn is_area(t: &TypeKind) -> bool {
        matches!(t, TypeKind::PlatformObject(facet)
        if platform_name_eq_ci(
            &facet.name,
            "ОбластьЯчеекТабличногоДокумента",
            "SpreadsheetDocumentRange",
        ))
    }
    is_spreadsheet(from) && is_area(to)
}

/// BSL identifiers are case-insensitive and doc authors spell platform names
/// in arbitrary case; `eq_ignore_ascii_case` folds only ASCII, so Cyrillic
/// names need the full Unicode lowercase fold.
fn platform_name_eq_ci(actual: &str, ru: &str, en: &str) -> bool {
    actual.eq_ignore_ascii_case(en) || actual.fold_lower() == ru.fold_lower()
}

/// Vendor docs widely annotate parameters with a GENERIC platform name —
/// ДокументМенеджер, СправочникОбъект, ТабличнаяЧасть, ДанныеФормыСтруктура —
/// while the call site passes the concretely typed value, and by platform
/// semantics the concrete value IS the generic (`Документы.X` is a
/// `ДокументМенеджер`). One direction only: a generic value is never admitted
/// where a concrete one is required, and concrete-vs-other-concrete keeps
/// firing.
fn is_concrete_to_generic_platform_bridge(from: &TypeKind, to: &TypeKind) -> bool {
    let TypeKind::PlatformObject(to_facet) = to else {
        return false;
    };
    let matches = |(ru, en): (&str, &str)| platform_name_eq_ci(&to_facet.name, ru, en);
    match from {
        TypeKind::ObjectManager(f) => generic_manager_names(f.mdo).is_some_and(matches),
        TypeKind::ThisManager { owner, .. } => {
            generic_manager_names(owner.mdo_type).is_some_and(matches)
        }
        TypeKind::ThisObject { owner, .. } => {
            generic_object_names(owner.mdo_type).is_some_and(matches)
        }
        TypeKind::FormData { kind, .. } => {
            let en = match kind {
                bsl_types::facet::FormDataFacet::Structure => "FormDataStructure",
                bsl_types::facet::FormDataFacet::Collection => "FormDataCollection",
                bsl_types::facet::FormDataFacet::StructureWithCollection => {
                    "FormDataStructureAndCollection"
                }
                bsl_types::facet::FormDataFacet::Tree => "FormDataTree",
            };
            matches((kind.platform_type_name(), en))
        }
        TypeKind::MetadataRef(f) => match f.kind {
            MetadataKind::TabularSection { .. } => matches(("ТабличнаяЧасть", "TabularSection")),
            kind => object_mdo_of_kind(kind).and_then(generic_object_names).is_some_and(matches),
        },
        TypeKind::FormControl { kind, .. } => {
            generic_form_control_names(*kind).is_some_and(matches)
        }
        _ => false,
    }
}

/// A form control (`Элементы.<имя>`) IS its generic platform type —
/// `Элементы.Список` is a `ТаблицаФормы`, an input field is a `ПолеФормы` —
/// while БСП command/handler helpers document the parameter with that generic
/// name. One direction only, like the manager/object generics above.
fn generic_form_control_names(
    kind: bsl_types::facet::FormElementFacet,
) -> Option<(&'static str, &'static str)> {
    crate::platform_type_name::form_control_name_for(kind)
}

/// `УправляемаяФорма`/`ManagedForm` is the version-older NAME of the managed
/// form whose current name is `ФормаКлиентскогоПриложения`/`ClientApplicationForm`
/// — the same runtime type (the platform dump carries the whole surface on the
/// new name and leaves the old name an empty alias). БСП helpers still annotate
/// parameters with the deprecated name while the call site passes the form
/// module's `ЭтотОбъект`. Both sides are plain `PlatformObject` names, so
/// `is_concrete_to_generic_platform_bridge` (which keys on richer `from` kinds)
/// cannot express this. One direction only.
///
/// Deliberately NOT here, despite looking similar:
/// - `ФормаКлиентскогоПриложения` → `Форма`. `Форма` is the legacy ORDINARY-form
///   type, a different paradigm with members the managed form lacks (`Обновить`,
///   `ПодключитьОбработчикИзмененияДанных`, `Стиль`, `ЭлементыФормы`, …); a
///   parameter genuinely typed `Форма` is a stale/loose annotation, not a
///   supertype the managed form satisfies.
/// - `ВсеЭлементыФормы` → `ЭлементыФормы`. Not a subtype relation — `ЭлементыФормы`
///   has `Получить`/`Индекс` that `ВсеЭлементыФормы` lacks (it carries the
///   mutators `Добавить`/`Удалить`/… instead).
fn is_platform_object_supertype_bridge(from: &TypeKind, to: &TypeKind) -> bool {
    let (TypeKind::PlatformObject(from_facet), TypeKind::PlatformObject(to_facet)) = (from, to)
    else {
        return false;
    };
    // The platform dump qualifies a concrete variant of a family with a colon —
    // `ОбъектМетаданных: HTTPСервис`, `ОбъектМетаданных: Документ`, … (e.g. every member of
    // the `Метаданные.НайтиПоТипу` return union). Such a phantom IS the generic family it
    // qualifies, so it is assignable to the bare `ОбъектМетаданных` a doc comment annotates.
    // One direction only — the bare generic carries no colon and never matches as `from`.
    if let Some((prefix, _specific)) = from_facet.name.split_once(':') {
        if stdx::case::eq_ignore_case(&to_facet.name, prefix.trim()) {
            return true;
        }
    }
    const SUPERTYPES: &[(&str, &str, &str, &str)] = &[(
        "ФормаКлиентскогоПриложения",
        "ClientApplicationForm",
        "УправляемаяФорма",
        "ManagedForm",
    )];
    SUPERTYPES.iter().any(|(sub_ru, sub_en, sup_ru, sup_en)| {
        platform_name_eq_ci(&from_facet.name, sub_ru, sub_en)
            && platform_name_eq_ci(&to_facet.name, sup_ru, sup_en)
    })
}

fn object_mdo_of_kind(kind: MetadataKind) -> Option<bsl_metadata::MdoType> {
    use bsl_metadata::MdoType as M;
    match kind {
        MetadataKind::CatalogObject => Some(M::Catalog),
        MetadataKind::DocumentObject => Some(M::Document),
        MetadataKind::TaskObject => Some(M::Task),
        MetadataKind::BusinessProcessObject => Some(M::BusinessProcess),
        MetadataKind::ExchangePlanObject => Some(M::ExchangePlan),
        MetadataKind::ChartOfAccountsObject => Some(M::ChartOfAccounts),
        MetadataKind::DataProcessorObject => Some(M::DataProcessor),
        MetadataKind::ReportObject => Some(M::Report),
        MetadataKind::ExternalDataProcessorObject => Some(M::ExternalDataProcessor),
        MetadataKind::ExternalReportObject => Some(M::ExternalReport),
        MetadataKind::ChartOfCharacteristicTypesObject => Some(M::ChartOfCharacteristicTypes),
        MetadataKind::ChartOfCalculationTypesObject => Some(M::ChartOfCalculationTypes),
        _ => None,
    }
}

fn generic_manager_names(mdo: bsl_metadata::MdoType) -> Option<(&'static str, &'static str)> {
    crate::platform_type_name::manager_name_for(mdo)
}

fn generic_object_names(mdo: bsl_metadata::MdoType) -> Option<(&'static str, &'static str)> {
    crate::platform_type_name::object_name_for(mdo)
}

fn is_array_bridge(db: &dyn TypeKernelDb, from: &TypeKind, to: &TypeKind) -> bool {
    match (from, to) {
        (TypeKind::Array(a), TypeKind::Array(b)) => match (a.element, b.element) {
            (Some(_), None) | (None, Some(_)) => true,
            (Some(ae), Some(be)) => is_assignable(db, ae, be),
            (None, None) => false,
        },
        _ => false,
    }
}

pub fn is_coercible_to(db: &dyn TypeKernelDb, from: TypeId, to: TypeId) -> bool {
    if matches!(db.lookup_type(to), TypeKind::String(_)) {
        return true;
    }
    // СообщитьПользователю and friends document the parameter as ЛюбаяСсылка
    // yet are canonically called with ЭтотОбъект — БСП resolves the object to
    // its ref internally. Argument-position policy only: in the lattice an
    // object stays distinct from a ref.
    if matches!(db.lookup_type(from), TypeKind::ThisObject { .. })
        && matches!(db.lookup_type(to), TypeKind::AnyRef)
    {
        return true;
    }
    // A document/catalog tabular section shares the whole row API with
    // ТаблицаЗначений (НайтиСтроки, Добавить, Итог, Свернуть, indexing), and
    // vendor helpers that fill or scan a named column routinely document the
    // slot as the bare ТаблицаЗначений (alone or as a union member) yet are
    // called with `Объект.<ТЧ>`. This is argument-position policy, NOT subtype
    // truth: a tabular section is NOT a ValueTable in the lattice (it lacks the
    // column surface — Колонки, Индексы, Скопировать), so is_assignable keeps
    // them distinct and function variance stays sound; only at the call
    // boundary do we admit the idiom. One direction only — a real ТаблицаЗначений
    // is never accepted where a concrete tabular section is required.
    if matches!(db.lookup_type(from), TypeKind::MetadataRef(facet)
        if matches!(facet.kind, MetadataKind::TabularSection { .. }))
        && to_admits_bare_value_table(db, to)
    {
        return true;
    }
    // An argument of a union type is an over-approximation: at runtime exactly
    // one member flows in. Yet a blanket "any member fits" rule would mask
    // real bugs (a dynamic РезультатЗапроса.Выгрузить yields ТаблицаЗначений |
    // ДеревоЗначений, and loading the tree arm into a ТабличнаяЧасть is a
    // genuine error), so acceptance is narrow:
    // - «flag» members (Неопределено/Null) must EACH fit on their own —
    //   passing a maybe-absent value into a slot that does not admit absence
    //   is exactly the bug this check exists to catch;
    // - of the regular members one must fit, and every non-fitting regular
    //   member must be ДвоичныеДанные: that is the template-payload ambiguity
    //   (ПолучитьМакет returns ДвоичныеДанные | ТабличныйДокумент and the
    //   author knows the template's kind), where flagging the canonical print
    //   flow is noise. Any other non-fitting alternative keeps the call
    //   flagged.
    // - a union made only of form-item kinds is accepted when one kind fits:
    //   it is what `Элементы.Добавить`/`Вставить` return when the `Тип`
    //   argument is not a literal (a variable, a parameter, `ТипЗнч(Элемент)`)
    //   and what a loop over `Элементы` yields, and the author chose the kind
    //   through that argument or a check the analyzer does not narrow on, just
    //   as with the template above.
    //   A literal `Тип("…")` narrows the result to one kind during inference,
    //   so a field passed where only a group fits is still flagged.
    // This is argument-position policy, NOT subtype truth: is_assignable
    // keeps the sound all-members rule that function variance relies on.
    if let TypeKind::Union(parts) = db.lookup_type(from) {
        let mut has_regular = false;
        let mut some_regular_fits = false;
        let mut nonfitting_regular_all_binary = true;
        let mut regular_all_form_items = true;
        for part in parts.iter() {
            let part_kind = db.lookup_type(*part);
            if matches!(part_kind, TypeKind::Undefined | TypeKind::Null) {
                if !is_assignable(db, *part, to) {
                    return false;
                }
            } else {
                has_regular = true;
                regular_all_form_items &= is_form_item_kind(part_kind);
                if is_coercible_to(db, *part, to) {
                    some_regular_fits = true;
                } else if !is_binary_data(part_kind) {
                    nonfitting_regular_all_binary = false;
                }
            }
        }
        return !has_regular
            || (some_regular_fits && (nonfitting_regular_all_binary || regular_all_form_items));
    }
    is_assignable(db, from, to)
}

/// A parameter slot accepts the bare ТаблицаЗначений — directly or as one
/// member of a documented union (`ТаблицаЗначений, РезультатЗапроса, …`).
/// Projected value tables never reach parameter position (doc types lower with
/// projection `None`); a carried projection therefore signals an inferred,
/// column-shaped target the tabular section is not proven to satisfy, so it is
/// excluded.
fn to_admits_bare_value_table(db: &dyn TypeKernelDb, to: TypeId) -> bool {
    match db.lookup_type(to) {
        TypeKind::ValueTable(facet) => facet.projection.is_none(),
        TypeKind::Union(parts) => parts.iter().any(|p| to_admits_bare_value_table(db, *p)),
        _ => false,
    }
}

fn is_binary_data(kind: &TypeKind) -> bool {
    matches!(kind, TypeKind::PlatformObject(facet)
        if platform_name_eq_ci(&facet.name, "ДвоичныеДанные", "BinaryData"))
}

fn is_form_item_kind(kind: &TypeKind) -> bool {
    match kind {
        TypeKind::PlatformObject(facet) => {
            crate::platform_type_name::is_form_control_type_name(&facet.name)
        }
        TypeKind::FormControl { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::builders::Builders;
    use bsl_types::facet::{ArgArity, FunctionFacet, FunctionOrigin, ParamPassing, ParamSpec};
    use bsl_types::testing::InMemoryDb;
    use std::sync::Arc;

    fn fn_ty(db: &dyn TypeKernelDb, params: Vec<TypeId>, ret: TypeId) -> TypeId {
        let params: Arc<[ParamSpec]> = params
            .into_iter()
            .enumerate()
            .map(|(idx, ty)| {
                ParamSpec::new(format!("p{}", idx + 1), ty, ParamPassing::ByRef, false)
            })
            .collect();
        db.function(FunctionFacet::new(
            params.clone(),
            vec![None; params.len()].into(),
            params.len() as u16,
            ArgArity::Fixed(params.len() as u16),
            ret,
            FunctionOrigin::Unknown,
        ))
    }

    fn metadata_ref_id(db: &dyn TypeKernelDb, kind: MetadataKind, name: &str) -> TypeId {
        db.metadata_ref(kind, name.to_string(), &bsl_types::testing::RootConfigCtx)
    }

    #[test]
    fn cold_kind_metadata_reference_is_soft_assignable() {
        use bsl_types::kind::MetadataReferenceKind;
        let db = InMemoryDb::default();

        let job = db.metadata_reference(MetadataReferenceKind::ScheduledJob, "X".into());
        let roles = db.metadata_reference_collection(MetadataReferenceKind::Role);
        let any_meta = db.any_metadata_ref(bsl_metadata::MdoType::Catalog);
        let cat_ref = metadata_ref_id(&db, MetadataKind::CatalogRef, "Контрагенты");
        let string = db.string(None, false);

        // A cold-kind reference passed where a metadata object is expected must not manufacture a
        // TypeMismatch (the real ERP shape: a scheduled-job/service reference into a platform call).
        assert!(is_assignable(&db, job, any_meta), "cold ref ≤ AnyMetadataRef");
        assert!(is_assignable(&db, job, cat_ref), "cold ref ≤ MetadataRef family");
        assert!(is_assignable(&db, roles, any_meta), "cold collection ≤ AnyMetadataRef");
        // Unknown-like on both sides: it never drives a mismatch, matching pre-Wave-1 behavior.
        assert!(is_assignable(&db, job, string), "cold ref stays soft (Unknown-like)");
        assert!(is_assignable(&db, cat_ref, job), "soft on the to-side too");
    }

    #[test]
    fn function_reflexive() {
        let db = InMemoryDb::new();
        let f = fn_ty(&db, vec![db.number(None, None), db.string(None, false)], db.boolean());
        assert!(is_assignable(&db, f, f));
    }

    #[test]
    fn common_module_coerces_to_platform_common_module() {
        use bsl_types::kind::ConfigId;
        let db = InMemoryDb::new();
        let specific = db.common_module("ИнтеграцияЕГАИСКлиент".to_string(), ConfigId::Root);
        let generic = db.platform_object("ОбщийМодуль".to_string());
        // a specific common module IS a `ОбщийМодуль` (Liskov), in both subtype and arg position
        assert!(is_assignable(&db, specific, generic));
        assert!(is_coercible_to(&db, specific, generic));
        // not the reverse — a generic `ОбщийМодуль` is not a specific module
        assert!(!is_assignable(&db, generic, specific));
    }

    #[test]
    fn function_arity_mismatch_is_rejected() {
        let db = InMemoryDb::new();
        let one = fn_ty(&db, vec![db.number(None, None)], db.boolean());
        let two = fn_ty(&db, vec![db.number(None, None), db.string(None, false)], db.boolean());
        assert!(!is_assignable(&db, one, two));
        assert!(!is_assignable(&db, two, one));
    }

    #[test]
    fn function_covariant_return_widens() {
        let db = InMemoryDb::new();
        let narrow = fn_ty(&db, vec![], db.number(None, None));
        let wide =
            fn_ty(&db, vec![], db.union(vec![db.number(None, None), db.string(None, false)]));
        assert!(
            is_assignable(&db, narrow, wide),
            "Number return ≤ Union return (covariant widening)"
        );
        assert!(
            !is_assignable(&db, wide, narrow),
            "Union return ≤ Number return must fail — String leg cannot satisfy Number callers"
        );
    }

    #[test]
    fn function_contravariant_param_widens() {
        let db = InMemoryDb::new();
        let wide_param = fn_ty(
            &db,
            vec![db.union(vec![db.number(None, None), db.string(None, false)])],
            db.boolean(),
        );
        let narrow_param = fn_ty(&db, vec![db.number(None, None)], db.boolean());
        assert!(
            is_assignable(&db, wide_param, narrow_param),
            "Fn(Union) ≤ Fn(Number) (contravariant — wider accepting subtype)"
        );
        assert!(
            !is_assignable(&db, narrow_param, wide_param),
            "Fn(Number) ≤ Fn(Union) must fail — String callers would slip through"
        );
    }

    #[test]
    fn function_mixed_variance() {
        let db = InMemoryDb::new();
        let from = fn_ty(
            &db,
            vec![db.union(vec![db.number(None, None), db.string(None, false)])],
            db.number(None, None),
        );
        let to = fn_ty(
            &db,
            vec![db.number(None, None)],
            db.union(vec![db.number(None, None), db.string(None, false)]),
        );
        assert!(is_assignable(&db, from, to));
        assert!(!is_assignable(&db, to, from));
    }

    #[test]
    fn tabular_row_metadata_ref_assignable_to_platform_object() {
        let db = InMemoryDb::new();
        let row = metadata_ref_id(
            &db,
            MetadataKind::TabularSectionRow { parent: bsl_metadata::MdoType::Catalog },
            "X.Y",
        );
        // Both the spaced platform spelling and the compact doc-comment spelling
        // (RU + EN) must bridge, so a parameter documented with either matches a
        // real tabular-section row value.
        for name in [
            "Строка табличной части",
            "Line of a tabular section",
            "СтрокаТабличнойЧасти",
            "TabularSectionRow",
        ] {
            assert!(
                is_assignable(&db, row, db.platform_object(name.to_string())),
                "row must be assignable to {name:?}"
            );
        }
    }

    #[test]
    fn tabular_row_platform_object_assignable_to_metadata_ref() {
        let db = InMemoryDb::new();
        let row = metadata_ref_id(
            &db,
            MetadataKind::TabularSectionRow { parent: bsl_metadata::MdoType::Document },
            "X.Y",
        );
        let generic = db.platform_object("Строка табличной части".to_string());
        assert!(is_assignable(&db, generic, row));
    }

    #[test]
    fn tabular_row_bridge_does_not_open_unrelated_platform_objects() {
        let db = InMemoryDb::new();
        let row = metadata_ref_id(
            &db,
            MetadataKind::TabularSectionRow { parent: bsl_metadata::MdoType::Catalog },
            "X.Y",
        );
        let unrelated = db.platform_object("ТаблицаЗначений".to_string());
        assert!(!is_assignable(&db, row, unrelated));
        assert!(!is_assignable(&db, unrelated, row));
    }

    #[test]
    fn coercible_anything_to_string() {
        let db = InMemoryDb::new();
        assert!(is_coercible_to(&db, db.number(None, None), db.string(None, false)));
        assert!(is_coercible_to(
            &db,
            db.date(bsl_types::facet::DateComponent::DateTime),
            db.string(None, false)
        ));
        assert!(is_coercible_to(&db, db.boolean(), db.string(None, false)));
        assert!(is_coercible_to(&db, db.null(), db.string(None, false)));
        assert!(is_coercible_to(&db, db.undefined(), db.string(None, false)));
        assert!(is_coercible_to(
            &db,
            db.union(vec![
                db.number(None, None),
                db.date(bsl_types::facet::DateComponent::DateTime)
            ]),
            db.string(None, false)
        ));
    }

    #[test]
    fn coercion_does_not_open_reverse_direction() {
        let db = InMemoryDb::new();
        assert!(!is_coercible_to(&db, db.string(None, false), db.number(None, None)));
        assert!(!is_coercible_to(
            &db,
            db.string(None, false),
            db.date(bsl_types::facet::DateComponent::DateTime)
        ));
        assert!(!is_coercible_to(&db, db.string(None, false), db.boolean()));
    }

    #[test]
    fn coercion_does_not_leak_into_is_assignable() {
        let db = InMemoryDb::new();
        assert!(!is_assignable(&db, db.number(None, None), db.string(None, false)));
        assert!(!is_assignable(
            &db,
            db.date(bsl_types::facet::DateComponent::DateTime),
            db.string(None, false)
        ));
        assert!(!is_assignable(
            &db,
            db.union(vec![
                db.number(None, None),
                db.date(bsl_types::facet::DateComponent::DateTime)
            ]),
            db.string(None, false)
        ));
    }

    #[test]
    fn typed_array_assignable_to_unparameterised_array() {
        let db = InMemoryDb::new();
        let typed = db.array(Some(db.string(None, false)));
        assert!(is_assignable(&db, typed, db.array(None)));
    }

    #[test]
    fn unparameterised_array_assignable_to_typed_array_gradual() {
        let db = InMemoryDb::new();
        assert!(is_assignable(&db, db.array(None), db.array(Some(db.number(None, None)))));
    }

    #[test]
    fn typed_array_covariant_on_element() {
        let db = InMemoryDb::new();
        let narrow = db.array(Some(db.number(None, None)));
        let wide = db.array(Some(db.union(vec![db.number(None, None), db.string(None, false)])));
        assert!(is_assignable(&db, narrow, wide), "TypedArray covariant: Number ≤ Number|String");
        assert!(
            !is_assignable(&db, wide, narrow),
            "TypedArray covariant: Number|String ≰ Number — String leg cannot satisfy Number callers"
        );
    }

    #[test]
    fn typed_array_unrelated_elements_rejected() {
        let db = InMemoryDb::new();
        let str_arr = db.array(Some(db.string(None, false)));
        let num_arr = db.array(Some(db.number(None, None)));
        assert!(!is_assignable(&db, str_arr, num_arr));
        assert!(!is_assignable(&db, num_arr, str_arr));
    }

    #[test]
    fn typed_array_reflexivity_holds() {
        let db = InMemoryDb::new();
        let ta = db.array(Some(db.string(None, false)));
        assert!(is_assignable(&db, ta, ta));
    }

    #[test]
    fn typed_array_bridge_does_not_open_unrelated_ty_pairs() {
        let db = InMemoryDb::new();
        assert!(!is_assignable(&db, db.string(None, false), db.array(None)));
        assert!(!is_assignable(&db, db.array(None), db.string(None, false)));
        assert!(!is_assignable(&db, db.number(None, None), db.array(Some(db.number(None, None)))));
    }

    #[test]
    fn function_unknown_short_circuit_wins_over_variance() {
        let db = InMemoryDb::new();
        let f = fn_ty(&db, vec![db.number(None, None)], db.boolean());
        assert!(is_assignable(&db, db.unknown(), f));
        assert!(is_assignable(&db, f, db.unknown()));
    }

    #[test]
    fn null_assignable_to_ref_types() {
        let db = InMemoryDb::new();
        let cat_ref = metadata_ref_id(&db, MetadataKind::CatalogRef, "Контрагенты");
        assert!(is_assignable(&db, db.null(), cat_ref));
        assert!(!is_assignable(&db, db.null(), db.number(None, None)));
    }

    #[test]
    fn any_is_universal_both_directions() {
        let db = InMemoryDb::new();
        let any = db.any();
        let number = db.number(None, None);
        assert!(is_assignable(&db, number, any), "A ≤ Any (universal top)");
        assert!(is_assignable(&db, any, number), "Any ≤ A (universal, gradual)");
    }

    #[test]
    fn concrete_ref_assignable_to_any_ref() {
        let db = InMemoryDb::new();
        let cat = metadata_ref_id(&db, MetadataKind::CatalogRef, "Контрагенты");
        let doc = metadata_ref_id(&db, MetadataKind::DocumentRef, "ПКО");
        assert!(is_assignable(&db, cat, db.any_ref()));
        assert!(is_assignable(&db, doc, db.any_ref()));
    }

    #[test]
    fn any_metadata_ref_assignable_to_any_ref() {
        let db = InMemoryDb::new();
        assert!(is_assignable(
            &db,
            db.any_metadata_ref(bsl_metadata::MdoType::Catalog),
            db.any_ref()
        ));
    }

    #[test]
    fn any_ref_not_assignable_to_concrete_ref() {
        let db = InMemoryDb::new();
        let cat = metadata_ref_id(&db, MetadataKind::CatalogRef, "Контрагенты");
        assert!(!is_assignable(&db, db.any_ref(), cat));
        assert!(!is_assignable(
            &db,
            db.any_ref(),
            db.any_metadata_ref(bsl_metadata::MdoType::Catalog)
        ));
    }

    #[test]
    fn concrete_ref_assignable_to_matching_flavour() {
        let db = InMemoryDb::new();
        let cat = metadata_ref_id(&db, MetadataKind::CatalogRef, "Контрагенты");
        let doc = metadata_ref_id(&db, MetadataKind::DocumentRef, "ПКО");
        let any_catalog = db.any_metadata_ref(bsl_metadata::MdoType::Catalog);
        assert!(is_assignable(&db, cat, any_catalog), "CatalogRef ≤ AnyMetadataRef{{Catalog}}");
        assert!(
            !is_assignable(&db, doc, any_catalog),
            "DocumentRef ≰ AnyMetadataRef{{Catalog}} — wrong flavour"
        );
    }

    #[test]
    fn any_metadata_ref_not_assignable_to_concrete_ref() {
        let db = InMemoryDb::new();
        let cat = metadata_ref_id(&db, MetadataKind::CatalogRef, "Контрагенты");
        let any_catalog = db.any_metadata_ref(bsl_metadata::MdoType::Catalog);
        assert!(!is_assignable(&db, any_catalog, cat));
    }

    #[test]
    fn null_assignable_to_any_ref() {
        let db = InMemoryDb::new();
        assert!(is_assignable(&db, db.null(), db.any_ref()));
        assert!(is_assignable(&db, db.null(), db.any_metadata_ref(bsl_metadata::MdoType::Catalog)));
    }

    #[test]
    fn number_not_assignable_to_any_ref() {
        let db = InMemoryDb::new();
        assert!(!is_assignable(&db, db.number(None, None), db.any_ref()));
        assert!(!is_assignable(&db, db.string(None, false), db.any_ref()));
    }

    #[test]
    fn union_arg_coercible_when_a_regular_member_fits() {
        let db = InMemoryDb::new();
        let spreadsheet = db.platform_object("ТабличныйДокумент".to_string());
        let binary = db.platform_object("ДвоичныеДанные".to_string());
        let from = db.union(vec![binary, spreadsheet]);
        assert!(
            is_coercible_to(&db, from, spreadsheet),
            "a union argument with a fitting regular member is an over-approximation, not a bug"
        );
        assert!(
            !is_assignable(&db, from, spreadsheet),
            "the lattice keeps the sound all-members rule for unions"
        );
    }

    #[test]
    fn union_arg_with_binary_alternative_accepted_for_same_shape() {
        // The complementary positive to the rejection below: the SAME fitting
        // arm (ТаблицаЗначений), but the alternative is the binary payload —
        // the template-ambiguity carve-out applies and the call passes.
        let db = InMemoryDb::new();
        let table = db.value_table(None, bsl_types::facet::TableSource::Unknown);
        let binary = db.platform_object("ДвоичныеДанные".to_string());
        let from = db.union(vec![table, binary]);
        assert!(is_coercible_to(&db, from, table));
    }

    #[test]
    fn union_arg_with_non_binary_alternative_rejected() {
        // A dynamic РезультатЗапроса.Выгрузить yields ТаблицаЗначений |
        // ДеревоЗначений; loading the tree arm into a flat ТаблицаЗначений
        // slot is a genuine error and must keep firing — the existential
        // acceptance is reserved for the binary-payload template ambiguity.
        let db = InMemoryDb::new();
        let table = db.value_table(None, bsl_types::facet::TableSource::Unknown);
        let tree = db.platform_object("ДеревоЗначений".to_string());
        let from = db.union(vec![table, tree]);
        assert!(!is_coercible_to(&db, from, table));
    }

    #[test]
    fn union_arg_with_undefined_flag_member_rejected() {
        let db = InMemoryDb::new();
        let structure = db.structure(None);
        let from = db.union(vec![structure, db.undefined()]);
        assert!(
            !is_coercible_to(&db, from, structure),
            "a maybe-absent value into a slot that does not admit absence must keep firing"
        );
    }

    #[test]
    fn union_arg_with_null_flag_into_ref_passes() {
        let db = InMemoryDb::new();
        let cat = metadata_ref_id(&db, MetadataKind::CatalogRef, "Контрагенты");
        let from = db.union(vec![cat, db.null()]);
        assert!(
            is_coercible_to(&db, from, cat),
            "Null flows into ref slots by the null-to-ref rule, so the flag member fits"
        );
    }

    #[test]
    fn union_arg_without_fitting_regular_member_rejected() {
        let db = InMemoryDb::new();
        let from = db.union(vec![db.number(None, None), db.boolean()]);
        let to = db.date(bsl_types::facet::DateComponent::DateTime);
        assert!(!is_coercible_to(&db, from, to));
    }

    #[test]
    fn spreadsheet_document_assignable_to_area_param_one_way() {
        let db = InMemoryDb::new();
        let spreadsheet = db.platform_object("ТабличныйДокумент".to_string());
        let area = db.platform_object("ОбластьЯчеекТабличногоДокумента".to_string());
        let area_en = db.platform_object("SpreadsheetDocumentRange".to_string());
        assert!(is_assignable(&db, spreadsheet, area));
        assert!(is_assignable(&db, spreadsheet, area_en));
        assert!(
            !is_assignable(&db, area, spreadsheet),
            "an area value must not be admitted where a full spreadsheet document is required"
        );
        let unrelated = db.platform_object("ТаблицаЗначений".to_string());
        assert!(!is_assignable(&db, spreadsheet, unrelated));
        assert!(!is_assignable(&db, unrelated, area));
    }

    #[test]
    fn concrete_manager_assignable_to_generic_manager_one_way() {
        let db = InMemoryDb::new();
        let manager = db.object_manager(
            bsl_metadata::MdoType::Document,
            "ПКО".to_string(),
            &bsl_types::testing::RootConfigCtx,
        );
        let generic_ru = db.platform_object("ДокументМенеджер".to_string());
        let generic_en = db.platform_object("DocumentManager".to_string());
        assert!(is_assignable(&db, manager, generic_ru));
        assert!(is_assignable(&db, manager, generic_en));
        assert!(
            !is_assignable(&db, generic_ru, manager),
            "a generic manager must not be admitted where a concrete one is required"
        );
        let wrong_kind = db.platform_object("СправочникМенеджер".to_string());
        assert!(
            !is_assignable(&db, manager, wrong_kind),
            "a document manager is not a catalog manager"
        );
    }

    #[test]
    fn this_object_assignable_to_generic_object_name() {
        let db = InMemoryDb::new();
        let owner = bsl_types::facet::MdoRefFacet::new(
            bsl_metadata::MdoType::Document,
            "ЗаказДавальца".to_string(),
        );
        let this_object = db.mk_this_object(bsl_types::kind::ConfigId::Root, owner.clone());
        let generic = db.platform_object("ДокументОбъект".to_string());
        assert!(is_assignable(&db, this_object, generic));
        let wrong = db.platform_object("СправочникОбъект".to_string());
        assert!(!is_assignable(&db, this_object, wrong));

        let this_manager = db.mk_this_manager(bsl_types::kind::ConfigId::Root, owner);
        let generic_manager = db.platform_object("ДокументМенеджер".to_string());
        assert!(is_assignable(&db, this_manager, generic_manager));
    }

    #[test]
    fn tabular_section_assignable_to_generic_tabular_section() {
        let db = InMemoryDb::new();
        let ts = metadata_ref_id(
            &db,
            MetadataKind::TabularSection { parent: bsl_metadata::MdoType::Document },
            "ЗаказДавальца.Товары",
        );
        assert!(is_assignable(&db, ts, db.platform_object("ТабличнаяЧасть".to_string())));
        assert!(!is_assignable(&db, ts, db.platform_object("СправочникОбъект".to_string())));
    }

    #[test]
    fn typed_form_data_assignable_to_generic_form_data_name() {
        let db = InMemoryDb::new();
        let owner = bsl_types::facet::MdoRefFacet::new(
            bsl_metadata::MdoType::Document,
            "Анкета".to_string(),
        );
        let typed = db.mk_form_data(bsl_types::facet::FormDataFacet::Structure, Some(owner));
        let generic = db.platform_object("ДанныеФормыСтруктура".to_string());
        assert!(is_assignable(&db, typed, generic));
        let other = db.platform_object("ДанныеФормыКоллекция".to_string());
        assert!(
            !is_assignable(&db, typed, other),
            "a structure is not a collection — only the matching generic name widens"
        );
    }

    #[test]
    fn managed_form_admits_its_deprecated_synonym_name() {
        let db = InMemoryDb::new();
        let form = db.platform_object("ФормаКлиентскогоПриложения".to_string());
        for synonym in ["УправляемаяФорма", "управляемаяформа", "ManagedForm"]
        {
            assert!(
                is_assignable(&db, form, db.platform_object(synonym.to_string())),
                "ФормаКлиентскогоПриложения must satisfy its deprecated synonym {synonym:?}"
            );
        }
        assert!(
            !is_assignable(&db, db.platform_object("УправляемаяФорма".to_string()), form),
            "one direction only"
        );
        // `Форма` is the legacy ordinary-form type with its own API, NOT a
        // supertype the managed form satisfies — must keep firing.
        assert!(!is_assignable(&db, form, db.platform_object("Форма".to_string())));
        assert!(!is_assignable(&db, form, db.platform_object("Form".to_string())));
        assert!(
            !is_assignable(&db, form, db.platform_object("Структура".to_string())),
            "the bridge must not open unrelated platform objects"
        );
    }

    #[test]
    fn all_form_items_is_not_widened_to_form_items_collection() {
        let db = InMemoryDb::new();
        // ВсеЭлементыФормы and ЭлементыФормы are NOT in a subtype relation: the
        // latter has Получить/Индекс that the former lacks, so the bridge must
        // not admit one for the other in either direction.
        let all = db.platform_object("ВсеЭлементыФормы".to_string());
        let items = db.platform_object("ЭлементыФормы".to_string());
        assert!(!is_assignable(&db, all, items));
        assert!(!is_assignable(&db, items, all));
    }

    /// Корпусные имена сводит к канону интернирование, а внекорпусному номиналу
    /// канона взять неоткуда — регистр ему складывает уже присваиваемость.
    #[test]
    fn uncorpused_nominal_names_fold_case() {
        let db = InMemoryDb::new();
        for (a, b) in
            [("ДокументМенеджер", "документменеджер"), ("УправляемаяФорма", "УПРАВЛЯЕМАЯФОРМА")]
        {
            let left = db.platform_object(a.to_string());
            let right = db.platform_object(b.to_string());
            assert!(is_assignable(&db, left, right), "{a} и {b} — одно имя");
            assert!(is_assignable(&db, right, left), "и в обратную сторону");
        }
        let unrelated = db.platform_object("ДокументМенеджер".to_string());
        let other = db.platform_object("СправочникМенеджер".to_string());
        assert!(!is_assignable(&db, unrelated, other), "разные имена совместимыми не становятся");
    }

    #[test]
    fn generic_bridge_is_cyrillic_case_insensitive() {
        let db = InMemoryDb::new();
        let manager = db.object_manager(
            bsl_metadata::MdoType::Document,
            "ПКО".to_string(),
            &bsl_types::testing::RootConfigCtx,
        );
        let lowercase = db.platform_object("документМенеджер".to_string());
        assert!(
            is_assignable(&db, manager, lowercase),
            "BSL identifiers are case-insensitive, Cyrillic included"
        );
    }

    #[test]
    fn this_object_coercible_to_any_ref_argument_only() {
        let db = InMemoryDb::new();
        let owner = bsl_types::facet::MdoRefFacet::new(
            bsl_metadata::MdoType::Document,
            "ЗаказДавальца".to_string(),
        );
        let this_object = db.mk_this_object(bsl_types::kind::ConfigId::Root, owner.clone());
        assert!(
            is_coercible_to(&db, this_object, db.any_ref()),
            "СообщитьПользователю(..., ЭтотОбъект, ...) is the canonical БСП call"
        );
        assert!(
            !is_assignable(&db, this_object, db.any_ref()),
            "in the lattice an object stays distinct from a ref"
        );
        let this_manager = db.mk_this_manager(bsl_types::kind::ConfigId::Root, owner);
        assert!(
            !is_coercible_to(&db, this_manager, db.any_ref()),
            "a manager is not a ref under any policy"
        );
    }

    #[test]
    fn never_is_bottom() {
        let db = InMemoryDb::new();
        let never = db.never();
        let number = db.number(None, None);
        assert!(is_assignable(&db, never, number), "Never ≤ A (bottom)");
        assert!(!is_assignable(&db, number, never), "A ≤ Never must fail (not reflexive)");
        assert!(is_assignable(&db, never, never), "Never ≤ Never (reflexive)");
    }

    fn projected_value_table(db: &dyn TypeKernelDb, col: &str) -> TypeId {
        use bsl_types::facet::TableSource;
        use bsl_types::kind::{
            Projection, ProjectionField, ProjectionFieldSource, ProjectionOrigin,
        };
        let proj = Arc::new(Projection::new(
            Arc::from([ProjectionField::new(
                col.to_string(),
                db.string(None, false),
                ProjectionFieldSource::Column,
            )]),
            ProjectionOrigin::SdblQuery,
            None,
        ));
        db.value_table(Some(proj), TableSource::SdblUnload)
    }

    #[test]
    fn value_table_projection_coerces_to_bare_union_member() {
        use bsl_types::facet::TableSource;
        let db = InMemoryDb::new();

        // `Запрос.Выполнить().Выгрузить()` — a projected ValueTable.
        let arg = projected_value_table(&db, "ШтрихкодУпаковки");
        // A callee documents the slot as `Неопределено | ТаблицаЗначений`: the
        // ValueTable member is bare (no projection).
        let bare = db.value_table(None, TableSource::Unknown);
        let param = db.union(vec![db.undefined(), bare]);

        assert!(
            is_coercible_to(&db, arg, param),
            "a projected ValueTable must satisfy a bare ТаблицаЗначений union member"
        );
        assert!(
            is_assignable(&db, arg, bare),
            "the union .any() path relies on direct assignability"
        );
    }

    #[test]
    fn tabular_section_coercible_to_bare_value_table_argument_only() {
        use bsl_types::facet::TableSource;
        let db = InMemoryDb::new();
        // `ЗаполнитьДатыОтгрузкиВТаблице(Дата, Объект.Продукция, …)` — the helper
        // documents the slot as the bare `ТаблицаЗначений`.
        let ts = metadata_ref_id(
            &db,
            MetadataKind::TabularSection { parent: bsl_metadata::MdoType::Document },
            "ЗаказДавальца.Продукция",
        );
        let value_table = db.value_table(None, TableSource::Unknown);
        assert!(
            is_coercible_to(&db, ts, value_table),
            "a tabular section satisfies a ТаблицаЗначений slot at the call boundary"
        );
        // A documented union slot `ТаблицаЗначений, РезультатЗапроса, Массив`
        // still admits the tabular section through its bare ValueTable member.
        let union_slot = db.union(vec![
            value_table,
            db.platform_object("РезультатЗапроса".to_string()),
            db.array(None),
        ]);
        assert!(is_coercible_to(&db, ts, union_slot), "the bare ВТ union member admits it");

        assert!(
            !is_assignable(&db, ts, value_table),
            "in the lattice a tabular section stays distinct from a ValueTable"
        );
        assert!(
            !is_coercible_to(&db, value_table, ts),
            "one direction only — a real ТаблицаЗначений is not a concrete tabular section"
        );
        // A column-shaped (projected) target is an inferred slot, not a doc
        // type; the tabular section is not proven to carry those columns.
        let projected = projected_value_table(&db, "Номенклатура");
        assert!(
            !is_coercible_to(&db, ts, projected),
            "a projected ValueTable target is not admitted without proven column shape"
        );
    }

    #[test]
    fn form_table_control_assignable_to_generic_form_table_one_way() {
        use bsl_types::facet::FormElementFacet;
        let db = InMemoryDb::new();
        // `Элементы.Список` is a form table control passed into a parameter
        // documented `ТаблицаФормы` (a union member in the БСП command helper).
        let control = db.mk_form_control(FormElementFacet::Table, None);
        let generic_ru = db.platform_object("ТаблицаФормы".to_string());
        let generic_en = db.platform_object("FormTable".to_string());
        assert!(is_assignable(&db, control, generic_ru));
        assert!(is_assignable(&db, control, generic_en));
        assert!(
            !is_assignable(&db, generic_ru, control),
            "a generic ТаблицаФормы must not be admitted where a concrete control is required"
        );
        let wrong = db.platform_object("ПолеФормы".to_string());
        assert!(
            !is_assignable(&db, control, wrong),
            "a table control is not a ПолеФормы — only the matching generic name widens"
        );
        let field = db.mk_form_control(FormElementFacet::Field, None);
        assert!(is_assignable(&db, field, db.platform_object("ПолеФормы".to_string())));
    }

    #[test]
    fn form_item_union_argument_is_accepted_when_one_kind_fits() {
        let db = InMemoryDb::new();
        let item = |name: &str| db.platform_object(name.to_string());
        let created = db.union(vec![
            item("ДекорацияФормы"),
            item("ГруппаФормы"),
            item("КнопкаФормы"),
            item("ТаблицаФормы"),
            item("ПолеФормы"),
        ]);
        let parent_slot = db.union(vec![
            item("ГруппаФормы"),
            item("ТаблицаФормы"),
            item("ФормаКлиентскогоПриложения"),
        ]);
        assert!(
            is_coercible_to(&db, created, parent_slot),
            "the kind chosen by a non-literal Тип argument may be a group"
        );
        assert!(
            !is_assignable(&db, created, parent_slot),
            "the lattice keeps the all-members rule"
        );
        assert!(
            !is_coercible_to(&db, item("ПолеФормы"), parent_slot),
            "a narrowed field still cannot be a parent"
        );
        let mixed = db.union(vec![item("ГруппаФормы"), db.number(None, None)]);
        assert!(
            !is_coercible_to(&db, mixed, item("ГруппаФормы")),
            "only a union made of form items gets the latitude"
        );
        let maybe_absent = db.union(vec![item("ГруппаФормы"), item("ПолеФормы"), db.undefined()]);
        assert!(
            !is_coercible_to(&db, maybe_absent, parent_slot),
            "a possibly absent item still fails a slot that does not admit Неопределено"
        );
    }

    #[test]
    fn form_group_family_all_widen_to_generic_form_group() {
        use bsl_types::facet::FormElementFacet as K;
        let db = InMemoryDb::new();
        let group_ru = db.platform_object("ГруппаФормы".to_string());
        let group_en = db.platform_object("FormGroup".to_string());
        for kind in [K::Group, K::UsualGroup, K::Pages, K::Page, K::CommandBar, K::ButtonGroup] {
            let control = db.mk_form_control(kind, None);
            assert!(is_assignable(&db, control, group_ru), "{kind:?} widens to ГруппаФормы");
            assert!(is_assignable(&db, control, group_en), "{kind:?} widens to FormGroup");
            assert!(
                !is_assignable(&db, control, db.platform_object("ТаблицаФормы".to_string())),
                "{kind:?} is a group, not a ТаблицаФормы"
            );
        }
        // An untyped `Other` control widens to nothing.
        let other = db.mk_form_control(K::Other, None);
        assert!(!is_assignable(&db, other, group_ru));
    }

    #[test]
    fn value_table_distinct_projections_stay_distinct_but_bridge_the_bare_type() {
        use bsl_types::facet::TableSource;
        let db = InMemoryDb::new();
        let a = projected_value_table(&db, "A");
        let b = projected_value_table(&db, "B");
        let bare = db.value_table(None, TableSource::Unknown);

        // Two fully-projected, differently-shaped tables remain distinguishable.
        assert!(!is_assignable(&db, a, b));
        // Either is interchangeable with the bare type, both directions.
        assert!(is_assignable(&db, a, bare));
        assert!(is_assignable(&db, bare, a));
    }

    #[test]
    fn colon_qualified_platform_phantom_is_assignable_to_its_generic() {
        // `Метаданные.НайтиПоТипу` returns a union of `ОбъектМетаданных: <variant>` phantoms;
        // each IS an `ОбъектМетаданных` (the bare name a doc comment annotates), but not the
        // reverse, and not a different generic.
        let db = InMemoryDb::new();
        let specific = db.platform_object("ОбъектМетаданных: HTTPСервис".to_string());
        let generic = db.platform_object("ОбъектМетаданных".to_string());
        assert!(is_assignable(&db, specific, generic), "specific variant IS the generic");
        assert!(!is_assignable(&db, generic, specific), "generic is NOT a specific variant");
        assert!(
            !is_assignable(
                &db,
                specific,
                db.platform_object("ОбъектМетаданныхСправочник".to_string())
            ),
            "the colon prefix must match the generic exactly, not merely share a stem"
        );
    }
}
