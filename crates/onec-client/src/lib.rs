use reqwest::header;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("invalid JSON response from 1C: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("1C returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("{0}")]
    Validation(String),
}

#[derive(Clone)]
pub struct Client {
    base_url: String,
    user: Option<String>,
    password: Option<String>,
    http: reqwest::Client,
}

/// Process-wide HTTP client: building one makes the TLS verifier read the system root
/// certificates and warn about every unreadable file, so all [`Client`]s share a single instance.
static SHARED_HTTP: Mutex<Option<reqwest::Client>> = Mutex::new(None);

#[cfg(test)]
static HTTP_BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn build_http() -> Result<reqwest::Client, reqwest::Error> {
    #[cfg(test)]
    HTTP_BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

    let mut headers = header::HeaderMap::new();
    headers.insert(header::CONNECTION, header::HeaderValue::from_static("close"));

    reqwest::Client::builder().timeout(Duration::from_secs(30)).default_headers(headers).build()
}

/// Returns the client held in `slot`, building it on first use. A failed build leaves the slot
/// empty, so the next call builds again and reports its own error.
fn shared_or_build<T: Clone, E>(
    slot: &Mutex<Option<T>>,
    build: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(value) = slot.as_ref() {
        return Ok(value.clone());
    }
    let value = build()?;
    *slot = Some(value.clone());
    Ok(value)
}

impl Client {
    /// All clients of a process share one HTTP client, so the system root certificates are read
    /// once per process: changes to the system trust store become visible after a restart.
    ///
    /// # Panics
    ///
    /// Panics if the HTTP client cannot be built; the failure is not cached, so a later call
    /// builds it again.
    pub fn new(base_url: &str, user: &str, password: &str) -> Self {
        let http = shared_or_build(&SHARED_HTTP, build_http).expect("failed to build HTTP client");

        let (user, password) = if user.is_empty() {
            (None, None)
        } else {
            (Some(user.to_string()), Some(password.to_string()))
        };

        Self { base_url: base_url.trim_end_matches('/').to_string(), user, password, http }
    }

    pub async fn execute_query(&self, request: &QueryRequest) -> Result<QueryResult, Error> {
        self.post("/query", request).await
    }

    pub async fn validate_query(
        &self,
        request: &ValidateQueryRequest,
    ) -> Result<ValidateQueryResult, Error> {
        self.post("/validate-query", request).await
    }

    pub async fn check_syntax(
        &self,
        request: &CheckSyntaxRequest,
    ) -> Result<CheckSyntaxResult, Error> {
        self.post("/check-syntax", request).await
    }

    pub async fn execute_code(&self, request: &ExecuteRequest) -> Result<ExecuteResult, Error> {
        self.post("/execute", request).await
    }

    pub async fn eval_expression(&self, request: &EvalRequest) -> Result<EvalResult, Error> {
        self.post("/eval", request).await
    }

    pub async fn event_log(&self, request: &EventLogRequest) -> Result<EventLogResult, Error> {
        self.post("/event-log", request).await
    }

    pub async fn list_metadata(
        &self,
        request: &MetadataListRequest,
    ) -> Result<MetadataListResult, Error> {
        self.post("/metadata-list", request).await
    }

    pub async fn metadata_structure(
        &self,
        request: &MetadataStructureRequest,
    ) -> Result<MetadataStructureResult, Error> {
        self.post("/metadata-structure", request).await
    }

    async fn post<T: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        endpoint: &str,
        body: &T,
    ) -> Result<R, Error> {
        let url = format!("{}{}", self.base_url, endpoint);
        let mut req = self.http.post(&url).json(body);

        if let Some(ref user) = self.user {
            req = req.basic_auth(user, self.password.as_deref());
        }

        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let body = if body.len() > 4096 { body[..4096].to_string() } else { body };
            return Err(Error::Status { status: status.as_u16(), body });
        }

        let body = resp.bytes().await?;
        decode_json(&body)
    }
}

fn decode_json<R: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<R, Error> {
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    Ok(serde_json::from_slice(body)?)
}

#[derive(Debug, Serialize)]
pub struct QueryRequest {
    pub query: String,
    pub limit: u32,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub parameters: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub total: u32,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct ValidateQueryRequest {
    pub query: String,
}

#[derive(Debug, Deserialize)]
pub struct ValidateQueryResult {
    pub valid: bool,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckSyntaxRequest {
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct CheckSyntaxResult {
    pub valid: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExecuteRequest {
    pub code: String,
}

#[derive(Debug, Deserialize)]
pub struct ExecuteResult {
    pub success: bool,
    pub error: Option<String>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct EvalRequest {
    pub expression: String,
}

#[derive(Debug, Deserialize)]
pub struct EvalResult {
    pub success: bool,
    pub result: Option<serde_json::Value>,
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    pub error: Option<String>,
}

/// Filters for a read of the 1C event log (журнал регистрации). Dates are ISO-8601
/// strings; every filter is optional and omitted from the payload when `None` so the
/// 1C side falls back to no restriction on that dimension.
#[derive(Debug, Default, Serialize)]
pub struct EventLogRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    pub limit: u32,
}

#[derive(Debug, Deserialize)]
pub struct EventLogResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub total: u32,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct MetadataListRequest {
    pub meta_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name_mask: Option<String>,
    pub limit: u32,
}

#[derive(Debug, Deserialize)]
pub struct MetadataListItem {
    pub name: String,
    pub full_name: String,
    pub synonym: String,
}

#[derive(Debug, Deserialize)]
pub struct MetadataListResult {
    pub items: Vec<MetadataListItem>,
    pub returned: u32,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct MetadataStructureRequest {
    pub meta_type: String,
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MetadataStructureResult {
    pub name: String,
    pub full_name: String,
    pub synonym: String,
    #[serde(rename = "СтандартныеРеквизиты", default)]
    pub standard_attributes: Vec<MetadataStructureItem>,
    #[serde(rename = "Реквизиты", default)]
    pub attributes: Vec<MetadataStructureItem>,
    #[serde(rename = "Измерения", default)]
    pub dimensions: Vec<MetadataStructureItem>,
    #[serde(rename = "Ресурсы", default)]
    pub resources: Vec<MetadataStructureItem>,
    #[serde(rename = "ТабличныеЧасти", default)]
    pub tabular_sections: Vec<MetadataTabularSection>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MetadataTabularSection {
    pub name: String,
    pub synonym: String,
    /// `None` is a legacy producer that did not return the section's fields;
    /// `Some([])` is a confirmed empty section.
    #[serde(default)]
    pub attributes: Option<Vec<MetadataStructureItem>>,
}

#[derive(Debug, Serialize)]
pub struct MetadataStructureItem {
    pub name: String,
    pub synonym: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(rename = "type_variants", skip_serializing_if = "Vec::is_empty")]
    pub type_variants: Vec<MetadataTypeVariant>,
}

impl<'de> Deserialize<'de> for MetadataStructureItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawItem {
            name: String,
            synonym: String,
            #[serde(rename = "type")]
            type_name: Option<String>,
            #[serde(rename = "typeVariants")]
            type_variants: Option<Vec<MetadataTypeVariant>>,
        }

        let raw = RawItem::deserialize(deserializer)?;
        let type_variants = raw.type_variants.unwrap_or_else(|| {
            raw.type_name
                .as_ref()
                .map(|presentation| {
                    vec![MetadataTypeVariant {
                        technical_name: None,
                        presentation: presentation.clone(),
                        resolution: "unresolved",
                        reason: Some("legacy_type_only"),
                    }]
                })
                .unwrap_or_default()
        });
        Ok(Self { name: raw.name, synonym: raw.synonym, type_name: raw.type_name, type_variants })
    }
}

#[derive(Debug, Serialize)]
pub struct MetadataTypeVariant {
    pub technical_name: Option<String>,
    pub presentation: String,
    pub resolution: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

impl<'de> Deserialize<'de> for MetadataTypeVariant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct RawVariant {
            #[serde(rename = "technicalName")]
            technical_name: Option<String>,
            presentation: String,
        }

        let raw = RawVariant::deserialize(deserializer)?;
        let (technical_name, resolution, reason) = match raw.technical_name {
            Some(name) if known_producer_type_id(&name) => (Some(name), "source", None),
            Some(_) => (None, "unresolved", Some("unknown_technical_name")),
            None => (None, "unresolved", Some("technical_name_unavailable")),
        };
        Ok(Self { technical_name, presentation: raw.presentation, resolution, reason })
    }
}

fn known_producer_type_id(name: &str) -> bool {
    const PLATFORM: &[&str] = &[
        "Строка",
        "Число",
        "Булево",
        "Дата",
        "Неопределено",
        "Null",
        "Массив",
        "Структура",
        "Соответствие",
        "ТаблицаЗначений",
        "ДеревоЗначений",
        "СписокЗначений",
        "ОписаниеТипов",
        "УникальныйИдентификатор",
    ];
    const APPLIED: &[&str] = &[
        "СправочникСсылка",
        "ДокументСсылка",
        "ПеречислениеСсылка",
        "ЗадачаСсылка",
        "БизнесПроцессСсылка",
        "ОбработкаОбъект",
        "ОтчетОбъект",
        "ПланОбменаСсылка",
        "ПланСчетовСсылка",
        "ПланВидовХарактеристикСсылка",
        "ПланВидовРасчетаСсылка",
        "РегистрСведенийКлючЗаписи",
        "РегистрНакопленияКлючЗаписи",
        "РегистрБухгалтерииКлючЗаписи",
        "РегистрРасчетаКлючЗаписи",
    ];

    PLATFORM.contains(&name)
        || name.split_once('.').is_some_and(|(prefix, object)| {
            APPLIED.contains(&prefix)
                && !object.is_empty()
                && object.chars().all(|character| character == '_' || character.is_alphanumeric())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn clients_of_a_process_build_the_http_client_once() {
        let first = Client::new("http://localhost:1", "", "");
        let second = Client::new("http://localhost:2", "user", "secret");

        assert_eq!(HTTP_BUILDS.load(Ordering::SeqCst), 1);
        assert_eq!(first.base_url, "http://localhost:1");
        assert_eq!(second.base_url, "http://localhost:2");
        assert_eq!(second.user.as_deref(), Some("user"));
        assert_eq!(first.user, None);
    }

    #[test]
    fn failed_build_is_not_cached() {
        let slot = Mutex::new(None);
        let mut attempts = 0;

        for _ in 0..2 {
            let result: Result<u32, &str> = shared_or_build(&slot, || {
                attempts += 1;
                Err("boom")
            });
            assert_eq!(result, Err("boom"));
        }
        assert_eq!(attempts, 2);

        assert_eq!(shared_or_build(&slot, || Ok::<_, &str>(7)), Ok(7));
        assert_eq!(shared_or_build(&slot, || Err("must not rebuild")), Ok(7));
    }

    #[test]
    fn metadata_list_omits_empty_mask() {
        let request =
            MetadataListRequest { meta_type: "Constants".into(), name_mask: None, limit: 10 };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["meta_type"], "Constants");
        assert!(value.get("name_mask").is_none());
        assert_eq!(value["limit"], 10);
    }

    #[test]
    fn metadata_structure_contract_is_stable() {
        let request = MetadataStructureRequest {
            meta_type: "Catalogs".into(),
            name: "Организации".into(),
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["meta_type"], "Catalogs");
        assert_eq!(value["name"], "Организации");
    }

    #[test]
    fn metadata_structure_types_known_variants_and_tolerates_future_fields() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../bsl-analyzer/tests/fixtures/live_metadata_type_variants.json"
        ))
        .unwrap();

        for locale in ["ru", "en"] {
            let result: MetadataStructureResult =
                serde_json::from_value(fixture[locale].clone()).unwrap();
            let find = |name| result.attributes.iter().find(|item| item.name == name).unwrap();

            for name in ["Primitive", "Platform", "Applied", "ReportApplied"] {
                let variant = &find(name).type_variants[0];
                assert_eq!(variant.resolution, "source");
                assert!(variant.technical_name.is_some());
                assert!(variant.reason.is_none());
            }
            assert_eq!(find("Composite").type_variants.len(), 2);
            let same = &find("SamePresentation").type_variants;
            assert_eq!(same.len(), 2);
            assert_eq!(same[0].presentation, same[1].presentation);
            assert_ne!(same[0].technical_name, same[1].technical_name);
            assert!(same.iter().all(|variant| variant.resolution == "source"));

            let unsupported = &find("Unsupported").type_variants[0];
            assert_eq!(unsupported.resolution, "unresolved");
            assert_eq!(unsupported.reason, Some("technical_name_unavailable"));

            let future = &find("FutureUnknown").type_variants[0];
            assert!(future.technical_name.is_none());
            assert_eq!(future.resolution, "unresolved");
            assert_eq!(future.reason, Some("unknown_technical_name"));

            let normalized = serde_json::to_value(&result).unwrap();
            let sections = normalized["ТабличныеЧасти"].as_array().unwrap();
            let rows = sections.iter().find(|item| item["name"] == "Rows").unwrap();
            let fields =
                rows["attributes"].as_array().expect("new producer attributes survive DTO");
            assert_eq!(fields[0]["type_variants"].as_array().unwrap().len(), 2);
            assert_eq!(
                sections.iter().find(|item| item["name"] == "EmptyRows").unwrap()["attributes"],
                serde_json::json!([])
            );
        }
    }

    #[test]
    fn legacy_metadata_type_stays_unresolved_across_infobases() {
        let result: MetadataStructureResult = serde_json::from_str(include_str!(
            "../../bsl-analyzer/tests/fixtures/live_metadata_legacy.json"
        ))
        .unwrap();

        assert_eq!(result.attributes.len(), 2);
        assert_eq!(
            serde_json::to_value(&result).unwrap()["ТабличныеЧасти"][0]["attributes"],
            serde_json::Value::Null,
            "missing legacy tabular attributes must not become a confirmed empty list"
        );
        assert_eq!(
            result.attributes[0].type_variants[0].presentation,
            result.attributes[1].type_variants[0].presentation
        );
        for attribute in result.attributes {
            let variant = &attribute.type_variants[0];
            assert!(variant.technical_name.is_none(), "must not resolve from workspace names");
            assert_eq!(variant.resolution, "unresolved");
            assert_eq!(variant.reason, Some("legacy_type_only"));
        }
    }

    #[test]
    fn metadata_structure_compatibility_matrix_keeps_legacy_type() {
        const RELEASE: &str = "v0.2.70";
        const COMMIT: &str = "d7e50c494995ee8dee742a6236b16134d2a42e87";
        const SOURCE_SHA256: &str =
            "2d3afd71024f10162c97666a895852784121d8270a117e0212a717efee9905b2";
        let old: serde_json::Value = serde_json::from_str(include_str!(
            "../../bsl-analyzer/tests/fixtures/live_metadata_legacy.json"
        ))
        .unwrap();
        let new_fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../bsl-analyzer/tests/fixtures/live_metadata_type_variants.json"
        ))
        .unwrap();
        let new = new_fixture["ru"].clone();

        assert_eq!(
            new_fixture["_fixture"]["producer_contract"],
            "typeVariants-tabularAttributes-v1"
        );
        assert_eq!(new_fixture["_fixture"]["producer_http_version"], "1.2.0");
        assert_eq!(new_fixture["_fixture"]["producer_extension_version"], "1.1.0");

        for fixture in [&old, &new_fixture] {
            assert_eq!(fixture["_fixture"]["version"], "1");
            assert_eq!(fixture["_fixture"]["legacy_consumer_release"], RELEASE);
            assert_eq!(fixture["_fixture"]["legacy_consumer_commit"], COMMIT);
            assert_eq!(fixture["_fixture"]["legacy_onec_client_source_sha256"], SOURCE_SHA256);
        }

        // New consumer × old/new producer responses.
        for response in [old.clone(), new.clone()] {
            let parsed: MetadataStructureResult = serde_json::from_value(response).unwrap();
            assert!(parsed.attributes.iter().all(|item| item.type_name.is_some()));
        }

        // Frozen v0.2.70 used `serde_json::Value` as MetadataStructureResult. It reads the
        // preserved `type` field and ignores every added field in both response versions.
        for response in [old, new] {
            let legacy: serde_json::Value = serde_json::from_value(response).unwrap();
            let attributes = legacy["Реквизиты"].as_array().unwrap();
            assert!(attributes.iter().all(|item| item["type"].is_string()));
        }
    }

    #[test]
    fn response_decoder_accepts_utf8_bom_emitted_by_1c() {
        let result: QueryResult = decode_json(
            b"\xEF\xBB\xBF{\"columns\":[\"Value\"],\"rows\":[[1]],\"total\":1,\"truncated\":false}",
        )
        .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.rows[0][0], 1);
    }
}
