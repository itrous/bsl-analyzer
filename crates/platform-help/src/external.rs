//! A corpus package published at a URL: its manifest is fetched, the corpus it
//! names is downloaded and checked against the manifest digest, and the verified
//! package is kept in a cache of its own for this URL. When the URL cannot
//! deliver a valid package, the last valid package of the same URL serves.

use std::path::PathBuf;
use std::time::Duration;

use crate::installed::LoadContext;
use crate::package::{self, CorpusManifest, Slot, VerifiedCorpus};

/// A help corpus is tens of megabytes; far beyond that is not a corpus.
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;
const MAX_MANIFEST: u64 = 1024 * 1024;

/// Where the packages of `url` are cached: one slot per URL, so a failing URL
/// never falls back to another URL's data.
pub fn cache_dir_for(url: &str, context: &LoadContext) -> PathBuf {
    context.cache_dir.join("external").join(&package::sha256_hex(url.as_bytes())[..32])
}

/// The package published at `url`, refreshed when the published manifest
/// changed. Returns the corpus and where its package lives; on a fetch or
/// format failure, the last valid package of this URL with the reason.
pub fn load_url(
    url: &str,
    context: &LoadContext,
) -> Result<(VerifiedCorpus, PathBuf, Option<String>), String> {
    let cache = cache_dir_for(url, context);
    let slot = Slot::new(cache.clone());
    match fetch(url, &slot) {
        Ok(corpus) => Ok((corpus, cache, None)),
        Err(reason) => match slot.read() {
            Ok(corpus) => Ok((corpus, cache, Some(reason))),
            Err(_) => Err(format!("{reason}; no previously downloaded package of {url}")),
        },
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .new_agent()
}

fn get(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let mut response = agent.get(url).call().map_err(|error| format!("{url}: {error}"))?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|error| format!("{url}: {error}"))
}

/// The file at `url`, read whole up to the corpus size limit.
pub(crate) fn download(url: &str) -> Result<Vec<u8>, String> {
    get(&agent(), url, MAX_DOWNLOAD)
}

fn fetch(url: &str, slot: &Slot) -> Result<VerifiedCorpus, String> {
    let observed = slot.generation();
    let agent = agent();
    let manifest_bytes = get(&agent, url, MAX_MANIFEST)?;
    let manifest =
        CorpusManifest::parse(&manifest_bytes).map_err(|reason| format!("{url}: {reason}"))?;

    // The published package is the cached one: nothing to download. The whole
    // manifest must match, not only the corpus digest, or the cache would keep
    // reporting superseded metadata.
    if let Ok(cached) = slot.read() {
        if cached.manifest.as_ref().is_some_and(|cached| same_publication(cached, &manifest)) {
            return Ok(cached);
        }
    }

    let base = url::Url::parse(url).map_err(|error| format!("{url}: {error}"))?;
    let corpus_url = base
        .join(&manifest.corpus_file)
        .map_err(|error| format!("{url}: corpus location: {error}"))?;
    let bytes = get(&agent, corpus_url.as_str(), MAX_DOWNLOAD)?;
    let corpus = manifest.clone().verify(bytes).map_err(|reason| format!("{url}: {reason}"))?;
    // Only a corpus the analyzer can serve replaces the cached one.
    bsl_platform::PlatformSnapshot::from_corpus_json(&corpus.bytes)
        .map_err(|error| format!("{url}: invalid help corpus: {error}"))?;
    slot.publish_unless_superseded(
        &corpus.bytes,
        &manifest.corpus_id,
        manifest.platform_version.as_deref(),
        &manifest.extractor_version,
        &[],
        Some(observed),
    )
}

/// Whether a cached manifest describes the published one; the corpus file name
/// is the cache's own and does not count.
fn same_publication(cached: &CorpusManifest, published: &CorpusManifest) -> bool {
    cached.schema_version == published.schema_version
        && cached.corpus_id == published.corpus_id
        && cached.platform_version == published.platform_version
        && cached.extractor_version == published.extractor_version
        && cached.sha256.eq_ignore_ascii_case(&published.sha256)
}
