mod epf_module;

pub use epf_module::{
    EpfModuleCheckResult, EpfModulePosition, EPF_MODULE_RESPONSE_BYTES, EPF_MODULE_TIMEOUT_MS,
};

use reqwest::header;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Request(#[source] reqwest::Error),

    #[error("unknown_execution_state: HTTP execution transport failed; the operation may still be running; do not retry automatically")]
    UnknownExecutionState(#[source] reqwest::Error),

    #[error("1C connection '{connection}': HTTP 404 with an empty body; the HTTP service or endpoint may be missing or unpublished; check the selected connection and service publication")]
    EndpointUnavailable { connection: String },

    #[error("invalid JSON response from 1C: {0}")]
    Decode(#[from] serde_json::Error),

    #[error("1C returned status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("EPF module check stopped before HTTP ({code}); no remote carrier was created")]
    ModuleCheckBeforeRequest { code: &'static str },

    #[error("Invalid EPF module check acknowledgement ({code}); remote cleanup is unconfirmed")]
    ModuleCheckProtocol { code: &'static str },

    #[error("{0}")]
    Validation(String),
}

#[derive(Clone)]
pub struct Client {
    base_url: String,
    user: Option<String>,
    password: Option<String>,
    http: reqwest::Client,
    execute_http: reqwest::Client,
    execute_timeout: Duration,
    connection_name: String,
}

/// Default execute budget, retained for profiles without an explicit override.
pub const DEFAULT_EXECUTE_TIMEOUT_MS: u64 = 30_000;

/// Validate a finite execute budget before constructing or forwarding a profile.
pub fn validate_execute_timeout_ms(timeout_ms: u64) -> Result<Duration, Error> {
    let timeout = Duration::from_millis(timeout_ms);
    if timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none() {
        return Err(Error::Validation(
            "execute_timeout_ms must be a positive finite duration".into(),
        ));
    }
    Ok(timeout)
}

impl Client {
    pub fn new(base_url: &str, user: &str, password: &str) -> Self {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::CONNECTION, header::HeaderValue::from_static("close"));

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .default_headers(headers.clone())
            .build()
            .expect("failed to build HTTP client");

        let execute_http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()
            .expect("failed to build HTTP client");

        let (user, password) = if user.is_empty() {
            (None, None)
        } else {
            (Some(user.to_string()), Some(password.to_string()))
        };

        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            user,
            password,
            http,
            execute_http,
            execute_timeout: Duration::from_millis(DEFAULT_EXECUTE_TIMEOUT_MS),
            connection_name: "default".into(),
        }
    }

    /// Configure only the execute budget; read/query/check requests retain their timeout.
    pub fn with_execute_timeout_ms(mut self, timeout_ms: u64) -> Result<Self, Error> {
        self.execute_timeout = validate_execute_timeout_ms(timeout_ms)?;
        Ok(self)
    }

    /// Attach the selected public profile name to endpoint diagnostics without exposing its URL.
    pub fn with_connection_name(mut self, connection: &str) -> Self {
        self.connection_name = self.redact_error_body(connection.to_owned());
        self
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
        let executing = endpoint == "/execute";
        let http = if executing { &self.execute_http } else { &self.http };
        let mut req = http.post(&url).json(body);
        if executing {
            req = req.timeout(self.execute_timeout);
        }

        if let Some(ref user) = self.user {
            req = req.basic_auth(user, self.password.as_deref());
        }

        let transport_error = |error: reqwest::Error| {
            let error = error.without_url();
            if executing {
                Error::UnknownExecutionState(error)
            } else {
                Error::Request(error)
            }
        };
        let resp = req.send().await.map_err(transport_error)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.map_err(transport_error)?;
            if status.as_u16() == 404 && body.trim().is_empty() {
                return Err(Error::EndpointUnavailable {
                    connection: self.connection_name.clone(),
                });
            }
            let mut body = self.redact_error_body(body);
            let mut end = body.len().min(4096);
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
            return Err(Error::Status { status: status.as_u16(), body });
        }

        let body = resp.bytes().await.map_err(transport_error)?;
        decode_json(&body)
    }

    fn redact_error_body(&self, mut body: String) -> String {
        // A server/proxy can echo a redirected or credential-bearing URL unrelated to base_url.
        for prefix in ["http://", "https://"] {
            while let Some(start) = body.to_ascii_lowercase().find(prefix) {
                let tail = &body[start..];
                let end = start
                    + tail
                        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
                        .unwrap_or(tail.len());
                body.replace_range(start..end, "<endpoint>");
            }
        }
        for credential in [self.user.as_deref(), self.password.as_deref()].into_iter().flatten() {
            if !credential.is_empty() {
                body = body.replace(credential, "<redacted>");
            }
        }
        body
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

#[derive(Debug, Deserialize, Serialize)]
pub struct ExecuteResult {
    pub success: bool,
    pub error: Option<String>,
    pub context: Option<serde_json::Map<String, serde_json::Value>>,
    pub duration_ms: Option<u64>,
    /// Absent on legacy producers; an empty array confirms supported but empty output.
    #[serde(default)]
    pub messages: Option<Vec<ExecuteMessage>>,
    #[serde(default)]
    pub messages_truncated: bool,
}

/// A user message in producer order, independent from the final execution status.
#[derive(Debug, Deserialize, Serialize)]
pub struct ExecuteMessage {
    pub text: String,
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
    /// Absent on legacy producers, independently from confirmed empty properties.
    #[serde(default)]
    pub read_properties: Option<serde_json::Value>,
    #[serde(default)]
    pub enum_values: Option<Vec<MetadataEnumValue>>,
    /// Runtime accounts, independently from configured XML Item identifiers.
    /// Missing means unavailable on the producer; an empty array is confirmed empty.
    #[serde(default)]
    pub predefined_accounts: Option<Vec<MetadataPredefinedAccount>>,
    #[serde(default)]
    pub predefined_unread: bool,
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

/// Identity of an infobase reference, never a configured PredefinedData Item UUID.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct MetadataAccountReference {
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub parent_uuid: Option<String>,
    /// `Some(true)` distinguishes a root account from an unread parent.
    #[serde(default)]
    pub parent_known_empty: Option<bool>,
}

/// An accounting flag addressed by its configured technical name.
#[derive(Debug, Deserialize, Serialize)]
pub struct MetadataAccountingFlag {
    pub name: String,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// One row of the account's native extra-dimension table, in native row order.
#[derive(Debug, Deserialize, Serialize)]
pub struct MetadataAccountDimension {
    #[serde(default)]
    pub predefined_name: Option<String>,
    #[serde(default)]
    pub runtime_ref_uuid: Option<String>,
    #[serde(default)]
    pub turnover: Option<bool>,
    #[serde(default)]
    pub predefined: Option<bool>,
    #[serde(default)]
    pub accounting_flags: Option<Vec<MetadataAccountingFlag>>,
}

/// A live predefined account. Its name is technical; runtime UUIDs stay separate
/// from source metadata identities and every unread property remains unknown.
#[derive(Debug, Deserialize, Serialize)]
pub struct MetadataPredefinedAccount {
    pub name: String,
    #[serde(default)]
    pub runtime_reference: MetadataAccountReference,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub account_type: Option<String>,
    #[serde(default)]
    pub off_balance: Option<bool>,
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub accounting_flags: Option<Vec<MetadataAccountingFlag>>,
    #[serde(default)]
    pub ext_dimension_types: Option<Vec<MetadataAccountDimension>>,
}

/// An enumeration value reported by live metadata, preserving its technical name.
#[derive(Debug, Deserialize, Serialize)]
pub struct MetadataEnumValue {
    pub name: String,
    pub synonym: String,
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

    fn http_response(
        status: &str,
        body: &str,
        delay: Duration,
    ) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = [0; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("POST /execute ") || request.starts_with("POST /query "));
            std::thread::sleep(delay);
            let _ = stream.write_all(response.as_bytes());
            listener.set_nonblocking(true).unwrap();
            let mut count = 1;
            let until = std::time::Instant::now() + Duration::from_millis(100);
            while std::time::Instant::now() < until {
                match listener.accept() {
                    Ok(_) => count += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("test server failed: {error}"),
                }
            }
            count
        });
        (url, handle)
    }

    #[test]
    fn execute_budget_defaults_and_validation() {
        let client = Client::new("http://localhost", "", "");
        assert_eq!(client.execute_timeout, Duration::from_secs(30));
        assert!(client.with_execute_timeout_ms(0).is_err());
        assert_eq!(validate_execute_timeout_ms(60_000).unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn execute_messages_distinguish_legacy_empty_and_failure_output() {
        let legacy: ExecuteResult = decode_json(br#"{"success":true}"#).unwrap();
        assert!(legacy.messages.is_none());
        let empty: ExecuteResult = decode_json(br#"{"success":true,"messages":[]}"#).unwrap();
        assert!(empty.messages.unwrap().is_empty());
        let failure: ExecuteResult = decode_json(br#"{"success":false,"error":"failed","messages":[{"text":"first"},{"text":"second"}],"messages_truncated":true}"#).unwrap();
        assert!(!failure.success);
        assert_eq!(
            failure.messages.unwrap().iter().map(|item| item.text.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(failure.messages_truncated);
    }

    #[tokio::test]
    #[ignore = "31-second HTTP integration probe; run explicitly with --ignored"]
    async fn execute_can_complete_after_the_legacy_thirty_second_budget() {
        let (url, server) = http_response("200 OK", r#"{"success":true}"#, Duration::from_secs(31));
        let result = Client::new(&url, "", "")
            .with_execute_timeout_ms(45_000)
            .unwrap()
            .execute_code(&ExecuteRequest { code: "x=1;".into() })
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(server.join().unwrap(), 1);
    }

    #[tokio::test]
    async fn execute_timeout_has_unknown_outcome_and_no_retry_while_server_continues() {
        let (url, server) =
            http_response("200 OK", r#"{"success":true}"#, Duration::from_millis(150));
        let error = Client::new(&url, "private-user", "private-password")
            .with_execute_timeout_ms(25)
            .unwrap()
            .execute_code(&ExecuteRequest { code: "x=1;".into() })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::UnknownExecutionState(_)));
        assert!(error.to_string().contains("unknown_execution_state"));
        assert!(!error.to_string().contains(&url));
        assert_eq!(server.join().unwrap(), 1, "the independent server marker completes once");
    }

    #[tokio::test]
    async fn execute_short_success_and_server_failure_are_terminal() {
        for (status, success) in [("200 OK", true), ("500 Internal Server Error", false)] {
            let (url, server) = http_response(status, r#"{"success":true}"#, Duration::ZERO);
            let result = Client::new(&url, "", "")
                .execute_code(&ExecuteRequest { code: "x=1;".into() })
                .await;
            if success {
                assert!(result.unwrap().success);
            } else {
                assert!(matches!(result, Err(Error::Status { status: 500, .. })));
            }
            assert_eq!(server.join().unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn execute_does_not_follow_a_redirect_to_a_second_run() {
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let status = format!(
            "307 Temporary Redirect\r\nLocation: http://{}/execute",
            target.local_addr().unwrap()
        );
        let (url, server) = http_response(&status, "", Duration::ZERO);
        let result =
            Client::new(&url, "", "").execute_code(&ExecuteRequest { code: "x=1;".into() }).await;
        assert!(matches!(result, Err(Error::Status { status: 307, .. })));
        assert_eq!(server.join().unwrap(), 1);
        target.set_nonblocking(true).unwrap();
        assert_eq!(target.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn read_query_keeps_its_existing_redirect_policy() {
        let (target_url, target) = http_response(
            "200 OK",
            r#"{"columns":[],"rows":[],"total":0,"truncated":false}"#,
            Duration::ZERO,
        );
        let status = format!("307 Temporary Redirect\r\nLocation: {target_url}/query");
        let (url, redirect) = http_response(&status, "", Duration::ZERO);
        let result = Client::new(&url, "", "")
            .execute_query(&QueryRequest {
                query: "SELECT 1".into(),
                limit: 1,
                parameters: HashMap::new(),
            })
            .await
            .unwrap();
        assert_eq!(result.total, 0);
        assert_eq!(redirect.join().unwrap(), 1);
        assert_eq!(target.join().unwrap(), 1);
    }

    #[tokio::test]
    async fn empty_404_names_connection_without_disclosing_endpoint_or_credentials() {
        let (url, server) = http_response("404 Not Found", "", Duration::ZERO);
        let error = Client::new(&url, "private-user", "private-password")
            .with_connection_name("test-base")
            .execute_code(&ExecuteRequest { code: "x=1;".into() })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::EndpointUnavailable { .. }));
        let text = error.to_string();
        assert!(text.contains("test-base") && text.contains("unpublished"));
        for secret in [&url, "private-user", "private-password"] {
            assert!(!text.contains(secret));
        }
        assert_eq!(server.join().unwrap(), 1);
    }

    #[tokio::test]
    async fn endpoint_profile_diagnostic_redacts_urls_and_configured_credentials() {
        for name in [
            "https://private-user:private-password@private.example/base?token=value",
            "profile-private-user-private-password",
        ] {
            let (url, server) = http_response("404 Not Found", "", Duration::ZERO);
            let error = Client::new(&url, "private-user", "private-password")
                .with_connection_name(name)
                .execute_code(&ExecuteRequest { code: "x=1;".into() })
                .await
                .unwrap_err();
            assert_eq!(server.join().unwrap(), 1);
            assert!(matches!(error, Error::EndpointUnavailable { .. }));
            let message = error.to_string();
            for secret in ["private-user", "private-password", "private.example", "token=value"] {
                assert!(!message.contains(secret), "profile diagnostic disclosed {secret}");
            }
        }
    }

    #[tokio::test]
    async fn nonempty_query_404_retains_ordinary_status_without_publication_advice() {
        let (url, server) =
            http_response("404 Not Found", "unknown query parameter", Duration::ZERO);
        let error = Client::new(&url, "", "")
            .execute_query(&QueryRequest {
                query: "SELECT 1".into(),
                limit: 1,
                parameters: HashMap::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(server.join().unwrap(), 1);
        let Error::Status { status, body } = error else {
            panic!("a nonempty query error must preserve its HTTP status");
        };
        assert_eq!(status, 404);
        assert_eq!(body, "unknown query parameter");
    }

    #[tokio::test]
    async fn ordinary_http_error_is_utf8_safe_and_redacts_private_endpoints() {
        let body = format!("{} {}", "x".repeat(4094), "я".repeat(100));
        let (url, server) = http_response("400 Bad Request", &body, Duration::ZERO);
        let error = Client::new(&url, "", "")
            .execute_code(&ExecuteRequest { code: "x=1;".into() })
            .await
            .unwrap_err();
        let Error::Status { status, body } = error else {
            panic!("expected ordinary status");
        };
        assert_eq!(status, 400);
        assert_eq!(body.len(), 4095, "the cap must not split a multibyte character");
        assert_eq!(server.join().unwrap(), 1);
        let client = Client::new("https://internal.example/base", "user", "password-value");
        let redacted = client.redact_error_body("https://user:password-value@other.example/private?token=x password-value https://internal.example/base HTTP://upper.example/private HtTpS://mixed.example/private".into());
        for secret in [
            "other.example",
            "internal.example",
            "password-value",
            "token=x",
            "upper.example",
            "mixed.example",
        ] {
            assert!(!redacted.contains(secret));
        }
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
    fn live_metadata_read_properties_distinguish_legacy_and_confirmed_values() {
        let mut value =
            serde_json::json!({"name":"Test", "full_name":"Catalog.Test", "synonym":"Test"});
        let legacy: MetadataStructureResult = serde_json::from_value(value.clone()).unwrap();
        assert!(legacy.read_properties.is_none());
        assert!(legacy.enum_values.is_none());
        value["read_properties"] = serde_json::json!({"hierarchical":false,"code_length":0});
        value["enum_values"] = serde_json::json!([]);
        let current: MetadataStructureResult = serde_json::from_value(value).unwrap();
        assert_eq!(current.read_properties.unwrap()["hierarchical"], false);
        assert!(current.enum_values.unwrap().is_empty());
    }

    #[test]
    fn metadata_predefined_accounts_preserve_runtime_and_unknown_identities() {
        let mut fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/live_predefined_accounts.json"))
                .unwrap();
        let current: MetadataStructureResult = serde_json::from_value(fixture.clone()).unwrap();
        let accounts = current.predefined_accounts.unwrap();
        assert_eq!(accounts[0].runtime_reference.parent_known_empty, Some(true));
        assert!(accounts[0].runtime_reference.parent_uuid.is_none());
        assert_eq!(accounts[1].runtime_reference.parent_uuid, accounts[0].runtime_reference.uuid);
        assert_eq!(accounts[0].accounting_flags.as_ref().unwrap()[0].enabled, Some(false));
        let dimensions = accounts[0].ext_dimension_types.as_ref().unwrap();
        assert_eq!(
            dimensions[0].predefined_name.as_deref(),
            Some("ChartOfCharacteristicTypes.OwnKinds.Second")
        );
        assert_eq!(dimensions[1].turnover, Some(true));
        fixture["predefined_accounts"][0]["runtime_reference"] = serde_json::json!({});
        let unknown: MetadataStructureResult = serde_json::from_value(fixture.clone()).unwrap();
        assert!(unknown.predefined_accounts.unwrap()[0]
            .runtime_reference
            .parent_known_empty
            .is_none());
        fixture["predefined_accounts"] = serde_json::json!([]);
        let empty: MetadataStructureResult = serde_json::from_value(fixture.clone()).unwrap();
        assert!(empty.predefined_accounts.unwrap().is_empty());
        fixture.as_object_mut().unwrap().remove("predefined_accounts");
        let legacy: MetadataStructureResult = serde_json::from_value(fixture).unwrap();
        assert!(legacy.predefined_accounts.is_none());
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
