use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use project_model::SourceSet;

use crate::graph_query::GraphDb;

/// One graph-relevant file's identity: canonical `/`-normalised path, source
/// metadata captured during enumeration, and the complete content hash read by
/// that same scan. Graph freshness uses `content_hash`; metadata remains available
/// to diagnostics and unread reporting.
#[derive(Clone)]
pub(crate) struct FileStat {
    pub(crate) path: String,
    /// The physical canonical path used to read the bytes. Kept separately from the
    /// rendered string so a non-UTF8 path is still attributed through the walk's exact
    /// spellings when a root is resolved.
    pub(crate) canonical: PathBuf,
    /// The spelling through which the walk reached the file. A symlink may lead outside
    /// every canonical root; `WorkspaceRoots::root_of` then uses this spelling to retain
    /// the file under the declared root that actually walked to it.
    pub(crate) walked: PathBuf,
    pub(super) mtime: u128,
    pub(crate) len: u64,
    /// The hash read during the owning scan. Keeping it with the stat means
    /// the verdict and the persisted fingerprint describe the same bytes.
    pub(crate) content_hash: Option<[u8; 32]>,
    /// The stat identity `content_hash` was taken under, persisted so a later process can
    /// reuse the hash without reading the file.
    pub(crate) stat: super::content_hash::StatIdentity,
    /// When the bytes behind `content_hash` were read; `None` when they were not.
    pub(crate) observed_at_ns: Option<u128>,
}

impl FileStat {
    /// A row built without touching disk, for tests about what a consumer DOES with
    /// stats rather than how they are produced.
    #[cfg(test)]
    pub(crate) fn for_test(path: &str, mtime: u128, len: u64) -> FileStat {
        FileStat {
            path: path.to_string(),
            canonical: PathBuf::from(path),
            walked: PathBuf::from(path),
            mtime,
            len,
            content_hash: None,
            stat: super::content_hash::StatIdentity { len, mtime_ns: mtime, change: None },
            observed_at_ns: None,
        }
    }

    /// The per-file fingerprint stored in (and compared against) the `files` table.
    /// Must stay deterministic across runs so a reload's recomputed value matches the
    /// stored one for an unchanged file. Real files use their complete byte hash;
    /// the metadata fallback exists only for synthetic diagnostic fixtures and for
    /// reporting an unreadable file as changed rather than silently unchanged.
    pub(crate) fn fingerprint(&self) -> u64 {
        let hash = self.persisted_content_hash();
        u64::from_le_bytes(hash[..8].try_into().expect("blake3 yields >= 8 bytes"))
    }

    /// The complete BLAKE3 digest used by the portable graph fingerprint. It is
    /// the result of the owning scan; do not re-read an unreadable or synthetic
    /// row here, because that would make the verdict and fingerprint describe
    /// different observations of the tree.
    pub(crate) fn content_hash(&self) -> Option<[u8; 32]> {
        self.content_hash
    }

    /// The complete digest persisted for this row, including a distinct marker for a
    /// file whose bytes could not be read. The marker is derived here so every writer
    /// uses the same observation without reaching into the stat fields.
    pub(crate) fn persisted_content_hash(&self) -> [u8; 32] {
        self.content_hash.unwrap_or_else(|| {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"bsl-analyzer-unreadable-file\0");
            hasher.update(&self.mtime.to_le_bytes());
            hasher.update(&self.len.to_le_bytes());
            *hasher.finalize().as_bytes()
        })
    }

    /// The observation behind `content_hash`, for a row whose bytes this scan actually read.
    pub(crate) fn persisted_observation(&self) -> Option<super::content_hash::Observation> {
        Some(super::content_hash::Observation {
            stat: self.stat,
            hash: self.content_hash?,
            observed_at_ns: self.observed_at_ns?,
        })
    }

    /// The durable key for this scan row. Both spellings are required: the canonical
    /// spelling decides ownership when the target lies under a registered root, while
    /// the walked spelling is the only usable identity for a target outside all roots.
    pub(crate) fn key(&self, roots: &bsl_search::WorkspaceRoots) -> Option<bsl_search::FileKey> {
        roots.root_of(&self.walked, &self.canonical)
    }
}

/// The drift fingerprint of a single file on disk, matching the per-file value
/// the stats scan produces, or `None` if it is absent or not a regular file.
/// Lets the event-driven drift path re-stat only the changed paths instead of
/// walking the whole workspace — events are hints, this stat is the truth.
pub(crate) fn file_fingerprint(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Some(
        FileStat {
            path: String::new(),
            canonical: PathBuf::new(),
            walked: PathBuf::new(),
            mtime,
            len: meta.len(),
            content_hash: std::fs::read(path).ok().map(|bytes| *blake3::hash(&bytes).as_bytes()),
            stat: super::content_hash::StatIdentity::of(&meta),
            observed_at_ns: None,
        }
        .fingerprint(),
    )
}

/// Enumerate every graph-relevant file (`.bsl` sources + `.xml` metadata descriptors)
/// under the scan roots, once. Covers both extensions because graph resolution
/// depends on configuration visibility registered from the metadata, not only on
/// module text. Captures the complete content hash in the same observation as the
/// walk and mirrors the loader's scan roots and symlink/canonicalization policy so
/// it compares the same file universe (otherwise it would report phantom drift).
/// Retained as the test-side wrapper (production callers derive the roots from an
/// explicit `ProjectSnapshot` so stats and topology come from one project state).
#[cfg(test)]
pub(crate) fn scan_file_stats(workspace_root: &Path) -> Vec<FileStat> {
    scan_stats_over_roots(&super::input::scan_roots(workspace_root)).0
}

/// The scan over an explicit set of roots (each a directory, or occasionally a
/// single file for a misconfigured extension path), projected into stats shape —
/// together with the verdict of the walk that produced them.
///
/// The verdict is returned, not dropped, because the rows alone cannot be told
/// apart from a shorter tree: a caller reconciling a store against them would
/// read a subtree it could not enter as a batch of deletions.
///
/// One call is one traversal (parallel across top-level directories inside
/// [`SourceSet::scan`]). An operation with several passes over the same universe
/// must take ONE `SourceSet` and project it instead of calling this per pass.
/// Test-side wrapper: production always states its exclusions, so the form that
/// narrows by nothing is not reachable there by construction.
#[cfg(test)]
pub(crate) fn scan_stats_over_roots(
    roots: &[PathBuf],
) -> (Vec<FileStat>, super::universe::ScanVerdict) {
    scan_stats_over_roots_excluding(roots, &[])
}

/// [`scan_stats_over_roots`] without descending into `excluded`.
#[cfg(test)]
pub(crate) fn scan_stats_over_roots_excluding(
    roots: &[PathBuf],
    excluded: &[PathBuf],
) -> (Vec<FileStat>, super::universe::ScanVerdict) {
    scan_stats_in_scope(roots, excluded, &project_model::ExcludedPaths::default())
}

/// [`scan_stats_over_roots`] over one project snapshot's scope: its roots, less its
/// cache holes and the directories its user excluded.
pub(crate) fn scan_stats_over_project(
    project: &super::input::ProjectSnapshot,
) -> (Vec<FileStat>, super::universe::ScanVerdict) {
    scan_stats_in_scope(&project.scan_roots, &project.excluded, &project.user_excluded)
}

fn scan_stats_in_scope(
    roots: &[PathBuf],
    excluded: &[PathBuf],
    user_excluded: &project_model::ExcludedPaths,
) -> (Vec<FileStat>, super::universe::ScanVerdict) {
    let set = SourceSet::scan_in_scope(roots, excluded, user_excluded);
    let scan = super::universe::file_stats_with_content_errors(&set, roots);
    (
        scan.stats,
        super::universe::ScanVerdict::of(&set).with_content_unreadable(scan.content_unreadable),
    )
}

/// A cheap fingerprint of the workspace identity: the order-independent fold of
/// every graph-relevant file's `(root_id, path, content_hash)` plus the extension-topology
/// hash. Test-side wrapper — production callers scan a universe explicitly and
/// fold it with [`fingerprint_of_project`], so the verdict of the same scan stays in hand.
#[cfg(test)]
pub(super) fn workspace_fingerprint(workspace_root: &Path) -> crate::graph_db::GraphFp {
    let cache = crate::cache::WorkspaceCacheLayout::for_workspace(workspace_root);
    cache.ensure().expect("test cache directory is creatable");
    let excluded: Vec<PathBuf> = cache.exclusions(workspace_root);
    workspace_fingerprint_over(&super::input::ProjectSnapshot::load_excluding(
        workspace_root,
        &excluded,
    ))
}

/// The fingerprint over an already-loaded project snapshot. Test-side companion
/// of [`workspace_fingerprint`] — production brackets scan a universe explicitly
/// and fold it with [`fingerprint_of_project`], keeping the same scan's verdict in hand.
#[cfg(test)]
pub(crate) fn workspace_fingerprint_over(
    project: &super::input::ProjectSnapshot,
) -> crate::graph_db::GraphFp {
    let (stats, _) = scan_stats_over_roots_excluding(&project.scan_roots, &project.excluded);
    fingerprint_of_project(&stats, project).expect("test project scan has owned readable files")
}

/// Fingerprint the graph input in its durable address space. Physical paths are
/// deliberately not part of this value: a graph published for one checkout can
/// be checked against the same tree after it has moved. Every field is length
/// prefixed so root and path boundaries cannot collide. `None` means that a
/// source is outside the supplied root table; an unreadable source receives a
/// distinct marker and the scan verdict keeps the result incomplete.
pub(crate) fn portable_fingerprint_of(
    stats: &[FileStat],
    roots: Option<&bsl_search::WorkspaceRoots>,
    topology: u64,
) -> Option<crate::graph_db::GraphFp> {
    let roots = roots?;
    let mut entries: Vec<(String, String, [u8; 32])> = stats
        .iter()
        .map(|stat| {
            let key = stat.key(roots)?;
            let hash = stat.persisted_content_hash();
            Some((key.root_id, key.path, hash))
        })
        .collect::<Option<Vec<_>>>()?;
    entries.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bsl-analyzer-portable-files-v1\0");
    for (root_id, path, hash) in entries {
        update_len_prefixed(&mut hasher, root_id.as_bytes());
        update_len_prefixed(&mut hasher, path.as_bytes());
        hasher.update(&hash);
    }
    let digest = hasher.finalize();
    Some(crate::graph_db::GraphFp {
        files: u64::from_le_bytes(
            digest.as_bytes()[..8].try_into().expect("blake3 yields >= 8 bytes"),
        ),
        topology,
    })
}

pub(crate) fn fingerprint_of_project(
    stats: &[FileStat],
    project: &super::input::ProjectSnapshot,
) -> Option<crate::graph_db::GraphFp> {
    portable_fingerprint_of(stats, project.search_roots.as_ref(), project.portable_topology)
}

fn update_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Stable 64-bit hash of a snapshot's extension-topology fingerprint. BLAKE3 of
/// the full hex digest (not `DefaultHasher`), so the value persisted in a graph's
/// meta survives a toolchain upgrade. `None` — legacy path-only registration or
/// the invalid-project fallback — hashes as the empty input, distinct from every
/// real digest.
pub(crate) fn topology_u64(configs: &ide::WorkspaceConfigsSnapshot) -> u64 {
    topology_hex_u64(configs.fingerprint.as_deref().unwrap_or(""))
}

/// Check the topology of a graph against the project snapshot used by a transition witness.
/// Keeping the project load outside this predicate lets callers that already loaded it compare
/// against one topology/root snapshot, so a configuration edit cannot land between two
/// independent loads and pair a graph with another generation's roots. The stale-transition
/// path deliberately allows file drift; providers that need a current graph use
/// [`graph_matches_live_project_strict`] instead.
pub(crate) fn graph_matches_live_project(
    graph: &GraphDb,
    project: &super::ProjectSnapshot,
) -> bool {
    let Ok((_, fingerprint, _)) = graph.freshness_token() else {
        return false;
    };
    project.validated
        && project.search_roots.is_some()
        && fingerprint.topology == project.portable_topology
}

/// Strict variant used before handing a cached graph to a long-lived search context provider.
/// The stale-transition path intentionally uses [`graph_matches_live_project`] to keep a
/// topology witness while a rebuild is being scheduled; a provider must prove the complete
/// current byte fingerprint and reject a force-stale publication instead.
pub(crate) fn graph_matches_live_project_strict(
    graph: &GraphDb,
    project: &super::ProjectSnapshot,
) -> bool {
    let Ok((_, fingerprint, force_stale)) = graph.freshness_token() else {
        return false;
    };
    if force_stale || !project.validated || project.search_roots.is_none() {
        return false;
    }
    let universe = super::universe::ScannedUniverse::scan_project(project);
    if !universe.clean() {
        return false;
    }
    let Some(current) = fingerprint_of_project(&universe.stats, project) else {
        return false;
    };
    current == fingerprint
}

/// The BLAKE3-based 64-bit fold shared by every consumer that reduces the
/// topology hex digest to one word (graph freshness, broker identity), so the
/// same topology always reduces to the same value everywhere.
pub(crate) fn topology_hex_u64(hex: &str) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(hex.as_bytes());
    let bytes = hasher.finalize();
    u64::from_le_bytes(bytes.as_bytes()[..8].try_into().expect("blake3 yields >= 8 bytes"))
}

/// Granular drift between a built graph's stored per-file fingerprints and the
/// current on-disk state. The body-only fast path acts on this; today it is computed
/// for observability while the full rebuild still runs.
pub(crate) struct WorkspaceDiff {
    pub(crate) added: Vec<String>,
    pub(crate) removed: Vec<String>,
    pub(crate) modified: Vec<String>,
}

pub(crate) fn classify_changes_with_roots(
    stored: &std::collections::HashMap<bsl_search::FileKey, [u8; 32]>,
    current: &[FileStat],
    roots: Option<&bsl_search::WorkspaceRoots>,
) -> WorkspaceDiff {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut seen = HashSet::with_capacity(current.len());
    for stat in current {
        let Some(key) = roots.and_then(|roots| stat.key(roots)) else {
            added.push(stat.path.clone());
            continue;
        };
        let hash = stat.persisted_content_hash();
        seen.insert(key.clone());
        match stored.get(&key) {
            None => added.push(stat.path.clone()),
            Some(fp) if *fp != hash => modified.push(stat.path.clone()),
            Some(_) => {}
        }
    }
    let mut removed = Vec::new();
    for key in stored.keys().filter(|key| !seen.contains(*key)) {
        if let Some(path) = roots.and_then(|roots| roots.resolve(key)) {
            removed.push(path.to_string_lossy().into_owned());
        } else {
            removed.push(key.path.clone());
        }
    }
    added.sort();
    modified.sort();
    removed.sort();
    WorkspaceDiff { added, removed, modified }
}

impl WorkspaceDiff {
    pub(crate) fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }

    /// Whether any changed file is `.xml` metadata. Metadata drift can change
    /// configuration visibility for *any* module, so it forces a full rebuild — no
    /// fast path is sound for it.
    #[cfg(test)]
    pub(crate) fn touches_metadata(&self) -> bool {
        self.added
            .iter()
            .chain(&self.removed)
            .chain(&self.modified)
            .any(|p| bsl_conventions::str_has_extension(p, bsl_conventions::XML_EXTENSION))
    }
}

/// Computes a semantic structure hash of an XML document, ignoring elements
/// like `<Version>` and `<Comment>`, XML comments, and non-significant whitespace.
pub(crate) fn xml_semantic_hash(xml_text: &str) -> [u8; 32] {
    let Ok(doc) = bsl_metadata::roxmltree::Document::parse(xml_text) else {
        return *blake3::hash(xml_text.as_bytes()).as_bytes();
    };
    let mut hasher = blake3::Hasher::new();
    hash_xml_node(doc.root(), &mut hasher);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
pub(crate) fn xml_semantic_hash_u64(xml_text: &str) -> u64 {
    let hash = xml_semantic_hash(xml_text);
    u64::from_le_bytes(hash[..8].try_into().expect("blake3 hash >= 8 bytes"))
}

pub(crate) fn xml_semantic_hash_file(path: &Path) -> Option<[u8; 32]> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(xml_semantic_hash(&content))
}

fn hash_xml_node(node: bsl_metadata::roxmltree::Node<'_, '_>, hasher: &mut blake3::Hasher) {
    if node.is_element() {
        let tag = node.tag_name().name();
        if node.parent().is_some_and(|parent| {
            parent.is_element()
                && parent.tag_name().name().eq_ignore_ascii_case("Properties")
                && (tag.eq_ignore_ascii_case("Version") || tag.eq_ignore_ascii_case("Comment"))
        }) {
            return;
        }
        hasher.update(b"E+");
        hash_xml_part(node.tag_name().namespace().unwrap_or(""), hasher);
        hash_xml_part(tag, hasher);
        let mut attrs: Vec<_> = node.attributes().collect();
        attrs.sort_by_key(|a| (a.namespace().unwrap_or(""), a.name()));
        for attr in attrs {
            hasher.update(b"A");
            hash_xml_part(attr.namespace().unwrap_or(""), hasher);
            hash_xml_part(attr.name(), hasher);
            hash_xml_part(attr.value(), hasher);
        }
    } else if node.is_text() {
        if let Some(text) = node.text() {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                hasher.update(b"T");
                hash_xml_part(trimmed, hasher);
            }
        }
    }

    for child in node.children() {
        if child.is_comment() {
            continue;
        }
        hash_xml_node(child, hasher);
    }
    if node.is_element() {
        hasher.update(b"E-");
    }
}

fn hash_xml_part(value: &str, hasher: &mut blake3::Hasher) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

/// Classify per-file drift between the stored fingerprint map (read from a built
/// graph's `files` table) and the current on-disk stats. A path present only on disk
/// is `added`, present only in the store is `removed`, present in both with a
/// different fingerprint is `modified`.
pub(crate) fn classify_changes(
    stored: &std::collections::HashMap<String, u64>,
    current: &[FileStat],
) -> WorkspaceDiff {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut seen: HashSet<&str> = HashSet::with_capacity(current.len());

    for stat in current {
        seen.insert(stat.path.as_str());
        match stored.get(&stat.path) {
            None => added.push(stat.path.clone()),
            Some(&fp) if fp != stat.fingerprint() => modified.push(stat.path.clone()),
            Some(_) => {}
        }
    }
    let mut removed: Vec<String> =
        stored.keys().filter(|p| !seen.contains(p.as_str())).cloned().collect();

    added.sort();
    modified.sort();
    removed.sort();
    WorkspaceDiff { added, removed, modified }
}

#[cfg(test)]
mod diff_tests {
    use super::*;

    #[test]
    fn metadata_drift_is_seen_in_any_extension_spelling() {
        let diff = WorkspaceDiff {
            added: vec!["cfg/Meta.XML".to_string()],
            removed: Vec::new(),
            modified: Vec::new(),
        };
        assert!(diff.touches_metadata(), "верхнерегистровый .XML — тоже дрейф метаданных");
    }

    #[test]
    fn xml_semantic_hash_ignores_version_and_comment() {
        let xml1 = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject>
    <Configuration>
        <Properties>
            <Name>TestConfig</Name>
            <Version>1.0.0.1</Version>
            <Comment>Initial release</Comment>
        </Properties>
    </Configuration>
</MetaDataObject>"#;

        let xml2 = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject>
    <Configuration>
        <Properties>
            <Name>TestConfig</Name>
            <Version>1.0.0.2</Version>
            <Comment>Hotfix release with updated comment</Comment>
        </Properties>
    </Configuration>
</MetaDataObject>"#;

        assert_eq!(xml_semantic_hash(xml1), xml_semantic_hash(xml2));
        assert_eq!(xml_semantic_hash_u64(xml1), xml_semantic_hash_u64(xml2));
    }

    #[test]
    fn xml_semantic_hash_detects_structural_changes() {
        let xml1 =
            r#"<Configuration><Properties><Name>Config1</Name></Properties></Configuration>"#;
        let xml2 =
            r#"<Configuration><Properties><Name>Config2</Name></Properties></Configuration>"#;
        assert_ne!(xml_semantic_hash(xml1), xml_semantic_hash(xml2));
    }

    #[test]
    fn xml_semantic_hash_ignores_comments_and_formatting() {
        let xml1 =
            r#"<Configuration><Properties><Name>Config1</Name></Properties></Configuration>"#;
        let xml2 = r#"<Configuration>
    <!-- some XML comment -->
    <Properties>
        <Name>
            Config1
        </Name>
    </Properties>
</Configuration>"#;
        assert_eq!(xml_semantic_hash(xml1), xml_semantic_hash(xml2));
    }

    #[test]
    fn xml_semantic_hash_keeps_tree_boundaries_and_namespaces() {
        assert_ne!(xml_semantic_hash("<A><B/></A>"), xml_semantic_hash("<A/><B/>"));
        assert_ne!(
            xml_semantic_hash("<A xmlns='urn:one'/>"),
            xml_semantic_hash("<A xmlns='urn:two'/>")
        );
    }
}
