use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use super::context::MetadataContextInput;
use super::documents::matches_context_input;
use anyhow::{bail, Context, Result};
use ide::metadata_rename_plan::{
    MetadataDirectoryEntry, MetadataDirectoryEntryKind, MetadataDirectorySnapshot,
    MetadataOpenDocument, MetadataPlanFileKind, MetadataPlanRoot, PreparedMetadataRenameFile,
    PreparedMetadataRenameSnapshot,
};
use ide_db::base_db::{decode_disk_bytes, SourceDatabase, BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT};
use ide_db::RootDatabaseImpl;
use project_model::{ManagedBaselineDirectory, ManagedDirectoryEntryKind};
use vfs::FileId;

pub(crate) fn capture_metadata_snapshot(
    db: &RootDatabaseImpl,
    roots: Vec<MetadataPlanRoot>,
    context_sha256: [u8; 32],
    context_inputs: &[MetadataContextInput],
    documents: &BTreeMap<PathBuf, MetadataOpenDocument>,
    cancelled: &AtomicBool,
) -> Result<PreparedMetadataRenameSnapshot> {
    let mut resident_paths = BTreeMap::new();
    for source_root in [BSL_SOURCE_ROOT, METADATA_SOURCE_ROOT] {
        let Some(input) = db.try_source_root_input(source_root) else { continue };
        let files = input.root(db).file_set();
        for id in files.iter() {
            let Some(path) = files.path_for_file(&id) else { continue };
            let path = path.as_path().to_path_buf();
            if resident_paths.insert(path, id).is_some_and(|old| old != id) {
                bail!("multiple resident file identities address the same source path");
            }
        }
    }
    let mut snapshot = PreparedMetadataRenameSnapshot {
        roots,
        files: Vec::new(),
        directories: Vec::new(),
        context_sha256,
        unindexed_paths: Vec::new(),
    };
    let mut total_bytes = 0u64;
    for root in snapshot.roots.clone() {
        check_cancelled(cancelled)?;
        verify_root_binding(&root)?;
        let directory = ManagedBaselineDirectory::open_project_root(&root.canonical_path)?;
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            check_cancelled(cancelled)?;
            let entries = directory.read_directory(&portable_path(&relative)?)?;
            let mut roster = Vec::with_capacity(entries.len());
            for entry in entries {
                check_cancelled(cancelled)?;
                let name = entry
                    .name
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("metadata source path is not Unicode"))?;
                let relative_file = relative.join(&name);
                let kind = if entry.kind == ManagedDirectoryEntryKind::Symlink {
                    MetadataDirectoryEntryKind::Symlink
                } else if entry.kind == ManagedDirectoryEntryKind::Directory {
                    pending.push(relative_file.clone());
                    MetadataDirectoryEntryKind::Directory
                } else if entry.kind == ManagedDirectoryEntryKind::File {
                    let stored_path = portable_path(&relative_file)?;
                    let file = directory.open_file(&stored_path).with_context(|| {
                        format!("cannot capture metadata source {}", relative_file.display())
                    })?;
                    let mut bytes = Vec::new();
                    let limit = 64 * 1024 * 1024;
                    file.take(limit + 1).read_to_end(&mut bytes)?;
                    total_bytes = total_bytes.saturating_add(bytes.len() as u64);
                    if bytes.len() as u64 > limit
                        || total_bytes > 128 * 1024 * 1024
                        || snapshot.files.len() >= 10_000
                    {
                        bail!("metadata source snapshot exceeds size or file-count limit");
                    }
                    let source_path = root.source_path.join(&relative_file);
                    let file_id =
                        resident_file(&resident_paths, &source_path, root.case_sensitive)?;
                    let kind = if context_inputs
                        .iter()
                        .any(|input| matches_context_input(input, &source_path))
                    {
                        MetadataPlanFileKind::ProjectInput
                    } else {
                        file_kind(&relative_file, &bytes)
                    };
                    if kind == MetadataPlanFileKind::Unsupported
                        || (!matches!(
                            kind,
                            MetadataPlanFileKind::OpaqueResource
                                | MetadataPlanFileKind::ProjectInput
                        ) && file_id.is_none())
                    {
                        snapshot.unindexed_paths.push(source_path.clone());
                    }
                    let open_document = documents.get(&source_path).map(|document| {
                        let mut document = document.clone();
                        document.dirty |= decode_disk_bytes(&bytes).is_none_or(|text| {
                            ide::metadata_rename_plan::metadata_document_fingerprint(&text)
                                != document.document_text_sha256
                        });
                        document
                    });
                    snapshot.files.push(PreparedMetadataRenameFile {
                        file_id,
                        root_id: root.id.clone(),
                        relative_path: relative_file,
                        kind,
                        bytes: bytes.into(),
                        open_document,
                        effective_form_attributes: None,
                    });
                    MetadataDirectoryEntryKind::File
                } else {
                    bail!("metadata source entry is neither a regular file nor a directory");
                };
                roster.push(MetadataDirectoryEntry { name, kind });
            }
            snapshot.directories.push(MetadataDirectorySnapshot {
                root_id: root.id.clone(),
                relative_path: relative,
                entries: roster,
            });
        }
        verify_root_binding(&root)?;
    }
    for path in documents.keys() {
        if !snapshot.files.iter().any(|file| {
            snapshot.roots.iter().any(|root| {
                root.id == file.root_id && root.source_path.join(&file.relative_path) == *path
            })
        }) {
            snapshot.unindexed_paths.push(path.clone());
        }
    }
    snapshot.unindexed_paths.sort();
    snapshot.unindexed_paths.dedup();
    snapshot
        .files
        .sort_by(|a, b| (&a.root_id, &a.relative_path).cmp(&(&b.root_id, &b.relative_path)));
    snapshot
        .directories
        .sort_by(|a, b| (&a.root_id, &a.relative_path).cmp(&(&b.root_id, &b.relative_path)));
    Ok(snapshot)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("metadata rename capture cancelled");
    }
    Ok(())
}

fn verify_root_binding(root: &MetadataPlanRoot) -> Result<()> {
    if std::fs::canonicalize(&root.source_path)? != root.canonical_path {
        bail!("metadata source root binding changed");
    }
    Ok(())
}

fn portable_path(path: &Path) -> Result<String> {
    let mut result = Vec::new();
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            bail!("metadata source path is not relative");
        };
        result.push(name.to_str().context("metadata source path is not Unicode")?);
    }
    Ok(result.join("/"))
}

fn resident_file(
    paths: &BTreeMap<PathBuf, FileId>,
    path: &Path,
    case_sensitive: bool,
) -> Result<Option<FileId>> {
    let lookup = |path: &Path| -> Result<Option<FileId>> {
        if case_sensitive {
            return Ok(paths.get(path).copied());
        }
        let mut matches = paths.iter().filter(|(candidate, _)| {
            stdx::case::eq_ignore_case(&candidate.to_string_lossy(), &path.to_string_lossy())
        });
        let result = matches.next().map(|(_, id)| *id);
        if matches.next().is_some() {
            bail!("metadata source path has ambiguous case-insensitive resident identities");
        }
        Ok(result)
    };
    if let Some(id) = lookup(path)? {
        return Ok(Some(id));
    }
    // Shared owner roots are canonical; a project's declared Windows path may lack their prefix.
    match path.canonicalize() {
        Ok(canonical) => lookup(&canonical),
        Err(_) => Ok(None),
    }
}

fn file_kind(path: &Path, bytes: &[u8]) -> MetadataPlanFileKind {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some(bsl_conventions::BSL_EXTENSION) => MetadataPlanFileKind::Bsl,
        Some(bsl_conventions::XML_EXTENSION) => MetadataPlanFileKind::Xml,
        Some("png") if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => {
            MetadataPlanFileKind::OpaqueResource
        }
        Some("jpg" | "jpeg") if bytes.starts_with(b"\xff\xd8\xff") => {
            MetadataPlanFileKind::OpaqueResource
        }
        Some("gif") if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => {
            MetadataPlanFileKind::OpaqueResource
        }
        Some("bmp") if bytes.starts_with(b"BM") => MetadataPlanFileKind::OpaqueResource,
        Some("ico") if bytes.starts_with(b"\0\0\x01\0") => MetadataPlanFileKind::OpaqueResource,
        _ => MetadataPlanFileKind::Unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide::metadata_rename_plan::{
        validate_metadata_rename_roster, MetadataPlanProblem, MetadataRootRole,
    };
    use ide_db::base_db::SourceRoot;
    use vfs::{FileSet, VfsPath};

    fn root(path: &Path) -> MetadataPlanRoot {
        MetadataPlanRoot {
            id: "main".into(),
            source_path: path.to_path_buf(),
            canonical_path: std::fs::canonicalize(path).unwrap(),
            role: MetadataRootRole::Main,
            writable: true,
            case_sensitive: !cfg!(windows),
        }
    }

    #[test]
    fn capture_keeps_raw_bytes_and_the_dirty_document_version_separate() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("Module.bsl");
        let before = "\u{feff}Процедура Тест()\r\nКонецПроцедуры\r\n";
        std::fs::write(&path, before).unwrap();
        let mut db = RootDatabaseImpl::new();
        let id = FileId(0);
        let mut files = FileSet::default();
        files.insert(id, VfsPath::from(path.clone()));
        db.set_source_root(BSL_SOURCE_ROOT, SourceRoot::new_local(files));
        db.set_file_source_root(id, BSL_SOURCE_ROOT);
        db.set_file_text(id, before);
        let documents = BTreeMap::from([(
            path.clone(),
            MetadataOpenDocument {
                version: 7,
                document_text_sha256: bsl_metadata::rename_xml::metadata_source_fingerprint(
                    "Процедура Изменена()\nКонецПроцедуры".as_bytes(),
                ),
                dirty: false,
            },
        )]);

        let snapshot = capture_metadata_snapshot(
            &db,
            vec![root(temporary.path())],
            [42; 32],
            &[],
            &documents,
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(snapshot.files.len(), 1);
        assert_eq!(snapshot.files[0].bytes.as_ref(), before.as_bytes());
        let document = snapshot.files[0].open_document.as_ref().unwrap();
        assert!(document.dirty);
        assert_eq!(document.version, 7);
        assert_eq!(
            validate_metadata_rename_roster(&snapshot),
            [MetadataPlanProblem::DirtyDocument]
        );
        assert_eq!(std::fs::read(&path).unwrap(), before.as_bytes());
    }

    #[test]
    fn unindexed_and_unsupported_sources_are_kept_in_the_namespace() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("Nested")).unwrap();
        std::fs::write(temporary.path().join("Nested/Form.xml"), "<Form/>").unwrap();
        std::fs::write(
            temporary.path().join("looks-like-an-image.png"),
            "Метаданные.Справочники.Товары",
        )
        .unwrap();

        let snapshot = capture_metadata_snapshot(
            &RootDatabaseImpl::new(),
            vec![root(temporary.path())],
            [0; 32],
            &[],
            &BTreeMap::new(),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert_eq!(snapshot.files.len(), 2);
        assert_eq!(snapshot.directories.len(), 2);
        assert_eq!(snapshot.unindexed_paths.len(), 2);
        assert!(validate_metadata_rename_roster(&snapshot)
            .contains(&MetadataPlanProblem::UnindexedSource));
        assert!(snapshot.files.iter().any(|file| file.kind == MetadataPlanFileKind::Unsupported));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_listed_without_reading_its_target() {
        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("Outside.bsl"), "unowned source").unwrap();
        std::os::unix::fs::symlink(outside.path(), temporary.path().join("Linked")).unwrap();

        let snapshot = capture_metadata_snapshot(
            &RootDatabaseImpl::new(),
            vec![root(temporary.path())],
            [0; 32],
            &[],
            &BTreeMap::new(),
            &AtomicBool::new(false),
        )
        .unwrap();

        assert!(snapshot.files.is_empty());
        assert_eq!(snapshot.directories.len(), 1);
        assert_eq!(snapshot.directories[0].entries[0].kind, MetadataDirectoryEntryKind::Symlink);
        assert!(validate_metadata_rename_roster(&snapshot).contains(&MetadataPlanProblem::Symlink));
    }

    #[test]
    fn cancellation_preempts_opening_a_source_root() {
        let temporary = tempfile::tempdir().unwrap();
        let selected = root(temporary.path());
        let error = capture_metadata_snapshot(
            &RootDatabaseImpl::new(),
            vec![selected],
            [0; 32],
            &[],
            &BTreeMap::new(),
            &AtomicBool::new(true),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn a_case_insensitive_binding_requires_one_resident_identity() {
        let paths = BTreeMap::from([
            (PathBuf::from("Root/Module.bsl"), FileId(1)),
            (PathBuf::from("Root/MODULE.bsl"), FileId(2)),
        ]);
        assert!(resident_file(&paths, Path::new("Root/Module.bsl"), false).is_err());
        assert_eq!(
            resident_file(&paths, Path::new("Root/Module.bsl"), true).unwrap(),
            Some(FileId(1))
        );
    }

    #[test]
    fn a_canonical_resident_is_found_after_a_declared_path_miss() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("Nested")).unwrap();
        let path = temporary.path().join("Module.bsl");
        std::fs::write(&path, "source").unwrap();
        let declared = temporary.path().join("Nested/../Module.bsl");
        let canonical = path.canonicalize().unwrap();
        let paths = BTreeMap::from([(canonical, FileId(7))]);
        assert!(!paths.contains_key(&declared));
        for case_sensitive in [false, true] {
            assert_eq!(resident_file(&paths, &declared, case_sensitive).unwrap(), Some(FileId(7)));
        }
        assert_eq!(
            resident_file(&paths, &temporary.path().join("Missing.bsl"), false).unwrap(),
            None
        );
    }

    #[test]
    fn canonical_fallback_does_not_choose_between_ambiguous_resident_identities() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::create_dir(temporary.path().join("Nested")).unwrap();
        let path = temporary.path().join("Module.bsl");
        std::fs::write(&path, "source").unwrap();
        let canonical = path.canonicalize().unwrap();
        let paths = BTreeMap::from([
            (canonical.clone(), FileId(1)),
            (canonical.with_file_name("MODULE.bsl"), FileId(2)),
        ]);
        let declared = temporary.path().join("Nested/../Module.bsl");
        assert!(!paths.contains_key(&declared));
        assert!(resident_file(&paths, &declared, false).is_err());
        assert_eq!(resident_file(&paths, &declared, true).unwrap(), Some(FileId(1)));
    }

    #[test]
    fn xml_editor_versions_are_bound_by_path_even_without_a_resident_file_id() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("Form.xml");
        std::fs::write(&path, "<Form/>").unwrap();
        let state = MetadataOpenDocument {
            version: 12,
            document_text_sha256: bsl_metadata::rename_xml::metadata_source_fingerprint(b"<Form/>"),
            dirty: false,
        };
        let documents = BTreeMap::from([(path, state.clone())]);
        let snapshot = capture_metadata_snapshot(
            &RootDatabaseImpl::new(),
            vec![root(temporary.path())],
            [0; 32],
            &[],
            &documents,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(snapshot.files[0].file_id, None);
        assert_eq!(snapshot.files[0].open_document, Some(state));
        assert!(validate_metadata_rename_roster(&snapshot)
            .contains(&MetadataPlanProblem::UnindexedSource));
    }

    #[test]
    fn clean_claim_with_different_xml_text_is_dirty_and_missing_editor_source_is_incomplete() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("Form.xml");
        std::fs::write(&path, "<Form/>").unwrap();
        let documents = BTreeMap::from([
            (
                path,
                MetadataOpenDocument { version: 1, document_text_sha256: [0; 32], dirty: false },
            ),
            (
                temporary.path().join("Unsaved.xml"),
                MetadataOpenDocument { version: 1, document_text_sha256: [0; 32], dirty: false },
            ),
        ]);
        let snapshot = capture_metadata_snapshot(
            &RootDatabaseImpl::new(),
            vec![root(temporary.path())],
            [0; 32],
            &[],
            &documents,
            &AtomicBool::new(false),
        )
        .unwrap();
        let problems = validate_metadata_rename_roster(&snapshot);
        assert!(problems.contains(&MetadataPlanProblem::DirtyDocument));
        assert!(problems.contains(&MetadataPlanProblem::UnindexedSource));
        assert!(snapshot.unindexed_paths.contains(&temporary.path().join("Unsaved.xml")));
    }
}
