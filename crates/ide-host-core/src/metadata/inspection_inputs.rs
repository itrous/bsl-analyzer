use std::path::{Path, PathBuf};
use std::sync::Arc;

use bsl_conventions::{ConventionalName, DirTree};
use ide::RootDatabaseImpl;

/// Binary identity is captured by the owner and transferred with source snapshots.
pub(super) fn bootstrap_fingerprints(
    db: &mut RootDatabaseImpl,
    paths: impl Iterator<Item = PathBuf>,
) {
    let mut values = paths
        .filter(|path| is_ordinary_binary(path))
        .map(|path| {
            let value = bsl_metadata::inspection_sources::binary_fingerprint(&path).ok();
            (path, value)
        })
        .collect::<Vec<_>>();
    values.sort_by(|a, b| a.0.cmp(&b.0));
    values.dedup_by(|a, b| a.0 == b.0);
    db.set_ordinary_form_fingerprints(Arc::new(values));
}

pub(super) fn is_binary(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        [ConventionalName::FormBin.canonical(), ConventionalName::TemplateBin.canonical()]
            .iter()
            .any(|candidate| name.to_string_lossy().eq_ignore_ascii_case(candidate))
    })
}

fn is_ordinary_binary(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.to_string_lossy().eq_ignore_ascii_case(ConventionalName::FormBin.canonical())
    })
}

/// Re-read only a changed/new container or one whose companion sidecar changed.
pub(super) fn refresh_fingerprints(
    db: &mut RootDatabaseImpl,
    roots: impl Iterator<Item = PathBuf>,
    tree: &dyn DirTree,
    changed: &[PathBuf],
) -> bool {
    let previous = db.ordinary_form_fingerprints();
    let mut values = previous.as_ref().clone();
    for root in roots {
        let discovered = bsl_metadata::inspection_sources::discover(&root, tree)
            .into_iter()
            .filter(|path| is_ordinary_binary(path))
            .collect::<Vec<_>>();
        values.retain(|(path, _)| !path.starts_with(&root) || discovered.contains(path));
        for path in discovered {
            let existing = values.iter().position(|(existing, _)| existing == &path);
            let companion = path.parent().map(|parent| {
                parent
                    .join(ConventionalName::Form.canonical())
                    .join(ConventionalName::FormJson.canonical())
            });
            let touched = changed.iter().any(|changed| {
                stdx_path_eq(changed, &path)
                    || companion
                        .as_deref()
                        .is_some_and(|companion| stdx_path_eq(changed, companion))
            });
            if touched || existing.is_none() {
                let value = bsl_metadata::inspection_sources::binary_fingerprint(&path).ok();
                if let Some(index) = existing {
                    values[index].1 = value;
                } else {
                    values.push((path, value));
                }
            }
        }
    }
    values.sort_by(|a, b| a.0.cmp(&b.0));
    if values == *previous {
        return false;
    }
    db.set_ordinary_form_fingerprints(Arc::new(values));
    true
}

fn stdx_path_eq(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
        || a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsl_metadata::xml_parser::form_inspection::parse_ordinary_form_inspection;
    use std::fs::{File, FileTimes};

    #[test]
    fn form_fingerprints_reject_changed_bytes_with_preserved_mtime_and_keep_other_roots() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("base");
        let other = temporary.path().join("other");
        let paths = [
            root.join("Documents/Order/Forms/One/Ext/Form.bin"),
            root.join("Documents/Order/Forms/Two/Ext/Form.bin"),
            other.join("CommonForms/Other/Ext/Form.bin"),
        ];
        for path in &paths {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"original").unwrap();
        }
        let mut db = RootDatabaseImpl::new();
        bootstrap_fingerprints(&mut db, paths.iter().cloned());
        let before = db.ordinary_form_fingerprints();
        let first = before.iter().find(|(path, _)| path == &paths[0]).unwrap().1.clone().unwrap();
        let sidecar=format!("{{\"schema\":3,\"source\":{{\"sha256\":\"{}\"}},\"form\":{{\"pages\":[]}},\"controls\":[]}}",first.sha256);
        assert!(
            parse_ordinary_form_inspection(&sidecar, "One", Some(&first.sha256), "base").is_ok()
        );
        let modified = std::fs::metadata(&paths[0]).unwrap().modified().unwrap();
        std::fs::write(&paths[0], b"changed").unwrap();
        File::options()
            .write(true)
            .open(&paths[0])
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(refresh_fingerprints(
            &mut db,
            std::iter::once(root.clone()),
            &bsl_conventions::RealFs,
            &[paths[0].clone()]
        ));
        let after = db.ordinary_form_fingerprints();
        let first_after =
            after.iter().find(|(path, _)| path == &paths[0]).unwrap().1.clone().unwrap();
        assert!(parse_ordinary_form_inspection(&sidecar, "One", Some(&first_after.sha256), "base")
            .is_err());
        assert_eq!(
            after.iter().find(|(path, _)| path == &paths[1]),
            before.iter().find(|(path, _)| path == &paths[1])
        );
        assert_eq!(
            after.iter().find(|(path, _)| path == &paths[2]),
            before.iter().find(|(path, _)| path == &paths[2])
        );
        assert_eq!(std::fs::metadata(&paths[0]).unwrap().modified().unwrap(), modified);
    }

    #[test]
    fn ordinary_fingerprint_snapshot_is_immutable_until_the_owner_publishes_again() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let path = root.join("CommonForms/Dialog/Ext/Form.bin");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"first").unwrap();
        let mut db = RootDatabaseImpl::new();
        db.set_all_config_paths(vec![(None, root.to_path_buf())], None);
        bootstrap_fingerprints(&mut db, std::iter::once(path.clone()));
        let before = db.ordinary_form_fingerprints();
        let snapshot = crate::workspace::WorkspaceSnapshot::capture(&db, 1);
        std::fs::write(&path, b"second").unwrap();
        let view = crate::workspace::EditorView::new(snapshot);
        assert_eq!(view.database().ordinary_form_fingerprints(), before);
        assert!(refresh_fingerprints(
            &mut db,
            std::iter::once(root.to_path_buf()),
            &bsl_conventions::RealFs,
            &[path]
        ));
        assert_ne!(db.ordinary_form_fingerprints(), before);
        assert_eq!(view.database().ordinary_form_fingerprints(), before);
        let next =
            crate::workspace::EditorView::new(crate::workspace::WorkspaceSnapshot::capture(&db, 2));
        assert_eq!(next.database().ordinary_form_fingerprints(), db.ordinary_form_fingerprints());
    }
}
