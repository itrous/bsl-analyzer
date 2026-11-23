use super::*;

use bsl_conventions::{ConventionalName, XML_EXTENSION};

use bsl_metadata::inspection_sources::InspectionIdentity;
use serde_json::{json, Value};

fn segment(value: &str) -> Result<&str, McpError> {
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || matches!(value, "." | "..")
    {
        return Err(McpError::invalid_params(
            "Expected one metadata name without path separators",
            None,
        ));
    }
    Ok(value)
}

fn container(object_type: &str, object_name: Option<&str>, form: bool) -> Result<String, McpError> {
    let common = if form {
        matches!(object_type.to_lowercase().as_str(), "commonform" | "общаяформа")
    } else {
        matches!(object_type.to_lowercase().as_str(), "commontemplate" | "общиймакет")
    };
    if common {
        return Ok(if form { "CommonForms" } else { "CommonTemplates" }.into());
    }
    let directory = mdo_type_to_dir(object_type)
        .ok_or_else(|| McpError::invalid_params("Unsupported metadata owner type", None))?;
    let name = segment(object_name.ok_or_else(|| {
        McpError::invalid_params("object_name is required for an object's forms/templates", None)
    })?)?;
    Ok(format!(
        "{directory}/{name}/{}",
        if form { ConventionalName::Forms.canonical() } else { "Templates" }
    ))
}

pub fn form_from_db(
    db: &ide::RootDatabaseImpl,
    root_id: Option<&str>,
    object_type: &str,
    object_name: Option<&str>,
    form_name: Option<&str>,
) -> Result<CallToolResult, McpError> {
    let roots = db.inspection_roots(root_id).map_err(|e| McpError::invalid_params(e, None))?;
    let container = container(object_type, object_name, true)?;
    let Some(name) = form_name else {
        return Ok(listing(db, &roots, &container, "forms"));
    };
    let name = segment(name)?;
    let composition = db
        .metadata_inspect_form(roots, &container, name)
        .map_err(|error| McpError::internal_error(error, None))?;
    let mut reasons = composition
        .inspection_reasons()
        .map(|(code, origin, detail)| match detail {
            Some(detail) => json!({"code":code,"origin":origin,"detail":detail}),
            None => json!({"code":code,"origin":origin}),
        })
        .collect::<Vec<_>>();
    let Some((identity, view)) = composition.inspection() else {
        if reasons.is_empty() {
            return Err(McpError::invalid_params(
                format!("Form {name} not found in selected source scope"),
                None,
            ));
        }
        return Ok(card(
            format!("# Форма: {name}\n\nСодержимое недоступно."),
            json!({"name":name,"form":null,"root_id":root_id.unwrap_or("")}),
            reasons,
        ));
    };
    if view.parameters.is_none() {
        reasons.push(json!({"code":"form_parameters_unread","path":"/object/form/parameters"}));
    }
    if view.attributes.is_none() {
        reasons.push(json!({"code":"form_attributes_unread","path":"/object/form/attributes"}));
    }
    for (index, parameter) in view.parameters.as_deref().unwrap_or_default().iter().enumerate() {
        let path = format!("/object/form/parameters/{index}/attr_type");
        if let Some(ty) = &parameter.attr_type {
            let mut type_reasons = Vec::new();
            collect_type_reasons(ty, &path, &mut type_reasons);
            reasons.extend(
                type_reasons.into_iter().map(|(path, code)| json!({"code":code,"path":path})),
            );
        } else {
            reasons.push(json!({"code":"parameter_type_unread","path":path}));
        }
    }
    for (index, attribute) in view.attributes.as_deref().unwrap_or_default().iter().enumerate() {
        let path = format!("/object/form/attributes/{index}/attr_type");
        if let Some(ty) = &attribute.attr_type {
            let mut type_reasons = Vec::new();
            collect_type_reasons(ty, &path, &mut type_reasons);
            reasons.extend(
                type_reasons.into_iter().map(|(path, code)| json!({"code":code,"path":path})),
            );
        } else {
            reasons.push(json!({"code":"attribute_type_unread","path":path}));
        }
    }
    let mut text = format!(
        "# Форма: {name}\n\nТип: {}\nUUID: {}\n",
        view.form_type,
        if identity.uuid.is_nil() {
            "недоступен".into()
        } else {
            identity.uuid.to_string()
        }
    );
    text.push_str("\n## Параметры формы\n");
    match &view.parameters {
        None => text.push_str("unknown\n"),
        Some(parameters) => {
            for parameter in parameters {
                let _ = writeln!(
                    text,
                    "- {}: {} [{}]",
                    parameter.name,
                    parameter
                        .attr_type
                        .as_ref()
                        .map(format_source_type)
                        .unwrap_or_else(|| "unknown".into()),
                    parameter.origin
                );
            }
        }
    };
    text.push_str("\n## Реквизиты формы\n");
    match &view.attributes {
        None => text.push_str("unknown\n"),
        Some(attributes) => {
            for attribute in attributes {
                let _ = writeln!(
                    text,
                    "- {}: {} [{}]\n  Choice/fill: {}",
                    attribute.path.join("."),
                    attribute
                        .attr_type
                        .as_ref()
                        .map(format_source_type)
                        .unwrap_or_else(|| "unknown".into()),
                    attribute.type_origin.as_deref().unwrap_or("unknown"),
                    json!(attribute.properties)
                );
            }
        }
    }
    text.push_str("\n## Элементы формы\n");
    for element in &view.elements {
        let _ = writeln!(
            text,
            "- {} ({}, id={}, parent={:?}, page={:?}) DataPath={} [{}]",
            element.name,
            element.kind,
            element.id,
            element.parent_id,
            element.page,
            element.data_path.as_deref().unwrap_or("unknown"),
            element.origin
        );
        let _ = writeln!(text, "  Choice/fill: {}", json!(element.properties));
    }
    text.push_str("\n## Привязки\n");
    for binding in &view.bindings {
        let _ = writeln!(
            text,
            "- {} {:?}.{} → {} ({}) [{}]",
            binding.owner_kind,
            binding.owner,
            binding.event,
            binding.procedure,
            binding.call_type.as_deref().unwrap_or("normal"),
            binding.origin
        );
    }
    if !reasons.is_empty() {
        let _ = writeln!(text, "\nПолнота: partial; {}", json!(reasons));
    }
    Ok(card(
        text,
        json!({"name":name,"identity":identity,"form":view,"root_id":root_id.unwrap_or("")}),
        reasons,
    ))
}

fn listing(
    db: &ide::RootDatabaseImpl,
    roots: &[(String, std::path::PathBuf)],
    container: &str,
    category: &str,
) -> CallToolResult {
    let mut objects = Vec::new();
    let mut text = format!("# {category}\n\n");
    for (label, root) in roots {
        for name in db.inspection_names(root, container) {
            let _ = writeln!(text, "- {name} [{label}]");
            objects.push(json!({"name":name,"root_id":label}));
        }
    }
    card(text, json!({"name":category,"items":objects}), vec![])
}

pub fn template_from_db(
    db: &ide::RootDatabaseImpl,
    root_id: Option<&str>,
    object_type: &str,
    object_name: Option<&str>,
    template_name: Option<&str>,
) -> Result<CallToolResult, McpError> {
    let roots = db.inspection_roots(root_id).map_err(|e| McpError::invalid_params(e, None))?;
    let container = container(object_type, object_name, false)?;
    let Some(name) = template_name else {
        return Ok(listing(db, &roots, &container, "templates"));
    };
    let name = segment(name)?;
    let mut result = None;
    for (label, root) in &roots {
        let Some(descriptor) =
            db.inspection_file(root, &format!("{container}/{name}.{XML_EXTENSION}"))
        else {
            continue;
        };
        let identity = InspectionIdentity::from_xml(
            &descriptor,
            if container == "CommonTemplates" { "CommonTemplate" } else { "Template" },
        )
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let origin = format!("{label}:{container}/{name}");
        let content = db.inspection_file(
            root,
            &format!(
                "{container}/{name}/{}/{}",
                ConventionalName::Ext.canonical(),
                ConventionalName::TemplateXml.canonical()
            ),
        );
        let mut reasons = Vec::new();
        let value = match (identity.template_type.as_deref(), content) {
            (Some("DataCompositionSchema"), Some(text)) => {
                bsl_metadata::xml_parser::content_inspection::parse_composition_inspection(
                    &text, &origin,
                )
                .map(|value| json!({"composition":value}))
            }
            (Some("SpreadsheetDocument"), Some(text)) => {
                bsl_metadata::xml_parser::content_inspection::parse_spreadsheet_inspection(
                    &text, &origin,
                )
                .map(|value| json!({"spreadsheet":value}))
            }
            (kind, _) => {
                reasons.push(json!({
                    "code":"template_representation_unavailable",
                    "template_type":kind,
                    "origin":origin,
                }));
                Ok(Value::Null)
            }
        }
        .unwrap_or_else(|error| {
            reasons.push(json!({
                "code":"template_content_unread",
                "detail":error.to_string(),
                "origin":origin,
            }));
            Value::Null
        });
        if let Some(paths) =
            value.pointer("/composition/unsupported_paths").and_then(Value::as_array)
        {
            if !paths.is_empty() {
                reasons.push(
                    json!({"code":"composition_content_unsupported","paths":paths,"origin":origin}),
                );
            }
        }
        let text = format!(
            "# Макет: {name}\n\nИсточник: {origin}\nТип: {}\n\n{}\n",
            identity.template_type.as_deref().unwrap_or("unknown"),
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
        result = Some(card(
            text,
            json!({"name":name,"identity":identity,"content":value,"origin":origin}),
            reasons,
        ));
    }
    result.ok_or_else(|| {
        McpError::invalid_params(
            format!("Template {name} not found in selected source scope"),
            None,
        )
    })
}

pub fn role_from_db(
    db: &ide::RootDatabaseImpl,
    root_id: Option<&str>,
    name: &str,
) -> Result<CallToolResult, McpError> {
    segment(name)?;
    let roots = db.inspection_roots(root_id).map_err(|e| McpError::invalid_params(e, None))?;
    let selected =
        roots.last().ok_or_else(|| McpError::invalid_params("No metadata roots", None))?;
    let (label, root) = selected;
    let descriptor =
        db.inspection_file(root, &format!("Roles/{name}.{XML_EXTENSION}")).ok_or_else(|| {
            McpError::invalid_params(format!("Role {name} not found in root {label}"), None)
        })?;
    let identity = InspectionIdentity::from_xml(&descriptor, "Role")
        .map_err(|e| McpError::internal_error(e.to_string(), None))?;
    let origin = format!("{label}:Roles/{name}");
    let mut reasons = Vec::new();
    let rights = db
        .inspection_file(
            root,
            &format!(
                "Roles/{name}/{}/{}",
                ConventionalName::Ext.canonical(),
                ConventionalName::RightsXml.canonical()
            ),
        )
        .and_then(
            |text| match bsl_metadata::xml_parser::content_inspection::parse_role_inspection(
                &text, &origin,
            ) {
                Ok(value) => Some(value),
                Err(error) => {
                    reasons.push(json!({"code":"rights_unread","detail":error.to_string()}));
                    None
                }
            },
        );
    if rights.is_none() && reasons.is_empty() {
        reasons.push(json!({"code":"rights_unavailable"}));
    }
    if let Some(rights) = &rights {
        if !rights.unsupported_paths.is_empty() {
            reasons.push(json!({"code":"rights_content_unsupported","paths":rights.unsupported_paths,"origin":origin}));
        }
    }
    let text = format!(
        "# Роль: {name}\n\nИсточник: {origin}\nЭто объявленные права роли.\n\n{}\n",
        serde_json::to_string_pretty(&rights).unwrap_or_default()
    );
    Ok(card(
        text,
        json!({"name":name,"identity":identity,"rights":rights,"origin":origin}),
        reasons,
    ))
}

fn card(text: String, object: Value, reasons: Vec<Value>) -> CallToolResult {
    let status = if reasons.is_empty() { "complete" } else { "partial" };
    crate::tools::response::structured_with_text(
        text,
        json!({
            "schema_version": "2",
            "source": "source",
            "object": object,
            "completeness": {"status": status, "reasons": reasons},
            "truncated": false,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_db::{SourceDatabase, SourceRoot, METADATA_SOURCE_ROOT};
    use bsl_metadata::inspection_sources::BinaryFingerprint;
    use vfs::{file_set::FileSet, FileId, VfsPath};

    fn database(files: &[(&str, &str)]) -> ide::RootDatabaseImpl {
        let mut db = ide::RootDatabaseImpl::new();
        db.set_all_config_paths(
            vec![
                (None, "/inspection/base".into()),
                (Some("Extension".into()), "/inspection/ext".into()),
                (Some("Other".into()), "/inspection/other".into()),
            ],
            None,
        );
        let mut set = FileSet::new();
        let mut fingerprints = Vec::new();
        for (index, (path, text)) in files.iter().enumerate() {
            if path.ends_with("Form.bin") {
                let value = serde_json::from_str::<
                    bsl_metadata::inspection_sources::BinaryFingerprint,
                >(text)
                .unwrap();
                fingerprints
                    .push((std::path::PathBuf::from(format!("/inspection/{path}")), Some(value)));
                continue;
            }
            let id = FileId::from_raw(index as u32 + 100);
            set.insert(id, VfsPath::new(std::path::PathBuf::from(format!("/inspection/{path}"))));
            db.set_file_source_root(id, METADATA_SOURCE_ROOT);
            db.set_file_text(id, text);
        }
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(set));
        if !fingerprints.is_empty() {
            db.set_ordinary_form_fingerprints(std::sync::Arc::new(fingerprints));
        }
        db
    }

    fn descriptor(kind: &str, name: &str, uuid: &str, properties: &str) -> String {
        format!("<MetaDataObject xmlns='http://v8.1c.ru/8.3/MDClasses'><{kind} uuid='{uuid}'><Properties><Name>{name}</Name>{properties}</Properties></{kind}></MetaDataObject>")
    }

    #[test]
    fn unknown_extension_form_keeps_only_proven_content() {
        let base = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000001",
            "<FormType>Managed</FormType>",
        );
        let form = "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'>
            <ChildItems><InputField name='BaseField' id='1'/></ChildItems>
            <Parameters/><Attributes/>
        </Form>";
        let fragment = form.replace("BaseField", "ExtensionField");
        for (has_base, ownership, has_base_snapshot) in [
            (true, Some("<ObjectBelonging>Unknown</ObjectBelonging>"), false),
            (false, Some("<ObjectBelonging/>"), false),
            (
                true,
                Some("<ExtendedConfigurationObject>invalid</ExtendedConfigurationObject>"),
                false,
            ),
            (true, Some(""), true),
            (true, None, false),
        ] {
            let extension = ownership.map(|ownership| {
                descriptor(
                    "CommonForm",
                    "Dialog",
                    "10000000-0000-0000-0000-000000000002",
                    &format!("<FormType>Managed</FormType>{ownership}"),
                )
            });
            let fragment = if has_base_snapshot {
                fragment.replace("</Form>", "<BaseForm/></Form>")
            } else {
                fragment.clone()
            };
            let mut files = vec![("ext/CommonForms/Dialog/Ext/Form.xml", fragment.as_str())];
            if has_base {
                files.extend([
                    ("base/CommonForms/Dialog.xml", base.as_str()),
                    ("base/CommonForms/Dialog/Ext/Form.xml", form),
                ]);
            }
            if let Some(extension) = &extension {
                files.push(("ext/CommonForms/Dialog.xml", extension));
            }
            let db = database(&files);
            let body = form_from_db(&db, Some("Extension"), "CommonForm", None, Some("Dialog"))
                .unwrap()
                .structured_content
                .unwrap();
            if has_base {
                assert_eq!(body["object"]["form"]["elements"][0]["name"], "BaseField");
                assert_eq!(body["object"]["form"]["elements"].as_array().unwrap().len(), 1);
            } else {
                assert!(body["object"]["form"].is_null());
            }
            let mut reasons = vec![json!({
                "code":"adopted_base_identity_unavailable",
                "origin":"Extension:CommonForms/Dialog"
            })];
            if extension.is_none() {
                reasons.push(json!({
                    "code":"form_descriptor_unavailable",
                    "origin":"Extension:CommonForms/Dialog"
                }));
            }
            assert_eq!(body["completeness"]["reasons"], json!(reasons));
            assert_eq!(body["completeness"]["status"], "partial");
        }
    }

    #[test]
    fn unread_extension_descriptor_without_content_is_partial() {
        let base = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000001",
            "<FormType>Managed</FormType>",
        );
        let extension = base.replace("000000000001", "000000000002");
        let form = "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'>
            <ChildItems><InputField name='BaseField' id='1'/></ChildItems>
            <Parameters/><Attributes/>
        </Form>";
        let read = |has_base| {
            let mut files = vec![("ext/CommonForms/Dialog.xml", extension.as_str())];
            if has_base {
                files.extend([
                    ("base/CommonForms/Dialog.xml", base.as_str()),
                    ("base/CommonForms/Dialog/Ext/Form.xml", form),
                ]);
            }
            let mut db = database(&files);
            db.set_file_unreadable(FileId::from_raw(100));
            form_from_db(&db, Some("Extension"), "CommonForm", None, Some("Dialog"))
                .map(|result| result.structured_content.unwrap())
        };
        let with_base = read(true);
        let without_base = read(false);
        assert_eq!(
            [with_base.as_ref().ok(), without_base.as_ref().ok()]
                .map(|body| body.map(|body| &body["completeness"]["status"])),
            [Some(&json!("partial")), Some(&json!("partial"))]
        );
        let with_base = with_base.unwrap();
        let without_base = without_base.unwrap();
        assert_form_elements(&with_base, &["BaseField"]);
        assert!(without_base["object"]["form"].is_null());
        for body in [with_base, without_base] {
            let reasons = body["completeness"]["reasons"].as_array().unwrap();
            for code in ["adopted_base_identity_unavailable", "form_descriptor_unavailable"] {
                assert!(reasons.contains(&json!({
                    "code": code,
                    "origin": "Extension:CommonForms/Dialog"
                })));
            }
        }
    }

    #[test]
    fn orphan_form_fragment_cannot_supply_the_base_for_a_later_extension() {
        let extension = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000002",
            concat!(
                "<FormType>Managed</FormType><ObjectBelonging>Adopted</ObjectBelonging>",
                "<ExtendedConfigurationObject>10000000-0000-0000-0000-000000000001",
                "</ExtendedConfigurationObject>"
            ),
        );
        let other = extension.replace("000000000002", "000000000003");
        let fragment = "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'>
            <BaseForm/><ChildItems><InputField name='First' id='1'/></ChildItems>
            <Parameters/><Attributes/>
        </Form>";
        let later = fragment.replace("'First' id='1'", "'Second' id='2'");
        let mut db = database(&[
            ("ext/CommonForms/Dialog.xml", &extension),
            ("ext/CommonForms/Dialog/Ext/Form.xml", fragment),
            ("other/CommonForms/Dialog.xml", &other),
            ("other/CommonForms/Dialog/Ext/Form.xml", &later),
        ]);
        let mut scope = db.workspace_configs_snapshot().as_ref().clone();
        scope.closures[2] = vec![1];
        db.set_workspace_configs_snapshot(scope);
        let read = |db: &ide::RootDatabaseImpl, root: &str| {
            form_from_db(db, Some(root), "CommonForm", None, Some("Dialog"))
                .unwrap()
                .structured_content
                .unwrap()
        };
        assert_form_elements(&read(&db, "Extension"), &["First"]);
        let body = read(&db, "Other");
        assert_form_elements(&body, &["Second"]);
        assert_eq!(
            body["completeness"]["reasons"][0],
            json!({"code":"adopted_base_identity_unavailable","origin":"Other:CommonForms/Dialog"})
        );

        db.set_file_text(
            FileId::from_raw(102),
            &descriptor(
                "CommonForm",
                "Dialog",
                "10000000-0000-0000-0000-000000000003",
                "<FormType>Managed</FormType><ObjectBelonging>Own</ObjectBelonging>",
            ),
        );
        let body = read(&db, "Other");
        assert!(body["object"]["form"].is_null());
        assert_eq!(body["completeness"]["reasons"][0]["code"], "adopted_base_identity_unavailable");
    }

    #[test]
    fn form_composition_tracks_vfs_contents_and_visible_root_chain() {
        let base = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000001",
            "<FormType>Managed</FormType>",
        );
        let extension = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000002",
            concat!(
                "<FormType>Managed</FormType><ObjectBelonging>Adopted</ObjectBelonging>",
                "<ExtendedConfigurationObject>10000000-0000-0000-0000-000000000001",
                "</ExtendedConfigurationObject>"
            ),
        );
        let form = "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'>
            <ChildItems><InputField name='Base' id='1'/></ChildItems>
            <Parameters/><Attributes/>
        </Form>";
        let overlay = form.replace("'Base' id='1'", "'Extension' id='2'");
        let mut db = database(&[
            ("base/CommonForms/Dialog.xml", &base),
            ("base/CommonForms/Dialog/Ext/Form.xml", form),
            ("ext/CommonForms/Dialog.xml", &extension),
            ("ext/CommonForms/Dialog/Ext/Form.xml", &overlay),
        ]);
        let read = |db: &ide::RootDatabaseImpl| {
            form_from_db(db, Some("Other"), "CommonForm", None, Some("Dialog"))
                .unwrap()
                .structured_content
                .unwrap()
        };
        assert_form_elements(&read(&db), &["Base"]);

        let mut scope = db.workspace_configs_snapshot().as_ref().clone();
        scope.closures[2] = vec![1];
        db.set_workspace_configs_snapshot(scope.clone());
        assert_form_elements(&read(&db), &["Base", "Extension"]);

        db.set_file_text(FileId::from_raw(103), &overlay.replace("Extension", "Revised"));
        assert_form_elements(&read(&db), &["Base", "Revised"]);

        db.set_file_text(
            FileId::from_raw(102),
            &descriptor(
                "CommonForm",
                "Dialog",
                "10000000-0000-0000-0000-000000000002",
                "<FormType>Managed</FormType>",
            ),
        );
        let body = read(&db);
        assert_form_elements(&body, &["Revised"]);
        assert_eq!(body["completeness"]["status"], "complete");

        let source = db.try_source_root_input(METADATA_SOURCE_ROOT).unwrap();
        let mut files = source.root(&db).file_set().clone();
        files.remove(FileId::from_raw(103));
        db.set_source_root(METADATA_SOURCE_ROOT, SourceRoot::new_metadata(files));
        let body = read(&db);
        assert!(body["object"]["form"].is_null());
        assert_eq!(body["completeness"]["reasons"][0]["code"], "form_content_unavailable");

        scope.closures[2].clear();
        db.set_workspace_configs_snapshot(scope);
        assert_form_elements(&read(&db), &["Base"]);
    }

    fn assert_form_elements(body: &Value, expected: &[&str]) {
        let elements = body["object"]["form"]["elements"].as_array().unwrap();
        assert_eq!(
            elements.iter().map(|element| element["name"].as_str().unwrap()).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn form_composition_tracks_initial_and_changed_ordinary_fingerprints() {
        let descriptor = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000001",
            "<FormType>Ordinary</FormType>",
        );
        let sidecar = include_str!("../../../../bsl-metadata/fixtures/inspection/ordinary.json");
        let mut db = database(&[
            ("base/CommonForms/Dialog.xml", &descriptor),
            ("base/CommonForms/Dialog/Ext/Form/Form.json", sidecar),
        ]);
        let read = |db: &ide::RootDatabaseImpl| {
            form_from_db(db, None, "CommonForm", None, Some("Dialog"))
                .unwrap()
                .structured_content
                .unwrap()
        };
        let fingerprint = |db: &mut ide::RootDatabaseImpl, hash: &str, stale| {
            db.set_ordinary_form_fingerprints(std::sync::Arc::new(vec![(
                "/inspection/base/CommonForms/Dialog/Ext/Form.bin".into(),
                Some(BinaryFingerprint { sha256: hash.repeat(64), legacy_sidecar_stale: stale }),
            )]));
        };

        assert!(read(&db)["object"]["form"].is_null());
        fingerprint(&mut db, "a", Some(true));
        assert_eq!(read(&db)["object"]["form"]["freshness"], "sha256");
        fingerprint(&mut db, "b", Some(true));
        assert!(read(&db)["object"]["form"].is_null());

        let mut legacy: Value = serde_json::from_str(sidecar).unwrap();
        legacy.as_object_mut().unwrap().remove("source");
        db.set_file_text(FileId::from_raw(101), &legacy.to_string());
        assert_eq!(
            read(&db)["completeness"]["reasons"][0],
            json!({
                "code":"form_content_unavailable",
                "origin":":CommonForms/Dialog",
                "detail":"ordinary_sidecar_stale"
            })
        );
        fingerprint(&mut db, "b", Some(false));
        assert_eq!(read(&db)["object"]["form"]["freshness"], "mtime_only");
    }

    #[test]
    fn source_form_uses_selected_root_adoption_and_current_event_slots() {
        let base = descriptor(
            "Form",
            "Dialog",
            "10000000-0000-0000-0000-000000000001",
            "<FormType>Managed</FormType>",
        );
        let ext = descriptor(
            "Form",
            "Dialog",
            "10000000-0000-0000-0000-000000000002",
            concat!(
                "<FormType>Managed</FormType><ObjectBelonging>Adopted</ObjectBelonging>",
                "<ExtendedConfigurationObject>10000000-0000-0000-0000-000000000001",
                "</ExtendedConfigurationObject>"
            ),
        );
        let db = database(&[
            ("base/Documents/Order/Forms/Dialog.xml", &base),
            (
                "base/Documents/Order/Forms/Dialog/Ext/Form.xml",
                include_str!("../../../../bsl-metadata/fixtures/inspection/managed-base.xml"),
            ),
            ("ext/Documents/Order/Forms/Dialog.xml", &ext),
            (
                "ext/Documents/Order/Forms/Dialog/Ext/Form.xml",
                include_str!("../../../../bsl-metadata/fixtures/inspection/managed.xml"),
            ),
            (
                "other/Documents/Order/Forms/Dialog/Ext/Form.xml",
                "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'>
                    <ChildItems><InputField name='Unrelated' id='55'/></ChildItems>
                </Form>",
            ),
        ]);
        let result =
            form_from_db(&db, Some("Extension"), "Document", Some("Order"), Some("Dialog"))
                .unwrap();
        let body = result.structured_content.as_ref().unwrap();
        let form = &body["object"]["form"];
        assert_eq!(form["elements"].as_array().unwrap().len(), 6);
        let elements = form["elements"].as_array().unwrap();
        assert_eq!(
            elements
                .iter()
                .map(|e| (
                    e["name"].as_str().unwrap(),
                    e["id"].as_u64().unwrap(),
                    e["parent_id"].as_u64()
                ))
                .collect::<Vec<_>>(),
            [
                ("BaseField", 10, None),
                ("Rows", 1, None),
                ("RowsBar", 2, Some(1)),
                ("Actions", 3, Some(2)),
                ("Refresh", 4, Some(3)),
                ("Partner", 5, Some(1))
            ]
        );
        let ids = elements
            .iter()
            .map(|e| e["id"].as_u64().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), elements.len());
        assert_eq!(elements[2]["kind"], "AutoCommandBar");
        assert_eq!(elements[3]["kind"], "ButtonGroup");
        assert_eq!(elements[4]["kind"], "Button");
        assert_eq!(elements[0]["origin"], ":Documents/Order/Forms/Dialog");
        assert!(elements[1..]
            .iter()
            .all(|e| e["origin"] == "Extension:Documents/Order/Forms/Dialog"));
        let parameters = form["parameters"].as_array().unwrap();
        assert_eq!(
            parameters.iter().map(|p| p["name"].as_str().unwrap()).collect::<Vec<_>>(),
            ["Kind", "Inherited", "UnreadType"]
        );
        assert_eq!(parameters[0]["attr_type"], json!({"Ref":{"mdo_type":"Enum","name":"Kinds"}}));
        assert_eq!(parameters[0]["key_parameter"], true);
        assert_eq!(parameters[0]["type_origin"], "Extension:Documents/Order/Forms/Dialog");
        assert_eq!(
            parameters[1]["attr_type"],
            json!({"Ref":{"mdo_type":"Catalog","name":"Partners"}})
        );
        assert_eq!(parameters[1]["type_origin"], ":Documents/Order/Forms/Dialog");
        assert_eq!(parameters[1]["key_parameter"], false);
        assert!(parameters[2]["attr_type"].is_null());
        assert_eq!(form["attributes"].as_array().unwrap().len(), 1);
        assert_eq!(form["attributes"][0]["path"], json!(["Kind"]));
        assert_eq!(form["attributes"][0]["attr_type"], json!({"String":{"length":null}}));
        assert_eq!(body["completeness"]["status"], "partial");
        assert_eq!(body["completeness"]["reasons"][0]["code"], "parameter_type_unread");
        assert_eq!(
            body["completeness"]["reasons"][0]["path"],
            "/object/form/parameters/2/attr_type"
        );
        assert!(form["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["procedure"] == "BasePrepare"
                && b["origin"] == ":Documents/Order/Forms/Dialog"));
        assert!(form["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["procedure"] == "PrepareAfter" && b["call_type"] == "After"));
        assert!(!form.to_string().contains("Unrelated"));
        assert_eq!(form["elements"][0]["data_path_origin"], ":Documents/Order/Forms/Dialog");
        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("## Параметры формы\n- Kind: Перечисление.Kinds"));
        assert!(text.contains("## Реквизиты формы\n- Kind: Строка"));
        assert!(text.contains("RowsBar (AutoCommandBar, id=2, parent=Some(1)"));
        assert!(text.contains("Refresh (Button, id=4, parent=Some(3)"));
        assert!(!text.contains("HistoricalField"));
        let bounded = bound_source_response(result, 128).unwrap();
        assert!(bounded.structured_content.as_ref().unwrap()["object"].is_null());
        assert_eq!(
            bounded.structured_content.as_ref().unwrap()["completeness"]["reasons"][0]["code"],
            "output_budget"
        );
        assert!(live_result_payload_bytes(&bounded) <= 512);
        let base = form_from_db(&db, Some(""), "Document", Some("Order"), Some("Dialog")).unwrap();
        assert_eq!(base.structured_content.as_ref().unwrap()["completeness"]["status"], "complete");
        assert_eq!(
            base.structured_content.as_ref().unwrap()["object"]["form"]["parameters"][0]
                ["attr_type"],
            json!({"String":{"length":null}})
        );
        assert_eq!(
            base.structured_content.unwrap()["object"]["form"]["elements"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn form_contract_adoption_chain_retains_original_anchor_and_source_scopes() {
        let original = "10000000-0000-0000-0000-000000000001";
        let base = descriptor(
            "Form",
            "Dialog",
            original,
            "<FormType>Managed</FormType><ObjectBelonging>Own</ObjectBelonging>",
        );
        let first = descriptor(
            "Form",
            "Dialog",
            "10000000-0000-0000-0000-000000000002",
            &format!(
                "<FormType>Managed</FormType><ObjectBelonging>Adopted</ObjectBelonging>
                 <ExtendedConfigurationObject>{original}</ExtendedConfigurationObject>"
            ),
        );
        let content = |element: &str, id: u32, event: &str, procedure: &str, attribute: &str| {
            format!("<Form xmlns='http://v8.1c.ru/8.3/xcf/logform' xmlns:v8='http://v8.1c.ru/8.1/data/core' xmlns:xs='http://www.w3.org/2001/XMLSchema'><Events><Event name='{event}'>{procedure}</Event></Events><ChildItems><InputField name='{element}' id='{id}'/></ChildItems><Parameters/><Attributes><Attribute name='{attribute}' id='1'><Type><v8:Type>xs:string</v8:Type></Type></Attribute></Attributes></Form>")
        };
        let base_content =
            content("BaseField", 10, "OnCreateAtServer", "BasePrepare", "BaseAttribute");
        let first_content = content("E1Field", 11, "OnOpen", "E1Open", "E1Attribute");
        let second_content = content("E2Field", 12, "OnClose", "E2Close", "E2Attribute");
        for (belonging, target, merged) in [
            ("Adopted", Some(original), true),
            ("Own", None, false),
            ("Adopted", Some("10000000-0000-0000-0000-000000000099"), false),
        ] {
            let target = target
                .map(|target| {
                    format!("<ExtendedConfigurationObject>{target}</ExtendedConfigurationObject>")
                })
                .unwrap_or_default();
            let second = descriptor(
                "Form",
                "Dialog",
                "10000000-0000-0000-0000-000000000003",
                &format!(
                    "<FormType>Managed</FormType><ObjectBelonging>{belonging}</ObjectBelonging>
                     {target}"
                ),
            );
            let mut db = database(&[
                ("base/Documents/Order/Forms/Dialog.xml", &base),
                ("base/Documents/Order/Forms/Dialog/Ext/Form.xml", &base_content),
                ("ext/Documents/Order/Forms/Dialog.xml", &first),
                ("ext/Documents/Order/Forms/Dialog/Ext/Form.xml", &first_content),
                ("other/Documents/Order/Forms/Dialog.xml", &second),
                ("other/Documents/Order/Forms/Dialog/Ext/Form.xml", &second_content),
            ]);
            let mut scope = ide::WorkspaceConfigsSnapshot::from_paths(vec![
                (None, "/inspection/base".into()),
                (Some("E1".into()), "/inspection/ext".into()),
                (Some("E2".into()), "/inspection/other".into()),
            ]);
            scope.closures[2] = vec![1];
            db.set_workspace_configs_snapshot(scope);
            let body = form_from_db(&db, Some("E2"), "Document", Some("Order"), Some("Dialog"))
                .unwrap()
                .structured_content
                .unwrap();
            let form = &body["object"]["form"];
            assert_eq!(body["object"]["identity"]["uuid"], "10000000-0000-0000-0000-000000000003");
            let elements = form["elements"].as_array().unwrap();
            if merged {
                assert_eq!(
                    elements.iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>(),
                    ["BaseField", "E1Field", "E2Field"]
                );
                assert_eq!(
                    form["attributes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|a| a["path"][0].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["BaseAttribute", "E1Attribute", "E2Attribute"]
                );
                assert_eq!(
                    form["origins"],
                    json!([
                        ":Documents/Order/Forms/Dialog",
                        "E1:Documents/Order/Forms/Dialog",
                        "E2:Documents/Order/Forms/Dialog"
                    ])
                );
                for (name, origin) in [
                    ("BasePrepare", ":Documents/Order/Forms/Dialog"),
                    ("E1Open", "E1:Documents/Order/Forms/Dialog"),
                    ("E2Close", "E2:Documents/Order/Forms/Dialog"),
                ] {
                    assert!(form["bindings"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|b| b["procedure"] == name && b["origin"] == origin));
                }
                assert_eq!(body["object"]["identity"]["extends"], original);
                assert_eq!(body["completeness"]["status"], "complete");
            } else {
                assert_eq!(elements.len(), 1);
                assert_eq!(elements[0]["name"], "E2Field");
                assert_eq!(form["attributes"].as_array().unwrap().len(), 1);
                assert_eq!(form["origins"], json!(["E2:Documents/Order/Forms/Dialog"]));
                assert_eq!(
                    body["completeness"]["status"],
                    if belonging == "Own" { "complete" } else { "partial" }
                );
                if belonging == "Adopted" {
                    assert!(body["completeness"]["reasons"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["code"] == "adopted_base_identity_unavailable"));
                }
            }
        }
    }

    #[test]
    fn form_contract_attribute_types_drive_completeness_without_losing_raw_types() {
        let identity = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000001",
            "<FormType>Managed</FormType><ObjectBelonging>Own</ObjectBelonging>",
        );
        let source = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../bsl-metadata/fixtures/inspection/managed-attribute-types.xml"
        ));
        let db = database(&[
            ("base/CommonForms/Dialog.xml", &identity),
            ("base/CommonForms/Dialog/Ext/Form.xml", source),
        ]);
        let result = form_from_db(&db, Some(""), "CommonForm", None, Some("Dialog")).unwrap();
        let body = result.structured_content.as_ref().unwrap();
        let attributes = &body["object"]["form"]["attributes"];
        assert_eq!(
            attributes[0]["attr_type"],
            json!({"Ref":{"mdo_type":"Catalog","name":"Partners"}})
        );
        assert_eq!(attributes[1]["attr_type"], json!({"UnknownNamed":"future:Unresolved"}));
        assert_eq!(attributes[2]["attr_type"], "Unknown");
        assert!(attributes[3]["attr_type"].is_null());
        assert_eq!(
            attributes[4]["attr_type"]["Composite"]["types"],
            json!([{"String":{"length":null}},{"UnknownNamed":"future:Unresolved"}])
        );
        assert_eq!(attributes[7]["path"], json!(["Rows", "Future"]));
        assert_eq!(attributes[8]["path"], json!(["Rows", "Unstated"]));
        assert!(attributes[8]["attr_type"].is_null());
        assert_eq!(attributes[9]["path"], json!(["Rows", "Lines", "Extra"]));
        assert_eq!(attributes[9]["attr_type"], json!({"UnknownNamed":"future:Unresolved"}));
        assert_eq!(body["completeness"]["status"], "partial");
        assert_eq!(
            body["completeness"]["reasons"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (r["path"].as_str().unwrap(), r["code"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            [
                ("/object/form/attributes/1/attr_type", "unknown_technical_name"),
                ("/object/form/attributes/2/attr_type", "technical_name_unavailable"),
                ("/object/form/attributes/3/attr_type", "attribute_type_unread"),
                ("/object/form/attributes/4/attr_type", "unknown_technical_name"),
                ("/object/form/attributes/7/attr_type", "unknown_technical_name"),
                ("/object/form/attributes/8/attr_type", "attribute_type_unread"),
                ("/object/form/attributes/9/attr_type", "unknown_technical_name")
            ]
        );
        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("- Future: Не разрешён: future:Unresolved"));
        assert!(text.contains("- PartialComposite: Строка; Не разрешён: future:Unresolved"));
        assert!(text.contains("- Rows.Future: Не разрешён: future:Unresolved"));
        let known="<Form xmlns='http://v8.1c.ru/8.3/xcf/logform' xmlns:v8='http://v8.1c.ru/8.1/data/core' xmlns:cfg='http://v8.1c.ru/8.1/data/enterprise/current-config' xmlns:xs='http://www.w3.org/2001/XMLSchema'><Parameters/><Attributes><Attribute name='Partner'><Type><v8:Type>cfg:CatalogRef.Partners</v8:Type></Type></Attribute><Attribute name='Rows'><Type><v8:Type>v8:ValueTable</v8:Type></Type><Columns><Column name='Title'><Type><v8:Type>xs:string</v8:Type></Type></Column></Columns></Attribute></Attributes></Form>";
        let db = database(&[
            ("base/CommonForms/Dialog.xml", &identity),
            ("base/CommonForms/Dialog/Ext/Form.xml", known),
        ]);
        let known = form_from_db(&db, Some(""), "CommonForm", None, Some("Dialog"))
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(known["completeness"]["status"], "complete");
        assert_eq!(known["object"]["form"]["attributes"].as_array().unwrap().len(), 3);
        assert_eq!(known["object"]["form"]["attributes"][2]["path"], json!(["Rows", "Title"]));
    }

    #[test]
    fn source_form_parameter_technical_types_are_complete_only_when_known() {
        let identity = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000001",
            "<FormType>Managed</FormType>",
        );
        let db = database(&[
            ("base/CommonForms/Dialog.xml", &identity),
            (
                "base/CommonForms/Dialog/Ext/Form.xml",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../bsl-metadata/fixtures/inspection/managed-parameter-types.xml"
                )),
            ),
        ]);
        let result = form_from_db(&db, Some(""), "CommonForm", None, Some("Dialog")).unwrap();
        let body = result.structured_content.as_ref().unwrap();
        let parameters = &body["object"]["form"]["parameters"];
        assert_eq!(
            parameters[0]["attr_type"],
            json!({"Ref":{"mdo_type":"Catalog","name":"Partners"}})
        );
        assert_eq!(parameters[1]["attr_type"], json!({"UnknownNamed":"future:Unresolved"}));
        assert_eq!(parameters[2]["attr_type"], "Unknown");
        assert_eq!(
            parameters[3]["attr_type"]["Composite"]["types"],
            json!([{"String":{"length":null}},{"Ref":{"mdo_type":"Catalog","name":"Partners"}}])
        );
        assert_eq!(
            parameters[4]["attr_type"]["Composite"]["types"][1],
            json!({"UnknownNamed":"future:Unresolved"})
        );
        assert!(parameters[5]["attr_type"].is_null());
        assert_eq!(
            body["object"]["form"]["attributes"][0]["attr_type"],
            json!({"String":{"length":null}})
        );
        let reasons = body["completeness"]["reasons"].as_array().unwrap();
        assert_eq!(
            reasons
                .iter()
                .map(|r| (r["path"].as_str().unwrap(), r["code"].as_str().unwrap()))
                .collect::<Vec<_>>(),
            [
                ("/object/form/parameters/1/attr_type", "unknown_technical_name"),
                ("/object/form/parameters/2/attr_type", "technical_name_unavailable"),
                ("/object/form/parameters/4/attr_type", "unknown_technical_name"),
                ("/object/form/parameters/5/attr_type", "parameter_type_unread")
            ]
        );
        assert_eq!(body["completeness"]["status"], "partial");
        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("- Partner: Справочник.Partners"));
        assert!(
            text.contains("- Future: Не разрешён: future:Unresolved (technical_name_unavailable)")
        );
        assert!(text.contains("- KnownComposite: Строка; Справочник.Partners"));
        assert!(text.contains("- PartialComposite: Строка; Не разрешён: future:Unresolved (technical_name_unavailable)"));
    }

    #[test]
    fn source_form_missing_and_empty_sections_remain_distinct() {
        let identity = descriptor(
            "CommonForm",
            "Dialog",
            "10000000-0000-0000-0000-000000000001",
            "<FormType>Managed</FormType>",
        );
        for (content, missing) in [
            ("<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'/>", true),
            (
                "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'><Parameters/><Attributes/></Form>",
                false,
            ),
        ] {
            let db = database(&[
                ("base/CommonForms/Dialog.xml", &identity),
                ("base/CommonForms/Dialog/Ext/Form.xml", content),
            ]);
            let result = form_from_db(&db, Some(""), "CommonForm", None, Some("Dialog")).unwrap();
            let body = result.structured_content.as_ref().unwrap();
            let form = &body["object"]["form"];
            assert_eq!(form["parameters"].is_null(), missing);
            assert_eq!(form["attributes"].is_null(), missing);
            assert_eq!(
                body["completeness"]["status"],
                if missing { "partial" } else { "complete" }
            );
            let text = result
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if missing {
                assert_eq!(
                    body["completeness"]["reasons"],
                    json!([{"code":"form_parameters_unread","path":"/object/form/parameters"},{"code":"form_attributes_unread","path":"/object/form/attributes"}])
                );
                assert!(text.contains("## Параметры формы\nunknown"));
                assert!(text.contains("## Реквизиты формы\nunknown"));
            } else {
                assert_eq!(form["parameters"], json!([]));
                assert_eq!(form["attributes"], json!([]));
                assert!(!text.contains("unknown"));
            }
        }
    }

    #[test]
    fn source_form_mismatched_base_and_missing_representation_are_explicit() {
        let ext = descriptor(
            "Form",
            "Dialog",
            "10000000-0000-0000-0000-000000000002",
            concat!(
                "<FormType>Managed</FormType><ObjectBelonging>Adopted</ObjectBelonging>",
                "<ExtendedConfigurationObject>10000000-0000-0000-0000-000000000099",
                "</ExtendedConfigurationObject>"
            ),
        );
        let ordinary = descriptor(
            "Form",
            "Legacy",
            "10000000-0000-0000-0000-000000000003",
            "<FormType>Ordinary</FormType>",
        );
        let db = database(&[
            ("ext/Documents/Order/Forms/Dialog.xml", &ext),
            (
                "ext/Documents/Order/Forms/Dialog/Ext/Form.xml",
                "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'/>",
            ),
            ("base/Documents/Order/Forms/Legacy.xml", &ordinary),
        ]);
        let body = form_from_db(&db, Some("Extension"), "Document", Some("Order"), Some("Dialog"))
            .unwrap()
            .structured_content
            .unwrap();
        assert!(body["completeness"]["reasons"]
            .to_string()
            .contains("adopted_base_identity_unavailable"));
        let body = form_from_db(&db, Some(""), "Document", Some("Order"), Some("Legacy"))
            .unwrap()
            .structured_content
            .unwrap();
        assert!(body["object"]["form"].is_null());
        assert_eq!(body["completeness"]["status"], "partial");
    }

    #[test]
    fn declared_managed_form_requires_managed_representation() {
        let identity = descriptor(
            "Form",
            "Dialog",
            "10000000-0000-0000-0000-000000000004",
            "<FormType>Managed</FormType>",
        );
        let source = ("base/Documents/Order/Forms/Dialog.xml", identity.as_str());
        let ordinary = (
            "base/Documents/Order/Forms/Dialog/Ext/Form/Form.json",
            r#"{"schema":2,"form":{"name":"Dialog"},"controls":[]}"#,
        );
        for files in [vec![source], vec![source, ordinary]] {
            let db = database(&files);
            let body = form_from_db(&db, Some(""), "Document", Some("Order"), Some("Dialog"))
                .unwrap()
                .structured_content
                .unwrap();
            assert!(body["object"]["form"].is_null());
            assert_eq!(body["completeness"]["status"], "partial");
            assert_eq!(
                body["completeness"]["reasons"][0]["detail"],
                "managed_representation_unavailable"
            );
        }
    }

    #[test]
    fn source_ordinary_form_preserves_pages_and_requires_matching_hash() {
        let identity = descriptor(
            "Form",
            "Dialog",
            "10000000-0000-0000-0000-000000000003",
            "<FormType>Ordinary</FormType>",
        );
        let fingerprint = json!({"sha256":"a".repeat(64),"legacy_sidecar_stale":false}).to_string();
        let db = database(&[
            ("base/Documents/Order/Forms/Dialog.xml", &identity),
            (
                "base/Documents/Order/Forms/Dialog/Ext/Form/Form.json",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../bsl-metadata/fixtures/inspection/ordinary.json"
                )),
            ),
            ("base/Documents/Order/Forms/Dialog/Ext/Form.bin", &fingerprint),
        ]);
        let body = form_from_db(&db, None, "Document", Some("Order"), Some("Dialog"))
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(body["object"]["form"]["form_type"], "Ordinary");
        assert_eq!(body["object"]["form"]["pages"], json!(["Main", "Details"]));
        assert_eq!(body["object"]["form"]["elements"][1]["data_path"], "Document");
        assert_eq!(body["object"]["form"]["freshness"], "sha256");
        assert_eq!(body["completeness"]["status"], "partial");

        let identity = descriptor(
            "Form",
            "ФормаОбычнаяСОписанием",
            "10000000-0000-0000-0000-000000000003",
            "<FormType>Ordinary</FormType>",
        );
        let mut source: Value = serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-metadata/fixtures/designer/Catalogs/Справочник1/Forms/ФормаОбычнаяСОписанием/Ext/Form/Form.json"))).unwrap();
        source["controls"].as_array_mut().unwrap().last_mut().unwrap()["handlers"] =
            json!({"ПриОткрытии":"БезымянныйПриОткрытии"});
        source["controls"].as_array_mut().unwrap().push(json!({"kind":"list","id":22,"name":"Список","columns":[{"name":"Код","handlers":{"ПриИзменении":"КодПриИзменении","Пустое":""}}]}));
        source["form"]["boundHandlers"]
            .as_array_mut()
            .unwrap()
            .extend(["КодПриИзменении", "БезымянныйПриОткрытии"].map(|name| json!(name)));
        let source = source.to_string();
        let db = database(&[
            ("base/Catalogs/Справочник1/Forms/ФормаОбычнаяСОписанием.xml", &identity),
            ("base/Catalogs/Справочник1/Forms/ФормаОбычнаяСОписанием/Ext/Form/Form.json", &source),
        ]);
        let result = form_from_db(
            &db,
            Some(""),
            "Catalog",
            Some("Справочник1"),
            Some("ФормаОбычнаяСОписанием"),
        )
        .unwrap();
        let body = result.structured_content.as_ref().unwrap();
        let bindings = body["object"]["form"]["bindings"].as_array().unwrap();
        let flat = bindings
            .iter()
            .filter(|b| b["procedure"] == "ДействияФормыВыполнить")
            .collect::<Vec<_>>();
        assert_eq!(flat.len(), 1);
        assert_eq!(flat[0]["event"], "Привязка");
        assert_eq!(flat[0]["owner_kind"], "unknown");
        assert!(flat[0]["owner"].is_null());
        assert!(flat[0]["call_type"].is_null());
        assert_eq!(flat[0]["origin"], ":Catalogs/Справочник1/Forms/ФормаОбычнаяСОписанием");
        assert_eq!(bindings.iter().filter(|b| b["procedure"] == "ПриОткрытии").count(), 1);
        assert_eq!(bindings.len(), 5);
        let column = bindings.iter().find(|b| b["procedure"] == "КодПриИзменении").unwrap();
        assert_eq!(column["owner_kind"], "column");
        assert_eq!(column["owner"], "Список.Код");
        assert_eq!(column["event"], "ПриИзменении");
        let nameless = bindings.iter().find(|b| b["procedure"] == "БезымянныйПриОткрытии").unwrap();
        assert_eq!(nameless["owner_kind"], "unknown");
        assert!(nameless["owner"].is_null());
        assert_eq!(nameless["event"], "ПриОткрытии");
        let text = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|c| c.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Привязка → ДействияФормыВыполнить"));
        assert!(text.contains("ПриИзменении → КодПриИзменении"));
    }

    #[test]
    fn source_template_positive_composition_sheet_and_unsupported_binary() {
        let composition = descriptor(
            "Template",
            "Schema",
            "10000000-0000-0000-0000-000000000011",
            "<TemplateType>DataCompositionSchema</TemplateType>",
        );
        let spreadsheet = descriptor(
            "Template",
            "Print",
            "10000000-0000-0000-0000-000000000012",
            "<TemplateType>SpreadsheetDocument</TemplateType>",
        );
        let db = database(&[
            ("base/Reports/Report/Templates/Schema.xml", &composition),
            (
                "base/Reports/Report/Templates/Schema/Ext/Template.xml",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../bsl-metadata/fixtures/inspection/composition.xml"
                )),
            ),
            ("base/Reports/Report/Templates/Print.xml", &spreadsheet),
            (
                "base/Reports/Report/Templates/Print/Ext/Template.xml",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../bsl-metadata/fixtures/inspection/spreadsheet.xml"
                )),
            ),
            ("ext/Reports/Report/Templates/Print.xml", &spreadsheet),
            ("other/Reports/Report/Templates/Schema.xml", &composition),
            ("other/Reports/Report/Templates/Schema/Ext/Template.xml", "<foreign/>"),
        ]);
        let composition_result =
            template_from_db(&db, Some(""), "Report", Some("Report"), Some("Schema")).unwrap();
        let body = composition_result.structured_content.as_ref().unwrap();
        assert_eq!(body["source"], "source");
        assert_eq!(body["completeness"]["status"], "complete");
        assert_eq!(body["object"]["identity"]["uuid"], "10000000-0000-0000-0000-000000000011");
        let composition = &body["object"]["content"]["composition"];
        assert_eq!(composition["origin"], ":Reports/Report/Templates/Schema");
        assert_eq!(body["object"]["origin"], composition["origin"]);
        assert_eq!(composition["data_sources"][0]["children"][0]["text"], "LocalSource");
        let union = &composition["data_sets"][0];
        assert_eq!(
            union["kind"],
            "{http://v8.1c.ru/8.1/data-composition-system/schema}DataSetUnion"
        );
        let dataset = &union["data_sets"][0];
        assert_eq!(dataset["name"], "Partners");
        assert_eq!(dataset["data_source"], "LocalSource");
        assert_eq!(dataset["query"], "SELECT Ref\nFROM Catalog.Partners WHERE Code = &Code");
        assert_eq!(dataset["fields"][0]["children"][0]["text"], "Ref");
        assert_eq!(dataset["parameter_bindings"][0]["children"][1]["text"], "&Code");
        assert_eq!(composition["parameters"][0]["children"][0]["text"], "Code");
        assert_eq!(
            composition["parameters"][0]["children"][1]["children"][0]["namespace"],
            "http://v8.1c.ru/8.1/data/core"
        );
        assert_eq!(
            composition["parameters"][0]["children"][2]["attributes"]
                ["{http://www.w3.org/2001/XMLSchema-instance}type"],
            "{http://www.w3.org/2001/XMLSchema}string"
        );
        assert_eq!(composition["calculated_fields"][0]["children"][1]["text"], "Ref");
        assert_eq!(composition["settings_variants"][0]["children"][0]["text"], "Main");
        assert_eq!(
            composition["settings_variants"][0]["children"][0]["namespace"],
            "http://v8.1c.ru/8.1/data-composition-system/settings"
        );
        let sheet_result =
            template_from_db(&db, Some(""), "Report", Some("Report"), Some("Print")).unwrap();
        let body = sheet_result.structured_content.as_ref().unwrap();
        assert_eq!(body["completeness"]["status"], "complete");
        let sheet = &body["object"]["content"]["spreadsheet"];
        assert_eq!(sheet["origin"], ":Reports/Report/Templates/Print");
        assert_eq!(
            sheet["areas"][0],
            json!({"name":"Header","kind":"Rows","begin_row":0,"end_row":1,"begin_column":-1,"end_column":-1,"columns_id":null})
        );
        assert_eq!(
            sheet["parameters"][0],
            json!({"row":0,"column":3,"columns_id":null,"name":"Title","template":null,"template_parameters":[],"detail_parameter":null})
        );
        assert_eq!(sheet["parameters"][1]["row"], 0);
        assert_eq!(sheet["parameters"][1]["column"], 4);
        assert_eq!(sheet["parameters"][1]["template_parameters"], json!(["Partner", "Amount"]));
        for result in [composition_result, sheet_result] {
            let bounded = bound_source_response(result, 128).unwrap();
            let body = bounded.structured_content.as_ref().unwrap();
            assert!(body["object"].is_null());
            assert_eq!(body["truncated"], true);
            assert_eq!(body["completeness"]["reasons"][0]["code"], "output_budget");
            assert!(live_result_payload_bytes(&bounded) <= 512);
        }
        let body =
            template_from_db(&db, Some("Extension"), "Report", Some("Report"), Some("Print"))
                .unwrap()
                .structured_content
                .unwrap();
        assert!(body["object"]["content"].is_null());
        assert_eq!(body["completeness"]["status"], "partial");
        assert_eq!(
            body["completeness"]["reasons"][0]["code"],
            "template_representation_unavailable"
        );
        let malformed =
            template_from_db(&db, Some("Other"), "Report", Some("Report"), Some("Schema"))
                .unwrap()
                .structured_content
                .unwrap();
        assert!(malformed["object"]["content"].is_null());
        assert_eq!(malformed["completeness"]["status"], "partial");
        assert_eq!(malformed["completeness"]["reasons"][0]["code"], "template_content_unread");
        assert_eq!(malformed["object"]["origin"], "Other:Reports/Report/Templates/Schema");
    }

    #[test]
    fn source_spreadsheet_native_xml_keeps_coordinates_and_descriptor_identity() {
        let identity = descriptor(
            "CommonTemplate",
            "Print",
            "10000000-0000-0000-0000-000000000012",
            "<TemplateType>SpreadsheetDocument</TemplateType>",
        );
        let db = database(&[
            ("base/CommonTemplates/Print.xml", &identity),
            (
                "base/CommonTemplates/Print/Ext/Template.xml",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../bsl-metadata/fixtures/inspection/spreadsheet-native.xml"
                )),
            ),
        ]);
        let body = template_from_db(&db, Some(""), "CommonTemplate", None, Some("Print"))
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(body["object"]["identity"]["uuid"], "10000000-0000-0000-0000-000000000012");
        assert_eq!(body["object"]["identity"]["template_type"], "SpreadsheetDocument");
        assert_eq!(body["object"]["origin"], ":CommonTemplates/Print");
        assert_eq!(body["completeness"]["status"], "complete");
        let sheet = &body["object"]["content"]["spreadsheet"];
        assert_eq!(sheet["origin"], ":CommonTemplates/Print");
        assert_eq!(
            sheet["areas"][0],
            json!({"name":"Header","kind":"Rows","begin_row":0,"end_row":1,"begin_column":-1,"end_column":-1,"columns_id":null})
        );
        assert_eq!(sheet["parameters"].as_array().unwrap().len(), 2);
        assert_eq!(sheet["parameters"][0]["name"], "Title");
        assert_eq!(sheet["parameters"][0]["row"], 0);
        assert_eq!(sheet["parameters"][0]["column"], 3);
        assert_eq!(sheet["parameters"][1]["row"], 0);
        assert_eq!(sheet["parameters"][1]["column"], 4);
        assert_eq!(sheet["parameters"][1]["template_parameters"], json!(["Partner", "Amount"]));
        assert!(body["object"]["content"].get("form").is_none());
    }

    #[test]
    fn source_composition_retains_known_content_but_marks_unprojected_source_paths() {
        let descriptor = descriptor(
            "Template",
            "Schema",
            "10000000-0000-0000-0000-000000000011",
            "<TemplateType>DataCompositionSchema</TemplateType>",
        );
        let source = "<DataCompositionSchema xmlns='http://v8.1c.ru/8.1/data-composition-system/schema' xmlns:xsi='http://www.w3.org/2001/XMLSchema-instance'><dataSet xsi:type='DataSetQuery'><name>Known</name><query>SELECT Ref FROM Catalog.Partners</query><link><expression>UnsupportedLink</expression></link></dataSet><totalField><dataPath>Ref</dataPath></totalField></DataCompositionSchema>";
        let db = database(&[
            ("base/Reports/Report/Templates/Schema.xml", &descriptor),
            ("base/Reports/Report/Templates/Schema/Ext/Template.xml", source),
        ]);
        let body = template_from_db(&db, Some(""), "Report", Some("Report"), Some("Schema"))
            .unwrap()
            .structured_content
            .unwrap();
        assert_eq!(
            body["object"]["content"]["composition"]["data_sets"][0]["query"],
            "SELECT Ref FROM Catalog.Partners"
        );
        assert_eq!(body["completeness"]["status"], "partial");
        assert_eq!(body["completeness"]["reasons"][0]["code"], "composition_content_unsupported");
        assert_eq!(body["completeness"]["reasons"][0]["paths"], json!(["/DataCompositionSchema/dataSet/{http://v8.1c.ru/8.1/data-composition-system/schema}link", "/DataCompositionSchema/{http://v8.1c.ru/8.1/data-composition-system/schema}totalField"]));
    }

    #[test]
    fn source_role_retains_rights_but_marks_unprojected_source_paths() {
        let identity = descriptor("Role", "Reader", "10000000-0000-0000-0000-000000000021", "");
        let db = database(&[("base/Roles/Reader.xml", &identity), ("base/Roles/Reader/Ext/Rights.xml", "<Rights xmlns='http://v8.1c.ru/8.2/roles'><object><name>Catalog.Partners</name><right><name>Read</name><value>true</value><unknownCondition>Ref = &amp;Other</unknownCondition></right></object><unknownSection>Unprojected</unknownSection></Rights>")]);
        let body = role_from_db(&db, Some(""), "Reader").unwrap().structured_content.unwrap();
        assert_eq!(body["object"]["rights"]["objects"][0]["rights"][0]["value"], true);
        assert_eq!(body["completeness"]["status"], "partial");
        assert_eq!(body["completeness"]["reasons"][0]["code"], "rights_content_unsupported");
        assert_eq!(
            body["completeness"]["reasons"][0]["paths"],
            json!([
                "/Rights/object/right/{http://v8.1c.ru/8.2/roles}unknownCondition",
                "/Rights/{http://v8.1c.ru/8.2/roles}unknownSection"
            ])
        );
    }

    #[test]
    fn source_roles_keep_exact_scope_rights_and_rls_with_budget() {
        let identity = descriptor("Role", "Reader", "10000000-0000-0000-0000-000000000021", "");
        let unread = descriptor("Role", "Unread", "10000000-0000-0000-0000-000000000022", "");
        let db = database(&[
            ("base/Roles/Reader.xml", &identity),
            (
                "base/Roles/Reader/Ext/Rights.xml",
                include_str!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../bsl-metadata/fixtures/inspection/rights.xml"
                )),
            ),
            ("ext/Roles/Reader.xml", &identity),
            ("ext/Roles/Reader/Ext/Rights.xml", "<Rights xmlns='http://v8.1c.ru/8.2/roles'/>"),
            ("base/Roles/Unread.xml", &unread),
            ("ext/Roles/Unread.xml", &unread),
            ("ext/Roles/Unread/Ext/Rights.xml", "<Rights/>"),
        ]);
        let result = role_from_db(&db, Some(""), "Reader").unwrap();
        let body = result.structured_content.as_ref().unwrap();
        assert_eq!(body["completeness"]["status"], "complete");
        assert_eq!(body["object"]["origin"], ":Roles/Reader");
        let rights = &body["object"]["rights"];
        assert_eq!(rights["origin"], ":Roles/Reader");
        assert_eq!(rights["set_for_new_objects"], false);
        assert!(rights["set_for_attributes_by_default"].is_null());
        assert_eq!(rights["independent_rights_of_child_objects"], true);
        assert_eq!(rights["objects"][0]["object"], "Catalog.Partners.Attribute.Secret");
        assert_eq!(rights["objects"][0]["rights"][0]["name"], "Read");
        assert_eq!(rights["objects"][0]["rights"][1]["name"], "View");
        assert_eq!(rights["objects"][0]["rights"][1]["value"], true);
        assert_eq!(
            rights["objects"][0]["rights"][1]["restrictions"][0]["children"][0]["text"],
            "Partner = &AllowedPartner"
        );
        assert_eq!(
            rights["objects"][0]["rights"][1]["restrictions"][0]["children"][1]["text"],
            "true"
        );
        assert_eq!(rights["objects"][1]["object"], "Configuration.Demo");
        assert!(rights["objects"][1]["rights"][0]["value"].is_null());
        assert_eq!(rights["restriction_templates"][0]["children"][1]["text"], "Ref IN (&Allowed)");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["object"]["rights"]["objects"][0]["rights"]
                [0]["value"],
            false
        );
        assert!(result.structured_content.as_ref().unwrap()["object"]["rights"]
            .to_string()
            .contains("AllowedPartner"));
        let other =
            role_from_db(&db, Some("Extension"), "Reader").unwrap().structured_content.unwrap();
        assert!(other["object"]["rights"]["objects"].as_array().unwrap().is_empty());
        assert_eq!(other["object"]["rights"]["origin"], "Extension:Roles/Reader");
        assert!(role_from_db(&db, Some("Other"), "Reader").is_err());
        for (root, code) in [("", "rights_unavailable"), ("Extension", "rights_unread")] {
            let unread =
                role_from_db(&db, Some(root), "Unread").unwrap().structured_content.unwrap();
            assert!(unread["object"]["rights"].is_null());
            assert_eq!(unread["completeness"]["status"], "partial");
            assert_eq!(unread["completeness"]["reasons"][0]["code"], code);
        }
        let bounded = bound_source_response(result, 128).unwrap().structured_content.unwrap();
        assert!(bounded["object"].is_null());
        assert_eq!(bounded["completeness"]["reasons"][0]["code"], "output_budget");
    }
    #[test]
    fn own_ordinary_form_without_representation_does_not_return_managed_base() {
        let base = descriptor(
            "Form",
            "Dialog",
            "10000000-0000-0000-0000-000000000031",
            "<FormType>Managed</FormType>",
        );
        let own = descriptor(
            "Form",
            "Dialog",
            "10000000-0000-0000-0000-000000000032",
            "<FormType>Ordinary</FormType><ObjectBelonging>Own</ObjectBelonging>",
        );
        let db = database(&[
            ("base/Documents/Order/Forms/Dialog.xml", &base),
            (
                "base/Documents/Order/Forms/Dialog/Ext/Form.xml",
                "<Form xmlns='http://v8.1c.ru/8.3/xcf/logform'><Parameters/><Attributes/></Form>",
            ),
            ("ext/Documents/Order/Forms/Dialog.xml", &own),
        ]);
        let body = form_from_db(&db, Some("Extension"), "Document", Some("Order"), Some("Dialog"))
            .unwrap()
            .structured_content
            .unwrap();
        assert!(body["object"]["form"].is_null());
        assert_eq!(body["completeness"]["status"], "partial");
        assert!(body["completeness"]["reasons"]
            .to_string()
            .contains("ordinary_representation_unavailable"));
    }
}
