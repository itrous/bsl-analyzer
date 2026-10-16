use bsl_metadata::metadata_object::MdoType;
use bsl_metadata::traits::MdObject;
use bsl_metadata::Configuration;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData as McpError;
use std::collections::BTreeMap;
use std::fmt::Write;

pub async fn get_live_metadata_tree(
    state: &crate::SharedState,
    connection: Option<&str>,
    meta_type: &str,
    name_mask: Option<String>,
    limit: u32,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected =
        state.onec_connection(connection).map_err(|e| McpError::invalid_params(e, None))?;
    let client = selected
        .http_client()
        .ok_or_else(|| McpError::invalid_params("This 1C connection has no HTTP backend", None))?;
    let result = client
        .list_metadata(&onec_client::MetadataListRequest {
            meta_type: meta_type.to_string(),
            name_mask,
            limit: limit.clamp(1, 1000),
        })
        .await
        .map_err(live_metadata_error)?;
    live_metadata_tree_response(connection, result, max_output_tokens)
}

pub async fn get_live_metadata_object(
    state: &crate::SharedState,
    connection: Option<&str>,
    meta_type: &str,
    name: &str,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let selected =
        state.onec_connection(connection).map_err(|e| McpError::invalid_params(e, None))?;
    let client = selected
        .http_client()
        .ok_or_else(|| McpError::invalid_params("This 1C connection has no HTTP backend", None))?;
    let value = client
        .metadata_structure(&onec_client::MetadataStructureRequest {
            meta_type: meta_type.to_string(),
            name: name.to_string(),
        })
        .await
        .map_err(live_metadata_error)?;
    live_metadata_object_response(connection, value, max_output_tokens)
}

fn live_metadata_error(error: onec_client::Error) -> McpError {
    McpError::internal_error(format!("Ошибка чтения метаданных 1С: {error}"), None)
}

// ponytail: each rejected candidate costs one full serialization, up to one pass per returned
// metadata item (O(n²) bytes in the largest case). Keep the simple exact check while a single
// metadata object stays comfortably small; if real producer objects make this slow, account for
// removed array suffixes incrementally and serialize only the final envelope.
fn live_metadata_object_response(
    connection: Option<&str>,
    object: onec_client::MetadataStructureResult,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let mut object = serde_json::to_value(object).map_err(|error| {
        McpError::internal_error(format!("Ошибка сериализации метаданных 1С: {error}"), None)
    })?;
    let mut truncated = false;
    let ceiling = max_output_tokens.saturating_mul(4);

    loop {
        let completeness = live_object_completeness(&object, truncated);
        let result = crate::tools::response::structured(serde_json::json!({
            "schema_version": "2",
            "source": "infobase",
            "connection": connection,
            "object": object.clone(),
            "completeness": completeness,
            "truncated": truncated,
        }));
        if live_result_payload_bytes(&result) <= ceiling {
            return Ok(result);
        }

        let mut removed = false;
        for field in ["Ресурсы", "Измерения", "ТабличныеЧасти", "Реквизиты", "СтандартныеРеквизиты"]
        {
            if object
                .get_mut(field)
                .and_then(serde_json::Value::as_array_mut)
                .is_some_and(|items| items.pop().is_some())
            {
                removed = true;
                truncated = true;
                break;
            }
        }
        if !removed {
            return Err(metadata_budget_error(live_result_payload_bytes(&result)));
        }
    }
}

fn live_metadata_tree_response(
    connection: Option<&str>,
    result: onec_client::MetadataListResult,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let mut items: Vec<_> = result
        .items
        .into_iter()
        .map(|item| {
            serde_json::json!({ "name": item.name, "full_name": item.full_name, "synonym": item.synonym })
        })
        .collect();
    let mut truncated = result.truncated;
    let ceiling = max_output_tokens.saturating_mul(4);

    loop {
        let response = crate::tools::response::structured(serde_json::json!({
            "schema_version": "2",
            "source": "infobase",
            "connection": connection,
            "items": items.clone(),
            "returned": items.len(),
            "truncated": truncated,
        }));
        if live_result_payload_bytes(&response) <= ceiling {
            return Ok(response);
        }
        if items.pop().is_none() {
            return Err(metadata_budget_error(live_result_payload_bytes(&response)));
        }
        truncated = true;
    }
}

fn live_result_payload_bytes(result: &CallToolResult) -> usize {
    crate::tools::response::serialized_bytes(&result.content)
        + result
            .structured_content
            .as_ref()
            .map(crate::tools::response::serialized_bytes)
            .unwrap_or_default()
}

fn metadata_budget_error(minimum_bytes: usize) -> McpError {
    McpError::invalid_params(
        "max_output_tokens is too small for the metadata response",
        Some(serde_json::json!({
            "reason": "budget_too_small",
            "minimum_output_tokens": minimum_bytes.saturating_add(3) / 4,
        })),
    )
}

fn live_object_completeness(object: &serde_json::Value, truncated: bool) -> serde_json::Value {
    let mut reasons = Vec::new();
    for field in ["СтандартныеРеквизиты", "Реквизиты", "Измерения", "Ресурсы"]
    {
        if let Some(items) = object.get(field).and_then(serde_json::Value::as_array) {
            for (index, item) in items.iter().enumerate() {
                let path = pointer_child(&pointer_child("", field), &index.to_string());
                push_unresolved_type_reasons(item, &path, &mut reasons);
            }
        }
    }
    if let Some(sections) = object.get("ТабличныеЧасти").and_then(serde_json::Value::as_array)
    {
        for (section_index, section) in sections.iter().enumerate() {
            let section_path =
                pointer_child(&pointer_child("", "ТабличныеЧасти"), &section_index.to_string());
            match section.get("attributes") {
                Some(serde_json::Value::Null) | None => reasons.push(serde_json::json!({
                    "code": "legacy_tabular_attributes",
                    "path": pointer_child(&section_path, "attributes"),
                })),
                Some(serde_json::Value::Array(attributes)) => {
                    for (attribute_index, attribute) in attributes.iter().enumerate() {
                        push_unresolved_type_reasons(
                            attribute,
                            &pointer_child(
                                &pointer_child(&section_path, "attributes"),
                                &attribute_index.to_string(),
                            ),
                            &mut reasons,
                        );
                    }
                }
                _ => unreachable!("tabular attributes are normalized to an array or null"),
            }
        }
    }
    if truncated {
        reasons.push(serde_json::json!({ "code": "output_budget", "path": "" }));
    }
    serde_json::json!({
        "status": if reasons.is_empty() { "complete" } else { "partial" },
        "reasons": reasons,
    })
}

fn pointer_child(parent: &str, segment: &str) -> String {
    let escaped = segment.replace('~', "~0").replace('/', "~1");
    format!("{parent}/{escaped}")
}

fn push_unresolved_type_reasons(
    item: &serde_json::Value,
    path: &str,
    reasons: &mut Vec<serde_json::Value>,
) {
    if let Some(variants) = item.get("type_variants").and_then(serde_json::Value::as_array) {
        for (index, variant) in variants.iter().enumerate() {
            if variant.get("resolution").and_then(serde_json::Value::as_str) == Some("unresolved") {
                reasons.push(serde_json::json!({
                    "code": "unresolved_type",
                    "path": pointer_child(
                        &pointer_child(&pointer_child(path, "type_variants"), &index.to_string()),
                        "technical_name",
                    ),
                }));
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceKind {
    Http,
    Web,
    Integration,
}

fn parse_service_kind(raw: &str) -> Option<ServiceKind> {
    match raw.to_lowercase().as_str() {
        "httpservice" | "httpservices" | "httpсервис" | "httpсервисы" => {
            Some(ServiceKind::Http)
        }
        "webservice" | "webservices" | "webсервис" | "webсервисы" => {
            Some(ServiceKind::Web)
        }
        "integrationservice" | "integrationservices" | "сервисинтеграции" | "сервисыинтеграции" => {
            Some(ServiceKind::Integration)
        }
        _ => None,
    }
}

fn dash(s: &str) -> &str {
    if s.is_empty() {
        "—"
    } else {
        s
    }
}

/// Walk a whole-configuration collection with a cancellation checkpoint per item.
///
/// These renderers only format what the substrate already holds: they run no salsa
/// query, so a cancelled request has no query boundary to unwind at and would list
/// every object in the configuration before anyone noticed. Every walk that scales with
/// configuration size goes through this iterator — the SELECTION of a category as much
/// as the listing of what it selected, because a category with no objects still costs a
/// full pass and never reaches the listing loop.
///
/// Where it deliberately does not go: rendering ONE entity — an object's attributes, a
/// register's dimensions, a service's operations. That work is bounded by the entity,
/// and the salsa query that resolved it is itself a checkpoint, so the only window left
/// is one object's worth of `writeln!`.
fn checkpointed<'a, I>(
    db: &'a ide::RootDatabaseImpl,
    items: I,
) -> impl Iterator<Item = I::Item> + 'a
where
    I: IntoIterator + 'a,
{
    items.into_iter().inspect(|_| salsa::Database::unwind_if_revision_cancelled(db))
}

pub fn get_metadata_tree(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
    extensions: &[(String, Configuration)],
    filter: Option<String>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    if let Some(ref category) = filter {
        format_filtered_tree(db, config, category, max_output_tokens)
    } else {
        let mut result = format_summary_tree(db, config)?;
        if !extensions.is_empty() {
            let text = result.content[0].as_text().expect("text").text.clone();
            let mut out = text;
            for (name, ext_config) in extensions {
                out.push_str(&format_extension_summary(db, name, ext_config));
            }
            result = CallToolResult::success(vec![ContentBlock::text(out)]);
        }
        Ok(result)
    }
}

/// `db` is here for one reason: these loops enumerate a whole configuration without
/// running a single salsa query, so a cancelled request is observed at the checkpoint
/// or not until the last object.
fn format_summary_tree(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
) -> Result<CallToolResult, McpError> {
    let mut categories: BTreeMap<&str, usize> = BTreeMap::new();

    for obj in checkpointed(db, config.metadata_objects()) {
        let key = obj.mdo_type.russian_name();
        *categories.entry(key).or_default() += 1;
    }

    for reg in checkpointed(db, config.registers()) {
        let key = reg.mdo_type().russian_name();
        *categories.entry(key).or_default() += 1;
    }

    let common_modules = config.common_modules().len();
    if common_modules > 0 {
        categories.insert("ОбщийМодуль", common_modules);
    }

    let event_subs = config.event_subscriptions().len();
    if event_subs > 0 {
        categories.insert("ПодпискаНаСобытие", event_subs);
    }

    let defined_types = config.defined_types().len();
    if defined_types > 0 {
        categories.insert("ОпределяемыйТип", defined_types);
    }

    let scheduled_jobs = config.scheduled_jobs().len();
    if scheduled_jobs > 0 {
        categories.insert("РегламентноеЗадание", scheduled_jobs);
    }

    let roles = config.roles().len();
    if roles > 0 {
        categories.insert("Роль", roles);
    }

    let http_services = config.http_services().len();
    if http_services > 0 {
        categories.insert("HTTPСервис", http_services);
    }

    let web_services = config.web_services().len();
    if web_services > 0 {
        categories.insert("WebСервис", web_services);
    }

    let integration_services = config.integration_services().len();
    if integration_services > 0 {
        categories.insert("СервисИнтеграции", integration_services);
    }

    let total: usize = categories.values().sum();
    let mut out = format!("# {} — дерево метаданных\n\n", config.name());
    let _ = writeln!(out, "Всего объектов: {total}\n");
    let _ = writeln!(out, "| Категория | Количество |");
    let _ = writeln!(out, "|-----------|------------|");
    for (category, count) in &categories {
        let _ = writeln!(out, "| {category} | {count} |");
    }
    let _ = writeln!(out, "\nИспользуйте `filter` для получения списка объектов категории.");

    Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
}

/// Map a user-supplied category filter to a canonical category the tree understands.
///
/// The strict [`MdoType`] parser only knows the singular RU/EN names (`Справочник`/`Catalog`),
/// but an agent naturally tries the on-disk directory / plural forms (`Catalogs`, `Справочники`)
/// or over-qualifies a category with an object (`Справочник.Пользователи`). Normalise those to
/// the singular so the filter just works instead of erroring on a reasonable input.
fn normalize_filter_category(raw: &str) -> String {
    // `Справочник.Пользователи` — a category over-qualified with an object name; keep the head.
    let head = raw.split('.').next().unwrap_or(raw).trim();
    // English plurals ARE the directory names (`Catalogs`, `Documents`); stripping a trailing
    // ASCII `s` recovers the singular the parser knows, with no per-type table to maintain.
    let depluralised = head.strip_suffix('s').filter(|s| !s.is_empty());
    if let Some(en) = depluralised {
        if en.parse::<MdoType>().is_ok() {
            return en.to_string();
        }
    }
    // Russian plurals the parser does not enumerate (the few categories an agent commonly asks
    // for by plural). Anything else passes through unchanged for the parser / special-case match.
    match head.to_lowercase().as_str() {
        "справочники" => "Справочник".to_string(),
        "документы" => "Документ".to_string(),
        "отчеты" | "отчёты" => "Отчет".to_string(),
        "обработки" => "Обработка".to_string(),
        "перечисления" => "Перечисление".to_string(),
        "задачи" => "Задача".to_string(),
        "константы" => "Константа".to_string(),
        "бизнеспроцессы" => "БизнесПроцесс".to_string(),
        "планыобмена" => "ПланОбмена".to_string(),
        _ => head.to_string(),
    }
}

fn format_filtered_tree(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
    raw_category: &str,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let category = normalize_filter_category(raw_category);
    let category = category.as_str();
    let mut out = String::new();
    let mut found = false;

    if let Ok(mdo_type) = category.parse::<MdoType>() {
        // The SELECTION walks the whole configuration too, and for a category with no
        // objects the listing loop below never runs — so the checkpoint has to be here,
        // not only on what survives the filter.
        let objects: Vec<_> = checkpointed(db, config.metadata_objects())
            .filter(|o| o.mdo_type == mdo_type)
            .collect();

        if !objects.is_empty() {
            found = true;
            let _ = writeln!(out, "# {} ({})\n", mdo_type.russian_name(), objects.len());
            for obj in checkpointed(db, &objects) {
                if let Some(ref name_en) = obj.name_en {
                    let _ = writeln!(out, "- {} ({name_en})", obj.name);
                } else {
                    let _ = writeln!(out, "- {}", obj.name);
                }
            }
        }

        let registers: Vec<_> =
            checkpointed(db, config.registers()).filter(|r| r.mdo_type() == mdo_type).collect();

        if !registers.is_empty() {
            found = true;
            let _ = writeln!(out, "# {} ({})\n", mdo_type.russian_name(), registers.len());
            for reg in checkpointed(db, &registers) {
                let _ = writeln!(out, "- {}", reg.name());
            }
        }
    }

    if !found {
        if let Some(kind) = parse_service_kind(category) {
            found = format_service_listing(db, config, kind, &mut out);
        }
    }

    if !found {
        match category.to_lowercase().as_str() {
            "общиемодули" | "общиймодуль" | "commonmodule" | "commonmodules" =>
            {
                let modules = config.common_modules();
                if !modules.is_empty() {
                    found = true;
                    let _ = writeln!(out, "# ОбщиеМодули ({})\n", modules.len());
                    for m in checkpointed(db, modules) {
                        let flags = format_common_module_flags(m);
                        let _ = writeln!(out, "- {} {flags}", m.name());
                    }
                }
            }
            "подписканасобытие" | "eventsubscription" | "eventsubscriptions" => {
                let subs = config.event_subscriptions();
                if !subs.is_empty() {
                    found = true;
                    let _ = writeln!(out, "# ПодпискиНаСобытия ({})\n", subs.len());
                    for s in checkpointed(db, subs) {
                        let _ = writeln!(out, "- {}", s.name());
                    }
                }
            }
            "определяемыйтип" | "definedtype" | "definedtypes" => {
                let types = config.defined_types();
                if !types.is_empty() {
                    found = true;
                    let _ = writeln!(out, "# ОпределяемыеТипы ({})\n", types.len());
                    for t in checkpointed(db, types) {
                        let _ = writeln!(out, "- {}", t.name());
                    }
                }
            }
            "роль" | "role" | "roles" => {
                let roles = config.roles();
                if !roles.is_empty() {
                    found = true;
                    let _ = writeln!(out, "# Роли ({})\n", roles.len());
                    for r in checkpointed(db, roles) {
                        let _ = writeln!(out, "- {}", r.name());
                    }
                }
            }
            _ => {}
        }
    }

    if !found {
        return Err(McpError::invalid_params(
            format!("Категория '{raw_category}' не найдена. Вызовите get_metadata_tree без фильтра для списка категорий."),
            None,
        ));
    }

    // A category listing on a large config (e.g. every Справочник in ERP) can be thousands of
    // lines; cap it to the output budget at a line boundary so the response stays bounded.
    crate::tools::response::truncate_text_to_budget(
        &mut out,
        max_output_tokens,
        "\n-- список усечён под max_output_tokens; повысьте бюджет или запросите объект действием `object` --\n",
    );

    Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
}

fn format_common_module_flags(m: &bsl_metadata::CommonModule) -> String {
    let mut flags = Vec::new();
    if m.is_server() {
        flags.push("Сервер");
    }
    if m.is_client_managed_application() {
        flags.push("Клиент");
    }
    if m.is_global() {
        flags.push("Глобальный");
    }
    if m.is_privileged() {
        flags.push("Привилегированный");
    }
    if m.is_server_call() {
        flags.push("ВызовСервера");
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!("[{}]", flags.join(", "))
    }
}

fn format_service_listing(
    db: &ide::RootDatabaseImpl,
    config: &Configuration,
    kind: ServiceKind,
    out: &mut String,
) -> bool {
    match kind {
        ServiceKind::Http => {
            let services = config.http_services();
            if services.is_empty() {
                return false;
            }
            let _ = writeln!(out, "# HTTPСервисы ({})\n", services.len());
            for service in checkpointed(db, services) {
                let _ = writeln!(out, "- {}", service.name());
            }
            true
        }
        ServiceKind::Web => {
            let services = config.web_services();
            if services.is_empty() {
                return false;
            }
            let _ = writeln!(out, "# WebСервисы ({})\n", services.len());
            for service in checkpointed(db, services) {
                let _ = writeln!(out, "- {}", service.name());
            }
            true
        }
        ServiceKind::Integration => {
            let services = config.integration_services();
            if services.is_empty() {
                return false;
            }
            let _ = writeln!(out, "# СервисыИнтеграции ({})\n", services.len());
            for service in checkpointed(db, services) {
                let _ = writeln!(out, "- {}", service.name());
            }
            true
        }
    }
}

/// The whole-`Configuration` object formatter. Production resolves objects from the
/// resident substrate via [`object_from_db`] (sharing the `format_*` renderers below);
/// this variant is retained as the fixture-driven contract test for that formatting, so
/// it is test-only.
#[cfg(test)]
pub fn get_object_structure(
    config: &Configuration,
    object_type: &str,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    if let Some(kind) = parse_service_kind(object_type) {
        return get_service_structure(config, kind, object_name);
    }

    let mdo_type: MdoType =
        object_type.parse().map_err(|e: String| McpError::invalid_params(e, None))?;

    // An event subscription is not a data-bearing object: it lives in its own catalog,
    // so it is looked up by name rather than via `find_metadata_object`/`find_register`.
    if mdo_type == MdoType::EventSubscription {
        return match config.find_event_subscription(object_name) {
            Some(sub) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_event_subscription_structure(sub),
            )])),
            None => Err(McpError::invalid_params(
                format!("ПодпискаНаСобытие.{} не найдена в конфигурации", object_name),
                None,
            )),
        };
    }

    if let Some(obj) = config.find_metadata_object(mdo_type, object_name) {
        return Ok(CallToolResult::success(vec![ContentBlock::text(
            format_metadata_object_structure(obj, mdo_type),
        )]));
    }

    if let Some(reg) = config.find_register_by_type_and_name(mdo_type, object_name) {
        return Ok(CallToolResult::success(vec![ContentBlock::text(format_register_structure(
            reg,
        ))]));
    }

    Err(McpError::invalid_params(
        format!("Объект {}.{} не найден в конфигурации", mdo_type.russian_name(), object_name),
        None,
    ))
}

#[cfg(test)]
fn get_service_structure(
    config: &Configuration,
    kind: ServiceKind,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    match kind {
        ServiceKind::Http => match config.find_http_service(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_http_service_structure(service),
            )])),
            None => Err(McpError::invalid_params(
                format!("HTTPService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
        ServiceKind::Web => match config.find_web_service(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_web_service_structure(service),
            )])),
            None => Err(McpError::invalid_params(
                format!("WebService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
        ServiceKind::Integration => match config.find_integration_service(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_integration_service_structure(service),
            )])),
            None => Err(McpError::invalid_params(
                format!("IntegrationService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
    }
}

fn format_event_subscription_structure(sub: &bsl_metadata::EventSubscription) -> String {
    let mut out = format!("# ПодпискаНаСобытие.{}\n\n", sub.name());
    writeln!(out, "- Источник: {}", dash(sub.source())).ok();
    writeln!(out, "- Событие: {}", dash(sub.event())).ok();
    writeln!(out, "- Обработчик: {}", dash(sub.handler_string())).ok();
    if let Some(handler) = sub.parse_handler() {
        writeln!(out, "  - Модуль: {}", handler.module_name).ok();
        writeln!(out, "  - Метод: {}", handler.method_name).ok();
    }
    out
}

fn format_http_service_structure(service: &bsl_metadata::HTTPService) -> String {
    let mut out = format!("# HTTPService.{}\n\n", service.name());
    let _ = writeln!(out, "- RootURL: {}", dash(service.root_url()));
    if let Some(uri) = service.uri() {
        let _ = writeln!(out, "- Модуль: {uri}");
    }

    let templates = service.url_templates();
    if !templates.is_empty() {
        let _ = writeln!(out, "\n## URLTemplates ({})\n", templates.len());
        for template in templates {
            let _ = writeln!(out, "### {}", template.name());
            let _ = writeln!(out, "- URLTemplate: {}", dash(template.template()));
            let methods = template.methods();
            if !methods.is_empty() {
                let _ = writeln!(out, "\n| Метод | HTTPMethod | Обработчик |");
                let _ = writeln!(out, "|-------|------------|------------|");
                for method in methods {
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} |",
                        method.name(),
                        dash(method.http_method()),
                        dash(method.handler())
                    );
                }
            }
            out.push('\n');
        }
    }

    out
}

fn format_web_service_structure(service: &bsl_metadata::WebService) -> String {
    let mut out = format!("# WebService.{}\n\n", service.name());
    let _ = writeln!(out, "- Namespace: {}", dash(service.namespace()));
    if let Some(uri) = service.uri() {
        let _ = writeln!(out, "- Модуль: {uri}");
    }

    let operations = service.operations();
    if !operations.is_empty() {
        let _ = writeln!(out, "\n## Операции ({})\n", operations.len());
        let _ = writeln!(out, "| Операция | Обработчик | Параметры |");
        let _ = writeln!(out, "|----------|------------|-----------|");
        for operation in operations {
            let parameters = operation
                .parameters()
                .iter()
                .map(bsl_metadata::WebServiceParameter::name)
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "| {} | {} | {} |",
                operation.name(),
                dash(operation.procedure_name()),
                dash(&parameters)
            );
        }
    }

    out
}

fn format_integration_service_structure(service: &bsl_metadata::IntegrationService) -> String {
    let mut out = format!("# IntegrationService.{}\n\n", service.name());
    let channels = service.channels();
    if !channels.is_empty() {
        let _ = writeln!(out, "## Каналы ({})\n", channels.len());
        let _ = writeln!(out, "| Канал | ReceiveMessageProcessing |");
        let _ = writeln!(out, "|-------|--------------------------|");
        for channel in channels {
            let _ = writeln!(
                out,
                "| {} | {} |",
                channel.name(),
                dash(channel.receive_message_processing())
            );
        }
    }

    out
}

fn format_metadata_object_structure(
    obj: &bsl_metadata::MetadataObject,
    mdo_type: MdoType,
) -> String {
    let mut out = format!("# {}.{}\n\n", mdo_type.russian_name(), escape_markdown(&obj.name));
    let unresolved = metadata_object_type_reasons(obj);

    if let Some(ref name_en) = obj.name_en {
        let _ = writeln!(out, "Английское имя: {}\n", escape_markdown(name_en));
    }

    append_type_completeness(&mut out, &unresolved);

    if !obj.attributes.is_empty() {
        let _ = writeln!(out, "## Реквизиты ({})\n", obj.attributes.len());
        let _ = writeln!(out, "| Имя | Тип |");
        let _ = writeln!(out, "|-----|-----|");
        for attr in &obj.attributes {
            let _ = writeln!(
                out,
                "| {} | {} |",
                escape_markdown(&attr.name),
                format_source_type(&attr.attr_type)
            );
        }
        out.push('\n');
    }

    if !obj.tabular_sections.is_empty() {
        let _ = writeln!(out, "## Табличные части ({})\n", obj.tabular_sections.len());
        for ts in &obj.tabular_sections {
            let _ = writeln!(out, "### {}\n", escape_markdown(ts.name()));
            if !ts.attributes().is_empty() {
                let _ = writeln!(out, "| Имя | Тип |");
                let _ = writeln!(out, "|-----|-----|");
                for attr in ts.attributes() {
                    let _ = writeln!(
                        out,
                        "| {} | {} |",
                        escape_markdown(attr.name()),
                        format_source_type(attr.attr_type())
                    );
                }
                out.push('\n');
            }
        }
    }

    if !obj.enum_values.is_empty() {
        let _ = writeln!(out, "## Значения перечисления ({})\n", obj.enum_values.len());
        for val in &obj.enum_values {
            if let Some(ref name_en) = val.name_en {
                let _ = writeln!(
                    out,
                    "- {} ({})",
                    escape_markdown(&val.name),
                    escape_markdown(name_en)
                );
            } else {
                let _ = writeln!(out, "- {}", escape_markdown(&val.name));
            }
        }
        out.push('\n');
    }

    if !obj.predefined_items.is_empty() {
        let _ = writeln!(out, "## Предопределённые элементы ({})\n", obj.predefined_items.len());
        for item in &obj.predefined_items {
            let _ = writeln!(out, "- {}", escape_markdown(&item.name));
        }
    }

    out
}

fn metadata_object_type_reasons(obj: &bsl_metadata::MetadataObject) -> Vec<(String, &'static str)> {
    let mut reasons = Vec::new();
    for attr in &obj.attributes {
        collect_type_reasons(&attr.attr_type, &format!("Реквизиты.{}", attr.name), &mut reasons);
    }
    for section in &obj.tabular_sections {
        for attr in section.attributes() {
            collect_type_reasons(
                attr.attr_type(),
                &format!("ТабличныеЧасти.{}.Реквизиты.{}", section.name(), attr.name()),
                &mut reasons,
            );
        }
    }
    reasons
}

fn collect_type_reasons(
    ty: &bsl_metadata::AttributeType,
    path: &str,
    reasons: &mut Vec<(String, &'static str)>,
) {
    use bsl_metadata::AttributeType;
    match ty {
        AttributeType::Unknown => reasons.push((path.to_string(), "technical_name_unavailable")),
        AttributeType::UnknownNamed(_) => {
            reasons.push((path.to_string(), "unknown_technical_name"))
        }
        AttributeType::Composite { types } => {
            for ty in types {
                collect_type_reasons(ty, path, reasons);
            }
        }
        _ => {}
    }
}

fn append_type_completeness(out: &mut String, reasons: &[(String, &'static str)]) {
    if reasons.is_empty() {
        out.push_str("Полнота типов: complete\n\n");
    } else {
        out.push_str("Полнота типов: partial\n");
        for (path, reason) in reasons {
            let _ = writeln!(out, "- {}: {reason}", escape_markdown(path));
        }
        out.push('\n');
    }
}

/// Markdown-safe by structure, not by presentation: object names are identifiers the
/// caller matches against the source configuration, so they travel verbatim — `_` and
/// `*` used to be escaped and made the names unmatchable (github#171). Only what would
/// corrupt the emitted structure is handled: the table cell separator (and the
/// backslash that could otherwise unescape it) and line breaks.
fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '|' => escaped.push_str("\\|"),
            '\n' | '\r' => escaped.push(' '),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn format_source_type(ty: &bsl_metadata::AttributeType) -> String {
    use bsl_metadata::AttributeType;

    match ty {
        AttributeType::Composite { types } => {
            types.iter().map(format_source_type).collect::<Vec<_>>().join("; ")
        }
        AttributeType::UnknownNamed(presentation) => {
            format!("Не разрешён: {} (technical_name_unavailable)", escape_markdown(presentation))
        }
        AttributeType::Unknown => {
            "Не разрешён: неизвестный вариант (technical_name_unavailable)".to_string()
        }
        other => escape_markdown(&other.to_string()),
    }
}

fn format_register_structure(reg: &bsl_metadata::Register) -> String {
    let mut out =
        format!("# {}.{}\n\n", reg.mdo_type().russian_name(), escape_markdown(reg.name()));
    let mut reasons = Vec::new();
    for dim in reg.dimensions() {
        collect_type_reasons(
            &register_type(dim.attr_type(), dim.type_str()),
            &format!("Измерения.{}", dim.name()),
            &mut reasons,
        );
    }
    for res in reg.resources() {
        collect_type_reasons(
            &register_type(res.attr_type(), res.type_str()),
            &format!("Ресурсы.{}", res.name()),
            &mut reasons,
        );
    }
    for attr in reg.attributes() {
        collect_type_reasons(
            &register_type(attr.attr_type(), attr.type_str()),
            &format!("Реквизиты.{}", attr.name()),
            &mut reasons,
        );
    }
    append_type_completeness(&mut out, &reasons);

    if !reg.dimensions().is_empty() {
        let _ = writeln!(out, "## Измерения ({})\n", reg.dimensions().len());
        let _ = writeln!(out, "| Имя | Тип |");
        let _ = writeln!(out, "|-----|-----|");
        for dim in reg.dimensions() {
            let type_str = format_source_type(&register_type(dim.attr_type(), dim.type_str()));
            let _ = writeln!(out, "| {} | {} |", escape_markdown(dim.name()), type_str);
        }
        out.push('\n');
    }

    if !reg.resources().is_empty() {
        let _ = writeln!(out, "## Ресурсы ({})\n", reg.resources().len());
        let _ = writeln!(out, "| Имя | Тип |");
        let _ = writeln!(out, "|-----|-----|");
        for res in reg.resources() {
            let type_str = format_source_type(&register_type(res.attr_type(), res.type_str()));
            let _ = writeln!(out, "| {} | {} |", escape_markdown(res.name()), type_str);
        }
        out.push('\n');
    }

    if !reg.attributes().is_empty() {
        let _ = writeln!(out, "## Реквизиты ({})\n", reg.attributes().len());
        let _ = writeln!(out, "| Имя | Тип |");
        let _ = writeln!(out, "|-----|-----|");
        for attr in reg.attributes() {
            let type_str = format_source_type(&register_type(attr.attr_type(), attr.type_str()));
            let _ = writeln!(out, "| {} | {} |", escape_markdown(attr.name()), type_str);
        }
        out.push('\n');
    }

    if let Some(periodicity) = reg.periodicity() {
        let _ = writeln!(out, "Периодичность: {periodicity:?}");
    }
    if let Some(register_type) = reg.register_type() {
        let _ = writeln!(out, "Вид регистра: {register_type:?}");
    }

    out
}

fn register_type(
    parsed: Option<&bsl_metadata::AttributeType>,
    source: &str,
) -> bsl_metadata::AttributeType {
    parsed.cloned().unwrap_or_else(|| {
        if source.is_empty() {
            bsl_metadata::AttributeType::Unknown
        } else {
            bsl_metadata::AttributeType::UnknownNamed(source.to_string())
        }
    })
}

pub fn get_configuration_info(
    config: &Configuration,
    extensions: &[(String, Configuration)],
) -> Result<CallToolResult, McpError> {
    let total_objects = config.metadata_objects().len()
        + config.registers().len()
        + config.common_modules().len()
        + config.event_subscriptions().len()
        + config.defined_types().len()
        + config.scheduled_jobs().len()
        + config.roles().len()
        + config.http_services().len()
        + config.web_services().len()
        + config.integration_services().len();

    let mut out = format!("# Конфигурация: {}\n\n", config.name());
    let _ = writeln!(out, "- UUID: {}", config.uuid());
    let _ = writeln!(out, "- Всего объектов метаданных: {total_objects}");
    let _ = writeln!(out, "- Общие модули: {}", config.common_modules().len());
    let _ = writeln!(out, "- Объекты метаданных: {}", config.metadata_objects().len());
    let _ = writeln!(out, "- Регистры: {}", config.registers().len());
    let _ = writeln!(out, "- Подписки на события: {}", config.event_subscriptions().len());
    let _ = writeln!(out, "- Определяемые типы: {}", config.defined_types().len());
    let _ = writeln!(out, "- Регламентные задания: {}", config.scheduled_jobs().len());
    let _ = writeln!(out, "- Роли: {}", config.roles().len());
    let _ = writeln!(out, "- HTTP-сервисы: {}", config.http_services().len());
    let _ = writeln!(out, "- Web-сервисы: {}", config.web_services().len());
    let _ = writeln!(out, "- Сервисы интеграции: {}", config.integration_services().len());

    if !extensions.is_empty() {
        let _ = writeln!(out, "\n## Расширения ({})\n", extensions.len());
        for (name, ext_config) in extensions {
            let ext_objects = ext_config.metadata_objects().len()
                + ext_config.registers().len()
                + ext_config.common_modules().len();
            let _ = writeln!(out, "- **{name}**: {ext_objects} объектов");
        }
    }

    Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
}

/// Read forms from disk. `source_root` MUST be the configuration root (the
/// `Configuration.xml`-bearing directory, e.g. `src/cf`) — passing the repo root when the
/// configuration is nested under `src/cf` makes every form look missing.
///
/// `object_name` is required for an object's forms (`<TypeDir>/<object>/Forms/…`) but ignored
/// for `CommonForm`, which is a top-level form with no parent object (`CommonForms/<Form>/…`).
pub fn get_form_structure(
    source_root: Option<&std::path::Path>,
    object_type: &str,
    object_name: Option<&str>,
    form_name: Option<&str>,
) -> Result<CallToolResult, McpError> {
    let root = source_root.ok_or_else(|| {
        McpError::invalid_params("Configuration root не задан, формы недоступны", None)
    })?;

    // CommonForm has no parent object: its forms live directly under `CommonForms/<Form>`,
    // not `<TypeDir>/<object>/Forms/<Form>`, so it takes a distinct path and no `object_name`.
    // `to_lowercase` (not `eq_ignore_ascii_case`) so the Cyrillic alias folds case too.
    if matches!(object_type.to_lowercase().as_str(), "commonform" | "общаяформа") {
        let common_forms = bsl_conventions::find_child_ci(root, "CommonForms")
            .unwrap_or_else(|| root.join("CommonForms"));
        return forms_in_container(&common_forms, form_name, "ОбщаяФорма");
    }

    let object_name = object_name.ok_or_else(|| {
        McpError::invalid_params(
            "'object_name' обязателен для форм объекта (кроме CommonForm)",
            None,
        )
    })?;
    let type_dir = mdo_type_to_dir(object_type).ok_or_else(|| {
        McpError::invalid_params(format!("Неизвестный тип объекта: {object_type}"), None)
    })?;

    let type_root =
        bsl_conventions::find_child_ci(root, type_dir).unwrap_or_else(|| root.join(type_dir));
    let object_dir = type_root.join(object_name);
    let forms_dir = bsl_conventions::find_child_ci(
        &object_dir,
        bsl_conventions::ConventionalName::Forms.canonical(),
    )
    .unwrap_or_else(|| object_dir.join(bsl_conventions::ConventionalName::Forms.canonical()));
    forms_in_container(&forms_dir, form_name, &format!("{object_type}.{object_name}"))
}

/// Read a single form (when `form_name` is given) or list every form directory inside
/// `container` (`.../Forms` for an object, `CommonForms` for a common form). `title` labels
/// the listing header and the empty-set error.
fn forms_in_container(
    container: &std::path::Path,
    form_name: Option<&str>,
    title: &str,
) -> Result<CallToolResult, McpError> {
    if !container.exists() {
        return Err(McpError::invalid_params(
            format!("Каталог форм не найден: {}", container.display()),
            None,
        ));
    }

    if let Some(fname) = form_name {
        let form_dir = container.join(fname);
        let form_xml_path = bsl_conventions::resolve_chain_ci(
            &form_dir,
            &[
                bsl_conventions::ConventionalName::Ext.canonical(),
                bsl_conventions::ConventionalName::FormXml.canonical(),
            ],
        )
        .unwrap_or_else(|| {
            form_dir
                .join(bsl_conventions::ConventionalName::Ext.canonical())
                .join(bsl_conventions::ConventionalName::FormXml.canonical())
        });
        if !form_xml_path.exists() {
            return Err(McpError::invalid_params(
                format!("Форма не найдена: {}", form_xml_path.display()),
                None,
            ));
        }
        let xml = std::fs::read_to_string(&form_xml_path)
            .map_err(|e| McpError::internal_error(format!("Ошибка чтения формы: {e}"), None))?;
        let form = bsl_metadata::xml_parser::parse_form_xml(&xml)
            .map_err(|e| McpError::internal_error(format!("Ошибка разбора формы: {e}"), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(format_form(&form, Some(fname)))]))
    } else {
        let mut form_names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(container) {
            for entry in entries.flatten() {
                if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    if let Some(name) = entry.file_name().to_str() {
                        form_names.push(name.to_string());
                    }
                }
            }
        }
        form_names.sort();

        if form_names.is_empty() {
            return Err(McpError::invalid_params(format!("Формы не найдены для {title}"), None));
        }

        let mut out = format!("# Формы {title}\n\n");
        for name in &form_names {
            let _ = writeln!(out, "- {name}");
        }
        let _ =
            writeln!(out, "\nИспользуйте `form_name` для получения структуры конкретной формы.");
        Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
    }
}

/// Render a parsed form. `requested_name` is the form name the caller asked for: `Ext/Form.xml`
/// holds the managed-form *content* (items, attributes, handlers) but not the form's own name or
/// UUID — those live in the parent object's metadata — so the parsed name is usually empty and
/// the UUID nil. Fall back to the requested name for the heading, and label a nil UUID as
/// unavailable instead of printing a misleading all-zero one.
fn format_form(form: &bsl_metadata::Form, requested_name: Option<&str>) -> String {
    let title = match (form.name(), requested_name) {
        (name, _) if !name.is_empty() => name,
        (_, Some(requested)) => requested,
        (_, None) => "<имя недоступно>",
    };
    let mut out = format!("# Форма: {title}\n\n");
    let _ = writeln!(out, "- Тип: {:?}", form.form_type());
    let uuid = form.uuid();
    if uuid.is_nil() {
        let _ = writeln!(out, "- UUID: недоступен (хранится в метаданных объекта, не в Form.xml)");
    } else {
        let _ = writeln!(out, "- UUID: {uuid}");
    }

    if !form.attributes.is_empty() {
        let _ = writeln!(out, "\n## Реквизиты формы ({})\n", form.attributes.len());
        for attr in &form.attributes {
            let main_marker = if attr.is_main { " (основной)" } else { "" };
            let _ = writeln!(out, "- {}{}: {}", attr.name, main_marker, attr.attr_type);
            for col in &attr.columns {
                let _ = writeln!(out, "    - {}: {}", col.name, col.attr_type);
            }
        }
    }

    let elements = form.elements();
    if !elements.is_empty() {
        let _ = writeln!(out, "\n## Элементы ({})\n", elements.len());
        let _ = writeln!(out, "| Имя | DataPath |");
        let _ = writeln!(out, "|-----|----------|");
        for el in elements {
            let dp = el.data_path.as_deref().unwrap_or("—");
            let _ = writeln!(out, "| {} | {dp} |", el.name);
        }
    }

    if !form.event_handlers.is_empty() {
        let _ = writeln!(out, "\n## Обработчики событий ({})\n", form.event_handlers.len());
        for h in &form.event_handlers {
            let _ = writeln!(out, "- {}", h.handler_name);
        }
    }

    if !form.command_handlers.is_empty() {
        let _ = writeln!(out, "\n## Обработчики команд ({})\n", form.command_handlers.len());
        for h in &form.command_handlers {
            let _ = writeln!(out, "- {h}");
        }
    }

    out
}

fn format_extension_summary(
    db: &ide::RootDatabaseImpl,
    name: &str,
    config: &Configuration,
) -> String {
    use std::collections::BTreeMap;
    let mut out = format!("\n---\n\n# Расширение: {name}\n\n");
    let mut categories: BTreeMap<&str, usize> = BTreeMap::new();

    for obj in checkpointed(db, config.metadata_objects()) {
        let key = obj.mdo_type.russian_name();
        *categories.entry(key).or_default() += 1;
    }
    for reg in checkpointed(db, config.registers()) {
        let key = reg.mdo_type().russian_name();
        *categories.entry(key).or_default() += 1;
    }
    let common_modules = config.common_modules().len();
    if common_modules > 0 {
        categories.insert("ОбщийМодуль", common_modules);
    }

    let total: usize = categories.values().sum();
    let _ = writeln!(out, "Всего объектов: {total}\n");
    let _ = writeln!(out, "| Категория | Количество |");
    let _ = writeln!(out, "|-----------|------------|");
    for (category, count) in &categories {
        let _ = writeln!(out, "| {category} | {count} |");
    }
    out
}

/// Whether the `object` action can resolve `object_type`, mirroring [`object_from_db`]'s
/// dispatch order: a service kind (`parse_service_kind`) FIRST, then an [`MdoType`]. The
/// force-rescan miss-retry gate uses this so it cannot drift from what the resolver
/// actually accepts — `MdoType::from_str` has NO service variants, so gating on it alone
/// would never retry a just-added HTTP/Web/Integration service (they'd stay "not found"
/// until the next periodic drift poll, unlike catalogs/registers/subscriptions).
pub(crate) fn is_resolvable_object_type(object_type: &str) -> bool {
    parse_service_kind(object_type).is_some() || object_type.parse::<MdoType>().is_ok()
}

/// The `metadata object` action reading the resident host: resolve the object across the
/// base configuration and every extension (a point-lookup on the per-MDO substrate, never
/// a whole-config load) and format it. The db counterpart of [`get_object_structure`];
/// they share the `format_*` renderers. Extension-merged visibility, so an object defined
/// only in an extension is found (wider than the retired base-only read).
pub fn object_from_db(
    db: &ide::RootDatabaseImpl,
    object_type: &str,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    if let Some(kind) = parse_service_kind(object_type) {
        return service_from_db(db, kind, object_name);
    }

    let mdo_type: MdoType =
        object_type.parse().map_err(|e: String| McpError::invalid_params(e, None))?;

    if mdo_type == MdoType::EventSubscription {
        return match db.resolve_event_subscription_across_roots(object_name) {
            Some(sub) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_event_subscription_structure(&sub),
            )])),
            None => Err(McpError::invalid_params(
                format!("ПодпискаНаСобытие.{object_name} не найдена в конфигурации"),
                None,
            )),
        };
    }

    if let Some(obj) = db.resolve_metadata_object_across_roots(mdo_type, object_name) {
        return Ok(CallToolResult::success(vec![ContentBlock::text(
            format_metadata_object_structure(&obj, mdo_type),
        )]));
    }

    if let Some(reg) = db.resolve_register_across_roots(mdo_type, object_name) {
        return Ok(CallToolResult::success(vec![ContentBlock::text(format_register_structure(
            &reg,
        ))]));
    }

    Err(McpError::invalid_params(
        format!("Объект {}.{} не найден в конфигурации", mdo_type.russian_name(), object_name),
        None,
    ))
}

fn service_from_db(
    db: &ide::RootDatabaseImpl,
    kind: ServiceKind,
    object_name: &str,
) -> Result<CallToolResult, McpError> {
    match kind {
        ServiceKind::Http => match db.resolve_http_service_across_roots(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_http_service_structure(&service),
            )])),
            None => Err(McpError::invalid_params(
                format!("HTTPService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
        ServiceKind::Web => match db.resolve_web_service_across_roots(object_name) {
            Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                format_web_service_structure(&service),
            )])),
            None => Err(McpError::invalid_params(
                format!("WebService.{object_name} не найден в конфигурации"),
                None,
            )),
        },
        ServiceKind::Integration => {
            match db.resolve_integration_service_across_roots(object_name) {
                Some(service) => Ok(CallToolResult::success(vec![ContentBlock::text(
                    format_integration_service_structure(&service),
                )])),
                None => Err(McpError::invalid_params(
                    format!("IntegrationService.{object_name} не найден в конфигурации"),
                    None,
                )),
            }
        }
    }
}

/// The base configuration plus each extension's, sourced from the resident db's Channel-2
/// `load_configuration` query (Salsa-cached, invalidated by metadata drift). The `tree`
/// and `info` enumeration payload — which inherently needs whole-config counts — after
/// the `MetadataCache` retirement. `object` never calls this (it stays on the substrate).
pub fn configs_from_db(
    db: &ide::RootDatabaseImpl,
) -> (Configuration, Vec<(String, Configuration)>) {
    let paths = db.designer_config_paths();
    let base = paths
        .iter()
        .find(|(label, _)| label.is_none())
        .map(|(_, p)| (*db.configuration_for_root(p)).clone())
        .unwrap_or_else(|| Configuration::new("Configuration"));
    let extensions = paths
        .iter()
        .filter_map(|(label, p)| {
            label.as_ref().map(|name| (name.clone(), (*db.configuration_for_root(p)).clone()))
        })
        .collect();
    (base, extensions)
}

/// A "still loading, retry shortly" envelope for a metadata call issued while the resident
/// db is building (or rebuilding after an idle eviction). Never a hard "not loaded" error —
/// the resident becomes ready, OR fails and is retried on its own backoff, so the answer is
/// always either "ask again in a moment" or "here is why it stopped and when the next try
/// comes". A `status` of `failed` (with `error` and `retry_after_ms`) is a build that did not
/// finish; it is not an eternal wait, and it is not terminal either.
///
/// The machine-readable body has the same shape `diagnostics` returns — only `detail`
/// differs, naming this tool's view of the one build — so "not ready" is recognized by one
/// field check across every tool reading the resident. The Russian
/// sentence stays the text block instead of the JSON mirror `graph`/`diagnostics` emit:
/// `metadata` is a text tool (its answers are listings, not JSON), and a consumer that
/// matched the sentence before the envelope existed keeps working while it migrates to
/// `structuredContent`.
pub fn loading(report: &crate::diagnostics_state::StatusReport) -> CallToolResult {
    let mut msg = String::from("Метаданные загружаются, повторите запрос через несколько секунд.");
    if let Some(ms) = report.elapsed_ms {
        let _ = write!(msg, " (идёт загрузка, {ms} мс)");
    }
    crate::tools::response::structured_with_text(
        msg,
        crate::tools::resident::loading_body(
            report,
            "resident analysis database is building; retry shortly",
        ),
    )
}

#[cfg(test)]
fn fixture_config() -> Configuration {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
    bsl_metadata::load_from_directory(path).expect("failed to load fixture configuration")
}

fn mdo_type_to_dir(object_type: &str) -> Option<&'static str> {
    match object_type.to_lowercase().as_str() {
        "catalog" | "справочник" => Some("Catalogs"),
        "document" | "документ" => Some("Documents"),
        "dataprocessor" | "обработка" => Some("DataProcessors"),
        "report" | "отчет" | "отчёт" => Some("Reports"),
        "enum" | "перечисление" => Some("Enums"),
        "chartofcharacteristictypes" | "планвидовхарактеристик" => {
            Some("ChartsOfCharacteristicTypes")
        }
        "chartofaccounts" | "плансчетов" => Some("ChartsOfAccounts"),
        "exchangeplan" | "планобмена" => Some("ExchangePlans"),
        "businessprocess" | "бизнеспроцесс" => Some("BusinessProcesses"),
        "task" | "задача" => Some("Tasks"),
        "eventsubscription" | "подписканасобытие" => Some("EventSubscriptions"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_text(result: &CallToolResult) -> &str {
        result.content[0].as_text().expect("expected text content").text.as_str()
    }

    #[test]
    fn source_metadata_renders_each_union_variant_and_marks_unresolved_types() {
        fn config_with_union(type_entries: &str) -> bsl_metadata::Configuration {
            let dir = tempfile::tempdir().expect("source fixture tempdir");
            std::fs::create_dir_all(dir.path().join("Catalogs")).expect("Catalogs directory");
            std::fs::write(
                dir.path().join("Configuration.xml"),
                "<Configuration><Name>ТестоваяКонфигурация</Name></Configuration>",
            )
            .expect("Configuration.xml");

            let catalog = |name: &str, uuid: &str, children: &str| {
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" version="2.10">
  <Catalog uuid="{uuid}">
    <Properties><Name>{name}</Name><Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Общее представление</v8:content></v8:item></Synonym></Properties>
    <ChildObjects>{children}</ChildObjects>
  </Catalog>
</MetaDataObject>"#
                )
            };
            for (name, uuid) in [
                ("А", "00000000-0000-0000-0000-000000000001"),
                ("Б", "00000000-0000-0000-0000-000000000002"),
            ] {
                let xml = catalog(name, uuid, "");
                std::fs::write(dir.path().join("Catalogs").join(format!("{name}.xml")), xml)
                    .expect("write target catalog");
            }
            let attribute = format!(
                r#"<Attribute uuid="00000000-0000-0000-0000-000000000004"><Properties><Name>Вариант</Name><Synonym/><Type>{type_entries}</Type></Properties></Attribute>"#
            );
            let xml =
                catalog("ТестовыйКаталог", "00000000-0000-0000-0000-000000000003", &attribute);
            std::fs::write(dir.path().join("Catalogs/ТестовыйКаталог.xml"), xml)
                .expect("write catalog with union");
            bsl_metadata::load_from_directory(dir.path()).expect("load source fixture")
        }

        let complete_config = config_with_union(
            "<v8:Type>cfg:CatalogRef.А</v8:Type><v8:Type>cfg:CatalogRef.Б</v8:Type>",
        );
        let complete =
            get_object_structure(&complete_config, "Справочник", "ТестовыйКаталог").unwrap();
        let complete = extract_text(&complete);
        assert!(complete.contains("| Вариант | Справочник.А; Справочник.Б |"), "{complete}");
        assert!(complete.contains("Полнота типов: complete"), "{complete}");

        let partial_config = config_with_union(
            "<v8:Type>cfg:CatalogRef.А</v8:Type><v8:Type>d5p1:FutureType</v8:Type>",
        );
        let partial =
            get_object_structure(&partial_config, "Справочник", "ТестовыйКаталог").unwrap();
        let partial = extract_text(&partial);
        assert!(
            partial.contains("| Вариант | Справочник.А; Не разрешён: d5p1:FutureType (technical_name_unavailable) |"),
            "{partial}"
        );
        assert!(
            partial.contains("Полнота типов: partial\n- Реквизиты.Вариант: unknown_technical_name"),
            "{partial}"
        );
    }

    /// Имена — идентификаторы для последующих вызовов, и `_`/`*` в них не
    /// экранируются; экранирование осталось только структурным (github#171).
    #[test]
    fn names_travel_verbatim_and_only_structure_is_escaped() {
        assert_eq!(
            escape_markdown("Номенклатура_С_Подчёркиванием"),
            "Номенклатура_С_Подчёркиванием"
        );
        assert_eq!(escape_markdown("Тип*Звёздочка"), "Тип*Звёздочка");
        assert_eq!(escape_markdown("a|b"), "a\\|b");
        // Обратный слэш перед `|` обязан удвоиться: иначе `\|` из имени
        // «разэкранировался» бы и пайп разорвал ячейку.
        assert_eq!(escape_markdown("a\\|b"), "a\\\\\\|b");
        assert_eq!(escape_markdown("a\nb"), "a b");
        assert_eq!(escape_markdown("a\\b"), "a\\\\b");
    }

    /// Подчёркивание в имени объекта и реквизита доезжает до вывода дословно:
    /// агент сопоставляет эти имена с исходной конфигурацией и не должен
    /// разбирать экранирование (github#171).
    #[test]
    fn object_and_attribute_names_with_underscores_stay_verbatim() {
        use bsl_metadata::metadata_object::MdoType;
        use bsl_metadata::{Attribute, AttributeType, Configuration, MetadataObject};

        let mut config = Configuration::new("Тест");
        let mut object = MetadataObject::new(MdoType::Catalog, "Номенклатура_С_Подчёркиванием");
        object.add_attribute(Attribute {
            name: "Характеристика_НДС".into(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(20) },
        });
        config.add_metadata_object(object);

        let result =
            get_object_structure(&config, "Справочник", "Номенклатура_С_Подчёркиванием").unwrap();
        let text = extract_text(&result);

        assert!(text.contains("# Справочник.Номенклатура_С_Подчёркиванием"), "{text}");
        assert!(text.contains("Характеристика_НДС"), "{text}");
        assert!(!text.contains('\\'), "имена обязаны ехать дословно: {text}");
    }

    #[test]
    fn source_type_completeness_uses_qualified_paths_for_sections_and_registers() {
        use bsl_metadata::{
            metadata_object::MdoType,
            register::{Register, RegisterResource},
            tabular_section::{TabularSection, TabularSectionAttribute},
            AttributeType, Configuration, MetadataObject,
        };

        let mut config = Configuration::new("Тест");
        let mut object = MetadataObject::new(MdoType::Catalog, "Справочник");
        for (uuid, section_name) in [
            ("00000000-0000-0000-0000-000000000001", "Товары"),
            ("00000000-0000-0000-0000-000000000002", "Услуги"),
        ] {
            let mut section = TabularSection::new(uuid.parse().unwrap(), section_name);
            section.set_attributes(vec![TabularSectionAttribute::new(
                uuid.parse().unwrap(),
                "Сумма|Итого",
                AttributeType::Unknown,
            )]);
            object.add_tabular_section(section);
        }
        config.add_metadata_object(object);

        let mut unknown_resource = RegisterResource::new(
            "00000000-0000-0000-0000-000000000004".parse().unwrap(),
            "Будущий|Ресурс",
        );
        unknown_resource.set_attr_type(AttributeType::UnknownNamed("future|type".into()));
        let mut known_resource = RegisterResource::new(
            "00000000-0000-0000-0000-000000000005".parse().unwrap(),
            "Количество",
        );
        known_resource.set_attr_type(AttributeType::String { length: Some(20) });
        config.add_register(
            Register::builder()
                .name("Продажи")
                .mdo_type(MdoType::InformationRegister)
                .add_resource(known_resource)
                .add_resource(unknown_resource)
                .build(),
        );

        let object_text = get_object_structure(&config, "Справочник", "Справочник").unwrap();
        let object_text = extract_text(&object_text);
        assert!(object_text.contains("Полнота типов: partial\n- ТабличныеЧасти.Товары.Реквизиты.Сумма\\|Итого: technical_name_unavailable\n- ТабличныеЧасти.Услуги.Реквизиты.Сумма\\|Итого: technical_name_unavailable"), "{object_text}");
        assert!(
            object_text.find("Полнота типов").unwrap()
                < object_text.find("## Табличные части").unwrap(),
            "{object_text}"
        );
        assert!(
            object_text.contains(
                "| Сумма\\|Итого | Не разрешён: неизвестный вариант (technical_name_unavailable) |"
            ),
            "{object_text}"
        );

        let register_text = get_object_structure(&config, "РегистрСведений", "Продажи").unwrap();
        let register_text = extract_text(&register_text);
        assert!(
            register_text.contains(
                "Полнота типов: partial\n- Ресурсы.Будущий\\|Ресурс: unknown_technical_name"
            ),
            "{register_text}"
        );
        assert!(
            register_text.find("Полнота типов").unwrap()
                < register_text.find("## Ресурсы").unwrap(),
            "{register_text}"
        );
        assert!(
            register_text.contains(
                "| Будущий\\|Ресурс | Не разрешён: future\\|type (technical_name_unavailable) |"
            ),
            "{register_text}"
        );
        assert!(register_text.contains("| Количество | Строка(20) |"), "{register_text}");
    }

    /// Every listing a cancelled `metadata` call can enter stops at its first item
    /// instead of enumerating the configuration for a response nobody will read.
    ///
    /// One filter per branch, because the branches name different collections and a
    /// checkpoint on one says nothing about the next. What a pre-cancelled token cannot
    /// reach is a SECOND loop on the same path — the first one unwinds before it — so
    /// the summary's register pass and the extension summary are covered by entry only;
    /// they walk the same iterator, and it is the iterator that carries the checkpoint.
    #[test]
    fn a_cancelled_listing_stops_at_the_first_item() {
        let config = fixture_config();
        let db = ide::RootDatabaseImpl::new();
        salsa::Database::cancellation_token(&db).cancel();

        // Each entry names the collection the branch walks; a branch whose collection is
        // empty in the fixture would answer "category not found" and the assertion says so.
        for filter in [
            None,
            Some("Справочник"),
            Some("РегистрСведений"),
            Some("ОбщийМодуль"),
            Some("ПодпискаНаСобытие"),
            Some("Роль"),
            Some("HTTPСервис"),
            Some("WebСервис"),
            Some("СервисИнтеграции"),
        ] {
            let filter = filter.map(str::to_string);
            let got = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                get_metadata_tree(&db, &config, &[], filter.clone(), usize::MAX)
            }));
            assert!(
                matches!(got, Err(salsa::Cancelled::Local)),
                "filter {filter:?} rendered its whole listing after the request was cancelled"
            );
        }

        let got = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            format_extension_summary(&db, "Расширение", &config)
        }));
        assert!(
            matches!(got, Err(salsa::Cancelled::Local)),
            "the extension summary counted every object after the request was cancelled"
        );

        // The SELECTION of a category with no objects, on a configuration with no
        // registers either: every later loop is empty, so this input can only be
        // answered by the checkpoint on the selection itself. On the full fixture the
        // register selection unwinds first and hides whether this one exists at all.
        let objects_only = objects_only_config();
        let got = salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
            get_metadata_tree(&db, &objects_only, &[], Some("ПланОбмена".to_string()), usize::MAX)
        }));
        assert!(
            matches!(got, Err(salsa::Cancelled::Local)),
            "the category selection walked every object after the request was cancelled"
        );
    }

    /// The designer fixture with its registers left out — see the test above for why
    /// their absence is the point.
    fn objects_only_config() -> Configuration {
        let dir = tempfile::tempdir().expect("tempdir");
        let fixture = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/designer"
        ));
        std::fs::copy(fixture.join("Configuration.xml"), dir.path().join("Configuration.xml"))
            .expect("copy");
        copy_tree(&fixture.join("Catalogs"), &dir.path().join("Catalogs"));
        let config = bsl_metadata::load_from_directory(dir.path()).expect("load");
        assert!(
            config.registers().is_empty() && !config.metadata_objects().is_empty(),
            "the stand must have objects and no registers, or it proves nothing"
        );
        config
    }

    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).expect("mkdir");
        for entry in std::fs::read_dir(from).expect("read_dir").flatten() {
            let target = to.join(entry.file_name());
            if entry.file_type().expect("file_type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).expect("copy");
            }
        }
    }

    #[test]
    fn live_object_serializes_versioned_normalized_variants() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../bsl-analyzer/tests/fixtures/live_metadata_type_variants.json"
        ))
        .unwrap();
        let object: onec_client::MetadataStructureResult =
            serde_json::from_value(fixture["ru"].clone()).unwrap();
        let result = live_metadata_object_response(Some("test"), object, 6_000).unwrap();
        let body = result.structured_content.unwrap();

        assert_eq!(body["schema_version"], "2");
        assert_eq!(body["source"], "infobase");
        assert_eq!(body["connection"], "test");
        assert!(body["object"].get("futureField").is_none());
        let attributes = body["object"]["Реквизиты"].as_array().unwrap();
        assert_eq!(attributes[0]["type_variants"][0]["resolution"], "source");
        let attribute = |name| attributes.iter().find(|item| item["name"] == name).unwrap();
        assert_eq!(
            attribute("Unsupported")["type_variants"][0]["reason"],
            "technical_name_unavailable"
        );
        assert_eq!(
            attribute("FutureUnknown")["type_variants"][0]["reason"],
            "unknown_technical_name"
        );
        let sections = body["object"]["ТабличныеЧасти"].as_array().unwrap();
        let rows = sections.iter().find(|item| item["name"] == "Rows").unwrap();
        assert_eq!(rows["attributes"][0]["type_variants"].as_array().unwrap().len(), 2);
        assert_eq!(
            sections.iter().find(|item| item["name"] == "EmptyRows").unwrap()["attributes"],
            serde_json::json!([])
        );
        assert_eq!(body["completeness"]["status"], "partial");
        let reasons = body["completeness"]["reasons"].as_array().unwrap();
        assert!(reasons.iter().any(|reason| {
            reason["code"] == "unresolved_type"
                && reason["path"] == "/ТабличныеЧасти/0/attributes/1/type_variants/0/technical_name"
        }));
        for reason in reasons {
            let path = reason["path"].as_str().expect("reason path is a JSON Pointer");
            if path.is_empty() {
                continue;
            }
            assert!(body["object"].pointer(path).is_some(), "unresolved {path} in {body}");
        }

        let legacy: onec_client::MetadataStructureResult = serde_json::from_str(include_str!(
            "../../../bsl-analyzer/tests/fixtures/live_metadata_legacy.json"
        ))
        .unwrap();
        let legacy = live_metadata_object_response(Some("test"), legacy, 6_000).unwrap();
        let legacy = legacy.structured_content.unwrap();
        assert_eq!(legacy["object"]["ТабличныеЧасти"][0]["attributes"], serde_json::Value::Null);
        assert_eq!(legacy["completeness"]["status"], "partial");
        let legacy_reason = legacy["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|reason| reason["code"] == "legacy_tabular_attributes")
            .expect("legacy attributes reason");
        assert_eq!(legacy_reason["code"], "legacy_tabular_attributes");
        assert_eq!(legacy_reason["path"], "/ТабличныеЧасти/0/attributes");
        assert!(legacy["object"].pointer(legacy_reason["path"].as_str().unwrap()).is_some());

        let escaping = serde_json::json!({ "a~/b": true });
        let escaped_pointer = pointer_child("", "a~/b");
        assert_eq!(escaped_pointer, "/a~0~1b");
        assert_eq!(escaping.pointer(&escaped_pointer), Some(&serde_json::Value::Bool(true)));

        let mut large = object_from_fixture();
        large.attributes = (0..100)
            .map(|index| onec_client::MetadataStructureItem {
                name: format!("Attribute{index}"),
                synonym: String::new(),
                type_name: Some("Строка".to_string()),
                type_variants: vec![onec_client::MetadataTypeVariant {
                    technical_name: Some("Строка".to_string()),
                    presentation: "Строка".to_string(),
                    resolution: "source",
                    reason: None,
                }],
            })
            .collect();
        let fitted = live_metadata_object_response(Some("test"), large, 700).unwrap();
        assert!(live_result_payload_bytes(&fitted) <= 700 * 4);
        let fitted_body = fitted.structured_content.as_ref().unwrap();
        assert_eq!(fitted_body["truncated"], true);
        assert_eq!(fitted_body["completeness"]["status"], "partial");
        assert!(fitted_body["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| { reason["code"] == "output_budget" && reason["path"] == "" }));
        for reason in fitted_body["completeness"]["reasons"].as_array().unwrap() {
            if reason["code"] == "output_budget" {
                assert_eq!(reason["path"], "");
            } else {
                assert!(
                    fitted_body["object"].pointer(reason["path"].as_str().unwrap()).is_some(),
                    "unresolved path after budget truncation: {reason} in {fitted_body}"
                );
            }
        }

        let error =
            live_metadata_object_response(Some("test"), object_from_fixture(), 0).unwrap_err();
        assert_eq!(error.code.0, -32602);
        assert_eq!(error.data.as_ref().unwrap()["reason"], "budget_too_small");
        let minimum = error.data.unwrap()["minimum_output_tokens"].as_u64().unwrap() as usize;
        assert!(minimum > 0);
        assert!(live_metadata_object_response(Some("test"), object_from_fixture(), minimum).is_ok());
    }

    fn object_from_fixture() -> onec_client::MetadataStructureResult {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../bsl-analyzer/tests/fixtures/live_metadata_type_variants.json"
        ))
        .unwrap();
        serde_json::from_value(fixture["ru"].clone()).unwrap()
    }

    #[test]
    fn live_tree_budget_keeps_whole_items_and_recounts_service_truncation() {
        let result = onec_client::MetadataListResult {
            items: (0..100)
                .map(|index| onec_client::MetadataListItem {
                    name: format!("Имя{index}"),
                    full_name: format!("Справочник.Имя{index}"),
                    synonym: "Значение".repeat(20),
                })
                .collect(),
            returned: 777,
            truncated: true,
        };
        let fitted = live_metadata_tree_response(Some("test"), result, 700).unwrap();
        assert!(live_result_payload_bytes(&fitted) <= 700 * 4);
        let body = fitted.structured_content.as_ref().unwrap();
        assert_eq!(body["schema_version"], "2");
        assert_eq!(body["returned"], body["items"].as_array().unwrap().len());
        assert_eq!(body["truncated"], true);
        let error = live_metadata_tree_response(
            Some("test"),
            onec_client::MetadataListResult { items: Vec::new(), returned: 0, truncated: false },
            0,
        )
        .unwrap_err();
        assert_eq!(error.code.0, -32602);
        assert_eq!(error.data.as_ref().unwrap()["reason"], "budget_too_small");
        let minimum = error.data.unwrap()["minimum_output_tokens"].as_u64().unwrap() as usize;
        assert!(minimum > 0);
        let minimum_response = live_metadata_tree_response(
            Some("test"),
            onec_client::MetadataListResult { items: Vec::new(), returned: 0, truncated: false },
            minimum,
        )
        .unwrap();
        assert_eq!(minimum_response.structured_content.unwrap()["schema_version"], "2");
    }

    #[test]
    fn live_object_budget_never_splits_union_or_tabular_section() {
        let mut union = object_from_fixture();
        union.standard_attributes.clear();
        union.attributes.clear();
        union.dimensions.clear();
        union.resources.clear();
        union.tabular_sections.clear();
        union.attributes.push(onec_client::MetadataStructureItem {
            name: "БольшойСоставнойТип".into(),
            synonym: String::new(),
            type_name: Some("Составной".into()),
            type_variants: vec![
                onec_client::MetadataTypeVariant {
                    technical_name: Some("СправочникСсылка.А".into()),
                    presentation: "А".repeat(2_000),
                    resolution: "source",
                    reason: None,
                },
                onec_client::MetadataTypeVariant {
                    technical_name: Some("СправочникСсылка.Б".into()),
                    presentation: "Б".repeat(2_000),
                    resolution: "source",
                    reason: None,
                },
            ],
        });
        let fitted_union = live_metadata_object_response(Some("test"), union, 700).unwrap();
        assert!(live_result_payload_bytes(&fitted_union) <= 700 * 4);
        let union_body = fitted_union.structured_content.as_ref().unwrap();
        assert!(union_body["object"]["Реквизиты"].as_array().unwrap().is_empty());
        assert!(union_body["truncated"].as_bool().unwrap());
        assert_eq!(
            union_body["completeness"]["reasons"].as_array().unwrap().last().unwrap()["path"],
            ""
        );

        let mut section = object_from_fixture();
        section.standard_attributes.clear();
        section.attributes.clear();
        section.dimensions.clear();
        section.resources.clear();
        section.tabular_sections = vec![onec_client::MetadataTabularSection {
            name: "ОченьБольшаяТабличнаяЧасть".into(),
            synonym: String::new(),
            attributes: Some(vec![onec_client::MetadataStructureItem {
                name: "ОченьБольшойРеквизит".into(),
                synonym: String::new(),
                type_name: Some("Строка".into()),
                type_variants: vec![onec_client::MetadataTypeVariant {
                    technical_name: Some("Строка".into()),
                    presentation: "Строка".repeat(2_000),
                    resolution: "source",
                    reason: None,
                }],
            }]),
        }];
        let fitted_section = live_metadata_object_response(Some("test"), section, 700).unwrap();
        assert!(live_result_payload_bytes(&fitted_section) <= 700 * 4);
        let section_body = fitted_section.structured_content.as_ref().unwrap();
        assert!(section_body["object"]["ТабличныеЧасти"].as_array().unwrap().is_empty());
        assert!(section_body["truncated"].as_bool().unwrap());
        assert!(section_body["completeness"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .all(|reason| reason["code"] == "output_budget" && reason["path"] == ""));
    }

    #[test]
    fn live_service_failure_remains_an_mcp_error() {
        let error = live_metadata_error(onec_client::Error::Status {
            status: 500,
            body: "service failed".to_string(),
        });
        assert_eq!(error.code.0, -32603);
        assert!(error.message.contains("1C returned status 500"));
    }

    #[test]
    fn get_form_structure_lists_forms_under_the_given_root() {
        // The object form directory is `<root>/<TypeDir>/<object>/Forms`. Passing the repo root
        // when the configuration is nested under `src/cf` makes every form look missing — so the
        // caller must pass the configuration root. This locks that the function resolves forms
        // relative to whatever root it is handed (the config root in production).
        let tmp = tempfile::tempdir().unwrap();
        let forms = tmp.path().join("Catalogs").join("Пользователи").join("Forms");
        std::fs::create_dir_all(forms.join("ФормаСписка")).unwrap();
        std::fs::create_dir_all(forms.join("ФормаЭлемента")).unwrap();

        let result =
            get_form_structure(Some(tmp.path()), "Catalog", Some("Пользователи"), None).unwrap();
        let text = extract_text(&result);
        assert!(text.contains("ФормаСписка"), "should list ФормаСписка: {text}");
        assert!(text.contains("ФормаЭлемента"), "should list ФормаЭлемента: {text}");

        // A wrong root (the repo root, missing the `src/cf` config segment) finds nothing.
        let repo_root = tmp.path().join("repo_root_without_config");
        std::fs::create_dir_all(&repo_root).unwrap();
        assert!(
            get_form_structure(Some(&repo_root), "Catalog", Some("Пользователи"), None).is_err(),
            "a root without the object tree must error, not silently succeed",
        );
    }

    #[test]
    fn get_form_structure_lists_common_forms_without_object_name() {
        // CommonForm is a top-level form: it lives under `CommonForms/<Form>`, has no parent
        // object, and so must resolve with object_name = None (the protocol's failing case).
        let tmp = tempfile::tempdir().unwrap();
        let common = tmp.path().join("CommonForms");
        std::fs::create_dir_all(common.join("ОтправкаSMS")).unwrap();
        std::fs::create_dir_all(common.join("Настройки")).unwrap();

        let result = get_form_structure(Some(tmp.path()), "CommonForm", None, None).unwrap();
        let text = extract_text(&result);
        assert!(text.contains("ОтправкаSMS"), "should list ОтправкаSMS: {text}");
        assert!(text.contains("Настройки"), "should list Настройки: {text}");

        // The localized type name resolves to the same place.
        assert!(get_form_structure(Some(tmp.path()), "ОбщаяФорма", None, None).is_ok());
    }

    /// A "not ready" answer must be recognizable by a field, not by matching the sentence:
    /// an unrecognized retry envelope is read as content, and an object that exists then
    /// looks absent. The sentence stays in the text block for people and for consumers that
    /// matched it before the envelope existed.
    #[test]
    fn loading_carries_a_machine_readable_envelope_beside_the_sentence() {
        let report = crate::diagnostics_state::StatusReport {
            state: "loading",
            generation: 0,
            files: None,
            unread_files: None,
            reload: "none",
            error: None,
            elapsed_ms: Some(20),
            failed_age_ms: None,
            attempts: 0,
            retry_after_ms: None,
            watch: None,
        };

        let result = loading(&report);
        let body =
            result.structured_content.as_ref().expect("loading answers with structuredContent");

        assert_eq!(body["status"], "loading");
        assert_eq!(body["state"], "loading");
        assert_eq!(body["elapsed_ms"], 20);
        assert!(body["detail"].as_str().is_some_and(|d| !d.is_empty()));
        assert!(
            extract_text(&result).starts_with("Метаданные загружаются"),
            "the human sentence stays the text block: {}",
            extract_text(&result),
        );
    }

    #[test]
    fn get_form_structure_requires_object_name_for_object_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let err = get_form_structure(Some(tmp.path()), "Catalog", None, None).unwrap_err();
        assert!(
            format!("{err:?}").contains("object_name"),
            "object forms must still require object_name: {err:?}",
        );
    }

    #[test]
    fn test_metadata_tree_summary() {
        let config = fixture_config();
        let result =
            get_metadata_tree(&ide::RootDatabaseImpl::new(), &config, &[], None, usize::MAX)
                .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("дерево метаданных"), "should have header");
        assert!(text.contains("Всего объектов:"), "should have total");
        assert!(text.contains("Справочник"), "should list catalogs");
        assert!(text.contains("Документ"), "should list documents");
    }

    #[test]
    fn test_metadata_tree_filter_catalogs() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("Справочник".into()),
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("Справочник"), "should have category name");
        assert!(text.contains("Справочник1"), "should list Справочник1");
    }

    #[test]
    fn filter_category_accepts_plural_and_overqualified_aliases() {
        // English directory/plural form.
        assert_eq!(normalize_filter_category("Catalogs"), "Catalog");
        assert_eq!(normalize_filter_category("Documents"), "Document");
        // Russian plural.
        assert_eq!(normalize_filter_category("Справочники"), "Справочник");
        assert_eq!(normalize_filter_category("Отчёты"), "Отчет");
        // Over-qualified with an object name — keep the category head.
        assert_eq!(normalize_filter_category("Справочник.Пользователи"), "Справочник");
        // The canonical singular passes through untouched.
        assert_eq!(normalize_filter_category("Справочник"), "Справочник");
        // A non-plural unknown is left for the parser / special-case match to handle.
        assert_eq!(normalize_filter_category("ОбщиеМодули"), "ОбщиеМодули");
    }

    #[test]
    fn test_metadata_tree_filter_plural_and_dotted_resolve() {
        let config = fixture_config();
        for input in ["Catalogs", "Справочники", "Справочник.Справочник1"]
        {
            let result = get_metadata_tree(
                &ide::RootDatabaseImpl::new(),
                &config,
                &[],
                Some(input.into()),
                usize::MAX,
            )
            .unwrap();
            let text = extract_text(&result);
            assert!(
                text.contains("Справочник1"),
                "filter `{input}` should list Справочник1: {text}"
            );
        }
    }

    #[test]
    fn test_metadata_tree_filter_common_modules() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("ОбщиеМодули".into()),
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("ОбщиеМодули"), "should have category name");
    }

    #[test]
    fn test_metadata_tree_filter_invalid() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("НесуществующаяКатегория".into()),
            usize::MAX,
        );

        assert!(result.is_err(), "should return error for unknown category");
    }

    #[test]
    fn test_object_structure_catalog() {
        let config = fixture_config();
        let result = get_object_structure(&config, "Catalog", "Справочник1").unwrap();
        let text = extract_text(&result);

        assert!(text.contains("Справочник.Справочник1"), "should have object header");
    }

    #[test]
    fn test_object_structure_event_subscription() {
        // Regression: `ПодпискаНаСобытие` used to fail to parse as an MDO type ("Unknown
        // MDO type"); it now resolves and reports its source/event/handler.
        let config = fixture_config();
        let result =
            get_object_structure(&config, "ПодпискаНаСобытие", "ПриЗаписиСправочника").unwrap();
        let text = extract_text(&result);
        assert!(text.contains("ПодпискаНаСобытие.ПриЗаписиСправочника"), "header");
        assert!(text.contains("Событие: OnWrite"), "event");
        assert!(
            text.contains("Обработчик: CommonModule.ОбщийПодпискиНаСобытия.ПриЗаписиСправочника"),
            "handler string"
        );
        assert!(text.contains("Метод: ПриЗаписиСправочника"), "parsed handler method");
    }

    #[test]
    fn test_object_structure_event_subscription_not_found_is_graceful() {
        let config = fixture_config();
        // A missing subscription is an in-band invalid-params error, not a parse crash.
        assert!(get_object_structure(&config, "ПодпискаНаСобытие", "НетТакой").is_err());
    }

    #[test]
    fn test_object_structure_not_found() {
        let config = fixture_config();
        let result = get_object_structure(&config, "Catalog", "НесуществующийОбъект");

        assert!(result.is_err(), "should return error for missing object");
    }

    #[test]
    fn test_object_structure_invalid_type() {
        let config = fixture_config();
        let result = get_object_structure(&config, "InvalidType", "Test");

        assert!(result.is_err(), "should return error for invalid type");
    }

    #[test]
    fn test_configuration_info() {
        let config = fixture_config();
        let result = get_configuration_info(&config, &[]).unwrap();
        let text = extract_text(&result);

        assert!(text.contains("# Конфигурация:"), "should have config header");
        assert!(text.contains("UUID:"), "should have UUID");
        assert!(text.contains("Общие модули:"), "should have common modules count");
        assert!(text.contains("Регистры:"), "should have registers count");
    }

    #[test]
    fn test_form_structure_list_forms() {
        let fixture_root =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
        let root = std::path::Path::new(fixture_root);
        let result = get_form_structure(Some(root), "Document", Some("Документ1"), None).unwrap();
        let text = extract_text(&result);

        assert!(text.contains("Формы Document.Документ1"), "should have forms header");
        assert!(text.contains("ФормаДокумента"), "should list document form");
        assert!(text.contains("ФормаСписка"), "should list list form");
    }

    #[test]
    fn test_form_structure_specific_form() {
        let fixture_root =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
        let root = std::path::Path::new(fixture_root);
        let result =
            get_form_structure(Some(root), "Document", Some("Документ1"), Some("ФормаДокумента"))
                .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("# Форма:"), "should have form header");
        // The heading carries a name (parsed, or the requested form name as fallback when
        // Ext/Form.xml omits it), never a blank title.
        assert!(!text.contains("# Форма: \n"), "heading must not be blank: {text}");
        // A nil UUID (the common case — Form.xml carries no UUID) is labelled, never printed as
        // a misleading all-zero UUID.
        assert!(
            !text.contains("00000000-0000-0000-0000-000000000000"),
            "nil UUID must be labelled, not printed as zeros: {text}",
        );
    }

    #[test]
    fn test_form_structure_no_workspace() {
        let result = get_form_structure(None, "Catalog", Some("Test"), None);
        assert!(result.is_err(), "should fail without workspace root");
    }

    #[test]
    fn test_form_structure_form_not_found() {
        let fixture_root =
            concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer");
        let root = std::path::Path::new(fixture_root);
        let result = get_form_structure(
            Some(root),
            "Document",
            Some("Документ1"),
            Some("НесуществующаяФорма"),
        );

        assert!(result.is_err(), "should fail for missing form");
    }

    // ===== Wave 2d: explicit service enumerate/object/config-info =====
    //
    // These pin the contract that the MCP metadata tools enumerate HTTP/Web/Integration
    // services as explicit category listings and produce structured object dumps, instead
    // of forcing the agent through the hot typed config resolver. They RED-fail until
    // `format_filtered_tree` grows HTTP/Web/Integration-service branches and
    // `get_object_structure` recognises the service MDO types.

    #[test]
    fn metadata_tree_filter_lists_http_services_explicitly() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("HTTPСервис".into()),
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("HTTPСервис1"), "filter must enumerate http services: {text}");
        assert!(
            text.contains("HTTP") && text.contains("Сервис"),
            "filter must label the http-service category explicitly: {text}"
        );
    }

    #[test]
    fn metadata_tree_filter_lists_web_services_explicitly() {
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("WebСервис".into()),
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(text.contains("WebСервис1"), "filter must enumerate web services: {text}");
    }

    #[test]
    fn metadata_tree_filter_lists_integration_services_explicitly() {
        // IntegrationService is deliberately absent from `MetadataReferenceKind` (no
        // `Метаданные.СервисыИнтеграции` plural), but the MCP enumeration surface must
        // still list integration services as a discrete category for agent discovery.
        let config = fixture_config();
        let result = get_metadata_tree(
            &ide::RootDatabaseImpl::new(),
            &config,
            &[],
            Some("СервисИнтеграции".into()),
            usize::MAX,
        )
        .unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("ОбменСообщениями"),
            "filter must enumerate integration services: {text}"
        );
    }

    #[test]
    fn object_structure_dumps_http_service_explicitly() {
        let config = fixture_config();
        let result = get_object_structure(&config, "HTTPService", "HTTPСервис1").unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("HTTPService.HTTPСервис1") || text.contains("HTTPСервис1"),
            "object structure must name the http service: {text}"
        );
        assert!(
            text.contains("URLTemplate") || text.contains("Метод") || text.contains("RootURL"),
            "object structure must dump http-service shape (templates/methods/root): {text}"
        );
    }

    #[test]
    fn object_structure_dumps_web_service_explicitly() {
        let config = fixture_config();
        let result = get_object_structure(&config, "WebService", "WebСервис1").unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("WebService.WebСервис1") || text.contains("WebСервис1"),
            "object structure must name the web service: {text}"
        );
        assert!(
            text.contains("Операция") || text.contains("Namespace"),
            "object structure must dump web-service shape (operations/namespace): {text}"
        );
    }

    #[test]
    fn object_structure_dumps_integration_service_explicitly() {
        let config = fixture_config();
        let result =
            get_object_structure(&config, "IntegrationService", "ОбменСообщениями").unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("IntegrationService.ОбменСообщениями")
                || text.contains("ОбменСообщениями"),
            "object structure must name the integration service: {text}"
        );
        assert!(
            text.contains("Канал") || text.contains("Channel"),
            "object structure must dump integration-service channels: {text}"
        );
    }

    #[test]
    fn configuration_info_reports_http_and_web_service_counts_explicitly() {
        let config = fixture_config();
        let result = get_configuration_info(&config, &[]).unwrap();
        let text = extract_text(&result);

        assert!(
            text.contains("HTTP-сервисы:") && !text.contains("HTTP-сервисы: 0"),
            "configuration info must report non-zero http-service count: {text}"
        );
        assert!(
            text.contains("Web-сервисы:") && !text.contains("Web-сервисы: 0"),
            "configuration info must report non-zero web-service count: {text}"
        );
    }

    /// The force-rescan retry gate predicate must accept every type `object_from_db` can
    /// dispatch — including HTTP/Web/Integration services, which are NOT `MdoType` variants
    /// (they route through `parse_service_kind`). Gating on `MdoType::from_str` alone (the
    /// bug) would classify a service miss as non-retryable, so a just-added service would
    /// never force a re-scan.
    #[test]
    fn is_resolvable_object_type_covers_services_not_in_mdotype() {
        for t in
            ["HTTPService", "WebService", "IntegrationService", "httpсервис", "сервисинтеграции"]
        {
            assert!(
                t.parse::<MdoType>().is_err(),
                "{t} is deliberately not an MdoType — this is what the old gate missed",
            );
            assert!(
                is_resolvable_object_type(t),
                "{t} must be resolvable (dispatched via parse_service_kind)",
            );
        }
        for t in ["Catalog", "InformationRegister", "ПодпискаНаСобытие", "Справочник"]
        {
            assert!(is_resolvable_object_type(t), "{t} (an MdoType) must stay resolvable");
        }
        assert!(
            !is_resolvable_object_type("НеизвестныйТип"),
            "a genuinely unknown type must not be classified resolvable (no wasted force-scan)",
        );
    }
}

#[cfg(test)]
mod form_probe_case_tests {
    #[test]
    fn a_case_variant_form_xml_is_found_by_the_tool_probe() {
        let dir = tempfile::tempdir().unwrap();
        let container = dir.path().join("Forms");
        std::fs::create_dir_all(container.join("Ф/EXT")).unwrap();
        std::fs::write(container.join("Ф/EXT/FORM.XML"), "<Form/>").unwrap();
        let result = super::forms_in_container(&container, Some("Ф"), "Тест");
        assert!(result.is_ok(), "форма с EXT/FORM.XML читается: {result:?}");
    }
}
