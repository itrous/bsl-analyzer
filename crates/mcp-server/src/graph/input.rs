use std::path::{Path, PathBuf};
use std::sync::Arc;

use base_db::{SourceDatabase, SourceRoot, SourceRootId};
use ide::RootDatabaseImpl;
#[cfg(test)]
use project_model::SourceSet;
use vfs::FileId;

/// The whole workspace is loaded into a single source root.
pub(crate) const GRAPH_SOURCE_ROOT: SourceRootId = SourceRootId(0);

/// One immutable projection of the validated project, loaded ONCE per
/// operation (graph build, incremental update, resident build): the scan
/// universe and the workspace-configs snapshot (roots + dependency closures +
/// topology fingerprint) travel together, so no operation can mix the file
/// enumeration of one project state with the config registration of another.
pub(crate) struct ProjectSnapshot {
    pub workspace_root: PathBuf,
    pub scan_roots: Vec<PathBuf>,
    pub configs: ide::WorkspaceConfigsSnapshot,
    /// Topology identity in the graph's durable address space. The physical
    /// configuration fingerprint in `configs` remains available to diagnostics
    /// and coordination; this value is only for graph/search reuse.
    pub portable_topology: u64,
    /// Search ownership derived from the same validated project as `scan_roots` and
    /// `configs`. It travels with a published build so the publish hook never reloads a
    /// newer project and mixes its roots with an older graph.
    pub search_roots: Option<bsl_search::WorkspaceRoots>,
    /// Subtrees inside `scan_roots` that no pass of this operation may descend into.
    ///
    /// Travels with the snapshot for the same reason everything else here does: the file
    /// enumeration of one project state must never be mixed with the registration of
    /// another, and "what is not mine to read" is part of that enumeration.
    pub excluded: Vec<PathBuf>,
    /// The directories the user took out of the project (`[source].exclude`), from the
    /// same validated project as `scan_roots`. Kept apart from `excluded`: nothing
    /// declared inside one of these wins it back, a root included.
    pub user_excluded: project_model::ExcludedPaths,
    /// The age of this composition, for ordering hub declarations made from it.
    ///
    /// Taken when the snapshot is, and carried into every declaration that speaks for it:
    /// the moment a pass takes its snapshot and the moment it declares the roots are
    /// arbitrarily far apart, and a build overtaken by a newer one must not roll the hub
    /// back onto the roots it left behind (github#184). See
    /// [`crate::change_hub::next_topology_epoch`].
    pub declaration_epoch: u64,
    /// Whether these roots are a VALIDATED declaration or the restricted fallback below.
    ///
    /// A fallback declares nothing. It is what the loader does when it cannot read the
    /// project, and a root it fails to mention is not a root that has gone away — so it may
    /// never be the authority that retires an obligation belonging to one.
    pub validated: bool,
}

impl ProjectSnapshot {
    /// Graph passes run only after the daemon bootstrap validated the project;
    /// a config broken by a mid-session edit restricts the scan to the
    /// workspace root (loud in logs) instead of walking a wrong universe.
    /// Test-side wrapper. Production states its exclusions: a pass that walked the
    /// tree without them would index the server's own cache as workspace source, and
    /// the compiler is the only thing that catches a call site added later and missed.
    #[cfg(test)]
    pub(crate) fn load(workspace_root: &Path) -> Self {
        Self::load_excluding(workspace_root, &[])
    }

    /// [`Self::load`] carrying subtrees no pass may descend into.
    pub(crate) fn load_excluding(workspace_root: &Path, excluded: &[PathBuf]) -> Self {
        match crate::project::at(workspace_root) {
            Ok(project) => Self::from_project_excluding(&project, excluded),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "invalid project; graph scan restricted to workspace root, no config roots"
                );
                Self {
                    workspace_root: workspace_root.to_path_buf(),
                    scan_roots: vec![workspace_root.to_path_buf()],
                    configs: ide::WorkspaceConfigsSnapshot::default(),
                    portable_topology: 0,
                    search_roots: None,
                    excluded: excluded.to_vec(),
                    user_excluded: project_model::ExcludedPaths::default(),
                    declaration_epoch: crate::change_hub::next_topology_epoch(),
                    validated: false,
                }
            }
        }
    }

    /// Test-side wrapper: production always states its exclusions, so the form that
    /// narrows by nothing is not reachable there by construction.
    #[cfg(test)]
    pub(crate) fn from_project(project: &project_model::Project) -> Self {
        Self::from_project_excluding(project, &[])
    }

    pub(crate) fn from_project_excluding(
        project: &project_model::Project,
        excluded: &[PathBuf],
    ) -> Self {
        let scan_roots = project.source_roots();
        // The MCP file universe is enumerated canonically (`enumerate_bsl_files`
        // canonicalizes every `.bsl`), so the registered roots must be canonical
        // too — a raw symlinked root would miss both prefix matching and the
        // unbootstrapped root-join fallbacks.
        let search_roots = crate::project::workspace_roots(project, excluded).0;
        let portable_topology = portable_topology(project, &search_roots, excluded);
        Self {
            workspace_root: project.root.clone(),
            scan_roots,
            configs: ide::WorkspaceConfigsSnapshot::from_project(project).canonicalized(),
            portable_topology,
            search_roots: Some(search_roots),
            excluded: excluded.to_vec(),
            user_excluded: project.source_exclusions().clone(),
            declaration_epoch: crate::change_hub::next_topology_epoch(),
            validated: true,
        }
    }
}

fn portable_topology(
    project: &project_model::Project,
    roots: &bsl_search::WorkspaceRoots,
    excluded: &[PathBuf],
) -> u64 {
    let base = project.configuration_path().unwrap_or(&project.root);
    let topology = project.extension_topology().portable_fingerprint(base, &project.root);
    let workspace = std::fs::canonicalize(&project.root).unwrap_or_else(|_| project.root.clone());
    let mut exclusions: Vec<Vec<u8>> = excluded
        .iter()
        .filter_map(|path| portable_exclusion_key(roots, &workspace, path))
        .collect();
    // The user's exclusions change which files the roots hold, so a graph or corpus
    // built under one list must not be reused under another — even with every root
    // where it was.
    exclusions.extend(
        project
            .source_exclusions()
            .declared()
            .map(|path| portable_user_exclusion_key(&project.root, path)),
    );
    // Cache aliases must identify one exclusion even when its directory appears.
    exclusions.sort();
    exclusions.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bsl-analyzer-portable-topology-v1\0");
    hasher.update(topology.as_bytes());
    for exclusion in exclusions {
        hasher.update(&(exclusion.len() as u64).to_le_bytes());
        hasher.update(&exclusion);
    }
    let digest = hasher.finalize();
    u64::from_le_bytes(digest.as_bytes()[..8].try_into().expect("blake3 yields >= 8 bytes"))
}

fn portable_exclusion_key(
    roots: &bsl_search::WorkspaceRoots,
    workspace: &Path,
    path: &Path,
) -> Option<Vec<u8>> {
    let declared = path;
    let declared_key = roots.key_of_path(declared);
    let canonical = std::fs::canonicalize(declared).unwrap_or_else(|_| declared.to_path_buf());
    let mut encoded = Vec::new();
    match declared_key.or_else(|| roots.key_of_path(&canonical)) {
        Some(key) => {
            encoded.push(0);
            append_len_prefixed(&mut encoded, key.root_id.as_bytes());
            append_len_prefixed(&mut encoded, key.path.as_bytes());
        }
        None => {
            let relative = canonical
                .strip_prefix(workspace)
                .ok()
                .or_else(|| declared.strip_prefix(roots.workspace()).ok())?;
            encoded.push(1);
            append_len_prefixed(&mut encoded, relative.to_string_lossy().as_bytes());
        }
    }
    Some(encoded)
}

/// A user exclusion in the workspace's durable address space: its declared spelling,
/// relative to the project root when it lies inside it, absolute otherwise. Never
/// resolved through the file system — creating the excluded directory later must not
/// make an unchanged configuration read as a moved topology. Tagged apart from the
/// cache exclusions.
fn portable_user_exclusion_key(project_root: &Path, path: &Path) -> Vec<u8> {
    let spelling = path.strip_prefix(project_root).unwrap_or(path);
    let mut encoded = vec![2];
    append_len_prefixed(&mut encoded, spelling.as_os_str().as_encoded_bytes());
    encoded
}

fn append_len_prefixed(target: &mut Vec<u8>, bytes: &[u8]) {
    target.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    target.extend_from_slice(bytes);
}

/// The configuration source directory plus every extension directory — the file
/// universe both the loader and the drift scan must agree on.
#[cfg(test)]
pub(super) fn scan_roots(workspace_root: &Path) -> Vec<PathBuf> {
    ProjectSnapshot::load(workspace_root).scan_roots
}

/// Enumerate every `.bsl` file under the config + extension roots, assigning a
/// stable [`FileId`] in scan order. No file text is read — this is the cheap
/// file-id↔path map that lets the graph build load one batch of texts at a time
/// while keeping ids consistent across batches.
///
/// Test-side wrapper: production paths scan a `ScannedUniverse` explicitly and
/// share it across their passes, so the verdict of the same scan stays in hand.
#[cfg(test)]
pub(crate) fn enumerate_bsl_files(project: &ProjectSnapshot) -> Vec<(FileId, PathBuf)> {
    super::universe::bsl_files_from(&SourceSet::scan(&project.scan_roots))
}

pub(crate) use ide_host_core::build_source_root;

/// A loaded batch database together with the paths whose bytes could not be read.
///
/// The report exists because an unreadable file is registered with empty text and is
/// then indistinguishable from an empty module: every consumer downstream would erase
/// that module's knowledge on a text nobody could read. Callers accumulate `unread`
/// into a SET — one batch is opened many times per build, so a per-call count would
/// multiply.
#[must_use]
pub(crate) struct BatchLoad {
    pub(crate) db: RootDatabaseImpl,
    pub(crate) unread: Vec<PathBuf>,
}

/// Build a batch database that shares the whole-workspace `source_root` (so any
/// target is addressable by path through the module index) but loads text only for
/// `batch_files` — the only modules this database lowers.
///
/// `file_source_root` is set ONLY for `batch_files`: the per-file source-root input
/// is read solely for the file being lowered (resolver / infer / `get_file_path`),
/// and the build never lowers a non-batch file. Cross-batch call targets resolve
/// through the path-keyed module index built from the shared source root, which
/// never consults `file_source_root`. Setting it for all files would re-pay a
/// whole-config-sized loop on every batch database for no resolution benefit.
pub(crate) fn db_for_files(
    source_root: &SourceRoot,
    batch_files: &[(FileId, PathBuf)],
    configs: &ide::WorkspaceConfigsSnapshot,
    config_cache: Option<&Arc<ide::GraphConfigCache>>,
) -> BatchLoad {
    let mut db = RootDatabaseImpl::default();
    if let Some(cache) = config_cache {
        db.set_graph_config_cache(Arc::clone(cache));
    }
    db.set_source_root(GRAPH_SOURCE_ROOT, source_root.clone());
    let mut unread = Vec::new();
    for (file_id, path) in batch_files {
        db.set_file_source_root(*file_id, GRAPH_SOURCE_ROOT);
        match std::fs::read_to_string(path) {
            Ok(text) => db.set_file_text(*file_id, &text),
            Err(e) => {
                tracing::warn!(path = %path.display(), "graph scan: read failed: {e}");
                // The empty overlay is load-bearing, not leniency: without a text
                // input `file_text_query` re-reads from disk and panics. What changes
                // is that the substitution stops being silent.
                db.set_file_unreadable(*file_id);
                unread.push(path.clone());
            }
        }
    }
    db.set_workspace_configs_snapshot(configs.clone());
    ide::warm_batch_config_roots(&db, batch_files);
    BatchLoad { db, unread }
}

/// Like [`db_for_files`] but disk-backed: registers each file's content revision
/// instead of pinning its text as a salsa input, then drops the text. The resident
/// diagnostics database holds the WHOLE workspace, so the eager `set_file_text` path
/// would pin every file's `Arc<str>` in the overlay map (outside the salsa LRU) and
/// OOM on a large config. Here `file_text_query` re-reads each file from disk on
/// demand under its `lru` cap (`base_db::queries::file_text_query`), verifying the
/// bytes against the recorded revision — the same disk-backed contract the LSP server
/// and the CLI `analyze` path use, so only the working set's text stays resident.
///
/// `file_source_root` is set for every file (not just a batch): `file_text_query`
/// derives the on-disk path through it, so a lazily-read file must have it. An
/// unreadable file falls back to an empty overlay so a later query yields `""`
/// instead of panicking on the disk re-read.
pub(crate) fn db_for_files_lazy(
    source_root: &SourceRoot,
    all_files: &[(FileId, PathBuf)],
    configs: &ide::WorkspaceConfigsSnapshot,
    config_cache: Option<&Arc<ide::GraphConfigCache>>,
) -> BatchLoad {
    let mut db = RootDatabaseImpl::default();
    if let Some(cache) = config_cache {
        db.set_graph_config_cache(Arc::clone(cache));
    }
    db.set_source_root(GRAPH_SOURCE_ROOT, source_root.clone());
    let unread = ide_host_core::register_files_disk_backed(&mut db, GRAPH_SOURCE_ROOT, all_files)
        .into_iter()
        .map(|(path, _err)| path)
        .collect();
    db.set_workspace_configs_snapshot(configs.clone());
    BatchLoad { db, unread }
}

/// Walk the configuration source and extension directories, load every `.bsl`
/// file into a fresh database, and register the config metadata paths. Test-only:
/// the production graph is built straight into SQLite per batch, never as one
/// whole-config in-memory database.
#[cfg(test)]
pub(super) fn load_workspace_db(
    workspace_root: &Path,
) -> anyhow::Result<(RootDatabaseImpl, usize)> {
    let project = ProjectSnapshot::load(workspace_root);
    let files = enumerate_bsl_files(&project);
    let source_root = build_source_root(&files);
    let loaded = db_for_files(&source_root, &files, &project.configs, None);
    Ok((loaded.db, files.len()))
}

#[cfg(test)]
mod tests {
    use super::ProjectSnapshot;
    use crate::graph::test_support::write_common_module;
    use std::fs;
    use std::path::Path;

    /// Configuration under `src/cf`, extension under `src/cfe/Расш` — outside the
    /// configuration directory on purpose. Inside it, the configuration's own
    /// recursive walk would cover the extension anyway, and a scan-root set that
    /// lost the extension would still enumerate its modules.
    fn workspace_with_an_extension(root: &Path) {
        let cf = root.join("src/cf");
        fs::create_dir_all(&cf).unwrap();
        fs::write(cf.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(&cf, "Сервер", true, "&НаСервере\nФункция Ч() Экспорт КонецФункции");

        let ext = root.join("src/cfe/Расш");
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("Configuration.xml"), "<Configuration/>").unwrap();
        write_common_module(
            &ext,
            "РасшМодуль",
            true,
            "&НаСервере\nФункция Р() Экспорт КонецФункции",
        );
    }

    #[test]
    fn the_scan_universe_is_every_root_the_project_declares() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        workspace_with_an_extension(root);
        let project = project_model::Project::new(root).expect("valid test project");
        assert_eq!(project.source_roots().len(), 2, "the stand must declare an extension root");

        let snapshot = ProjectSnapshot::from_project(&project);

        assert_eq!(
            snapshot.scan_roots,
            project.source_roots(),
            "the graph universe must be the project's own root set, not a second derivation"
        );
    }

    /// Separate from the root-set comparison above: that one compares directory
    /// lists, this one asks what the walk actually returns. A root set that lost
    /// the extension yields no module from it, which is the defect itself.
    #[test]
    fn a_module_in_an_extension_reaches_the_graph_universe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        workspace_with_an_extension(root);
        let project = project_model::Project::new(root).expect("valid test project");
        let snapshot = ProjectSnapshot::from_project(&project);

        let files = super::enumerate_bsl_files(&snapshot);

        assert!(
            files.iter().any(|(_, path)| path.ends_with("CommonModules/РасшМодуль/Ext/Module.bsl")),
            "the extension module must be enumerated: {files:?}"
        );
    }

    #[test]
    fn a_fully_excluded_root_is_not_restored_by_the_graph_scanner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        workspace_with_an_extension(root);
        let ordinary = project_model::Project::new(root).unwrap();
        let ordinary_snapshot = ProjectSnapshot::from_project(&ordinary);
        assert!(!super::enumerate_bsl_files(&ordinary_snapshot).is_empty());

        let scoped = project_model::Project::with_config(
            root,
            project_model::ProjectConfig {
                configuration_root: Some("src/cf".to_owned()),
                extensions: Some(vec![project_model::ExtensionDecl::Path(
                    "src/cfe/Расш".to_owned(),
                )]),
                source_exclude: vec!["src".to_owned()],
                ..project_model::ProjectConfig::default()
            },
        )
        .unwrap();
        let snapshot = ProjectSnapshot::from_project(&scoped);
        assert!(snapshot.scan_roots.is_empty());
        assert!(super::enumerate_bsl_files(&snapshot).is_empty());
        assert!(crate::graph::universe::ScannedUniverse::scan_project(&snapshot).files.is_empty());
    }

    #[test]
    fn user_exclusions_change_portable_identity_and_the_scanned_universe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        workspace_with_an_extension(root);
        let hidden = root.join("src/cf/CommonModules/Hidden/Ext/Module.bsl");
        fs::create_dir_all(hidden.parent().unwrap()).unwrap();
        fs::write(&hidden, "Процедура Скрытая()\nКонецПроцедуры").unwrap();

        let ordinary = project_model::Project::with_config(
            root,
            project_model::ProjectConfig {
                configuration_root: Some("src/cf".to_owned()),
                ..project_model::ProjectConfig::default()
            },
        )
        .unwrap();
        let scoped = project_model::Project::with_config(
            root,
            project_model::ProjectConfig {
                configuration_root: Some("src/cf".to_owned()),
                source_exclude: vec!["src/cf/CommonModules/Hidden".to_owned()],
                ..project_model::ProjectConfig::default()
            },
        )
        .unwrap();
        let ordinary = ProjectSnapshot::from_project(&ordinary);
        let scoped = ProjectSnapshot::from_project(&scoped);

        assert_ne!(ordinary.portable_topology, scoped.portable_topology);
        let ordinary_files = crate::graph::universe::ScannedUniverse::scan_project(&ordinary).files;
        let scoped_files = crate::graph::universe::ScannedUniverse::scan_project(&scoped).files;
        assert!(ordinary_files.iter().any(|(_, path)| path == &hidden));
        assert!(scoped_files.iter().all(|(_, path)| path != &hidden));
        assert!(
            scoped_files
                .iter()
                .any(|(_, path)| path.ends_with("CommonModules/Сервер/Ext/Module.bsl")),
            "the allowed sibling disappeared"
        );
    }

    #[test]
    fn an_owned_cache_directory_created_during_the_first_build_keeps_topology() {
        let dir = tempfile::tempdir().unwrap();
        crate::graph::test_support::sample_workspace(dir.path());
        let cache = crate::cache::WorkspaceCacheLayout::for_workspace(dir.path());

        let before = ProjectSnapshot::load_excluding(dir.path(), &cache.exclusions(dir.path()));
        cache.ensure().unwrap();
        let after = ProjectSnapshot::load_excluding(dir.path(), &cache.exclusions(dir.path()));

        assert_eq!(
            before.portable_topology, after.portable_topology,
            "cache aliases identify one owned hole whether or not its directory exists",
        );
    }
}
