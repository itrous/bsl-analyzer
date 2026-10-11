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
            Err(_) => Err(format!(
                "{reason}; no previously downloaded package of {}",
                bsl_platform::redact_url(url)
            )),
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

/// `message` without the secrets of `url`. The transport quotes the URL in its
/// errors, but not always in the form it was given (a fragment is dropped, the
/// authority and query are normalized), so the whole URL, its fragment-less and
/// its normalized forms, and its userinfo and query are each removed on their
/// own.
fn scrubbed(message: String, url: &str) -> String {
    let shown = bsl_platform::redact_url(url);
    let without_fragment = url.split('#').next().unwrap_or(url);
    let mut message = message.replace(url, &shown).replace(without_fragment, &shown);
    let normalized = url::Url::parse(url).ok();
    if let Some(normalized) = &normalized {
        message = message.replace(normalized.as_str(), &shown);
    }
    // Both readings of where the userinfo ends: the transport's (inside the
    // authority) and the widest one (the last `@` anywhere).
    if let Some((_, rest)) = url.split_once("://") {
        let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
        for (userinfo, tail) in
            [authority.rsplit_once('@'), rest.rsplit_once('@')].into_iter().flatten()
        {
            message = message.replace(&format!("://{userinfo}@"), "://");
            // Bare, together with the host after it, so that even a short
            // userinfo cannot match unrelated text.
            let host = &tail[..tail.find(['/', '?', '#']).unwrap_or(tail.len())];
            if !host.is_empty() {
                message = message.replace(&format!("{userinfo}@{host}"), host);
            }
        }
    }
    // The userinfo as the transport normalizes it (percent-encoded), alone or
    // with the host that follows it, quoted with or without the scheme.
    if let Some(normalized) = &normalized {
        let userinfo = match normalized.password() {
            Some(password) => format!("{}:{password}", normalized.username()),
            None => normalized.username().to_owned(),
        };
        if !userinfo.is_empty() {
            let host = normalized.host_str().unwrap_or_default();
            let host = match normalized.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            };
            message = message.replace(&format!("://{userinfo}@"), "://");
            message = message.replace(&format!("{userinfo}@{host}"), &host);
        }
    }
    // The query in the spelling given and as the transport normalizes it.
    let queries = [
        without_fragment.split_once('?').map(|(_, query)| query.to_owned()),
        normalized.as_ref().and_then(|normalized| normalized.query().map(str::to_owned)),
    ];
    for query in queries.into_iter().flatten().filter(|query| !query.is_empty()) {
        // With its `?`, so a short query does not eat unrelated text.
        message = message.replace(&format!("?{query}"), "");
    }
    message
}

fn get(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, String> {
    let shown = bsl_platform::redact_url(url);
    let failed = |error: ureq::Error| format!("{shown}: {}", scrubbed(error.to_string(), url));
    let mut response = agent.get(url).call().map_err(failed)?;
    response.body_mut().with_config().limit(limit).read_to_vec().map_err(failed)
}

fn fetch(url: &str, slot: &Slot) -> Result<VerifiedCorpus, String> {
    let observed = slot.generation();
    let agent = agent();
    let manifest_bytes = get(&agent, url, MAX_MANIFEST)?;
    let shown = bsl_platform::redact_url(url);
    let manifest =
        CorpusManifest::parse(&manifest_bytes).map_err(|reason| format!("{shown}: {reason}"))?;

    // The published package is the cached one: nothing to download. The whole
    // manifest must match, not only the corpus digest, or the cache would keep
    // reporting superseded metadata.
    if let Ok(cached) = slot.read() {
        if cached.manifest.as_ref().is_some_and(|cached| same_publication(cached, &manifest)) {
            return Ok(cached);
        }
    }

    let base = url::Url::parse(url).map_err(|error| format!("{shown}: {error}"))?;
    let corpus_url = base
        .join(&manifest.corpus_file)
        .map_err(|error| format!("{shown}: corpus location: {error}"))?;
    let bytes = get(&agent, corpus_url.as_str(), MAX_DOWNLOAD)?;
    let corpus = manifest.clone().verify(bytes).map_err(|reason| format!("{shown}: {reason}"))?;
    // Only a corpus the analyzer can serve replaces the cached one.
    bsl_platform::PlatformSnapshot::from_corpus_json(&corpus.bytes)
        .map_err(|error| format!("{shown}: invalid help corpus: {error}"))?;
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

#[cfg(test)]
mod tests {
    use super::scrubbed;

    #[test]
    fn transport_messages_lose_secrets_in_any_spelling_of_the_url() {
        let url = "https://user:secret@127.0.0.1:1/help/m.json?token=abc#frag";
        // The transport quoted the URL without its fragment, and once only the
        // authority and query.
        for quoted in [
            "https://user:secret@127.0.0.1:1/help/m.json?token=abc#frag: refused",
            "https://user:secret@127.0.0.1:1/help/m.json?token=abc: refused",
            "refused by user:secret@127.0.0.1:1/help/m.json?token=abc",
        ] {
            let message = scrubbed(quoted.to_owned(), url);
            assert!(!message.contains("secret") && !message.contains("token"), "{message}");
            assert!(message.contains("refused"), "{message}");
        }
        // A password with `/` must not survive, and a short query must not eat
        // unrelated text.
        let slashed = scrubbed(
            "https://admin:pa/ss@h/m.json refused".to_owned(),
            "https://admin:pa/ss@h/m.json",
        );
        assert!(!slashed.contains("pa/ss") && !slashed.contains("admin"), "{slashed}");
        assert_eq!(scrubbed("request refused".to_owned(), "https://h/m.json?q"), "request refused");
        // A short userinfo, quoted without the scheme.
        let short = scrubbed(
            "refused by a:b@127.0.0.1:1/m.json".to_owned(),
            "https://a:b@127.0.0.1:1/m.json",
        );
        assert!(!short.contains("a:b"), "{short}");
        assert!(short.contains("127.0.0.1:1/m.json"), "{short}");
        // A userinfo the transport percent-encoded, quoted without the scheme.
        for (url, quoted) in [
            ("https://user:pa ss@127.0.0.1:1/m.json", "refused by user:pa%20ss@127.0.0.1:1/m.json"),
            ("https://user:p@ss@h/m.json", "refused by user:p%40ss@h/m.json"),
            (
                "https://user:секрет@h/m.json",
                "refused by user:%D1%81%D0%B5%D0%BA%D1%80%D0%B5%D1%82@h/m.json",
            ),
        ] {
            let message = scrubbed(quoted.to_owned(), url);
            assert!(
                !message.contains("%20ss")
                    && !message.contains("%40ss")
                    && !message.contains("%D1"),
                "{message}"
            );
            assert!(message.contains("refused by") && message.contains("/m.json"), "{message}");
        }
        // A query the transport percent-encoded.
        let encoded = scrubbed(
            "https://host.invalid/m.json?token=%E7%A7%98%E5%AF%86: refused".to_owned(),
            "https://host.invalid/m.json?token=秘密",
        );
        assert!(!encoded.contains("token") && !encoded.contains("%E7"), "{encoded}");
        assert!(encoded.contains("refused"), "{encoded}");
        assert_eq!(bsl_platform::redact_url("https://admin:pa/ss@h/m.json"), "https://<invalid>");
    }
}
