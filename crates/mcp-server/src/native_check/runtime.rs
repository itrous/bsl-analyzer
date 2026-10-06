use std::time::Instant;
use tokio_util::sync::CancellationToken;

const RUNTIME_PASSPORT_CODE: &str = r#"Информация = Новый СистемнаяИнформация;
Контекст.Вставить("build", Информация.ВерсияПриложения);
Контекст.Вставить("identity", СтрокаСоединенияИнформационнойБазы());
Активные = Новый Массив;
Для Каждого Расш Из РасширенияКонфигурации.Получить(, ИсточникРасширенийКонфигурации.СеансАктивные) Цикл
    Данные = Новый Структура;
    Данные.Вставить("name", Расш.Имя);
    Данные.Вставить("active", Расш.Активно);
    Данные.Вставить("safe_mode", Расш.БезопасныйРежим);
    Данные.Вставить("scope", Строка(Расш.ОбластьДействия));
    Активные.Добавить(Данные);
КонецЦикла;
Контекст.Вставить("applied", Активные);
Отключенные = Новый Массив;
Для Каждого Расш Из РасширенияКонфигурации.Получить(, ИсточникРасширенийКонфигурации.СеансОтключенные) Цикл
    Отключенные.Добавить(Расш.Имя);
КонецЦикла;
Контекст.Вставить("disabled", Отключенные);"#;

const MAX_IDENTITY_BYTES: usize = 4096;
const MAX_BUILD_BYTES: usize = 64;
const MAX_EXTENSION_COUNT: usize = 128;
const MAX_EXTENSION_NAME_BYTES: usize = 128;
const MAX_EXTENSION_DATA_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeExtension {
    pub name: String,
    pub active: bool,
    pub safe_mode: bool,
    /// Normalized to `infobase`; data-separation scopes are rejected.
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerRuntimePassport {
    pub build: String,
    /// Retains the order returned by the current session; it does not prove compiler order.
    pub applied: Vec<RuntimeExtension>,
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimePassportError {
    Cancelled,
    DeadlineExceeded,
    Unavailable,
    IdentityMismatch,
    BuildMismatch,
    UnsupportedScope,
    InvalidPayload,
}

impl RuntimePassportError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Unavailable => "runtime_passport_unavailable",
            Self::IdentityMismatch => "source_identity_mismatch",
            Self::BuildMismatch => "runtime_build_mismatch",
            Self::UnsupportedScope => "unsupported_extension_scope",
            Self::InvalidPayload => "invalid_runtime_passport",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "The runtime passport request was cancelled",
            Self::DeadlineExceeded => "The runtime passport request exceeded its deadline",
            Self::Unavailable => "The runtime passport is unavailable",
            Self::IdentityMismatch => "The runtime identity does not match the configured source",
            Self::BuildMismatch => "The runtime build does not match the configured build",
            Self::UnsupportedScope => "The runtime reports an unsupported extension scope",
            Self::InvalidPayload => "The runtime passport has an invalid shape",
        }
    }
}

/// Read fixed, built-in runtime metadata. The checked user source is never passed here.
pub(crate) async fn server_runtime_passport(
    client: &onec_client::Client,
    configured_source: &str,
    expected_build: &str,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<ServerRuntimePassport, RuntimePassportError> {
    use onec_client::ExecuteRequest;

    let request = ExecuteRequest { code: RUNTIME_PASSPORT_CODE.to_owned() };
    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(RuntimePassportError::Cancelled),
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            return Err(RuntimePassportError::DeadlineExceeded);
        }
        response = client.execute_code(&request) => response,
    }
    .map_err(|_| RuntimePassportError::Unavailable)?;

    if !response.success {
        return Err(RuntimePassportError::Unavailable);
    }
    let context = response.context.as_ref().ok_or(RuntimePassportError::InvalidPayload)?;
    parse_runtime_passport(context, configured_source, expected_build)
}

fn parse_runtime_passport(
    context: &serde_json::Map<String, serde_json::Value>,
    configured_source: &str,
    expected_build: &str,
) -> Result<ServerRuntimePassport, RuntimePassportError> {
    if !has_exact_keys(context, &["build", "identity", "applied", "disabled"]) {
        return Err(RuntimePassportError::InvalidPayload);
    }
    let build = bounded_string(context.get("build"), 1, MAX_BUILD_BYTES)
        .ok_or(RuntimePassportError::InvalidPayload)?
        .to_owned();
    if !build.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return Err(RuntimePassportError::InvalidPayload);
    }
    if build != expected_build {
        return Err(RuntimePassportError::BuildMismatch);
    }

    let identity = bounded_string(context.get("identity"), 1, MAX_IDENTITY_BYTES)
        .ok_or(RuntimePassportError::InvalidPayload)?;
    if !identities_match(configured_source, identity) {
        return Err(RuntimePassportError::IdentityMismatch);
    }

    let applied_values = context
        .get("applied")
        .and_then(serde_json::Value::as_array)
        .filter(|items| items.len() <= MAX_EXTENSION_COUNT)
        .ok_or(RuntimePassportError::InvalidPayload)?;
    let disabled_values = context
        .get("disabled")
        .and_then(serde_json::Value::as_array)
        .filter(|items| items.len() <= MAX_EXTENSION_COUNT)
        .ok_or(RuntimePassportError::InvalidPayload)?;

    let mut applied = Vec::with_capacity(applied_values.len());
    let mut names = std::collections::HashSet::new();
    let mut total_name_bytes = 0usize;
    for value in applied_values {
        let object = value.as_object().ok_or(RuntimePassportError::InvalidPayload)?;
        if !has_exact_keys(object, &["name", "active", "safe_mode", "scope"]) {
            return Err(RuntimePassportError::InvalidPayload);
        }
        let name = bounded_string(object.get("name"), 1, MAX_EXTENSION_NAME_BYTES)
            .ok_or(RuntimePassportError::InvalidPayload)?;
        if name.trim().is_empty() || name.chars().any(char::is_control) || !names.insert(name) {
            return Err(RuntimePassportError::InvalidPayload);
        }
        total_name_bytes = total_name_bytes
            .checked_add(name.len())
            .filter(|size| *size <= MAX_EXTENSION_DATA_BYTES)
            .ok_or(RuntimePassportError::InvalidPayload)?;
        let active = object
            .get("active")
            .and_then(serde_json::Value::as_bool)
            .ok_or(RuntimePassportError::InvalidPayload)?;
        let safe_mode = object
            .get("safe_mode")
            .and_then(serde_json::Value::as_bool)
            .ok_or(RuntimePassportError::InvalidPayload)?;
        let scope = normalize_scope(
            bounded_string(object.get("scope"), 1, 64)
                .ok_or(RuntimePassportError::InvalidPayload)?,
        )?;
        applied.push(RuntimeExtension { name: name.to_owned(), active, safe_mode, scope });
    }

    let mut disabled = Vec::with_capacity(disabled_values.len());
    for value in disabled_values {
        let name = bounded_string(Some(value), 1, MAX_EXTENSION_NAME_BYTES)
            .ok_or(RuntimePassportError::InvalidPayload)?;
        if name.trim().is_empty() || name.chars().any(char::is_control) || !names.insert(name) {
            return Err(RuntimePassportError::InvalidPayload);
        }
        total_name_bytes = total_name_bytes
            .checked_add(name.len())
            .filter(|size| *size <= MAX_EXTENSION_DATA_BYTES)
            .ok_or(RuntimePassportError::InvalidPayload)?;
        disabled.push(name.to_owned());
    }

    Ok(ServerRuntimePassport { build, applied, disabled })
}

fn has_exact_keys(object: &serde_json::Map<String, serde_json::Value>, expected: &[&str]) -> bool {
    object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key))
}

fn bounded_string(
    value: Option<&serde_json::Value>,
    min_bytes: usize,
    max_bytes: usize,
) -> Option<&str> {
    let value = value?.as_str()?;
    (value.len() >= min_bytes && value.len() <= max_bytes).then_some(value)
}

fn normalize_scope(value: &str) -> Result<String, RuntimePassportError> {
    match value {
        "ИнформационнаяБаза" | "Информационная база" | "InfoBase" | "Infobase" => {
            Ok("infobase".to_owned())
        }
        "РазделениеДанных" | "DataSeparation" => {
            Err(RuntimePassportError::UnsupportedScope)
        }
        _ => Err(RuntimePassportError::InvalidPayload),
    }
}

fn identities_match(configured_source: &str, runtime_identity: &str) -> bool {
    let Some(configured) = super::source::parse_server_location(configured_source) else {
        return false;
    };
    let Some((runtime_server, runtime_ref)) = parse_connection_identity(runtime_identity) else {
        return false;
    };
    canonical_server(configured.host) == canonical_server(&runtime_server)
        && configured.base.eq_ignore_ascii_case(&runtime_ref)
}

fn canonical_server(value: &str) -> String {
    let value = value.trim();
    if value.starts_with('[') {
        return value.to_ascii_lowercase();
    }
    if let Some((host, port)) = value.rsplit_once(':') {
        if !host.contains(':') && !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
        {
            return format!("{}:{port}", host.trim_end_matches('.').to_ascii_lowercase());
        }
    }
    value.trim_end_matches('.').to_ascii_lowercase()
}

fn parse_connection_identity(value: &str) -> Option<(String, String)> {
    if value.len() > MAX_IDENTITY_BYTES || value.bytes().any(|byte| byte.is_ascii_control()) {
        return None;
    }
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ';' if !quoted => {
                fields.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if quoted {
        return None;
    }
    fields.push(current);

    let mut server = None;
    let mut infobase = None;
    let mut property_names = std::collections::HashSet::new();
    for field in fields {
        let Some((key, value)) = field.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() || !property_names.insert(key.to_ascii_lowercase()) {
            return None;
        }
        let value = value.trim();
        if key.eq_ignore_ascii_case("Srvr") {
            if server.is_some() {
                return None;
            }
            server = Some(normalize_connection_value(value)?);
        } else if key.eq_ignore_ascii_case("Ref") {
            if infobase.is_some() {
                return None;
            }
            infobase = Some(normalize_connection_value(value)?);
        }
    }
    Some((server?, infobase?))
}

fn normalize_connection_value(value: &str) -> Option<String> {
    let value = value.trim();
    let normalized = if value.starts_with('"') {
        if !value.ends_with('"') || value.len() < 2 {
            return None;
        }
        value[1..value.len() - 1].to_owned()
    } else {
        if value.contains('"') {
            return None;
        }
        value.to_owned()
    };
    if normalized.trim().is_empty()
        || normalized.len() > 255
        || normalized.chars().any(char::is_control)
    {
        return None;
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use super::{identities_match, parse_runtime_passport, RuntimePassportError};
    use serde_json::{json, Map};

    #[test]
    fn runtime_passport_schema_checks_identity_and_preserves_extension_array_order() {
        let context = json!({
            "build": "8.3.27.1989",
            "identity": "Srvr=\"APP.EXAMPLE.\";Ref=\"demo\";",
            "applied": [
                {"name":"First","active":true,"safe_mode":false,"scope":"InfoBase"},
                {"name":"Second","active":true,"safe_mode":true,"scope":"Информационная база"}
            ],
            "disabled": ["Dormant"]
        });
        let parsed =
            parse_runtime_passport(context.as_object().unwrap(), "app.example/demo", "8.3.27.1989")
                .unwrap();
        assert_eq!(
            parsed.applied.iter().map(|item| item.name.as_str()).collect::<Vec<_>>(),
            ["First", "Second"]
        );
        assert_eq!(parsed.disabled, vec!["Dormant".to_owned()]);
        assert_eq!(parsed.applied[0].scope, "infobase");
    }

    #[test]
    fn runtime_passport_rejects_unknown_scope_and_mismatched_source() {
        let context: Map<String, serde_json::Value> = json!({
            "build": "8.3.27.1989",
            "identity": "Srvr=server;Ref=demo",
            "applied": [{"name":"Scoped","active":true,"safe_mode":false,"scope":"РазделениеДанных"}],
            "disabled": []
        })
        .as_object()
        .unwrap()
        .clone();
        assert_eq!(
            parse_runtime_passport(&context, "server/demo", "8.3.27.1989"),
            Err(RuntimePassportError::UnsupportedScope)
        );
        assert!(!identities_match("other/demo", "Srvr=server;Ref=demo"));
    }
}
