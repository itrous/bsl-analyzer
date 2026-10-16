use std::{env, error::Error, io, path::Path};

pub(super) fn resolve_project_url(
    postgres: &project_model::SearchPostgresConfig,
    mode: project_model::PostgresAccessMode,
) -> Result<project_model::ResolvedPostgresUrl, project_model::ResolvePostgresUrlError> {
    project_model::resolve_postgres_url(postgres, mode)
}

pub(super) fn build_project_adapter(
    source_dir: &Path,
    mode: project_model::PostgresAccessMode,
) -> Result<bsl_search::ExternalBaselineAdapter, Box<dyn Error + Send + Sync>> {
    let project = project_model::Project::new(source_dir)?;
    project.warn_about_external_discovery_rejections();
    build_project_adapter_for_project(&project, mode)
}

pub(super) fn build_project_adapter_for_project(
    project: &project_model::Project,
    mode: project_model::PostgresAccessMode,
) -> Result<bsl_search::ExternalBaselineAdapter, Box<dyn Error + Send + Sync>> {
    let resolved =
        resolve_project_url(&project.config.search.baseline.postgres, mode).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("failed to resolve PostgreSQL {} credentials: {error}", mode.as_str()),
            )
        })?;
    build_adapter(&resolved.url, project.config.search.baseline.postgres.schema.as_deref())
}

pub(super) fn build_adapter(
    pg_url: &str,
    pg_schema: Option<&str>,
) -> Result<bsl_search::ExternalBaselineAdapter, Box<dyn Error + Send + Sync>> {
    let mut config = bsl_search::ExternalBaselineConfig::postgres(pg_url.to_owned());
    if let Some(schema) = pg_schema {
        config = config.with_schema(schema.to_owned());
    }
    Ok(bsl_search::ExternalBaselineAdapter::new(config)?)
}

pub(super) fn embedder_config(
    project: &project_model::Project,
) -> Result<Option<bsl_search::EmbedderConfig>, bsl_search::SearchError> {
    let emb = &project.config.search.baseline.embedding;

    // Resolve the opt-in triple before checking whether an embedding endpoint is configured.
    // A partial or invalid request must not silently downgrade a requested bounded publish.
    let token_policy = mcp_server::resolve_embedding_token_profile_values(
        Some(&project.config),
        Some(&project.root),
        [None, None, None],
        [
            env::var_os("EMBEDDING_MAX_INPUT_TOKENS"),
            env::var_os("EMBEDDING_TOKENIZER_FILE"),
            env::var_os("EMBEDDING_TOKENIZER_SHA256"),
        ],
    )?
    .and_then(|profile| profile.token_policy);

    let Some(model) = emb.model.clone().or_else(|| env::var("EMBEDDING_MODEL").ok()) else {
        return Ok(None);
    };
    let Some(base_url) = env::var("EMBEDDING_URL").ok().or_else(|| emb.url.clone()) else {
        return Ok(None);
    };
    let max_request_bytes = bsl_search::EmbedderConfig::request_bytes_from_env()?;
    // Same contract as the MCP server: an undeclared width sends no `dimensions`, so
    // a baseline published here keeps the storage identity the server will claim.
    let dim = match emb.dimension {
        Some(dim) => Some(dim),
        None => bsl_search::EmbedderConfig::dim_from_env()?,
    };

    Ok(Some(bsl_search::EmbedderConfig {
        base_url,
        model,
        dim,
        api_key: env::var("EMBEDDING_API_KEY").ok(),
        provider: emb.provider.clone().or_else(|| env::var("EMBEDDING_PROVIDER").ok()),
        query_prefix: emb.resolve_query_prefix(env::var("EMBEDDING_QUERY_PREFIX").ok().as_deref()),
        document_prefix: emb
            .resolve_document_prefix(env::var("EMBEDDING_DOCUMENT_PREFIX").ok().as_deref()),
        max_request_bytes,
        token_policy,
    }))
}

pub(super) fn embedding_execution_policy_from_env() -> bsl_search::EmbeddingExecutionPolicy {
    bsl_search::EmbeddingExecutionPolicy {
        batch_size: env::var("EMBEDDING_BATCH_SIZE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(32),
        concurrency: env::var("EMBEDDING_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(10),
        progress_interval: env::var("EMBEDDING_PROGRESS_INTERVAL")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(20),
    }
}
