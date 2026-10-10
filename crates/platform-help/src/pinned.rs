//! The corpus `auto` downloads when neither a saved snapshot nor an installation
//! can serve: one immutable release asset whose digest is compiled in, so the
//! analyzer only ever accepts the exact corpus it was released with. A mirror
//! named by the user replaces it, and then what the mirror serves is the user's
//! responsibility. A download is cached and later starts read the cache without
//! touching the network.

use std::path::PathBuf;

use crate::external;
use crate::installed::LoadContext;
use crate::package::{self, Slot, VerifiedCorpus};

/// The release asset holding the corpus this analyzer is released with.
pub const PINNED_URL: &str =
    "https://github.com/itrous/bsl-platform-help/releases/download/corpus-3c759994/platform_data.json";
/// SHA-256 of [`PINNED_URL`]'s file; any other content is refused.
pub const PINNED_SHA256: &str = "3c759994cbd82a1522b1c497d9f2ef68c77b776d5c6723ba5a72623b570cacce";
/// Names a mirror in place of [`PINNED_URL`]; an empty value turns the
/// download off, which is how the test suite stays off the network.
pub const PINNED_URL_ENV: &str = "BSL_PLATFORM_HELP_PINNED_URL";

const NOTICE_FILE: &str = "NOTICE.md";

/// A corpus file and, for the compiled-in one, the digest it must have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedCorpus {
    pub url: String,
    /// `None` for a mirror: its content is taken as the user's choice.
    pub sha256: Option<String>,
}

impl PinnedCorpus {
    /// The compiled-in corpus, or the mirror the environment names; `None`
    /// when the download is turned off or the named mirror is unreadable,
    /// which never falls back to the compiled-in URL.
    pub fn from_environment() -> Option<Self> {
        let Some(value) = std::env::var_os(PINNED_URL_ENV) else {
            return Some(Self::compiled_in());
        };
        let Some(url) = value.to_str() else {
            tracing::warn!(
                "{PINNED_URL_ENV} is not valid UTF-8; the platform help corpus is not downloaded"
            );
            return None;
        };
        let url = url.trim();
        (!url.is_empty()).then(|| Self { url: url.to_owned(), sha256: None })
    }

    pub fn compiled_in() -> Self {
        Self { url: PINNED_URL.to_owned(), sha256: Some(PINNED_SHA256.to_owned()) }
    }

    /// Where the download is kept: one slot per digest for a pinned corpus, so
    /// a changed pin never serves the previous corpus, and one per URL for a
    /// mirror, so another mirror never serves this one's copy.
    pub fn cache_dir(&self, context: &LoadContext) -> PathBuf {
        let key = match &self.sha256 {
            Some(digest) => digest.get(..32).unwrap_or(digest).to_owned(),
            None => format!("mirror-{}", &package::sha256_hex(self.url.as_bytes())[..32]),
        };
        context.cache_dir.join("pinned").join(key)
    }

    fn accepts(&self, digest: &str) -> bool {
        self.sha256.as_deref().is_none_or(|pinned| pinned.eq_ignore_ascii_case(digest))
    }
}

/// The pinned corpus: the cached download when there is one, else a fresh
/// download. Returns where the package is kept.
pub fn load(
    pinned: &PinnedCorpus,
    context: &LoadContext,
) -> Result<(VerifiedCorpus, PathBuf), String> {
    if let Some(digest) = &pinned.sha256 {
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{digest:?} is not a SHA-256 digest"));
        }
    }
    let cache = pinned.cache_dir(context);
    let slot = Slot::new(cache.clone());
    if let Ok(corpus) = slot.read() {
        if pinned.accepts(&corpus.sha256) {
            return Ok((corpus, cache));
        }
    }
    let observed = slot.generation();
    let _span = tracing::info_span!("download_platform_help", url = %pinned.url).entered();
    let bytes = external::download(&pinned.url)?;
    let digest = package::sha256_hex(&bytes);
    if !pinned.accepts(&digest) {
        return Err(format!(
            "{}: SHA-256 {digest} differs from the pinned {}",
            pinned.url,
            pinned.sha256.as_deref().unwrap_or_default()
        ));
    }
    bsl_platform::PlatformSnapshot::from_corpus_json(&bytes)
        .map_err(|error| format!("{}: invalid help corpus: {error}", pinned.url))?;
    let corpus_id = match pinned.sha256 {
        Some(_) => format!("pinned-{}", &digest[..8]),
        None => format!("mirror-{}", &digest[..8]),
    };
    // A mirror may change: a slower download must not replace a newer copy.
    let corpus = slot.publish_unless_superseded(
        &bytes,
        &corpus_id,
        None,
        "published",
        &[(NOTICE_FILE, crate::PACKAGE_NOTICE.as_bytes())],
        Some(observed),
    )?;
    Ok((corpus, cache))
}
