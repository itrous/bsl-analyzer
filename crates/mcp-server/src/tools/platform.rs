use crate::tools::response::{serialized_bytes, structured_with_text, truncate_text_to_budget};
use bsl_platform::PlatformDataInner;
use rmcp::model::CallToolResult;
use rmcp::ErrorData as McpError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt::Write;
use std::sync::Arc;

pub const REFERENCE_DOCUMENT_SCHEMA_VERSION: u32 = 1;

/// A large platform type unfolds its constructors, methods and properties in full, so the
/// card goes out through the output budget. The note points at the cheaper follow-up: one
/// member's card via `type_name` instead of the whole type.
const BUDGET_NOTE: &str = "\n-- карточка усечена под max_output_tokens; повысьте бюджет или \
                           запросите один метод: name=\"ИмяМетода\", type_name=\"ИмяТипа\" --\n";

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, JsonSchema, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlatformReferenceKind {
    Type,
    Method,
    Property,
    Constructor,
    GlobalFunction,
}

impl PlatformReferenceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Method => "method",
            Self::Property => "property",
            Self::Constructor => "constructor",
            Self::GlobalFunction => "global_function",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize)]
pub(crate) struct PlatformReference {
    pub(crate) reference_id: String,
    pub(crate) kind: PlatformReferenceKind,
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) english_name: Option<String>,
    pub(crate) description: Option<String>,
}

#[derive(JsonSchema, Serialize)]
pub(crate) struct ListPlatformResponse {
    action: ListPlatformAction,
    schema_version: ListPlatformSchemaVersion,
    items: Vec<PlatformReference>,
    shown: usize,
    total: usize,
    budget_exhausted: bool,
    budget_hint: Option<String>,
}

#[derive(JsonSchema, Serialize)]
enum ListPlatformAction {
    #[serde(rename = "list_platform")]
    ListPlatform,
}

#[derive(JsonSchema, Serialize)]
enum ListPlatformSchemaVersion {
    #[serde(rename = "1")]
    V1,
}

pub(crate) fn list_platform(
    kind: Option<PlatformReferenceKind>,
    name: Option<&str>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let needle = name.map(str::to_lowercase);
    let mut items: Vec<_> = platform_references()
        .into_iter()
        .filter(|item| kind.is_none_or(|kind| item.kind == kind))
        .filter(|item| {
            needle.as_deref().is_none_or(|needle| {
                item.name.to_lowercase().contains(needle)
                    || item
                        .english_name
                        .as_deref()
                        .is_some_and(|name| name.to_lowercase().contains(needle))
            })
        })
        .collect();
    sort_platform_references(&mut items);
    let total = items.len();
    let lines: Vec<_> = items
        .iter()
        .map(|item| {
            if item.owner.is_empty() {
                format!("{}: {} [{}]", item.kind.as_str(), item.name, item.reference_id)
            } else {
                format!(
                    "{}: {}.{} [{}]",
                    item.kind.as_str(),
                    item.owner,
                    item.name,
                    item.reference_id
                )
            }
        })
        .collect();
    let build = |shown: usize| -> Result<CallToolResult, McpError> {
        let exhausted = shown < total;
        let body = serde_json::to_value(ListPlatformResponse {
            action: ListPlatformAction::ListPlatform,
            schema_version: ListPlatformSchemaVersion::V1,
            shown,
            total,
            items: items[..shown].to_vec(),
            budget_exhausted: exhausted,
            budget_hint: exhausted
                .then(|| "narrow kind/name or increase max_output_tokens".to_owned()),
        })
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(structured_with_text(lines[..shown].join("\n"), body))
    };
    let full = build(total)?;
    let ceiling = max_output_tokens.saturating_mul(4);
    if serialized_bytes(&full) <= ceiling {
        return Ok(full);
    }
    let (mut low, mut high) = (0usize, total);
    while low < high {
        let mid = (low + high).div_ceil(2);
        if serialized_bytes(&build(mid)?) <= ceiling {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    build(low)
}

pub(crate) fn platform_references() -> Vec<PlatformReference> {
    let platform = PlatformDataInner::instance();
    let mut references = Vec::with_capacity(
        platform.all_types().len()
            + platform.all_methods().len()
            + platform.all_properties().len()
            + platform.all_constructors().len()
            + platform.all_global_functions().len(),
    );

    references.extend(platform.all_types().iter().map(reference_for_type));
    references
        .extend(platform.all_methods().iter().map(|item| reference_for_method(platform, item)));
    references.extend(
        platform.all_properties().iter().map(|item| reference_for_property(platform, item)),
    );
    references.extend(
        platform.all_constructors().iter().map(|item| reference_for_constructor(platform, item)),
    );
    references.extend(
        platform
            .all_global_functions()
            .iter()
            .map(|item| reference_for_global_function(platform, item)),
    );
    sort_platform_references(&mut references);
    references
}

pub(crate) fn platform_reference_for_document(
    kind: &str,
    title: &str,
) -> Option<PlatformReference> {
    let platform = PlatformDataInner::instance();
    match kind {
        "type" => platform
            .all_types()
            .iter()
            .find(|item| title == format!("{} / {}", item.name, item.english_name))
            .map(reference_for_type),
        "method" => platform
            .all_methods()
            .iter()
            .find(|item| {
                title
                    == format!(
                        "{}.{} / {}.{}",
                        item.type_name, item.name, item.type_name, item.english_name
                    )
            })
            .map(|item| reference_for_method(platform, item)),
        "property" => platform
            .all_properties()
            .iter()
            .find(|item| {
                title
                    == format!(
                        "{}.{} / {}.{}",
                        item.type_name, item.name, item.type_name, item.english_name
                    )
            })
            .map(|item| reference_for_property(platform, item)),
        "constructor" => platform
            .all_constructors()
            .iter()
            .find(|item| {
                title
                    == format!(
                        "Новый {} ({})",
                        item.type_name,
                        item.variant_name.as_deref().unwrap_or("Новый")
                    )
            })
            .map(|item| reference_for_constructor(platform, item)),
        "global_function" => platform
            .all_global_functions()
            .iter()
            .find(|item| title == format!("{} / {}", item.name, item.english_name))
            .map(|item| reference_for_global_function(platform, item)),
        _ => None,
    }
}

pub fn build_reference_documents() -> Vec<bsl_search::Document> {
    build_reference_documents_from(PlatformDataInner::instance())
}

/// Reference documents of `platform`, which need not be the process snapshot.
pub fn build_reference_documents_from(platform: &PlatformDataInner) -> Vec<bsl_search::Document> {
    let mut documents = Vec::new();

    for item in platform.all_types() {
        let names = |values: Vec<String>| values.join(", ");
        documents.push(bsl_search::Document {
            title: format!("{} / {}", item.name, item.english_name),
            body: format!(
                "Тип: {} / {}\nМетоды: {}\nСвойства: {}\nКонструкторы: {}",
                item.name,
                item.english_name,
                names(
                    platform
                        .get_type_methods(&item.name)
                        .iter()
                        .map(|value| format!("{} / {}", value.name, value.english_name))
                        .collect(),
                ),
                names(
                    platform
                        .all_properties()
                        .iter()
                        .filter(|value| {
                            value.type_name == item.name || value.type_name == item.english_name
                        })
                        .map(|value| format!("{} / {}", value.name, value.english_name))
                        .collect(),
                ),
                names(
                    platform
                        .get_constructors(&item.name)
                        .iter()
                        .map(|value| {
                            value.variant_name.as_deref().unwrap_or("Новый").to_owned()
                        })
                        .collect(),
                ),
            ),
            kind: "type".to_owned(),
        });
    }
    for item in platform.all_methods() {
        documents.push(callable_document(
            "method",
            format!("{}.{} / {}.{}", item.type_name, item.name, item.type_name, item.english_name),
            format!("Тип: {}\nМетод: {} / {}\n", item.type_name, item.name, item.english_name),
            item.return_type.as_deref(),
            platform.get_method_docs(item.id).as_ref(),
        ));
    }
    for item in platform.all_properties() {
        let docs = platform.get_property_docs(item.id);
        let mut body = format!(
            "Тип: {}\nСвойство: {} / {}\nТип значения: {}\nТолько чтение: {}\n",
            item.type_name,
            item.name,
            item.english_name,
            item.property_types.join(", "),
            item.is_readonly,
        );
        if let Some(docs) = docs {
            if !docs.description.is_empty() {
                let _ = writeln!(body, "Описание: {}", docs.description);
            }
        }
        documents.push(bsl_search::Document {
            title: format!(
                "{}.{} / {}.{}",
                item.type_name, item.name, item.type_name, item.english_name
            ),
            body,
            kind: "property".to_owned(),
        });
    }
    for item in platform.all_constructors() {
        let docs = platform.get_constructor_docs(item.id);
        let name = item.variant_name.as_deref().unwrap_or("Новый");
        let mut body = format!("Тип: {}\nКонструктор: {name}\n", item.type_name);
        if let Some(docs) = docs {
            append_documentation(
                &mut body,
                &docs.syntax,
                &docs.description,
                &docs.params,
                &docs.examples,
            );
        }
        documents.push(bsl_search::Document {
            title: format!("Новый {} ({name})", item.type_name),
            body,
            kind: "constructor".to_owned(),
        });
    }
    for item in platform.all_global_functions() {
        documents.push(callable_document(
            "global_function",
            format!("{} / {}", item.name, item.english_name),
            format!("Глобальная функция: {} / {}\n", item.name, item.english_name),
            item.return_type.as_deref(),
            platform.get_global_function_docs(item.id).as_ref(),
        ));
    }
    documents.sort_by(|left, right| {
        (&left.kind, &left.title, &left.body).cmp(&(&right.kind, &right.title, &right.body))
    });
    documents
}

pub fn reference_documents_fingerprint(documents: &[bsl_search::Document]) -> String {
    let mut canonical: Vec<_> = documents
        .iter()
        .map(|item| json!({"kind": item.kind, "title": item.title, "body": item.body}))
        .collect();
    canonical.sort_by(|left, right| {
        serde_json::to_string(left)
            .expect("reference document serializes")
            .cmp(&serde_json::to_string(right).expect("reference document serializes"))
    });
    blake3::hash(
        serde_json::to_vec(&(REFERENCE_DOCUMENT_SCHEMA_VERSION, canonical))
            .expect("reference corpus serializes")
            .as_slice(),
    )
    .to_hex()
    .to_string()
}

fn callable_document(
    kind: &str,
    title: String,
    mut body: String,
    return_type: Option<&str>,
    docs: Option<&bsl_platform::MethodDocs>,
) -> bsl_search::Document {
    if let Some(return_type) = return_type {
        let _ = writeln!(body, "Возвращает: {return_type}");
    }
    if let Some(docs) = docs {
        append_documentation(
            &mut body,
            &docs.syntax,
            &docs.description,
            &docs.params,
            &docs.examples,
        );
    }
    bsl_search::Document { title, body, kind: kind.to_owned() }
}

fn append_documentation(
    body: &mut String,
    syntax: &str,
    description: &str,
    params: &[bsl_platform::ParamDocs],
    examples: &[bsl_platform::CodeExample],
) {
    if !syntax.is_empty() {
        let _ = writeln!(body, "Синтаксис: {syntax}");
    }
    if !description.is_empty() {
        let _ = writeln!(body, "Описание: {description}");
    }
    for param in params {
        let _ = writeln!(body, "Параметр {}: {}", param.name, param.description);
    }
    for example in examples {
        let _ = writeln!(body, "Пример: {}", example.code);
    }
}

fn reference_for_type(item: &bsl_platform::PlatformType) -> PlatformReference {
    let english_name = non_empty(item.english_name.as_str());
    reference(
        PlatformReferenceKind::Type,
        String::new(),
        item.name.as_str(),
        english_name.clone(),
        None,
        json!(["type", item.name.as_str(), english_name, item.xdto_name.as_deref()]),
    )
}

fn reference_for_method(
    platform: &PlatformDataInner,
    item: &bsl_platform::PlatformMethod,
) -> PlatformReference {
    let (owner, owner_identity) = owner_identity(platform, item.type_name.as_str());
    let english_name = non_empty(item.english_name.as_str());
    let signature = callable_identity(
        item.return_type.as_deref(),
        &item.parameters,
        item.variants
            .iter()
            .map(|variant| (variant.variant_name.as_deref(), variant.parameters.as_slice())),
    );
    reference(
        PlatformReferenceKind::Method,
        owner,
        item.name.as_str(),
        english_name.clone(),
        platform.get_method_docs(item.id).and_then(|docs| non_empty(&docs.description)),
        json!(["method", owner_identity, item.name.as_str(), english_name, signature]),
    )
}

fn reference_for_property(
    platform: &PlatformDataInner,
    item: &bsl_platform::PlatformProperty,
) -> PlatformReference {
    let (owner, owner_identity) = owner_identity(platform, item.type_name.as_str());
    let english_name = non_empty(item.english_name.as_str());
    reference(
        PlatformReferenceKind::Property,
        owner,
        item.name.as_str(),
        english_name.clone(),
        platform.get_property_docs(item.id).and_then(|docs| non_empty(&docs.description)),
        json!([
            "property",
            owner_identity,
            item.name.as_str(),
            english_name,
            item.property_types.iter().map(|value| value.as_str()).collect::<Vec<_>>(),
            item.is_readonly
        ]),
    )
}

fn reference_for_constructor(
    platform: &PlatformDataInner,
    item: &bsl_platform::PlatformConstructor,
) -> PlatformReference {
    let (owner, owner_identity) = owner_identity(platform, item.type_name.as_str());
    let signature = callable_identity(
        None,
        &item.parameters,
        std::iter::empty::<(Option<&str>, &[bsl_platform::MethodParam])>(),
    );
    let name = item.variant_name.as_deref().unwrap_or(&signature);
    reference(
        PlatformReferenceKind::Constructor,
        owner,
        name,
        None,
        platform.get_constructor_docs(item.id).and_then(|docs| non_empty(&docs.description)),
        json!(["constructor", owner_identity, name, signature]),
    )
}

fn reference_for_global_function(
    platform: &PlatformDataInner,
    item: &bsl_platform::GlobalFunction,
) -> PlatformReference {
    let english_name = non_empty(item.english_name.as_str());
    let signature = callable_identity(
        item.return_type.as_deref(),
        &item.parameters,
        item.variants
            .iter()
            .map(|variant| (variant.variant_name.as_deref(), variant.parameters.as_slice())),
    );
    reference(
        PlatformReferenceKind::GlobalFunction,
        String::new(),
        item.name.as_str(),
        english_name.clone(),
        platform.get_global_function_docs(item.id).and_then(|docs| non_empty(&docs.description)),
        json!(["global_function", item.name.as_str(), english_name, signature]),
    )
}

fn sort_platform_references(references: &mut [PlatformReference]) {
    references.sort_by_cached_key(|item| {
        (item.kind, item.owner.to_lowercase(), item.name.to_lowercase(), item.reference_id.clone())
    });
}

fn reference(
    kind: PlatformReferenceKind,
    owner: String,
    name: &str,
    english_name: Option<String>,
    description: Option<String>,
    identity: Value,
) -> PlatformReference {
    let digest =
        blake3::hash(serde_json::to_string(&identity).expect("identity serializes").as_bytes());
    PlatformReference {
        reference_id: format!(
            "{}:{}:{}~{}",
            kind.as_str(),
            percent_encode(&owner),
            percent_encode(name),
            digest.to_hex()
        ),
        kind,
        owner,
        name: name.to_owned(),
        english_name,
        description,
    }
}

fn owner_identity(platform: &PlatformDataInner, owner: &str) -> (String, Value) {
    let owner_type = platform
        .all_types()
        .iter()
        .find(|item| item.english_name == owner)
        .or_else(|| platform.all_types().iter().find(|item| item.name == owner));
    match owner_type {
        Some(item) => (
            item.name.to_string(),
            json!([
                item.name.as_str(),
                non_empty(item.english_name.as_str()),
                item.xdto_name.as_deref()
            ]),
        ),
        None => (owner.to_owned(), json!([owner, null, null])),
    }
}

fn callable_identity<'a>(
    return_type: Option<&str>,
    parameters: &[bsl_platform::MethodParam],
    variants: impl Iterator<Item = (Option<&'a str>, &'a [bsl_platform::MethodParam])>,
) -> String {
    let mut identity = format!("({})", parameter_identity(parameters));
    for (name, parameters) in variants {
        let _ =
            write!(identity, "|{}({})", name.unwrap_or_default(), parameter_identity(parameters));
    }
    if let Some(return_type) = return_type {
        let _ = write!(identity, "->{return_type}");
    }
    identity
}

fn parameter_identity(parameters: &[bsl_platform::MethodParam]) -> String {
    parameters
        .iter()
        .map(|parameter| {
            format!(
                "{}:{}:{}:{}",
                parameter.name,
                parameter.param_type.as_deref().unwrap_or_default(),
                parameter.is_optional,
                parameter.is_variadic
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[derive(JsonSchema, Serialize)]
#[serde(untagged)]
#[allow(
    clippy::large_enum_variant,
    reason = "boxing the flattened card changes the published Schemars union shape"
)]
enum SyntaxHelpResponse {
    Card {
        schema_version: SyntaxHelpSchemaVersion,
        #[serde(flatten)]
        item: SyntaxHelpItem,
        text_truncated: bool,
        budget_exhausted: bool,
    },
    NotFound {
        schema_version: SyntaxHelpSchemaVersion,
        status: SyntaxHelpNotFoundStatus,
        reference_id: Option<String>,
        name: Option<String>,
        type_name: Option<String>,
        budget_exhausted: bool,
    },
    BudgetExhausted {
        schema_version: SyntaxHelpSchemaVersion,
        status: SyntaxHelpBudgetStatus,
        budget_exhausted: bool,
        budget_hint: String,
    },
}

#[derive(JsonSchema, Serialize)]
enum SyntaxHelpSchemaVersion {
    #[serde(rename = "2")]
    V2,
}

#[derive(JsonSchema, Serialize)]
enum SyntaxHelpNotFoundStatus {
    #[serde(rename = "not_found")]
    NotFound,
}

#[derive(JsonSchema, Serialize)]
enum SyntaxHelpBudgetStatus {
    #[serde(rename = "budget_exhausted")]
    BudgetExhausted,
}

pub(crate) fn syntax_help_output_schema() -> Arc<serde_json::Map<String, Value>> {
    let generated = rmcp::handler::server::tool::schema_for_type::<SyntaxHelpResponse>();
    let mut schema = generated.as_ref().clone();
    let flattened = (|| {
        let Value::Array(mut responses) = schema.remove("anyOf")? else { return None };
        let card = responses.first()?.as_object()?;
        let common_properties = card.get("properties")?.as_object()?.clone();
        let common_required = card.get("required")?.as_array()?.clone();
        let item_branches = card.get("oneOf")?.as_array()?;
        let mut one_of = Vec::with_capacity(item_branches.len() + responses.len() - 1);
        for item in item_branches {
            let mut item = item.clone();
            let item_object = item.as_object_mut()?;
            item_object.get_mut("properties")?.as_object_mut()?.extend(common_properties.clone());
            let required = item_object.get_mut("required")?.as_array_mut()?;
            for key in &common_required {
                if !required.contains(key) {
                    required.push(key.clone());
                }
            }
            one_of.push(item);
        }
        one_of.extend(responses.drain(1..));
        schema.insert("oneOf".into(), Value::Array(one_of));
        Some(schema)
    })();
    let mut schema = flattened.unwrap_or_else(|| generated.as_ref().clone());
    crate::contract::ensure_object_root(&mut schema);
    Arc::new(schema)
}

#[derive(JsonSchema, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SyntaxHelpItem {
    Type {
        name: String,
        english_name: String,
        min_version: Option<String>,
        contexts: Vec<SyntaxContext>,
        iterable_element_types: Vec<String>,
        xdto_name: Option<String>,
        constructors: Vec<SyntaxCallable>,
        methods: Vec<SyntaxMethodSummary>,
        properties: Vec<SyntaxPropertySummary>,
    },
    Method {
        matches: Vec<SyntaxCallable>,
    },
    Constructor {
        constructor: SyntaxCallable,
    },
    GlobalFunction {
        function: SyntaxCallable,
    },
    Keyword {
        name: String,
        english_name: String,
        syntax: String,
        description: String,
        parameters: Vec<SyntaxDocParameter>,
        min_version: Option<String>,
    },
    Property {
        name: String,
        english_name: String,
        owner_type: Option<String>,
        property_types: Vec<String>,
        read_only: bool,
        min_version: Option<String>,
        contexts: Vec<SyntaxContext>,
        description: Option<String>,
        notes: Option<String>,
        see_also: Vec<String>,
    },
}

#[derive(JsonSchema, Serialize)]
struct SyntaxCallable {
    name: String,
    english_name: Option<String>,
    owner_type: Option<String>,
    variant_name: Option<String>,
    return_type: Option<String>,
    parameters: Vec<SyntaxParameter>,
    variants: Vec<SyntaxVariant>,
    min_version: Option<String>,
    contexts: Vec<SyntaxContext>,
    documentation: Option<SyntaxDocumentation>,
}

#[derive(JsonSchema, Serialize)]
struct SyntaxMethodSummary {
    name: String,
    english_name: String,
    return_type: Option<String>,
    min_version: Option<String>,
    contexts: Vec<SyntaxContext>,
}

#[derive(JsonSchema, Serialize)]
struct SyntaxPropertySummary {
    name: String,
    english_name: String,
    property_types: Vec<String>,
    read_only: bool,
    min_version: Option<String>,
    contexts: Vec<SyntaxContext>,
}

#[derive(JsonSchema, Serialize)]
struct SyntaxVariant {
    name: Option<String>,
    parameters: Vec<SyntaxParameter>,
}

#[derive(JsonSchema, Serialize)]
struct SyntaxParameter {
    name: String,
    parameter_type: Option<String>,
    optional: bool,
    variadic: bool,
}

#[derive(JsonSchema, Serialize)]
struct SyntaxDocumentation {
    syntax: String,
    description: String,
    parameters: Vec<SyntaxDocParameter>,
    examples: Vec<SyntaxExample>,
    notes: Option<String>,
    see_also: Vec<String>,
}

#[derive(JsonSchema, Serialize)]
struct SyntaxDocParameter {
    name: String,
    description: String,
    default_value: Option<String>,
}

#[derive(JsonSchema, Serialize)]
struct SyntaxExample {
    code: String,
    description: Option<String>,
}

#[derive(JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum SyntaxContext {
    ThickClient,
    ThinClient,
    WebClient,
    Server,
    MobileClient,
    ExternalConnection,
}

pub fn bsl_syntax_help(
    name: &str,
    type_name: Option<&str>,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let Ok((text, item)) = syntax_help_card(name, type_name) else {
        return syntax_help_not_found(None, Some(name), type_name);
    };
    syntax_help_result(text, item, max_output_tokens)
}

pub(crate) fn bsl_syntax_help_by_reference_id(
    reference_id: &str,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let platform = PlatformDataInner::instance();
    let card = platform
        .all_types()
        .iter()
        .find(|item| reference_for_type(item).reference_id == reference_id)
        .map(|item| type_card(platform, item))
        .or_else(|| {
            platform
                .all_methods()
                .iter()
                .find(|item| reference_for_method(platform, item).reference_id == reference_id)
                .map(|item| method_card(platform, item))
        })
        .or_else(|| {
            platform
                .all_properties()
                .iter()
                .find(|item| reference_for_property(platform, item).reference_id == reference_id)
                .map(|item| property_card(platform, item))
        })
        .or_else(|| {
            platform
                .all_constructors()
                .iter()
                .find(|item| reference_for_constructor(platform, item).reference_id == reference_id)
                .map(|item| constructor_card(platform, item))
        })
        .or_else(|| {
            platform
                .all_global_functions()
                .iter()
                .find(|item| {
                    reference_for_global_function(platform, item).reference_id == reference_id
                })
                .map(|item| {
                    (
                        format_global_function(platform, item),
                        SyntaxHelpItem::GlobalFunction {
                            function: callable_from_global_function(platform, item),
                        },
                    )
                })
        });
    match card {
        Some((text, item)) => syntax_help_result(text, item, max_output_tokens),
        None => syntax_help_not_found(Some(reference_id), None, None),
    }
}

fn syntax_help_result(
    mut text: String,
    item: SyntaxHelpItem,
    max_output_tokens: usize,
) -> Result<CallToolResult, McpError> {
    let mut body = serde_json::to_value(SyntaxHelpResponse::Card {
        schema_version: SyntaxHelpSchemaVersion::V2,
        item,
        text_truncated: false,
        budget_exhausted: false,
    })
    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    // The Markdown is served first, but not out of the card identity's pocket: charging it the
    // whole ceiling leaves nothing for the kind and the names, and the pair then trips the
    // over-ceiling check below — an answer with a truncated rendering and a naming card becomes
    // an empty envelope. What the identity needs is reserved before the text is cut.
    let text_budget = max_output_tokens.saturating_sub(identity_bytes(&body).div_ceil(4));
    let text_truncated = truncate_text_to_budget(&mut text, text_budget, BUDGET_NOTE);
    body["text_truncated"] = json!(text_truncated);
    let budget_exhausted = fit_card_to_budget(&mut body, max_output_tokens, text.len());
    body["budget_exhausted"] = json!(budget_exhausted);
    if text.len() + serialized_bytes(&body) > max_output_tokens.saturating_mul(4) {
        text.clear();
        body = serde_json::to_value(SyntaxHelpResponse::BudgetExhausted {
            schema_version: SyntaxHelpSchemaVersion::V2,
            status: SyntaxHelpBudgetStatus::BudgetExhausted,
            budget_exhausted: true,
            budget_hint: "increase max_output_tokens or request one exact reference_id".to_owned(),
        })
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    }
    Ok(structured_with_text(text, body))
}

fn syntax_help_not_found(
    reference_id: Option<&str>,
    name: Option<&str>,
    type_name: Option<&str>,
) -> Result<CallToolResult, McpError> {
    let target = reference_id.or(name).unwrap_or_default();
    let body = serde_json::to_value(SyntaxHelpResponse::NotFound {
        schema_version: SyntaxHelpSchemaVersion::V2,
        status: SyntaxHelpNotFoundStatus::NotFound,
        reference_id: reference_id.map(str::to_owned),
        name: name.map(str::to_owned),
        type_name: type_name.map(str::to_owned),
        budget_exhausted: false,
    })
    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    Ok(structured_with_text(format!("Сущность платформы не найдена: {target}"), body))
}

/// The card's own size with every listing emptied — the kind, the names, the flags, which no
/// budget may drop.
fn identity_bytes(body: &Value) -> usize {
    let mut identity = body.clone();
    for (key, _) in LISTINGS {
        if let Some(array) = identity.get_mut(key).and_then(Value::as_array_mut) {
            array.clear();
        }
    }
    serialized_bytes(&identity)
}

/// Fit the card into what the Markdown left of `max_output_tokens`, reporting whether the pair
/// still exceeds it.
///
/// The response carries the rendering and the card together, so both are charged to one ceiling
/// — budgeting the text alone overshoots it by the card's size, which for a large platform type
/// is the larger half (see [`crate::tools::response::structured_with_text`]). The text is served
/// first, because clients have parsed it since before the card existed; the card's listings then
/// take what is left, in the order and with the floors [`LISTINGS`] gives them. A pair that is
/// still over the ceiling says so through this flag instead of going out silently oversized.
fn fit_card_to_budget(body: &mut Value, max_output_tokens: usize, text_bytes: usize) -> bool {
    let ceiling = max_output_tokens.saturating_mul(4);
    let mut listings = Vec::new();
    for (key, keep_one) in LISTINGS {
        if let Some(array) = body.get_mut(key).and_then(Value::as_array_mut) {
            listings.push((key, std::mem::take(array), keep_one));
        }
    }

    // With the listings taken out, what remains is the identity of the card — the kind, the
    // names, the flags — which no budget may drop: a card that cannot say what it describes is
    // worse than a card that says it is partial.
    let mut left = ceiling.saturating_sub(text_bytes + serialized_bytes(body));
    let mut dropped = false;
    for (key, mut items, keep_one) in listings {
        let (used, cut) = fit_listing(&mut items, left, keep_one);
        left = left.saturating_sub(used);
        dropped |= cut;
        body[key] = Value::Array(items);
    }
    dropped || text_bytes + serialized_bytes(body) > ceiling
}

/// The card's listings, in the order they are paid for, each with whether its first entry
/// survives a budget too small for it.
///
/// `matches` is the answer to the lookup itself, and an empty one would read as "nothing found";
/// a type's constructors and methods are supplementary, and the Markdown note already points at
/// the single-member lookup that returns them affordably.
const LISTINGS: [(&str, bool); 4] =
    [("matches", true), ("constructors", false), ("methods", false), ("properties", false)];

/// Keep the leading entries of a listing that fit `budget_bytes`, reporting the bytes kept and
/// whether anything was dropped.
fn fit_listing(items: &mut Vec<Value>, budget_bytes: usize, keep_one: bool) -> (usize, bool) {
    let mut used = 0usize;
    let mut keep = 0usize;
    for (i, item) in items.iter().enumerate() {
        let next = used + serialized_bytes(item) + usize::from(i > 0);
        if next > budget_bytes && !(keep_one && keep == 0) {
            break;
        }
        used = next;
        keep = i + 1;
    }
    let dropped = keep < items.len();
    items.truncate(keep);
    (used, dropped)
}

fn syntax_help_card(
    name: &str,
    type_name: Option<&str>,
) -> Result<(String, SyntaxHelpItem), McpError> {
    let platform = PlatformDataInner::instance();

    if let Some(tn) = type_name {
        return search_member_of_type(platform, tn, name);
    }

    if let Some(func) = platform.get_global_function(name) {
        return Ok((
            format_global_function(platform, func),
            SyntaxHelpItem::GlobalFunction {
                function: callable_from_global_function(platform, func),
            },
        ));
    }

    let types = platform.all_types();
    let name_lower = name.to_lowercase();
    if let Some(pt) = types.iter().find(|t| {
        t.name.to_lowercase() == name_lower || t.english_name.to_lowercase() == name_lower
    }) {
        return Ok(type_card(platform, pt));
    }

    let all_methods = platform.all_methods();
    let matches: Vec<_> = all_methods
        .iter()
        .filter(|m| {
            m.name.to_lowercase() == name_lower || m.english_name.to_lowercase() == name_lower
        })
        .collect();

    if !matches.is_empty() {
        let mut out = format!("# Метод: {name}\n\n");
        if matches.len() > 1 {
            let _ = writeln!(out, "Найден у нескольких типов:\n");
        }
        for m in &matches {
            let _ = writeln!(
                out,
                "## {}.{} / {}.{}\n",
                m.type_name, m.name, m.type_name, m.english_name
            );
            format_method_signature(&mut out, &m.name, &m.parameters, m.return_type.as_deref());
            if let Some(docs) = platform.get_method_docs(m.id) {
                format_docs(&mut out, &docs);
            }
            out.push('\n');
        }
        return Ok((
            out,
            SyntaxHelpItem::Method {
                matches: matches
                    .into_iter()
                    .map(|method| callable_from_method(platform, method))
                    .collect(),
            },
        ));
    }

    if let Some(kw) = platform.get_keyword_docs(name) {
        return Ok((
            format_keyword_docs(&kw),
            SyntaxHelpItem::Keyword {
                name: kw.keyword_ru.to_string(),
                english_name: kw.keyword_en.to_string(),
                syntax: kw.syntax.clone(),
                description: kw.description.clone(),
                parameters: kw.params.iter().map(doc_parameter).collect(),
                min_version: kw.min_version.clone(),
            },
        ));
    }

    if let Some(property) = platform.get_global_property(name) {
        return Ok(property_card(platform, property));
    }

    Err(McpError::invalid_params(
        format!(
            "'{name}' не найдено среди типов, методов, свойств, глобальных функций и ключевых \
             слов платформы"
        ),
        None,
    ))
}

fn type_card(
    platform: &PlatformDataInner,
    item: &bsl_platform::PlatformType,
) -> (String, SyntaxHelpItem) {
    (
        format_type_info(platform, item),
        SyntaxHelpItem::Type {
            name: item.name.to_string(),
            english_name: item.english_name.to_string(),
            min_version: item.min_version.as_ref().map(ToString::to_string),
            contexts: context_names(item.context),
            iterable_element_types: item
                .iter_element_types
                .iter()
                .map(ToString::to_string)
                .collect(),
            xdto_name: item.xdto_name.as_ref().map(ToString::to_string),
            constructors: platform
                .get_constructors(&item.name)
                .into_iter()
                .map(|constructor| callable_from_constructor(platform, constructor))
                .collect(),
            methods: platform
                .get_type_methods(&item.name)
                .into_iter()
                .map(method_summary)
                .collect(),
            properties: platform
                .all_properties()
                .iter()
                .filter(|property| {
                    property.type_name == item.name || property.type_name == item.english_name
                })
                .map(property_summary)
                .collect(),
        },
    )
}

/// A member of a named type: a method, or failing that a property.
///
/// Both are members and both are addressed the same way — `(type, name)` — so
/// one of them answering and the other not made every property of every type a
/// name the dictionary could find and this tool could not open.
fn search_member_of_type(
    platform: &PlatformDataInner,
    type_name: &str,
    member_name: &str,
) -> Result<(String, SyntaxHelpItem), McpError> {
    if platform.get_method(type_name, member_name).is_some() {
        return search_method(platform, type_name, member_name);
    }
    // The property table is keyed by the owner the platform records, and for a
    // member of the global context that owner is `Global context` — the very
    // string the dictionary publishes. There is no second lookup to fall back
    // to: searching the global properties by bare name would answer a WRONG
    // `type_name` with a card about something else instead of refusing it.
    if let Some(property) = platform.get_property(type_name, member_name) {
        return Ok(property_card(platform, property));
    }
    Err(McpError::invalid_params(
        format!("'{member_name}' не найдено среди методов и свойств типа '{type_name}'"),
        None,
    ))
}

fn property_card(
    platform: &PlatformDataInner,
    property: &bsl_platform::PlatformProperty,
) -> (String, SyntaxHelpItem) {
    let docs = platform.get_property_docs(property.id);
    let owner = (!property.type_name.is_empty()).then(|| property.type_name.to_string());

    let mut out = match &owner {
        Some(owner) => {
            format!("# {}.{} / {}.{}\n\n", owner, property.name, owner, property.english_name)
        }
        None => format!("# {} / {}\n\n", property.name, property.english_name),
    };
    if !property.property_types.is_empty() {
        let _ = writeln!(out, "Тип: {}\n", property.property_types.join(", "));
    }
    if property.is_readonly {
        let _ = writeln!(out, "Только чтение.\n");
    }
    if let Some(docs) = &docs {
        if !docs.description.is_empty() {
            let _ = writeln!(out, "## Описание\n\n{}\n", docs.description);
        }
        if let Some(notes) = &docs.notes {
            let _ = writeln!(out, "## Примечания\n\n{notes}\n");
        }
    }

    (
        out,
        SyntaxHelpItem::Property {
            name: property.name.to_string(),
            english_name: property.english_name.to_string(),
            owner_type: owner,
            property_types: property.property_types.iter().map(ToString::to_string).collect(),
            read_only: property.is_readonly,
            min_version: property.min_version.as_ref().map(ToString::to_string),
            contexts: context_names(property.context),
            description: docs.as_ref().map(|d| d.description.clone()),
            notes: docs.as_ref().and_then(|d| d.notes.clone()),
            see_also: docs.map(|d| d.see_also).unwrap_or_default(),
        },
    )
}

fn search_method(
    platform: &PlatformDataInner,
    type_name: &str,
    method_name: &str,
) -> Result<(String, SyntaxHelpItem), McpError> {
    if let Some(method) = platform.get_method(type_name, method_name) {
        Ok(method_card(platform, method))
    } else {
        Err(McpError::invalid_params(
            format!("Метод '{method_name}' не найден у типа '{type_name}'"),
            None,
        ))
    }
}

fn method_card(
    platform: &PlatformDataInner,
    method: &bsl_platform::PlatformMethod,
) -> (String, SyntaxHelpItem) {
    let mut out = format!(
        "# {}.{} / {}.{}\n\n",
        method.type_name, method.name, method.type_name, method.english_name
    );
    format_method_signature(
        &mut out,
        &method.name,
        &method.parameters,
        method.return_type.as_deref(),
    );
    if let Some(docs) = platform.get_method_docs(method.id) {
        format_docs(&mut out, &docs);
    }
    (out, SyntaxHelpItem::Method { matches: vec![callable_from_method(platform, method)] })
}

fn constructor_card(
    platform: &PlatformDataInner,
    constructor: &bsl_platform::PlatformConstructor,
) -> (String, SyntaxHelpItem) {
    let callable = callable_from_constructor(platform, constructor);
    let mut out = format!("# Конструктор: Новый {}\n\n", constructor.type_name);
    format_constructor_signature(&mut out, &constructor.type_name, &constructor.parameters);
    if let Some(docs) = platform.get_constructor_docs(constructor.id) {
        format_doc_body(
            &mut out,
            &docs.description,
            &docs.params,
            &docs.examples,
            docs.notes.as_deref(),
        );
    }
    (out, SyntaxHelpItem::Constructor { constructor: callable })
}

fn callable_from_method(
    platform: &PlatformDataInner,
    method: &bsl_platform::PlatformMethod,
) -> SyntaxCallable {
    SyntaxCallable {
        name: method.name.to_string(),
        english_name: Some(method.english_name.to_string()),
        owner_type: Some(method.type_name.to_string()),
        variant_name: None,
        return_type: method.return_type.as_ref().map(ToString::to_string),
        parameters: method.parameters.iter().map(parameter).collect(),
        variants: method
            .variants
            .iter()
            .map(|variant| SyntaxVariant {
                name: variant.variant_name.as_ref().map(ToString::to_string),
                parameters: variant.parameters.iter().map(parameter).collect(),
            })
            .collect(),
        min_version: method.min_version.as_ref().map(ToString::to_string),
        contexts: context_names(method.context),
        documentation: platform.get_method_docs(method.id).as_ref().map(documentation),
    }
}

fn callable_from_global_function(
    platform: &PlatformDataInner,
    function: &bsl_platform::GlobalFunction,
) -> SyntaxCallable {
    SyntaxCallable {
        name: function.name.to_string(),
        english_name: Some(function.english_name.to_string()),
        owner_type: None,
        variant_name: None,
        return_type: function.return_type.as_ref().map(ToString::to_string),
        parameters: function.parameters.iter().map(parameter).collect(),
        variants: function
            .variants
            .iter()
            .map(|variant| SyntaxVariant {
                name: variant.variant_name.as_ref().map(ToString::to_string),
                parameters: variant.parameters.iter().map(parameter).collect(),
            })
            .collect(),
        min_version: function.min_version.as_ref().map(ToString::to_string),
        contexts: context_names(function.context),
        documentation: platform.get_global_function_docs(function.id).as_ref().map(documentation),
    }
}

fn callable_from_constructor(
    platform: &PlatformDataInner,
    constructor: &bsl_platform::PlatformConstructor,
) -> SyntaxCallable {
    SyntaxCallable {
        name: "Новый".to_owned(),
        english_name: Some("New".to_owned()),
        owner_type: Some(constructor.type_name.to_string()),
        variant_name: constructor.variant_name.as_ref().map(ToString::to_string),
        return_type: Some(constructor.type_name.to_string()),
        parameters: constructor.parameters.iter().map(parameter).collect(),
        variants: Vec::new(),
        min_version: constructor.min_version.as_ref().map(ToString::to_string),
        contexts: context_names(constructor.context),
        documentation: platform.get_constructor_docs(constructor.id).as_ref().map(|docs| {
            SyntaxDocumentation {
                syntax: docs.syntax.clone(),
                description: docs.description.clone(),
                parameters: docs.params.iter().map(doc_parameter).collect(),
                examples: docs.examples.iter().map(example).collect(),
                notes: docs.notes.clone(),
                see_also: docs.see_also.clone(),
            }
        }),
    }
}

fn method_summary(method: &bsl_platform::PlatformMethod) -> SyntaxMethodSummary {
    SyntaxMethodSummary {
        name: method.name.to_string(),
        english_name: method.english_name.to_string(),
        return_type: method.return_type.as_ref().map(ToString::to_string),
        min_version: method.min_version.as_ref().map(ToString::to_string),
        contexts: context_names(method.context),
    }
}

fn property_summary(property: &bsl_platform::PlatformProperty) -> SyntaxPropertySummary {
    SyntaxPropertySummary {
        name: property.name.to_string(),
        english_name: property.english_name.to_string(),
        property_types: property.property_types.iter().map(ToString::to_string).collect(),
        read_only: property.is_readonly,
        min_version: property.min_version.as_ref().map(ToString::to_string),
        contexts: context_names(property.context),
    }
}

fn parameter(param: &bsl_platform::MethodParam) -> SyntaxParameter {
    SyntaxParameter {
        name: param.name.to_string(),
        parameter_type: param.param_type.as_ref().map(ToString::to_string),
        optional: param.is_optional,
        variadic: param.is_variadic,
    }
}

fn documentation(docs: &bsl_platform::MethodDocs) -> SyntaxDocumentation {
    SyntaxDocumentation {
        syntax: docs.syntax.clone(),
        description: docs.description.clone(),
        parameters: docs.params.iter().map(doc_parameter).collect(),
        examples: docs.examples.iter().map(example).collect(),
        notes: docs.notes.clone(),
        see_also: docs.see_also.clone(),
    }
}

fn doc_parameter(param: &bsl_platform::ParamDocs) -> SyntaxDocParameter {
    SyntaxDocParameter {
        name: param.name.to_string(),
        description: param.description.clone(),
        default_value: param.default_value.clone(),
    }
}

fn example(example: &bsl_platform::CodeExample) -> SyntaxExample {
    SyntaxExample { code: example.code.clone(), description: example.description.clone() }
}

/// An entry with no availability markup is available everywhere, so an empty list here means
/// exactly one thing: the platform marked the entry available in no context at all.
fn context_names(context: Option<bsl_platform::ContextAvailability>) -> Vec<SyntaxContext> {
    let context = bsl_platform::ContextAvailability::effective(context.as_ref());
    [
        (context.thick_client, SyntaxContext::ThickClient),
        (context.thin_client, SyntaxContext::ThinClient),
        (context.web_client, SyntaxContext::WebClient),
        (context.server, SyntaxContext::Server),
        (context.mobile_client, SyntaxContext::MobileClient),
        (context.external_connection, SyntaxContext::ExternalConnection),
    ]
    .into_iter()
    .filter(|(available, _)| *available)
    .map(|(_, name)| name)
    .collect()
}

fn format_global_function(
    platform: &PlatformDataInner,
    func: &bsl_platform::GlobalFunction,
) -> String {
    let mut out = format!("# {} / {}\n\n", func.name, func.english_name);
    format_method_signature(&mut out, &func.name, &func.parameters, func.return_type.as_deref());
    if let Some(docs) = platform.get_global_function_docs(func.id) {
        format_docs(&mut out, &docs);
    }
    out
}

fn format_type_info(platform: &PlatformDataInner, pt: &bsl_platform::PlatformType) -> String {
    let mut out = format!("# {} / {}\n\n", pt.name, pt.english_name);

    format_constructors(&mut out, platform, &pt.name);

    let methods = platform.get_type_methods(&pt.name);
    if methods.is_empty() {
        // Only call a type method-less if it also has no constructor surface — otherwise
        // the constructor section above already carries its real API.
        if platform.get_constructors(&pt.name).is_empty() {
            let _ = writeln!(out, "Методов нет.");
        }
    } else {
        let _ = writeln!(out, "## Методы ({})\n", methods.len());
        let _ = writeln!(out, "| Имя | English | Возвращает |");
        let _ = writeln!(out, "|-----|---------|------------|");
        for m in &methods {
            let ret = m.return_type.as_deref().unwrap_or("—");
            let _ = writeln!(out, "| {} | {} | {ret} |", m.name, m.english_name);
        }
        let _ = writeln!(out, "\nИспользуйте `bsl_syntax_help(name=\"ИмяМетода\", type_name=\"{}\")` для подробной справки.", pt.name);
    }

    out
}

fn format_keyword_docs(kw: &bsl_platform::KeywordDocs) -> String {
    let mut out = format!("# {} / {}\n\n", kw.keyword_ru, kw.keyword_en);
    let _ = writeln!(out, "## Синтаксис\n\n```bsl\n{}\n```\n", kw.syntax);
    let _ = writeln!(out, "## Описание\n\n{}", kw.description);
    if !kw.params.is_empty() {
        let _ = writeln!(out, "\n## Параметры\n");
        for p in &kw.params {
            let _ = writeln!(out, "- **{}**: {}", p.name, p.description);
        }
    }
    out
}

fn format_method_signature(
    out: &mut String,
    name: &str,
    params: &[bsl_platform::MethodParam],
    return_type: Option<&str>,
) {
    let _ = write!(out, "```bsl\n{name}(");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            let _ = write!(out, ", ");
        }
        if p.is_optional {
            let _ = write!(out, "[");
        }
        let _ = write!(out, "{}", p.name);
        if let Some(ref pt) = p.param_type {
            let _ = write!(out, ": {pt}");
        }
        if p.is_optional {
            let _ = write!(out, "]");
        }
    }
    let _ = write!(out, ")");
    if let Some(ret) = return_type {
        let _ = write!(out, ": {ret}");
    }
    let _ = writeln!(out, "\n```\n");
}

fn format_docs(out: &mut String, docs: &bsl_platform::MethodDocs) {
    format_doc_body(out, &docs.description, &docs.params, &docs.examples, docs.notes.as_deref());
}

/// Render the shared documentation body (description / params / examples / notes).
/// Methods and constructors carry the same doc fields, so both render through here.
fn format_doc_body(
    out: &mut String,
    description: &str,
    params: &[bsl_platform::ParamDocs],
    examples: &[bsl_platform::CodeExample],
    notes: Option<&str>,
) {
    if !description.is_empty() {
        let _ = writeln!(out, "## Описание\n\n{description}\n");
    }
    if !params.is_empty() {
        let _ = writeln!(out, "## Параметры\n");
        for p in params {
            if let Some(ref def) = p.default_value {
                let _ = writeln!(out, "- **{}** (по умолчанию: {def}): {}", p.name, p.description);
            } else {
                let _ = writeln!(out, "- **{}**: {}", p.name, p.description);
            }
        }
        out.push('\n');
    }
    if !examples.is_empty() {
        let _ = writeln!(out, "## Примеры\n");
        for ex in examples {
            if let Some(ref desc) = ex.description {
                let _ = writeln!(out, "{desc}\n");
            }
            let _ = writeln!(out, "```bsl\n{}\n```\n", ex.code);
        }
    }
    if let Some(notes) = notes {
        let _ = writeln!(out, "## Примечания\n\n{notes}\n");
    }
}

/// Render a type's constructors (`Новый Тип(…)`). A type whose entire API is its
/// constructor (`ОписаниеОповещения`, `Граница`, `ОписаниеТипов`) would otherwise show
/// only "Методов нет." and hide its real surface.
fn format_constructors(out: &mut String, platform: &PlatformDataInner, type_name: &str) {
    let constructors = platform.get_constructors(type_name);
    if constructors.is_empty() {
        return;
    }
    let _ = writeln!(out, "## Конструкторы ({})\n", constructors.len());
    let many = constructors.len() > 1;
    for ctor in &constructors {
        if many {
            if let Some(ref variant) = ctor.variant_name {
                let _ = writeln!(out, "### {variant}\n");
            }
        }
        let docs = platform.get_constructor_docs(ctor.id);
        match docs.as_ref().filter(|d| !d.syntax.is_empty()) {
            Some(d) => {
                let _ = writeln!(out, "```bsl\n{}\n```\n", d.syntax);
            }
            None => format_constructor_signature(out, type_name, &ctor.parameters),
        }
        if let Some(d) = docs.as_ref() {
            format_doc_body(out, &d.description, &d.params, &d.examples, d.notes.as_deref());
        }
    }
}

/// Fallback constructor signature built from parameter metadata when prose docs carry
/// no ready-made `syntax` string.
fn format_constructor_signature(
    out: &mut String,
    type_name: &str,
    params: &[bsl_platform::MethodParam],
) {
    let _ = write!(out, "```bsl\nНовый {type_name}(");
    for (i, p) in params.iter().enumerate() {
        if i > 0 {
            let _ = write!(out, ", ");
        }
        if p.is_optional {
            let _ = write!(out, "[");
        }
        let _ = write!(out, "{}", p.name);
        if let Some(ref pt) = p.param_type {
            let _ = write!(out, ": {pt}");
        }
        if p.is_optional {
            let _ = write!(out, "]");
        }
    }
    let _ = writeln!(out, ")\n```\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_ids_are_encoded_identity_digests_and_stable_when_a_peer_is_added() {
        let original = reference(
            PlatformReferenceKind::Type,
            "Владелец:100%".to_owned(),
            "Имя Юникод",
            Some("Name".to_owned()),
            None,
            json!(["type", "Имя Юникод", "Name", "XdtoA"]),
        );
        let peer = reference(
            PlatformReferenceKind::Type,
            "Владелец:100%".to_owned(),
            "Имя Юникод",
            Some("Other".to_owned()),
            None,
            json!(["type", "Имя Юникод", "Other", "XdtoB"]),
        );
        assert!(original.reference_id.contains("%3A"));
        assert!(original.reference_id.contains("%25"));
        assert!(original.reference_id.contains("%D0"));
        let digest = original.reference_id.rsplit_once('~').unwrap().1;
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert_ne!(original.reference_id, peer.reference_id);
        assert_eq!(
            original.reference_id,
            reference(
                PlatformReferenceKind::Type,
                "Владелец:100%".to_owned(),
                "Имя Юникод",
                Some("Name".to_owned()),
                None,
                json!(["type", "Имя Юникод", "Name", "XdtoA"]),
            )
            .reference_id
        );
    }

    #[test]
    fn platform_catalog_contains_all_five_kinds_and_distinguishes_type_homonyms() {
        let references = platform_references();
        if PlatformDataInner::instance().all_types().is_empty() {
            return;
        }
        for kind in [
            PlatformReferenceKind::Type,
            PlatformReferenceKind::Method,
            PlatformReferenceKind::Property,
            PlatformReferenceKind::Constructor,
            PlatformReferenceKind::GlobalFunction,
        ] {
            assert!(references.iter().any(|item| item.kind == kind), "missing {kind:?}");
        }
        let homonyms: Vec<_> = references
            .iter()
            .filter(|item| item.kind == PlatformReferenceKind::Type && item.name == "ЭлементыФормы")
            .collect();
        assert!(homonyms.len() >= 2, "expected real platform homonyms: {homonyms:?}");
        let ids: std::collections::BTreeSet<_> =
            homonyms.iter().map(|item| item.reference_id.as_str()).collect();
        assert_eq!(ids.len(), homonyms.len());
        assert!(references.windows(2).all(|pair| {
            (
                pair[0].kind,
                pair[0].owner.to_lowercase(),
                pair[0].name.to_lowercase(),
                &pair[0].reference_id,
            ) <= (
                pair[1].kind,
                pair[1].owner.to_lowercase(),
                pair[1].name.to_lowercase(),
                &pair[1].reference_id,
            )
        }));
    }

    #[test]
    fn list_platform_filters_bilingual_names_and_preserves_sorted_dto() {
        if PlatformDataInner::instance().all_types().is_empty() {
            return;
        }
        let result = list_platform(Some(PlatformReferenceKind::Type), Some("array"), 6000).unwrap();
        let body = structured(&result);
        assert_eq!(body["action"], "list_platform");
        assert_eq!(body["schema_version"], "1");
        let items = body["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert!(items.iter().all(|item| item["kind"] == "type"));
        assert!(items.iter().any(|item| item["english_name"] == "Array"));
        assert_eq!(body["shown"], items.len());
        assert_eq!(body["total"], items.len());

        let ru = list_platform(Some(PlatformReferenceKind::Type), Some("мАсСиВ"), 6000).unwrap();
        assert!(structured(&ru)["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["name"] == "Массив"));

        let references = platform_references();
        let method = references
            .iter()
            .find(|item| {
                item.kind == PlatformReferenceKind::Method
                    && item.english_name.as_ref().is_some_and(|name| !name.is_empty())
            })
            .expect("the real catalog must contain a bilingual method");
        for query in
            [method.name.to_uppercase(), method.english_name.as_ref().unwrap().to_uppercase()]
        {
            let result =
                list_platform(Some(PlatformReferenceKind::Method), Some(&query), usize::MAX)
                    .unwrap();
            let items = structured(&result)["items"].as_array().unwrap();
            assert!(items.iter().all(|item| item["kind"] == "method"));
            assert!(items.iter().any(|item| item["reference_id"] == method.reference_id));
        }

        assert_eq!(non_empty(""), None);
    }

    #[test]
    fn list_platform_budget_keeps_a_stable_prefix_and_a_tiny_empty_envelope() {
        if PlatformDataInner::instance().all_types().is_empty() {
            return;
        }
        let first = list_platform(None, None, 1_000).unwrap();
        let second = list_platform(None, None, 1_000).unwrap();
        let body = structured(&first);
        let items = body["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert!(body["shown"].as_u64().unwrap() < body["total"].as_u64().unwrap());
        assert_eq!(body["budget_exhausted"], true);
        assert!(body["budget_hint"].as_str().is_some_and(|hint| hint.contains("kind/name")));
        assert!(serialized_bytes(&first) <= 4_000);
        assert_eq!(body, structured(&second));
        assert_eq!(extract_text(&first), extract_text(&second));

        let tiny = list_platform(None, None, 1).unwrap();
        let tiny_body = structured(&tiny);
        assert_eq!(tiny_body["action"], "list_platform");
        assert_eq!(tiny_body["schema_version"], "1");
        assert_eq!(tiny_body["shown"], 0);
        assert!(tiny_body["items"].as_array().unwrap().is_empty());
        assert_eq!(tiny_body["budget_exhausted"], true);
        assert!(tiny_body["budget_hint"].is_string());
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn every_catalog_kind_round_trips_by_exact_reference_id() {
        let references = platform_references();
        for (kind, expected) in [
            (PlatformReferenceKind::Type, "type"),
            (PlatformReferenceKind::Method, "method"),
            (PlatformReferenceKind::Property, "property"),
            (PlatformReferenceKind::Constructor, "constructor"),
            (PlatformReferenceKind::GlobalFunction, "global_function"),
        ] {
            let reference = references
                .iter()
                .find(|item| item.kind == kind)
                .unwrap_or_else(|| panic!("missing {expected} reference"));
            let result = bsl_syntax_help_by_reference_id(&reference.reference_id, 50_000).unwrap();
            assert_eq!(structured(&result)["kind"], expected, "{}", reference.reference_id);
            assert_eq!(structured(&result)["schema_version"], "2");
        }

        let missing = bsl_syntax_help_by_reference_id("type::missing~deadbeef", 6000).unwrap();
        assert_eq!(structured(&missing)["status"], "not_found");
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn homonymous_type_ids_open_distinct_exact_cards() {
        let references: Vec<_> = platform_references()
            .into_iter()
            .filter(|item| item.kind == PlatformReferenceKind::Type && item.name == "ЭлементыФормы")
            .collect();
        assert_eq!(references.len(), 2);
        let first = bsl_syntax_help_by_reference_id(&references[0].reference_id, 50_000).unwrap();
        let second = bsl_syntax_help_by_reference_id(&references[1].reference_id, 50_000).unwrap();
        assert_ne!(structured(&first), structured(&second));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn method_property_collisions_and_constructor_overloads_open_exact_cards() {
        let references = platform_references();
        let property = references
            .iter()
            .find(|property| {
                property.kind == PlatformReferenceKind::Property
                    && references.iter().any(|method| {
                        method.kind == PlatformReferenceKind::Method
                            && method.owner == property.owner
                            && method.name == property.name
                    })
            })
            .expect("the real catalog must contain a method/property collision");
        let method = references
            .iter()
            .find(|method| {
                method.kind == PlatformReferenceKind::Method
                    && method.owner == property.owner
                    && method.name == property.name
            })
            .unwrap();
        assert_ne!(method.reference_id, property.reference_id);
        for (reference, kind) in [(method, "method"), (property, "property")] {
            let card = bsl_syntax_help_by_reference_id(&reference.reference_id, 50_000).unwrap();
            assert_eq!(structured(&card)["kind"], kind);
        }

        let mut constructors_by_owner = std::collections::BTreeMap::<_, Vec<_>>::new();
        for constructor in
            references.iter().filter(|item| item.kind == PlatformReferenceKind::Constructor)
        {
            constructors_by_owner.entry(&constructor.owner).or_default().push(constructor);
        }
        let variants = constructors_by_owner
            .into_values()
            .find(|variants| variants.len() > 1)
            .expect("the real catalog must contain constructor overloads");
        assert_ne!(variants[0].reference_id, variants[1].reference_id);
        let first = bsl_syntax_help_by_reference_id(&variants[0].reference_id, 50_000).unwrap();
        let second = bsl_syntax_help_by_reference_id(&variants[1].reference_id, 50_000).unwrap();
        assert_eq!(structured(&first)["kind"], "constructor");
        assert_eq!(structured(&second)["kind"], "constructor");
        assert_ne!(structured(&first), structured(&second));
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn reference_document_builder_is_deterministic_and_complete() {
        let documents = build_reference_documents();
        let fields = |items: Vec<bsl_search::Document>| {
            items.into_iter().map(|item| (item.kind, item.title, item.body)).collect::<Vec<_>>()
        };
        assert_eq!(fields(build_reference_documents()), fields(build_reference_documents()));
        assert!(documents.windows(2).all(|pair| {
            (&pair[0].kind, &pair[0].title, &pair[0].body)
                <= (&pair[1].kind, &pair[1].title, &pair[1].body)
        }));
        let kinds: std::collections::BTreeSet<_> =
            documents.iter().map(|document| document.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["constructor", "global_function", "method", "property", "type"].into_iter().collect()
        );
    }

    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn reference_fingerprint_ignores_order_but_tracks_content() {
        let documents = build_reference_documents();
        let fingerprint = reference_documents_fingerprint(&documents);
        let mut reversed = build_reference_documents();
        reversed.reverse();
        assert_eq!(fingerprint, reference_documents_fingerprint(&reversed));

        let mut changed = build_reference_documents();
        changed[0].body.push_str(" changed");
        assert_ne!(fingerprint, reference_documents_fingerprint(&changed));
        assert_eq!(fingerprint.len(), 64);
    }

    fn extract_text(result: &CallToolResult) -> &str {
        result.content[0].as_text().expect("expected text content").text.as_str()
    }

    fn structured(result: &CallToolResult) -> &serde_json::Value {
        result.structured_content.as_ref().expect("expected structuredContent")
    }

    /// The dictionary's operational rule, for the one address class this tool
    /// owns: a published `syntax_help` reference has to open here.
    ///
    /// The round trip is the check, not a list of "which platform members are
    /// addressable" — such a list beside the code would be a second source of
    /// truth about this resolver and would drift from it in silence. It did:
    /// every property of every type was published with an address that resolved
    /// through the method table alone.
    #[test]
    fn every_published_platform_reference_opens_here() {
        let platform = PlatformDataInner::instance();
        if platform.all_types().is_empty() {
            return;
        }
        let db = ide::RootDatabaseImpl::new();

        let mut checked = 0usize;
        let mut kinds = std::collections::BTreeSet::new();
        for needle in ["Справочники", "Документы", "Массив", "СтрНайти", "ФоновыеЗадания"]
        {
            let query = ide::NameQuery::new(needle, 100)
                .with_categories(&[ide::NameCategory::PlatformMember]);
            for candidate in ide::lookup_names(&db, &query, &[]).candidates {
                let Some(reference) = &candidate.platform_ref else { continue };
                let (_, item) = syntax_help_card(&reference.name, reference.type_name.as_deref())
                    .unwrap_or_else(|error| {
                        panic!(
                            "`{}` (type {:?}) is published as a syntax_help address and this tool \
                             refuses it: {error}",
                            reference.name, reference.type_name,
                        )
                    });
                checked += 1;
                kinds.insert(match item {
                    SyntaxHelpItem::Type { .. } => "type",
                    SyntaxHelpItem::Method { .. } => "method",
                    SyntaxHelpItem::Constructor { .. } => "constructor",
                    SyntaxHelpItem::GlobalFunction { .. } => "global_function",
                    SyntaxHelpItem::Keyword { .. } => "keyword",
                    SyntaxHelpItem::Property { .. } => "property",
                });
            }
        }

        // Without these the loop is green on a stand that published nothing, or
        // only the easy class. A property is the class that used to be dead.
        assert!(checked >= 5, "too few references to prove anything: {checked}");
        assert!(kinds.contains("property"), "no property was published: {kinds:?}");
        assert!(kinds.len() >= 2, "only one kind of reference was exercised: {kinds:?}");
    }

    /// A wrong `type_name` is a miss, not a licence to answer about something
    /// else. `Справочники` is a property of the global context and of no other
    /// type; asked for on `Массив` it has to be refused, or a client with a typo
    /// gets a confident card about a different object.
    #[test]
    fn a_member_asked_for_on_the_wrong_type_is_refused() {
        let platform = PlatformDataInner::instance();
        if platform.all_types().is_empty() {
            return;
        }

        assert!(syntax_help_card("Справочники", Some("Массив")).is_err());
        // The premise: the same name on its own owner does answer, so the
        // refusal above is about the owner and not about the name being unknown.
        assert!(syntax_help_card("Справочники", Some("Global context")).is_ok());
    }

    #[test]
    fn test_syntax_help_type_lookup() {
        let platform = PlatformDataInner::instance();
        if platform.all_types().is_empty() {
            return;
        }

        let result = bsl_syntax_help("Массив", None, 6000).unwrap();
        let text = extract_text(&result);
        assert!(text.contains("Массив"), "should find Array type");
        assert!(text.contains("Array"), "should show english name");
        let body = structured(&result);
        assert_eq!(body["schema_version"], "2");
        assert_eq!(body["kind"], "type");
        assert_eq!(body["name"], "Массив");
        assert!(body["constructors"].is_array());
        assert!(body["methods"].is_array());
    }

    #[test]
    fn test_syntax_help_type_english() {
        let platform = PlatformDataInner::instance();
        if platform.all_types().is_empty() {
            return;
        }

        let result = bsl_syntax_help("Array", None, 6000).unwrap();
        let text = extract_text(&result);
        assert!(text.contains("Массив") || text.contains("Array"), "should find by english name");
    }

    #[test]
    fn test_syntax_help_constructor_only_type_renders_constructor() {
        let platform = PlatformDataInner::instance();
        // Skip only when no platform data is generated at all. Do NOT guard on the
        // constructor lookup itself — that would mask a bilingual-keying regression (the
        // RU type name `ОписаниеОповещения` must resolve to the constructor keyed under
        // the EN `CallbackDescription`), which is exactly what this test protects.
        if platform.all_constructors().is_empty() {
            return;
        }
        assert!(
            !platform.get_constructors("ОписаниеОповещения").is_empty(),
            "RU type name must resolve to the CallbackDescription constructor"
        );

        let result = bsl_syntax_help("ОписаниеОповещения", None, 6000).unwrap();
        let text = extract_text(&result);
        assert!(text.contains("Конструктор"), "constructor-only type must show its constructor");
        assert!(
            text.contains("Новый") || text.contains("ИмяПроцедуры"),
            "should show the Новый syntax or its parameters"
        );
        assert!(
            !text.contains("Методов нет."),
            "must not claim no surface when a constructor exists"
        );
    }

    #[test]
    fn test_syntax_help_method_with_type() {
        let platform = PlatformDataInner::instance();
        if platform.all_methods().is_empty() {
            return;
        }

        let method = &platform.all_methods()[0];
        let result = bsl_syntax_help(&method.name, Some(&method.type_name), 6000).unwrap();
        let text = extract_text(&result);
        assert!(text.contains(method.name.as_str()), "should contain method name");
        assert!(text.contains("```bsl"), "should have code block");
        let body = structured(&result);
        assert_eq!(body["kind"], "method");
        assert_eq!(body["matches"][0]["name"], method.name.as_str());
        assert_eq!(body["matches"][0]["owner_type"], method.type_name.as_str());
        assert!(body["matches"][0]["parameters"].is_array());
    }

    #[test]
    fn test_syntax_help_global_function() {
        let platform = PlatformDataInner::instance();
        if platform.all_global_functions().is_empty() {
            return;
        }

        let result = bsl_syntax_help("Сообщить", None, 6000).unwrap();
        let text = extract_text(&result);
        assert!(text.contains("Сообщить") || text.contains("Message"), "should find global fn");
        let body = structured(&result);
        assert_eq!(body["kind"], "global_function");
        assert_eq!(body["function"]["name"], "Сообщить");
    }

    #[test]
    fn test_syntax_help_card_is_bounded_by_the_output_budget() {
        let platform = PlatformDataInner::instance();
        if platform.all_types().is_empty() {
            return;
        }

        let full = extract_text(&bsl_syntax_help("Массив", None, 6000).unwrap()).to_string();
        let clipped = bsl_syntax_help("Массив", None, 100).unwrap();
        let clipped_text = extract_text(&clipped);
        assert!(clipped_text.len() < full.len(), "a 100-token budget must clip the card");
        assert_eq!(structured(&clipped)["status"], "budget_exhausted");
        assert_eq!(structured(&clipped)["budget_exhausted"], true);
        assert!(structured(&clipped)["budget_hint"].is_string());
    }

    fn pair_bytes(result: &CallToolResult) -> usize {
        extract_text(result).len() + serde_json::to_string(structured(result)).unwrap().len()
    }

    /// The response carries the Markdown and the card, so the ceiling covers both. `ЧтениеДанных`
    /// is the input that shows it: charging the text alone shipped 76 KB against a declared
    /// budget of 6000 tokens, of which 60 KB was the card nobody bounded.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn the_card_and_the_markdown_share_one_budget() {
        for name in ["ЧтениеДанных", "COMSafeArray", "Массив"] {
            let result = bsl_syntax_help(name, None, 6000).unwrap();

            assert!(pair_bytes(&result) <= 6000 * 4, "{name}: {} bytes", pair_bytes(&result));
        }
        assert_eq!(
            structured(&bsl_syntax_help("ЧтениеДанных", None, 6000).unwrap())["budget_exhausted"],
            true
        );
    }

    /// A budget smaller than the card identity returns the published minimal envelope instead of
    /// silently exceeding the ceiling with a partial entity.
    ///
    /// The control is the same lookup with room for the identity: it must still answer with the
    /// card. Without it the assertions below pass on an implementation that lets the Markdown
    /// spend the whole ceiling and then replaces every answer with the minimal envelope.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_budget_below_the_cards_identity_overshoots_by_that_much_and_says_so() {
        let tiny = bsl_syntax_help("Массив", None, 1).unwrap();
        let minimal = structured(&tiny);

        assert_eq!(minimal["budget_exhausted"], true);
        assert_eq!(minimal["status"], "budget_exhausted");
        assert!(minimal["budget_hint"].is_string());

        let result = bsl_syntax_help("Массив", None, 600).unwrap();
        let card = structured(&result);

        assert_eq!(card["kind"], "type", "{card}");
        assert_eq!(card["name"], "Массив", "{card}");
        assert!(!extract_text(&result).is_empty(), "усечённый Markdown не пропадает");
        assert!(pair_bytes(&result) <= 600 * 4 + 1024, "{} bytes", pair_bytes(&result));
    }

    /// A budget below one exact match returns the minimal envelope rather than an empty `matches`
    /// array that could be mistaken for not-found.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn a_matched_lookup_keeps_one_entry_at_any_budget() {
        let platform = PlatformDataInner::instance();
        let method = &platform.all_methods()[0];

        let result = bsl_syntax_help(&method.name, Some(&method.type_name), 1).unwrap();
        let card = structured(&result);

        assert_eq!(card["budget_exhausted"], true);
        assert_eq!(card["status"], "budget_exhausted");
    }

    /// The syntax helper marks availability only where the platform limits it, so an unmarked
    /// entry is available everywhere. Not hypothetical: every constructor in the data is
    /// unmarked, and an empty list would tell a machine consumer the exact opposite.
    #[test]
    #[cfg_attr(not(corpus_contract), ignore = "corpus contract: needs the platform help corpus")]
    fn an_unmarked_availability_reads_as_every_context() {
        let card = structured(&bsl_syntax_help("Граница", None, 6000).unwrap()).clone();

        assert_eq!(
            card["constructors"][0]["contexts"],
            json!([
                "thick_client",
                "thin_client",
                "web_client",
                "server",
                "mobile_client",
                "external_connection"
            ])
        );
    }

    /// Nothing was cut, so nothing may claim it was: a flag that is true either way tells a
    /// consumer nothing about whether the card it holds is complete.
    #[test]
    fn a_card_within_the_budget_is_not_flagged() {
        let card = structured(&bsl_syntax_help("Если", None, 6000).unwrap()).clone();

        assert_eq!(card["budget_exhausted"], false);
        assert_eq!(card["text_truncated"], false);
    }

    #[test]
    fn test_syntax_help_not_found() {
        let result = bsl_syntax_help("НесуществующийТипМетодФункция", None, 6000).unwrap();
        assert_eq!(structured(&result)["status"], "not_found");
        assert_eq!(structured(&result)["schema_version"], "2");
    }

    #[test]
    fn test_syntax_help_method_not_found_on_type() {
        let platform = PlatformDataInner::instance();
        if platform.all_types().is_empty() {
            return;
        }

        let result = bsl_syntax_help("НесуществующийМетод", Some("Массив"), 6000).unwrap();
        assert_eq!(structured(&result)["status"], "not_found");
    }
}
