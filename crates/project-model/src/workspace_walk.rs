//! Walking a workspace's source roots under one explicit policy.
//!
//! Every place that enumerates the working tree — the graph's file universe, the
//! topology fingerprint, the search overlay, the boot reconcile — has to agree on
//! what a source file is, whether links are followed and what an incomplete walk
//! means. This module owns that policy; callers own identity: it deliberately does
//! not attribute a file to a root and does not de-duplicate, because the store keys
//! files by `(root, relative path)` while the graph keys them by canonical path, and
//! collapsing either one here would silently lose the other's entries.

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::file_role::{file_role, FileRole};
use crate::path_scope::PathScope;

/// One file the walk reached, in both spellings it can be named by.
#[derive(Debug, Clone)]
pub struct WalkedFile {
    /// Which of the passed roots the walk arrived through.
    pub root: PathBuf,
    pub role: FileRole,
    /// The spelling the walk used to get here — the only handle on a file reached
    /// through a link that leaves every declared root.
    pub walked: PathBuf,
    /// The physical spelling, after resolving every link in the path.
    pub canonical: PathBuf,
    /// Taken WITH the link followed: the target is what gets read, so the target is
    /// what a fingerprint built from `(len, mtime)` must describe.
    pub metadata: Metadata,
}

/// The result of one walk. Completeness is a property of THIS walk, not of the tree:
/// a caller with several independent passes over the same roots must answer for each.
#[derive(Debug, Default)]
pub struct WalkOutcome {
    pub files: Vec<WalkedFile>,
    /// A root or subtree could not be read — coverage is INCOMPLETE, so a caller must
    /// not treat the file list as authoritative (reconciling against it would delete
    /// healthy entries).
    pub unreadable: usize,
    /// A symlink loop. Coverage is COMPLETE: `walkdir` reports the loop and carries on,
    /// having already walked the tree the link points back into.
    pub loops: usize,
    /// A link whose target does not exist. Coverage is COMPLETE: there is no file behind
    /// it to index.
    pub dangling: usize,
    /// How many times the walk canonicalised a path. The per-directory cache is only
    /// observable here: the file list and the elapsed time look the same whether each
    /// directory or each file was resolved, so a gate on the cache needs the count of
    /// the operation itself.
    pub canonicalizations: usize,
    /// How many files kept their WALKED spelling because no canonicalisation
    /// succeeded. Identity is degraded for such a file: its key cannot be matched
    /// against a canonical listing, so a caller comparing two walks must treat this
    /// walk as suspect even when `unreadable` is zero.
    pub canonical_fallbacks: usize,
}

impl WalkOutcome {
    /// Fold another walk's results into this one. Files keep their arrival order;
    /// every error class is a plain sum — none of the counters depends on which
    /// walk saw the error.
    pub fn absorb(&mut self, other: WalkOutcome) {
        self.files.extend(other.files);
        self.unreadable += other.unreadable;
        self.loops += other.loops;
        self.dangling += other.dangling;
        self.canonicalizations += other.canonicalizations;
        self.canonical_fallbacks += other.canonical_fallbacks;
    }
}

/// Walk every root, yielding each source or metadata file once per way the walk reached
/// it.
pub fn walk_workspace_roots(roots: &[PathBuf]) -> WalkOutcome {
    let scope = PathScope::new(roots, &[]);
    let mut outcome = WalkOutcome::default();
    for root in roots {
        walk_one_root(root, &scope, &mut outcome);
    }
    outcome
}

fn walk_one_root(root: &Path, scope: &PathScope, outcome: &mut WalkOutcome) {
    walk_tree(root, root, scope, outcome);
}

/// Walk the tree at `start`, attributing every file to `root`. The two differ when a
/// partitioned scan descends a subtree in isolation: the walk starts at the subtree,
/// but the file still belongs to the workspace root the caller passed.
///
/// Siblings are visited in name order, so the sequence of files is a property of the
/// tree, not of the file system's directory-entry order — callers assign ids by this
/// sequence and compare it across scans.
pub(crate) fn walk_tree(start: &Path, root: &Path, scope: &PathScope, outcome: &mut WalkOutcome) {
    let mut dir_cache: HashMap<PathBuf, PathBuf> = HashMap::new();
    // `filter_entry` rather than a check inside the loop: skipping a directory here
    // means the walk never descends into it, so an excluded subtree costs nothing at
    // all. Filtering entries instead would still pay for the whole tree and only drop
    // its files. Non-directories below the start pass through unless an exclusion
    // pattern matches them — the rest is decided by `process_file_entry`; the start
    // itself is decided here whatever it is, so a file root the user excluded is not
    // taken either.
    let mut resolved = crate::path_scope::ResolvedDirs::default();
    let walk = walkdir::WalkDir::new(start)
        .follow_links(true)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            if entry.file_type().is_dir() || entry.depth() == 0 || entry.path_is_symlink() {
                !scope.prunes_dir(entry.path(), entry.path_is_symlink(), &mut resolved)
            } else {
                !scope.exclusions().excludes_walked_file(entry.path())
            }
        });
    for entry in walk {
        match entry {
            Ok(entry) => process_file_entry(&entry, root, &mut dir_cache, outcome),
            Err(e) => classify_walk_error(&e, outcome),
        }
    }
}

/// The per-entry half of the walk policy: role for both spellings, canonicalisation
/// through the per-directory cache, metadata of the target. Shared by the deep walk
/// and the shallow partitioning pass so the two can never classify a file differently.
pub(crate) fn process_file_entry(
    entry: &walkdir::DirEntry,
    root: &Path,
    dir_cache: &mut HashMap<PathBuf, PathBuf>,
    outcome: &mut WalkOutcome,
) {
    // Following links, so this is the TARGET's type: a link to a directory is not
    // a file, and a directory named `Foo.bsl` never becomes one.
    if !entry.file_type().is_file() {
        return;
    }
    let walked = entry.path();
    // The role has to hold for BOTH spellings. Taking a file whose canonical name
    // alone qualifies would produce a key no walk of the target's root can rebuild,
    // and taking one whose walked name alone qualifies would produce a key the
    // point-update path rejects by suffix — the two together are the only files
    // every path in the system agrees about.
    let role = file_role(walked);
    if role == FileRole::Ignored {
        return;
    }
    let canonical = canonical_path(walked, entry.path_is_symlink(), dir_cache, outcome);
    if file_role(&canonical) != role {
        return;
    }
    let metadata = match entry.metadata() {
        Ok(metadata) => metadata,
        // The file was reachable a moment ago and is not now: the walk saw less
        // than the tree holds, which is exactly what `unreadable` means.
        Err(_) => {
            outcome.unreadable += 1;
            return;
        }
    };
    outcome.files.push(WalkedFile {
        root: root.to_path_buf(),
        role,
        walked: walked.to_path_buf(),
        canonical,
        metadata,
    });
}

/// Sort a walk error into the three classes that mean different things to a caller.
/// Only `unreadable` says the file list is short; treating either of the other two that
/// way would let one dead link or one benign loop block reconciliation forever.
pub(crate) fn classify_walk_error(error: &walkdir::Error, outcome: &mut WalkOutcome) {
    // The root itself. Whatever went wrong, everything under it is hidden, so an empty
    // list here is not a full walk — not even when the root is a link into nothing.
    if error.depth() == 0 {
        outcome.unreadable += 1;
        return;
    }
    if error.loop_ancestor().is_some() || error.path().is_some_and(links_form_a_cycle) {
        outcome.loops += 1;
    } else if error.path().is_some_and(is_dangling_link) {
        outcome.dangling += 1;
    } else {
        outcome.unreadable += 1;
    }
}

/// Whether the link chain starting at `path` comes back to a path it has already been
/// through. `walkdir` sets `loop_ancestor` only for a link back into a directory already
/// on its stack; a file linking to itself, or two links pointing at each other, are
/// rejected by the kernel first and arrive as a plain IO error.
///
/// The errno alone cannot answer this: `ELOOP` is returned just as readily for a long
/// but FINITE chain, and behind that one there is an unread file — calling it a benign
/// cycle would let a caller reconcile against a list that is short. Walking the chain is
/// the only way to tell the two apart, and it happens on the error path only.
///
/// A hop is recognised by what its path RESOLVES to, not by how it is spelled: a link
/// to `sub/../Loop.bsl` returns to itself every step while the string grows without
/// end, and comparing spellings would keep missing the repeat. The link itself cannot
/// be canonicalised — that is the very call the kernel refuses — so its directory is,
/// which also resolves `..` through any links on the way, as lexical folding would not.
///
/// A chain longer than the bound, or one whose directory will not resolve, reads as NOT
/// a cycle: that is the conservative answer, since it makes the caller keep its rows
/// rather than drop them.
fn links_form_a_cycle(path: &Path) -> bool {
    const MAX_HOPS: usize = 256;
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut current = path.to_path_buf();
    for _ in 0..MAX_HOPS {
        let Some(identity) = resolved_identity(&current) else {
            return false;
        };
        if !visited.insert(identity) {
            return true;
        }
        let Ok(target) = std::fs::read_link(&current) else {
            return false;
        };
        current = match current.parent() {
            Some(parent) if target.is_relative() => parent.join(target),
            _ => target,
        };
    }
    false
}

/// Whether ANY component of `path` is a symlink whose chain provably cycles — the
/// point-lookup counterpart of the walk's loop classification. `links_form_a_cycle`
/// alone answers only for the path it is handed: for a file under a cycled DIRECTORY
/// (`D -> D` above `D/M.bsl`) its identity step canonicalises the parent, which is the
/// very call the kernel refuses, and it conservatively says "not a cycle". The walk,
/// however, classifies `D` itself as a benign loop and never yields the file — so a
/// point lookup that stopped at the final component would disagree with the walk and
/// keep a row the walk-driven reconcile removes. Checking each component keeps the two
/// answers the same. A long but finite chain still reads as NOT a cycle, on any
/// component: behind it there is a live file.
pub fn path_crosses_a_link_cycle(path: &Path) -> bool {
    let mut prefix = PathBuf::new();
    for component in path.components() {
        prefix.push(component);
        let is_link = std::fs::symlink_metadata(&prefix)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        if is_link && links_form_a_cycle(&prefix) {
            return true;
        }
    }
    false
}

/// A path's identity for cycle detection: its directory resolved for real, plus its own
/// name. `None` when the directory cannot be resolved, which leaves the caller with the
/// conservative answer rather than a guess.
fn resolved_identity(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?;
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty())?;
    Some(std::fs::canonicalize(parent).ok()?.join(name))
}

/// The canonical path of a walked file, reusing one canonicalisation per containing
/// directory: only directory components can hide links, so a plain file inherits its
/// directory's canonical prefix. A file that is ITSELF a link is resolved in full.
///
/// The cache is keyed by the WALKED directory, not the canonical one. Two directory
/// links to one target are two distinct ways to reach the same files, and each file
/// has to keep the spelling the walk actually used to get to it.
fn canonical_path(
    walked: &Path,
    is_symlink: bool,
    dir_cache: &mut HashMap<PathBuf, PathBuf>,
    outcome: &mut WalkOutcome,
) -> PathBuf {
    if is_symlink {
        return canonicalize_counted(walked, outcome);
    }
    // A root given as a bare relative file name has an EMPTY parent, which resolves to
    // nothing: the per-directory shortcut has no directory to work from, and falling
    // back to the walked spelling would hand back a relative path where the contract
    // promises a physical one.
    let parent = walked.parent().filter(|parent| !parent.as_os_str().is_empty());
    let (Some(parent), Some(name)) = (parent, walked.file_name()) else {
        return canonicalize_counted(walked, outcome);
    };
    if let Some(canonical_parent) = dir_cache.get(parent) {
        return canonical_parent.join(name);
    }
    outcome.canonicalizations += 1;
    match std::fs::canonicalize(parent) {
        Ok(canonical_parent) => {
            dir_cache.insert(parent.to_path_buf(), canonical_parent.clone());
            canonical_parent.join(name)
        }
        // The directory would not resolve, so resolving the file itself is the only
        // way left to reach a physical spelling.
        Err(_) => canonicalize_counted(walked, outcome),
    }
}

fn canonicalize_counted(path: &Path, outcome: &mut WalkOutcome) -> PathBuf {
    outcome.canonicalizations += 1;
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        // This is the ONE place a walked spelling substitutes for a canonical one:
        // the earlier parent-resolution failure merely re-routed here, so counting
        // it too would report two fallbacks for one degraded file.
        outcome.canonical_fallbacks += 1;
        path.to_path_buf()
    })
}

/// Whether a path is a link whose target cannot exist, as opposed to one whose target
/// merely could not be read. The kinds are allow-listed on purpose: a permission error,
/// an IO error or a stale network handle all describe a file that is still there, and
/// calling those absent would let a caller reconcile a live file out of its store.
fn is_dangling_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
        && matches!(std::fs::metadata(path), Err(e) if target_cannot_exist(e.kind()))
}

/// `NotADirectory` belongs here beside `NotFound`: a path component that is a plain
/// file makes the target unreachable in principle, not just for now.
fn target_cannot_exist(kind: ErrorKind) -> bool {
    matches!(kind, ErrorKind::NotFound | ErrorKind::NotADirectory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// The one test that has to change the process-wide working directory holds this
    /// while it does, and restores the directory before releasing it.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn walk(root: &Path) -> WalkOutcome {
        walk_workspace_roots(&[root.to_path_buf()])
    }

    fn walked_names(outcome: &WalkOutcome) -> Vec<String> {
        let mut names: Vec<String> = outcome
            .files
            .iter()
            .map(|f| f.walked.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        // SAFETY: `geteuid` takes no arguments, cannot fail and touches no memory.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn a_healthy_walk_reports_no_errors() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("CommonModules/M/Ext/Module.bsl"), "Процедура П() КонецПроцедуры");
        write(&dir.path().join("Configuration.xml"), "<x/>");

        let outcome = walk(dir.path());

        assert_eq!(outcome.files.len(), 2);
        assert_eq!((outcome.unreadable, outcome.loops, outcome.dangling), (0, 0, 0));
        assert_eq!(outcome.canonical_fallbacks, 0);
    }

    #[test]
    fn a_failed_canonicalisation_counts_one_fallback_not_two() {
        let mut outcome = WalkOutcome::default();
        let mut cache = HashMap::new();
        // Both the parent and the file itself fail to canonicalise: the parent
        // failure only re-routes to the whole-file attempt, so exactly ONE walked
        // spelling substitutes for a canonical one.
        let path = Path::new("/nonexistent-bsl-analyzer-probe/dir/File.bsl");
        let resolved = canonical_path(path, false, &mut cache, &mut outcome);
        assert_eq!(resolved, path);
        assert_eq!(outcome.canonical_fallbacks, 1, "one degraded file, one fallback");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_subtree_is_counted_as_incomplete_coverage() {
        if running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Visible.bsl"), "");
        let closed = dir.path().join("closed");
        write(&closed.join("Hidden.bsl"), "");
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();

        let outcome = walk(dir.path());
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(outcome.unreadable > 0, "an unreadable subtree must not look like a full walk");
        assert_eq!(outcome.loops, 0);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_root_is_counted_rather_than_reported_as_an_empty_tree() {
        if running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root.join("M.bsl"), "");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();

        let outcome = walk(&root);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(outcome.files.is_empty());
        assert!(outcome.unreadable > 0, "an unreadable root is not an empty tree");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_leaves_coverage_complete() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        write(&a.join("b/M.bsl"), "");
        std::os::unix::fs::symlink(&a, a.join("b/loop")).unwrap();

        let outcome = walk(dir.path());

        assert!(outcome.loops > 0, "the loop must be reported");
        assert_eq!(outcome.unreadable, 0, "a loop does not make coverage incomplete");
        assert_eq!(walked_names(&outcome), vec!["M.bsl"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_loop_does_not_hang_and_yields_each_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        write(&a.join("b/M.bsl"), "");
        write(&a.join("b/N.bsl"), "");
        std::os::unix::fs::symlink(&a, a.join("b/loop")).unwrap();

        let outcome = walk(dir.path());

        assert_eq!(walked_names(&outcome), vec!["M.bsl", "N.bsl"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_leaves_coverage_complete() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Live.bsl"), "");
        std::os::unix::fs::symlink(
            dir.path().join("missing/Target.bsl"),
            dir.path().join("Dead.bsl"),
        )
        .unwrap();

        let outcome = walk(dir.path());

        assert!(outcome.dangling > 0, "a link into nothing must be its own class");
        assert_eq!(
            outcome.unreadable, 0,
            "a dead link is not incomplete coverage: there is no file behind it"
        );
        assert_eq!(outcome.loops, 0);
        assert_eq!(walked_names(&outcome), vec!["Live.bsl"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_points_at_itself_leaves_coverage_complete() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Live.bsl"), "");
        std::os::unix::fs::symlink("Loop.bsl", dir.path().join("Loop.bsl")).unwrap();

        let outcome = walk(dir.path());

        assert_eq!(
            (outcome.unreadable, outcome.loops, outcome.dangling),
            (0, 1, 0),
            "a cycle the OS rejects before walkdir sees an ancestor is still a cycle"
        );
        assert_eq!(walked_names(&outcome), vec!["Live.bsl"]);
    }

    /// The point-lookup predicate must agree with the walk on every shape of cycle: a
    /// cycled ANCESTOR directory hides the file from the walk (a benign loop, nothing
    /// yielded), so a point lookup of the file must call it a cycle too — while a link
    /// that merely chains too far is a live file on both sides.
    #[cfg(unix)]
    #[test]
    fn a_file_under_a_cycled_directory_crosses_a_link_cycle() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Live.bsl"), "");
        std::os::unix::fs::symlink("D", dir.path().join("D")).unwrap();

        let outcome = walk(dir.path());
        assert!(outcome.loops > 0, "the walk classifies the cycled directory as a loop");
        assert!(
            super::path_crosses_a_link_cycle(&dir.path().join("D/M.bsl")),
            "a component-wise check sees the cycle the final component hides"
        );
        assert!(
            super::path_crosses_a_link_cycle(&dir.path().join("D")),
            "the cycled link itself is a cycle"
        );
        assert!(
            !super::path_crosses_a_link_cycle(&dir.path().join("Live.bsl")),
            "a plain live file crosses nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_long_finite_chain_does_not_cross_a_link_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let mut target = dir.path().join("Target.bsl");
        write(&target, "");
        for hop in (0..64).rev() {
            let link = dir.path().join(format!("chain{hop}.bsl"));
            std::os::unix::fs::symlink(&target, &link).unwrap();
            target = link;
        }
        assert!(
            !super::path_crosses_a_link_cycle(&target),
            "a finite chain hides a live file; calling it a cycle would drop the file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cycle_spelled_through_parent_components_is_still_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        write(&dir.path().join("Live.bsl"), "");
        std::os::unix::fs::symlink("sub/../Loop.bsl", dir.path().join("Loop.bsl")).unwrap();

        let outcome = walk(dir.path());

        assert_eq!(
            (outcome.unreadable, outcome.loops, outcome.dangling),
            (0, 1, 0),
            "identity of a path is what it resolves to, not how it is spelled"
        );
        assert_eq!(walked_names(&outcome), vec!["Live.bsl"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_long_but_acyclic_link_chain_is_incomplete_coverage_not_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        let mut target = dir.path().join("outside/Target.bsl");
        write(&target, "");
        for hop in (0..64).rev() {
            let link = root.join(format!("l{hop}"));
            std::os::unix::fs::symlink(&target, &link).unwrap();
            target = link;
        }
        std::os::unix::fs::symlink(&target, root.join("Alias.bsl")).unwrap();

        let outcome = walk(&root);

        assert_eq!(
            outcome.loops, 0,
            "a chain that never returns to itself is not a cycle: there IS a file behind it"
        );
        assert!(outcome.unreadable > 0);
    }

    #[cfg(unix)]
    #[test]
    fn two_links_pointing_at_each_other_are_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Live.bsl"), "");
        std::os::unix::fs::symlink("B.bsl", dir.path().join("A.bsl")).unwrap();
        std::os::unix::fs::symlink("A.bsl", dir.path().join("B.bsl")).unwrap();

        let outcome = walk(dir.path());

        assert_eq!(outcome.unreadable, 0, "nothing is hidden behind a pair of mutual links");
        assert!(outcome.loops > 0);
        assert_eq!(walked_names(&outcome), vec!["Live.bsl"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_through_a_non_directory_is_a_dead_link() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("blocker"), "");
        std::os::unix::fs::symlink("blocker/Target.bsl", dir.path().join("Dead.bsl")).unwrap();

        let outcome = walk(dir.path());

        assert_eq!(
            (outcome.unreadable, outcome.loops, outcome.dangling),
            (0, 0, 1),
            "a path component that is a file means the target cannot exist"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_root_link_whose_target_vanished_is_an_unreadable_root() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        write(&target.join("M.bsl"), "");
        let root = dir.path().join("root");
        std::os::unix::fs::symlink(&target, &root).unwrap();
        fs::remove_dir_all(&target).unwrap();

        let outcome = walk(&root);

        assert!(
            outcome.unreadable > 0,
            "the root vanishing hides the whole tree, so the empty list is not a full walk"
        );
    }

    #[test]
    fn a_plain_file_passed_as_a_root_gets_its_physical_spelling() {
        let _guard = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("M.bsl"), "");
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();
        let absolute = fs::canonicalize("M.bsl").unwrap();

        let outcome = walk_workspace_roots(&[PathBuf::from("M.bsl"), absolute.clone()]);
        std::env::set_current_dir(previous).unwrap();

        assert_eq!(outcome.files.len(), 2);
        assert_eq!(
            outcome.files[0].canonical, outcome.files[1].canonical,
            "one physical file must have one canonical spelling however its root was named"
        );
        assert_eq!(outcome.files[0].canonical, absolute);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_an_unreadable_target_is_incomplete_coverage_not_a_dead_link() {
        if running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        write(&private.join("Target.bsl"), "");
        std::os::unix::fs::symlink(private.join("Target.bsl"), dir.path().join("Alias.bsl"))
            .unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o000)).unwrap();

        let outcome = walk(dir.path());
        fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            outcome.dangling, 0,
            "the target exists; calling it absent would let a caller delete a live file"
        );
        assert!(outcome.unreadable > 0);
    }

    #[test]
    fn a_directory_named_like_a_source_file_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("Foo.bsl")).unwrap();
        write(&dir.path().join("Real.bsl"), "");

        let outcome = walk(dir.path());

        assert_eq!(walked_names(&outcome), vec!["Real.bsl"]);
    }

    #[test]
    fn both_roles_come_out_of_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("M.bsl"), "");
        write(&dir.path().join("M.xml"), "");
        write(&dir.path().join("M.txt"), "");

        let outcome = walk(dir.path());

        let mut roles: Vec<(String, FileRole)> = outcome
            .files
            .iter()
            .map(|f| (f.walked.file_name().unwrap().to_string_lossy().into_owned(), f.role))
            .collect();
        roles.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            roles,
            vec![
                ("M.bsl".to_owned(), FileRole::Source),
                ("M.xml".to_owned(), FileRole::MetadataWatched),
            ]
        );
    }

    #[test]
    fn extension_matching_ignores_case() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Upper.BSL"), "");
        write(&dir.path().join("Upper.XML"), "");

        let outcome = walk(dir.path());

        let mut roles: Vec<FileRole> = outcome.files.iter().map(|f| f.role).collect();
        roles.sort_by_key(|r| format!("{r:?}"));
        assert_eq!(roles, vec![FileRole::MetadataWatched, FileRole::Source]);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_is_taken_only_when_both_spellings_agree_on_its_role() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        write(&outside.join("Target.bsl"), "");
        write(&outside.join("Target.txt"), "");
        write(&outside.join("Target.xml"), "");
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        write(&root.join("Plain.bsl"), "");
        let link = |name: &str, target: &Path| {
            std::os::unix::fs::symlink(target, root.join(name)).unwrap();
        };
        link("SameRole.bsl", &outside.join("Target.bsl"));
        link("ToNonSource.bsl", &outside.join("Target.txt"));
        link("FromNonSource.txt", &outside.join("Target.bsl"));
        link("RoleMismatch.xml", &outside.join("Target.bsl"));
        link("RoleMismatchBack.bsl", &outside.join("Target.xml"));

        let outcome = walk(&root);

        assert_eq!(walked_names(&outcome), vec!["Plain.bsl", "SameRole.bsl"]);
    }

    #[cfg(unix)]
    #[test]
    fn two_aliases_in_one_root_both_come_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside/Target.bsl");
        write(&target, "");
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&target, root.join("Alias1.bsl")).unwrap();
        std::os::unix::fs::symlink(&target, root.join("Alias2.bsl")).unwrap();

        let outcome = walk(&root);

        assert_eq!(walked_names(&outcome), vec!["Alias1.bsl", "Alias2.bsl"]);
        let canonicals: std::collections::HashSet<&Path> =
            outcome.files.iter().map(|f| f.canonical.as_path()).collect();
        assert_eq!(canonicals.len(), 1, "both aliases name one physical file");
    }

    #[test]
    fn a_file_under_two_nested_roots_comes_through_for_each() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("ws");
        let inner = outer.join("src/cf");
        write(&inner.join("M.bsl"), "");

        let outcome = walk_workspace_roots(&[outer.clone(), inner.clone()]);

        assert_eq!(outcome.files.len(), 2, "nesting is allowed; each root reports its own reach");
        let roots: std::collections::HashSet<&Path> =
            outcome.files.iter().map(|f| f.root.as_path()).collect();
        assert_eq!(roots.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_plain_file_under_a_directory_link_keeps_both_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        write(&tree.join("M.bsl"), "");
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&tree, root.join("Linked")).unwrap();

        let outcome = walk(&root);

        assert_eq!(outcome.files.len(), 1);
        let file = &outcome.files[0];
        assert_eq!(file.walked, root.join("Linked/M.bsl"));
        assert_eq!(file.canonical, fs::canonicalize(tree.join("M.bsl")).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn two_directory_links_to_one_tree_keep_their_own_walked_spellings() {
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("tree");
        write(&tree.join("M.bsl"), "");
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&tree, root.join("A")).unwrap();
        std::os::unix::fs::symlink(&tree, root.join("B")).unwrap();

        let outcome = walk(&root);

        let mut walked: Vec<&Path> = outcome.files.iter().map(|f| f.walked.as_path()).collect();
        walked.sort();
        assert_eq!(walked, vec![root.join("A/M.bsl").as_path(), root.join("B/M.bsl").as_path()]);
        let canonicals: std::collections::HashSet<&Path> =
            outcome.files.iter().map(|f| f.canonical.as_path()).collect();
        assert_eq!(canonicals.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn metadata_describes_the_target_of_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside/Target.bsl");
        write(&target, &"x".repeat(4096));
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&target, root.join("Alias.bsl")).unwrap();

        let outcome = walk(&root);

        assert_eq!(outcome.files.len(), 1);
        assert_eq!(
            outcome.files[0].metadata.len(),
            4096,
            "the read follows the link, so the fingerprint must describe the target"
        );
    }

    #[test]
    fn metadata_of_a_plain_file_is_its_own() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("M.bsl"), &"x".repeat(17));

        let outcome = walk(dir.path());

        assert_eq!(outcome.files[0].metadata.len(), 17);
    }

    /// A caller-stated exclusion narrows the walk; a NAME never does. The two live side
    /// by side on purpose: `no_directory_is_excluded_from_the_walk` below keeps the
    /// policy, this keeps the mechanism.
    #[test]
    fn a_stated_exclusion_is_not_descended_into() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".build/vendor/CommonModules/X/Ext/Module.bsl"), "");
        write(&dir.path().join("CommonModules/Y/Ext/Module.bsl"), "");
        // A sibling whose name merely begins the same way: the match is on whole
        // components, and a string prefix would swallow this one too.
        write(&dir.path().join(".buildfoo/M.bsl"), "");

        let mut outcome = WalkOutcome::default();
        let scope = PathScope::new(&[dir.path().to_path_buf()], &[dir.path().join(".build")]);
        walk_one_root(dir.path(), &scope, &mut outcome);

        let names: Vec<String> =
            outcome.files.iter().map(|f| f.walked.display().to_string()).collect();
        assert_eq!(names.len(), 2, "walked: {names:?}");
        assert!(names.iter().all(|n| !n.contains("/.build/")), "walked: {names:?}");
        assert!(names.iter().any(|n| n.contains(".buildfoo")), "walked: {names:?}");
    }

    /// A root inside a hole wins over it — and only for itself.
    ///
    /// Two properties in one, because a coarser rule satisfies the first and fails the
    /// second: dropping the whole hole also keeps the walk non-empty, and re-opens every
    /// other subtree of the cache to every scan from then on. The file watcher decides
    /// the same pair of inputs the same way; the two must agree, or a file ends up
    /// walked but unwatched.
    #[test]
    fn a_root_inside_a_hole_wins_only_for_itself() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join(".build/newext/CommonModules/E/Ext/Module.bsl"), "");
        write(&dir.path().join(".build/vendor/CommonModules/V/Ext/Module.bsl"), "");
        write(&dir.path().join("CommonModules/W/Ext/Module.bsl"), "");

        let roots = vec![dir.path().to_path_buf(), dir.path().join(".build/newext")];
        let scope = PathScope::new(&roots, &[dir.path().join(".build")]);

        let mut outcome = WalkOutcome::default();
        for root in &roots {
            walk_one_root(root, &scope, &mut outcome);
        }
        let names: Vec<String> =
            outcome.files.iter().map(|f| f.walked.display().to_string()).collect();

        assert!(names.iter().any(|n| n.contains("/W/")), "the workspace module: {names:?}");
        assert!(
            names.iter().any(|n| n.contains("/E/")),
            "the declared root under the hole: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("/V/")),
            "the rest of the hole was re-opened: {names:?}"
        );
    }

    /// A hole that swallows a root must never empty the walk.
    ///
    /// An empty outcome has `unreadable == 0`, so `SourceSet::clean` reports a complete
    /// view of the tree — and a reconcile over that deletes every stored row. The
    /// assertion is on the OUTCOME, because that is the value the destructive path reads.
    #[test]
    fn a_hole_covering_a_root_cannot_empty_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("CommonModules/W/Ext/Module.bsl"), "");
        let root = dir.path().to_path_buf();

        for hole in [root.clone(), dir.path().parent().unwrap().to_path_buf()] {
            let scope = PathScope::new(std::slice::from_ref(&root), std::slice::from_ref(&hole));
            let mut outcome = WalkOutcome::default();
            walk_one_root(&root, &scope, &mut outcome);
            assert_eq!(
                outcome.files.len(),
                1,
                "the walk went empty under a hole covering its own root ({})",
                hole.display()
            );
            assert_eq!(outcome.unreadable, 0, "an empty walk would also look complete");
        }
    }

    /// A scan root can carry `..` all the way here: `discover_source_path` builds it as
    /// `root.join(config_root)` and never resolves the result, so `walkdir` hands the
    /// hole test entries spelled `<ws>/../elsewhere/sources/...`. A hole named
    /// absolutely matches none of those, and a lexical test alone silently walks the
    /// cache back in as sources.
    #[test]
    fn a_hole_under_a_root_reached_through_dot_dot_is_still_skipped() {
        let outer = tempfile::tempdir().unwrap();
        let sources = outer.path().join("elsewhere").join("sources");
        fs::create_dir_all(outer.path().join("ws")).unwrap();
        write(&sources.join("CommonModules/W/Ext/Module.bsl"), "");
        write(&sources.join(".build/CommonModules/V/Ext/Module.bsl"), "");

        let root = outer.path().join("ws").join("..").join("elsewhere").join("sources");
        let scope = PathScope::new(std::slice::from_ref(&root), &[sources.join(".build")]);

        let mut outcome = WalkOutcome::default();
        walk_one_root(&root, &scope, &mut outcome);

        let names: Vec<String> =
            outcome.files.iter().map(|f| f.walked.display().to_string()).collect();
        assert!(names.iter().any(|n| n.contains("/W/")), "the sources were not walked: {names:?}");
        assert!(
            !names.iter().any(|n| n.contains("/V/")),
            "the cache was walked back in as sources: {names:?}"
        );
    }

    /// Skipping holes must stay free per entry: the decision is lexical, so it costs no
    /// system call however large the tree is.
    ///
    /// The observable is the count of canonicalisations against the shape of the tree,
    /// not against a walk without holes. That comparison looks like the natural one and
    /// is vacuous: a walk without holes visits exactly the skipped subtree more, so a
    /// per-entry cost lands in BOTH arms and the difference stays constant under every
    /// implementation. What does distinguish them is the rule the walk already obeys —
    /// one canonicalisation per directory holding files, from the per-directory cache in
    /// `canonical_path`. A cost paid per visited ENTRY breaks that equality; the two
    /// tree sizes are here so the assertion is that rule and not a remembered number.
    ///
    /// What it cannot catch: a bare `std::fs::canonicalize` written into the predicate
    /// without counting itself. Nothing observable sees that one — what holds it off is
    /// there being a single implementation of the hole rule to review at all.
    #[test]
    fn skipping_a_hole_costs_no_canonicalisation_per_entry() {
        let measure = |directories: usize| {
            let dir = tempfile::tempdir().unwrap();
            for i in 0..directories {
                write(&dir.path().join(format!("d{i}/M.bsl")), "");
            }
            for i in 0..4 {
                write(&dir.path().join(format!(".build/c{i}/M.bsl")), "");
            }
            let root = dir.path().to_path_buf();
            let walk_with = |holes: &[PathBuf]| {
                let scope = PathScope::new(std::slice::from_ref(&root), holes);
                let mut outcome = WalkOutcome::default();
                walk_one_root(&root, &scope, &mut outcome);
                outcome
            };
            let open = walk_with(&[]);
            let holed = walk_with(&[root.join(".build")]);
            assert_eq!(open.files.len(), directories + 4, "the open walk missed files");
            assert_eq!(holed.files.len(), directories, "the holed walk missed files");
            // The open walk pins the rule the holed one is then measured against: the
            // count follows the directories that hold files, nothing else.
            assert_eq!(
                open.canonicalizations,
                directories + 4,
                "the walk stopped costing one canonicalisation per directory"
            );
            holed.canonicalizations
        };

        for directories in [8, 32] {
            assert_eq!(
                measure(directories),
                directories,
                "skipping a hole cost more than the directories the walk kept"
            );
        }
    }

    #[test]
    fn no_directory_is_excluded_from_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        for excluded in [".git", "target", ".build"] {
            write(&dir.path().join(excluded).join("M.bsl"), "");
        }

        let outcome = walk(dir.path());

        assert_eq!(
            outcome.files.len(),
            3,
            "narrowing belongs to the set of roots; a hidden exclusion here would silently \
             shrink the graph's file universe"
        );
    }

    #[test]
    fn canonicalisation_is_paid_per_directory_not_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = 3;
        let files_per_dir = 20;
        for d in 0..dirs {
            for f in 0..files_per_dir {
                write(&dir.path().join(format!("d{d}/M{f}.bsl")), "");
            }
        }

        let outcome = walk(dir.path());

        assert_eq!(outcome.files.len(), dirs * files_per_dir);
        assert!(
            outcome.canonicalizations <= dirs + 1,
            "expected one canonicalisation per directory, got {} for {} files",
            outcome.canonicalizations,
            outcome.files.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonicalisation_is_paid_per_file_when_every_file_is_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside/Target.bsl");
        write(&target, "");
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let files = 20;
        for f in 0..files {
            std::os::unix::fs::symlink(&target, root.join(format!("A{f}.bsl"))).unwrap();
        }

        let outcome = walk(&root);

        assert!(
            outcome.canonicalizations >= files,
            "the counter must track the operation it names: {} canonicalisations for {} links",
            outcome.canonicalizations,
            files
        );
    }
}
