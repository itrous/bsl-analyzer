//! Projections of one workspace scan into the graph's two legacy shapes.
//!
//! `SourceSet::scan` walks once and keeps every occurrence; each projection here
//! filters and de-duplicates the way ITS consumer historically keyed files, so the
//! two shapes can come from one traversal without changing either universe:
//! enumeration keys by canonical `PathBuf`, the stats scan keys by the canonical
//! lossy string, and both filter BEFORE de-duplicating — the order the old code
//! used, which decides who survives when several spellings collapse into one key.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use project_model::{file_role::FileRole, SourceSet};
use vfs::FileId;

use super::scan::FileStat;

/// What one walk may speak for, carried alongside its projections.
///
/// The two counters are NOT interchangeable and are kept apart on purpose.
/// `unreadable` says the file list is SHORT: a file the walk did not list may
/// still be on disk. `canonical_fallbacks` says a listed file kept its walked
/// spelling, so its KEY may differ from the one an earlier walk produced for the
/// same file — a shifted key looks exactly like a removal paired with an
/// addition. A consumer that folds both into one flag can still act correctly,
/// but one that must decide per direction needs to tell them apart.
///
/// The counters are PRIVATE, and production code has exactly one constructor —
/// [`ScanVerdict::of`]. That is not tidiness: a verdict assembled field by field at a
/// call site (`ScanVerdict { unreadable: set.unreadable, canonical_fallbacks: 0 }`)
/// compiles, reads plausibly, and disables one leg of the policy silently. No test can
/// catch it either — a canonicalisation fallback is not producible through a real tree,
/// so every filesystem stand exercises `unreadable` alone. Making the shape
/// unconstructible is the only closure that cannot be forgotten. `Default` is NOT
/// derived for the same reason: a derived one is a second crate-wide constructor, and
/// the verdict it yields — every counter zero — is precisely the one that says "this
/// walk speaks for the whole tree".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanVerdict {
    unreadable: usize,
    canonical_fallbacks: usize,
    content_unreadable: usize,
}

impl ScanVerdict {
    /// The only way a verdict comes into being: read off a walk that produced it.
    pub(crate) fn of(set: &SourceSet) -> ScanVerdict {
        ScanVerdict {
            unreadable: set.unreadable,
            canonical_fallbacks: set.canonical_fallbacks,
            content_unreadable: 0,
        }
    }

    pub(crate) fn with_content_unreadable(mut self, count: usize) -> ScanVerdict {
        self.content_unreadable = count;
        self
    }

    /// A verdict stated outright, for tests about what a consumer DOES with one.
    #[cfg(test)]
    pub(crate) fn for_test(unreadable: usize, canonical_fallbacks: usize) -> ScanVerdict {
        ScanVerdict { unreadable, canonical_fallbacks, content_unreadable: 0 }
    }

    /// Whether this walk may speak for the whole tree — see [`SourceSet::clean`].
    pub(crate) fn clean(&self) -> bool {
        self.coverage_complete() && self.identity_exact() && self.content_unreadable == 0
    }

    /// Nothing was hidden from the walk, so a file it did not list is genuinely
    /// absent.
    pub(crate) fn coverage_complete(&self) -> bool {
        self.unreadable == 0
    }

    /// Every listed file carries its physical spelling, so its key is comparable
    /// with the keys an earlier walk produced.
    pub(crate) fn identity_exact(&self) -> bool {
        self.canonical_fallbacks == 0
    }
}

/// One traversal of the scan roots, projected into every shape the graph's passes
/// consume. Passes handed the same instance provably share one universe: the
/// enumeration a build lowers, the stats its `files` table persists and the
/// fingerprint its bracket compares all come from the same walk, so no pass can
/// see a tree another pass did not.
pub(crate) struct ScannedUniverse {
    /// The `.bsl` enumeration — see [`bsl_files_from`].
    pub(crate) files: Vec<(FileId, PathBuf)>,
    /// The `.bsl` + `.xml` stats rows — see [`file_stats_with_content_errors`].
    pub(crate) stats: Vec<FileStat>,
    /// The walked spelling of each retained stat in this exact scan. The
    /// graph keeps canonical source paths for reading, but durable root attribution
    /// needs the walked alias when a symlink target lies outside the registered roots.
    walked_by_canonical: HashMap<PathBuf, PathBuf>,
    verdict: ScanVerdict,
    /// How many files this walk read rather than reusing a remembered hash.
    pub(crate) hashed_files: usize,
}

impl ScannedUniverse {
    /// One walk, all projections.
    /// Test-side wrapper: production always states its exclusions, so the form that
    /// narrows by nothing is not reachable there by construction.
    #[cfg(test)]
    pub(crate) fn scan(roots: &[PathBuf]) -> ScannedUniverse {
        Self::scan_excluding(roots, &[])
    }

    /// [`Self::scan`] without descending into `excluded`.
    #[cfg(test)]
    pub(crate) fn scan_excluding(roots: &[PathBuf], excluded: &[PathBuf]) -> ScannedUniverse {
        Self::scan_in_scope(roots, excluded, &project_model::ExcludedPaths::default())
    }

    /// The universe of one project snapshot: its roots, less both its cache holes and
    /// the directories its user excluded — the scope that snapshot was taken under,
    /// never a newer one.
    pub(crate) fn scan_project(project: &super::input::ProjectSnapshot) -> ScannedUniverse {
        Self::scan_in_scope(&project.scan_roots, &project.excluded, &project.user_excluded)
    }

    fn scan_in_scope(
        roots: &[PathBuf],
        excluded: &[PathBuf],
        user_excluded: &project_model::ExcludedPaths,
    ) -> ScannedUniverse {
        let set = SourceSet::scan_in_scope(roots, excluded, user_excluded);
        let ContentScan { stats, content_unreadable: unreadable, hashed } =
            file_stats_with_content_errors(&set, roots);
        let walked_by_canonical =
            stats.iter().map(|stat| (stat.canonical.clone(), stat.walked.clone())).collect();
        // The content pass is part of the same walk's authority. A listed
        // file that could not be hashed must keep the snapshot dirty; it is
        // never equivalent to an empty or deleted input.
        let universe = ScannedUniverse {
            files: bsl_files_from(&set),
            stats,
            walked_by_canonical,
            verdict: ScanVerdict::of(&set).with_content_unreadable(unreadable),
            hashed_files: hashed,
        };
        tracing::debug!(
            files = universe.stats.len(),
            hashed = universe.hashed_files,
            "graph scan read file contents"
        );
        universe
    }

    /// Whether the walk behind these projections may speak for the whole tree —
    /// see [`SourceSet::clean`]. A publication over an unclean universe must not
    /// claim a coherent snapshot, and a cache must not be adopted as fresh
    /// against one.
    pub(crate) fn clean(&self) -> bool {
        self.verdict.clean()
    }

    /// The walked spelling paired with a canonical path in this scan, if the path
    /// was present in the universe.
    pub(crate) fn walked_path_for(&self, canonical: &Path) -> Option<&Path> {
        self.walked_by_canonical.get(canonical).map(PathBuf::as_path)
    }

    /// Resolve every graph-relevant row through the roots and expose aliases under
    /// both canonical and walked spellings. Node rows use the walked spelling, while
    /// unread reports and file rows may still carry the canonical one.
    pub(crate) fn file_key_aliases(
        &self,
        roots: &bsl_search::WorkspaceRoots,
    ) -> Option<rustc_hash::FxHashMap<String, bsl_search::FileKey>> {
        let mut aliases = rustc_hash::FxHashMap::default();
        for stat in &self.stats {
            let key = stat.key(roots)?;
            aliases.insert(stat.path.replace('\\', "/"), key.clone());
            aliases.insert(stat.path.clone(), key.clone());
            aliases.insert(stat.walked.to_string_lossy().replace('\\', "/"), key);
        }
        Some(aliases)
    }

    /// Resolve a canonical path from this scan using the walked spelling retained
    /// for it. This is used for unread files whose loader reports the canonical path.
    pub(crate) fn key_for_path(
        &self,
        roots: &bsl_search::WorkspaceRoots,
        canonical: &Path,
    ) -> Option<bsl_search::FileKey> {
        let walked = self.walked_path_for(canonical)?;
        roots.root_of(walked, canonical)
    }
}

/// The `.bsl` universe in enumeration shape: canonical paths with stable
/// [`FileId`]s assigned in scan order, first occurrence of each canonical path
/// winning — the keying `enumerate_bsl_files` has always used. Membership is the
/// walker's own role verdict, so a case-variant spelling the walker took is in.
pub(crate) fn bsl_files_from(set: &SourceSet) -> Vec<(FileId, PathBuf)> {
    let mut entries: Vec<(FileId, PathBuf)> = Vec::new();
    let mut seen: HashSet<&PathBuf> = HashSet::new();
    let mut next_id = 0u32;
    for file in &set.files {
        if file.role != FileRole::Source {
            continue;
        }
        if !seen.insert(&file.canonical) {
            continue;
        }
        entries.push((FileId(next_id), file.canonical.clone()));
        next_id += 1;
    }
    entries
}

/// The `.bsl` + `.xml` universe in stats shape: `(canonical lossy string, mtime,
/// len, complete content hash)` rows, first occurrence of each STRING winning —
/// the stats scan has always keyed by the converted string, so two canonical paths
/// that collapse into one lossy spelling still yield one row.
#[cfg(test)]
pub(crate) fn file_stats_from(set: &SourceSet) -> Vec<FileStat> {
    file_stats_with_content_errors(set, &[]).stats
}

pub(crate) fn file_stats_with_content_errors(set: &SourceSet, roots: &[PathBuf]) -> ContentScan {
    // The rows are decided before any byte is read: a second spelling of one
    // canonical path is dropped here, so the file is read once however the reads
    // are scheduled.
    let mut seen: HashSet<String> = HashSet::new();
    let listed: Vec<(&project_model::WalkedFile, String, super::content_hash::StatIdentity)> = set
        .files
        .iter()
        .filter(|file| file.role != FileRole::Ignored)
        .filter_map(|file| {
            let path = file.canonical.to_string_lossy().into_owned();
            seen.insert(path.clone())
                .then(|| (file, path, super::content_hash::StatIdentity::of(&file.metadata)))
        })
        .collect();
    // Hashing the contents is the scan's cost on a cold cache (7 GB on a large
    // configuration); each file's hash is independent, so they are read on a pool,
    // largest first, under a cap on the bytes in flight.
    let sizes: Vec<u64> = listed.iter().map(|(_, _, stat)| stat.len).collect();
    let contents = super::byte_budget::map_largest_first(
        &sizes,
        super::byte_budget::SCAN_BYTES_IN_FLIGHT,
        |index| {
            let (file, _, stat) = &listed[index];
            super::content_hash::content_of(&file.canonical, *stat)
        },
    );
    let mut stats: Vec<FileStat> = Vec::with_capacity(listed.len());
    let mut content_unreadable = 0;
    let mut hashed = 0;
    for ((file, path, stat), content) in listed.into_iter().zip(contents) {
        hashed += usize::from(content.read);
        if content.hash.is_none() {
            content_unreadable += 1;
        }
        stats.push(FileStat {
            path,
            canonical: file.canonical.clone(),
            walked: file.walked.clone(),
            mtime: stat.mtime_ns,
            len: stat.len,
            content_hash: content.hash,
            stat,
            observed_at_ns: content.observed_at_ns,
        });
    }
    // Only a walk that listed everything can tell a deleted file from one it could not see.
    if set.unreadable == 0 {
        let listed: HashSet<&Path> = stats.iter().map(|stat| stat.canonical.as_path()).collect();
        super::content_hash::retain_listed(roots, &listed);
    }
    ContentScan { stats, content_unreadable, hashed }
}

/// The stats rows of one walk and what reading their contents cost.
pub(crate) struct ContentScan {
    pub(crate) stats: Vec<FileStat>,
    /// Listed files whose bytes could not be read.
    pub(crate) content_unreadable: usize,
    /// Files this walk read, as opposed to reusing a hash remembered for an unchanged stat.
    pub(crate) hashed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    /// A tree whose files are an hour old, so no remembered hash falls inside the racy window.
    fn aged_tree(root: &Path) -> Vec<PathBuf> {
        let files = vec![
            root.join("CommonModules/Первый/Ext/Module.bsl"),
            root.join("CommonModules/Второй/Ext/Module.bsl"),
            root.join("CommonModules/Первый.xml"),
        ];
        for (index, file) in files.iter().enumerate() {
            write(file, &format!("Процедура П{index}() Экспорт КонецПроцедуры"));
            age(file);
        }
        files
    }

    fn age(file: &Path) {
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        fs::OpenOptions::new().write(true).open(file).unwrap().set_modified(hour_ago).unwrap();
    }

    fn hash_of(universe: &ScannedUniverse, file: &Path) -> Option<[u8; 32]> {
        let file = file.canonicalize().unwrap();
        universe.stats.iter().find(|stat| stat.canonical == file).and_then(|stat| stat.content_hash)
    }

    #[test]
    fn an_unchanged_file_is_not_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = aged_tree(&root);
        let roots = vec![root.clone()];

        let first = ScannedUniverse::scan_excluding(&roots, &[]);
        assert_eq!(first.hashed_files, files.len(), "the first walk reads every file");
        let second = ScannedUniverse::scan_excluding(&roots, &[]);
        assert_eq!(second.hashed_files, 0, "an unchanged tree is not read again");
        assert_eq!(hash_of(&second, &files[0]), hash_of(&first, &files[0]));

        write(&files[0], "Процедура Другая() Экспорт КонецПроцедуры");
        age(&files[0]);
        let third = ScannedUniverse::scan_excluding(&roots, &[]);
        assert_eq!(third.hashed_files, 1, "only the edited file is read");
        assert_ne!(hash_of(&third, &files[0]), hash_of(&first, &files[0]));
    }

    /// A write that restores both the size and the modification time still moves the change
    /// time, which a write cannot set back.
    #[cfg(unix)]
    #[test]
    fn a_same_size_edit_with_a_restored_mtime_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = aged_tree(&root);
        let roots = vec![root.clone()];
        let first = ScannedUniverse::scan_excluding(&roots, &[]);
        let before = fs::metadata(&files[0]).unwrap();

        let original = fs::read_to_string(&files[0]).unwrap();
        let replacement = original.replace("П0", "Я0");
        assert_eq!(replacement.len(), original.len());
        fs::write(&files[0], &replacement).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&files[0])
            .unwrap()
            .set_modified(before.modified().unwrap())
            .unwrap();
        let after = fs::metadata(&files[0]).unwrap();
        assert_eq!(
            (after.len(), after.modified().unwrap()),
            (before.len(), before.modified().unwrap())
        );

        let second = ScannedUniverse::scan_excluding(&roots, &[]);
        assert_eq!(second.hashed_files, 1, "the edited file is read again");
        assert_ne!(hash_of(&second, &files[0]), hash_of(&first, &files[0]));
    }

    #[test]
    fn a_deleted_file_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = aged_tree(&root);
        let roots = vec![root.clone()];
        ScannedUniverse::scan_excluding(&roots, &[]);
        assert!(super::super::content_hash::remembers(&files[1]));

        fs::remove_file(&files[1]).unwrap();
        ScannedUniverse::scan_excluding(&roots, &[]);
        assert!(
            !super::super::content_hash::remembers(&files[1]),
            "a walk that no longer lists a file forgets its hash",
        );
        assert!(super::super::content_hash::remembers(&files[0]));
    }

    fn scan(root: &Path) -> SourceSet {
        SourceSet::scan(std::slice::from_ref(&root.to_path_buf()))
    }

    /// Both counters reach the verdict, each answering its own question.
    ///
    /// The canonicalisation-fallback leg has no filesystem stand anywhere — no real
    /// tree produces a fallback (the walk resolves the path or the path does not
    /// exist) — so this is the only place that leg is exercised at all. What keeps a
    /// hand-assembled verdict from disabling it elsewhere is not a test but the
    /// private fields: `of` is the sole way production code obtains one.
    #[test]
    fn the_verdict_carries_both_counters_apart() {
        let hidden = SourceSet { unreadable: 1, ..SourceSet::default() };
        let verdict = ScanVerdict::of(&hidden);
        assert!(!verdict.coverage_complete(), "a subtree stayed unread");
        assert!(verdict.identity_exact(), "but every listed file kept its physical spelling");
        assert!(!verdict.clean());

        let degraded = SourceSet { canonical_fallbacks: 1, ..SourceSet::default() };
        let verdict = ScanVerdict::of(&degraded);
        assert!(verdict.coverage_complete(), "the walk reached everything");
        assert!(!verdict.identity_exact(), "yet one file's key is a walked spelling");
        assert!(!verdict.clean());

        let both = SourceSet { unreadable: 2, canonical_fallbacks: 3, ..SourceSet::default() };
        assert_eq!(
            ScanVerdict::of(&both),
            ScanVerdict::for_test(2, 3),
            "neither counter is derived from the other",
        );

        // Loops and dangling links leave coverage complete: they must not reach the
        // verdict at all, or a tree with one dead symlink would be permanently suspect.
        let benign = SourceSet { loops: 4, dangling: 5, ..SourceSet::default() };
        assert!(ScanVerdict::of(&benign).clean());
    }

    /// The verdict a stats scan hands back describes the walk it just performed — the
    /// step between `SourceSet::scan` and the consumer, which the unit test above does
    /// not cross.
    #[cfg(unix)]
    #[test]
    fn the_stats_scan_reports_the_walk_it_performed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("CommonModules/M/Ext/Module.bsl"), "");
        write(&root.join("Configuration.xml"), "<x/>");

        let roots = vec![root.to_path_buf()];
        let (stats, verdict) = super::super::scan::scan_stats_over_roots(&roots);
        assert!(verdict.clean(), "a healthy tree yields a verdict that may speak for it");
        assert_eq!(stats.len(), 2);

        let closed = root.join("Closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&closed).is_ok() {
            // Permissions do not bind this user (UID 0): the input cannot exist.
            fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let (_, verdict) = super::super::scan::scan_stats_over_roots(&roots);
        assert!(!verdict.coverage_complete(), "a door it could not open shortens the list");
        assert!(!verdict.clean());
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// An independent sequential reference for the enumeration universe: its own
    /// `WalkDir`, the exact-extension predicate and the canonical-`PathBuf`
    /// de-duplication the historical `enumerate_bsl_files` used — deliberately NOT
    /// built from the shared walker, so a defect in the walker or the projection
    /// cannot cancel out of the comparison.
    fn reference_bsl_files(root: &Path) -> Vec<PathBuf> {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in walkdir::WalkDir::new(root).follow_links(true) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|e| e.to_str()) != Some("bsl")
            {
                continue;
            }
            let path = entry.path().canonicalize().unwrap_or_else(|_| entry.path().to_path_buf());
            if seen.insert(path.clone()) {
                files.push(path);
            }
        }
        files.sort();
        files
    }

    #[test]
    fn the_enumeration_projection_matches_an_independent_reference() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("CommonModules/M/Ext/Module.bsl"), "");
        write(&dir.path().join("Catalogs/C/Ext/ObjectModule.bsl"), "");
        write(&dir.path().join("Configuration.xml"), "<x/>");
        write(&dir.path().join("README.md"), "");

        let set = scan(dir.path());
        let mut projected: Vec<PathBuf> =
            bsl_files_from(&set).into_iter().map(|(_, p)| p).collect();
        projected.sort();

        assert_eq!(projected, reference_bsl_files(dir.path()));
        assert_eq!(projected.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn the_enumeration_projection_matches_the_reference_through_links() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        write(&tree.join("M.bsl"), "");
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        write(&root.join("Own.bsl"), "");
        std::os::unix::fs::symlink(&tree, root.join("Linked")).unwrap();
        std::os::unix::fs::symlink(tree.join("M.bsl"), root.join("Alias.bsl")).unwrap();

        let set = scan(&root);
        let mut projected: Vec<PathBuf> =
            bsl_files_from(&set).into_iter().map(|(_, p)| p).collect();
        projected.sort();

        assert_eq!(projected, reference_bsl_files(&root));
    }

    #[test]
    fn a_case_variant_spelling_is_in_both_projections() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Lower.bsl"), "");
        write(&dir.path().join("Upper.BSL"), "");
        write(&dir.path().join("Meta.XML"), "<x/>");

        let set = scan(dir.path());

        // The projections take the walker's own role verdict: whatever spelling
        // the walker admitted is in the universe, no second narrowing.
        assert_eq!(set.files.len(), 3, "the shared walker is case-insensitive");
        let bsl: Vec<PathBuf> = bsl_files_from(&set).into_iter().map(|(_, p)| p).collect();
        assert_eq!(bsl.len(), 2);
        let stats = file_stats_from(&set);
        assert_eq!(stats.len(), 3, "оба .bsl и .XML — вся вселенная в статах");
    }

    #[test]
    fn both_projections_come_from_the_one_scan_not_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Kept.bsl"), "");
        write(&dir.path().join("Doomed.bsl"), "");

        let set = scan(dir.path());
        fs::remove_file(dir.path().join("Doomed.bsl")).unwrap();

        // The set was taken before the deletion, so every projection of it still
        // describes the pre-deletion universe — that is the whole point of sharing
        // one scan across an operation's passes.
        let bsl = bsl_files_from(&set);
        let stats = file_stats_from(&set);
        assert_eq!(bsl.len(), 2);
        assert_eq!(stats.len(), 2);
        let bsl_paths: HashSet<String> =
            bsl.iter().map(|(_, p)| p.to_string_lossy().into_owned()).collect();
        let stat_paths: HashSet<String> = stats.iter().map(|s| s.path.clone()).collect();
        assert_eq!(bsl_paths, stat_paths, "one universe, two shapes");

        let fresh = scan(dir.path());
        assert_eq!(bsl_files_from(&fresh).len(), 1, "a fresh scan does see the deletion");
    }

    /// Two walked files whose canonical paths differ in bytes but share one lossy
    /// spelling: the enumeration keeps both, the stats collapse to the first.
    ///
    /// Assembled rather than walked, because the only filesystems that hold such a pair
    /// are the ones that accept a name outside UTF-8 — APFS refuses one outright
    /// (`EILSEQ`), so on macOS no tree can stand for this. What the projections key by
    /// is theirs alone, and it is decided from the walked set, so a set stated outright
    /// exercises it wherever the tests run. `metadata` is the one part no test can
    /// fabricate, so it is borrowed from real files of the two lengths the assertion
    /// tells apart. The end-to-end pairing — that a real walk yields such a set at all,
    /// name-sorted — is [`a_lossy_collision_survives_a_real_walk`].
    #[cfg(unix)]
    #[test]
    fn a_lossy_collision_keeps_the_projections_on_their_own_keys() {
        use project_model::WalkedFile;
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("short.bsl");
        let long = dir.path().join("long.bsl");
        fs::write(&short, "x").unwrap();
        fs::write(&long, "xx").unwrap();

        // Distinct byte names, one lossy spelling, in the byte order a walk sorts them.
        let walked = |name: &[u8], on_disk: &Path| WalkedFile {
            root: dir.path().to_path_buf(),
            role: FileRole::Source,
            walked: dir.path().join(OsStr::from_bytes(name)),
            canonical: dir.path().join(OsStr::from_bytes(name)),
            metadata: fs::metadata(on_disk).unwrap(),
        };
        let set = SourceSet {
            files: vec![walked(b"\x80.bsl", &short), walked(b"\x81.bsl", &long)],
            ..SourceSet::default()
        };

        let bsl = bsl_files_from(&set);
        assert_eq!(bsl.len(), 2, "enumeration keys by PathBuf: both files stay");
        assert_ne!(bsl[0].1, bsl[1].1, "and on their own keys, not one key twice");
        let stats = file_stats_from(&set);
        assert_eq!(stats.len(), 1, "stats key by the lossy string: the rows collapse");
        // First occurrence wins, and the walk hands them over name-sorted, so the
        // survivor is the byte-wise first name — the deterministic winner that
        // replaced the readdir lottery.
        assert_eq!(stats[0].len, 1, "the surviving row must be the first-listed file's");
    }

    /// The pairing for the stand above: a real walk does hand such a pair to the
    /// projections, sorted by name. Only a filesystem that accepts a name outside UTF-8
    /// can hold one — APFS answers `EILSEQ` — so this half is where such names exist.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_lossy_collision_survives_a_real_walk() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        // Two distinct byte names, one lossy spelling. Different lengths, so the
        // surviving stats row is distinguishable.
        let first = dir.path().join(OsStr::from_bytes(b"\x80.bsl"));
        let second = dir.path().join(OsStr::from_bytes(b"\x81.bsl"));
        fs::write(&first, "x").unwrap();
        fs::write(&second, "xx").unwrap();

        let set = scan(dir.path());

        let bsl = bsl_files_from(&set);
        assert_eq!(bsl.len(), 2, "enumeration keys by PathBuf: both files stay");
        let stats = file_stats_from(&set);
        assert_eq!(stats.len(), 1, "stats key by the lossy string: the rows collapse");
        // The scan is name-sorted, so the surviving row is the byte-wise first
        // name — the deterministic winner that replaced the readdir lottery.
        assert_eq!(stats[0].len, 1, "the surviving row must be the first-sorted file's");
    }

    /// The sequential content pass this scan had before its reads went to a pool,
    /// kept as the reference the parallel pass is held to. Its only caller is the
    /// unix-bound pass test below, so it is unix-bound too: elsewhere it would be dead.
    #[cfg(unix)]
    fn reference_content_scan(set: &SourceSet) -> ContentScan {
        let mut stats: Vec<FileStat> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut content_unreadable = 0;
        let mut hashed = 0;
        for file in &set.files {
            if file.role == FileRole::Ignored {
                continue;
            }
            let path = file.canonical.to_string_lossy().into_owned();
            if !seen.insert(path.clone()) {
                continue;
            }
            let stat = super::super::content_hash::StatIdentity::of(&file.metadata);
            let content = super::super::content_hash::content_of(&file.canonical, stat);
            hashed += usize::from(content.read);
            if content.hash.is_none() {
                content_unreadable += 1;
            }
            stats.push(FileStat {
                path,
                canonical: file.canonical.clone(),
                walked: file.walked.clone(),
                mtime: stat.mtime_ns,
                len: stat.len,
                content_hash: content.hash,
                stat,
                observed_at_ns: content.observed_at_ns,
            });
        }
        ContentScan { stats, content_unreadable, hashed }
    }

    /// A walked row over a real file, for the unix-bound pass test below.
    #[cfg(unix)]
    fn walked(root: &Path, walked: &Path, canonical: &Path) -> project_model::WalkedFile {
        project_model::WalkedFile {
            root: root.to_path_buf(),
            role: project_model::file_role::file_role(canonical),
            walked: walked.to_path_buf(),
            canonical: canonical.to_path_buf(),
            metadata: fs::metadata(canonical).unwrap(),
        }
    }

    /// The content pass on a pool is a scheduling change only: row by row — order,
    /// hash, stat, whether the bytes were observed — and in its counters, it equals
    /// the sequential pass, over a set with an entry that cannot be read whoever runs
    /// the test (a directory), one closed by permissions, and two spellings of one
    /// canonical path.
    #[cfg(unix)]
    #[test]
    fn the_parallel_content_pass_equals_the_sequential_one() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut files = Vec::new();
        for index in 0..46u32 {
            let path = if index % 5 == 0 {
                root.join(format!("Catalogs/Obj{index}.xml"))
            } else {
                root.join(format!("CommonModules/M{index}/Ext/Module.bsl"))
            };
            write(
                &path,
                &format!(
                    "Процедура П{index}() Экспорт КонецПроцедуры {}",
                    "x".repeat((index * 97 % 500) as usize)
                ),
            );
            files.push(walked(&root, &path, &path));
        }
        // A directory named like a module: `read` fails with EISDIR for every user.
        let directory = root.join("CommonModules/Dir/Ext/Module.bsl");
        fs::create_dir_all(&directory).unwrap();
        files.insert(3, walked(&root, &directory, &directory));
        // Two spellings of one file: the second row is dropped, the file read once.
        let twice = root.join("CommonModules/M1/Ext/Module.bsl");
        let link = root.join("Link/Module.bsl");
        files.insert(20, walked(&root, &link, &twice));
        // Closed by permissions, unless permissions do not bind this user.
        let closed = root.join("CommonModules/Closed/Ext/Module.bsl");
        write(&closed, "Процедура З() КонецПроцедуры");
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        let closed_binds = fs::read(&closed).is_err();
        if !closed_binds {
            eprintln!("skipping the chmod 000 leg: mode 0o000 is not an obstacle for this user");
        }
        files.push(walked(&root, &closed, &closed));
        assert_eq!(files.len(), 49);
        let set = SourceSet {
            files,
            unreadable: 0,
            loops: 0,
            dangling: 0,
            canonicalizations: 0,
            canonical_fallbacks: 0,
        };

        // Each pass starts with nothing remembered under the root, so both read every
        // file however long the set-up took against the racy window of a fresh write.
        let forget = || {
            super::super::content_hash::retain_listed(std::slice::from_ref(&root), &HashSet::new())
        };
        forget();
        let expected = reference_content_scan(&set);
        forget();
        let actual = file_stats_with_content_errors(&set, &[]);
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(actual.hashed, expected.hashed);
        assert_eq!(actual.hashed, 48, "every distinct canonical path is read once");
        assert_eq!(actual.content_unreadable, expected.content_unreadable);
        assert_eq!(actual.content_unreadable, 1 + usize::from(closed_binds));
        assert_eq!(actual.stats.len(), expected.stats.len());
        for (actual, expected) in actual.stats.iter().zip(&expected.stats) {
            assert_eq!(actual.path, expected.path);
            assert_eq!(actual.canonical, expected.canonical);
            assert_eq!(actual.walked, expected.walked);
            assert_eq!(actual.content_hash, expected.content_hash, "{}", actual.path);
            assert_eq!(actual.stat, expected.stat);
            assert_eq!(actual.len, expected.len);
            assert_eq!(actual.mtime, expected.mtime);
            assert_eq!(actual.observed_at_ns.is_some(), expected.observed_at_ns.is_some());
        }
        assert!(actual.stats.iter().any(|stat| stat.canonical == twice && stat.walked == twice));
        assert!(
            !actual.stats.iter().any(|stat| stat.walked == link),
            "the second spelling is dropped"
        );
    }
}
