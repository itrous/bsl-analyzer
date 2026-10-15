//! The source set: one walk of the workspace as a value an operation can share.
//!
//! An operation that enumerates files, fingerprints them and reconciles a store
//! against them must do all three over the SAME universe — separate walks can each
//! see a different tree and disagree silently. `SourceSet::scan` walks once, under
//! the policy of [`crate::workspace_walk`], and hands every pass of the operation
//! the same files and the same completeness verdict.
//!
//! The set keeps every walked occurrence, un-deduplicated: consumers key files
//! differently (canonical path, lossy string, `(root, relative)`), so collapsing
//! here would silently lose entries some projection still needs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use stdx::path_exclusion::ExcludedPaths;

use crate::workspace_walk::{
    classify_walk_error, process_file_entry, walk_tree, WalkOutcome, WalkedFile,
};

/// Everything one traversal of the workspace roots saw, plus its verdict.
#[derive(Debug, Default)]
pub struct SourceSet {
    /// Every file the walk reached, once per way it was reached, in a deterministic
    /// order: roots in the caller's order, siblings by name, subtrees depth-first.
    pub files: Vec<WalkedFile>,
    /// See [`WalkOutcome::unreadable`]: coverage is incomplete.
    pub unreadable: usize,
    /// See [`WalkOutcome::loops`]: benign, coverage complete.
    pub loops: usize,
    /// See [`WalkOutcome::dangling`]: benign, coverage complete.
    pub dangling: usize,
    /// See [`WalkOutcome::canonicalizations`].
    pub canonicalizations: usize,
    /// See [`WalkOutcome::canonical_fallbacks`]: identity is degraded.
    pub canonical_fallbacks: usize,
}

impl SourceSet {
    /// Whether this scan may speak for the whole tree: nothing was hidden from it
    /// and every file carries its physical spelling. A caller must not reconcile a
    /// store, reuse a cache or publish a coherent-snapshot claim over a scan that
    /// is not clean. Loops and dangling links are deliberately absent here — both
    /// leave coverage complete.
    pub fn clean(&self) -> bool {
        self.unreadable == 0 && self.canonical_fallbacks == 0
    }

    /// Walk every root once and return the shared set.
    ///
    /// Traversal is parallel across each root's top-level directories, but the
    /// partitioning itself is a sequential shallow pass under the same
    /// classification policy as the deep walk, so an error keeps the depth it
    /// would have had in a single walk: an unreadable root is `unreadable`, a
    /// top-level dangling link stays `dangling` instead of becoming a "root" of
    /// its own, and a file root enters the universe. One call performs exactly
    /// one traversal of the tree and emits exactly one `workspace_scan` event —
    /// callers count those events to prove an operation walked once.
    pub fn scan(roots: &[PathBuf]) -> SourceSet {
        Self::scan_excluding(roots, &[])
    }

    /// [`Self::scan`] without descending into `excluded`.
    ///
    /// For subtrees the caller owns and the walk must not read back as sources — the
    /// analyzer's own derived cache, which by default sits inside the very tree being
    /// scanned. Kept as a separate entry point so the plain `scan` stays the one that
    /// narrows by nothing: an exclusion is always something a caller states, never
    /// something the walk decides by name.
    pub fn scan_excluding(roots: &[PathBuf], excluded: &[PathBuf]) -> SourceSet {
        Self::scan_in_scope(roots, excluded, &ExcludedPaths::default())
    }

    /// [`Self::scan_excluding`] that also never enters a directory the user took out of
    /// the project — including a root itself, which then contributes nothing. Kept apart
    /// from `excluded`, whose roots-inside-a-hole carve-out the user's list must not get.
    pub fn scan_in_scope(
        roots: &[PathBuf],
        excluded: &[PathBuf],
        user_excluded: &ExcludedPaths,
    ) -> SourceSet {
        SCANS_ON_THREAD.with(|c| c.set(c.get() + 1));
        let _span = tracing::info_span!("workspace_scan", roots = roots.len()).entered();
        let scope = crate::path_scope::PathScope::with_exclusions(roots, excluded, user_excluded);
        let mut outcome = WalkOutcome::default();
        let mut slots: Vec<Slot> = Vec::new();
        for root in roots {
            partition_root(root, &scope, &mut outcome, &mut slots);
        }

        // Deep-walk the directory units in parallel. `collect` keeps the input
        // order, so splicing the results back into the slot sequence reproduces
        // the exact order a single sorted depth-first walk would have produced.
        let unit_outcomes: Vec<WalkOutcome> = slots
            .par_iter()
            .filter_map(|slot| match slot {
                Slot::Subtree { unit, root } => {
                    let mut unit_outcome = WalkOutcome::default();
                    walk_tree(unit, root, &scope, &mut unit_outcome);
                    Some(unit_outcome)
                }
                Slot::File(_) => None,
            })
            .collect();

        let mut unit_outcomes = unit_outcomes.into_iter();
        for slot in slots {
            match slot {
                Slot::File(file) => outcome.files.push(*file),
                Slot::Subtree { .. } => {
                    let unit_outcome =
                        unit_outcomes.next().expect("one deep-walk result per subtree slot");
                    outcome.absorb(unit_outcome);
                }
            }
        }

        let set = SourceSet {
            files: outcome.files,
            unreadable: outcome.unreadable,
            loops: outcome.loops,
            dangling: outcome.dangling,
            canonicalizations: outcome.canonicalizations,
            canonical_fallbacks: outcome.canonical_fallbacks,
        };
        tracing::debug!(
            target: "workspace_scan",
            files = set.files.len(),
            unreadable = set.unreadable,
            canonical_fallbacks = set.canonical_fallbacks,
            "workspace scan complete"
        );
        set
    }
}

thread_local! {
    static SCANS_ON_THREAD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many workspace scans THIS thread has performed — the observable behind
/// walk-count gates: an operation that shares one scan across its passes shows
/// exactly the expected count here, and a pass that quietly walks on its own is
/// one extra. Thread-local on purpose: a scan is initiated on the operation's own
/// thread, and a process-wide count would let concurrent operations (or parallel
/// tests) pollute each other's readings.
pub fn scans_performed_on_thread() -> usize {
    SCANS_ON_THREAD.with(|c| c.get())
}

/// One position in the sorted top-level sequence of a root: either a file taken by
/// the shallow pass, or a directory whose subtree the parallel phase fills in.
enum Slot {
    File(Box<WalkedFile>),
    Subtree { unit: PathBuf, root: PathBuf },
}

/// The sequential shallow pass over one root (depth 0-1): classifies the root and
/// every top-level entry exactly as a deep walk would, and turns only top-level
/// DIRECTORIES into parallel units. Anything else — files, links of every kind,
/// errors — is handled here, at its original depth, because a unit walk restarts
/// depth at zero and would misread a top-level dangling link as an unreadable root.
fn partition_root(
    root: &Path,
    scope: &crate::path_scope::PathScope,
    outcome: &mut WalkOutcome,
    slots: &mut Vec<Slot>,
) {
    let mut dir_cache: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut resolved = crate::path_scope::ResolvedDirs::default();
    let shallow = walkdir::WalkDir::new(root)
        .follow_links(true)
        .max_depth(1)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            if entry.file_type().is_dir() || entry.depth() == 0 || entry.path_is_symlink() {
                !scope.prunes_dir(entry.path(), entry.path_is_symlink(), &mut resolved)
            } else {
                !scope.exclusions().excludes_walked_file(entry.path())
            }
        });
    for entry in shallow {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                classify_walk_error(&e, outcome);
                continue;
            }
        };
        if entry.file_type().is_dir() {
            // The root itself arrives at depth 0; its children are the units.
            if entry.depth() == 1 {
                slots.push(Slot::Subtree {
                    unit: entry.path().to_path_buf(),
                    root: root.to_path_buf(),
                });
            }
            continue;
        }
        let before = outcome.files.len();
        process_file_entry(&entry, root, &mut dir_cache, outcome);
        if outcome.files.len() > before {
            let file = outcome.files.pop().expect("a file was just pushed");
            slots.push(Slot::File(Box::new(file)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_role::FileRole;
    use std::fs;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn scan_one(root: &Path) -> SourceSet {
        SourceSet::scan(&[root.to_path_buf()])
    }

    fn walked_paths(set: &SourceSet) -> Vec<PathBuf> {
        set.files.iter().map(|f| f.walked.clone()).collect()
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        // SAFETY: `geteuid` takes no arguments, cannot fail and touches no memory.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn two_scans_of_an_unchanged_tree_agree_on_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["z", "m", "a"] {
            write(&dir.path().join(format!("{name}/inner/M.bsl")), "");
            write(&dir.path().join(format!("{name}.bsl")), "");
        }

        let first = scan_one(dir.path());
        let second = scan_one(dir.path());

        assert_eq!(walked_paths(&first), walked_paths(&second));
        assert_eq!(first.files.len(), 6);
    }

    #[test]
    fn the_order_is_the_sorted_order_not_the_file_systems() {
        let dir = tempfile::tempdir().unwrap();
        // Created in anti-lexicographic order on purpose: a file system that
        // replays insertion order would expose an unsorted walk here.
        for name in ["zz", "yy", "cc", "bb", "aa"] {
            write(&dir.path().join(format!("{name}/M.bsl")), "");
            write(&dir.path().join(format!("{name}.bsl")), "");
        }

        let set = scan_one(dir.path());

        let walked = walked_paths(&set);
        let mut sorted = walked.clone();
        sorted.sort();
        assert_eq!(walked, sorted, "sibling order must come from sorting, not from readdir");
    }

    #[test]
    fn roots_keep_the_callers_order_even_when_unsorted() {
        let dir = tempfile::tempdir().unwrap();
        let b = dir.path().join("b");
        let a = dir.path().join("a");
        write(&b.join("M.bsl"), "");
        write(&a.join("M.bsl"), "");

        let set = SourceSet::scan(&[b.clone(), a.clone()]);

        let roots: Vec<&Path> = set.files.iter().map(|f| f.root.as_path()).collect();
        assert_eq!(roots, vec![b.as_path(), a.as_path()], "root order is the caller's contract");
    }

    #[test]
    fn clean_requires_both_counters_at_zero() {
        let healthy = SourceSet::default();
        assert!(healthy.clean());
        let short = SourceSet { unreadable: 1, ..SourceSet::default() };
        assert!(!short.clean(), "a short walk must not claim the whole tree");
        let degraded = SourceSet { canonical_fallbacks: 1, ..SourceSet::default() };
        assert!(!degraded.clean(), "a walked-spelling substitute degrades identity");
        let benign = SourceSet { loops: 3, dangling: 2, ..SourceSet::default() };
        assert!(benign.clean(), "loops and dead links leave coverage complete");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_root_is_an_incomplete_scan_not_an_empty_tree() {
        if running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root.join("M.bsl"), "");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();

        let set = scan_one(&root);
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(set.files.is_empty());
        assert!(set.unreadable > 0);
        assert!(!set.clean());
    }

    #[test]
    fn a_file_root_is_in_the_universe_and_clean() {
        let dir = tempfile::tempdir().unwrap();
        let standalone = dir.path().join("Standalone.bsl");
        write(&standalone, "");

        let set = SourceSet::scan(std::slice::from_ref(&standalone));

        assert_eq!(set.files.len(), 1);
        assert_eq!(set.files[0].role, FileRole::Source);
        assert_eq!(set.unreadable, 0, "a file root is a legitimate root, not an error");
        assert!(set.clean());
    }

    #[test]
    fn a_healthy_directory_root_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("CommonModules/M/Ext/Module.bsl"), "");
        write(&dir.path().join("Configuration.xml"), "<x/>");

        let set = scan_one(dir.path());

        assert_eq!(set.files.len(), 2);
        assert_eq!(set.unreadable, 0);
        assert!(set.clean());
    }

    #[cfg(unix)]
    #[test]
    fn a_top_level_dangling_link_stays_dangling_after_partitioning() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Live.bsl"), "");
        std::os::unix::fs::symlink(
            dir.path().join("missing/Target.bsl"),
            dir.path().join("Dead.bsl"),
        )
        .unwrap();

        let set = scan_one(dir.path());

        assert!(set.dangling > 0, "a link into nothing must keep its class");
        assert_eq!(
            set.unreadable, 0,
            "partitioning must not shift a top-level entry to depth zero, \
             where a dead link reads as an unreadable root"
        );
        assert!(set.clean());
        assert_eq!(set.files.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_subtree_is_incomplete_coverage_through_the_partitioned_scan() {
        if running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("Visible.bsl"), "");
        let closed = dir.path().join("closed");
        write(&closed.join("Hidden.bsl"), "");
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();

        let set = scan_one(dir.path());
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(set.unreadable > 0);
        assert!(!set.clean());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_benign_through_the_partitioned_scan() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        write(&a.join("b/M.bsl"), "");
        std::os::unix::fs::symlink(&a, a.join("b/loop")).unwrap();

        let set = scan_one(dir.path());

        assert!(set.loops > 0);
        assert_eq!(set.unreadable, 0);
        assert!(set.clean());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_an_ancestor_above_the_unit_root_never_duplicates_a_canonical_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("A")).unwrap();
        write(&root.join("B/M.bsl"), "");
        std::os::unix::fs::symlink(&root, root.join("A/back")).unwrap();

        let set = scan_one(&root);

        // The unit for `A` lost the single walk's ancestor stack, so it may re-walk
        // part of the tree through `back` — the accepted price of partitioning. What
        // it must NOT do is manufacture a second canonical identity or dirty the
        // verdict: occurrences collapse by canonical spelling, and loops are benign.
        let canonicals: std::collections::HashSet<&Path> = set
            .files
            .iter()
            .filter(|f| f.role == FileRole::Source)
            .map(|f| f.canonical.as_path())
            .collect();
        assert_eq!(canonicals.len(), 1, "one physical file, one canonical spelling");
        assert_eq!(set.unreadable, 0);
        assert!(set.clean());
    }

    #[test]
    fn hard_exclusions_prune_both_scan_phases_and_may_remove_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root.join("Allowed.bsl"), "");
        write(&root.join(".tmp/Hidden.bsl"), "");
        write(&root.join(".tmp/deep/HiddenToo.xml"), "");
        write(&root.join(".tmp2/Visible.bsl"), "");
        let excluded = ExcludedPaths::new([root.join("a/../.tmp"), root.join("future")]);

        let set = SourceSet::scan_in_scope(std::slice::from_ref(&root), &[], &excluded);
        let paths = walked_paths(&set);
        assert!(paths.iter().any(|path| path.ends_with("Allowed.bsl")));
        assert!(paths.iter().any(|path| path.ends_with(".tmp2/Visible.bsl")));
        assert!(paths.iter().all(|path| !path.starts_with(root.join(".tmp"))), "{paths:?}");
        assert!(set.clean(), "an intentional prune is still a complete scoped scan");

        let empty = SourceSet::scan_in_scope(
            std::slice::from_ref(&root),
            &[],
            &ExcludedPaths::new([root.clone()]),
        );
        assert!(empty.files.is_empty(), "an excluded walk root was restored");
        assert!(empty.clean(), "an intentionally empty source universe is valid");
    }

    #[cfg(unix)]
    #[test]
    fn an_excluded_unreadable_subtree_is_pruned_before_descent() {
        if running_as_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root.join("Allowed.bsl"), "");
        let hidden = root.join("hidden");
        write(&hidden.join("deep/Hidden.bsl"), "");
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();

        let set = SourceSet::scan_in_scope(
            std::slice::from_ref(&root),
            &[],
            &ExcludedPaths::new([hidden.clone()]),
        );
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(set.unreadable, 0, "the walker entered the excluded directory");
        assert!(set.clean());
        assert!(walked_paths(&set).iter().any(|path| path.ends_with("Allowed.bsl")));
    }

    #[cfg(unix)]
    #[test]
    fn a_walk_through_a_symlink_cannot_reenter_an_excluded_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        write(&real.join("Visible.bsl"), "");
        write(&real.join(".tmp/Hidden.bsl"), "");
        write(&real.join(".tmp2/VisibleToo.bsl"), "");
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        let set = SourceSet::scan_in_scope(
            std::slice::from_ref(&alias),
            &[],
            &ExcludedPaths::new([real.join(".tmp")]),
        );
        let paths = walked_paths(&set);

        assert_eq!(paths.len(), 2, "{paths:?}");
        assert!(paths.iter().all(|path| !path.ends_with(".tmp/Hidden.bsl")));
        assert!(paths.iter().any(|path| path.ends_with(".tmp2/VisibleToo.bsl")));

        let reverse = SourceSet::scan_in_scope(
            std::slice::from_ref(&real),
            &[],
            &ExcludedPaths::new([alias.join(".tmp")]),
        );
        let reverse_paths = walked_paths(&reverse);
        assert_eq!(reverse_paths.len(), 2, "{reverse_paths:?}");
        assert!(reverse_paths.iter().all(|path| !path.ends_with(".tmp/Hidden.bsl")));
        assert!(reverse_paths.iter().any(|path| path.ends_with(".tmp2/VisibleToo.bsl")));
    }
}
