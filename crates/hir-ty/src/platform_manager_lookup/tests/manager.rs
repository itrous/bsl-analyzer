#[test]
fn platform_manager_typeid_round_trips_via_ty() {
    let db = InMemoryDb::new();
    let res = PlatformMethodResolution {
        signature: FunctionSignature {
            params: Box::new([]),
            defaults: Box::new([]),
            ret: db.number(None, None),
            max_args: Some(0),
            from_doc_comment: false,
            doc_see: Default::default(),
        },
        return_ty: db.number(None, None),
        overloads: vec![vec![db.string(None, false)]],
        env: hir_def::execution_env::EnvFlags::ALL,
        candidates: CallCandidateSet::try_from(Vec::new())
            .expect("an empty candidate set has no duplicate identities"),
        records: Vec::new(),
    };
    assert_eq!(res.return_ty, db.number(None, None));
    assert_eq!(res.overloads, vec![vec![db.string(None, false)]]);
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_create_item_on_catalog_returns_catalog_object() {
    let db = InMemoryDb::new();
    let res = resolve_platform_manager_method(
        &db,
        MdoType::Catalog,
        &Name::new("Номенклатура"),
        &Name::new("СоздатьЭлемент"),
    )
    .expect("platform data indexes CreateItem under CatalogManager");

    assert_eq!(
        res.return_ty,
        db.metadata_ref(
            MetadataKind::CatalogObject,
            "Номенклатура".to_string(),
            &RootConfigCtx
        )
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_find_by_code_on_catalog_returns_catalog_ref() {
    let db = InMemoryDb::new();
    let res = resolve_platform_manager_method(
        &db,
        MdoType::Catalog,
        &Name::new("Валюты"),
        &Name::new("НайтиПоКоду"),
    )
    .expect("platform data indexes FindByCode under CatalogManager");

    assert_eq!(
        res.return_ty,
        db.metadata_ref(MetadataKind::CatalogRef, "Валюты".to_string(), &RootConfigCtx)
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_find_by_code_param_lowers_to_union() {
    let db = InMemoryDb::new();
    let res = resolve_platform_manager_method(
        &db,
        MdoType::Catalog,
        &Name::new("Валюты"),
        &Name::new("НайтиПоКоду"),
    )
    .expect("platform data indexes FindByCode under CatalogManager");

    assert_eq!(
        res.signature.params.first(),
        Some(&db.union(vec![db.number(None, None), db.string(None, false)])),
        "first param of FindByCode must be a Union, not a single PlatformObject; got {:?}",
        res.signature.params.first(),
    );
}

#[test]
fn manager_unknown_method_returns_none() {
    let db = InMemoryDb::new();
    assert!(resolve_platform_manager_method(
        &db,
        MdoType::Catalog,
        &Name::new("Валюты"),
        &Name::new("НетТакогоМетода"),
    )
    .is_none());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_english_method_name_resolves() {
    let db = InMemoryDb::new();
    let res = resolve_platform_manager_method(
        &db,
        MdoType::Catalog,
        &Name::new("Номенклатура"),
        &Name::new("CreateItem"),
    )
    .expect("English 'CreateItem' must also resolve to CatalogManager.CreateItem");
    match db.lookup_type(res.return_ty) {
        TypeKind::MetadataRef(facet) => assert_eq!(facet.kind, MetadataKind::CatalogObject),
        other => panic!("expected MetadataRef{{CatalogObject}}, got {other:?}"),
    }
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_plural_lookup_exposes_all_platform_records_and_variants() {
    let db = InMemoryDb::new();
    let expected = bsl_platform::find_prefixed_methods("InformationRegisterManager", "Select");
    let resolution = resolve_platform_manager_method(
        &db,
        MdoType::InformationRegister,
        &Name::new("Курсы"),
        &Name::new("Выбрать"),
    )
    .expect("InformationRegisterManager.Select must resolve");

    assert_eq!(
        resolution.records.iter().map(|record| record.method_id).collect::<Vec<_>>(),
        expected.iter().map(|method| method.id).collect::<Vec<_>>(),
    );
    assert_eq!(
        resolution.records.iter().map(|record| record.overloads.len()).collect::<Vec<_>>(),
        expected.iter().map(|method| method.variants.len()).collect::<Vec<_>>(),
    );
    assert_eq!(resolution.signature, resolution.records[0].signature);
    assert_eq!(resolution.return_ty, resolution.records[0].return_ty);
    assert_eq!(resolution.overloads, resolution.records[0].overloads);
    assert_eq!(resolution.env, resolution.records[0].env);
}

#[test]
fn manager_mdo_without_prefix_returns_none() {
    let db = InMemoryDb::new();
    assert!(resolve_platform_manager_method(
        &db,
        MdoType::CommonModule,
        &Name::new("AnyName"),
        &Name::new("СоздатьЭлемент"),
    )
    .is_none());
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_create_record_set_on_information_register_returns_record_set() {
    let db = InMemoryDb::new();
    let res = resolve_platform_manager_method(
        &db,
        MdoType::InformationRegister,
        &Name::new("Курсы"),
        &Name::new("СоздатьНаборЗаписей"),
    )
    .expect("platform data indexes CreateRecordSet under InformationRegisterManager");
    assert_eq!(
        res.return_ty,
        db.metadata_ref(
            MetadataKind::InformationRegisterRecordSet,
            "Курсы".to_string(),
            &RootConfigCtx,
        )
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_create_record_set_on_accumulation_register_returns_record_set() {
    let db = InMemoryDb::new();
    let res = resolve_platform_manager_method(
        &db,
        MdoType::AccumulationRegister,
        &Name::new("ПродажиОбороты"),
        &Name::new("СоздатьНаборЗаписей"),
    )
    .expect("platform data indexes CreateRecordSet under AccumulationRegisterManager");
    assert_eq!(
        res.return_ty,
        db.metadata_ref(
            MetadataKind::AccumulationRegisterRecordSet,
            "ПродажиОбороты".to_string(),
            &RootConfigCtx,
        )
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_create_record_set_on_accounting_register_returns_record_set() {
    let db = InMemoryDb::new();
    let res = resolve_platform_manager_method(
        &db,
        MdoType::AccountingRegister,
        &Name::new("Хозрасчетный"),
        &Name::new("СоздатьНаборЗаписей"),
    )
    .expect("platform data indexes CreateRecordSet under AccountingRegisterManager");
    assert_eq!(
        res.return_ty,
        db.metadata_ref(
            MetadataKind::AccountingRegisterRecordSet,
            "Хозрасчетный".to_string(),
            &RootConfigCtx,
        )
    );
}

#[test]
#[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
fn manager_create_record_set_on_calculation_register_returns_record_set() {
    let db = InMemoryDb::new();
    let res = resolve_platform_manager_method(
        &db,
        MdoType::CalculationRegister,
        &Name::new("Начисления"),
        &Name::new("СоздатьНаборЗаписей"),
    )
    .expect("platform data indexes CreateRecordSet under CalculationRegisterManager");
    assert_eq!(
        res.return_ty,
        db.metadata_ref(
            MetadataKind::CalculationRegisterRecordSet,
            "Начисления".to_string(),
            &RootConfigCtx,
        )
    );
}
