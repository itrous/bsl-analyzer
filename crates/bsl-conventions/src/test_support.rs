//! Test doubles for [`DirTree`] behaviours a Linux test host cannot reproduce
//! on its own disk.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::tree::{DirTree, EntryKind, PathSetTree, TreeEntry};

/// A case-insensitive volume (APFS and NTFS in their default setup) over a
/// stored set of files.
///
/// A lookup succeeds for any spelling of a stored path, and a listing names
/// each child as it is STORED while keeping the prefix the caller asked with —
/// the way `read_dir` answers on NTFS. So `kind_of("/ws/catalogs")` hits a
/// stored `/ws/CATALOGS`, and `entries("/ws/Catalogs")` yields
/// `/ws/Catalogs/Товары.XML`.
pub struct CaseFoldingTree {
    stored: PathSetTree,
}

impl CaseFoldingTree {
    pub fn from_files(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self { stored: PathSetTree::from_files(paths) }
    }

    /// The stored spelling of `path`: every normal component is looked up
    /// among the stored children of the directory resolved so far, a
    /// byte-exact name first, then a case-folded one.
    fn stored_path(&self, path: &Path) -> Option<PathBuf> {
        let mut current = PathBuf::new();
        for component in path.components() {
            let Component::Normal(name) = component else {
                current.push(component);
                continue;
            };
            let children = self.stored.child_names(&current);
            let stored = children
                .iter()
                .find(|child| child.as_os_str() == name)
                .or_else(|| children.iter().find(|child| folds_equal(child, name)))?;
            current.push(stored);
        }
        Some(current)
    }
}

/// UTF-8 names compare after `to_lowercase`; any other name only byte-exactly.
fn folds_equal(a: &OsStr, b: &OsStr) -> bool {
    match (a.to_str(), b.to_str()) {
        (Some(a), Some(b)) => a.to_lowercase() == b.to_lowercase(),
        _ => a == b,
    }
}

impl DirTree for CaseFoldingTree {
    fn kind_of(&self, path: &Path) -> Option<EntryKind> {
        self.stored.kind_of(&self.stored_path(path)?)
    }

    fn entries(&self, dir: &Path) -> Vec<TreeEntry> {
        let Some(stored_dir) = self.stored_path(dir) else { return Vec::new() };
        self.stored
            .entries(&stored_dir)
            .into_iter()
            .filter_map(|entry| {
                let name = entry.path.file_name()?;
                Some(TreeEntry { path: dir.join(name), kind: entry.kind })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> CaseFoldingTree {
        CaseFoldingTree::from_files([
            PathBuf::from("/ws/CATALOGS/Товары.XML"),
            PathBuf::from("/ws/CATALOGS/Товары/EXT/MODULE.BSL"),
        ])
    }

    #[test]
    fn any_spelling_of_a_stored_path_is_found() {
        let tree = tree();
        assert_eq!(tree.kind_of(Path::new("/ws/catalogs")), Some(EntryKind::Dir));
        assert_eq!(
            tree.kind_of(Path::new("/ws/Catalogs/Товары/Ext/Module.bsl")),
            Some(EntryKind::File)
        );
        assert_eq!(tree.kind_of(Path::new("/ws/Documents")), None);
    }

    #[test]
    fn a_listing_keeps_the_asked_prefix_and_the_stored_names() {
        let mut listed: Vec<_> = tree()
            .entries(Path::new("/ws/Catalogs"))
            .into_iter()
            .map(|entry| (entry.path, entry.kind))
            .collect();
        listed.sort();
        assert_eq!(
            listed,
            vec![
                (PathBuf::from("/ws/Catalogs/Товары"), EntryKind::Dir),
                (PathBuf::from("/ws/Catalogs/Товары.XML"), EntryKind::File),
            ],
        );
    }
}
