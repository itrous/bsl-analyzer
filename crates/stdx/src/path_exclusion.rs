//! Directories a user has taken out of the project, and the one test every walker,
//! watcher and loader asks about them.
//!
//! The list is a hard exclusion: nothing declared inside it — a source root, an
//! extension, an opened document — brings any part of it back. That is what separates
//! it from the soft holes a cache punches in a tree, where a root declared inside the
//! hole wins for itself.
//!
//! Every exclusion is held under all the spellings a path inside it can arrive under,
//! because the callers do not agree on one: a walk yields paths spelled after the root
//! it started from, a watcher reports the path it armed, and the file system resolves
//! both to a third. Matching is by whole components, never by string prefix, so
//! `.tmp2` is not inside `.tmp`.
//!
//! Besides literal directories the set may carry glob patterns, matched against a
//! path relative to one base directory (the directory of the project config). A path
//! is excluded when it, or any directory above it below the base, matches: that is
//! how files no single directory can describe — a generated listing next to every
//! template — are taken out under the same hard rule.

use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// A set of excluded directories.
///
/// Equality and hashing look only at the declared spellings: the canonical ones are
/// derived from whatever the file system says at construction time, and a directory
/// created later must not make an unchanged configuration look changed.
#[derive(Debug, Clone, Default)]
pub struct ExcludedPaths {
    entries: Vec<Excluded>,
    /// Boxed so a set without patterns stays as small as it was: the set is embedded by
    /// value in many structures, enums among them.
    patterns: Option<Box<Patterns>>,
}

/// Glob patterns and the base they are relative to.
#[derive(Debug, Clone)]
struct Patterns {
    declared_base: PathBuf,
    /// Every prefix a matched path can arrive under, each with the base-relative path
    /// it stands for: the base under its declared and resolved spellings, and each walk
    /// root re-spelled by [`ExcludedPaths::respelled_under`].
    anchors: Vec<(PathBuf, PathBuf)>,
    globs: Arc<CompiledGlobs>,
}

#[derive(Debug)]
struct CompiledGlobs {
    sources: Vec<String>,
    set: globset::GlobSet,
}

/// A pattern that cannot be used, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidPattern {
    pub pattern: String,
    pub reason: String,
}

impl std::fmt::Display for InvalidPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pattern {:?} {}", self.pattern, self.reason)
    }
}

impl std::error::Error for InvalidPattern {}

#[derive(Debug, Clone)]
struct Excluded {
    declared: PathBuf,
    spellings: Vec<PathBuf>,
}

impl ExcludedPaths {
    /// Builds the set from absolute paths.
    ///
    /// Each path is normalised lexically, so `a/../.tmp` names `.tmp` and cannot be used
    /// to slip past the rule, and resolved through the file system on a best-effort
    /// basis: a directory that does not exist yet is excluded under its declared
    /// spelling and under its nearest existing ancestor's resolved spelling with the
    /// rest appended — so an exclusion declared through a symlink still matches the
    /// resolved paths walks and loaders produce before the directory is created.
    pub fn new<I, P>(paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut entries: Vec<Excluded> = Vec::new();
        for path in paths {
            let declared = normalize_lexically(path.as_ref());
            if entries.iter().any(|entry| entry.declared == declared) {
                continue;
            }
            let mut spellings = vec![declared.clone()];
            if let Some(resolved) = resolve_as_far_as_it_goes(&declared) {
                push_unique(&mut spellings, resolved);
            }
            entries.push(Excluded { declared, spellings });
        }
        Self { entries, patterns: None }
    }

    /// Adds glob patterns matched against paths relative to the absolute `base`.
    ///
    /// The syntax is `globset`'s with `/` as the separator: `*` and `?` stay within one
    /// path component, `**` crosses any number of them. A pattern without a `/` matches
    /// a name at any depth (`*.tmp` is `**/*.tmp`). An empty pattern, an absolute one, a
    /// negation (`!x`, which a hard exclusion cannot honour) and one the glob parser
    /// rejects are refused rather than dropped: a dropped exclusion silently puts back
    /// the tree it was meant to hide. On Windows matching ignores case, like the rest of
    /// this set. An empty list leaves the set exactly as it was.
    pub fn with_patterns<I, S>(mut self, base: &Path, patterns: I) -> Result<Self, InvalidPattern>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut sources = Vec::new();
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in patterns {
            let pattern = pattern.as_ref();
            let invalid = |reason: String| InvalidPattern { pattern: pattern.to_owned(), reason };
            let trimmed = pattern.trim();
            if trimmed.is_empty() {
                return Err(invalid("must not be empty".to_owned()));
            }
            if trimmed.starts_with('!') {
                return Err(invalid(
                    "is a negation; negated patterns are not supported".to_owned(),
                ));
            }
            let normalized = trimmed.replace('\\', "/");
            let path = Path::new(&normalized);
            // A drive-relative Windows value (`C:tmp`) is neither absolute nor rooted.
            let prefixed = path.components().any(|c| matches!(c, Component::Prefix(_)));
            if normalized.starts_with('/') || path.is_absolute() || prefixed {
                return Err(invalid("must be relative to the config file".to_owned()));
            }
            let normalized = normalized.trim_end_matches('/').trim_start_matches("./");
            let anchored = if normalized.contains('/') {
                normalized.to_owned()
            } else {
                format!("**/{normalized}")
            };
            let glob = globset::GlobBuilder::new(&anchored)
                .literal_separator(true)
                .case_insensitive(cfg!(windows))
                .backslash_escape(false)
                .build()
                .map_err(|error| invalid(format!("is not a valid glob: {}", error.kind())))?;
            builder.add(glob);
            sources.push(pattern.to_owned());
        }
        if sources.is_empty() {
            return Ok(self);
        }
        let set = builder.build().map_err(|error| InvalidPattern {
            pattern: sources.join(", "),
            reason: format!("cannot be compiled: {}", error.kind()),
        })?;
        let declared_base = normalize_lexically(base);
        let mut anchors = vec![(declared_base.clone(), PathBuf::new())];
        if let Some(resolved) = resolve_as_far_as_it_goes(&declared_base) {
            push_anchor(&mut anchors, resolved, PathBuf::new());
        }
        let globs = Arc::new(CompiledGlobs { sources, set });
        self.patterns = Some(Box::new(Patterns { declared_base, anchors, globs }));
        Ok(self)
    }

    /// Whether nothing is excluded: no directory and no pattern.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.patterns.is_none()
    }

    /// The patterns as they were declared.
    pub fn patterns(&self) -> impl Iterator<Item = &str> + '_ {
        self.patterns.iter().flat_map(|patterns| patterns.globs.sources.iter().map(String::as_str))
    }

    /// Whether a file a walk reached, inside directories it did not prune, is taken out
    /// by a pattern. Directories are pruned by [`Self::prunes_walked_dir`], so only the
    /// patterns are asked here; without patterns this costs nothing.
    pub fn excludes_walked_file(&self, path: &Path) -> bool {
        self.patterns.as_ref().is_some_and(|patterns| patterns.matches_any_spelling(path))
    }

    /// The excluded directories as they were declared (after lexical normalisation).
    pub fn declared(&self) -> impl Iterator<Item = &Path> + '_ {
        self.entries.iter().map(|entry| entry.declared.as_path())
    }

    /// A copy that also recognises each exclusion under the spelling a walk started at
    /// one of `roots` produces.
    ///
    /// A root reached through a symlink or a `..` yields entries that match neither the
    /// declared nor the resolved spelling of an exclusion inside it. Re-spelling the
    /// exclusion once per root costs one resolution per root; resolving every walked
    /// entry instead would cost one per file.
    pub fn respelled_under(&self, roots: &[PathBuf]) -> Self {
        let roots: Vec<(PathBuf, PathBuf)> = roots
            .iter()
            .map(|root| {
                let canonical = root
                    .canonicalize()
                    .map(simplify_verbatim)
                    .unwrap_or_else(|_| normalize_lexically(root));
                (root.clone(), canonical)
            })
            .collect();
        let entries = self
            .entries
            .iter()
            .map(|entry| {
                let mut spellings = entry.spellings.clone();
                for (declared_root, canonical_root) in &roots {
                    for spelling in &entry.spellings {
                        if let Ok(relative) = spelling.strip_prefix(canonical_root) {
                            push_unique(&mut spellings, declared_root.join(relative));
                        }
                    }
                }
                Excluded { declared: entry.declared.clone(), spellings }
            })
            .collect();
        let patterns =
            self.patterns.as_ref().map(|patterns| Box::new(patterns.respelled_under(&roots)));
        Self { entries, patterns }
    }

    /// Whether `path` is an excluded directory or lies anywhere beneath one.
    pub fn is_excluded(&self, path: &Path) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.any_spelling(|spelling| is_within(path, spelling)) {
            return true;
        }
        if self.excludes_walked_file(path) {
            return true;
        }
        has_dot_components(path) && {
            let normalized = normalize_lexically(path);
            self.any_spelling(|spelling| is_within(&normalized, spelling))
        }
    }

    /// [`Self::is_excluded`] that also asks under the spelling the file system resolves
    /// `path` to. For a single decision about a path that may be, or pass through, a
    /// symlink into an excluded directory — not for the per-entry test of a walk, which
    /// would pay a resolution per entry. A path that does not exist yet — a document
    /// opened before it is first saved — is resolved through its nearest existing
    /// ancestor, so a symlink alias still leads into the exclusion.
    pub fn is_excluded_resolved(&self, path: &Path) -> bool {
        self.is_excluded(path)
            || (!self.is_empty()
                && resolve_as_far_as_it_goes(path)
                    .is_some_and(|resolved| self.is_excluded(&resolved)))
    }

    /// Whether a walk must not enter the directory at `path` it has just reached, or
    /// take the symlink at `path`, whatever it points to.
    ///
    /// Asks about the walked spelling and about the resolved one, so that a symlink
    /// aliasing an excluded directory or file — or one of their ancestors — does not
    /// lead the walk back in under a name no exclusion carries. The resolved spelling is derived from
    /// the parent's, recorded in `resolved`, so only the walk's start and symlinks cost
    /// a resolution; a walk that follows links must pass every directory it enters.
    pub fn prunes_walked_dir(
        &self,
        path: &Path,
        is_symlink: bool,
        resolved: &mut ResolvedDirs,
    ) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.is_excluded(path) {
            return true;
        }
        let from_parent = || {
            let parent = resolved.0.get(path.parent()?)?;
            Some(parent.join(path.file_name()?))
        };
        let canonical = match (is_symlink, from_parent()) {
            (false, Some(canonical)) => canonical,
            _ => path.canonicalize().map(simplify_verbatim).unwrap_or_else(|_| path.to_path_buf()),
        };
        let excluded = self.is_excluded(&canonical);
        resolved.0.insert(path.to_path_buf(), canonical);
        excluded
    }

    /// [`Self::is_excluded_resolved`] for many paths in a row: the resolved spelling of
    /// each parent directory is taken once and kept in `resolved`, and only a path that
    /// is itself a symlink is resolved on its own. For bulk decisions over files that
    /// may have been reached through a symlink alias of an excluded directory.
    pub fn is_excluded_resolving(&self, path: &Path, resolved: &mut ResolvedDirs) -> bool {
        if self.is_empty() {
            return false;
        }
        if self.is_excluded(path) {
            return true;
        }
        let canonical = if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
        {
            path.canonicalize().ok().map(simplify_verbatim)
        } else {
            match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => {
                    let parent = resolved.0.entry(parent.to_path_buf()).or_insert_with(|| {
                        resolve_as_far_as_it_goes(parent).unwrap_or_else(|| parent.to_path_buf())
                    });
                    Some(parent.join(name))
                }
                _ => None,
            }
        };
        canonical.is_some_and(|canonical| self.is_excluded(&canonical))
    }

    /// Whether some excluded directory lies strictly beneath `dir`.
    ///
    /// The question a watcher asks before arming `dir` recursively: a recursive watch on
    /// such a directory would cover the exclusion too, including one that does not
    /// exist yet and is only created later. Patterns do not count: any directory may
    /// hold a match, so they are applied to each path a watch reports instead.
    pub fn has_exclusion_below(&self, dir: &Path) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let below = |dir: &Path| {
            self.any_spelling(|spelling| is_within(spelling, dir) && !same_path(spelling, dir))
        };
        below(dir) || (has_dot_components(dir) && below(&normalize_lexically(dir)))
    }

    /// [`Self::has_exclusion_below`] that also asks under the spelling the file system
    /// resolves `dir` to: a symlink to an ancestor of an exclusion has the exclusion
    /// below it just as much, though no lexical comparison can tell.
    pub fn has_exclusion_below_resolved(&self, dir: &Path) -> bool {
        self.has_exclusion_below(dir)
            || (!self.entries.is_empty()
                && resolve_as_far_as_it_goes(dir)
                    .is_some_and(|resolved| self.has_exclusion_below(&resolved)))
    }

    /// The heap this set owns: the entries and every spelling of each.
    pub fn heap_bytes(&self) -> usize {
        let patterns = self.patterns.as_ref().map_or(0, |patterns| {
            std::mem::size_of::<Patterns>()
                + patterns.declared_base.capacity()
                + crate::heap::vec_bytes::<(PathBuf, PathBuf)>(patterns.anchors.capacity())
                + patterns
                    .anchors
                    .iter()
                    .map(|(prefix, relative)| prefix.capacity() + relative.capacity())
                    .sum::<usize>()
        });
        patterns
            + crate::heap::vec_bytes::<Excluded>(self.entries.capacity())
            + self
                .entries
                .iter()
                .map(|entry| {
                    entry.declared.capacity()
                        + crate::heap::vec_bytes::<PathBuf>(entry.spellings.capacity())
                        + entry.spellings.iter().map(PathBuf::capacity).sum::<usize>()
                })
                .sum::<usize>()
    }

    fn any_spelling(&self, mut test: impl FnMut(&Path) -> bool) -> bool {
        self.entries.iter().any(|entry| entry.spellings.iter().any(|spelling| test(spelling)))
    }
}

/// The resolved spelling of each directory one walk has entered, for
/// [`ExcludedPaths::prunes_walked_dir`]. One per walk: entries are keyed by the walk's
/// own spelling of each directory.
#[derive(Debug, Default)]
pub struct ResolvedDirs(std::collections::HashMap<PathBuf, PathBuf>);

impl Patterns {
    fn respelled_under(&self, roots: &[(PathBuf, PathBuf)]) -> Self {
        let mut anchors = self.anchors.clone();
        for (declared_root, canonical_root) in roots {
            for (prefix, relative) in &self.anchors {
                // A root inside the base: a path spelled after the root stands for the
                // root's place relative to the base, under either spelling of the root.
                if let Some(below) = strip_within(canonical_root, prefix) {
                    push_anchor(&mut anchors, declared_root.clone(), relative.join(below));
                }
                if let Some(below) = strip_within(declared_root, prefix) {
                    push_anchor(&mut anchors, canonical_root.clone(), relative.join(below));
                }
                // The base inside a root: the base re-spelled after the root.
                if relative.as_os_str().is_empty() {
                    if let Some(below) = strip_within(prefix, canonical_root) {
                        push_anchor(&mut anchors, declared_root.join(below), PathBuf::new());
                    }
                }
            }
        }
        Self { declared_base: self.declared_base.clone(), anchors, globs: self.globs.clone() }
    }

    fn matches_any_spelling(&self, path: &Path) -> bool {
        if has_dot_components(path) {
            self.matches(&normalize_lexically(path))
        } else {
            self.matches(path)
        }
    }

    fn matches(&self, path: &Path) -> bool {
        self.anchors.iter().any(|(prefix, relative)| {
            strip_within(path, prefix).is_some_and(|below| self.matches_relative(relative, &below))
        })
    }

    /// Whether the base-relative path, or a directory above it, matches a pattern.
    fn matches_relative(&self, relative: &Path, below: &Path) -> bool {
        let mut candidate = String::new();
        for component in relative.components().chain(below.components()) {
            let Component::Normal(name) = component else { continue };
            if !candidate.is_empty() {
                candidate.push('/');
            }
            candidate.push_str(&name.to_string_lossy());
            if self.globs.set.is_match(&candidate) {
                return true;
            }
        }
        false
    }
}

fn push_anchor(anchors: &mut Vec<(PathBuf, PathBuf)>, prefix: PathBuf, relative: PathBuf) {
    if !anchors.iter().any(|(existing, _)| *existing == prefix) {
        anchors.push((prefix, relative));
    }
}

/// `path` with `prefix` taken off, compared by whole components the way [`is_within`]
/// compares them; `None` when `path` is not at or under `prefix`.
fn strip_within(path: &Path, prefix: &Path) -> Option<PathBuf> {
    if !is_within(path, prefix) {
        return None;
    }
    Some(path.components().skip(prefix.components().count()).collect())
}

impl PartialEq for ExcludedPaths {
    fn eq(&self, other: &Self) -> bool {
        self.entries.len() == other.entries.len()
            && self.entries.iter().zip(&other.entries).all(|(a, b)| a.declared == b.declared)
            && match (&self.patterns, &other.patterns) {
                (None, None) => true,
                (Some(a), Some(b)) => {
                    a.declared_base == b.declared_base && a.globs.sources == b.globs.sources
                }
                _ => false,
            }
    }
}

impl Eq for ExcludedPaths {}

impl Hash for ExcludedPaths {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.entries.len().hash(state);
        for entry in &self.entries {
            entry.declared.hash(state);
        }
        if let Some(patterns) = &self.patterns {
            patterns.declared_base.hash(state);
            patterns.globs.sources.hash(state);
        }
    }
}

/// Resolves `.` and `..` without touching the file system. A `..` that would climb
/// above the root of an absolute path is dropped, the way the file system treats it.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let popped =
                    matches!(out.components().next_back(), Some(Component::Normal(_))) && out.pop();
                if !popped && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` with every link resolved, in the spelling walks and events use (no Windows
/// verbatim prefix).
pub fn resolve(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize().map(simplify_verbatim)
}

/// [`resolve`] of the nearest existing ancestor of `path`, with the part that does not
/// exist yet appended; `None` when not even the root resolves.
///
/// The appended part is normalised lexically, `..` included: nothing in it exists, so
/// nothing in it can be a symlink that `..` would have to step back out of.
fn resolve_as_far_as_it_goes(path: &Path) -> Option<PathBuf> {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(resolved) = resolve(existing) {
            let mut out = resolved;
            for component in missing.iter().rev() {
                out.push(component);
            }
            return Some(normalize_lexically(&out));
        }
        missing.push(existing.components().next_back()?.as_os_str().to_os_string());
        existing = existing.parent()?;
    }
}

fn has_dot_components(path: &Path) -> bool {
    path.components().any(|component| matches!(component, Component::CurDir | Component::ParentDir))
}

fn push_unique(spellings: &mut Vec<PathBuf>, spelling: PathBuf) {
    if !spellings.contains(&spelling) {
        spellings.push(spelling);
    }
}

/// `canonicalize` on Windows answers with a verbatim `\\?\C:\...` path, which no
/// watcher event or walk entry is spelled with.
#[cfg(windows)]
fn simplify_verbatim(path: PathBuf) -> PathBuf {
    use std::path::Prefix;
    let mut components = path.components();
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) => {
                let mut out = PathBuf::from(format!("{}:\\", letter as char));
                for component in components {
                    if !matches!(component, Component::RootDir) {
                        out.push(component.as_os_str());
                    }
                }
                out
            }
            _ => path,
        },
        _ => path,
    }
}

#[cfg(not(windows))]
fn simplify_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// Whether `path` is `prefix` or lies beneath it, compared by whole components. On
/// Windows the file system ignores case, so the comparison does too.
fn is_within(path: &Path, prefix: &Path) -> bool {
    #[cfg(windows)]
    {
        let mut path = path.components();
        prefix.components().all(|expected| {
            path.next().is_some_and(|actual| {
                actual.as_os_str().to_string_lossy().to_lowercase()
                    == expected.as_os_str().to_string_lossy().to_lowercase()
            })
        })
    }
    #[cfg(not(windows))]
    {
        path.starts_with(prefix)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    is_within(left, right) && left.components().count() == right.components().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute(name: &str) -> PathBuf {
        std::path::absolute(Path::new("target").join("path-exclusion-tests").join(name)).unwrap()
    }

    #[test]
    fn exclusions_are_component_scoped_and_normalized() {
        let root = absolute("component");
        let excluded = ExcludedPaths::new([root.join("a/../.tmp")]);

        assert!(excluded.is_excluded(&root.join(".tmp")));
        assert!(excluded.is_excluded(&root.join("./.tmp/child/Module.bsl")));
        assert!(!excluded.is_excluded(&root.join(".tmp2/Module.bsl")));
        assert!(!excluded.is_excluded(&root.join("allowed/Module.bsl")));
    }

    fn with_patterns(base: &Path, patterns: &[&str]) -> ExcludedPaths {
        ExcludedPaths::default().with_patterns(base, patterns).unwrap()
    }

    #[test]
    fn patterns_match_files_and_directories_relative_to_the_base() {
        let base = absolute("patterns");
        let excluded = with_patterns(&base, &["temp", ".tool/**", "**/*.Queries.bsl", "src/gen"]);

        for hidden in [
            "temp",
            "temp/Module.bsl",
            "src/cf/temp/deep/Module.bsl",
            ".tool/Module.bsl",
            "src/cf/Reports/R/Templates/T/Ext/Template.Queries.bsl",
            "src/gen/Module.bsl",
            "./src/../temp/Module.bsl",
        ] {
            assert!(excluded.is_excluded(&base.join(hidden)), "{hidden} must be excluded");
            assert!(excluded.excludes_walked_file(&base.join(hidden)), "{hidden} (walk)");
        }
        for visible in [
            "temporary/Module.bsl",
            "src/cf/Reports/R/Ext/ObjectModule.bsl",
            "src/cf/Queries.bsl.txt",
            "other/src/gen/Module.bsl",
            ".tool2/Module.bsl",
        ] {
            assert!(!excluded.is_excluded(&base.join(visible)), "{visible} must stay");
        }
        assert!(
            !excluded.is_excluded(&absolute("elsewhere").join("temp/Module.bsl")),
            "a pattern matched a path outside its base"
        );
        assert!(!excluded.is_empty());
        assert_eq!(
            excluded.patterns().collect::<Vec<_>>(),
            ["temp", ".tool/**", "**/*.Queries.bsl", "src/gen"]
        );
    }

    #[test]
    fn a_star_does_not_cross_a_separator_in_an_anchored_pattern() {
        let base = absolute("separator");
        let excluded = with_patterns(&base, &["src/*.bsl"]);

        assert!(excluded.is_excluded(&base.join("src/Module.bsl")));
        assert!(!excluded.is_excluded(&base.join("src/nested/Module.bsl")));
    }

    #[cfg(windows)]
    #[test]
    fn patterns_ignore_case_on_windows() {
        let base = absolute("case");
        let excluded = with_patterns(&base, &["**/*.Queries.bsl"]);

        assert!(excluded.is_excluded(&base.join("Ext/TEMPLATE.QUERIES.BSL")));
    }

    #[test]
    fn patterns_follow_a_walk_root_respelled_under_another_name() {
        let base = absolute("respelled");
        let root = base.join("src/cf");
        let excluded = with_patterns(&base, &["src/cf/temp"]);
        // A root reached under another spelling: the walk yields paths under `alias`,
        // which the canonical root stands for.
        let alias = absolute("respelled-alias");
        let respelled = excluded.respelled_under(std::slice::from_ref(&root));
        assert!(respelled.is_excluded(&root.join("temp/Module.bsl")));

        let roots = [(alias.clone(), root.clone())];
        let patterns = excluded.patterns.as_ref().unwrap().respelled_under(&roots);
        assert!(patterns.matches(&alias.join("temp/Module.bsl")));
        assert!(!patterns.matches(&alias.join("kept/Module.bsl")));

        // The other direction: a root declared inside the base through a link whose
        // target lies elsewhere — files keyed by the target still match.
        let target = absolute("respelled-target");
        let roots = [(root.clone(), target.clone())];
        let patterns = excluded.patterns.as_ref().unwrap().respelled_under(&roots);
        assert!(patterns.matches(&target.join("temp/Module.bsl")));
        assert!(!patterns.matches(&target.join("kept/Module.bsl")));
    }

    #[test]
    fn unusable_patterns_are_refused_with_the_offending_value() {
        let base = absolute("invalid");
        for (pattern, reason) in [
            ("", "must not be empty"),
            ("   ", "must not be empty"),
            ("!temp", "negation"),
            ("/temp", "relative"),
            ("a[", "not a valid glob"),
        ] {
            let error = ExcludedPaths::default().with_patterns(&base, [pattern]).unwrap_err();
            assert_eq!(error.pattern, pattern);
            assert!(error.to_string().contains(reason), "{pattern:?}: {error}");
        }
        #[cfg(windows)]
        assert!(ExcludedPaths::default().with_patterns(&base, ["C:temp"]).is_err());
    }

    #[test]
    fn no_patterns_leave_the_set_unchanged() {
        let base = absolute("none");
        let plain = ExcludedPaths::new([base.join(".tmp")]);
        let with_none = plain.clone().with_patterns(&base, Vec::<String>::new()).unwrap();

        assert_eq!(plain, with_none);
        assert!(with_none.patterns().next().is_none());
        assert!(!with_none.excludes_walked_file(&base.join("temp/Module.bsl")));
        assert!(ExcludedPaths::default().with_patterns(&base, [""; 0]).unwrap().is_empty());
        assert_ne!(plain, plain.clone().with_patterns(&base, ["temp"]).unwrap());
    }

    #[test]
    fn a_future_exclusion_already_narrows_its_existing_parent() {
        let root = absolute("future");
        let future = root.join("missing/deep/.tmp");
        let excluded = ExcludedPaths::new([future.clone()]);

        assert!(excluded.has_exclusion_below(&root));
        assert!(excluded.has_exclusion_below(&root.join("missing")));
        assert!(!excluded.has_exclusion_below(&future));
        assert!(excluded.is_excluded(&future.join("Module.bsl")));
        assert!(!excluded.is_excluded(&root.join("missing/deep/.tmp2/Module.bsl")));
    }

    #[test]
    fn declared_identity_does_not_change_when_a_path_can_be_resolved() {
        let root = absolute("identity");
        let _ = std::fs::remove_dir_all(&root);
        let path = root.join(".tmp");
        let before = ExcludedPaths::new([path.clone()]);
        std::fs::create_dir_all(&path).unwrap();
        let after = ExcludedPaths::new([path.clone()]);

        assert_eq!(before, after);
        assert!(after.is_excluded_resolved(&path.join("Module.bsl")));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_alias_cannot_reenter_an_excluded_directory() {
        let root = absolute("symlink");
        let _ = std::fs::remove_dir_all(&root);
        let real = root.join("real");
        let excluded_dir = real.join(".tmp");
        std::fs::create_dir_all(&excluded_dir).unwrap();
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let excluded = ExcludedPaths::new([excluded_dir.clone()]);

        assert!(excluded.is_excluded_resolved(&alias.join(".tmp/Module.bsl")));
        assert!(excluded.is_excluded_resolved(&alias.join(".tmp/missing/../StillHidden.bsl")));
        assert!(!excluded.is_excluded_resolved(&alias.join(".tmp2/Module.bsl")));
        assert!(!excluded.is_excluded_resolved(&alias.join("allowed/missing/../StillAllowed.bsl")));

        let declared_through_alias = ExcludedPaths::new([alias.join(".tmp")]);
        assert!(declared_through_alias.is_excluded_resolved(&real.join(".tmp/Module.bsl")));
        assert!(!declared_through_alias.is_excluded_resolved(&real.join(".tmp2/Module.bsl")));

        std::fs::remove_dir_all(&root).unwrap();
    }
}
