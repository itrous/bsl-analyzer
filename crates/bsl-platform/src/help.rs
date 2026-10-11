//! Which help corpus the process serves, where it came from, and how far its
//! facts can be trusted for a project target.
//!
//! The process serves exactly one immutable snapshot. An application selects it
//! once, before the first lookup, with [`install_platform_help`]; it loads the
//! source at its boundary, since lookups never do I/O. Library use that never
//! selects is served the corpus named by [`CORPUS_ENV`], or the built-in
//! interface facts. Switching the source afterwards requires a restart.
//!
//! The built-in facts are the structured interface of the released platform —
//! names, signatures, parameters, types, versions, execution contexts — without
//! any of 1C's descriptive texts (`data/PROVENANCE.md`). They are the floor
//! every selection but `none` degrades to, so no process answers "nothing"
//! merely because no richer corpus could be loaded.

use std::fmt;
use std::path::PathBuf;

use crate::global_catalog::PlatformVersion;
use crate::snapshot::PlatformSnapshot;

/// A corpus JSON that serves library use (tests, tools) which selects no
/// source itself, and the application when its configuration names none. The
/// corpus-contract test suite runs with it.
pub const CORPUS_ENV: &str = "BSL_PLATFORM_HELP_CORPUS";

/// `url` without what can carry a secret: the userinfo and the query and
/// fragment. The scheme, host, port and path stay, which is what identifies a
/// source in a log.
///
/// The authority is read the way the transport reads it: it ends at the first
/// `/`, `?` or `#`, and its userinfo ends at the last `@` inside it. An
/// authority that is not a plain `host[:port]` after that is not shown at all,
/// since it can hold a password that the usual rules would leave in place. A
/// value without `://` is not shown either.
pub fn redact_url(url: &str) -> String {
    let cut = |text: &str| text.split(['?', '#']).next().unwrap_or(text).to_owned();
    // Without a literal `://` the authority cannot be told from the rest, so
    // nothing of the value is shown.
    let Some((scheme, rest)) = url.split_once("://").filter(|(scheme, _)| is_scheme(scheme)) else {
        return "<invalid>".to_owned();
    };
    let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    if !is_host_and_port(host) {
        return format!("{scheme}://<invalid>");
    }
    let path = if tail.starts_with('/') { cut(tail) } else { String::new() };
    // A control character (a line feed) would split or forge a log record.
    if path.chars().any(char::is_control) {
        return format!("{scheme}://{host}/<invalid>");
    }
    format!("{scheme}://{host}{path}")
}

/// A URL scheme: a letter, then letters, digits, `+`, `-` and `.`.
fn is_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

/// `host`, `host:port` or `[ipv6]:port`, in the characters a host can have.
fn is_host_and_port(authority: &str) -> bool {
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !authority.ends_with(']') && !port.contains(']') => {
            (host, Some(port))
        }
        _ => (authority, None),
    };
    let host_ok = match host.strip_prefix('[') {
        Some(inner) => inner.strip_suffix(']').is_some_and(|ip| {
            // `%` starts a zone id (`fe80::1%eth0`).
            !ip.is_empty()
                && ip.chars().all(|c| {
                    c.is_ascii_hexdigit() || c.is_alphanumeric() || matches!(c, ':' | '.' | '%')
                })
        }),
        None => {
            !host.is_empty()
                // Unicode letters: an internationalized name is a host as it is.
                && host.chars().all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        }
    };
    host_ok && port.is_none_or(|port| port.chars().all(|c| c.is_ascii_digit()))
}

/// Whether a [`CORPUS_ENV`] value is a URL or an attempt at one, rather than a
/// path. A value that is only almost a URL (`https:/host`, `https//host`,
/// `//user:pw@host`, `user:pw@host/m.json`) must not be read as a path and
/// printed with whatever secrets it holds, so it counts as a URL here, as does
/// any value with a `?`, `#` or `@`.
///
/// A drive path (`C:/help`), a path with a blank (`C:/Program Files/help`) and a
/// path that merely mentions a scheme (`./https://x`) are paths.
pub fn is_url_like(value: &str) -> bool {
    let text = value.trim();
    let starts = |scheme: &str| {
        text.get(..scheme.len()).is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    };
    if starts("https://") || starts("http://") || text.starts_with("//") {
        return true;
    }
    // Characters a URL carries and a path hardly does. A path that needs one
    // of them is given by `[platform_help]` instead.
    if text.contains(['?', '#', '@']) || text.starts_with("://") {
        return true;
    }
    // A scheme broken by a blank (`ht tps://…`).
    if text.contains("://") && text.contains(char::is_whitespace) {
        return true;
    }
    let word_len = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
        .unwrap_or(text.len());
    let (word, rest) = text.split_at(word_len);
    if word.is_empty() {
        return false;
    }
    if rest.starts_with("//") {
        return true;
    }
    if rest.starts_with(':') {
        // One letter is a drive, which carries none of a URL's characters.
        return word.len() >= 2 || text.contains(['?', '@']);
    }
    false
}

/// The interface facts compiled into the analyzer, in the help corpus JSON
/// shape. Regenerated from a full corpus by `scripts/strip-help-corpus-texts.py`.
const BUNDLED_FACTS: &[u8] = include_bytes!("../data/platform_facts.json");

/// Platform release the built-in facts describe (`data/PROVENANCE.md`).
pub const BUNDLED_PLATFORM_VERSION: &str = "8.3.27.2214";

/// A configured help source, as the application resolved it from the project
/// configuration. Paths are already absolute where the configuration allows it.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlatformHelpRequest {
    /// The built-in interface facts, and nothing richer: no discovery,
    /// extraction or network.
    Bundled,
    /// HBK archives of an installed platform: an explicit directory, or discovery.
    Installed { path: Option<PathBuf> },
    /// A prepared corpus package or legacy corpus JSON on disk.
    ExternalPath(PathBuf),
    /// A corpus package manifest published at a URL.
    ExternalUrl(String),
    /// The snapshot saved for `auto`, else an installed platform, else the
    /// built-in interface facts.
    Auto,
    /// Help explicitly disabled: no discovery, extraction or network.
    None,
    /// Nothing selected a source: library use without [`CORPUS_ENV`].
    Unselected,
}

impl PlatformHelpRequest {
    /// The source used when neither configuration nor the application chose one.
    pub fn default_for_build() -> Self {
        Self::Auto
    }

    pub fn kind(&self) -> PlatformHelpSourceKind {
        match self {
            Self::Bundled => PlatformHelpSourceKind::Bundled,
            Self::Installed { .. } => PlatformHelpSourceKind::Installed,
            Self::ExternalPath(_) | Self::ExternalUrl(_) => PlatformHelpSourceKind::External,
            Self::Auto => PlatformHelpSourceKind::Auto,
            Self::None | Self::Unselected => PlatformHelpSourceKind::None,
        }
    }
}

impl fmt::Display for PlatformHelpRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundled => formatter.write_str("bundled"),
            Self::Installed { path: Some(path) } => {
                write!(formatter, "installed ({})", path.display())
            }
            Self::Installed { path: None } => formatter.write_str("installed (discovery)"),
            Self::ExternalPath(path) => write!(formatter, "external ({})", path.display()),
            Self::ExternalUrl(url) => write!(formatter, "external ({})", redact_url(url)),
            Self::Auto => formatter.write_str("auto"),
            Self::None => formatter.write_str("none"),
            Self::Unselected => formatter.write_str("not selected"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlatformHelpSourceKind {
    Bundled,
    Installed,
    External,
    Auto,
    None,
}

impl PlatformHelpSourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Installed => "installed",
            Self::External => "external",
            Self::Auto => "auto",
            Self::None => "none",
        }
    }
}

/// Where the served snapshot actually came from. For `auto` this names the
/// source that produced the data (an installed platform or the saved snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformHelpOrigin {
    pub source: PlatformHelpSourceKind,
    /// Directory, file or URL the snapshot was read from, when there is one.
    pub location: Option<String>,
    /// Platform version of the corpus when it is known.
    pub platform_version: Option<String>,
    /// SHA-256 of the corpus JSON when it is known.
    pub digest: Option<String>,
}

/// Trust in the served help for one project target. Kept apart from
/// [`crate::PlatformCatalogStatus`]: only the EDT catalog proves absence of a
/// global name; help status never does, whatever its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformHelpStatus {
    /// No usable snapshot: the source is disabled, absent or failed to load.
    Missing,
    /// A valid snapshot of a known version matching the target release line.
    Available,
    /// A valid snapshot whose platform version is unknown.
    Unverified,
    /// A valid snapshot of a known version from another release line.
    UnsupportedTarget,
}

impl PlatformHelpStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Available => "available",
            Self::Unverified => "unverified",
            Self::UnsupportedTarget => "unsupported_target",
        }
    }
}

/// A loaded help selection ready to be published as the process snapshot.
#[derive(Debug, Clone)]
pub struct PlatformHelp {
    pub request: PlatformHelpRequest,
    pub snapshot: PlatformSnapshot,
    /// `None` when there is no usable snapshot; the snapshot is then empty.
    pub origin: Option<PlatformHelpOrigin>,
    /// Why the requested source serves no corpus of its own: the whole story
    /// for a `Missing` selection, and for `auto` degraded to the built-in facts
    /// why nothing richer loaded. `None` when the request is served in full.
    pub missing_reason: Option<String>,
}

impl PlatformHelp {
    pub fn loaded(
        request: PlatformHelpRequest,
        snapshot: PlatformSnapshot,
        origin: PlatformHelpOrigin,
    ) -> Self {
        Self { request, snapshot, origin: Some(origin), missing_reason: None }
    }

    /// An empty selection. Lookups answer nothing; own registries and the EDT
    /// catalog keep working.
    pub fn missing(request: PlatformHelpRequest, reason: impl Into<String>) -> Self {
        Self {
            request,
            snapshot: PlatformSnapshot::default(),
            origin: None,
            missing_reason: Some(reason.into()),
        }
    }

    /// The built-in interface facts serving `request`. `reason` says why no
    /// richer corpus serves, when `request` asked for one.
    pub fn bundled(request: PlatformHelpRequest, reason: Option<String>) -> Self {
        match PlatformSnapshot::from_corpus_json(BUNDLED_FACTS) {
            Ok(snapshot) => Self {
                request,
                snapshot,
                origin: Some(PlatformHelpOrigin {
                    source: PlatformHelpSourceKind::Bundled,
                    location: None,
                    platform_version: Some(BUNDLED_PLATFORM_VERSION.to_owned()),
                    digest: None,
                }),
                missing_reason: reason,
            },
            // The compiled-in file is checked by the crate's tests; a process
            // that still cannot decode it reports that rather than panicking.
            Err(error) => {
                let facts = format!("built-in interface facts: {error}");
                Self::missing(
                    request,
                    match reason {
                        Some(reason) => format!("{reason}; {facts}"),
                        None => facts,
                    },
                )
            }
        }
    }

    /// The selection for library use that selected nothing: the corpus named by
    /// [`CORPUS_ENV`] when set and not blank, otherwise the built-in interface facts.
    pub fn unselected() -> Self {
        let blank = |value: &std::ffi::OsString| {
            value.is_empty() || value.to_str().is_some_and(|text| text.trim().is_empty())
        };
        match std::env::var_os(CORPUS_ENV).filter(|value| !blank(value)) {
            Some(value) if is_url_like(&value.to_string_lossy()) => {
                let text = value.to_string_lossy();
                let shown = redact_url(&text);
                let reason = if text.trim_start().to_ascii_lowercase().starts_with("http://") {
                    format!("{CORPUS_ENV}: `http` is not accepted ({shown}); use `https` or a path")
                } else {
                    format!(
                        "{CORPUS_ENV}: a URL ({shown}) is supported only by the bsl-analyzer \
                         application; the library accepts a path"
                    )
                };
                Self::missing(PlatformHelpRequest::Unselected, reason)
            }
            Some(path) => Self::from_corpus_file(std::path::Path::new(&path)),
            None => Self::bundled(PlatformHelpRequest::Unselected, None),
        }
    }

    /// A corpus JSON read from disk, or the reason it cannot serve.
    pub fn from_corpus_file(path: &std::path::Path) -> Self {
        let request = PlatformHelpRequest::ExternalPath(path.to_path_buf());
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => return Self::missing(request, format!("{}: {error}", path.display())),
        };
        match PlatformSnapshot::from_corpus_json(&bytes) {
            Ok(snapshot) => Self::loaded(
                request,
                snapshot,
                PlatformHelpOrigin {
                    source: PlatformHelpSourceKind::External,
                    location: Some(path.display().to_string()),
                    platform_version: None,
                    digest: None,
                },
            ),
            Err(error) => {
                Self::missing(request, format!("{}: invalid help corpus: {error}", path.display()))
            }
        }
    }

    /// Selection for `request` that needs no I/O, or `None` when the request has
    /// to be loaded by the application.
    pub fn without_io(request: &PlatformHelpRequest) -> Option<Self> {
        match request {
            PlatformHelpRequest::Bundled => Some(Self::bundled(PlatformHelpRequest::Bundled, None)),
            PlatformHelpRequest::Unselected => Some(Self::unselected()),
            PlatformHelpRequest::None => Some(Self::missing(
                PlatformHelpRequest::None,
                "platform help is disabled (source = \"none\")",
            )),
            _ => None,
        }
    }
}

/// Status of a served selection for a project `target`, by the release-line rule
/// the EDT catalog uses.
pub(crate) fn status_for_target(
    origin: Option<&PlatformHelpOrigin>,
    target: Option<&str>,
) -> PlatformHelpStatus {
    let Some(origin) = origin else {
        return PlatformHelpStatus::Missing;
    };
    let Some(version) =
        origin.platform_version.as_deref().and_then(|v| v.parse::<PlatformVersion>().ok())
    else {
        return PlatformHelpStatus::Unverified;
    };
    let Some(target) = target else {
        return PlatformHelpStatus::Available;
    };
    match target.parse::<PlatformVersion>() {
        Ok(target) if target.same_release(version) => PlatformHelpStatus::Available,
        _ => PlatformHelpStatus::UnsupportedTarget,
    }
}

/// Outcome of publishing a selection as the process snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    /// The selection became the process snapshot.
    Installed,
    /// The same source was already serving; nothing changed.
    AlreadyActive,
}

/// A different source already serves this process; the new one takes effect
/// only after a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartRequired {
    pub active: PlatformHelpRequest,
    pub requested: PlatformHelpRequest,
}

impl fmt::Display for RestartRequired {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "platform help already serves `{}`; restart the server to switch to `{}`",
            self.active, self.requested
        )
    }
}

impl std::error::Error for RestartRequired {}

/// Publishes `help` as the process snapshot unless one is already fixed.
pub fn install_platform_help(help: PlatformHelp) -> Result<InstallOutcome, RestartRequired> {
    crate::db::install(help)
}

/// The source the process serves, once fixed (by installation or first access).
pub fn active_platform_help_request() -> Option<&'static PlatformHelpRequest> {
    crate::db::installed_request()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(version: Option<&str>) -> PlatformHelpOrigin {
        PlatformHelpOrigin {
            source: PlatformHelpSourceKind::External,
            location: None,
            platform_version: version.map(str::to_owned),
            digest: None,
        }
    }

    #[test]
    fn status_follows_release_line_and_unknown_version() {
        assert_eq!(status_for_target(None, Some("8.3.27")), PlatformHelpStatus::Missing);
        assert_eq!(status_for_target(Some(&origin(None)), None), PlatformHelpStatus::Unverified);
        let known = origin(Some("8.3.27.2214"));
        assert_eq!(status_for_target(Some(&known), None), PlatformHelpStatus::Available);
        assert_eq!(status_for_target(Some(&known), Some("8.3.27")), PlatformHelpStatus::Available);
        assert_eq!(
            status_for_target(Some(&known), Some("8.3.28")),
            PlatformHelpStatus::UnsupportedTarget
        );
        assert_eq!(
            status_for_target(Some(&known), Some("garbage")),
            PlatformHelpStatus::UnsupportedTarget
        );
    }
}
