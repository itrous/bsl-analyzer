use super::*;
use bsl_metadata::read_properties::ObjectReadProperties;
use serde_json::{json, Value};

pub(super) fn append_read_properties(
    out: &mut String,
    properties: &ObjectReadProperties,
    kind: MdoType,
) {
    fn localized(
        out: &mut String,
        path: &str,
        synonym: &Option<bsl_metadata::read_properties::LocalizedString>,
        complete: Option<bool>,
    ) {
        match synonym {
            None => {
                let _ = writeln!(out, "- Синоним {path}: unknown");
            }
            Some(values) if values.is_empty() => {
                let _ = writeln!(
                    out,
                    "- Синоним {path}: {}",
                    if complete == Some(true) { "пустой" } else { "unknown" }
                );
            }
            Some(values) => {
                for (language, value) in values {
                    let _ =
                        writeln!(out, "- Синоним {path} [{language}]: {}", escape_markdown(value));
                }
            }
        }
        if synonym.is_some() && complete != Some(true) {
            let _ = writeln!(out, "- Языки синонима {path}: partial");
        }
    }
    localized(out, "объекта", &properties.synonym, properties.synonym_complete);
    for member in &properties.members {
        let path = escape_markdown(&member.path.join("."));
        localized(out, &path, &member.synonym, member.synonym_complete);
        if let Some(value) = &member.use_mode {
            let _ = writeln!(out, "- Use {path}: {}", escape_markdown(value));
        }
        if let Some(value) = &member.fill_value {
            let _ = writeln!(out, "- FillValue {path}: {}", json!(value));
        }
        let _=writeln!(out,"- ChoiceParameters {path}: {}\n- ChoiceParameterLinks {path}: {}\n- FillChecking {path}: {}",member.choice.choice_parameters.as_ref().map(|value|json!(value).to_string()).as_deref().unwrap_or("unknown"),member.choice.choice_parameter_links.as_ref().map(|value|json!(value).to_string()).as_deref().unwrap_or("unknown"),member.choice.fill_checking.as_deref().unwrap_or("unknown"));
    }
    if kind == MdoType::Catalog {
        let _ = writeln!(
            out,
            "- Hierarchical: {}",
            properties.hierarchical.map(|v| v.to_string()).as_deref().unwrap_or("unknown")
        );
        for (name, value) in [
            ("HierarchyType", &properties.hierarchy_type),
            ("CodeType", &properties.code_type),
            ("CodeAllowedLength", &properties.code_allowed_length),
        ] {
            let _ = writeln!(out, "- {name}: {}", value.as_deref().unwrap_or("unknown"));
        }
        let _ = writeln!(
            out,
            "- CodeLength: {}",
            properties.code_length.map(|v| v.to_string()).as_deref().unwrap_or("unknown")
        );
    }
    if kind == MdoType::InformationRegister {
        let _ = writeln!(
            out,
            "- InformationRegisterPeriodicity: {}\n- WriteMode: {}",
            properties.periodicity.as_deref().unwrap_or("unknown"),
            properties.write_mode.as_deref().unwrap_or("unknown")
        );
    }
    if kind == MdoType::ChartOfCharacteristicTypes {
        let _ = writeln!(
            out,
            "- Тип значения характеристик: {}",
            properties
                .characteristic_value_type
                .as_ref()
                .map(format_source_type)
                .as_deref()
                .unwrap_or("unknown")
        );
    }
    if kind == MdoType::ExchangePlan {
        match &properties.exchange_plan_content {
            Some(items) => {
                let _ = writeln!(out, "Состав плана обмена ({}):", items.len());
                for item in items {
                    let _ = writeln!(
                        out,
                        "- {}: AutoRecord={}",
                        escape_markdown(&item.metadata),
                        item.auto_record.as_deref().unwrap_or("unknown")
                    );
                }
            }
            None => {
                let _ = writeln!(out, "Состав плана обмена: unknown");
            }
        }
    }
    if kind == MdoType::ChartOfAccounts {
        match &properties.predefined_accounts {
            Some(accounts) => {
                for account in accounts {
                    let _ = writeln!(out,"Предопределённый счёт {}: Code={}, Parent={}, AccountType={}, OffBalance={}",escape_markdown(&account.name),account.code.as_deref().unwrap_or("unknown"),account.parent.as_deref().unwrap_or("корневой"),account.account_type.as_deref().unwrap_or("unknown"),account.off_balance.map(|value|value.to_string()).as_deref().unwrap_or("unknown"));
                    let _ = writeln!(
                        out,
                        "AccountingFlags: {}\nExtDimensionTypes: {}",
                        json!(account.accounting_flags),
                        json!(account.ext_dimension_types)
                    );
                }
            }
            None => {
                let _ = writeln!(out, "Предопределённые счета: unknown");
            }
        }
    }
    out.push('\n');
}

pub(super) fn properties_reasons(properties: &ObjectReadProperties, kind: MdoType) -> Vec<Value> {
    let mut reasons = Vec::new();
    let mut missing = |path: &str| {
        reasons.push(
            json!({"code":"property_unavailable","path":format!("/object/read_properties/{path}")}),
        )
    };
    if properties.synonym.is_none() {
        missing("synonym");
    }
    if properties.synonym.is_some() && properties.synonym_complete != Some(true) {
        missing("synonym_complete");
    }
    if kind == MdoType::Catalog {
        for (field, present) in [
            ("hierarchical", properties.hierarchical.is_some()),
            ("hierarchy_type", properties.hierarchy_type.is_some()),
            ("code_type", properties.code_type.is_some()),
            ("code_length", properties.code_length.is_some()),
            ("code_allowed_length", properties.code_allowed_length.is_some()),
        ] {
            if !present {
                missing(field);
            }
        }
    }
    if kind == MdoType::InformationRegister {
        if properties.periodicity.is_none() {
            missing("periodicity");
        }
        if properties.write_mode.is_none() {
            missing("write_mode");
        }
    }
    if kind == MdoType::ChartOfCharacteristicTypes && properties.characteristic_value_type.is_none()
    {
        missing("characteristic_value_type");
    }
    if kind == MdoType::ExchangePlan && !properties.exchange_plan_content_complete {
        missing("exchange_plan_content");
    }
    if kind == MdoType::ChartOfAccounts && properties.predefined_accounts.is_none() {
        missing("predefined_accounts");
    }
    if let Some(items) = &properties.exchange_plan_content {
        for (index, item) in items.iter().enumerate() {
            if item.auto_record.is_none() {
                missing(&format!("exchange_plan_content/{index}/auto_record"));
            }
        }
    }
    for (index, member) in properties.members.iter().enumerate() {
        if member.synonym.is_none() {
            missing(&format!("members/{index}/synonym"));
        }
        if member.synonym.is_some() && member.synonym_complete != Some(true) {
            missing(&format!("members/{index}/synonym_complete"));
        }
        if kind == MdoType::Catalog
            && member
                .path
                .first()
                .is_some_and(|kind| matches!(kind.as_str(), "Attribute" | "TabularSection"))
            && member.use_mode.is_none()
        {
            missing(&format!("members/{index}/use_mode"));
        }
        if member.path.first().is_some_and(|kind| kind == "Resource") && member.fill_value.is_none()
        {
            missing(&format!("members/{index}/fill_value"));
        }
    }
    if let Some(accounts) = &properties.predefined_accounts {
        for (index, account) in accounts.iter().enumerate() {
            reasons.extend(predefined_account_reasons(
                &json!(account),
                &format!("/object/read_properties/predefined_accounts/{index}"),
                false,
            ));
        }
    }
    reasons
}

pub(super) fn characteristic_type_response(
    object: &bsl_metadata::MetadataObject,
    resolver: &dyn bsl_metadata::QueryMetadataResolver,
    roots: Option<Vec<String>>,
) -> Result<CallToolResult, McpError> {
    let response = source_metadata_object_response(object)?;
    let mut text = response.content[0].as_text().unwrap().text.clone();
    let mut body = response.structured_content.unwrap();
    let mut reasons = Vec::new();
    let resolved = object.read_properties.characteristic_value_type.as_ref().map(|ty| {
        use bsl_metadata::metadata_resolver::{resolve_type_description, TypeResolutionIssueKind};
        let (resolved, issues) = resolve_type_description(
            ty,
            resolver,
            crate::tools::response::DEFAULT_OUTPUT_BUDGET_TOKENS,
        );
        for issue in issues {
            let path = if matches!(resolved, bsl_metadata::AttributeType::Composite { .. }) {
                format!(
                    "/object/resolved_characteristic_value_type/Composite/types/{}",
                    issue.variant_index
                )
            } else {
                "/object/resolved_characteristic_value_type".into()
            };
            let code = match issue.kind {
                TypeResolutionIssueKind::UnresolvedDefinedType => "unresolved_defined_type",
                TypeResolutionIssueKind::ReferencedTypeUnavailable => "referenced_type_unavailable",
                TypeResolutionIssueKind::TechnicalNameUnavailable => "technical_name_unavailable",
                TypeResolutionIssueKind::LimitExceeded => "output_budget",
            };
            reasons.push(json!({"code":code,"path":path}));
        }
        resolved
    });
    let _ = writeln!(
        text,
        "- Разрешённый тип значения характеристик: {}",
        resolved.as_ref().map(format_source_type).as_deref().unwrap_or("unknown")
    );
    for reason in &reasons {
        let _ = writeln!(
            text,
            "- {}: {}",
            reason["path"].as_str().unwrap(),
            reason["code"].as_str().unwrap()
        );
    }
    body["object"]["resolved_characteristic_value_type"] = json!(resolved);
    if let Some(roots) = roots {
        body["source_roots"] = json!(roots);
    }
    if !reasons.is_empty() {
        body["completeness"]["status"] = json!("partial");
        body["completeness"]["reasons"].as_array_mut().unwrap().extend(reasons);
    }
    Ok(crate::tools::response::structured_with_text(text, body))
}

pub(super) fn source_metadata_object_response(
    object: &bsl_metadata::MetadataObject,
) -> Result<CallToolResult, McpError> {
    let mut reasons = properties_reasons(&object.read_properties, object.mdo_type);
    if object.mdo_type == MdoType::ChartOfAccounts && object.predefined_unread {
        reasons.push(json!({
            "code": "predefined_set_incomplete",
            "path": "/object/read_properties/predefined_accounts",
        }));
    }
    for (index, attribute) in object.attributes.iter().enumerate() {
        let mut unresolved = Vec::new();
        collect_type_reasons(
            &attribute.attr_type,
            &format!("/object/attributes/{index}/attr_type"),
            &mut unresolved,
        );
        reasons.extend(unresolved.into_iter().map(|(path, code)| json!({"code":code,"path":path})));
    }
    for (section_index, section) in object.tabular_sections.iter().enumerate() {
        for (index, attribute) in section.attributes().iter().enumerate() {
            let mut unresolved = Vec::new();
            collect_type_reasons(
                attribute.attr_type(),
                &format!("/object/tabular_sections/{section_index}/attributes/{index}/attr_type"),
                &mut unresolved,
            );
            reasons.extend(
                unresolved.into_iter().map(|(path, code)| json!({"code":code,"path":path})),
            );
        }
    }
    if let Some(ty) = &object.read_properties.characteristic_value_type {
        let mut unresolved = Vec::new();
        collect_type_reasons(
            ty,
            "/object/read_properties/characteristic_value_type",
            &mut unresolved,
        );
        reasons.extend(unresolved.into_iter().map(|(path, code)| json!({"code":code,"path":path})));
    }
    Ok(source_card(
        format_metadata_object_structure(object, object.mdo_type),
        json!({"name":object.name,"mdo_type":object.mdo_type,"uuid":object.uuid,"object_belonging":object.object_belonging,"extended_configuration_object":object.extended_configuration_object,"read_properties":object.read_properties,"attributes":object.attributes,"tabular_sections":object.tabular_sections,"enum_values":object.enum_values,"predefined_items":object.predefined_items,"predefined_unread":object.predefined_unread}),
        reasons,
    ))
}

pub(super) fn source_register_response(
    register: &bsl_metadata::Register,
) -> Result<CallToolResult, McpError> {
    let mut reasons = properties_reasons(register.read_properties(), register.mdo_type());
    let mut object = json!({"name":register.name(),"mdo_type":register.mdo_type(),"uuid":register.uuid(),"read_properties":register.read_properties(),"dimensions":[],"resources":[],"attributes":[]});
    for (category, name, ty, raw) in register
        .dimensions()
        .iter()
        .map(|v| ("dimensions", v.name(), v.attr_type(), v.type_str()))
        .chain(
            register
                .resources()
                .iter()
                .map(|v| ("resources", v.name(), v.attr_type(), v.type_str())),
        )
        .chain(
            register
                .attributes()
                .iter()
                .map(|v| ("attributes", v.name(), v.attr_type(), v.type_str())),
        )
    {
        let ty = register_type(ty, raw);
        let mut unresolved = Vec::new();
        let fields = object[category].as_array_mut().expect("register collection is an array");
        collect_type_reasons(
            &ty,
            &format!("/object/{category}/{}/type", fields.len()),
            &mut unresolved,
        );
        reasons.extend(unresolved.into_iter().map(|(path, code)| json!({"code":code,"path":path})));
        fields.push(json!({"name":name,"type":ty}));
    }
    Ok(source_card(format_register_structure(register), object, reasons))
}

fn source_card(text: String, object: Value, reasons: Vec<Value>) -> CallToolResult {
    crate::tools::response::structured_with_text(
        text,
        json!({
            "schema_version":"2","source":"source","object":object,
            "completeness":{"status":if reasons.is_empty() {"complete"} else {"partial"},"reasons":reasons},"truncated":false
        }),
    )
}

/// Enforce the combined wire budget. An omitted card stays explicitly unavailable.
pub fn bound_source_response(
    mut result: CallToolResult,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let ceiling = max_output_tokens.saturating_mul(4);
    if live_result_payload_bytes(&result) <= ceiling {
        return Ok(result);
    }
    if let Some(body) = result.structured_content.as_mut() {
        if let Some(object) = body.get("object") {
            let identity = object.get("name").cloned().unwrap_or(Value::Null);
            body["object"] = Value::Null;
            body["name"] = identity;
            body["completeness"] =
                json!({"status":"partial","reasons":[{"code":"output_budget","path":"/object"}]});
            body["truncated"] = json!(true);
            result.content = vec![ContentBlock::text(
                "Карточка не помещается в max_output_tokens; свойства не выданы. Увеличьте бюджет.",
            )];
        }
    } else {
        let mut text = String::new();
        for content in &result.content {
            if let Some(value) = content.as_text() {
                text.push_str(&value.text);
            }
        }
        crate::tools::response::truncate_text_to_budget(
            &mut text,
            max_output_tokens / 2,
            "\n-- усечено под max_output_tokens --",
        );
        result.content = vec![ContentBlock::text(text)];
    }
    if live_result_payload_bytes(&result) <= ceiling {
        Ok(result)
    } else {
        Err(metadata_budget_error(live_result_payload_bytes(&result)))
    }
}

pub(super) fn is_scheduled_job(raw: &str) -> bool {
    matches!(
        raw.to_lowercase().as_str(),
        "scheduledjob" | "scheduledjobs" | "регламентноезадание" | "регламентныезадания"
    )
}

pub(super) fn scheduled_job_response(job: &bsl_metadata::ScheduledJob) -> CallToolResult {
    let handler = (!job.method_name().is_empty()).then_some(job.method_name());
    let mut reasons = Vec::new();
    for (field, missing) in [
        ("handler", handler.is_none()),
        ("schedule", job.schedule().is_none()),
        ("use", job.enabled().is_none()),
    ] {
        if missing {
            reasons.push(json!({"code":"property_unavailable","path":format!("/object/{field}")}));
        }
    }
    source_card(
        format!(
            "# РегламентноеЗадание.{}\n\nОбработчик: {}\nSchedule: {}\nUse: {}\n",
            escape_markdown(job.name()),
            handler.unwrap_or("unknown"),
            job.schedule().map(|v| json!(v).to_string()).as_deref().unwrap_or("unknown"),
            job.enabled().map(|v| v.to_string()).as_deref().unwrap_or("unknown")
        ),
        json!({"name":job.name(),"handler":handler,"schedule":job.schedule(),"use":job.enabled()}),
        reasons,
    )
}

pub(super) fn subsystem_response(subsystem: &bsl_metadata::Subsystem) -> CallToolResult {
    let mut reasons = Vec::new();
    if subsystem.resolution_incomplete {
        reasons.push(json!({"code":"source_resolution_incomplete","path":"/object/source_roots"}));
    }
    if subsystem.object_belonging == bsl_metadata::ObjectBelonging::Adopted {
        reasons.push(
            json!({"code":"unresolved_adoption","path":"/object/extended_configuration_object"}),
        );
    }
    for (field, missing) in [
        ("content", subsystem.content_refs.is_none() || subsystem.content_unread),
        ("picture", subsystem.picture.is_none()),
    ] {
        if missing {
            reasons.push(json!({"code":"property_unavailable","path":format!("/object/{field}")}));
        }
    }
    let mut text = format!(
        "# Подсистема.{}\n\nСостав: {}\nДочерние подсистемы: {}\nКартинка: {}\n",
        escape_markdown(subsystem.address()),
        json!(subsystem.content_refs),
        json!(subsystem.child_subsystems()),
        subsystem.picture.as_deref().unwrap_or("unknown")
    );
    if subsystem.resolution_incomplete
        || subsystem.object_belonging == bsl_metadata::ObjectBelonging::Adopted
    {
        text.push_str("\nЭффективный объект не подтверждён (partial); показаны известные свойства выбранных источников.\n");
    }
    source_card(
        text,
        json!({"name":subsystem.address(),"local_name":subsystem.name(),"parent":subsystem.parent,"source_roots":subsystem.source_roots,"uuid":subsystem.uuid,"object_belonging":subsystem.object_belonging,"extended_configuration_object":subsystem.extended_configuration_object,"content":subsystem.content_refs,"child_subsystems":subsystem.child_subsystems(),"picture":subsystem.picture}),
        reasons,
    )
}

pub(super) fn read_object_response(
    object: &bsl_metadata::read_object::ReadObject,
    roots: &[String],
) -> CallToolResult {
    use bsl_metadata::read_object::ReadObjectKind;
    let mut reasons = Vec::new();
    if object.resolution_incomplete {
        reasons.push(json!({"code":"source_resolution_incomplete","path":"/source_roots"}));
    }
    if object.object_belonging == bsl_metadata::ObjectBelonging::Adopted {
        reasons.push(
            json!({"code":"unresolved_adoption","path":"/object/extended_configuration_object"}),
        );
    }
    if object.synonym.is_none() {
        reasons.push(json!({"code":"property_unavailable","path":"/object/synonym"}));
    }
    if object.kind == ReadObjectKind::FunctionalOption {
        for (field, missing) in [
            ("location", object.location.is_none()),
            ("privileged_get_mode", object.privileged_get_mode.is_none()),
            ("content", object.content.is_none()),
        ] {
            if missing {
                reasons
                    .push(json!({"code":"property_unavailable","path":format!("/object/{field}")}));
            }
        }
    }
    if object.kind == ReadObjectKind::CommonCommand {
        for (field, missing) in [
            ("group", object.group.is_none()),
            ("picture", object.picture.is_none()),
            ("parameter_type", object.parameter_type.is_none()),
            ("modifies_data", object.modifies_data.is_none()),
        ] {
            if missing {
                reasons
                    .push(json!({"code":"property_unavailable","path":format!("/object/{field}")}));
            }
        }
    }
    if object.kind == ReadObjectKind::CommonPicture && object.picture.is_none() {
        reasons.push(json!({"code":"picture_content_unavailable","path":"/object/picture"}));
    }
    let mut text = format!("# {}.{}\n\nСиноним: {}\nХранение: {}\nПривилегированное получение: {}\nСостав: {}\nГруппа команды: {}\nКартинка: {}\n",object.kind.russian_name(),escape_markdown(&object.name),json!(object.synonym),object.location.as_deref().unwrap_or("unknown"),object.privileged_get_mode.map(|v|v.to_string()).as_deref().unwrap_or("unknown"),json!(object.content),object.group.as_deref().unwrap_or("unknown"),object.picture.as_deref().unwrap_or("unknown"));
    if object.resolution_incomplete
        || object.object_belonging == bsl_metadata::ObjectBelonging::Adopted
    {
        text.push_str("\nЭффективный объект не подтверждён (partial); показаны известные свойства выбранных источников.\n");
    }
    let mut result = source_card(text, json!(object), reasons);
    if let Some(body) = result.structured_content.as_mut() {
        body["source_roots"] = json!(roots);
    }
    result
}

pub(super) fn predefined_account_reasons(account: &Value, path: &str, runtime: bool) -> Vec<Value> {
    let mut reasons = Vec::new();
    let mut invalid = Vec::new();
    let mut missing = |field: &str| {
        reasons.push(json!({"code":"property_unavailable","path":format!("{path}/{field}")}));
    };
    for field in
        ["name", "code", "account_type", "off_balance", "accounting_flags", "ext_dimension_types"]
    {
        if account.get(field).is_none_or(Value::is_null) {
            missing(field);
        }
    }
    let valid_uuid = |value: &Value| {
        value
            .as_str()
            .and_then(|text| bsl_metadata::Uuid::parse_str(text).ok())
            .is_some_and(|uuid| !uuid.is_nil())
    };
    if runtime {
        for field in ["uuid", "parent_uuid"] {
            if let Some(value) = account
                .pointer(&format!("/runtime_reference/{field}"))
                .filter(|value| !value.is_null())
            {
                let contradictory_parent = field == "parent_uuid"
                    && account
                        .pointer("/runtime_reference/parent_known_empty")
                        .and_then(Value::as_bool)
                        == Some(true);
                if !valid_uuid(value) || contradictory_parent {
                    invalid.push(json!({"code":"invalid_runtime_identity","path":format!("{path}/runtime_reference/{field}")}));
                }
            }
        }
        if account.pointer("/runtime_reference/uuid").is_none_or(Value::is_null) {
            missing("runtime_reference/uuid");
        }
        match account.pointer("/runtime_reference/parent_known_empty").and_then(Value::as_bool) {
            Some(false)
                if account.pointer("/runtime_reference/parent_uuid").is_none_or(Value::is_null) =>
            {
                missing("runtime_reference/parent_uuid");
            }
            None => missing("runtime_reference/parent_known_empty"),
            _ => {}
        }
    }
    let flag_fields: &[&str] = if runtime { &["enabled"] } else { &["reference", "enabled"] };
    if let Some(flags) = account.get("accounting_flags").and_then(Value::as_array) {
        for (index, flag) in flags.iter().enumerate() {
            for field in flag_fields {
                if flag.get(field).is_none_or(Value::is_null) {
                    missing(&format!("accounting_flags/{index}/{field}"));
                }
            }
        }
    }
    if let Some(dimensions) = account.get("ext_dimension_types").and_then(Value::as_array) {
        for (index, dimension) in dimensions.iter().enumerate() {
            if runtime
                && dimension
                    .get("runtime_ref_uuid")
                    .filter(|value| !value.is_null())
                    .is_some_and(|value| !valid_uuid(value))
            {
                invalid.push(json!({"code":"invalid_runtime_identity","path":format!("{path}/ext_dimension_types/{index}/runtime_ref_uuid")}));
            }
            let fields: &[&str] = if runtime {
                &[
                    "predefined_name",
                    "runtime_ref_uuid",
                    "turnover",
                    "predefined",
                    "accounting_flags",
                ]
            } else {
                &["name", "turnover", "accounting_flags"]
            };
            for field in fields {
                if dimension.get(field).is_none_or(Value::is_null) {
                    missing(&format!("ext_dimension_types/{index}/{field}"));
                }
            }
            if let Some(flags) = dimension.get("accounting_flags").and_then(Value::as_array) {
                for (position, flag) in flags.iter().enumerate() {
                    for field in flag_fields {
                        if flag.get(field).is_none_or(Value::is_null) {
                            missing(&format!(
                                "ext_dimension_types/{index}/accounting_flags/{position}/{field}"
                            ));
                        }
                    }
                }
            }
        }
    }
    reasons.extend(invalid);
    reasons
}

/// Select an account by its exact code before applying the common output budget.
pub fn select_predefined_account(
    result: CallToolResult,
    code: &str,
) -> Result<CallToolResult, McpError> {
    let mut body = result.structured_content.ok_or_else(|| {
        McpError::invalid_params(
            "Предопределённые счета недоступны для этого вида метаданных",
            None,
        )
    })?;
    let chart = body.get("object").cloned().unwrap_or(Value::Null);
    let kind = chart
        .get("mdo_type")
        .and_then(Value::as_str)
        .and_then(|kind| kind.parse::<MdoType>().ok())
        .or_else(|| {
            chart
                .get("full_name")
                .and_then(Value::as_str)
                .and_then(|name| name.split('.').next())
                .and_then(|kind| {
                    kind.parse::<MdoType>().ok().or_else(|| MdoType::from_plural(kind))
                })
        });
    if kind != Some(MdoType::ChartOfAccounts) {
        return Err(McpError::invalid_params(
            "predefined_code поддержан только для плана счетов",
            None,
        ));
    }
    let runtime = body.get("source").and_then(Value::as_str) == Some("infobase");
    let accounts_path =
        if runtime { "/predefined_accounts" } else { "/read_properties/predefined_accounts" };
    let accounts = chart.pointer(accounts_path).and_then(Value::as_array).ok_or_else(|| {
        McpError::invalid_params(
            "Состав предопределённых счетов недоступен",
            Some(json!({"reason":"metadata_property_unavailable","path":accounts_path})),
        )
    })?;
    let unread = chart.get("predefined_unread") == Some(&Value::Bool(true))
        || body.get("truncated") == Some(&Value::Bool(true))
        || accounts.iter().any(|account| account.get("code").is_none_or(Value::is_null));
    let mut matching =
        accounts.iter().filter(|account| account.get("code").and_then(Value::as_str) == Some(code));
    let account = matching.next().ok_or_else(|| {
        if unread {
            return McpError::invalid_params(
                "Неполный состав предопределённых счетов не позволяет подтвердить отсутствие кода",
                Some(json!({"reason":"incomplete_predefined_set"})),
            );
        }
        McpError::invalid_params(
            format!("Предопределённый счёт с кодом '{code}' не найден в выбранном плане счетов"),
            None,
        )
    })?;
    if matching.next().is_some() {
        return Err(McpError::invalid_params(
            "Коду соответствуют несколько предопределённых счетов",
            Some(json!({"reason":"ambiguous_account_code"})),
        ));
    }
    body["chart"] = chart.get("name").cloned().unwrap_or(Value::Null);
    body["object"] = account.clone();
    let mut reasons = predefined_account_reasons(account, "/object", runtime);
    if unread {
        reasons.push(json!({"code":"predefined_set_incomplete","path":"/object"}));
    }
    body["completeness"] =
        json!({"status":if reasons.is_empty() {"complete"} else {"partial"},"reasons":reasons});
    let text = format!(
        "# Предопределённый счёт {}\n\n{}\n",
        escape_markdown(code),
        serde_json::to_string_pretty(account).expect("JSON account serializes")
    );
    Ok(crate::tools::response::structured_with_text(text, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_card_keeps_locales_and_unknown_properties_in_both_outputs() {
        let localized = |ru: &str, en: &str| {
            format!(
                r#"<Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>{ru}</v8:content></v8:item><v8:item><v8:lang>en</v8:lang><v8:content>{en}</v8:content></v8:item></Synonym>"#
            )
        };
        let document = |object: &str, attribute: &str, section: &str, cell: &str| {
            format!(
                r#"<MetaDataObject xmlns:v8="urn:core" xmlns:xs="http://www.w3.org/2001/XMLSchema">
    <Catalog uuid="00000000-0000-0000-0000-000000000001">
     <Properties><Name>Code</Name>{object}<CodeLength>9</CodeLength><CodeAllowedLength>Variable</CodeAllowedLength></Properties>
     <ChildObjects>
      <Attribute uuid="00000000-0000-0000-0000-000000000002"><Properties><Name>Field</Name>{attribute}<Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute>
      <TabularSection uuid="00000000-0000-0000-0000-000000000003"><Properties><Name>Rows</Name>{section}</Properties>
       <ChildObjects><Attribute uuid="00000000-0000-0000-0000-000000000004"><Properties><Name>Cell</Name>{cell}<Type><v8:Type>xs:string</v8:Type></Type></Properties></Attribute></ChildObjects>
      </TabularSection>
     </ChildObjects>
    </Catalog></MetaDataObject>"#
            )
        };
        let translated = document(
            &localized("Объект &amp; каталог", "Object &amp; catalog"),
            &localized("Реквизит &amp; поле", "Attribute &amp; field"),
            &localized("Часть &amp; строки", "Section &amp; rows"),
            &localized("Ячейка &amp; значение", "Cell &amp; value"),
        );
        let empty = document("<Synonym/>", "<Synonym/>", "<Synonym/>", "<Synonym/>");
        let unknown = document("", "", "", "");
        for (case, xml) in [("translated", translated), ("empty", empty), ("unknown", unknown)] {
            let object = bsl_metadata::xml_parser::parse_catalog_xml(&xml).unwrap();
            let response = source_metadata_object_response(&object).unwrap();
            let text = &response.content[0].as_text().unwrap().text;
            let body = response.structured_content.as_ref().unwrap();
            let props = &body["object"]["read_properties"];
            let member = |path: &[&str]| {
                props["members"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|member| member["path"] == json!(path))
                    .unwrap()
            };
            let facts = [
                ("объекта", &props["synonym"], "Объект & каталог", "Object & catalog"),
                (
                    "Attribute.Field",
                    &member(&["Attribute", "Field"])["synonym"],
                    "Реквизит & поле",
                    "Attribute & field",
                ),
                (
                    "TabularSection.Rows",
                    &member(&["TabularSection", "Rows"])["synonym"],
                    "Часть & строки",
                    "Section & rows",
                ),
                (
                    "TabularSection.Rows.Attribute.Cell",
                    &member(&["TabularSection", "Rows", "Attribute", "Cell"])["synonym"],
                    "Ячейка & значение",
                    "Cell & value",
                ),
            ];
            for (path, value, ru, en) in facts {
                match case {
                    "translated" => {
                        assert_eq!(value["ru"], ru, "{path}");
                        assert_eq!(value["en"], en, "{path}");
                        assert!(text.contains(&format!("Синоним {path} [ru]: {ru}")), "{text}");
                        assert!(text.contains(&format!("Синоним {path} [en]: {en}")), "{text}");
                    }
                    "empty" => {
                        assert_eq!(value, &json!({}), "{path}");
                        assert!(text.contains(&format!("Синоним {path}: пустой")), "{text}");
                    }
                    "unknown" => {
                        assert!(value.is_null(), "{path}");
                        assert!(text.contains(&format!("Синоним {path}: unknown")), "{text}");
                    }
                    _ => unreachable!(),
                }
            }
            assert!(text.contains("HierarchyType: unknown"));
            assert_eq!(props["hierarchy_type"], Value::Null);
            assert_eq!(props["code_allowed_length"], "Variable");
            assert_eq!(body["completeness"]["status"], "partial");
        }
    }

    #[test]
    fn source_card_budget_omits_details_explicitly_and_preserves_valid_json() {
        let mut object = bsl_metadata::MetadataObject::new(MdoType::Catalog, "Catalog");
        for index in 0..100 {
            object.attributes.push(bsl_metadata::Attribute {
                name: format!("Field{index}"),
                name_en: None,
                attr_type: bsl_metadata::AttributeType::Boolean,
            });
        }
        let response =
            bound_source_response(source_metadata_object_response(&object).unwrap(), 300).unwrap();
        assert!(live_result_payload_bytes(&response) <= 1200);
        let body = response.structured_content.unwrap();
        assert_eq!(body["truncated"], true);
        assert_eq!(body["object"], Value::Null);
        assert_eq!(body["completeness"]["reasons"][0]["code"], "output_budget");
        assert_eq!(body["name"], "Catalog");
        assert!(
            bound_source_response(source_metadata_object_response(&object).unwrap(), 1).is_err()
        );
    }

    #[test]
    fn source_predefined_selector_does_not_claim_absence_for_unread_set() {
        let main = "<MetaDataObject><ChartOfAccounts><Properties><Name>Accounts</Name><Synonym/></Properties></ChartOfAccounts></MetaDataObject>";
        let predefined = r#"<PredefinedData><Item id="one"><Name>Known</Name><Code>01</Code><AccountType>Active</AccountType><OffBalance>false</OffBalance><AccountingFlags/><ExtDimensionTypes/></Item></PredefinedData>"#;
        let mut object = bsl_metadata::parse_metadata_object_from_texts(
            MdoType::ChartOfAccounts,
            main,
            Some(predefined),
        )
        .unwrap();
        object.predefined_unread = true;
        let response = source_metadata_object_response(&object).unwrap();
        let error = select_predefined_account(response.clone(), "missing").unwrap_err();
        assert_eq!(error.data.as_ref().unwrap()["reason"], "incomplete_predefined_set");
        let known = select_predefined_account(response, "01").unwrap();
        assert_eq!(known.structured_content.unwrap()["completeness"]["status"], "partial");
    }

    #[test]
    fn source_predefined_flags_keep_unknown_values_partial_in_object_and_selection() {
        let main = "<MetaDataObject><ChartOfAccounts><Properties><Name>Accounts</Name><Synonym/></Properties></ChartOfAccounts></MetaDataObject>";
        for (case, flags) in [
            ("unknown", "<AccountingFlags><Flag ref='Quantity'>unknown</Flag></AccountingFlags>"),
            ("missing-reference", "<AccountingFlags><Flag>true</Flag></AccountingFlags>"),
            ("false", "<AccountingFlags><Flag ref='Quantity'>false</Flag></AccountingFlags>"),
            ("empty", "<AccountingFlags/>"),
        ] {
            let predefined = format!(
                "<PredefinedData><Item id='one'><Name>Known</Name><Code>01</Code><AccountType>Active</AccountType><OffBalance>false</OffBalance>{flags}<ExtDimensionTypes><ExtDimensionType name='Kind'><Turnover>false</Turnover>{flags}</ExtDimensionType></ExtDimensionTypes></Item></PredefinedData>"
            );
            let object = bsl_metadata::parse_metadata_object_from_texts(
                MdoType::ChartOfAccounts,
                main,
                Some(&predefined),
            )
            .unwrap();
            let response = source_metadata_object_response(&object).unwrap();
            let selected = select_predefined_account(response.clone(), "01").unwrap();
            let full = response.structured_content.as_ref().unwrap();
            let selected = selected.structured_content.as_ref().unwrap();
            let account = &full["object"]["read_properties"]["predefined_accounts"][0];
            assert_eq!(&selected["object"], account, "{case}: selection preserves raw facts");
            let expected_flags = match case {
                "unknown" => json!([{"reference":"Quantity","enabled":null}]),
                "missing-reference" => json!([{"reference":null,"enabled":true}]),
                "false" => json!([{"reference":"Quantity","enabled":false}]),
                "empty" => json!([]),
                _ => unreachable!(),
            };
            assert_eq!(account["accounting_flags"], expected_flags, "{case}");
            assert_eq!(
                account["ext_dimension_types"][0]["accounting_flags"], expected_flags,
                "{case}"
            );
            for (card, prefix) in
                [(full, "/object/read_properties/predefined_accounts/0"), (selected, "/object")]
            {
                let reasons = card["completeness"]["reasons"].as_array().unwrap();
                let baseline = json!({"code":"technical_name_unavailable","path":"/object/tabular_sections/0/attributes/0/attr_type"});
                if prefix != "/object" {
                    assert!(
                        reasons.contains(&baseline),
                        "preserve the unrelated standard-field limit"
                    );
                }
                let flag_reasons: Vec<_> =
                    reasons.iter().filter(|reason| **reason != baseline).cloned().collect();
                let unknown_field = match case {
                    "unknown" => Some("enabled"),
                    "missing-reference" => Some("reference"),
                    _ => None,
                };
                if let Some(field) = unknown_field {
                    assert_eq!(card["completeness"]["status"], "partial", "{prefix}");
                    assert_eq!(
                        flag_reasons,
                        vec![
                            json!({"code":"property_unavailable","path":format!("{prefix}/accounting_flags/0/{field}")}),
                            json!({"code":"property_unavailable","path":format!("{prefix}/ext_dimension_types/0/accounting_flags/0/{field}")}),
                        ],
                        "{prefix}: both unknown flag locations need precise reasons"
                    );
                } else {
                    if prefix == "/object" {
                        assert_eq!(card["completeness"]["status"], "complete", "{case}: {prefix}");
                    }
                    assert!(flag_reasons.is_empty(), "{case}: {prefix}: {flag_reasons:?}");
                }
            }
        }
    }

    #[test]
    fn source_predefined_selector_matches_code_and_keeps_effective_account_details() {
        let main = "<MetaDataObject><ChartOfAccounts><Properties><Name>Accounts</Name><Synonym/></Properties></ChartOfAccounts></MetaDataObject>";
        let predefined = r#"<PredefinedData><Item id="one"><Name>NamedAccount</Name><Code>01</Code><AccountType>Active</AccountType><OffBalance>false</OffBalance><AccountingFlags/><ExtDimensionTypes/></Item></PredefinedData>"#;
        let mut object = bsl_metadata::parse_metadata_object_from_texts(
            MdoType::ChartOfAccounts,
            main,
            Some(predefined),
        )
        .unwrap();
        let mut overlay = bsl_metadata::parse_metadata_object_from_texts(MdoType::ChartOfAccounts,main,Some(r#"<PredefinedData><Item id="one"><Name>NamedAccount</Name><Code>02</Code><ExtDimensionTypes><ExtDimensionType name="Kind"><Turnover>true</Turnover><AccountingFlags/></ExtDimensionType></ExtDimensionTypes></Item></PredefinedData>"#)).unwrap();
        object.read_properties.set_source_origin(":ChartsOfAccounts/Accounts.xml");
        overlay.read_properties.set_source_origin("Extra:ChartsOfAccounts/Accounts.xml");
        object.apply_extension_overlay(&overlay);
        let card = source_metadata_object_response(&object).unwrap();
        assert!(select_predefined_account(card.clone(), "NamedAccount").is_err());
        assert!(select_predefined_account(card.clone(), "01").is_err());
        let selected = select_predefined_account(card, "02").unwrap();
        let body = selected.structured_content.unwrap();
        assert_eq!(body["chart"], "Accounts");
        assert_eq!(body["object"]["name"], "NamedAccount");
        assert_eq!(body["object"]["ext_dimension_types"][0]["turnover"], true);
        assert_eq!(
            body["object"]["source_origins"],
            json!([":ChartsOfAccounts/Accounts.xml", "Extra:ChartsOfAccounts/Accounts.xml"])
        );
        assert_eq!(body["completeness"]["status"], "complete");
    }
}

#[cfg(test)]
mod characteristic_type_tests;

#[cfg(test)]
mod subsystem_contract_tests;
