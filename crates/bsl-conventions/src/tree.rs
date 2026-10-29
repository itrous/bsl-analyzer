//! Where a probe looks.
//!
//! The layout rules — which directory holds what, which sibling names an
//! object, where a module body hides — are written once, in the walkers that
//! use them. What varies is only where those rules read: the resident reads the
//! real filesystem, while a consumer that already scanned the tree reads its own
//! list of paths and must not walk the disk a second time (a fresh walk can see
//! a tree the scan did not).
//!
//! So the rules take a source instead of calling `fs` directly. Two operations
//! are enough, and both are needed: a cheap existence probe, because the walkers
//! construct a canonical spelling first and a listing per probe would be a real
//! cost on a large dump, and a listing, taken only when the exact probe misses.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What a path is, when it is anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    File,
    Dir,
}

/// One child of a listed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
}

impl TreeEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }

    pub fn is_file(&self) -> bool {
        self.kind == EntryKind::File
    }
}

/// A directory tree the layout rules read.
pub trait DirTree {
    /// What `path` is, or `None` when it is nothing.
    fn kind_of(&self, path: &Path) -> Option<EntryKind>;

    /// The children of `dir`; empty when `dir` cannot be listed. Order is
    /// unspecified — every caller sorts its own output.
    fn entries(&self, dir: &Path) -> Vec<TreeEntry>;

    /// Immediate child names, without requiring the type of each entry's target.
    /// Name-only probes must not turn a missing marker into a stat of every sibling.
    fn child_names(&self, dir: &Path) -> Vec<OsString> {
        self.entries(dir)
            .into_iter()
            .filter_map(|entry| entry.path.file_name().map(OsString::from))
            .collect()
    }
}

/// The real filesystem.
pub struct RealFs;

impl DirTree for RealFs {
    fn child_names(&self, dir: &Path) -> Vec<OsString> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        entries.flatten().map(|entry| entry.file_name()).collect()
    }

    fn kind_of(&self, path: &Path) -> Option<EntryKind> {
        let meta = std::fs::metadata(path).ok()?;
        Some(if meta.is_dir() { EntryKind::Dir } else { EntryKind::File })
    }

    /// The kind is asked of the TARGET, not of the link: a dump may place an
    /// object's directory behind a symlink, and `DirEntry::file_type` — which
    /// stops at the link — would call it a file and send the object down the
    /// branch that has no sidecars. Costs one stat per entry, which is what the
    /// `is_dir` probes this replaced already cost.
    fn entries(&self, dir: &Path) -> Vec<TreeEntry> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        entries
            .flatten()
            .map(|entry| {
                let path = entry.path();
                let kind = if path.is_dir() { EntryKind::Dir } else { EntryKind::File };
                TreeEntry { path, kind }
            })
            .collect()
    }
}

/// A tree reconstructed from a list of file paths — the scanned universe.
///
/// Only files are listed anywhere, so a directory exists here exactly when some
/// listed file lies under it. A directory holding nothing that was scanned is
/// therefore invisible; the layout rules are written not to depend on bare
/// directory existence for that reason.
///
/// It also differs in CASE. This source's `kind_of` matches byte-exactly, while
/// [`RealFs`] inherits the filesystem's own answer: where that filesystem folds
/// case, a lookup of a constructed spelling hits. The walkers' probes
/// ([`find_stored_child_ci_in`](crate::find_stored_child_ci_in),
/// [`resolve_stored_chain_ci_in`](crate::resolve_stored_chain_ci_in),
/// [`find_listed_child_stem_exact`](crate::find_listed_child_stem_exact)) do
/// not let that difference through: they name a child from the parent's
/// listing, so both sources name one file with its stored spelling.
pub struct PathSetTree {
    children: HashMap<PathBuf, Vec<TreeEntry>>,
}

impl PathSetTree {
    pub fn from_files(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut children: HashMap<PathBuf, Vec<TreeEntry>> = HashMap::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        for path in paths {
            let mut kind = EntryKind::File;
            let mut child = path;
            while let Some(parent) = child.parent().map(Path::to_path_buf) {
                if !seen.insert(child.clone()) {
                    break;
                }
                children.entry(parent.clone()).or_default().push(TreeEntry { path: child, kind });
                kind = EntryKind::Dir;
                child = parent;
            }
        }
        Self { children }
    }
}

impl DirTree for PathSetTree {
    fn kind_of(&self, path: &Path) -> Option<EntryKind> {
        if self.children.contains_key(path) {
            return Some(EntryKind::Dir);
        }
        let parent = path.parent()?;
        self.children.get(parent)?.iter().find(|entry| entry.path == path).map(|entry| entry.kind)
    }

    fn entries(&self, dir: &Path) -> Vec<TreeEntry> {
        self.children.get(dir).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> PathSetTree {
        PathSetTree::from_files(paths.iter().map(PathBuf::from))
    }

    #[test]
    fn a_directory_exists_when_something_scanned_lies_under_it() {
        let tree = files(&["/ws/Catalogs/Товары/Товары.xml"]);
        assert_eq!(tree.kind_of(Path::new("/ws/Catalogs/Товары")), Some(EntryKind::Dir));
        assert_eq!(
            tree.kind_of(Path::new("/ws/Catalogs/Товары/Товары.xml")),
            Some(EntryKind::File),
        );
        assert_eq!(tree.kind_of(Path::new("/ws/Documents")), None);
    }

    /// Listing a directory names its immediate children only, each with the kind
    /// that decides which branch of a layout rule it takes.
    #[test]
    fn listing_names_immediate_children_with_their_kinds() {
        let tree = files(&[
            "/ws/CommonModules/Настройки/Ext/Module.bsl",
            "/ws/CommonModules/Настройки.xml",
        ]);
        let mut listed: Vec<_> = tree
            .entries(Path::new("/ws/CommonModules"))
            .into_iter()
            .map(|e| (e.path.file_name().unwrap().to_string_lossy().to_string(), e.kind))
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            vec![
                ("Настройки".to_string(), EntryKind::Dir),
                ("Настройки.xml".to_string(), EntryKind::File),
            ],
        );
    }

    /// Two files under one directory list it once, not twice.
    #[test]
    fn a_shared_parent_is_listed_once() {
        let tree = files(&["/ws/Roles/Полные.xml", "/ws/Roles/Урезанные.xml"]);
        assert_eq!(tree.entries(Path::new("/ws")).len(), 1);
    }
}
