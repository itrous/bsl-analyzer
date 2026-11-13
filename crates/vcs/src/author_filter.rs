//! git2-backed author attribution for the `ignored_authors` diagnostic filter.
//!
//! A [`AuthorFilter`] answers, per file line, "was this line last touched by
//! one of the ignored authors?". Blame is pinned to the repository HEAD
//! captured at construction time ([`git2::BlameOptions::newest_commit`]), so
//! one filter instance always attributes lines against a single immutable
//! history state; a moved HEAD is handled by building a new filter, never by
//! mutating an existing one. On top of the pinned blame, the current file
//! contents are overlaid via [`git2::Blame::blame_buffer`]: lines that differ
//! from the blamed revision come back with a zero commit id and count as the
//! user's own work.
//!
//! The filter is deliberately fail-open: any line whose author cannot be
//! attributed (uncommitted edits, untracked files, files outside the
//! repository, invalid UTF-8 in a signature) is kept, so misconfiguration can
//! hide nothing but noise, never a real finding on the user's own code.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use git2::Repository;
use tracing::debug;

thread_local! {
    // `git2::Repository` is `!Sync`, so parallel callers each keep their own
    // handle, reused across files and keyed by workdir in case one process
    // filters several repositories (tests, long-lived servers).
    static THREAD_REPO: RefCell<Option<(PathBuf, Repository)>> = const { RefCell::new(None) };
}

/// Why a filter could not be constructed. Surfaces map this to their failure
/// policy: the CLI turns everything into a hard error, LSP/MCP fail open.
#[derive(Debug)]
pub enum AuthorFilterError {
    /// No git repository above the analysis root.
    NoRepository(String),
    /// The repository has no working directory (bare).
    BareRepository,
    /// Shallow clone: lines older than the shallow boundary would be
    /// attributed to the boundary commit, which can silently suppress real
    /// findings. The caller must deepen the history first.
    ShallowRepository,
    /// The repository has no resolvable HEAD commit.
    UnbornHead(String),
}

impl std::fmt::Display for AuthorFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRepository(e) => write!(f, "git repository not found: {e}"),
            Self::BareRepository => write!(f, "bare repository has no working directory"),
            Self::ShallowRepository => write!(
                f,
                "shallow git clone: blame cannot attribute lines beyond the shallow \
                 boundary, so the author filter would suppress findings unreliably; \
                 run `git fetch --unshallow` first"
            ),
            Self::UnbornHead(e) => write!(f, "repository HEAD is not a commit: {e}"),
        }
    }
}

impl std::error::Error for AuthorFilterError {}

/// Per-line keep/drop verdicts for one file, 1-based like blame line numbers.
#[derive(Debug)]
pub struct LineKeep {
    /// `kept[line - 1]` — false only when the line is attributed to an
    /// ignored author. Lines outside the vector are treated as kept.
    kept: Vec<bool>,
    /// Number of `false` entries, for cheap "is there anything to drop" checks.
    ignored_lines: usize,
    /// The buffer differs from the blamed revision — edits (zero-oid hunks)
    /// or pure deletions (fewer buffer lines than committed lines).
    buffer_modified: bool,
}

impl LineKeep {
    fn all_kept(lines: usize) -> Self {
        Self { kept: vec![true; lines], ignored_lines: 0, buffer_modified: true }
    }

    pub fn ignored_line_count(&self) -> usize {
        self.ignored_lines
    }

    /// True when any line of the inclusive 1-based range is kept.
    ///
    /// The verdict map and the diagnostic ranges derive from the same text,
    /// so a line beyond the map can only be an EOF-anchored artifact (e.g. a
    /// parse error positioned after the trailing newline). For a pristine
    /// buffer it clamps to the last real line — an EOF parse error in an
    /// untouched vendor file is the vendor's. Once the buffer differs from
    /// the blamed revision in any way (including pure deletions, which leave
    /// no zero-oid line behind), EOF artifacts fail open: the parse state at
    /// EOF may be the user's doing.
    pub fn range_survives(&self, start_line: u32, end_line: u32) -> bool {
        if self.ignored_lines == 0 {
            return true;
        }
        let len = self.kept.len();
        if len == 0 {
            return true;
        }
        let start = start_line.max(1) as usize;
        let end = end_line.max(start_line).max(1) as usize;
        if end > len && self.buffer_modified {
            return true;
        }
        let start = start.min(len);
        let end = end.min(len);
        (start..=end).any(|line| self.kept[line - 1])
    }
}

struct CachedKeep {
    fingerprint: u64,
    keep: Arc<LineKeep>,
}

/// Line-author filter pinned to one repository HEAD state.
pub struct AuthorFilter {
    workdir: PathBuf,
    ignored: Vec<String>,
    head_oid: git2::Oid,
    /// Fingerprint of `.mailmap` at construction: attribution depends on it,
    /// so a mailmap edit makes the filter (and its cache) stale exactly like
    /// a moved HEAD.
    mailmap_fp: u64,
    cache: Mutex<HashMap<PathBuf, CachedKeep>>,
}

impl std::fmt::Debug for AuthorFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorFilter")
            .field("workdir", &self.workdir)
            .field("ignored", &self.ignored)
            .field("head_oid", &self.head_oid)
            .finish_non_exhaustive()
    }
}

impl AuthorFilter {
    /// Builds a filter rooted at (a parent of) `path`. Validates the
    /// repository shape once so per-file calls only see per-file conditions.
    pub fn new(path: &Path, ignored: Vec<String>) -> Result<Self, AuthorFilterError> {
        let repo = Repository::discover(path)
            .map_err(|e| AuthorFilterError::NoRepository(e.to_string()))?;
        if repo.is_shallow() {
            return Err(AuthorFilterError::ShallowRepository);
        }
        let workdir =
            repo.workdir().map(Path::to_path_buf).ok_or(AuthorFilterError::BareRepository)?;
        let head_oid = repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .map_err(|e| AuthorFilterError::UnbornHead(e.to_string()))?
            .id();

        debug!(
            "author filter: {} ignored author(s), HEAD {} at {}",
            ignored.len(),
            head_oid,
            workdir.display()
        );
        let mailmap_fp = mailmap_fingerprint_at(&workdir);
        Ok(Self { workdir, ignored, head_oid, mailmap_fp, cache: Mutex::new(HashMap::new()) })
    }

    /// The HEAD commit this filter attributes against. When the repository
    /// HEAD no longer matches, the filter is stale and must be rebuilt.
    pub fn head_identity(&self) -> String {
        self.head_oid.to_string()
    }

    /// `.mailmap` fingerprint captured at construction; compare against
    /// [`mailmap_fingerprint`] to detect attribution-relevant mailmap edits.
    pub fn mailmap_fp(&self) -> u64 {
        self.mailmap_fp
    }

    /// Short identity of every input attribution depends on (HEAD prefix +
    /// mailmap fingerprint), for folding into result ids.
    pub fn short_identity(&self) -> String {
        let head = self.head_oid.to_string();
        format!("{}+{:08x}", &head[..12.min(head.len())], self.mailmap_fp)
    }

    /// Opens a repository handle for [`Self::lines_kept`]. `git2::Repository`
    /// is `!Sync`, so each worker thread opens its own handle.
    pub fn open_repo(&self) -> Result<Repository, git2::Error> {
        Repository::open(&self.workdir)
    }

    /// [`Self::lines_kept`] over a per-thread repository handle, so callers
    /// on thread pools (rayon, tokio) need no git plumbing of their own.
    pub fn lines_kept_cached(
        &self,
        abs_path: &Path,
        contents: &[u8],
    ) -> anyhow::Result<Arc<LineKeep>> {
        THREAD_REPO.with(|slot| {
            let mut slot = slot.borrow_mut();
            let reuse = matches!(&*slot, Some((workdir, _)) if *workdir == self.workdir);
            if !reuse {
                let repo = self.open_repo().map_err(|e| {
                    anyhow::anyhow!("failed to open repository {}: {e}", self.workdir.display())
                })?;
                *slot = Some((self.workdir.clone(), repo));
            }
            let (_, repo) = slot.as_ref().expect("repository handle was just installed");
            self.lines_kept(repo, abs_path, contents)
                .map_err(|e| anyhow::anyhow!("blame failed for {}: {e}", abs_path.display()))
        })
    }

    /// Keep/drop verdicts for `abs_path` with `contents` as the current text.
    ///
    /// Untracked files, files outside the repository and gitlinks resolve to
    /// an all-kept map; real blame failures bubble up for the surface to map
    /// onto its failure policy.
    pub fn lines_kept(
        &self,
        repo: &Repository,
        abs_path: &Path,
        contents: &[u8],
    ) -> Result<Arc<LineKeep>, git2::Error> {
        let fingerprint = content_fingerprint(contents);
        if let Some(cached) = self.cache.lock().unwrap().get(abs_path) {
            if cached.fingerprint == fingerprint {
                return Ok(Arc::clone(&cached.keep));
            }
        }

        let keep = Arc::new(self.blame_lines(repo, abs_path, contents)?);
        self.cache
            .lock()
            .unwrap()
            .insert(abs_path.to_path_buf(), CachedKeep { fingerprint, keep: Arc::clone(&keep) });
        Ok(keep)
    }

    fn blame_lines(
        &self,
        repo: &Repository,
        abs_path: &Path,
        contents: &[u8],
    ) -> Result<LineKeep, git2::Error> {
        let line_count = count_lines(contents);
        let Some(rel_path) = self.workdir_relative(abs_path) else {
            // Outside the repository working directory (e.g. an extension
            // root elsewhere on disk): nothing to attribute, keep everything.
            return Ok(LineKeep::all_kept(line_count));
        };

        let mut opts = git2::BlameOptions::new();
        opts.newest_commit(self.head_oid);
        opts.first_parent(true);

        let blame = match repo.blame_file(&rel_path, Some(&mut opts)) {
            Ok(blame) => blame,
            // Untracked files and gitlinks have no blameable history; every
            // line is the user's own.
            Err(e) if e.code() == git2::ErrorCode::NotFound => {
                return Ok(LineKeep::all_kept(line_count));
            }
            Err(e) => return Err(e),
        };
        // The committed side's line count, for detecting pure deletions: a
        // deleted line leaves no zero-oid hunk in the buffer blame, only a
        // shorter buffer.
        let committed_lines: usize = blame.iter().map(|h| h.lines_in_hunk()).sum();
        let blame = blame.blame_buffer(contents)?;

        let mailmap = repo.mailmap().ok();
        // Hunks produced by `blame_buffer` carry no signatures (git2 would
        // dereference a null pointer), so authorship comes from the commit
        // object, memoized per commit — vendor files share a handful of them.
        let mut verdicts: HashMap<git2::Oid, bool> = HashMap::new();

        let mut kept = vec![true; line_count];
        let mut ignored_lines = 0usize;
        let mut saw_uncommitted = false;
        for hunk in blame.iter() {
            // Zero id: the buffer line differs from the blamed revision —
            // an uncommitted edit, always the user's own work.
            let commit_id = hunk.final_commit_id();
            if commit_id.is_zero() {
                saw_uncommitted = true;
                continue;
            }
            let ignored = match verdicts.get(&commit_id) {
                Some(&ignored) => ignored,
                None => {
                    let ignored = self.commit_author_ignored(repo, commit_id, mailmap.as_ref());
                    verdicts.insert(commit_id, ignored);
                    ignored
                }
            };
            if !ignored {
                continue;
            }
            let start = hunk.final_start_line();
            for line in start..start + hunk.lines_in_hunk() {
                if let Some(slot) = kept.get_mut(line - 1) {
                    if *slot {
                        *slot = false;
                        ignored_lines += 1;
                    }
                }
            }
        }

        let buffer_modified = saw_uncommitted || committed_lines != line_count;
        Ok(LineKeep { kept, ignored_lines, buffer_modified })
    }

    /// Whether the commit's author matches the ignored list, mailmap-resolved
    /// when a mailmap exists. An unreadable commit or a signature that is not
    /// valid UTF-8 never matches (fail-open: the line stays kept).
    fn commit_author_ignored(
        &self,
        repo: &Repository,
        commit_id: git2::Oid,
        mailmap: Option<&git2::Mailmap>,
    ) -> bool {
        let Ok(commit) = repo.find_commit(commit_id) else { return false };
        let author = match mailmap.and_then(|mm| commit.author_with_mailmap(mm).ok()) {
            Some(author) => author,
            None => commit.author(),
        };
        self.is_ignored(author.name(), author.email())
    }

    /// Exact match against the signature name or email.
    fn is_ignored(&self, name: Option<&str>, email: Option<&str>) -> bool {
        self.ignored
            .iter()
            .any(|ignored| name == Some(ignored.as_str()) || email == Some(ignored.as_str()))
    }

    fn workdir_relative(&self, abs_path: &Path) -> Option<PathBuf> {
        if let Ok(rel) = abs_path.strip_prefix(&self.workdir) {
            return Some(rel.to_path_buf());
        }
        // A caller that went through `canonicalize` holds the Windows verbatim
        // spelling (`\\?\C:\...`) while libgit2 spells its workdir plainly, and
        // `strip_prefix` compares components: the same file then looks like a file
        // outside the repository, and every line of it comes back kept. Simplifying is
        // lexical, so this costs nothing and runs before the stat below.
        let simplified = stdx::path_exclusion::simplify_verbatim(abs_path.to_path_buf());
        if let Ok(rel) = simplified.strip_prefix(&self.workdir) {
            return Some(rel.to_path_buf());
        }
        // The workdir is a realpath while callers may hold a lexical path
        // (symlinked analysis roots) — retry against the canonical file path.
        let canonical =
            stdx::path_exclusion::simplify_verbatim(std::fs::canonicalize(abs_path).ok()?);
        canonical.strip_prefix(&self.workdir).ok().map(Path::to_path_buf)
    }
}

/// The repository HEAD commit id, for cheap staleness polls against
/// [`AuthorFilter::head_identity`] without rebuilding the filter.
pub fn head_identity(repo_path: &Path) -> Result<String, git2::Error> {
    let repo = Repository::discover(repo_path)?;
    let oid = repo.head()?.peel_to_commit()?.id();
    Ok(oid.to_string())
}

/// The live `.mailmap` fingerprint, for staleness polls against
/// [`AuthorFilter::mailmap_fp`]. `None` when no repository is discoverable.
pub fn mailmap_fingerprint(repo_path: &Path) -> Option<u64> {
    let repo = Repository::discover(repo_path).ok()?;
    Some(mailmap_fingerprint_at(repo.workdir()?))
}

fn mailmap_fingerprint_at(workdir: &Path) -> u64 {
    // An absent mailmap hashes as empty contents — indistinguishable from an
    // empty file, which has the same (no-op) attribution effect.
    content_fingerprint(&std::fs::read(workdir.join(".mailmap")).unwrap_or_default())
}

fn content_fingerprint(contents: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    hasher.finish()
}

/// Number of blame lines in `contents`: a trailing newline does not open a
/// new line, any other trailing bytes do.
fn count_lines(contents: &[u8]) -> usize {
    let newlines = contents.iter().filter(|&&b| b == b'\n').count();
    if contents.last().is_some_and(|&b| b != b'\n') {
        newlines + 1
    } else {
        newlines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;

    const VENDOR: (&str, &str) = ("Фирма Тест", "vendor@example.com");
    const DEV: (&str, &str) = ("Разработчик", "dev@example.com");

    fn vendor_file() -> String {
        "Перем А;\nПерем Б;\nПерем В;\n".to_string()
    }

    fn filter(t: &TestRepo, ignored: &[&str]) -> AuthorFilter {
        AuthorFilter::new(t.root(), ignored.iter().map(|s| s.to_string()).collect()).unwrap()
    }

    fn kept_lines(
        t: &TestRepo,
        filter: &AuthorFilter,
        rel: &str,
        contents: &str,
    ) -> (Vec<bool>, usize) {
        let repo = filter.open_repo().unwrap();
        let keep = filter.lines_kept(&repo, &t.root().join(rel), contents.as_bytes()).unwrap();
        let lines = count_lines(contents.as_bytes());
        let kept = (1..=lines as u32).map(|l| keep.range_survives(l, l)).collect();
        (kept, keep.ignored_line_count())
    }

    #[test]
    fn ignored_author_lines_are_dropped_and_own_lines_kept() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");
        t.write("Module.bsl", "Перем А;\nПерем Б_Наша;\nПерем В;\n");
        t.commit_as(DEV.0, DEV.1, "own change");

        let f = filter(&t, &[VENDOR.0]);
        let (kept, ignored) =
            kept_lines(&t, &f, "Module.bsl", "Перем А;\nПерем Б_Наша;\nПерем В;\n");
        assert_eq!(kept, vec![false, true, false]);
        assert_eq!(ignored, 2);
    }

    #[test]
    fn matching_by_email_works_too() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");

        let f = filter(&t, &[VENDOR.1]);
        let (kept, _) = kept_lines(&t, &f, "Module.bsl", &vendor_file());
        assert_eq!(kept, vec![false, false, false]);
    }

    #[test]
    fn uncommitted_workdir_edit_is_always_kept() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");

        // Line 2 edited on disk, not committed: zero-oid via blame_buffer.
        let dirty = "Перем А;\nПерем Б_Диск;\nПерем В;\n";
        t.write("Module.bsl", dirty);

        let f = filter(&t, &[VENDOR.0]);
        let (kept, ignored) = kept_lines(&t, &f, "Module.bsl", dirty);
        assert_eq!(kept, vec![false, true, false]);
        assert_eq!(ignored, 2);
    }

    #[test]
    fn untracked_file_keeps_every_line() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");
        t.write("New.bsl", "Перем Новая;\n");

        let f = filter(&t, &[VENDOR.0]);
        let (kept, ignored) = kept_lines(&t, &f, "New.bsl", "Перем Новая;\n");
        assert_eq!(kept, vec![true]);
        assert_eq!(ignored, 0);
    }

    #[test]
    fn file_outside_the_repository_keeps_every_line() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");

        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("Ext.bsl");
        std::fs::write(&path, "Перем Внешняя;\n").unwrap();

        let f = filter(&t, &[VENDOR.0]);
        let repo = f.open_repo().unwrap();
        let keep = f.lines_kept(&repo, &path, "Перем Внешняя;\n".as_bytes()).unwrap();
        assert!(keep.range_survives(1, 1));
        assert_eq!(keep.ignored_line_count(), 0);
    }

    #[test]
    fn blame_is_pinned_to_the_head_captured_at_construction() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");

        let f = filter(&t, &[VENDOR.0]);

        // HEAD moves after the filter was built: the new line differs from
        // the pinned revision, so it surfaces as the user's own work.
        let advanced = "Перем А;\nПерем Б_Новая;\nПерем В;\n";
        t.write("Module.bsl", advanced);
        t.commit_as(VENDOR.0, VENDOR.1, "vendor again");

        let (kept, _) = kept_lines(&t, &f, "Module.bsl", advanced);
        assert_eq!(kept, vec![false, true, false]);
        assert_ne!(f.head_identity(), head_identity(t.root()).unwrap());
    }

    #[test]
    fn cache_is_keyed_by_content_fingerprint() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");

        let f = filter(&t, &[VENDOR.0]);
        let (kept, _) = kept_lines(&t, &f, "Module.bsl", &vendor_file());
        assert_eq!(kept, vec![false, false, false]);

        // Same path, edited contents: the cached entry must not be served.
        let dirty = "Перем А;\nПерем Б_Диск;\nПерем В;\n";
        let (kept, _) = kept_lines(&t, &f, "Module.bsl", dirty);
        assert_eq!(kept, vec![false, true, false]);
    }

    #[test]
    fn whole_file_rename_keeps_the_original_author_attribution() {
        // Characterization: libgit2 blame follows whole-file renames, so a
        // vendor file renamed by the developer stays attributed to the vendor
        // and keeps being filtered.
        let t = TestRepo::new();
        t.write("Old.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");
        t.remove("Old.bsl");
        t.write("New.bsl", &vendor_file());
        t.commit_as(DEV.0, DEV.1, "rename");

        let f = filter(&t, &[VENDOR.0]);
        let (kept, ignored) = kept_lines(&t, &f, "New.bsl", &vendor_file());
        assert_eq!(kept, vec![false, false, false]);
        assert_eq!(ignored, 3);
    }

    #[test]
    fn shallow_repository_is_rejected_at_construction() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        let oid = t.commit_as(VENDOR.0, VENDOR.1, "vendor");

        // A repository is shallow exactly when $GIT_DIR/shallow exists.
        std::fs::write(t.root().join(".git/shallow"), format!("{oid}\n")).unwrap();

        let err = AuthorFilter::new(t.root(), vec![VENDOR.0.to_string()]).unwrap_err();
        assert!(matches!(err, AuthorFilterError::ShallowRepository), "{err}");
        assert!(err.to_string().contains("--unshallow"), "{err}");
    }

    #[test]
    fn missing_repository_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = AuthorFilter::new(dir.path(), vec!["x".into()]).unwrap_err();
        assert!(matches!(err, AuthorFilterError::NoRepository(_)), "{err}");
    }

    #[test]
    fn range_survives_clamps_eof_anchored_lines_only_for_pristine_buffers() {
        let keep = LineKeep { kept: vec![true, false], ignored_lines: 1, buffer_modified: false };
        assert!(keep.range_survives(1, 2));
        assert!(!keep.range_survives(2, 2));
        // Pristine buffer: an EOF-anchored diagnostic (line past the map)
        // takes the last real line's verdict instead of surviving.
        assert!(!keep.range_survives(3, 3));
        assert!(!keep.range_survives(2, 3));
        // Degenerate input (0-based caller bug) clamps instead of panicking.
        assert!(keep.range_survives(0, 1));

        // A modified buffer fails open for EOF artifacts: the parse state at
        // EOF may be the user's uncommitted doing (e.g. a deleted token).
        let dirty = LineKeep { kept: vec![false, false], ignored_lines: 2, buffer_modified: true };
        assert!(dirty.range_survives(3, 3));
        assert!(dirty.range_survives(2, 3));
        assert!(!dirty.range_survives(1, 2));

        let all = LineKeep::all_kept(2);
        assert!(all.range_survives(1, 2));
        let empty = LineKeep { kept: vec![], ignored_lines: 1, buffer_modified: false };
        assert!(empty.range_survives(1, 1));
    }

    #[test]
    fn uncommitted_deletion_fails_open_for_eof_anchored_diagnostics() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");

        // Delete the last line on disk: no zero-oid hunk marks it, only a
        // shorter buffer. An EOF-anchored diagnostic must survive — the parse
        // break is the user's uncommitted edit, not vendor code.
        let truncated = "Перем А;\nПерем Б;\n";
        t.write("Module.bsl", truncated);

        let f = filter(&t, &[VENDOR.0]);
        let repo = f.open_repo().unwrap();
        let keep = f.lines_kept(&repo, &t.root().join("Module.bsl"), truncated.as_bytes()).unwrap();
        assert_eq!(keep.ignored_line_count(), 2, "surviving vendor lines stay attributed");
        assert!(keep.range_survives(3, 3), "EOF diagnostic past the truncated end must be kept");
        assert!(!keep.range_survives(1, 1), "in-map vendor lines are still filtered");
    }

    #[test]
    fn mailmap_resolves_aliases_and_is_part_of_the_filter_identity() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as("Псевдоним", "alias@example.com", "vendor via alias");

        // Without a mailmap the alias does not match the canonical name.
        let before = filter(&t, &[VENDOR.0]);
        let (kept, _) = kept_lines(&t, &before, "Module.bsl", &vendor_file());
        assert_eq!(kept, vec![true, true, true]);

        t.write(".mailmap", &format!("{} <{}> <alias@example.com>\n", VENDOR.0, VENDOR.1));
        let after = filter(&t, &[VENDOR.0]);
        let (kept, _) = kept_lines(&t, &after, "Module.bsl", &vendor_file());
        assert_eq!(kept, vec![false, false, false], "mailmap must canonicalise the author");

        // The mailmap participates in the filter identity, so a mailmap edit
        // is observable exactly like a moved HEAD.
        assert_ne!(before.short_identity(), after.short_identity());
        assert_ne!(Some(before.mailmap_fp()), mailmap_fingerprint(t.root()));
        assert_eq!(Some(after.mailmap_fp()), mailmap_fingerprint(t.root()));
    }

    #[test]
    fn count_lines_handles_trailing_newline_and_absence() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"a"), 1);
        assert_eq!(count_lines(b"a\n"), 1);
        assert_eq!(count_lines(b"a\nb"), 2);
        assert_eq!(count_lines(b"a\nb\n"), 2);
    }

    /// A file addressed in the spelling `canonicalize` answers with on Windows is the
    /// same file as the plainly spelled workdir, not a file outside the repository:
    /// attribution must still apply. Spelled verbatim, every line used to come back
    /// kept, and an author-filtered run dropped nothing at all.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_file_path_is_still_inside_the_workdir() {
        let t = TestRepo::new();
        t.write("Module.bsl", &vendor_file());
        t.commit_as(VENDOR.0, VENDOR.1, "vendor");

        let verbatim = std::fs::canonicalize(t.root().join("Module.bsl")).unwrap();
        assert!(
            verbatim.to_string_lossy().starts_with(r"\\?\"),
            "the fixture must exercise the verbatim spelling: {}",
            verbatim.display()
        );

        let f = filter(&t, &[VENDOR.0]);
        let repo = f.open_repo().unwrap();
        let keep = f.lines_kept(&repo, &verbatim, vendor_file().as_bytes()).unwrap();
        assert_eq!(keep.ignored_line_count(), 3, "every vendor line is attributed and dropped");
    }
}
