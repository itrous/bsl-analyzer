//! Analysis scope: which files — and which lines within them — count as "ours"
//! relative to a reference state (typically the vendor branch of a 1C
//! configuration). Diagnostics outside the scope are dropped by the final
//! diagnostics pipeline; files entirely outside it are skipped before any
//! handler runs.
//!
//! Pure data. Computing a scope from git lives in the `vcs` crate; loading one
//! from an external `diff-report.json` lives with the CLI. This module only
//! answers membership queries, and participates in the interned
//! [`crate::DiagnosticsConfigInput`] via a content fingerprint so replacing the
//! scope invalidates cached per-file diagnostics.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use rustc_hash::FxHasher;

/// A changed line range, 1-based inclusive on both ends.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hunk {
    pub start: u32,
    pub end: u32,
}

impl Hunk {
    /// `start`/`end` are 0-based (analyzer `LineIndex` coordinates), inclusive.
    pub fn overlaps(&self, start_0based: u32, end_0based: u32) -> bool {
        let start_1based = start_0based + 1;
        let end_1based = end_0based + 1;
        !(end_1based < self.start || start_1based > self.end)
    }
}

/// Per-file membership: `None` hunks = the whole file is in scope;
/// an empty hunk list = the file is listed but no line survives (out of scope).
type FileHunks = Option<Vec<Hunk>>;

#[derive(Debug)]
pub struct AnalysisScope {
    /// Human-readable reference the scope was built against (e.g. `vendor`).
    base_ref: String,
    /// Keys in the spelling [`normalize_path`] produces, which is also the spelling every
    /// lookup is normalized to; see [`AnalysisScope::build`].
    files: HashMap<PathBuf, FileHunks>,
    /// Basename → keys sharing it, for suffix matching of relative keys.
    filename_index: HashMap<String, Vec<PathBuf>>,
    /// Keys are workdir-relative (external `diff-report.json`) rather than
    /// absolute (native git modes); relative keys resolve by path suffix.
    relative_keys: bool,
    /// Content hash over every field above; backs `Hash` so the scope can sit
    /// inside an interned salsa struct without hashing the full map each time.
    fingerprint: u64,
}

enum ScopeEntry<'a> {
    OutOfScope,
    WholeFile,
    Hunks(&'a [Hunk]),
}

impl AnalysisScope {
    /// Scope from a native diff report: keys resolved to absolute paths under
    /// `workdir`, matched exactly (with a suffix fallback for defence).
    pub fn from_report(
        base_ref: impl Into<String>,
        workdir: &Path,
        files: impl IntoIterator<Item = (String, Option<Vec<[u32; 2]>>)>,
    ) -> Self {
        let files = files
            .into_iter()
            .map(|(rel, hunks)| (workdir.join(normalize_path(&rel)), convert_hunks(hunks)));
        Self::build(base_ref.into(), files, false)
    }

    /// Like [`Self::from_report`], but for a caller that addresses files
    /// through `lexical_root` (e.g. an LSP `root_uri` that may traverse a
    /// symlink) while git resolved the workdir through `realpath`: report keys
    /// under the canonical form of `lexical_root` are re-anchored onto its
    /// lexical spelling so they match the caller's file paths exactly.
    pub fn from_report_anchored(
        base_ref: impl Into<String>,
        workdir: &Path,
        lexical_root: &Path,
        files: impl IntoIterator<Item = (String, Option<Vec<[u32; 2]>>)>,
    ) -> Self {
        let root_real = lexical_root.canonicalize().unwrap_or_else(|_| lexical_root.to_path_buf());
        let files = files.into_iter().map(|(rel, hunks)| {
            let abs = workdir.join(normalize_path(&rel));
            let abs = match abs.strip_prefix(&root_real) {
                Ok(suffix) => lexical_root.join(suffix),
                Err(_) => abs,
            };
            (abs, convert_hunks(hunks))
        });
        Self::build(base_ref.into(), files, false)
    }

    /// Scope from an external relative-key report (`--diff-filter` JSON as
    /// produced by rtools): matched by path suffix.
    pub fn from_relative_report(
        base_ref: impl Into<String>,
        files: impl IntoIterator<Item = (String, Option<Vec<[u32; 2]>>)>,
    ) -> Self {
        let files =
            files.into_iter().map(|(rel, hunks)| (PathBuf::from(rel), convert_hunks(hunks)));
        Self::build(base_ref.into(), files, true)
    }

    /// Scope listing whole files only (`--changed-files`): every listed file is
    /// fully in scope, everything else is out.
    pub fn from_whole_files(
        base_ref: impl Into<String>,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        Self::build(base_ref.into(), paths.into_iter().map(|p| (p, None)), true)
    }

    fn build(
        base_ref: String,
        files: impl IntoIterator<Item = (PathBuf, FileHunks)>,
        relative_keys: bool,
    ) -> Self {
        // A key is reachable only in the spelling `find` normalizes its argument to, so the
        // normalization belongs here, where every constructor passes, and before the keys are
        // collected, so spellings of one file collapse in input order. The absolute constructors normalized the relative part alone and joined it
        // onto the workdir as given: a workdir spelled with `\`, or a canonicalized one on
        // Windows (`\\?\C:\...`, where `/` is not a separator at all), produced keys no lookup
        // could reach, and every file silently fell out of scope.
        let files: HashMap<PathBuf, FileHunks> = files
            .into_iter()
            .map(|(path, hunks)| (normalize_path(&path.to_string_lossy()), hunks))
            .collect();
        let mut filename_index: HashMap<String, Vec<PathBuf>> = HashMap::new();
        for path in files.keys() {
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                filename_index.entry(filename.to_string()).or_default().push(path.clone());
            }
        }

        let fingerprint = {
            let mut entries: Vec<(&PathBuf, &FileHunks)> = files.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut hasher = FxHasher::default();
            base_ref.hash(&mut hasher);
            relative_keys.hash(&mut hasher);
            for (path, hunks) in entries {
                path.hash(&mut hasher);
                hunks.hash(&mut hasher);
            }
            hasher.finish()
        };

        Self { base_ref, files, filename_index, relative_keys, fingerprint }
    }

    pub fn base_ref(&self) -> &str {
        &self.base_ref
    }

    /// Number of files with at least one line in scope.
    pub fn in_scope_file_count(&self) -> usize {
        self.files.values().filter(|h| h.as_ref().is_none_or(|hunks| !hunks.is_empty())).count()
    }

    /// Whether any line of `path` is in scope (file-gate).
    pub fn is_file_in_scope(&self, path: &Path) -> bool {
        match self.find(path) {
            ScopeEntry::OutOfScope => false,
            ScopeEntry::WholeFile => true,
            ScopeEntry::Hunks(hunks) => !hunks.is_empty(),
        }
    }

    /// Whether the 0-based inclusive line range overlaps the scope (line-gate).
    pub fn lines_in_scope(
        &self,
        path: &Path,
        start_line_0based: u32,
        end_line_0based: u32,
    ) -> bool {
        match self.find(path) {
            ScopeEntry::OutOfScope => false,
            ScopeEntry::WholeFile => true,
            ScopeEntry::Hunks(hunks) => {
                hunks.iter().any(|hunk| hunk.overlaps(start_line_0based, end_line_0based))
            }
        }
    }

    fn find(&self, path: &Path) -> ScopeEntry<'_> {
        let normalized = normalize_path(&path.to_string_lossy());

        if let Some(hunks) = self.files.get(&normalized) {
            return entry(hunks);
        }

        if !self.relative_keys {
            return ScopeEntry::OutOfScope;
        }

        // Relative keys: resolve by suffix via the basename index. More than
        // one candidate matching the same lookup path is ambiguous — treat the
        // file as fully in scope rather than picking an arbitrary entry.
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            return ScopeEntry::OutOfScope;
        };
        let Some(candidates) = self.filename_index.get(filename) else {
            return ScopeEntry::OutOfScope;
        };
        let normalized_str = normalized.to_string_lossy();

        let mut matched: Option<&PathBuf> = None;
        for diff_path in candidates {
            let diff_str = diff_path.to_string_lossy();
            if suffix_matches(&diff_str, &normalized_str)
                || suffix_matches(&normalized_str, &diff_str)
            {
                if matched.is_some() {
                    tracing::warn!(
                        path = %normalized.display(),
                        "ambiguous analysis-scope suffix match; treating the file as fully in scope"
                    );
                    return ScopeEntry::WholeFile;
                }
                matched = Some(diff_path);
            }
        }

        match matched.and_then(|p| self.files.get(p)) {
            Some(hunks) => entry(hunks),
            None => ScopeEntry::OutOfScope,
        }
    }
}

fn entry(hunks: &FileHunks) -> ScopeEntry<'_> {
    match hunks {
        None => ScopeEntry::WholeFile,
        Some(hunks) => ScopeEntry::Hunks(hunks),
    }
}

/// Whether `haystack` ends with `needle` on a `/` boundary (or equals it).
fn suffix_matches(haystack: &str, needle: &str) -> bool {
    if !haystack.ends_with(needle) {
        return false;
    }
    let prefix_len = haystack.len() - needle.len();
    prefix_len == 0 || haystack.as_bytes()[prefix_len - 1] == b'/'
}

fn convert_hunks(hunks: Option<Vec<[u32; 2]>>) -> FileHunks {
    hunks.map(|ranges| ranges.into_iter().map(|[start, end]| Hunk { start, end }).collect())
}

/// The spelling every scope key and every lookup is reduced to: `/` separators, no
/// leading `./`, and no Windows verbatim prefix.
///
/// The prefix matters because the two sides of a lookup are produced by different
/// code. A diff report names a file `C:\repo\src\Module.bsl`, while anything that
/// went through `canonicalize` names the same file `\\?\C:\repo\src\Module.bsl`, and
/// under a verbatim prefix `/` is not even a separator. Exact matching is all an
/// absolute-key scope has, so a lookup spelled verbatim put every file out of scope
/// and the report came back empty.
fn normalize_path(path: &str) -> PathBuf {
    let simplified = stdx::path_exclusion::simplify_verbatim(PathBuf::from(path));
    let simplified = simplified.to_string_lossy();
    let trimmed = simplified
        .strip_prefix("./")
        .or_else(|| simplified.strip_prefix(".\\"))
        .unwrap_or(&simplified);
    PathBuf::from(trimmed.replace('\\', "/"))
}

impl PartialEq for AnalysisScope {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
            && self.relative_keys == other.relative_keys
            && self.base_ref == other.base_ref
            && self.files == other.files
    }
}

impl Eq for AnalysisScope {}

impl Hash for AnalysisScope {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.fingerprint);
    }
}

/// Approximate owned heap of the scope, for salsa `heap_size` accounting of the
/// interned diagnostics config.
pub fn scope_heap_size(scope: &AnalysisScope) -> usize {
    let mut bytes = stdx::heap::map_table_bytes::<PathBuf, FileHunks>(scope.files.len())
        + stdx::heap::map_table_bytes::<String, Vec<PathBuf>>(scope.filename_index.len());
    for (path, hunks) in &scope.files {
        bytes += path.capacity();
        if let Some(hunks) = hunks {
            bytes += stdx::heap::vec_bytes::<Hunk>(hunks.len());
        }
    }
    for (name, keys) in &scope.filename_index {
        bytes += name.capacity();
        bytes += stdx::heap::vec_bytes::<PathBuf>(keys.len());
        bytes += keys.iter().map(|k| k.capacity()).sum::<usize>();
    }
    bytes += scope.base_ref.capacity();
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs_scope() -> AnalysisScope {
        AnalysisScope::from_report(
            "vendor",
            Path::new("/repo"),
            [
                ("src/Module.bsl".to_string(), Some(vec![[10, 20], [30, 40]])),
                ("src/New.bsl".to_string(), None),
                ("src/Untouched.bsl".to_string(), Some(vec![])),
            ],
        )
    }

    /// A workspace addressed through a symlink must still match: git resolves
    /// the workdir through `realpath`, the anchored constructor re-anchors the
    /// keys onto the caller's lexical root.
    #[cfg(unix)]
    #[test]
    fn anchored_report_matches_paths_through_a_symlinked_root() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Keys come from git as workdir(realpath)-relative.
        let real_canon = real.canonicalize().unwrap();
        let scope = AnalysisScope::from_report_anchored(
            "vendor",
            &real_canon,
            &link,
            [("src/Module.bsl".to_string(), None)],
        );

        assert!(
            scope.is_file_in_scope(&link.join("src/Module.bsl")),
            "the lexical (symlinked) spelling must be in scope"
        );
        assert!(!scope.is_file_in_scope(&real_canon.join("src/Module.bsl")));
    }

    #[test]
    fn absolute_keys_match_exactly_and_only_exactly() {
        let scope = abs_scope();

        assert!(scope.is_file_in_scope(Path::new("/repo/src/Module.bsl")));
        assert!(scope.is_file_in_scope(Path::new("/repo/src/New.bsl")));
        assert!(!scope.is_file_in_scope(Path::new("/repo/src/Untouched.bsl")));
        assert!(!scope.is_file_in_scope(Path::new("/repo/src/Other.bsl")));
        // No suffix fallback for absolute-key scopes: a stray same-named file
        // elsewhere must not inherit hunks.
        assert!(!scope.is_file_in_scope(Path::new("/elsewhere/src/Module.bsl")));
        assert_eq!(scope.in_scope_file_count(), 2);
    }

    #[test]
    fn line_gate_uses_zero_based_inclusive_ranges() {
        let scope = abs_scope();
        let path = Path::new("/repo/src/Module.bsl");

        assert!(scope.lines_in_scope(path, 9, 15));
        assert!(scope.lines_in_scope(path, 29, 35));
        assert!(!scope.lines_in_scope(path, 21, 28));
        assert!(!scope.lines_in_scope(path, 0, 5));
        assert!(scope.lines_in_scope(Path::new("/repo/src/New.bsl"), 0, 999));
        assert!(!scope.lines_in_scope(Path::new("/repo/src/Other.bsl"), 0, 999));
    }

    #[test]
    fn relative_keys_resolve_by_suffix() {
        let scope = AnalysisScope::from_relative_report(
            "vendor",
            [("src/cf/CommonModules/M/Ext/Module.bsl".to_string(), Some(vec![[3, 3]]))],
        );

        assert!(
            scope.is_file_in_scope(Path::new("/home/x/erp/src/cf/CommonModules/M/Ext/Module.bsl"))
        );
        assert!(scope.lines_in_scope(
            Path::new("/home/x/erp/src/cf/CommonModules/M/Ext/Module.bsl"),
            2,
            2
        ));
        // Same basename under a different MDO must not match.
        assert!(
            !scope.is_file_in_scope(Path::new("/home/x/erp/src/cf/CommonModules/N/Ext/Module.bsl"))
        );
    }

    #[test]
    fn ambiguous_suffix_match_is_conservatively_whole_file() {
        let scope = AnalysisScope::from_relative_report(
            "vendor",
            [
                ("a/Ext/Module.bsl".to_string(), Some(vec![[1, 1]])),
                ("b/a/Ext/Module.bsl".to_string(), Some(vec![[2, 2]])),
            ],
        );

        // `.../b/a/Ext/Module.bsl` suffix-matches both keys → whole file.
        assert!(scope.lines_in_scope(Path::new("/repo/b/a/Ext/Module.bsl"), 99, 99));
    }

    #[test]
    fn whole_files_scope_lists_files_entirely() {
        let scope = AnalysisScope::from_whole_files(
            "changed-files",
            [PathBuf::from("src/A.bsl"), PathBuf::from("/abs/B.bsl")],
        );

        assert!(scope.lines_in_scope(Path::new("/repo/src/A.bsl"), 500, 500));
        assert!(scope.is_file_in_scope(Path::new("/abs/B.bsl")));
        assert!(!scope.is_file_in_scope(Path::new("/repo/src/C.bsl")));
    }

    #[test]
    fn windows_and_dot_prefixed_keys_normalize() {
        let scope = AnalysisScope::from_relative_report(
            "vendor",
            [(".\\src\\Module.bsl".to_string(), None)],
        );
        assert!(scope.is_file_in_scope(Path::new("/repo/src/Module.bsl")));
    }

    /// A workdir in a spelling the lookup side normalizes away — Windows separators, or the
    /// verbatim prefix `canonicalize` returns there — must still resolve to its files.
    #[test]
    fn a_workdir_in_another_spelling_still_matches_its_files() {
        let scope = AnalysisScope::from_report(
            "vendor",
            Path::new(r"\\?\C:\repo"),
            [("src/Module.bsl".to_string(), Some(vec![[3, 4]]))],
        );

        let file = Path::new(r"\\?\C:\repo\src\Module.bsl");
        assert!(scope.is_file_in_scope(file), "the file is in scope: {scope:?}");
        assert!(scope.lines_in_scope(file, 2, 2), "line 3 (0-based 2) is in a hunk");
        assert!(!scope.lines_in_scope(file, 0, 0), "control: line 1 is not");
    }

    /// Two spellings of one file are one key, and the later entry of the report wins, the
    /// same on every build of the scope.
    #[test]
    fn spellings_of_one_file_collapse_to_the_last_entry() {
        let scope = AnalysisScope::from_relative_report(
            "vendor",
            [
                (r"src\Module.bsl".to_string(), Some(vec![[1, 1]])),
                ("./src/Module.bsl".to_string(), Some(vec![[5, 5]])),
            ],
        );

        let file = Path::new("/repo/src/Module.bsl");
        assert!(scope.lines_in_scope(file, 4, 4), "line 5 of the later entry: {scope:?}");
        assert!(!scope.lines_in_scope(file, 0, 0), "line 1 of the earlier entry is replaced");
    }

    #[test]
    fn equal_content_means_equal_fingerprint_and_hash() {
        let a = abs_scope();
        let b = abs_scope();
        let c = AnalysisScope::from_report(
            "vendor",
            Path::new("/repo"),
            [("src/Module.bsl".to_string(), Some(vec![[10, 21], [30, 40]]))],
        );

        assert_eq!(a, b);
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a, c);
        assert_ne!(a.fingerprint, c.fingerprint);
    }

    #[test]
    fn heap_size_counts_paths_and_hunks() {
        let small =
            AnalysisScope::from_report("vendor", Path::new("/repo"), [("a.bsl".to_string(), None)]);
        let large = abs_scope();

        let small_bytes = scope_heap_size(&small);
        let large_bytes = scope_heap_size(&large);
        // Both map tables are accounted for even in the minimal scope…
        assert!(
            small_bytes
                > stdx::heap::map_table_bytes::<PathBuf, Option<Vec<Hunk>>>(1)
                    + stdx::heap::map_table_bytes::<String, Vec<PathBuf>>(1)
        );
        // …and more files/hunks mean strictly more accounted heap.
        assert!(large_bytes > small_bytes, "{large_bytes} vs {small_bytes}");
    }

    /// The lookup side is the one that went through `canonicalize`: a walked file
    /// addressed through the Windows verbatim prefix must still find the key the diff
    /// report spelled plainly. Both spellings name the same file.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_lookup_finds_a_plainly_spelled_key() {
        let scope = AnalysisScope::from_report(
            "vendor",
            Path::new(r"C:\repo"),
            [("src/Module.bsl".to_string(), Some(vec![[3, 4]]))],
        );

        let file = Path::new(r"\\?\C:\repo\src\Module.bsl");
        assert!(scope.is_file_in_scope(file), "the file is in scope: {scope:?}");
        assert!(scope.lines_in_scope(file, 2, 2), "line 3 (0-based 2) is in a hunk");
        assert!(!scope.lines_in_scope(file, 0, 0), "control: line 1 is not");
    }
}
