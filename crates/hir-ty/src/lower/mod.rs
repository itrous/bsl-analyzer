pub(crate) mod builtin_names;
pub(crate) mod doc_structure;
pub mod type_string;

use std::collections::HashSet;
use stdx::case::CaseExt;

use bsl_metadata::{resolve_defined_type_terminal, MdoType, MetadataResolver};
use bsl_platform::PlatformData;
use bsl_types::builders::Builders;
use bsl_types::intern::TypeKernelDb;
use bsl_types::kind::{MetadataKind, MetadataReferenceKind, TypeId, TypeKind};
use bsl_types::testing::RootConfigCtx;
use hir_def::path::QualifiedName;
use hir_def::type_ref::TypeRef;
use hir_def::Name;

#[derive(Debug, Default, Clone, Copy)]
pub struct TyLoweringContext<'a> {
    resolver: Option<&'a dyn MetadataResolver>,
}

impl<'a> TyLoweringContext<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_resolver(resolver: &'a dyn MetadataResolver) -> Self {
        Self { resolver: Some(resolver) }
    }

    pub fn lower_type_ref_id(&self, db: &dyn TypeKernelDb, type_ref: &TypeRef) -> TypeId {
        let mut visited = HashSet::new();
        self.lower_type_ref_id_inner(db, type_ref, &mut visited)
    }

    fn lower_type_ref_id_inner(
        &self,
        db: &dyn TypeKernelDb,
        type_ref: &TypeRef,
        visited: &mut HashSet<String>,
    ) -> TypeId {
        match type_ref {
            TypeRef::Builtin(b) => builtin_names::builtin_to_typeid(db, *b),
            TypeRef::Array(Some(elem)) => {
                db.array(Some(self.lower_type_ref_id_inner(db, elem, visited)))
            }
            TypeRef::Array(None) => db.array(None),
            TypeRef::Map(_) => db.map(None, None),
            TypeRef::Name(qname) => match qname.len() {
                0 => db.unknown(),
                1 => {
                    degrade_unrecognised_annotation(db, self.lower_bare_name_id(db, qname.first()))
                }
                _ => self.lower_qualified_id_inner(db, qname, visited),
            },
            TypeRef::Union(parts) => {
                let lowered: Vec<TypeId> =
                    parts.iter().map(|t| self.lower_type_ref_id_inner(db, t, visited)).collect();
                db.union(lowered)
            }
            TypeRef::AnyRef => db.any_ref(),
            TypeRef::AnyRefOf(mdo) => db.any_metadata_ref(*mdo),
            TypeRef::Any => db.any(),
            TypeRef::Unknown => db.unknown(),
        }
    }

    pub fn lower_bare_name_id(&self, db: &dyn TypeKernelDb, name: &Name) -> TypeId {
        let raw = name.as_str();

        if let Some(tref) = TypeRef::from_bare_name(raw) {
            return self.lower_type_ref_id(db, &tref);
        }

        if let Some(mdo) = MdoType::from_plural(raw) {
            if mdo.manager_type_prefix().is_some() {
                return db.manager_collection(mdo);
            }
        }

        if let Some(kind) = MetadataReferenceKind::from_plural(raw) {
            return db.metadata_reference_collection(kind);
        }

        // A bare metadata-kind name with no specific object (e.g.
        // `РегистрНакопленияНаборЗаписей`, `СправочникСсылка`) names "any value of
        // this kind" — we cannot model it precisely. Lower to `Any` (the top type)
        // rather than `Unknown`: both are permissive in assignability, but the type
        // kernel drops `Unknown` from a union (`T | Unknown == T`) while `Any`
        // dominates it (`T | Any == Any`). A doc-comment that lists such a kind among
        // a union of accepted types (e.g. `ЛюбаяСсылка, …НаборЗаписей, ТаблицаЗначений`)
        // must stay permissive instead of silently narrowing to the modellable arms
        // and then flagging a concrete record set as a type mismatch.
        if metadata_kind_from_prefix(raw).is_some() {
            return db.any();
        }

        // A bare name that resolves to no known type is lowered to a nominal
        // phantom `PlatformObject`. This is authoritative: a construction
        // `Новый X` defines the variable's type as `X` (and the IDE keys hover /
        // go-to-definition / document-symbols on it) even when the corpus does
        // not model `X`. Where the name is only a *hypothesis* — a doc-comment /
        // parameter type that may be a typo — the annotation entry point
        // [`Self::lower_type_ref_id`] post-filters an unrecognised phantom down to
        // Unknown (see `degrade_unrecognised_annotation`); that filter is NOT
        // applied here, so constructions keep their type.
        db.platform_object(raw.to_string())
    }

    pub fn lower_qualified_id(&self, db: &dyn TypeKernelDb, qname: &QualifiedName) -> TypeId {
        let mut visited = HashSet::new();
        self.lower_qualified_id_inner(db, qname, &mut visited)
    }

    fn lower_qualified_id_inner(
        &self,
        db: &dyn TypeKernelDb,
        qname: &QualifiedName,
        visited: &mut HashSet<String>,
    ) -> TypeId {
        if qname.len() != 2 {
            return db.unknown();
        }

        let prefix = qname.first().as_str();

        if is_defined_type_prefix(prefix) {
            let Some(resolver) = self.resolver else {
                return db.unknown();
            };
            let name = qname.last().as_str();
            let key = name.fold_lower();

            if !visited.insert(key.clone()) {
                return db.unknown();
            }

            let mut chain_visited = HashSet::new();
            let result = resolve_defined_type_terminal(resolver, name, &mut chain_visited)
                .map(|underlying| {
                    let tref = TypeRef::from_attribute_type(&underlying);
                    self.lower_type_ref_id_inner(db, &tref, visited)
                })
                .unwrap_or_else(|| db.unknown());

            visited.remove(&key);
            return result;
        }

        match metadata_kind_from_prefix(prefix) {
            Some(kind) => db.metadata_ref(kind, qname.last().as_str().to_string(), &RootConfigCtx),
            None => db.unknown(),
        }
    }
}

/// In an *annotation* position (a doc-comment / parameter / return type), a name
/// that lowered to a phantom `PlatformObject` is only trustworthy when it is a
/// real type — the corpus or one of the curated real-but-uncorpused families. An
/// unrecognised phantom there is a doc-comment typo (`СпискоЗначений`) or free
/// prose (`документ`), which matches no genuinely inferred value and so only ever
/// produces false `TypeMismatch`; degrade it to Unknown. Constructions never
/// reach this filter (they lower through `lower_bare_name_id` directly), so a
/// `Новый X` keeps its type.
fn degrade_unrecognised_annotation(db: &dyn TypeKernelDb, id: TypeId) -> TypeId {
    if let TypeKind::PlatformObject(facet) = db.lookup_type(id) {
        if PlatformData::instance().get_type(&facet.name).is_none()
            && !crate::platform_type_name::is_known_non_corpus_type_name(&facet.name)
        {
            return db.unknown();
        }
    }
    id
}

fn is_defined_type_prefix(prefix: &str) -> bool {
    let lower = prefix.fold_lower();
    lower == "определяемыйтип" || lower == "definedtype"
}

fn metadata_kind_from_prefix(prefix: &str) -> Option<MetadataKind> {
    match prefix.fold_lower().as_str() {
        "catalogref" | "справочникссылка" => Some(MetadataKind::CatalogRef),
        "catalogobject" | "справочникобъект" => Some(MetadataKind::CatalogObject),
        "documentref" | "документссылка" => Some(MetadataKind::DocumentRef),
        "documentobject" | "документобъект" => Some(MetadataKind::DocumentObject),
        "enumref" | "перечислениессылка" => Some(MetadataKind::EnumRef),
        "taskref" | "задачассылка" => Some(MetadataKind::TaskRef),
        "taskobject" | "задачаобъект" => Some(MetadataKind::TaskObject),
        "businessprocessref" | "бизнеспроцессссылка" => {
            Some(MetadataKind::BusinessProcessRef)
        }
        "businessprocessobject" | "бизнеспроцессобъект" => {
            Some(MetadataKind::BusinessProcessObject)
        }
        "dataprocessorobject" | "обработкаобъект" => {
            Some(MetadataKind::DataProcessorObject)
        }
        "reportobject" | "отчётобъект" | "отчетобъект" => {
            Some(MetadataKind::ReportObject)
        }
        "externaldataprocessorobject" | "внешняяобработкаобъект" => {
            Some(MetadataKind::ExternalDataProcessorObject)
        }
        "externalreportobject" | "внешнийотчётобъект" | "внешнийотчетобъект" => {
            Some(MetadataKind::ExternalReportObject)
        }
        "exchangeplanref" | "планобменассылка" => {
            Some(MetadataKind::ExchangePlanRef)
        }
        "exchangeplanobject" | "планобменаобъект" => {
            Some(MetadataKind::ExchangePlanObject)
        }
        "chartofaccountsref" | "плансчетовссылка" => {
            Some(MetadataKind::ChartOfAccountsRef)
        }
        "chartofaccountsobject" | "плансчетовобъект" => {
            Some(MetadataKind::ChartOfAccountsObject)
        }
        "chartofcharacteristictypesref" | "планвидовхарактеристикссылка" => {
            Some(MetadataKind::ChartOfCharacteristicTypesRef)
        }
        "chartofcharacteristictypesobject" | "планвидовхарактеристикобъект" => {
            Some(MetadataKind::ChartOfCharacteristicTypesObject)
        }
        "chartofcalculationtypesref" | "планвидоврасчетассылка" => {
            Some(MetadataKind::ChartOfCalculationTypesRef)
        }
        "chartofcalculationtypesobject" | "планвидоврасчетаобъект" => {
            Some(MetadataKind::ChartOfCalculationTypesObject)
        }
        "informationregisterrecordmanager" | "регистрсведенийменеджерзаписи" => {
            Some(MetadataKind::InformationRegisterRecordManager)
        }
        "informationregisterrecordset" | "регистрсведенийнаборзаписей" => {
            Some(MetadataKind::InformationRegisterRecordSet)
        }
        "informationregisterref" | "регистрсведенийключзаписи" => {
            Some(MetadataKind::InformationRegisterRef)
        }
        "accumulationregisterrecordset" | "регистрнакоплениянаборзаписей" => {
            Some(MetadataKind::AccumulationRegisterRecordSet)
        }
        "accumulationregisterref" | "регистрнакопленияключзаписи" => {
            Some(MetadataKind::AccumulationRegisterRef)
        }
        "accountingregisterrecordset" | "регистрбухгалтериинаборзаписей" => {
            Some(MetadataKind::AccountingRegisterRecordSet)
        }
        "accountingregisterref" | "регистрбухгалтерииключзаписи" => {
            Some(MetadataKind::AccountingRegisterRef)
        }
        "calculationregisterrecordset" | "регистррасчетанаборзаписей" => {
            Some(MetadataKind::CalculationRegisterRecordSet)
        }
        "calculationregisterref" | "регистррасчетаключзаписи" => {
            Some(MetadataKind::CalculationRegisterRef)
        }
        "informationregisterrecord" => Some(MetadataKind::InformationRegisterRecord),
        "accumulationregisterrecord" => Some(MetadataKind::AccumulationRegisterRecord),
        "accountingregisterrecord" => Some(MetadataKind::AccountingRegisterRecord),
        "calculationregisterrecord" => Some(MetadataKind::CalculationRegisterRecord),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_types::kind::TypeKind;
    use bsl_types::testing::InMemoryDb;
    use hir_def::type_ref::BuiltinTypeRef;

    fn ctx() -> TyLoweringContext<'static> {
        TyLoweringContext::new()
    }

    fn assert_metadata_ref(
        db: &InMemoryDb,
        id: TypeId,
        expected_kind: MetadataKind,
        expected_name: &str,
    ) {
        match db.lookup_type(id) {
            TypeKind::MetadataRef(facet) => {
                assert_eq!(facet.kind, expected_kind);
                assert_eq!(facet.name.as_str(), expected_name);
            }
            other => {
                panic!("expected MetadataRef({expected_kind:?}, {expected_name}), got {other:?}")
            }
        }
    }

    #[test]
    fn ty_lowering_builtin_primitive() {
        let db = InMemoryDb::new();
        assert_eq!(
            ctx().lower_type_ref_id(&db, &TypeRef::Builtin(BuiltinTypeRef::Number)),
            db.number(None, None)
        );
        assert_eq!(
            ctx().lower_type_ref_id(&db, &TypeRef::Builtin(BuiltinTypeRef::String)),
            db.string(None, false)
        );
        assert_eq!(
            ctx().lower_type_ref_id(&db, &TypeRef::Builtin(BuiltinTypeRef::Undefined)),
            db.undefined()
        );
        assert_eq!(
            ctx().lower_type_ref_id(&db, &TypeRef::Builtin(BuiltinTypeRef::ValueTable)),
            db.value_table(None, bsl_types::facet::TableSource::Unknown)
        );
    }

    #[test]
    fn ty_lowering_array_with_elem_lowers_to_typed_array() {
        let db = InMemoryDb::new();
        let array_with_elem =
            TypeRef::Array(Some(Box::new(TypeRef::Builtin(BuiltinTypeRef::Number))));
        assert_eq!(
            ctx().lower_type_ref_id(&db, &array_with_elem),
            db.array(Some(db.number(None, None)))
        );
    }

    #[test]
    fn ty_lowering_jsdoc_array_of_string_round_trip() {
        let db = InMemoryDb::new();
        let doc = "// Возвращаемое значение:\n//   Массив из Строка - результат\n";
        let hints = hir_def::ty::doc_types::parse_method_doc_types(doc).unwrap();
        assert_eq!(
            ctx().lower_type_ref_id(&db, &hints.ret),
            db.array(Some(db.string(None, false)))
        );
    }

    #[test]
    fn ty_lowering_array_none_stays_unparameterised() {
        let db = InMemoryDb::new();
        assert_eq!(ctx().lower_type_ref_id(&db, &TypeRef::Array(None)), db.array(None));
    }

    #[test]
    fn ty_lowering_map_drops_kv_pairs() {
        let map_with_kv = TypeRef::Map(Some((
            Box::new(TypeRef::Builtin(BuiltinTypeRef::String)),
            Box::new(TypeRef::Builtin(BuiltinTypeRef::Number)),
        )));
        let db = InMemoryDb::new();
        assert_eq!(ctx().lower_type_ref_id(&db, &map_with_kv), db.map(None, None));
    }

    #[test]
    fn ty_lowering_bare_builtin_bilingual() {
        let db = InMemoryDb::new();
        assert_eq!(ctx().lower_bare_name_id(&db, &Name::new("Число")), db.number(None, None));
        assert_eq!(ctx().lower_bare_name_id(&db, &Name::new("NUMBER")), db.number(None, None));
        assert_eq!(ctx().lower_bare_name_id(&db, &Name::new("Массив")), db.array(None));
        assert_eq!(ctx().lower_bare_name_id(&db, &Name::new("Соответствие")), db.map(None, None));
    }

    #[test]
    fn ty_lowering_manager_collection_plural() {
        let db = InMemoryDb::new();
        assert_eq!(
            ctx().lower_bare_name_id(&db, &Name::new("Документы")),
            db.manager_collection(MdoType::Document)
        );
        assert_eq!(
            ctx().lower_bare_name_id(&db, &Name::new("Справочники")),
            db.manager_collection(MdoType::Catalog)
        );
    }

    #[test]
    fn ty_lowering_bare_unknown_falls_to_platform_object() {
        let db = InMemoryDb::new();
        let request = Name::new("Запрос");
        assert_eq!(
            ctx().lower_bare_name_id(&db, &request),
            db.platform_object("Запрос".to_string())
        );

        let mixed = Name::new("HTTPЗапрос");
        assert_eq!(
            ctx().lower_bare_name_id(&db, &mixed),
            db.platform_object("HTTPЗапрос".to_string())
        );
    }

    fn annotation_name(db: &InMemoryDb, name: &str) -> TypeId {
        ctx().lower_type_ref_id(db, &TypeRef::Name(QualifiedName::from_segments([Name::new(name)])))
    }

    #[test]
    fn ty_lowering_annotation_degrades_unrecognised_name_construction_keeps_it() {
        let db = InMemoryDb::new();
        for junk in ["СпискоЗначений", "документ", "СправочникОбъектИмяСправочника", "Структра"]
        {
            // In an annotation (doc-comment / parameter type) an unrecognised name
            // is a typo / prose that only ever produces false positives — degrade.
            assert_eq!(
                annotation_name(&db, junk),
                db.unknown(),
                "annotation {junk:?} must degrade to Unknown"
            );
            // The same name in a construction (`Новый junk`) is authoritative and
            // keeps its nominal type, so the IDE can hover / navigate it.
            assert_eq!(
                ctx().lower_bare_name_id(&db, &Name::new(junk)),
                db.platform_object(junk.to_string()),
                "construction {junk:?} must keep its nominal type"
            );
        }
    }

    #[test]
    fn ty_lowering_annotation_keeps_real_uncorpused_families() {
        let db = InMemoryDb::new();
        // Real platform types absent from the corpus stay nominal even as an
        // annotation, so doc-typed parameters keep matching real values.
        for name in [
            // Generic managers — bridge targets with subtype tests; must stay nominal.
            "ДокументМенеджер",
            "DocumentManager",
            "РегистрСведенийМенеджер",
            "ОбъектМетаданных",
            "ОбъектМетаданныхСправочник",
            "ТабличнаяЧасть",
            "СтрокаТабличнойЧасти",
            "НаборЗаписей",
            "РегистрНакопленияЗапись",
            "РасширениеУправляемойФормыДляОбъектов",
            "ВнешняяКомпонента",
            "УправляемаяФорма",
            "ТипXDTO",
        ] {
            assert_eq!(
                annotation_name(&db, name),
                db.platform_object(name.to_string()),
                "{name:?} is a real (uncorpused) type and must keep its phantom"
            );
        }
    }

    #[test]
    fn ty_lowering_bare_metadata_prefix_without_name_is_any() {
        // A bare kind prefix with no object names "any value of this kind"; it lowers
        // to `Any` (permissive top) so that, listed among a doc-comment union of
        // accepted types, it dominates the union instead of being dropped as Unknown.
        let db = InMemoryDb::new();
        assert_eq!(ctx().lower_bare_name_id(&db, &Name::new("СправочникСсылка")), db.any());
        assert_eq!(ctx().lower_bare_name_id(&db, &Name::new("CatalogRef")), db.any());
        assert_eq!(ctx().lower_bare_name_id(&db, &Name::new("documentobject")), db.any());
        // Generic object and record-set kind names resolve to the permissive `Any`
        // at this branch BEFORE the bare-name fallback gate — a deliberate
        // pre-existing choice (narrowing them to a nominal phantom would falsely
        // flag a concrete record set listed among a documented union). The
        // unknown-name degradation gate does NOT change this.
        assert_eq!(ctx().lower_bare_name_id(&db, &Name::new("СправочникОбъект")), db.any());
        assert_eq!(
            ctx().lower_bare_name_id(&db, &Name::new("РегистрСведенийНаборЗаписей")),
            db.any()
        );
    }

    #[test]
    fn ty_lowering_qualified_unmodelled_prefix_is_unknown() {
        let prefix = "ConstantValueManager";
        let db = InMemoryDb::new();
        let qname = QualifiedName::from_segments([Name::new(prefix), Name::new("Х")]);
        assert_eq!(
            ctx().lower_qualified_id(&db, &qname),
            db.unknown(),
            "expected Unknown for `{prefix}.Х`"
        );
    }

    #[test]
    fn metadata_kind_exchange_plan_and_chart_of_accounts_lower_bilingual() {
        for (prefix, expected) in [
            ("ExchangePlanRef", MetadataKind::ExchangePlanRef),
            ("ПланОбменаСсылка", MetadataKind::ExchangePlanRef),
            ("ExchangePlanObject", MetadataKind::ExchangePlanObject),
            ("ПланОбменаОбъект", MetadataKind::ExchangePlanObject),
            ("ChartOfAccountsRef", MetadataKind::ChartOfAccountsRef),
            ("ПланСчетовСсылка", MetadataKind::ChartOfAccountsRef),
            ("ChartOfAccountsObject", MetadataKind::ChartOfAccountsObject),
            ("ПланСчетовОбъект", MetadataKind::ChartOfAccountsObject),
            ("ChartOfCharacteristicTypesObject", MetadataKind::ChartOfCharacteristicTypesObject),
            ("ПланВидовХарактеристикОбъект", MetadataKind::ChartOfCharacteristicTypesObject),
            ("ChartOfCharacteristicTypesRef", MetadataKind::ChartOfCharacteristicTypesRef),
            ("ПланВидовХарактеристикСсылка", MetadataKind::ChartOfCharacteristicTypesRef),
            ("ChartOfCalculationTypesObject", MetadataKind::ChartOfCalculationTypesObject),
            ("ПланВидовРасчетаОбъект", MetadataKind::ChartOfCalculationTypesObject),
            ("ChartOfCalculationTypesRef", MetadataKind::ChartOfCalculationTypesRef),
            ("ПланВидовРасчетаСсылка", MetadataKind::ChartOfCalculationTypesRef),
        ] {
            let db = InMemoryDb::new();
            let qname = QualifiedName::from_segments([Name::new(prefix), Name::new("Х")]);
            let id = ctx().lower_qualified_id(&db, &qname);
            assert_metadata_ref(&db, id, expected, "Х");
        }
    }

    #[test]
    fn metadata_kind_enum_and_task_and_bp_lower_bilingual() {
        for (prefix, expected) in [
            ("EnumRef", MetadataKind::EnumRef),
            ("ПеречислениеСсылка", MetadataKind::EnumRef),
            ("TaskRef", MetadataKind::TaskRef),
            ("ЗадачаСсылка", MetadataKind::TaskRef),
            ("BusinessProcessRef", MetadataKind::BusinessProcessRef),
            ("БизнесПроцессСсылка", MetadataKind::BusinessProcessRef),
        ] {
            let db = InMemoryDb::new();
            let qname = QualifiedName::from_segments([Name::new(prefix), Name::new("Х")]);
            let id = ctx().lower_qualified_id(&db, &qname);
            assert_metadata_ref(&db, id, expected, "Х");
        }
    }

    #[test]
    fn metadata_kind_register_refs_lower_bilingual() {
        for (prefix, expected) in [
            ("InformationRegisterRef", MetadataKind::InformationRegisterRef),
            ("РегистрСведенийКлючЗаписи", MetadataKind::InformationRegisterRef),
            ("AccumulationRegisterRef", MetadataKind::AccumulationRegisterRef),
            ("РегистрНакопленияКлючЗаписи", MetadataKind::AccumulationRegisterRef),
            ("AccountingRegisterRef", MetadataKind::AccountingRegisterRef),
            ("РегистрБухгалтерииКлючЗаписи", MetadataKind::AccountingRegisterRef),
            ("CalculationRegisterRef", MetadataKind::CalculationRegisterRef),
            ("РегистрРасчетаКлючЗаписи", MetadataKind::CalculationRegisterRef),
        ] {
            let db = InMemoryDb::new();
            let qname = QualifiedName::from_segments([Name::new(prefix), Name::new("Х")]);
            let id = ctx().lower_qualified_id(&db, &qname);
            assert_metadata_ref(&db, id, expected, "Х");
        }
    }

    #[test]
    fn ty_lowering_qualified_metadata_ref_english() {
        let db = InMemoryDb::new();
        let qname = QualifiedName::from_segments([Name::new("CatalogRef"), Name::new("Товары")]);
        let id = ctx().lower_qualified_id(&db, &qname);
        assert_metadata_ref(&db, id, MetadataKind::CatalogRef, "Товары");
    }

    #[test]
    fn ty_lowering_qualified_metadata_ref_russian() {
        let db = InMemoryDb::new();
        let qname = QualifiedName::from_segments([Name::new("ДокументСсылка"), Name::new("ПКО")]);
        let id = ctx().lower_qualified_id(&db, &qname);
        assert_metadata_ref(&db, id, MetadataKind::DocumentRef, "ПКО");
    }

    #[test]
    fn ty_lowering_qualified_unknown_prefix_is_unknown() {
        let db = InMemoryDb::new();
        let qname = QualifiedName::from_segments([Name::new("ОбщийМодуль"), Name::new("Х")]);
        assert_eq!(ctx().lower_qualified_id(&db, &qname), db.unknown());
    }

    #[test]
    fn ty_lowering_qualified_three_segments_deferred_to_task7() {
        let three = QualifiedName::from_segments([
            Name::new("Документы"),
            Name::new("ПКО"),
            Name::new("СоздатьДокумент"),
        ]);
        let db = InMemoryDb::new();
        assert_eq!(ctx().lower_qualified_id(&db, &three), db.unknown());
    }

    #[test]
    fn ty_lowering_union_flows_through_ty_union_constructor() {
        let tr = TypeRef::Union(vec![
            TypeRef::Builtin(BuiltinTypeRef::Number),
            TypeRef::Builtin(BuiltinTypeRef::String),
        ]);
        let db = InMemoryDb::new();
        let ty = ctx().lower_type_ref_id(&db, &tr);
        match db.lookup_type(ty) {
            TypeKind::Union(parts) => assert_eq!(parts.len(), 2),
            other => panic!("expected TypeKind::Union, got {other:?}"),
        }

        let flipped = TypeRef::Union(vec![
            TypeRef::Builtin(BuiltinTypeRef::String),
            TypeRef::Builtin(BuiltinTypeRef::Number),
        ]);
        assert_eq!(ctx().lower_type_ref_id(&db, &flipped), ty);
    }

    #[test]
    fn ty_lowering_union_singleton_collapses() {
        let tr = TypeRef::Union(vec![TypeRef::Builtin(BuiltinTypeRef::Number)]);
        let db = InMemoryDb::new();
        assert_eq!(ctx().lower_type_ref_id(&db, &tr), db.number(None, None));
    }

    #[test]
    fn ty_lowering_union_empty_becomes_unknown() {
        let db = InMemoryDb::new();
        assert_eq!(ctx().lower_type_ref_id(&db, &TypeRef::Union(vec![])), db.unknown());
    }

    #[test]
    fn ty_lowering_type_ref_routes_through_name_branches() {
        let db = InMemoryDb::new();
        let single = TypeRef::Name(QualifiedName::from_segments([Name::new("Массив")]));
        assert_eq!(ctx().lower_type_ref_id(&db, &single), db.array(None));

        let qualified = TypeRef::Name(QualifiedName::from_segments([
            Name::new("СправочникСсылка"),
            Name::new("Номенклатура"),
        ]));
        let id = ctx().lower_type_ref_id(&db, &qualified);
        assert_metadata_ref(&db, id, MetadataKind::CatalogRef, "Номенклатура");

        assert_eq!(ctx().lower_type_ref_id(&db, &TypeRef::AnyRef), db.any_ref());
        assert_eq!(ctx().lower_type_ref_id(&db, &TypeRef::Unknown), db.unknown());
    }

    use bsl_metadata::{AttributeType, MetadataResolver};

    #[derive(Debug, Default)]
    struct MockResolver(std::collections::HashMap<String, AttributeType>);

    impl MockResolver {
        fn with(entries: &[(&str, AttributeType)]) -> Self {
            let mut map = std::collections::HashMap::new();
            for (name, at) in entries {
                map.insert(name.fold_lower(), at.clone());
            }
            Self(map)
        }
    }

    impl MetadataResolver for MockResolver {
        fn resolve_defined_type(&self, name: &str) -> Option<AttributeType> {
            self.0.get(&name.fold_lower()).cloned()
        }
    }

    #[test]
    fn defined_type_without_resolver_stays_unknown() {
        let qname = QualifiedName::from_segments([
            Name::new("ОпределяемыйТип"),
            Name::new("ДенежнаяСумма"),
        ]);
        let db = InMemoryDb::new();
        assert_eq!(ctx().lower_qualified_id(&db, &qname), db.unknown());
    }

    #[test]
    fn defined_type_with_resolver_lowers_to_underlying_primitive() {
        let resolver = MockResolver::with(&[(
            "ДенежнаяСумма",
            AttributeType::Number { precision: 15, scale: 2 },
        )]);

        let lowering = TyLoweringContext::with_resolver(&resolver);
        let qname = QualifiedName::from_segments([
            Name::new("ОпределяемыйТип"),
            Name::new("ДенежнаяСумма"),
        ]);
        let db = InMemoryDb::new();
        assert_eq!(lowering.lower_qualified_id(&db, &qname), db.number(None, None));
    }

    #[test]
    fn defined_type_chain_lowers_through_terminal_walk() {
        let resolver = MockResolver::with(&[
            ("A", AttributeType::DefinedType { name: "B".to_string() }),
            ("B", AttributeType::String { length: Some(64) }),
        ]);

        let lowering = TyLoweringContext::with_resolver(&resolver);
        let qname = QualifiedName::from_segments([Name::new("ОпределяемыйТип"), Name::new("A")]);
        let db = InMemoryDb::new();
        assert_eq!(lowering.lower_qualified_id(&db, &qname), db.string(None, false));
    }

    #[test]
    fn defined_type_cycle_returns_unknown_without_overflow() {
        let resolver = MockResolver::with(&[
            ("A", AttributeType::DefinedType { name: "B".to_string() }),
            ("B", AttributeType::DefinedType { name: "A".to_string() }),
        ]);

        let lowering = TyLoweringContext::with_resolver(&resolver);
        let qname = QualifiedName::from_segments([Name::new("ОпределяемыйТип"), Name::new("A")]);
        let db = InMemoryDb::new();
        assert_eq!(lowering.lower_qualified_id(&db, &qname), db.unknown());
    }

    #[test]
    fn defined_type_composite_underlying_lowers_to_union() {
        let resolver = MockResolver::with(&[(
            "ЛюбоеЧислоИлиСтрока",
            AttributeType::Composite {
                types: vec![
                    AttributeType::Number { precision: 10, scale: 0 },
                    AttributeType::String { length: None },
                ],
            },
        )]);

        let lowering = TyLoweringContext::with_resolver(&resolver);
        let qname = QualifiedName::from_segments([
            Name::new("ОпределяемыйТип"),
            Name::new("ЛюбоеЧислоИлиСтрока"),
        ]);
        let db = InMemoryDb::new();
        match db.lookup_type(lowering.lower_qualified_id(&db, &qname)) {
            TypeKind::Union(arms) => {
                assert!(arms.contains(&db.number(None, None)), "union must contain Number");
                assert!(arms.contains(&db.string(None, false)), "union must contain String");
            }
            other => panic!("expected TypeKind::Union, got {other:?}"),
        }
    }

    #[test]
    fn defined_type_sibling_arms_share_terminal_step_independently() {
        let resolver = MockResolver::with(&[
            ("A", AttributeType::DefinedType { name: "X".to_string() }),
            ("B", AttributeType::DefinedType { name: "X".to_string() }),
            ("X", AttributeType::Number { precision: 10, scale: 0 }),
        ]);
        let lowering = TyLoweringContext::with_resolver(&resolver);

        let arm = |name: &str| {
            TypeRef::Name(QualifiedName::from_segments([
                Name::new("ОпределяемыйТип"),
                Name::new(name),
            ]))
        };
        let tref = TypeRef::Union(vec![arm("A"), arm("B")]);
        let db = InMemoryDb::new();
        assert_eq!(lowering.lower_type_ref_id(&db, &tref), db.number(None, None));
    }

    #[test]
    fn defined_type_self_referential_composite_is_safe() {
        let resolver = MockResolver::with(&[(
            "A",
            AttributeType::Composite {
                types: vec![
                    AttributeType::DefinedType { name: "A".to_string() },
                    AttributeType::Number { precision: 10, scale: 0 },
                ],
            },
        )]);
        let lowering = TyLoweringContext::with_resolver(&resolver);
        let qname = QualifiedName::from_segments([Name::new("ОпределяемыйТип"), Name::new("A")]);
        let db = InMemoryDb::new();
        assert_eq!(lowering.lower_qualified_id(&db, &qname), db.number(None, None));
    }

    #[test]
    fn russian_prefix_is_case_insensitive() {
        let resolver = MockResolver::with(&[("X", AttributeType::Boolean)]);
        let lowering = TyLoweringContext::with_resolver(&resolver);
        for prefix in ["ОпределяемыйТип", "определяемыйтип", "ОПРЕДЕЛЯЕМЫЙТИП"]
        {
            let qname = QualifiedName::from_segments([Name::new(prefix), Name::new("X")]);
            let db = InMemoryDb::new();
            assert_eq!(
                lowering.lower_qualified_id(&db, &qname),
                db.boolean(),
                "case-insensitive lookup failed for `{prefix}`"
            );
        }
    }

    #[test]
    fn defined_type_english_prefix_also_resolves() {
        let resolver = MockResolver::with(&[("X", AttributeType::Boolean)]);
        let lowering = TyLoweringContext::with_resolver(&resolver);
        let qname = QualifiedName::from_segments([Name::new("DefinedType"), Name::new("X")]);
        let db = InMemoryDb::new();
        assert_eq!(lowering.lower_qualified_id(&db, &qname), db.boolean());
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn lower_type_ref_id_covers_resolver_free_branches() {
        let db = InMemoryDb::new();
        let lowering = ctx();

        let name = |s: &str| TypeRef::Name(QualifiedName::from_segments([Name::new(s)]));
        let qual = |a: &str, b: &str| {
            TypeRef::Name(QualifiedName::from_segments([Name::new(a), Name::new(b)]))
        };

        let cases = vec![
            (TypeRef::Builtin(BuiltinTypeRef::Number), db.number(None, None)),
            (
                TypeRef::Builtin(BuiltinTypeRef::ValueTable),
                db.value_table(None, bsl_types::facet::TableSource::Unknown),
            ),
            (
                TypeRef::Array(Some(Box::new(TypeRef::Builtin(BuiltinTypeRef::String)))),
                db.array(Some(db.string(None, false))),
            ),
            (TypeRef::Array(None), db.array(None)),
            (
                TypeRef::Map(Some((
                    Box::new(TypeRef::Builtin(BuiltinTypeRef::String)),
                    Box::new(TypeRef::Builtin(BuiltinTypeRef::Number)),
                ))),
                db.map(None, None),
            ),
            (TypeRef::AnyRef, db.any_ref()),
            (TypeRef::AnyRefOf(MdoType::Catalog), db.any_metadata_ref(MdoType::Catalog)),
            (TypeRef::Unknown, db.unknown()),
            (name("Число"), db.number(None, None)),
            (name("Документы"), db.manager_collection(MdoType::Document)),
            (name("СправочникСсылка"), db.any()),
            (name("Запрос"), db.platform_object("Запрос".to_string())),
            (
                qual("СправочникСсылка", "Товары"),
                db.metadata_ref(MetadataKind::CatalogRef, "Товары".to_string(), &RootConfigCtx),
            ),
            (qual("ОбщийМодуль", "Х"), db.unknown()),
            (qual("ОпределяемыйТип", "ДенежнаяСумма"), db.unknown()),
            (
                TypeRef::Name(QualifiedName::from_segments([
                    Name::new("Документы"),
                    Name::new("ПКО"),
                    Name::new("Создать"),
                ])),
                db.unknown(),
            ),
            (
                TypeRef::Union(vec![
                    TypeRef::Builtin(BuiltinTypeRef::Number),
                    TypeRef::Builtin(BuiltinTypeRef::String),
                ]),
                db.union(vec![db.number(None, None), db.string(None, false)]),
            ),
            (
                TypeRef::Union(vec![TypeRef::Builtin(BuiltinTypeRef::Number), TypeRef::Unknown]),
                db.union(vec![db.number(None, None), db.unknown()]),
            ),
            (
                TypeRef::Union(vec![TypeRef::Unknown, TypeRef::AnyRef]),
                db.union(vec![db.unknown(), db.any_ref()]),
            ),
            (
                TypeRef::Union(vec![
                    TypeRef::Builtin(BuiltinTypeRef::Boolean),
                    TypeRef::Builtin(BuiltinTypeRef::Boolean),
                ]),
                db.boolean(),
            ),
        ];

        for (tr, expected) in &cases {
            assert_eq!(lowering.lower_type_ref_id(&db, tr), *expected, "lowering drift for {tr:?}");
        }
    }

    #[test]
    fn lower_qualified_id_resolves_defined_types() {
        let db = InMemoryDb::new();
        let resolver = MockResolver::with(&[
            ("ДенежнаяСумма", AttributeType::Number { precision: 15, scale: 2 }),
            ("A", AttributeType::DefinedType { name: "B".to_string() }),
            ("B", AttributeType::String { length: Some(64) }),
            (
                "ЛюбоеЧислоИлиСтрока",
                AttributeType::Composite {
                    types: vec![
                        AttributeType::Number { precision: 10, scale: 0 },
                        AttributeType::String { length: None },
                    ],
                },
            ),
        ]);
        let lowering = TyLoweringContext::with_resolver(&resolver);

        let number_qname = QualifiedName::from_segments([
            Name::new("ОпределяемыйТип"),
            Name::new("ДенежнаяСумма"),
        ]);
        assert_eq!(lowering.lower_qualified_id(&db, &number_qname), db.number(None, None));

        let chained_qname =
            QualifiedName::from_segments([Name::new("ОпределяемыйТип"), Name::new("A")]);
        assert_eq!(lowering.lower_qualified_id(&db, &chained_qname), db.string(None, false));

        let union_qname = QualifiedName::from_segments([
            Name::new("ОпределяемыйТип"),
            Name::new("ЛюбоеЧислоИлиСтрока"),
        ]);
        assert_eq!(
            lowering.lower_qualified_id(&db, &union_qname),
            db.union(vec![db.number(None, None), db.string(None, false)])
        );
    }
}
