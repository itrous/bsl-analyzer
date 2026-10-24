use bsl_metadata::loader::{
    load_from_directory, parse_common_module_from_text, parse_event_subscription_from_text,
    parse_register_from_text, parse_scheduled_job_from_text,
};
use bsl_metadata::traits::{MdObject, Module};
use bsl_metadata::xml_parser::{
    parse_common_module_xml, parse_event_subscription_xml, parse_information_register_xml,
    parse_scheduled_job_xml,
};
use bsl_metadata::{CommonModule, Configuration, MdoType, ObjectBelonging, RegisterPeriodicity};
use serde_json::json;
use uuid::Uuid;

const MODULE: &str = include_str!("../../../fixtures/clean_room/CommonModule.xml");
const SUBSCRIPTION: &str = include_str!("../../../fixtures/clean_room/EventSubscription.xml");
const JOB: &str = include_str!("../../../fixtures/clean_room/ScheduledJob.xml");
const REGISTER: &str = include_str!("../../../fixtures/clean_room/InformationRegister.xml");
const ROOT: &str = include_str!("../../../fixtures/clean_room/Configuration.xml");
const MODULE_ID: &str = "15300000-0000-4000-8000-000000000001";
const BASE_ID: &str = "15300000-0000-4000-8000-000000000002";
const BOOLS: [(&str, &str); 7] = [
    ("Server", "server"),
    ("Global", "global"),
    ("ClientManagedApplication", "clientManagedApplication"),
    ("ClientOrdinaryApplication", "clientOrdinaryApplication"),
    ("ExternalConnection", "externalConnection"),
    ("ServerCall", "serverCall"),
    ("Privileged", "privileged"),
];

fn property(xml: &str, tag: &str, old: &str, value: Option<&str>) -> String {
    let before = format!("<{tag}>{old}</{tag}>");
    assert_eq!(xml.matches(&before).count(), 1, "fixture property {tag}");
    let after = value.map(|s| format!("<{tag}>{s}</{tag}>")).unwrap_or_default();
    xml.replace(&before, &after)
}

fn bools(module: &CommonModule) -> [bool; 7] {
    [
        module.is_server(),
        module.is_global(),
        module.is_client_managed_application(),
        module.is_client_ordinary_application(),
        module.is_external_connection(),
        module.is_server_call(),
        module.is_privileged(),
    ]
}

#[test]
fn ownership_tokens_and_invalid_base_reference_keep_the_preserved_fallbacks() {
    for (value, expected) in [
        (Some("Adopted"), ObjectBelonging::Adopted),
        (Some("Own"), ObjectBelonging::Own),
        (None, ObjectBelonging::Unknown),
        (Some(""), ObjectBelonging::Unknown),
        (Some("FutureOwnership"), ObjectBelonging::Unknown),
    ] {
        let xml = property(MODULE, "ObjectBelonging", "Adopted", value);
        let parsed = parse_common_module_xml(&xml).unwrap();
        assert_eq!(parsed.object_belonging(), expected, "{value:?}");
        assert_eq!(parsed.uuid(), &Uuid::parse_str(MODULE_ID).unwrap());
        assert_eq!(parsed.name(), "ЁжModule");
        assert_eq!(parsed.extends_uuid(), Some(&Uuid::parse_str(BASE_ID).unwrap()));
        assert_eq!(parse_common_module_from_text(&xml).unwrap(), parsed);
    }
    for value in [None, Some(""), Some("broken-uuid")] {
        let xml = property(MODULE, "ExtendedConfigurationObject", BASE_ID, value);
        let parsed = parse_common_module_xml(&xml).unwrap();
        assert_eq!(parsed.object_belonging(), ObjectBelonging::Adopted);
        assert!(parsed.extends_uuid().is_none());
        assert_eq!(parse_common_module_from_text(&xml).unwrap(), parsed);
    }
    assert_eq!(CommonModule::builder().build().object_belonging(), ObjectBelonging::Own);
}

#[test]
fn every_xml_boolean_preserves_presence_through_serde_and_overlay() {
    let all_true = parse_common_module_xml(MODULE).unwrap();
    assert_eq!(bools(&all_true), [true; 7]);
    for (index, (tag, key)) in BOOLS.into_iter().enumerate() {
        for (value, expected) in [
            (None, None),
            (Some("true"), Some(true)),
            (Some("false"), Some(false)),
            (Some(""), Some(false)),
            (Some("not-a-bool"), Some(false)),
        ] {
            let xml = property(MODULE, tag, "true", value);
            let parsed = parse_common_module_xml(&xml).unwrap();
            let wire = serde_json::to_value(&parsed).unwrap();
            assert_eq!(wire.get(key).cloned(), expected.map(|v| json!(v)), "{tag}: {value:?}");
            let restored: CommonModule = serde_json::from_value(wire).unwrap();
            assert_eq!(restored, parsed);
            assert_eq!(parse_common_module_from_text(&xml).unwrap(), parsed);
            let mut effective = all_true.clone();
            effective.apply_extension_overlay(&restored);
            let mut wanted = [true; 7];
            wanted[index] = expected.unwrap_or(true);
            assert_eq!(bools(&effective), wanted, "{tag}: {value:?}");
        }
    }
}

#[test]
fn reuse_values_include_observed_empty_and_distinguish_omission() {
    for (value, expected) in [
        (None, None),
        (Some("DontUse"), Some("DontUse")),
        (Some("DuringRequest"), Some("DuringRequest")),
        (Some("DuringSession"), Some("DuringSession")),
        (Some(""), Some("Unknown")),
        (Some("FutureReuse"), Some("Unknown")),
    ] {
        let xml = property(MODULE, "ReturnValuesReuse", "DuringSession", value);
        let parsed = parse_common_module_xml(&xml).unwrap();
        let wire = serde_json::to_value(&parsed).unwrap();
        assert_eq!(wire.get("returnValuesReuse").and_then(|v| v.as_str()), expected);
        let restored: CommonModule = serde_json::from_value(wire).unwrap();
        let mut base = parse_common_module_xml(MODULE).unwrap();
        base.apply_extension_overlay(&restored);
        assert_eq!(
            serde_json::to_value(base).unwrap()["returnValuesReuse"],
            expected.unwrap_or("DuringSession")
        );
    }
}

#[test]
fn removed_support_key_does_not_change_other_wire_fields_or_old_json_reading() {
    let module = parse_common_module_xml(MODULE).unwrap();
    let expected = json!({
        "uuid": MODULE_ID, "name": "ЁжModule", "objectBelonging": "Adopted",
        "extendedConfigurationObject": BASE_ID, "protected": false,
        "server": true, "global": true, "clientManagedApplication": true,
        "clientOrdinaryApplication": true, "externalConnection": true,
        "serverCall": true, "privileged": true, "returnValuesReuse": "DuringSession"
    });
    assert_eq!(serde_json::to_value(&module).unwrap(), expected);
    let mut legacy = expected;
    legacy["supportVariant"] = json!("Unknown");
    let restored: CommonModule = serde_json::from_value(legacy).unwrap();
    assert_eq!(restored, module);
    assert!(serde_json::to_value(restored).unwrap().get("supportVariant").is_none());
}

#[test]
fn xml_handlers_keep_full_incomplete_and_invalid_references_in_both_contexts() {
    for (value, expected) in [
        (Some("CommonModule.ЁжModule.Обработать"), Some(("ЁжModule", "Обработать"))),
        (Some("CommonModule.ЁжModule"), Some(("ЁжModule", ""))),
        (Some("Other.ЁжModule.Обработать"), None),
        (Some("CommonModule"), None),
        (Some(""), None),
        (None, None),
    ] {
        let sub_xml = property(SUBSCRIPTION, "Handler", "CommonModule.ЁжModule.Обработать", value);
        let job_xml = property(JOB, "MethodName", "CommonModule.ЁжModule.Обработать", value);
        let sub = parse_event_subscription_xml(&sub_xml).unwrap();
        let job = parse_scheduled_job_xml(&job_xml).unwrap();
        assert_eq!(sub.name(), "ПередЗаписью");
        assert_eq!(sub.event(), "BeforeWrite");
        assert_eq!(sub.source(), "cfg:DocumentObject.Запись;cfg:CatalogObject");
        assert_eq!(sub.handler_string(), value.unwrap_or(""));
        assert_eq!(job.name(), "Обновление");
        assert_eq!(job.method_name(), value.unwrap_or(""));
        let sub_handler = sub.parse_handler();
        let job_handler = job.parse_handler();
        assert_eq!(
            sub_handler.as_ref().map(|h| (h.module_name.as_str(), h.method_name.as_str())),
            expected
        );
        assert_eq!(
            job_handler.as_ref().map(|h| (h.module_name.as_str(), h.method_name.as_str())),
            expected
        );
        assert_eq!(parse_event_subscription_from_text(&sub_xml).unwrap(), sub);
        assert_eq!(parse_scheduled_job_from_text(&job_xml).unwrap(), job);
    }
}

#[test]
fn scheduled_job_boolean_fallbacks_are_not_constructor_defaults() {
    let enabled = parse_scheduled_job_xml(JOB).unwrap();
    assert!(enabled.is_enabled());
    assert!(enabled.is_predefined());
    for value in [None, Some("false"), Some(""), Some("invalid")] {
        let xml = property(&property(JOB, "Use", "true", value), "Predefined", "true", value);
        let job = parse_scheduled_job_xml(&xml).unwrap();
        assert!(!job.is_enabled(), "{value:?}");
        assert!(!job.is_predefined(), "{value:?}");
        assert_eq!(parse_scheduled_job_from_text(&xml).unwrap(), job);
    }
}

#[test]
fn observed_periodicities_keep_unknown_year_and_quarter_unsupported() {
    for (value, expected) in [
        (Some("Nonperiodical"), Some(RegisterPeriodicity::Nonperiodical)),
        (Some("Second"), Some(RegisterPeriodicity::Second)),
        (Some("Day"), Some(RegisterPeriodicity::Day)),
        (Some("Month"), Some(RegisterPeriodicity::Month)),
        (Some("RecorderPosition"), Some(RegisterPeriodicity::RecorderPosition)),
        (Some("Year"), None),
        (Some("Quarter"), None),
        (Some("FuturePeriod"), None),
        (Some(""), None),
        (None, None),
    ] {
        let xml = property(REGISTER, "InformationRegisterPeriodicity", "Day", value);
        let register = parse_information_register_xml(&xml).unwrap();
        assert_eq!(register.name(), "Измерения");
        assert_eq!(register.periodicity(), expected, "{value:?}");
        assert!(register.is_recorder_subordinate());
        assert!(register.enable_totals_slice_first());
        assert!(!register.enable_totals_slice_last());
        let slices = expected.is_some_and(|p| p != RegisterPeriodicity::Nonperiodical);
        assert_eq!(
            register.virtual_tables(),
            if slices {
                vec!["СрезПервых", "СрезПоследних"]
            } else {
                vec![]
            }
        );
        assert_eq!(parse_register_from_text(MdoType::InformationRegister, &xml).unwrap(), register);
    }
}

#[test]
fn both_totals_slice_flags_have_positive_and_negative_xml_controls() {
    for (tag, old) in [("EnableTotalsSliceFirst", "true"), ("EnableTotalsSliceLast", "false")] {
        for (value, expected) in [
            (Some("true"), true),
            (Some("false"), false),
            (None, false),
            (Some(""), false),
            (Some("invalid"), false),
        ] {
            let xml = property(REGISTER, tag, old, value);
            let register = parse_information_register_xml(&xml).unwrap();
            let flags = [register.enable_totals_slice_first(), register.enable_totals_slice_last()];
            let wanted =
                if tag == "EnableTotalsSliceFirst" { [expected, false] } else { [true, expected] };
            assert_eq!(flags, wanted, "{tag}: {value:?}");
            assert_eq!(
                parse_register_from_text(MdoType::InformationRegister, &xml).unwrap(),
                register
            );
        }
    }
}

#[test]
fn invalid_identity_errors_in_raw_parsers_are_skips_in_text_adapters() {
    for uuid in ["", "not-a-uuid"] {
        let module = MODULE.replace(MODULE_ID, uuid);
        assert!(parse_common_module_xml(&module).is_err());
        assert!(parse_common_module_from_text(&module).is_none());
        let sub = SUBSCRIPTION.replace("15300000-0000-4000-8000-000000000003", uuid);
        assert!(parse_event_subscription_xml(&sub).is_err());
        assert!(parse_event_subscription_from_text(&sub).is_none());
        let job = JOB.replace("15300000-0000-4000-8000-000000000004", uuid);
        assert!(parse_scheduled_job_xml(&job).is_err());
        assert!(parse_scheduled_job_from_text(&job).is_none());
        let register = REGISTER.replace("15300000-0000-4000-8000-000000000005", uuid);
        assert!(parse_information_register_xml(&register).is_err());
        assert!(parse_register_from_text(MdoType::InformationRegister, &register).is_none());
    }
}

#[test]
fn duplicate_name_and_uri_indexes_keep_distinct_winners_after_add_and_rebuild() {
    let first_id = Uuid::parse_str(MODULE_ID).unwrap();
    let last_id = Uuid::parse_str(BASE_ID).unwrap();
    let uri = "CommonModules/ЁжModule/Ext/Module.bsl";
    let mut config = Configuration::new("Duplicates");
    for (id, name) in [(first_id, "ЁжModule"), (last_id, "ёЖmODULE")] {
        config
            .add_common_module(CommonModule::builder().uuid(id).name(name).uri(Some(uri)).build());
    }
    for (kind, name, id) in [
        (MdoType::Catalog, "ЁжModule", first_id),
        (MdoType::Catalog, "ёЖmODULE", last_id),
        (MdoType::Document, "ЁжModule", last_id),
    ] {
        let mut object = bsl_metadata::MetadataObject::new(kind, name);
        object.set_uuid(id);
        config.add_metadata_object(object);
    }
    for rebuilt in [false, true] {
        if rebuilt {
            let raw = config.common_attribute_set().clone();
            config.merge_extension_overlay(
                &Configuration::new("Empty"),
                Some(bsl_metadata::ExtensionScope::InfoBase),
                Some(&raw),
            );
        }
        assert_eq!(config.find_common_module("ёЖmODULE").unwrap().uuid(), &last_id);
        assert_eq!(config.find_module_by_uri(uri).unwrap().uuid(), &last_id);
        assert_eq!(config.find_child_by_uri(uri).unwrap().uuid(), &last_id);
        assert_eq!(
            config
                .find_common_module_by_uri_lower("commonmodules/ёжmodule/ext/module.bsl")
                .unwrap()
                .uuid(),
            &first_id
        );
        assert_eq!(
            config.find_metadata_object(MdoType::Catalog, "ёЖmODULE").unwrap().uuid,
            Some(first_id)
        );
        assert_eq!(
            config.find_metadata_object(MdoType::Document, "ёЖmODULE").unwrap().uuid,
            Some(last_id)
        );
        assert_eq!(
            config.common_modules().iter().map(|m| *m.uuid()).collect::<Vec<_>>(),
            [first_id, last_id]
        );
        assert!(config.find_common_module("Missing").is_none());
        assert!(config.find_child_by_uri("Missing").is_none());
        assert!(!config.has_metadata_object(MdoType::Enum, "ЁжModule"));
    }
}

#[test]
fn root_xml_flags_and_identity_remain_distinct_from_disk_loader_defaults() {
    let raw = Configuration::from_xml_str(ROOT).unwrap();
    assert_eq!(raw.name(), "Наблюдения");
    assert_eq!(raw.uuid().to_string(), "15300000-0000-4000-8000-000000000006");
    assert!(raw.use_managed_form_in_ordinary_application());
    assert!(raw.use_ordinary_form_in_managed_application());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Configuration.xml"), ROOT).unwrap();
    std::fs::create_dir(dir.path().join("CommonModules")).unwrap();
    std::fs::write(dir.path().join("CommonModules/ЁжModule.xml"), MODULE).unwrap();
    let loaded = load_from_directory(dir.path()).unwrap();
    assert_eq!(loaded.name(), "Configuration");
    assert_eq!(loaded.uuid().get_version_num(), 4);
    assert!(!loaded.use_managed_form_in_ordinary_application());
    assert!(!loaded.use_ordinary_form_in_managed_application());
    // Full loading keys on the XML like discovery does; the directory only
    // accompanies a body.
    assert!(loaded.find_common_module("ёЖmODULE").is_some_and(|m| m.is_server()));
    std::fs::create_dir(dir.path().join("CommonModules/ЁжModule")).unwrap();
    let with_directory = load_from_directory(dir.path()).unwrap();
    let module = with_directory.find_common_module("ёЖmODULE").unwrap();
    assert!(module.is_server());
    assert_eq!(module.uuid().to_string(), MODULE_ID);
    let as_module: &dyn Module = module;
    assert_eq!(as_module.name(), "ЁжModule");
    for value in [None, Some("false"), Some("invalid")] {
        let xml = property(
            &property(ROOT, "UseManagedFormInOrdinaryApplication", "true", value),
            "UseOrdinaryFormInManagedApplication",
            "true",
            value,
        );
        let config = Configuration::from_xml_str(&xml).unwrap();
        assert!(!config.use_managed_form_in_ordinary_application());
        assert!(!config.use_ordinary_form_in_managed_application());
    }
}
