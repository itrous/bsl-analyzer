//! Read-only source inspection uses the same file identities as semantic metadata.

use std::path::{Component, PathBuf};
use std::sync::Arc;

use base_db::SourceDatabase;

use crate::RootDatabaseImpl;

/// Binary freshness is separate from the source-text file universe.
pub type OrdinaryFormFingerprint =
    (PathBuf, Option<bsl_metadata::inspection_sources::BinaryFingerprint>);

fn fingerprint_heap((values,): &(Arc<Vec<OrdinaryFormFingerprint>>,)) -> usize {
    stdx::heap::vec_bytes::<OrdinaryFormFingerprint>(values.len())
        + values
            .iter()
            .map(|item| {
                item.0.capacity() + item.1.as_ref().map_or(0, |value| value.estimated_heap_size())
            })
            .sum::<usize>()
}

#[salsa::input(singleton,debug,heap_size=fingerprint_heap)]
pub struct OrdinaryFormFingerprintsInput {
    #[returns(clone)]
    pub values: Arc<Vec<OrdinaryFormFingerprint>>,
}

/// The exact selected root and its visible base/dependencies, in composition order.
impl RootDatabaseImpl {
    pub fn ordinary_form_fingerprints(&self) -> Arc<Vec<OrdinaryFormFingerprint>> {
        OrdinaryFormFingerprintsInput::try_get(self)
            .map(|input| input.values(self))
            .unwrap_or_default()
    }

    pub fn set_ordinary_form_fingerprints(&mut self, values: Arc<Vec<OrdinaryFormFingerprint>>) {
        use salsa::Setter;
        if let Some(input) = OrdinaryFormFingerprintsInput::try_get(self) {
            if input.values(self) != values {
                input.set_values(self).to(values);
            }
        } else {
            OrdinaryFormFingerprintsInput::new(self, values);
        }
    }

    pub fn ordinary_form_fingerprint(
        &self,
        root: &std::path::Path,
        relative: &str,
    ) -> Option<bsl_metadata::inspection_sources::BinaryFingerprint> {
        if !valid_relative(relative) {
            return None;
        }
        let path = root.join(relative);
        let path = path.to_string_lossy();
        #[cfg(windows)]
        let path = path.replace('\\', "/");
        self.ordinary_form_fingerprints()
            .iter()
            .find(|item| {
                let candidate = item.0.to_string_lossy();
                #[cfg(windows)]
                let candidate = candidate.replace('\\', "/");
                stdx::case::eq_ignore_case(&candidate, &path)
            })
            .and_then(|item| item.1.clone())
    }

    pub fn inspection_roots(
        &self,
        root_id: Option<&str>,
    ) -> Result<Vec<(String, PathBuf)>, String> {
        let db = self;
        let snapshot = db.workspace_configs_snapshot();
        let selected = match root_id {
            Some(id) => {
                snapshot.paths.iter().position(|(label, _)| label.as_deref().unwrap_or("") == id)
            }
            None => snapshot
                .paths
                .iter()
                .position(|(label, _)| label.is_none())
                .or_else(|| (snapshot.paths.len() == 1).then_some(0)),
        }
        .ok_or_else(|| {
            "Selected metadata root is absent or ambiguous; specify root_id".to_string()
        })?;
        let mut visible = Vec::new();
        if let Some(base) = snapshot.paths.iter().position(|(label, _)| label.is_none()) {
            visible.push(base);
        }
        if let Some(closure) = snapshot.closures.get(selected) {
            for index in closure {
                if !visible.contains(index) {
                    visible.push(*index);
                }
            }
        }
        if !visible.contains(&selected) {
            visible.push(selected);
        }
        Ok(visible
            .into_iter()
            .map(|index| {
                let (label, path) = &snapshot.paths[index];
                (label.clone().unwrap_or_default(), path.clone())
            })
            .collect())
    }
}

fn valid_relative(relative: &str) -> bool {
    !relative.contains('\\')
        && std::path::Path::new(relative)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// The path comes only from a selected prepared root and declared relative segments.
impl RootDatabaseImpl {
    pub fn inspection_file_id(
        &self,
        root: &std::path::Path,
        relative: &str,
    ) -> Option<vfs::FileId> {
        let db = self;
        if !valid_relative(relative) {
            return None;
        }
        let source = db.try_source_root_input(base_db::METADATA_SOURCE_ROOT)?;
        let candidate = root.join(relative).to_string_lossy().into_owned();
        let modes = vec![
            bsl_conventions::SegmentMatch::Ci;
            std::path::Path::new(relative).components().count()
        ];
        base_db::resolve_vfs_path_ci_query(db, source, candidate, &modes)
    }

    pub fn inspection_file(&self, root: &std::path::Path, relative: &str) -> Option<Arc<str>> {
        let id = self.inspection_file_id(root, relative)?;
        (!self.file_is_unread(id)).then(|| self.file_text_ref(id).clone())
    }

    /// Descriptor names in one container; a missing content file still leaves its identity.
    pub fn inspection_names(&self, root: &std::path::Path, container: &str) -> Vec<String> {
        let db = self;
        if !valid_relative(container) {
            return Vec::new();
        }
        let Some(source) = db.try_source_root_input(base_db::METADATA_SOURCE_ROOT) else {
            return Vec::new();
        };
        let prefix = root.join(container);
        let mut names = Vec::new();
        let files = source.root(db).file_set();
        for id in files.iter() {
            let Some(path) = files.path_for_file(&id) else { continue };
            let path = path.as_path();
            let Some(parent) = path.parent() else { continue };
            if stdx::case::eq_ignore_case(&parent.to_string_lossy(), &prefix.to_string_lossy())
                && path.extension().is_some_and(|ext| {
                    ext.to_string_lossy().eq_ignore_ascii_case(bsl_conventions::XML_EXTENSION)
                })
            {
                if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(name.to_string());
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base_db::{SourceDatabase, SourceRoot};
    use vfs::{file_set::FileSet, FileId, VfsPath};

    #[cfg(windows)]
    #[test]
    fn ordinary_fingerprint_matches_mixed_native_separators() {
        let mut db = RootDatabaseImpl::default();
        let root = PathBuf::from(r"C:\Inspection\Владелец");
        let relative = "CommonForms/Dialog/Ext/Form.bin";
        let fingerprint = bsl_metadata::inspection_sources::BinaryFingerprint {
            sha256: "a".repeat(64),
            legacy_sidecar_stale: Some(false),
        };
        db.set_ordinary_form_fingerprints(Arc::new(vec![(
            PathBuf::from("c:/inspection/владелец/CommonForms/Dialog/Ext/Form.bin"),
            Some(fingerprint.clone()),
        )]));
        assert_eq!(db.ordinary_form_fingerprint(&root, relative), Some(fingerprint));
        assert!(db.ordinary_form_fingerprint(&root, "CommonForms/Other/Ext/Form.bin").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_fingerprint_keeps_literal_backslashes_distinct() {
        let mut db = RootDatabaseImpl::default();
        let root = PathBuf::from("/inspection/owner");
        let relative = "CommonForms/Dialog/Ext/Form.bin";
        let fingerprint = bsl_metadata::inspection_sources::BinaryFingerprint {
            sha256: "a".repeat(64),
            legacy_sidecar_stale: Some(false),
        };
        db.set_ordinary_form_fingerprints(Arc::new(vec![(
            root.join(r"CommonForms\Dialog\Ext\Form.bin"),
            Some(fingerprint.clone()),
        )]));
        assert!(db.ordinary_form_fingerprint(&root, relative).is_none());
        db.set_ordinary_form_fingerprints(Arc::new(vec![(
            root.join(relative),
            Some(fingerprint.clone()),
        )]));
        assert_eq!(db.ordinary_form_fingerprint(&root, relative), Some(fingerprint));
    }

    #[test]
    fn inspection_inputs_track_text_edits_deletions_and_empty_roots() {
        let mut db = RootDatabaseImpl::default();
        let root = PathBuf::from("/inspection");
        let relative = "CommonForms/Dialog/Ext/Form.xml";
        let id = FileId::from_raw(17);
        let other_id = FileId::from_raw(18);
        let other_root = PathBuf::from("/other-inspection");
        let mut files = FileSet::new();
        files.insert(id, VfsPath::new(root.join(relative)));
        files.insert(other_id, VfsPath::new(other_root.join(relative)));
        db.set_source_root(base_db::METADATA_SOURCE_ROOT, SourceRoot::new_metadata(files.clone()));
        db.set_file_source_root(id, base_db::METADATA_SOURCE_ROOT);
        db.set_file_source_root(other_id, base_db::METADATA_SOURCE_ROOT);
        db.set_file_text(other_id, "other-root");
        db.set_file_text(id, "first");
        assert_eq!(db.inspection_file(&root, relative).as_deref(), Some("first"));
        db.set_file_text(id, "second");
        assert_eq!(db.inspection_file(&root, relative).as_deref(), Some("second"));
        assert!(db.inspection_file(&root, "../outside").is_none());
        files.remove(id);
        db.set_source_root(base_db::METADATA_SOURCE_ROOT, SourceRoot::new_metadata(files));
        assert!(db.inspection_file(&root, relative).is_none());
        assert_eq!(db.inspection_file(&other_root, relative).as_deref(), Some("other-root"));
        db.set_source_root(base_db::METADATA_SOURCE_ROOT, SourceRoot::new_metadata(FileSet::new()));
        assert!(db.inspection_file(&root, relative).is_none());
        assert!(db.inspection_file(&other_root, relative).is_none());
    }
}
