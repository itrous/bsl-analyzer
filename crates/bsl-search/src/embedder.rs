use crate::error::{EmbeddingFailure, EmbeddingFailureCode, SearchError};
use crate::ports::EmbeddingGenerator;
use serde::{Deserialize, Serialize};
use std::ops::Range;

#[derive(Debug, Clone)]
pub struct EmbedderConfig {
    pub base_url: String,
    pub model: String,
    pub dim: Option<usize>,
    pub api_key: Option<String>,
    pub provider: Option<String>,
    pub query_prefix: String,
    pub document_prefix: String,
    pub max_request_bytes: usize,
    /// Exact, already-loaded tokenizer policy shared by clones for one embedding pass.
    pub token_policy: Option<crate::TokenPolicy>,
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434".to_owned(),
            model: "qwen3-embedding".to_owned(),
            dim: Some(1024),
            api_key: None,
            provider: None,
            query_prefix: String::new(),
            document_prefix: String::new(),
            max_request_bytes: Self::DEFAULT_MAX_REQUEST_BYTES,
            token_policy: None,
        }
    }
}

impl EmbedderConfig {
    pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1_048_576;

    pub fn request_bytes_from_env() -> Result<usize, SearchError> {
        match std::env::var("EMBEDDING_MAX_REQUEST_BYTES") {
            Ok(value) => Self::parse_request_bytes(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::parse_request_bytes(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(invalid_config()),
        }
    }

    fn parse_request_bytes(value: Option<&str>) -> Result<usize, SearchError> {
        match value {
            None => Ok(Self::DEFAULT_MAX_REQUEST_BYTES),
            Some(value) => {
                value.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(invalid_config)
            }
        }
    }

    /// `EMBEDDING_DIM` as declared. `None` means unset: the request omits `dimensions`
    /// and the model answers in its native width. A set-but-unusable value is a
    /// configuration error rather than a silent fallback, since a typo must not quietly
    /// choose the width the index is built for.
    pub fn dim_from_env() -> Result<Option<usize>, SearchError> {
        match std::env::var("EMBEDDING_DIM") {
            Ok(value) => Self::parse_dim(Some(&value)),
            Err(std::env::VarError::NotPresent) => Self::parse_dim(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(invalid_config()),
        }
    }

    fn parse_dim(value: Option<&str>) -> Result<Option<usize>, SearchError> {
        match value {
            None => Ok(None),
            Some(value) => {
                value.parse::<usize>().ok().filter(|n| *n > 0).map(Some).ok_or_else(invalid_config)
            }
        }
    }

    pub fn validate(&self) -> Result<(), SearchError> {
        if self.max_request_bytes == 0 {
            Err(invalid_config())
        } else {
            Ok(())
        }
    }
}

fn invalid_config() -> SearchError {
    EmbeddingFailure::new(EmbeddingFailureCode::EmbeddingInvalidConfig).into()
}

/// Отказ сервиса эмбеддингов, разделённый по тому, способен ли повтор его изменить.
enum BatchFailure {
    /// Сервис ответил, и ответ тот же самый сколько ни спрашивай: не принят ключ,
    /// неизвестна модель, негоден или слишком велик запрос. Повторять такое — значит платить
    /// полным расписанием отсрочек за уже известный ответ.
    Permanent(SearchError),
    /// Сеть, перегрузка, сбой на стороне сервиса — состояние, которое проходит.
    Transient(SearchError),
}

impl BatchFailure {
    fn into_error(self) -> SearchError {
        match self {
            Self::Permanent(error) | Self::Transient(error) => error,
        }
    }

    /// Клиентский код означает, что сервис разобрал запрос и отказал по существу, —
    /// кроме трёх, которые говорят «повтори позже», а не «не выйдет»: 408 (запрос не
    /// принят целиком), 425 (слишком рано) и 429 (слишком часто). Разряд перечислен
    /// целиком, а не вырезан из диапазона арифметикой: границы вида `400..=428` читаются
    /// как произвольные и при следующей правке молча захватывают лишний код.
    fn from_status(code: u16, error: SearchError) -> Self {
        const TRY_LATER: [u16; 3] = [408, 425, 429];
        if (400..=499).contains(&code) && !TRY_LATER.contains(&code) {
            Self::Permanent(error)
        } else {
            Self::Transient(error)
        }
    }
}

pub struct Embedder {
    config: EmbedderConfig,
    storage_identity: String,
    /// Resilient agent for the unattended batch indexing pass: a long global timeout, paired
    /// with [`Self::MAX_RETRIES`] in [`Self::embed_batch`].
    agent: ureq::Agent,
    /// Tight agent for interactive single-query embeds ([`Self::embed`]). A `search_code` caller
    /// is waiting and the engine mutex is held across the call, so the query embed must fail
    /// fast instead of inheriting the batch path's minutes-long timeout-and-retry budget.
    interactive_agent: ureq::Agent,
}

impl Clone for Embedder {
    fn clone(&self) -> Self {
        Self::new(self.config.clone())
    }
}

impl Embedder {
    /// Global timeout for an interactive query embed. Bounds how long [`Self::embed`] can hold
    /// the engine mutex, so one slow embed cannot stall every concurrent `search_code`.
    const INTERACTIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);

    pub fn new(config: EmbedderConfig) -> Self {
        let storage_identity = storage_identity(&config);
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(120)))
            .build()
            .new_agent();
        let interactive_agent = ureq::Agent::config_builder()
            .timeout_global(Some(Self::INTERACTIVE_TIMEOUT))
            .build()
            .new_agent();
        Self { config, storage_identity, agent, interactive_agent }
    }

    pub fn dim(&self) -> usize {
        self.config.dim.unwrap_or(1024)
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn model_id(&self) -> &str {
        self.storage_identity()
    }

    pub fn dimension(&self) -> usize {
        self.dim()
    }

    /// A clone of this embedder's configuration, so a caller can rebuild a standalone embedder
    /// (e.g. the off-lock overlay warmup) without reaching into private fields.
    pub fn config(&self) -> EmbedderConfig {
        self.config.clone()
    }

    /// Identity of the vectors produced by this input profile. Empty prefixes retain the
    /// historical model key; dimensions remain a separate part of the storage contract.
    pub fn storage_identity(&self) -> &str {
        &self.storage_identity
    }

    /// Stable SQLite/Postgres layout claim for token-bound indexing, when enabled.
    pub fn token_layout_claim(&self) -> Option<&str> {
        self.config.token_policy.as_ref().map(|_| self.storage_identity())
    }

    /// Tokenizer policy frozen into this embedder, if token-bound inputs are enabled.
    pub fn token_policy(&self) -> Option<&crate::TokenPolicy> {
        self.config.token_policy.as_ref()
    }

    /// Prefix used by document preparation. `check_singleton_input` accepts text after this
    /// prefix has already been applied.
    pub fn document_prefix(&self) -> &str {
        &self.config.document_prefix
    }

    /// Check an already-prefixed final input against both the token bound and the exact
    /// singleton request serialization limit.
    pub fn check_singleton_input(&self, final_input: &str) -> Result<(), SearchError> {
        self.config.validate()?;
        self.check_tokens(final_input)?;
        let item = serde_json::to_vec(final_input)
            .map_err(|_| failure(EmbeddingFailureCode::EmbeddingFailed))?
            .len();
        let singleton = self
            .serialize_request(&[])?
            .len()
            .checked_add(item)
            .ok_or_else(|| failure(EmbeddingFailureCode::EmbeddingInputTooLarge))?;
        if singleton > self.config.max_request_bytes {
            return Err(self.size_failure(singleton, true));
        }
        Ok(())
    }

    const MAX_RETRIES: u32 = 10;

    /// Plan caller-visible requests so each existing owner retains its checkpoints.
    pub fn batch_ranges(
        &self,
        texts: &[&str],
        max_items: usize,
    ) -> Result<Vec<Range<usize>>, SearchError> {
        self.config.validate()?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prepared = texts
            .iter()
            .map(|text| format!("{}{}", self.config.document_prefix, text))
            .collect::<Vec<_>>();
        let prepared: Vec<_> = prepared.iter().map(String::as_str).collect();
        let envelope = self.serialize_request(&[])?.len();
        let mut ranges = Vec::new();
        let mut start = 0;
        let mut bytes = envelope;
        for (i, text) in prepared.iter().enumerate() {
            self.check_tokens(text)?;
            let item = serde_json::to_vec(text)
                .map_err(|_| failure(EmbeddingFailureCode::EmbeddingFailed))?
                .len();
            let singleton = envelope
                .checked_add(item)
                .ok_or_else(|| failure(EmbeddingFailureCode::EmbeddingInputTooLarge))?;
            if singleton > self.config.max_request_bytes {
                return Err(self.size_failure(singleton, true));
            }
            let next = bytes.checked_add(item).and_then(|n| n.checked_add(usize::from(i > start)));
            if i - start == max_items.max(1)
                || next.is_none_or(|n| n > self.config.max_request_bytes)
            {
                ranges.push(start..i);
                start = i;
                bytes = singleton;
            } else {
                bytes = next.expect("checked above");
            }
        }
        ranges.push(start..prepared.len());
        Ok(ranges)
    }

    /// One bounded HTTP request. Work-set owners call `batch_ranges` before scheduling.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SearchError> {
        let body = self.prepare_request(texts)?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        for attempt in 0..Self::MAX_RETRIES {
            match self.send_request(&self.agent, &body, texts.len(), true) {
                Ok(result) => return Ok(result),
                // Отказ по существу повторять нельзя: полное расписание отсрочек — около
                // двух минут на партию, и на каждой партии индексации оно платится заново,
                // в каждом рабочем потоке. Снаружи это выглядит как замерший сервер, хотя
                // ответ был получен сразу и он окончателен.
                Err(BatchFailure::Permanent(error)) => {
                    tracing::warn!("embedding batch rejected, not retrying: {error}");
                    return Err(error);
                }
                Err(BatchFailure::Transient(error)) => {
                    if attempt + 1 == Self::MAX_RETRIES {
                        return Err(error);
                    }
                    let delay = std::time::Duration::from_millis(500 * 2u64.pow(attempt.min(6)));
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = Self::MAX_RETRIES,
                        delay_ms = delay.as_millis() as u64,
                        "embedding batch failed, retrying: {error}"
                    );
                    std::thread::sleep(delay);
                }
            }
        }
        unreachable!("the last attempt returns its result")
    }

    fn serialize_request(&self, texts: &[&str]) -> Result<Vec<u8>, SearchError> {
        let provider_only = self.config.provider.as_deref().map(|s| [s]);
        let provider =
            provider_only.as_ref().map(|only| ProviderRouting { only, allow_fallbacks: false });
        serde_json::to_vec(&EmbeddingRequest {
            model: &self.config.model,
            input: texts,
            dimensions: self.config.dim,
            provider,
        })
        .map_err(|_| failure(EmbeddingFailureCode::EmbeddingFailed))
    }

    fn prepare_request(&self, texts: &[&str]) -> Result<Vec<u8>, SearchError> {
        self.prepare_request_with_prefix(texts, &self.config.document_prefix)
    }

    fn prepare_request_with_prefix(
        &self,
        texts: &[&str],
        prefix: &str,
    ) -> Result<Vec<u8>, SearchError> {
        self.config.validate()?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let prepared = texts.iter().map(|text| format!("{prefix}{text}")).collect::<Vec<_>>();
        let prepared: Vec<_> = prepared.iter().map(String::as_str).collect();
        for text in &prepared {
            self.check_tokens(text)?;
        }
        let body = self.serialize_request(&prepared)?;
        if body.len() > self.config.max_request_bytes {
            return Err(self.size_failure(body.len(), texts.len() == 1));
        }
        Ok(body)
    }

    fn check_tokens(&self, input: &str) -> Result<(), SearchError> {
        if let Some(policy) = &self.config.token_policy {
            policy.check(input)?;
        }
        Ok(())
    }

    fn size_failure(&self, request_bytes: usize, singleton: bool) -> SearchError {
        EmbeddingFailure {
            code: if singleton {
                EmbeddingFailureCode::EmbeddingInputTooLarge
            } else {
                EmbeddingFailureCode::EmbeddingRequestTooLarge
            },
            request_bytes: Some(request_bytes),
            max_request_bytes: Some(self.config.max_request_bytes),
        }
        .into()
    }

    fn send_request(
        &self,
        agent: &ureq::Agent,
        body: &[u8],
        input_count: usize,
        count_as_document_batch: bool,
    ) -> Result<Vec<Vec<f32>>, BatchFailure> {
        if count_as_document_batch {
            crate::lifecycle::Context::current_embedding_request(input_count);
        }
        let url = format!("{}/v1/embeddings", self.config.base_url);
        let mut req = agent.post(&url).header("Content-Type", "application/json");
        if let Some(ref key) = self.config.api_key {
            req = req.header("Authorization", &format!("Bearer {key}"));
        }
        let mut resp = req.send(body).map_err(|e| {
            tracing::debug!(
                request_bytes = body.len(),
                input_count,
                "embedding request failed: {e}"
            );
            match e {
                ureq::Error::StatusCode(code) => {
                    BatchFailure::from_status(code, transport_failure(e, false))
                }
                e => BatchFailure::Transient(transport_failure(e, false)),
            }
        })?;
        // Retain ureq's existing 10 MiB read bound, independent of the request ceiling.
        let body = resp.body_mut().read_to_string().map_err(|e| {
            tracing::debug!("failed to read embedding response body: {e}");
            let error = transport_failure(e, true);
            // The same request draws an answer of the same size; only a broken read may pass.
            if error
                .embedding_failure()
                .is_some_and(|f| f.code == EmbeddingFailureCode::EmbeddingResponseTooLarge)
            {
                BatchFailure::Permanent(error)
            } else {
                BatchFailure::Transient(error)
            }
        })?;
        let mut data = serde_json::from_str::<EmbeddingResponse>(&body)
            .map_err(|e| {
                tracing::debug!(
                    response_bytes = body.len(),
                    "failed to parse embedding response: {e}"
                );
                BatchFailure::Transient(failure(EmbeddingFailureCode::EmbeddingInvalidResponse))
            })?
            .data;
        data.sort_by_key(|d| d.index);
        if data.len() != input_count
            || data.iter().enumerate().any(|(index, d)| {
                d.index != index
                    || d.embedding.len() != self.dim()
                    || d.embedding.iter().any(|v| !v.is_finite())
                    || d.embedding.iter().all(|v| *v == 0.0)
            })
        {
            // A parsed answer of the wrong shape — count, order, dimension or non-finite
            // values — is the provider's settled reply to this request, not a passing state:
            // retrying would pay the whole backoff schedule for the same vectors.
            return Err(BatchFailure::Permanent(failure(
                EmbeddingFailureCode::EmbeddingInvalidResponse,
            )));
        }
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }

    /// Embed a single interactive query, fail-fast. Unlike [`Self::embed_batch`] (the resilient
    /// indexing path), this makes ONE attempt on the tight-timeout [`Self::interactive_agent`]:
    /// the caller is an interactive `search_code` holding the engine mutex, so a stuck embedding
    /// service must surface an error in seconds rather than retry for minutes and block every
    /// concurrent search. A transient failure is the caller's to retry as a whole search.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, SearchError> {
        let body = self.prepare_request_with_prefix(&[text], &self.config.query_prefix)?;
        self.send_request(&self.interactive_agent, &body, 1, false)
            .map_err(BatchFailure::into_error)?
            .pop()
            .ok_or_else(|| failure(EmbeddingFailureCode::EmbeddingInvalidResponse))
    }

    /// Embed a batch fail-fast on the interactive agent. For the workspace-overlay refresh, which
    /// runs while the engine mutex is held (an interactive semantic search or the warmup prime):
    /// it must NOT inherit the indexing path's minutes-long retry budget and stall every
    /// concurrent search. A transient failure just leaves those chunks un-embedded until the next
    /// refresh re-attempts them; lexical search stays available meanwhile.
    pub fn embed_batch_interactive(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SearchError> {
        let body = self.prepare_request(texts)?;
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.send_request(&self.interactive_agent, &body, texts.len(), false)
            .map_err(BatchFailure::into_error)
    }

    pub fn health_check(&self) -> Result<(), SearchError> {
        self.config.validate()?;
        let health_url = format!("{}/health", self.config.base_url);
        let models_url = format!("{}/v1/models", self.config.base_url);
        // Проба доступности идёт коротким агентом: вопрос «сервис вообще там есть» полезен
        // только пока на ответ можно опереться, а на батчевом бюджете сама проба ждала бы
        // минутами.
        if self.interactive_agent.get(&health_url).call().is_err() {
            self.interactive_agent
                .get(&models_url)
                .call()
                .map_err(|e| transport_failure(e, false))?;
        }
        Ok(())
    }
}

fn failure(code: EmbeddingFailureCode) -> SearchError {
    EmbeddingFailure::new(code).into()
}

fn transport_failure(error: ureq::Error, reading_response: bool) -> SearchError {
    use EmbeddingFailureCode::*;
    let code = match error {
        ureq::Error::StatusCode(413) => EmbeddingRequestTooLarge,
        ureq::Error::StatusCode(_) => EmbeddingProviderError,
        ureq::Error::Timeout(_) => EmbeddingTimeout,
        ureq::Error::BodyExceedsLimit(_) if reading_response => EmbeddingResponseTooLarge,
        ureq::Error::Io(_)
        | ureq::Error::Http(_)
        | ureq::Error::BadUri(_)
        | ureq::Error::Protocol(_)
        | ureq::Error::HostNotFound
        | ureq::Error::ConnectionFailed
        | ureq::Error::Tls(_)
        | ureq::Error::Rustls(_)
        | ureq::Error::RedirectFailed
        | ureq::Error::TooManyRedirects
        | ureq::Error::InvalidProxyUrl
        | ureq::Error::ConnectProxyFailed(_)
        | ureq::Error::TlsRequired
        | ureq::Error::RequireHttpsOnly(_)
        | ureq::Error::LargeResponseHeader(_, _) => EmbeddingTransportError,
        _ => EmbeddingFailed,
    };
    failure(code)
}

impl EmbeddingGenerator for Embedder {
    fn model_id(&self) -> &str {
        self.storage_identity()
    }

    fn token_layout_claim(&self) -> Option<&str> {
        self.token_layout_claim()
    }

    fn dimension(&self) -> usize {
        self.dim()
    }

    fn batch_ranges(
        &self,
        texts: &[&str],
        max_items: usize,
    ) -> Result<Vec<Range<usize>>, SearchError> {
        Self::batch_ranges(self, texts, max_items)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SearchError> {
        Self::embed_batch(self, texts)
    }
}

fn storage_identity(config: &EmbedderConfig) -> String {
    if config.token_policy.is_none()
        && config.query_prefix.is_empty()
        && config.document_prefix.is_empty()
    {
        return config.model.clone();
    }
    let mut hasher = blake3::Hasher::new();
    let mut fields = vec![
        b"bsl-search-input-profile-v1".to_vec(),
        config.model.as_bytes().to_vec(),
        config.query_prefix.as_bytes().to_vec(),
        config.document_prefix.as_bytes().to_vec(),
        b"semantic-document-v1".to_vec(),
    ];
    if let Some(policy) = &config.token_policy {
        let envelope = identity_envelope(config);
        fields.extend([
            policy.tokenizer_sha256().as_bytes().to_vec(),
            policy.max_tokens().to_string().into_bytes(),
            policy.segmentation_version().as_bytes().to_vec(),
            config.max_request_bytes.to_string().into_bytes(),
            blake3::hash(&envelope).to_hex().as_bytes().to_vec(),
        ]);
    }
    for field in fields {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(&field);
    }
    format!("profile-v1:{}", hasher.finalize().to_hex())
}

fn identity_envelope(config: &EmbedderConfig) -> Vec<u8> {
    let provider_only = config.provider.as_deref().map(|provider| [provider]);
    let provider =
        provider_only.as_ref().map(|only| ProviderRouting { only, allow_fallbacks: false });
    serde_json::to_vec(&EmbeddingRequest {
        model: &config.model,
        input: &[],
        dimensions: config.dim,
        provider,
    })
    .unwrap_or_default()
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<ProviderRouting<'a>>,
}

#[derive(Serialize)]
struct ProviderRouting<'a> {
    only: &'a [&'a str],
    allow_fallbacks: bool,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

#[cfg(test)]
pub(crate) mod payload_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_configuration_defaults_override_and_rejection() {
        assert_eq!(EmbedderConfig::parse_request_bytes(None).unwrap(), 1_048_576);
        assert_eq!(EmbedderConfig::parse_request_bytes(Some("73")).unwrap(), 73);
        for value in ["0", "", "-1", "not-a-number", "184467440737095516160"] {
            let error = EmbedderConfig::parse_request_bytes(Some(value)).unwrap_err();
            assert_eq!(error.to_string(), "embedding_invalid_config");
        }
        assert_eq!(EmbedderConfig::parse_dim(None).unwrap(), None);
        assert_eq!(EmbedderConfig::parse_dim(Some("7")).unwrap(), Some(7));
        for value in ["0", "", " 7", "-1", "seven", "184467440737095516160"] {
            let error = EmbedderConfig::parse_dim(Some(value)).unwrap_err();
            assert_eq!(error.to_string(), "embedding_invalid_config");
        }
        let embedder = Embedder::new(EmbedderConfig { max_request_bytes: 0, ..Default::default() });
        assert_eq!(embedder.embed("input").unwrap_err().to_string(), "embedding_invalid_config");
        assert!(embedder.embed_batch_interactive(&[]).is_err());
    }

    #[test]
    fn token_budget_guards_batch_planning_transport_and_query_inputs() {
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokenizer.json");
        let artifact = br#"{"version":"1.0","truncation":null,"padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},"post_processor":null,"decoder":null,"model":{"type":"WordLevel","vocab":{"[UNK]":0,"document":1,"query":2,"extra":3,"hello":4},"unk_token":"[UNK]"}}"#;
        std::fs::write(&path, artifact).unwrap();
        let hash =
            Sha256::digest(artifact).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let policy = crate::TokenPolicy::load(&path, &hash, 2).unwrap();
        let config = EmbedderConfig {
            document_prefix: "document ".to_owned(),
            query_prefix: "query extra ".to_owned(),
            token_policy: Some(policy),
            ..Default::default()
        };
        let embedder = Embedder::new(config.clone());

        assert_eq!(embedder.batch_ranges(&["hello"], 8).unwrap(), vec![0..1]);
        let too_large = embedder.batch_ranges(&["hello unknown"], 8).unwrap_err();
        assert_eq!(
            too_large.embedding_failure().unwrap().code,
            EmbeddingFailureCode::EmbeddingInputTooLarge
        );

        let identity = embedder.storage_identity().to_owned();
        let other_limit = Embedder::new(EmbedderConfig {
            token_policy: Some(crate::TokenPolicy::load(&path, &hash, 3).unwrap()),
            ..config.clone()
        });
        assert_ne!(other_limit.storage_identity(), identity);
        let other_byte_ceiling = Embedder::new(EmbedderConfig {
            max_request_bytes: config.max_request_bytes + 1,
            ..config.clone()
        });
        assert_ne!(other_byte_ceiling.storage_identity(), identity);
        let other_envelope = Embedder::new(EmbedderConfig {
            provider: Some("alternate".to_owned()),
            ..config.clone()
        });
        assert_ne!(other_envelope.storage_identity(), identity);

        let copy = dir.path().join("same-tokenizer.json");
        std::fs::copy(&path, &copy).unwrap();
        let same_policy_other_path = Embedder::new(EmbedderConfig {
            token_policy: Some(crate::TokenPolicy::load(&copy, &hash, 2).unwrap()),
            ..config
        });
        assert_eq!(same_policy_other_path.storage_identity(), identity);
        let too_large = embedder.embed_batch_interactive(&["hello unknown"]).unwrap_err();
        assert_eq!(
            too_large.embedding_failure().unwrap().code,
            EmbeddingFailureCode::EmbeddingInputTooLarge
        );
        let too_large = embedder.embed("hello").unwrap_err();
        assert_eq!(
            too_large.embedding_failure().unwrap().code,
            EmbeddingFailureCode::EmbeddingInputTooLarge
        );
    }

    fn classify(code: u16) -> BatchFailure {
        BatchFailure::from_status(code, SearchError::Embedder(format!("HTTP {code}")))
    }

    /// Разряд перечислён целиком, а не представителем: у клиентских кодов «повтори
    /// позже» — это 408, 425 и 429, и все три обязаны остаться временными. Отнести
    /// хоть один к окончательным значит оборвать партию на состоянии, которое проходит
    /// само.
    #[test]
    fn client_codes_that_mean_try_later_stay_transient() {
        for code in [408, 425, 429] {
            assert!(
                matches!(classify(code), BatchFailure::Transient(_)),
                "HTTP {code} означает «повтори позже» и не может быть окончательным"
            );
        }
    }

    #[test]
    fn client_codes_that_mean_never_are_permanent() {
        for code in [400, 401, 403, 404, 413, 422] {
            assert!(
                matches!(classify(code), BatchFailure::Permanent(_)),
                "HTTP {code} — отказ по существу, повтор его не изменит"
            );
        }
    }

    #[test]
    fn server_codes_stay_transient() {
        for code in [500, 502, 503, 504] {
            assert!(
                matches!(classify(code), BatchFailure::Transient(_)),
                "HTTP {code} — сбой на стороне сервиса, повтор осмыслен"
            );
        }
    }
}
