//! Disk probes for constructed conventional paths.
//!
//! Two policies, chosen by what the answer is for.
//!
//! A WALKER — anything whose paths are compared with another [`DirTree`]
//! source's, such as the discovery the resident and the graph both run — names
//! a child the way the tree STORES it: [`find_stored_child_ci_in`],
//! [`resolve_stored_chain_ci_in`] and [`find_listed_child_stem_exact`] read the
//! parent's listing first. On a case-insensitive volume an exact probe hits for
//! any spelling, so a probe-first answer would carry the constructed spelling
//! while a byte-exact source, reading the same files, carries the stored one:
//! one file under two names. Listing costs one `read_dir` per probe, which the
//! walkers pay to agree.
//!
//! The [`RealFs`] wrappers — [`find_child_ci`], [`find_child_stem_exact`],
//! [`resolve_chain_ci`] — serve callers that only read the file or ask whether
//! it is there, some of them inside very large collection directories. They
//! probe the canonical spelling first and list the parent only on a miss, so a
//! configurator-written tree pays nothing extra. On a case-insensitive volume
//! the exact probe hits whatever the spelling and the constructed path comes
//! back; on a case-sensitive one a wrong-case construction misses and the
//! listing supplies the real spelling.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::tree::{DirTree, RealFs};

/// Find a child whose name is a WHOLLY conventional spelling (`Ext`,
/// `Module.bsl`, `Configuration.xml`): the entire name matches
/// case-insensitively. Never use this for a name built from an object name —
/// that is [`find_child_stem_exact`]'s contract.
pub fn find_child_ci(dir: &Path, conventional: &str) -> Option<PathBuf> {
    find_child_ci_in(&RealFs, dir, conventional)
}

/// [`find_child_ci`] against a given tree.
fn find_child_ci_in(tree: &dyn DirTree, dir: &Path, conventional: &str) -> Option<PathBuf> {
    let exact = dir.join(conventional);
    if tree.kind_of(&exact).is_some() {
        return Some(exact);
    }
    let names = tree.child_names(dir);
    names.iter().find(|name| is_spelling_of(name, conventional)).map(|name| dir.join(name))
}

/// A walker's [`find_child_ci`]: the child as the tree stores it. The listing
/// decides — a byte-exact name first, else the first case variant — and only a
/// name the listing does not show is probed, so a directory that can be passed
/// through but not read still answers as it did.
pub fn find_stored_child_ci_in(
    tree: &dyn DirTree,
    dir: &Path,
    conventional: &str,
) -> Option<PathBuf> {
    let names = tree.child_names(dir);
    let listed = names
        .iter()
        .find(|name| name.as_os_str() == conventional)
        .or_else(|| names.iter().find(|name| is_spelling_of(name, conventional)));
    if let Some(name) = listed {
        return Some(dir.join(name));
    }
    let exact = dir.join(conventional);
    tree.kind_of(&exact).map(|_| exact)
}

fn is_spelling_of(name: &OsStr, conventional: &str) -> bool {
    name.to_str().is_some_and(|n| n.eq_ignore_ascii_case(conventional))
}

/// Find a child built as `{stem}.{ext}` where `stem` is an OBJECT name: the
/// stem must match EXACTLY (an object's case is significant), only the
/// extension is compared case-insensitively. `Alpha.xml` therefore never
/// matches a neighbour's `alpha.xml`.
pub fn find_child_stem_exact(dir: &Path, stem: &str, ext: &str) -> Option<PathBuf> {
    find_child_stem_exact_in(&RealFs, dir, stem, ext)
}

/// [`find_child_stem_exact`] against a given tree.
pub fn find_child_stem_exact_in(
    tree: &dyn DirTree,
    dir: &Path,
    stem: &str,
    ext: &str,
) -> Option<PathBuf> {
    let exact = dir.join(format!("{stem}.{ext}"));
    if tree.kind_of(&exact).is_some() {
        return Some(exact);
    }
    let names = tree.child_names(dir);
    find_listed_child_stem_exact(dir, names.iter().map(|name| name.as_os_str()), stem, ext)
}

/// [`find_child_stem_exact`] answered from a listing of `dir` the caller
/// already holds: `{stem}.{ext}` byte-exactly first, else the first name whose
/// stem matches exactly and whose extension matches case-insensitively.
pub fn find_listed_child_stem_exact<'a>(
    dir: &Path,
    names: impl IntoIterator<Item = &'a OsStr>,
    stem: &str,
    ext: &str,
) -> Option<PathBuf> {
    let names: Vec<&OsStr> = names.into_iter().collect();
    let exact = format!("{stem}.{ext}");
    let listed = names.iter().find(|name| **name == OsStr::new(&exact)).or_else(|| {
        names.iter().find(|name| {
            let path = Path::new(name);
            path.file_stem().is_some_and(|s| s == OsStr::new(stem))
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        })
    })?;
    Some(dir.join(listed))
}

/// Resolve a chain of wholly conventional components (`["Ext", "Module.bsl"]`)
/// under `dir`, each level by [`find_child_ci`]. A single-level listing cannot
/// survive `EXT/MODULE.BSL` — when the joined probe misses, every component
/// may be misspelled, so each is resolved in turn.
pub fn resolve_chain_ci(dir: &Path, components: &[&str]) -> Option<PathBuf> {
    resolve_chain_ci_in(&RealFs, dir, components)
}

/// [`resolve_chain_ci`] against a given tree.
fn resolve_chain_ci_in(tree: &dyn DirTree, dir: &Path, components: &[&str]) -> Option<PathBuf> {
    let mut current = dir.to_path_buf();
    for component in components {
        current = find_child_ci_in(tree, &current, component)?;
    }
    Some(current)
}

/// A walker's [`resolve_chain_ci`]: each link named as the tree stores it,
/// by [`find_stored_child_ci_in`].
pub fn resolve_stored_chain_ci_in(
    tree: &dyn DirTree,
    dir: &Path,
    components: &[&str],
) -> Option<PathBuf> {
    let mut current = dir.to_path_buf();
    for component in components {
        current = find_stored_child_ci_in(tree, &current, component)?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, fs};

    struct ObservedFs {
        typed_listings: Cell<usize>,
    }

    impl DirTree for ObservedFs {
        fn child_names(&self, dir: &Path) -> Vec<std::ffi::OsString> {
            RealFs.child_names(dir)
        }

        fn kind_of(&self, path: &Path) -> Option<crate::EntryKind> {
            RealFs.kind_of(path)
        }

        fn entries(&self, dir: &Path) -> Vec<crate::TreeEntry> {
            self.typed_listings.set(self.typed_listings.get() + 1);
            RealFs.entries(dir)
        }
    }

    #[test]
    fn a_missing_name_does_not_request_sibling_kinds() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Other.xml"));
        fs::create_dir(dir.path().join("CommonModule")).unwrap();
        let tree = ObservedFs { typed_listings: Cell::new(0) };

        assert_eq!(find_child_ci_in(&tree, dir.path(), "Configuration.xml"), None);
        assert_eq!(tree.typed_listings.get(), 0, "name lookup must not inspect sibling kinds");
    }

    #[test]
    fn a_missing_stem_does_not_request_sibling_kinds() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Other.xml"));
        let tree = ObservedFs { typed_listings: Cell::new(0) };

        assert_eq!(find_child_stem_exact_in(&tree, dir.path(), "Missing", "xml"), None);
        assert_eq!(tree.typed_listings.get(), 0, "stem lookup must not inspect sibling kinds");
    }

    #[test]
    fn snapshot_fallback_preserves_case_sensitive_names() {
        let tree = crate::PathSetTree::from_files([
            PathBuf::from("/ws/EXT/MODULE.BSL"),
            PathBuf::from("/ws/Товар.XML"),
            PathBuf::from("/ws/alpha.xml"),
        ]);
        assert_eq!(
            resolve_chain_ci_in(&tree, Path::new("/ws"), &["Ext", "Module.bsl"]),
            Some(PathBuf::from("/ws/EXT/MODULE.BSL")),
        );
        assert_eq!(
            find_child_stem_exact_in(&tree, Path::new("/ws"), "Товар", "xml"),
            Some(PathBuf::from("/ws/Товар.XML")),
        );
        assert_eq!(find_child_stem_exact_in(&tree, Path::new("/ws"), "Alpha", "xml"), None);
        assert_eq!(find_child_ci_in(&tree, Path::new("/ws"), "Configuration.xml"), None);
    }

    #[test]
    fn an_exact_name_probe_does_not_list_children() {
        struct ExactOnly;
        impl DirTree for ExactOnly {
            fn kind_of(&self, _: &Path) -> Option<crate::EntryKind> {
                Some(crate::EntryKind::File)
            }
            fn entries(&self, _: &Path) -> Vec<crate::TreeEntry> {
                panic!("exact lookup must not enumerate siblings")
            }
            fn child_names(&self, _: &Path) -> Vec<std::ffi::OsString> {
                panic!("exact lookup must not enumerate siblings")
            }
        }
        assert_eq!(
            find_child_ci_in(&ExactOnly, Path::new("/ws"), "Module.bsl"),
            Some(PathBuf::from("/ws/Module.bsl")),
        );
        assert_eq!(
            find_child_stem_exact_in(&ExactOnly, Path::new("/ws"), "Товар", "xml"),
            Some(PathBuf::from("/ws/Товар.xml")),
        );
    }

    /// The files every byte-exact input of the agreement test holds, relative to
    /// its root: a wholly misspelled chain, case twins side by side, and an
    /// object XML next to its case-variant twin and alone. `Twins` and `Товар`
    /// list the exact spelling first; `Rev` and `Обр` list the case variant
    /// first, which is the order that tells "the exact name wins" from "the
    /// first match wins".
    const BYTE_EXACT_LAYOUT: &[&str] = &[
        "Upper/EXT/MODULE.BSL",
        "Twins/Ext/Module.bsl",
        "Twins/EXT/MODULE.BSL",
        "Rev/EXT/MODULE.BSL",
        "Rev/Ext/Module.bsl",
        "Товар.xml",
        "Товар.XML",
        "Обр.XML",
        "Обр.xml",
        "Только.XML",
    ];

    /// Where a lookup hits byte-exactly anyway, reading the listing first must
    /// change no answer: the graph's path set and a case-sensitive disk keep
    /// answering as before.
    fn assert_stored_probes_agree(tree: &dyn DirTree, root: &Path) {
        for (dir, name) in [
            ("", "Upper"),
            ("", "Configuration.xml"),
            ("Upper", "Ext"),
            ("Upper/EXT", "Module.bsl"),
            ("Twins", "Ext"),
            ("Twins/EXT", "Module.bsl"),
            ("Rev", "Ext"),
            ("Rev/EXT", "Module.bsl"),
        ] {
            let dir = root.join(dir);
            assert_eq!(
                find_stored_child_ci_in(tree, &dir, name),
                find_child_ci_in(tree, &dir, name),
                "{} / {name}",
                dir.display(),
            );
        }
        for dir in ["Upper", "Twins", "Rev", "Нет"] {
            let dir = root.join(dir);
            let chain = ["Ext", "Module.bsl"];
            assert_eq!(
                resolve_stored_chain_ci_in(tree, &dir, &chain),
                resolve_chain_ci_in(tree, &dir, &chain),
                "{}",
                dir.display(),
            );
        }
        let names = tree.child_names(root);
        // `find_child_stem_exact_in` answers a miss through the listed form
        // itself, so the stem rule is held to fixed answers, not only to it.
        for (stem, expected) in [
            ("Товар", Some("Товар.xml")),
            ("Обр", Some("Обр.xml")),
            ("Только", Some("Только.XML")),
            ("товар", None),
            ("Нет", None),
        ] {
            let expected = expected.map(|file| root.join(file));
            assert_eq!(
                find_listed_child_stem_exact(
                    root,
                    names.iter().map(|n| n.as_os_str()),
                    stem,
                    "xml"
                ),
                expected,
                "{stem}.xml",
            );
            assert_eq!(find_child_stem_exact_in(tree, root, stem, "xml"), expected, "{stem}.xml");
        }
    }

    #[test]
    fn stored_probes_agree_with_exact_first_on_a_byte_exact_tree() {
        let root = Path::new("/ws");
        let set = crate::PathSetTree::from_files(BYTE_EXACT_LAYOUT.iter().map(|f| root.join(f)));
        for (dir, variant, exact) in [("Rev", "EXT", "Ext"), ("", "Обр.XML", "Обр.xml")] {
            let listed = set.child_names(&root.join(dir));
            let at = |wanted: &str| listed.iter().position(|name| name == wanted);
            assert!(
                matches!((at(variant), at(exact)), (Some(v), Some(e)) if v < e),
                "the input has to list {variant} before {exact}, or it cannot tell an \
                 exact-first probe from a first-match one",
            );
        }
        assert_stored_probes_agree(&set, root);

        let dir = tempfile::tempdir().unwrap();
        if fs_keeps_case_distinct(dir.path()) {
            for file in BYTE_EXACT_LAYOUT {
                touch(&dir.path().join(file));
            }
            assert_stored_probes_agree(&RealFs, dir.path());
        }

        /// A directory that can be passed through but not read: every lookup
        /// under it hits, its listing is empty.
        struct Traversable;
        impl DirTree for Traversable {
            fn kind_of(&self, _: &Path) -> Option<crate::EntryKind> {
                Some(crate::EntryKind::File)
            }
            fn entries(&self, _: &Path) -> Vec<crate::TreeEntry> {
                Vec::new()
            }
        }
        assert_eq!(
            find_stored_child_ci_in(&Traversable, root, "Module.bsl"),
            find_child_ci_in(&Traversable, root, "Module.bsl"),
        );
        assert_eq!(
            resolve_stored_chain_ci_in(&Traversable, root, &["Ext", "Module.bsl"]),
            resolve_chain_ci_in(&Traversable, root, &["Ext", "Module.bsl"]),
        );

        // The same probes on a case-folding volume have to DISAGREE, or the
        // inputs above could not tell the two policies apart at all.
        let folding = crate::test_support::CaseFoldingTree::from_files([
            root.join("Upper/EXT/MODULE.BSL"),
            root.join("Только.XML"),
        ]);
        let upper = root.join("Upper");
        assert_eq!(
            resolve_stored_chain_ci_in(&folding, &upper, &["Ext", "Module.bsl"]),
            Some(root.join("Upper/EXT/MODULE.BSL")),
        );
        assert_eq!(
            resolve_chain_ci_in(&folding, &upper, &["Ext", "Module.bsl"]),
            Some(root.join("Upper/Ext/Module.bsl")),
        );
        assert_eq!(find_stored_child_ci_in(&folding, &upper, "Ext"), Some(root.join("Upper/EXT")));
        assert_eq!(find_child_ci_in(&folding, &upper, "Ext"), Some(root.join("Upper/Ext")));
        let names = folding.child_names(root);
        assert_eq!(
            find_listed_child_stem_exact(
                root,
                names.iter().map(|n| n.as_os_str()),
                "Только",
                "xml"
            ),
            Some(root.join("Только.XML")),
        );
        assert_eq!(
            find_child_stem_exact_in(&folding, root, "Только", "xml"),
            Some(root.join("Только.xml")),
        );
    }

    /// A walker's answer keeps the prefix it was asked with and names a link by
    /// the link's own name; `canonicalize` would resolve both, so the input is
    /// one where that resolution visibly changes the path.
    #[cfg(unix)]
    #[test]
    fn a_stored_probe_keeps_the_callers_prefix_and_the_links_own_name() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        touch(&target.join("real.bsl"));
        fs::create_dir(target.join("Ext")).unwrap();
        symlink(target.join("real.bsl"), target.join("Ext/MODULE.BSL")).unwrap();
        let link = dir.path().join("link");
        symlink(&target, &link).unwrap();

        let ext = find_stored_child_ci_in(&RealFs, &link, "Ext").unwrap();
        assert_eq!(ext, link.join("Ext"));
        assert_ne!(fs::canonicalize(&ext).unwrap(), ext);

        let found = resolve_stored_chain_ci_in(&RealFs, &link, &["Ext", "Module.bsl"]).unwrap();
        assert_eq!(found, link.join("Ext/MODULE.BSL"));
        assert_ne!(fs::canonicalize(&found).unwrap(), found);
    }

    #[test]
    fn missing_directories_and_file_parents_have_no_children() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Other.xml");
        touch(&file);
        for parent in [dir.path().join("missing"), file] {
            assert!(RealFs.child_names(&parent).is_empty());
            assert_eq!(find_child_ci(&parent, "Module.bsl"), None);
            assert_eq!(find_child_stem_exact(&parent, "Товар", "xml"), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn name_only_listing_keeps_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        touch(&target.path().join("Module.bsl"));
        symlink(target.path(), dir.path().join("EXT")).unwrap();
        symlink(target.path().join("Module.bsl"), dir.path().join("MODULE.BSL")).unwrap();
        symlink(target.path().join("absent"), dir.path().join("Configuration.xml")).unwrap();

        let mut names = RealFs.child_names(dir.path());
        names.sort();
        let mut typed_names: Vec<_> = RealFs
            .entries(dir.path())
            .iter()
            .map(|entry| entry.path.file_name().unwrap().to_owned())
            .collect();
        typed_names.sort();
        assert_eq!(names, typed_names);

        assert_eq!(
            find_child_ci(dir.path(), "Configuration.xml"),
            Some(dir.path().join("Configuration.xml"))
        );
        assert_eq!(find_child_ci(dir.path(), "Missing.xml"), None);
        assert!(find_child_ci(dir.path(), "Module.bsl").unwrap().is_file());
        assert!(resolve_chain_ci(dir.path(), &["Ext", "Module.bsl"]).unwrap().is_file());
        assert!(RealFs
            .entries(dir.path())
            .iter()
            .find(|e| e.path.ends_with("EXT"))
            .unwrap()
            .is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_unicode_sibling_does_not_hide_a_conventional_name() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let parent = Path::new("/ws");
        let non_unicode = OsString::from_vec(vec![0xff]);
        let tree = crate::PathSetTree::from_files([
            parent.join(&non_unicode),
            parent.join("CONFIGURATION.XML"),
        ]);
        assert!(tree.child_names(parent).contains(&non_unicode));
        assert_eq!(
            find_child_ci_in(&tree, parent, "Configuration.xml"),
            Some(parent.join("CONFIGURATION.XML")),
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_unicode_sibling_does_not_hide_a_conventional_name_from_a_walker() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let parent = Path::new("/ws");
        let non_unicode = OsString::from_vec(vec![0xff]);
        let tree = crate::PathSetTree::from_files([
            parent.join(&non_unicode),
            parent.join("CONFIGURATION.XML"),
            parent.join("Товар.XML"),
        ]);
        assert!(tree.child_names(parent).contains(&non_unicode));
        assert_eq!(
            find_stored_child_ci_in(&tree, parent, "Configuration.xml"),
            Some(parent.join("CONFIGURATION.XML")),
        );
        let names = tree.child_names(parent);
        assert_eq!(
            find_listed_child_stem_exact(
                parent,
                names.iter().map(|n| n.as_os_str()),
                "Товар",
                "xml"
            ),
            Some(parent.join("Товар.XML")),
        );
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    /// Does this volume keep two names that differ only in case apart? Where it
    /// does not (APFS and NTFS in their default setup), the exact probe hits
    /// whatever the spelling and the constructed path is what comes back. The
    /// volume is asked rather than the target OS, so a case-sensitive volume
    /// mounted on macOS or Windows still gets the real-spelling assertion.
    fn fs_keeps_case_distinct(dir: &Path) -> bool {
        let probe = dir.join("bsl_case_distinction_probe");
        if fs::write(&probe, "").is_err() {
            return false;
        }
        let distinct = fs::metadata(dir.join("BSL_CASE_DISTINCTION_PROBE")).is_err();
        let _ = fs::remove_file(&probe);
        distinct
    }

    #[test]
    fn an_exact_probe_hits_without_listing() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Module.bsl"));
        let found = find_child_ci(dir.path(), "Module.bsl").unwrap();
        assert!(found.ends_with("Module.bsl"));
    }

    /// На регистронезависимой ФС точная проба попадает при любом написании и
    /// листинг не выполняется — там возвращается сконструированное написание,
    /// как и до этого хелпера (граница узла).
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn a_case_variant_is_found_by_listing_and_keeps_its_real_spelling() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Module.BSL"));
        let found = find_child_ci(dir.path(), "Module.bsl").unwrap();
        assert_eq!(found.file_name().unwrap(), "Module.BSL", "real spelling, not the probe's");
    }

    #[test]
    fn an_absent_child_is_none() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Other.bsl"));
        assert_eq!(find_child_ci(dir.path(), "Module.bsl"), None);
    }

    #[test]
    fn a_stem_probe_takes_a_case_variant_extension() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("Товар.XML"));
        let found = find_child_stem_exact(dir.path(), "Товар", "xml").unwrap();
        let expected =
            if fs_keeps_case_distinct(dir.path()) { "Товар.XML" } else { "Товар.xml" };
        assert_eq!(found.file_name().unwrap(), expected);
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn a_stem_probe_never_takes_a_case_variant_stem() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("alpha.xml"));
        assert_eq!(
            find_child_stem_exact(dir.path(), "Alpha", "xml"),
            None,
            "an object name's case is significant: alpha.xml is another object"
        );
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn a_chain_resolves_each_component_by_its_own_listing() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("EXT/MODULE.BSL"));
        let found = resolve_chain_ci(dir.path(), &["Ext", "Module.bsl"]).unwrap();
        assert!(found.ends_with("EXT/MODULE.BSL"), "{}", found.display());
    }

    #[test]
    fn a_chain_with_a_missing_link_is_none() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("EXT")).unwrap();
        assert_eq!(resolve_chain_ci(dir.path(), &["Ext", "Module.bsl"]), None);
    }
}
