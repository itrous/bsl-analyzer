//! git2-backed diff reports for analysis scoping.
//!
//! Answers "what changed relative to a reference ref" — typically the vendor
//! branch of a 1C configuration — as per-file changed line ranges computed
//! from a merge-base diff. Two modes:
//!
//! - [`generate_diff_report`]: tree-to-tree between two committed refs (CI,
//!   reproducible);
//! - [`generate_workdir_diff_report`]: merge-base tree against the working
//!   directory plus index, so uncommitted and untracked edits count as
//!   changed (LSP/MCP/CLI on a live checkout).
//!
//! The JSON shape of [`DiffReport`] is byte-compatible with the
//! `diff-report.json` produced by rtools and consumed via `--diff-filter`.

pub mod author_filter;
#[cfg(test)]
pub(crate) mod test_support;

pub use author_filter::{
    head_identity, mailmap_fingerprint, AuthorFilter, AuthorFilterError, LineKeep,
};

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use git2::{Delta, DiffOptions, Repository};
use serde::{Deserialize, Serialize};
use tracing::debug;

#[derive(Debug, Serialize, Deserialize)]
pub struct DiffReport {
    pub base_ref: String,
    pub head_ref: String,
    /// Keyed by workdir-relative slash-separated path.
    pub files: HashMap<String, FileChange>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FileChange {
    /// Changed line ranges `[start, end]` (1-based, inclusive) on the new side.
    /// `None` = the whole file counts as changed (added, untracked, …).
    pub hunks: Option<Vec<[u32; 2]>>,
}

/// A diff report together with the working directory its paths are relative to,
/// so callers can resolve report keys to absolute paths.
#[derive(Debug)]
pub struct RepoDiff {
    /// Canonical absolute path of the repository working directory.
    pub workdir: PathBuf,
    pub report: DiffReport,
}

/// Tree-to-tree diff `merge-base(base, head) … head` (committed state only).
pub fn generate_diff_report(
    repo_path: &Path,
    base: &str,
    head: &str,
    bsl_only: bool,
) -> Result<RepoDiff> {
    let repo = discover_repo(repo_path)?;
    let workdir = repo_workdir(&repo)?;

    debug!("comparing {}...{} (merge-base)", base, head);

    let head_commit = resolve_commit(&repo, head)?;
    let base_tree = merge_base_tree(&repo, base, &head_commit)?;
    let head_tree =
        head_commit.tree().map_err(|e| anyhow!("failed to get tree for '{head}': {e}"))?;

    let mut diff_opts = diff_options(bsl_only);
    let diff = repo
        .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), Some(&mut diff_opts))
        .map_err(|e| anyhow!("failed to create diff: {e}"))?;

    let files = collect_file_changes(&diff)?;
    Ok(RepoDiff {
        workdir,
        report: DiffReport { base_ref: base.to_string(), head_ref: head.to_string(), files },
    })
}

/// Diff `merge-base(base, HEAD) … working directory + index`: uncommitted and
/// untracked edits count as changed. This is the mode for a live checkout.
pub fn generate_workdir_diff_report(
    repo_path: &Path,
    base: &str,
    bsl_only: bool,
) -> Result<RepoDiff> {
    let repo = discover_repo(repo_path)?;
    let workdir = repo_workdir(&repo)?;

    debug!("comparing {}...workdir (merge-base with HEAD)", base);

    let head_commit = resolve_commit(&repo, "HEAD")?;
    let base_tree = merge_base_tree(&repo, base, &head_commit)?;

    let mut diff_opts = diff_options(bsl_only);
    diff_opts.include_untracked(true).recurse_untracked_dirs(true);

    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&base_tree), Some(&mut diff_opts))
        .map_err(|e| anyhow!("failed to create workdir diff: {e}"))?;

    let files = collect_file_changes(&diff)?;
    Ok(RepoDiff {
        workdir,
        report: DiffReport { base_ref: base.to_string(), head_ref: "WORKDIR".to_string(), files },
    })
}

/// Cheap identity of a scope's inputs: the resolved OIDs of `base` and `HEAD`.
/// Changes exactly when a rebuilt scope could differ for an unchanged worktree
/// (a ref moved without touching watched files — fetch, rebase, branch reset).
pub fn scope_ref_identity(repo_path: &Path, base: &str) -> Result<(String, String)> {
    let repo = discover_repo(repo_path)?;
    let base_oid = resolve_commit(&repo, base)?.id().to_string();
    let head_oid = resolve_commit(&repo, "HEAD")?.id().to_string();
    Ok((base_oid, head_oid))
}

fn discover_repo(repo_path: &Path) -> Result<Repository> {
    Repository::discover(repo_path).map_err(|e| anyhow!("git repository not found: {e}"))
}

fn repo_workdir(repo: &Repository) -> Result<PathBuf> {
    repo.workdir()
        .ok_or_else(|| anyhow!("bare repository has no working directory"))?
        .canonicalize()
        .context("failed to canonicalize repository working directory")
}

fn resolve_commit<'r>(repo: &'r Repository, refname: &str) -> Result<git2::Commit<'r>> {
    repo.revparse_single(refname)
        .map_err(|e| anyhow!("ref '{refname}' not found: {e}"))?
        .peel_to_commit()
        .map_err(|e| anyhow!("'{refname}' is not a commit: {e}"))
}

fn merge_base_tree<'r>(
    repo: &'r Repository,
    base: &str,
    head_commit: &git2::Commit<'_>,
) -> Result<git2::Tree<'r>> {
    let base_commit = resolve_commit(repo, base)?;

    let merge_base_oid = repo
        .merge_base(base_commit.id(), head_commit.id())
        .map_err(|e| anyhow!("no common ancestor between '{base}' and the analyzed head: {e}"))?;

    debug!("merge-base: {merge_base_oid} (base '{base}')");

    repo.find_commit(merge_base_oid)
        .map_err(|e| anyhow!("failed to find merge-base commit: {e}"))?
        .tree()
        .map_err(|e| anyhow!("failed to get tree for merge-base: {e}"))
}

fn diff_options(bsl_only: bool) -> DiffOptions {
    let mut opts = DiffOptions::new();
    opts.ignore_submodules(true);
    // Zero context: hunk ranges cover exactly the changed lines, so the scope
    // does not leak diagnostics on untouched neighbouring lines.
    opts.context_lines(0);
    if bsl_only {
        // fnmatch character classes cover every case variant of the extension
        // (`.bsl` / `.BSL` / mixed) on case-sensitive filesystems.
        opts.pathspec("*.[bB][sS][lL]");
    }
    opts
}

/// Whole-file statuses: every line of the new side counts as changed.
/// `Deleted` files are skipped entirely; every other status collects hunks —
/// a delta without textual hunks (e.g. a mode-only change) then stays out of
/// scope, consistently with how [`FileChange`] empty hunks are interpreted.
fn is_whole_file_status(status: Delta) -> bool {
    matches!(status, Delta::Added | Delta::Untracked)
}

fn is_bsl_path(path: &Path) -> bool {
    bsl_conventions::has_extension(path, bsl_conventions::BSL_EXTENSION)
}

fn collect_file_changes(diff: &git2::Diff<'_>) -> Result<HashMap<String, FileChange>> {
    // RefCell for interior mutability across the two foreach closures.
    let files: RefCell<HashMap<String, FileChange>> = RefCell::new(HashMap::new());
    // A silently skipped path would be a silent false negative in the scope,
    // so a non-UTF-8 path aborts the traversal instead.
    let non_utf8: RefCell<Option<PathBuf>> = RefCell::new(None);

    let walk = diff.foreach(
        &mut |delta, _progress| {
            let status = delta.status();
            if status == Delta::Deleted {
                return true;
            }

            let Some(path) = delta.new_file().path() else { return true };
            // The pathspec already narrows the walk; this guards the report
            // contents even if a caller disables it or a pattern misses.
            if !is_bsl_path(path) {
                return true;
            }
            let Some(path) = path.to_str() else {
                *non_utf8.borrow_mut() = Some(path.to_path_buf());
                return false;
            };

            let hunks = if is_whole_file_status(status) { None } else { Some(Vec::new()) };
            files.borrow_mut().insert(path.to_string(), FileChange { hunks });
            true
        },
        None,
        Some(&mut |delta, hunk| {
            let Some(path) = delta.new_file().path().and_then(Path::to_str) else { return true };

            if let Some(file_change) = files.borrow_mut().get_mut(path) {
                // Whole-file entries (hunks = None) ignore individual ranges.
                if let Some(ref mut hunks) = file_change.hunks {
                    let start = hunk.new_start();
                    let lines = hunk.new_lines();
                    // A pure deletion has no new-side lines; mark the adjacent
                    // line so edits around it stay in scope.
                    let end = if lines > 0 { start + lines - 1 } else { start.max(1) };
                    hunks.push([start.max(1), end]);
                }
            }
            true
        }),
        None,
    );

    if let Some(path) = non_utf8.into_inner() {
        return Err(anyhow!("diff contains a non-UTF-8 path: {}", path.display()));
    }
    walk.map_err(|e| anyhow!("diff traversal error: {e}"))?;

    let mut files = files.into_inner();
    for file_change in files.values_mut() {
        if let Some(ref mut hunks) = file_change.hunks {
            merge_adjacent_hunks(hunks);
        }
    }

    debug!("found {} changed files", files.len());
    Ok(files)
}

/// Merges adjacent or overlapping `[start, end]` ranges in place.
fn merge_adjacent_hunks(hunks: &mut Vec<[u32; 2]>) {
    if hunks.len() <= 1 {
        return;
    }

    hunks.sort_by_key(|h| h[0]);

    let mut merged: Vec<[u32; 2]> = Vec::with_capacity(hunks.len());
    for hunk in hunks.drain(..) {
        if let Some(last) = merged.last_mut() {
            if hunk[0] <= last[1] + 1 {
                last[1] = last[1].max(hunk[1]);
                continue;
            }
        }
        merged.push(hunk);
    }

    *hunks = merged;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;
    #[cfg(unix)]
    use std::fs;

    fn five_lines(third: &str) -> String {
        format!("Перем А;\nПерем Б;\n{third}\nПерем Г;\nПерем Д;\n")
    }

    #[test]
    fn tree_to_tree_reports_exact_changed_lines() {
        let t = TestRepo::new();
        t.write("src/cf/CommonModules/M/Ext/Module.bsl", &five_lines("Перем В;"));
        t.commit("vendor");
        t.branch("vendor");

        t.write("src/cf/CommonModules/M/Ext/Module.bsl", &five_lines("Перем В_Наша;"));
        t.commit("change line 3");

        let diff = generate_diff_report(t.root(), "vendor", "HEAD", true).unwrap();
        let change = &diff.report.files["src/cf/CommonModules/M/Ext/Module.bsl"];
        assert_eq!(change.hunks.as_deref(), Some(&[[3, 3]][..]));
        assert_eq!(diff.report.files.len(), 1);
        assert_eq!(diff.workdir, t.root().canonicalize().unwrap());
    }

    #[test]
    fn diff_reports_anchor_native_git_paths_to_the_canonical_workdir() {
        let t = TestRepo::new();
        t.write("src/Own.bsl", &five_lines("Перем В;"));
        t.commit("vendor");
        t.branch("vendor");
        t.write("src/Own.bsl", &five_lines("Перем Наша;"));
        t.commit("change line 3");

        let expected_root = t.root().canonicalize().unwrap();
        let expected_file = t.root().join("src/Own.bsl").canonicalize().unwrap();
        for diff in [
            generate_diff_report(t.root(), "vendor", "HEAD", true).unwrap(),
            generate_workdir_diff_report(t.root(), "vendor", true).unwrap(),
        ] {
            assert_eq!(diff.workdir, expected_root);
            assert_eq!(diff.report.files.len(), 1);
            let change = &diff.report.files["src/Own.bsl"];
            assert_eq!(change.hunks.as_deref(), Some(&[[3, 3]][..]));
            assert_eq!(diff.workdir.join("src/Own.bsl"), expected_file);
        }
    }

    #[test]
    fn workdir_sees_staged_and_unstaged_edits_together() {
        let t = TestRepo::new();
        t.write("Module.bsl", &five_lines("Перем В;"));
        t.commit("vendor");
        t.branch("vendor");

        // Staged edit on line 1, unstaged edit on line 5.
        t.write("Module.bsl", "Перем А_Стейдж;\nПерем Б;\nПерем В;\nПерем Г;\nПерем Д;\n");
        t.stage_all();
        t.write("Module.bsl", "Перем А_Стейдж;\nПерем Б;\nПерем В;\nПерем Г;\nПерем Д_Диск;\n");

        let diff = generate_workdir_diff_report(t.root(), "vendor", true).unwrap();
        let change = &diff.report.files["Module.bsl"];
        assert_eq!(change.hunks.as_deref(), Some(&[[1, 1], [5, 5]][..]));
    }

    #[test]
    fn workdir_untracked_file_is_whole_file_including_uppercase_extension() {
        let t = TestRepo::new();
        t.write("Module.bsl", "Перем А;\n");
        t.commit("vendor");
        t.branch("vendor");

        t.write("New/Nested/Модуль.BSL", "Перем Б;\n");

        let diff = generate_workdir_diff_report(t.root(), "vendor", true).unwrap();
        let change = &diff.report.files["New/Nested/Модуль.BSL"];
        assert!(change.hunks.is_none(), "untracked file must count as whole-file");
    }

    #[test]
    fn merge_base_ignores_later_vendor_only_changes() {
        let t = TestRepo::new();
        t.write("Module.bsl", &five_lines("Перем В;"));
        t.commit("common ancestor");
        t.branch("feature");

        // vendor advances after the branch point: line 1 changes there.
        t.write("Module.bsl", "Перем А_Вендор;\nПерем Б;\nПерем В;\nПерем Г;\nПерем Д;\n");
        t.commit("vendor-only change");
        t.branch("vendor");

        // feature changes line 5 only.
        t.checkout("feature");
        t.write("Module.bsl", "Перем А;\nПерем Б;\nПерем В;\nПерем Г;\nПерем Д_Фича;\n");
        t.commit("feature change");

        let diff = generate_diff_report(t.root(), "vendor", "HEAD", true).unwrap();
        let change = &diff.report.files["Module.bsl"];
        assert_eq!(
            change.hunks.as_deref(),
            Some(&[[5, 5]][..]),
            "vendor-only line 1 change must not leak into the feature scope"
        );
    }

    #[test]
    fn deleted_files_are_skipped() {
        let t = TestRepo::new();
        t.write("Kept.bsl", "Перем А;\n");
        t.write("Gone.bsl", "Перем Б;\n");
        t.commit("vendor");
        t.branch("vendor");

        t.remove("Gone.bsl");
        t.commit("delete one");

        let diff = generate_diff_report(t.root(), "vendor", "HEAD", true).unwrap();
        assert!(diff.report.files.is_empty(), "deletion must not appear: {:?}", diff.report.files);
    }

    #[test]
    fn pure_deletion_inside_a_file_marks_the_adjacent_line() {
        let t = TestRepo::new();
        t.write("Module.bsl", &five_lines("Перем В;"));
        t.commit("vendor");
        t.branch("vendor");

        // Remove line 3 entirely: no new-side lines in the hunk.
        t.write("Module.bsl", "Перем А;\nПерем Б;\nПерем Г;\nПерем Д;\n");
        t.commit("delete line 3");

        let diff = generate_diff_report(t.root(), "vendor", "HEAD", true).unwrap();
        let change = &diff.report.files["Module.bsl"];
        // `@@ -3,1 +2,0 @@`: the deletion anchors on the preceding new-side line.
        assert_eq!(change.hunks.as_deref(), Some(&[[2, 2]][..]));
    }

    #[test]
    fn deleting_the_first_line_clamps_the_anchor_to_line_one() {
        let t = TestRepo::new();
        t.write("Module.bsl", &five_lines("Перем В;"));
        t.commit("vendor");
        t.branch("vendor");

        // `@@ -1,1 +0,0 @@`: new_start is 0, the anchor must clamp to line 1.
        t.write("Module.bsl", "Перем Б;\nПерем В;\nПерем Г;\nПерем Д;\n");
        t.commit("delete line 1");

        let diff = generate_diff_report(t.root(), "vendor", "HEAD", true).unwrap();
        let change = &diff.report.files["Module.bsl"];
        assert_eq!(change.hunks.as_deref(), Some(&[[1, 1]][..]));
    }

    #[cfg(unix)]
    #[test]
    fn mode_only_change_is_listed_without_hunks() {
        use std::os::unix::fs::PermissionsExt;

        let t = TestRepo::new();
        t.write("Module.bsl", "Перем А;\n");
        t.commit("vendor");
        t.branch("vendor");

        let path = t.root().join("Module.bsl");
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();

        let diff = generate_workdir_diff_report(t.root(), "vendor", true).unwrap();
        let change = &diff.report.files["Module.bsl"];
        assert_eq!(
            change.hunks.as_deref(),
            Some(&[][..]),
            "a delta with no textual hunks must stay listed but out of scope"
        );
    }

    #[test]
    fn non_bsl_files_are_excluded_when_bsl_only() {
        let t = TestRepo::new();
        t.write("Module.bsl", "Перем А;\n");
        t.write("Configuration.xml", "<a/>\n");
        t.commit("vendor");
        t.branch("vendor");

        t.write("Module.bsl", "Перем А_2;\n");
        t.write("Configuration.xml", "<b/>\n");
        t.commit("change both");

        let bsl = generate_diff_report(t.root(), "vendor", "HEAD", true).unwrap();
        assert!(bsl.report.files.contains_key("Module.bsl"));
        assert!(!bsl.report.files.contains_key("Configuration.xml"));
    }

    #[test]
    fn missing_repo_and_missing_ref_yield_descriptive_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = generate_diff_report(dir.path(), "vendor", "HEAD", true).unwrap_err();
        assert!(err.to_string().contains("git repository not found"), "{err}");

        let t = TestRepo::new();
        t.write("Module.bsl", "Перем А;\n");
        t.commit("initial");
        let err = generate_workdir_diff_report(t.root(), "no-such-branch", true).unwrap_err();
        assert!(err.to_string().contains("ref 'no-such-branch' not found"), "{err}");
    }

    #[test]
    fn report_json_shape_matches_rtools_diff_report() {
        let mut files = HashMap::new();
        files.insert("Module.bsl".to_string(), FileChange { hunks: Some(vec![[10, 24]]) });
        files.insert("New.bsl".to_string(), FileChange { hunks: None });
        let report = DiffReport { base_ref: "vendor".into(), head_ref: "HEAD".into(), files };

        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["base_ref"], "vendor");
        assert_eq!(json["files"]["Module.bsl"]["hunks"][0][0], 10);
        assert!(json["files"]["New.bsl"]["hunks"].is_null());
    }

    #[test]
    fn merge_adjacent_hunks_merges_adjacent_overlapping_and_unsorted() {
        let mut hunks: Vec<[u32; 2]> = vec![];
        merge_adjacent_hunks(&mut hunks);
        assert!(hunks.is_empty());

        let mut hunks = vec![[1, 5]];
        merge_adjacent_hunks(&mut hunks);
        assert_eq!(hunks, vec![[1, 5]]);

        let mut hunks = vec![[1, 5], [6, 10], [15, 20]];
        merge_adjacent_hunks(&mut hunks);
        assert_eq!(hunks, vec![[1, 10], [15, 20]]);

        let mut hunks = vec![[1, 10], [5, 15], [20, 25]];
        merge_adjacent_hunks(&mut hunks);
        assert_eq!(hunks, vec![[1, 15], [20, 25]]);

        let mut hunks = vec![[15, 20], [1, 5], [6, 10]];
        merge_adjacent_hunks(&mut hunks);
        assert_eq!(hunks, vec![[1, 10], [15, 20]]);
    }
}
