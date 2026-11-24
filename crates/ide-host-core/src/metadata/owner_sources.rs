//! Owner configurations contribute semantic dependencies without becoming analysis targets.

use super::VfsWrite;
use crate::{apply_source_batch, BatchTextSource, SourceBatchChange};
use base_db::{content_revision, read_disk_text, SourceDatabase, SourceRoot, BSL_SOURCE_ROOT};
use ide_db::RootDatabaseImpl;
use project_model::{FileRole, SourceSet};
use std::path::PathBuf;
use vfs::{FileSet, VfsPath};

pub(super) fn prepare(
    db: &mut RootDatabaseImpl,
    vfs: &impl VfsWrite,
    changed: Option<&[PathBuf]>,
) -> bool {
    let snapshot = db.workspace_configs_snapshot();
    let dependencies: Vec<_> =
        snapshot.prepared_roots().into_iter().skip(snapshot.paths.len()).collect();
    if dependencies.is_empty() {
        return false;
    }
    let changed_paths = changed.map(|paths| {
        paths
            .iter()
            .map(|path| {
                dependencies
                    .iter()
                    .find_map(|(_, configured, canonical)| {
                        path.strip_prefix(configured).ok().map(|relative| canonical.join(relative))
                    })
                    .unwrap_or_else(|| path.clone())
            })
            .collect::<Vec<_>>()
    });
    let changed = changed_paths.as_deref();
    let is_dependency = |path: &std::path::Path| {
        dependencies.iter().any(|(_, configured, canonical)| {
            path.starts_with(configured) || path.starts_with(canonical)
        })
    };
    if db.try_source_root_input(BSL_SOURCE_ROOT).is_none() {
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(FileSet::default()));
    }
    let old_files = db.source_root_input(BSL_SOURCE_ROOT).root(db).file_set().clone();
    let mut changes = Vec::new();
    if let Some(changed) = changed {
        for file in old_files.iter() {
            let Some(path) = old_files.path_for_file(&file) else { continue };
            let path = path.as_path();
            if is_dependency(path)
                && changed.iter().any(|changed| path.starts_with(changed))
                && !path.is_file()
            {
                changes.push(SourceBatchChange::Remove { file_id: file });
            }
        }
    }
    let roots = changed.map_or_else(
        || dependencies.iter().map(|(_, path, _)| path.clone()).collect::<Vec<_>>(),
        |changed| {
            changed
                .iter()
                .filter(|path| {
                    is_dependency(path)
                        && (path.is_dir() || project_model::is_bsl_source_path(path))
                })
                .cloned()
                .collect()
        },
    );
    let scan = if roots.is_empty() {
        SourceSet::default()
    } else {
        SourceSet::scan_in_scope(&roots, &[], &snapshot.source_exclusions)
    };
    let files = scan
        .files
        .into_iter()
        .filter(|file| file.role == FileRole::Source)
        .filter_map(|file| {
            let path = VfsPath::from(file.canonical);
            let existing = old_files.file_for_path(&path).copied();
            if changed.is_none() && existing.is_some() {
                return None;
            }
            let source = read_disk_text(path.as_path())
                .map_or(BatchTextSource::Unreadable, |text| {
                    BatchTextSource::DiskRevision(content_revision(&text))
                });
            Some((path, source))
        })
        .collect::<Vec<_>>();
    changes.extend(vfs.with_write(|vfs| {
        files
            .into_iter()
            .map(|(path, source)| SourceBatchChange::Upsert {
                file_id: vfs.alloc_file_id(path.clone()),
                path,
                source: Some(source),
            })
            .collect::<Vec<_>>()
    }));
    let effects = apply_source_batch(db, BSL_SOURCE_ROOT, changes);
    effects.bsl_source_changed || effects.file_set_changed
}
