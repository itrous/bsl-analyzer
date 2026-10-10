//! Startup selection of the platform help corpus.
//!
//! The application calls [`bootstrap`] once, after reading the project
//! configuration and before anything consults `bsl_platform` — metadata parsing,
//! analysis, MCP indexing. The chosen source is loaded here, at the application
//! boundary, and published as the process-wide snapshot; lookups never do I/O.
//! A different source configured later is reported by [`source_change_warning`]
//! and takes effect after a restart.

pub mod external;
pub mod installed;
pub mod package;
pub mod pinned;

use std::path::Path;

use bsl_platform::{
    install_platform_help, InstallOutcome, PlatformData, PlatformHelp, PlatformHelpOrigin,
    PlatformHelpRequest, PlatformHelpSourceKind, PlatformSnapshot,
};
pub use installed::LoadContext;
use project_model::{PlatformHelpSelection, ProjectConfig};

/// The help source a configuration asks for; the build default when it names none.
pub fn requested_source(
    config: Option<&ProjectConfig>,
    project_root: &Path,
) -> Result<PlatformHelpRequest, String> {
    let selection = match config {
        Some(config) => config.platform_help_selection(project_root).map_err(|e| e.to_string())?,
        None => None,
    };
    Ok(match selection {
        // A corpus named for the process stands in for an unset configuration,
        // which is how the corpus-contract suite runs the application.
        None => match std::env::var_os(bsl_platform::CORPUS_ENV).filter(|v| !v.is_empty()) {
            Some(corpus) => PlatformHelpRequest::ExternalPath(corpus.into()),
            None => PlatformHelpRequest::default_for_build(),
        },
        Some(PlatformHelpSelection::Bundled) => PlatformHelpRequest::Bundled,
        Some(PlatformHelpSelection::Installed { path }) => PlatformHelpRequest::Installed { path },
        Some(PlatformHelpSelection::ExternalPath(path)) => PlatformHelpRequest::ExternalPath(path),
        Some(PlatformHelpSelection::ExternalUrl(url)) => PlatformHelpRequest::ExternalUrl(url),
        Some(PlatformHelpSelection::Auto) => PlatformHelpRequest::Auto,
        Some(PlatformHelpSelection::None) => PlatformHelpRequest::None,
    })
}

/// Loads `request`. Never fails: a source that cannot deliver yields an empty
/// selection carrying the reason, and an explicit source is never replaced by
/// another one.
pub fn load(request: &PlatformHelpRequest) -> PlatformHelp {
    load_with(request, &LoadContext::from_environment())
}

/// [`load`] with explicit cache and discovery locations.
pub fn load_with(request: &PlatformHelpRequest, context: &LoadContext) -> PlatformHelp {
    if let Some(help) = PlatformHelp::without_io(request) {
        return help;
    }
    match request {
        PlatformHelpRequest::ExternalPath(path) => load_external_path(request, path),
        PlatformHelpRequest::Installed { path } => {
            load_installed(request, path.as_deref(), context)
        }
        PlatformHelpRequest::Auto => load_auto(request, context),
        PlatformHelpRequest::ExternalUrl(url) => load_external_url(request, url, context),
        PlatformHelpRequest::Bundled
        | PlatformHelpRequest::None
        | PlatformHelpRequest::Unselected => {
            unreachable!("resolved without I/O")
        }
    }
}

/// A verified corpus decoded into a selection, or the reason it is unusable.
fn decode(
    request: &PlatformHelpRequest,
    corpus: package::VerifiedCorpus,
    source: PlatformHelpSourceKind,
    location: &Path,
) -> Result<PlatformHelp, String> {
    let snapshot = PlatformSnapshot::from_corpus_json(&corpus.bytes)
        .map_err(|error| format!("{}: invalid help corpus: {error}", location.display()))?;
    Ok(PlatformHelp::loaded(
        request.clone(),
        snapshot,
        PlatformHelpOrigin {
            source,
            location: Some(location.display().to_string()),
            platform_version: corpus.manifest.and_then(|m| m.platform_version),
            digest: Some(corpus.sha256),
        },
    ))
}

fn load_external_url(
    request: &PlatformHelpRequest,
    url: &str,
    context: &LoadContext,
) -> PlatformHelp {
    let result = external::load_url(url, context).and_then(|(corpus, cache, stale_reason)| {
        if let Some(reason) = &stale_reason {
            tracing::warn!(%url, %reason, "serving the last valid package downloaded from this URL");
        }
        let mut help = decode(request, corpus, PlatformHelpSourceKind::External, &cache)
            .map_err(|reason| match &stale_reason {
                Some(fetch) => format!("{url}: {fetch}; the last downloaded package is unusable: {reason}"),
                None => reason,
            })?;
        if let Some(origin) = help.origin.as_mut() {
            origin.location = Some(url.to_owned());
        }
        Ok(help)
    });
    result.unwrap_or_else(|reason| PlatformHelp::missing(request.clone(), reason))
}

fn load_installed(
    request: &PlatformHelpRequest,
    path: Option<&Path>,
    context: &LoadContext,
) -> PlatformHelp {
    let result = installed::locate(path, context).and_then(|pair| {
        let corpus = installed::installed_corpus(&pair, context)?;
        decode(request, corpus, PlatformHelpSourceKind::Installed, &pair.dir)
    });
    result.unwrap_or_else(|reason| PlatformHelp::missing(request.clone(), reason))
}

/// The installation when there is one — reusing the `auto` snapshot while its
/// archives are unchanged — else the saved `auto` snapshot, else the pinned
/// corpus, else the built-in interface facts with the reason nothing richer
/// serves.
fn load_auto(request: &PlatformHelpRequest, context: &LoadContext) -> PlatformHelp {
    match load_local_auto(request, context) {
        Ok(help) => help,
        Err(local) => load_pinned(request, context, &local),
    }
}

/// The pinned corpus once nothing local can serve; `local` says why. Without
/// it the built-in interface facts serve, so `auto` never answers nothing.
fn load_pinned(request: &PlatformHelpRequest, context: &LoadContext, local: &str) -> PlatformHelp {
    let Some(pinned) = &context.pinned else {
        return PlatformHelp::bundled(
            request.clone(),
            Some(format!("{local}; the pinned corpus download is turned off")),
        );
    };
    let loaded = pinned::load(pinned, context).and_then(|(corpus, cache)| {
        let mut help = decode(request, corpus, PlatformHelpSourceKind::Auto, &cache)?;
        if let Some(origin) = help.origin.as_mut() {
            origin.location = Some(pinned.url.clone());
        }
        Ok(help)
    });
    loaded.unwrap_or_else(|reason| {
        PlatformHelp::bundled(request.clone(), Some(format!("{local}; pinned corpus: {reason}")))
    })
}

/// [`load_auto`] without the network: the reason when nothing local serves.
fn load_local_auto(
    request: &PlatformHelpRequest,
    context: &LoadContext,
) -> Result<PlatformHelp, String> {
    let saved = installed::AutoSnapshot::new(context);
    let saved_help = || {
        saved.read().and_then(|corpus| {
            decode(request, corpus, PlatformHelpSourceKind::Auto, saved.location())
        })
    };
    let pair = match installed::locate(None, context) {
        Ok(pair) => pair,
        Err(no_installation) => {
            return saved_help()
                .map_err(|_| format!("no saved auto snapshot and {no_installation}"));
        }
    };
    let fresh = installed::ArchiveIdentity::of(&pair).and_then(|identity| {
        if let Some((corpus, Some(saved_identity))) = saved.read_with_identity() {
            if saved_identity == identity {
                return decode(request, corpus, PlatformHelpSourceKind::Installed, &pair.dir);
            }
        }
        let corpus = saved.save(&identity, &pair, context)?;
        decode(request, corpus, PlatformHelpSourceKind::Installed, &pair.dir)
    });
    match fresh {
        Ok(help) => Ok(help),
        Err(reason) => match saved_help() {
            Ok(help) => {
                tracing::warn!(%reason, "installed platform help unusable; serving the saved auto snapshot");
                Ok(help)
            }
            Err(_) => Err(reason),
        },
    }
}

fn load_external_path(request: &PlatformHelpRequest, path: &Path) -> PlatformHelp {
    package::read_local(path)
        .and_then(|corpus| decode(request, corpus, PlatformHelpSourceKind::External, path))
        .unwrap_or_else(|reason| PlatformHelp::missing(request.clone(), reason))
}

/// Where a published package's corpus comes from.
pub enum PackageInput<'a> {
    /// A platform directory holding `shcntx_ru.hbk` and `shlang_ru.hbk`,
    /// extracted in-process.
    Archives(&'a Path),
    /// An existing corpus JSON, taken as it is.
    CorpusJson(&'a Path),
}

/// The notice shipped inside every package: the corpus is 1C's text, outside
/// this project's code licenses. Worded after `NOTICE` and the provenance of
/// the formerly bundled corpus.
pub const PACKAGE_NOTICE: &str = include_str!("package_notice.md");

/// Prepares a corpus package for publication at `output` (which must not
/// exist), with its manifest, digest and notice. The corpus must decode, so a
/// package that the analyzer would refuse is never produced.
pub fn prepare_package(
    input: PackageInput<'_>,
    output: &Path,
    corpus_id: &str,
    platform_version: Option<&str>,
) -> Result<package::CorpusManifest, String> {
    let (corpus, version, extractor) = match input {
        PackageInput::Archives(dir) => {
            let pair = installed::pair_in(dir, "--hbk-dir")?;
            let scratch =
                output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let data = html_parser::extract_corpus_from_hbk(&pair.shcntx, &pair.shlang, scratch)
                .map_err(|error| format!("{error:#}"))?;
            let json = data.to_json().map_err(|error| format!("{error:#}"))?;
            let version = platform_version.map(str::to_owned).or_else(|| pair.platform_version());
            (json, version, html_parser::EXTRACTOR_VERSION.to_owned())
        }
        PackageInput::CorpusJson(path) => {
            let bytes =
                std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
            (bytes, platform_version.map(str::to_owned), "imported".to_owned())
        }
    };
    PlatformSnapshot::from_corpus_json(&corpus)
        .map_err(|error| format!("the corpus would be refused: {error}"))?;
    package::write_package_with(
        output,
        &corpus,
        corpus_id,
        version.as_deref(),
        &extractor,
        &[("NOTICE.md", PACKAGE_NOTICE.as_bytes())],
    )
}

/// What [`bootstrap`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// The configured source now serves the process.
    Installed,
    /// The same source already served the process.
    AlreadyActive,
    /// Another source already serves the process; it keeps serving.
    RestartRequired(String),
}

/// Selects, loads and publishes the help corpus for this process, and reports
/// the result in the log. `config` is `None` when the project has no config file.
pub fn bootstrap(config: Option<&ProjectConfig>, project_root: &Path) -> BootstrapOutcome {
    let _span = tracing::info_span!("platform_help_bootstrap").entered();
    let request = match requested_source(config, project_root) {
        Ok(request) => request,
        Err(reason) => {
            tracing::warn!(%reason, "invalid platform_help configuration; platform help is unavailable");
            return publish(PlatformHelp::missing(PlatformHelpRequest::None, reason), config);
        }
    };
    if let Some(warning) = active_source_conflict(&request) {
        tracing::warn!("{warning}");
        return BootstrapOutcome::RestartRequired(warning);
    }
    if bsl_platform::active_platform_help_request() == Some(&request) {
        return BootstrapOutcome::AlreadyActive;
    }
    publish(load(&request), config)
}

fn publish(help: PlatformHelp, config: Option<&ProjectConfig>) -> BootstrapOutcome {
    match install_platform_help(help) {
        Ok(InstallOutcome::Installed) => {
            report_active(config.and_then(|c| c.target_platform_version.as_deref()));
            BootstrapOutcome::Installed
        }
        Ok(InstallOutcome::AlreadyActive) => BootstrapOutcome::AlreadyActive,
        Err(conflict) => {
            let warning = restart_warning(&conflict.active, &conflict.requested);
            tracing::warn!("{warning}");
            BootstrapOutcome::RestartRequired(warning)
        }
    }
}

/// Logs the served source, its provenance and its status for `target`.
pub fn report_active(target: Option<&str>) {
    let data = PlatformData::instance();
    let status = data.help_status_for_target(target);
    match (data.help_origin(), data.help_missing_reason()) {
        (Some(origin), None) => tracing::info!(
            source = %data.help_request(),
            origin = origin.source.as_str(),
            location = origin.location.as_deref().unwrap_or("-"),
            platform_version = origin.platform_version.as_deref().unwrap_or("unknown"),
            digest = origin.digest.as_deref().unwrap_or("-"),
            status = status.as_str(),
            types = data.all_types().len(),
            methods = data.all_methods().len(),
            "platform help loaded"
        ),
        (Some(origin), Some(reason)) => tracing::warn!(
            source = %data.help_request(),
            origin = origin.source.as_str(),
            platform_version = origin.platform_version.as_deref().unwrap_or("unknown"),
            status = status.as_str(),
            reason,
            "platform help degraded to the built-in interface facts; descriptions are empty"
        ),
        (None, reason) => tracing::warn!(
            source = %data.help_request(),
            status = status.as_str(),
            reason = reason.unwrap_or("unknown"),
            "platform help unavailable; platform lookups and docs are empty"
        ),
    }
}

/// When the configuration now asks for a source other than the one serving the
/// process: the message to show. The served snapshot does not change.
pub fn source_change_warning(
    config: Option<&ProjectConfig>,
    project_root: &Path,
) -> Option<String> {
    // A process that never selected a source has no running source to change:
    // that is library use, and a late selection warns on its own in bootstrap.
    if bsl_platform::active_platform_help_request() == Some(&PlatformHelpRequest::Unselected) {
        return None;
    }
    let request = match requested_source(config, project_root) {
        Ok(request) => request,
        Err(reason) => return Some(format!("invalid platform_help configuration: {reason}")),
    };
    active_source_conflict(&request)
}

fn active_source_conflict(request: &PlatformHelpRequest) -> Option<String> {
    let active = bsl_platform::active_platform_help_request()?;
    (active != request).then(|| restart_warning(active, request))
}

fn restart_warning(active: &PlatformHelpRequest, requested: &PlatformHelpRequest) -> String {
    format!(
        "platform help source changed from `{active}` to `{requested}`; restart the server to switch the platform help source (the current help stays in use)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/../bsl-platform/tests/fixtures/help/corpus.json");

    #[test]
    fn external_path_loads_package_and_legacy_json() {
        let dir = tempfile::tempdir().unwrap();
        let corpus = std::fs::read(FIXTURE).unwrap();
        let package = dir.path().join("package");
        package::write_package(&package, &corpus, "fixture", Some("8.3.27.2214"), "test").unwrap();

        let request = PlatformHelpRequest::ExternalPath(package.clone());
        let help = load(&request);
        let origin = help.origin.as_ref().expect("package loads");
        assert_eq!(origin.platform_version.as_deref(), Some("8.3.27.2214"));
        assert_eq!(origin.digest.as_deref(), Some(package::sha256_hex(&corpus).as_str()));
        assert!(!help.snapshot.is_empty());

        let legacy = load(&PlatformHelpRequest::ExternalPath(FIXTURE.into()));
        assert_eq!(legacy.origin.as_ref().unwrap().platform_version, None);
    }

    #[test]
    fn failing_sources_are_missing_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let absent = load(&PlatformHelpRequest::ExternalPath(dir.path().join("nope.json")));
        assert!(absent.origin.is_none() && absent.snapshot.is_empty());
        assert!(absent.missing_reason.unwrap().contains("nope.json"));

        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, b"{\"methods\": 3}").unwrap();
        let broken = load(&PlatformHelpRequest::ExternalPath(broken));
        assert!(broken.missing_reason.unwrap().contains("invalid help corpus"));

        let none = load(&PlatformHelpRequest::None);
        assert!(none.origin.is_none() && none.missing_reason.unwrap().contains("disabled"));
    }

    #[test]
    fn unset_configuration_asks_for_the_build_default() {
        // The corpus-contract run names its corpus for the process; that is the
        // documented stand-in for an unset configuration.
        let expected = match std::env::var_os(bsl_platform::CORPUS_ENV).filter(|v| !v.is_empty()) {
            Some(corpus) => PlatformHelpRequest::ExternalPath(corpus.into()),
            None => PlatformHelpRequest::default_for_build(),
        };
        assert_eq!(requested_source(None, Path::new("/")).unwrap(), expected);
        let config: ProjectConfig =
            serde_json::from_str(r#"{"platformHelp":{"source":"none"}}"#).unwrap();
        assert_eq!(
            requested_source(Some(&config), Path::new("/")).unwrap(),
            PlatformHelpRequest::None
        );
    }
}
