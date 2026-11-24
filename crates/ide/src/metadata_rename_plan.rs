//! A metadata plan contains original byte edits and source namespace operations.

mod build;
pub use build::prepare_metadata_rename_plan;

use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use hir::metadata_symbol::{MetadataSymbolIdentity, MetadataSymbolPath};
use vfs::FileId;

use crate::metadata_rename::MetadataPreviewIssue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataRootRole {
    Main,
    Extension,
    Dependency,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPlanRoot {
    pub id: String,
    pub source_path: PathBuf,
    pub canonical_path: PathBuf,
    pub role: MetadataRootRole,
    pub writable: bool,
    pub case_sensitive: bool,
}

/// This hash addresses decoded editor text; raw disk bytes have their own fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataOpenDocument {
    pub version: i32,
    pub document_text_sha256: [u8; 32],
    pub dirty: bool,
}

/// An editor excludes the file's leading UTF-8 BOM while raw source fingerprints retain it.
pub fn metadata_document_fingerprint(text: &str) -> [u8; 32] {
    bsl_metadata::rename_xml::metadata_source_fingerprint(
        text.strip_prefix('\u{feff}').unwrap_or(text).as_bytes(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataPlanFileKind {
    Bsl,
    Xml,
    /// Assigned only when the source format establishes that the payload contains no metadata paths.
    OpaqueResource,
    /// An exact Analyzer configuration input, bound by the applying layer and never renamed.
    ProjectInput,
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct PreparedMetadataRenameFile {
    /// A session binding for semantic resolution; the journal addresses roots and relative paths.
    pub file_id: Option<FileId>,
    pub root_id: String,
    pub relative_path: PathBuf,
    pub kind: MetadataPlanFileKind,
    pub bytes: Arc<[u8]>,
    pub open_document: Option<MetadataOpenDocument>,
    pub effective_form_attributes: Option<Arc<Vec<bsl_metadata::FormAttribute>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataDirectoryEntryKind {
    File,
    Directory,
    /// Symlinks are refused before staging, including links to otherwise permitted roots.
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataDirectoryEntry {
    pub name: String,
    pub kind: MetadataDirectoryEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataDirectorySnapshot {
    pub root_id: String,
    pub relative_path: PathBuf,
    pub entries: Vec<MetadataDirectoryEntry>,
}

#[derive(Debug, Clone)]
pub struct PreparedMetadataRenameSnapshot {
    pub roots: Vec<MetadataPlanRoot>,
    pub files: Vec<PreparedMetadataRenameFile>,
    pub directories: Vec<MetadataDirectorySnapshot>,
    /// Covers owner/dependency bindings and source-root roles, independently of file content.
    pub context_sha256: [u8; 32],
    pub unindexed_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataFileRead {
    pub file_id: Option<FileId>,
    pub root_id: String,
    pub relative_path: PathBuf,
    pub before_sha256: [u8; 32],
    pub open_document: Option<MetadataOpenDocument>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRawEdit {
    pub range: Range<usize>,
    pub expected_bytes: Vec<u8>,
    pub replacement_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataPlannedFile {
    pub root_id: String,
    pub relative_path: PathBuf,
    pub before_sha256: [u8; 32],
    pub edits: Vec<MetadataRawEdit>,
    pub after_bytes: Vec<u8>,
}

/// Text edits address the old paths and precede these renames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataResourceRename {
    pub root_id: String,
    pub from_relative: PathBuf,
    pub to_relative: PathBuf,
    pub kind: MetadataDirectoryEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataPlanProblem {
    Reference(MetadataPreviewIssue),
    Xml { file_id: FileId, reason: bsl_metadata::rename_xml::XmlReferenceProblem },
    UnindexedSource,
    InvalidPath,
    Symlink,
    NamespaceIncomplete,
    DuplicateSource,
    DirtyDocument,
    ResidentTextMismatch,
    DocumentTextMismatch,
    ReadonlyRoot,
    ResourceCollision,
    NameCollision,
    UnsupportedResourceLayout,
    OverlappingEdits,
    ExpectedBytesMismatch,
    StaleSource,
    StaleDirectory,
    StaleContext,
    ReadonlyContextInput,
}

#[derive(Debug, Clone)]
pub struct MetadataRenamePlan {
    pub target: MetadataSymbolIdentity,
    pub path: MetadataSymbolPath,
    pub new_name: String,
    pub roots: Vec<MetadataPlanRoot>,
    pub context_sha256: [u8; 32],
    pub read_set: Vec<MetadataFileRead>,
    pub directories: Vec<MetadataDirectorySnapshot>,
    pub files: Vec<MetadataPlannedFile>,
    pub resources: Vec<MetadataResourceRename>,
    pub problems: Vec<MetadataPlanProblem>,
}

impl MetadataRenamePlan {
    pub fn can_apply(&self) -> bool {
        self.problems.is_empty()
    }
}

fn relative_path(path: &Path, allow_root: bool) -> bool {
    (allow_root || !path.as_os_str().is_empty())
        && path.components().all(|component| match component {
            Component::Normal(name) => {
                name.to_str().is_some_and(|name| !name.contains(['\\', ':', '\0']))
            }
            _ => false,
        })
}

/// Every entry used to prove coverage must have an immutable input of its own.
pub fn validate_metadata_rename_roster(
    snapshot: &PreparedMetadataRenameSnapshot,
) -> Vec<MetadataPlanProblem> {
    let mut problems = Vec::new();
    if !snapshot.unindexed_paths.is_empty() {
        problems.push(MetadataPlanProblem::UnindexedSource);
    }
    for (index, root) in snapshot.roots.iter().enumerate() {
        if !root.source_path.is_absolute()
            || !root.canonical_path.is_absolute()
            || snapshot.roots[..index]
                .iter()
                .any(|other| other.id == root.id || other.canonical_path == root.canonical_path)
        {
            problems.push(MetadataPlanProblem::InvalidPath);
        }
        if !snapshot.directories.iter().any(|directory| {
            directory.root_id == root.id && directory.relative_path.as_os_str().is_empty()
        }) {
            problems.push(MetadataPlanProblem::NamespaceIncomplete);
        }
    }
    for (index, file) in snapshot.files.iter().enumerate() {
        if !relative_path(&file.relative_path, false)
            || !snapshot.roots.iter().any(|root| root.id == file.root_id)
        {
            problems.push(MetadataPlanProblem::InvalidPath);
        }
        if snapshot.files[..index].iter().any(|other| {
            (other.root_id == file.root_id && other.relative_path == file.relative_path)
                || file.file_id.is_some_and(|id| other.file_id == Some(id))
        }) {
            problems.push(MetadataPlanProblem::DuplicateSource);
        }
        if file.open_document.as_ref().is_some_and(|document| document.dirty) {
            problems.push(MetadataPlanProblem::DirtyDocument);
        }
        let listed = file.relative_path.parent().zip(file.relative_path.file_name()).is_some_and(
            |(parent, name)| {
                snapshot.directories.iter().any(|directory| {
                    directory.root_id == file.root_id
                        && directory.relative_path == parent
                        && directory.entries.iter().any(|entry| {
                            entry.kind == MetadataDirectoryEntryKind::File
                                && std::ffi::OsStr::new(&entry.name) == name
                        })
                })
            },
        );
        if !listed {
            problems.push(MetadataPlanProblem::NamespaceIncomplete);
        }
    }
    for (index, directory) in snapshot.directories.iter().enumerate() {
        if !relative_path(&directory.relative_path, true)
            || !snapshot.roots.iter().any(|root| root.id == directory.root_id)
        {
            problems.push(MetadataPlanProblem::InvalidPath);
        }
        if snapshot.directories[..index].iter().any(|other| {
            other.root_id == directory.root_id && other.relative_path == directory.relative_path
        }) {
            problems.push(MetadataPlanProblem::DuplicateSource);
        }
        if !directory.relative_path.as_os_str().is_empty() {
            let listed = directory
                .relative_path
                .parent()
                .zip(directory.relative_path.file_name())
                .is_some_and(|(parent, name)| {
                    snapshot.directories.iter().any(|ancestor| {
                        ancestor.root_id == directory.root_id
                            && ancestor.relative_path == parent
                            && ancestor.entries.iter().any(|entry| {
                                entry.kind == MetadataDirectoryEntryKind::Directory
                                    && std::ffi::OsStr::new(&entry.name) == name
                            })
                    })
                });
            if !listed {
                problems.push(MetadataPlanProblem::NamespaceIncomplete);
            }
        }
        for entry in &directory.entries {
            if !relative_path(Path::new(&entry.name), false)
                || Path::new(&entry.name).components().count() != 1
            {
                problems.push(MetadataPlanProblem::InvalidPath);
                continue;
            }
            let path = directory.relative_path.join(&entry.name);
            let present = match entry.kind {
                MetadataDirectoryEntryKind::File => snapshot
                    .files
                    .iter()
                    .any(|file| file.root_id == directory.root_id && file.relative_path == path),
                MetadataDirectoryEntryKind::Directory => snapshot
                    .directories
                    .iter()
                    .any(|child| child.root_id == directory.root_id && child.relative_path == path),
                MetadataDirectoryEntryKind::Symlink => {
                    problems.push(MetadataPlanProblem::Symlink);
                    true
                }
            };
            if !present {
                problems.push(MetadataPlanProblem::NamespaceIncomplete);
            }
        }
    }
    problems
}

/// A replacement is accepted only against the exact original range and bytes.
pub fn render_metadata_edits(
    before: &[u8],
    edits: &mut Vec<MetadataRawEdit>,
) -> Result<Vec<u8>, MetadataPlanProblem> {
    edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
    edits.dedup();
    let mut output = Vec::new();
    let mut cursor = 0;
    let mut previous = None;
    for edit in edits {
        if edit.range.start < cursor || previous.as_ref() == Some(&edit.range) {
            return Err(MetadataPlanProblem::OverlappingEdits);
        }
        if before.get(edit.range.clone()) != Some(edit.expected_bytes.as_slice()) {
            return Err(MetadataPlanProblem::ExpectedBytesMismatch);
        }
        output.extend_from_slice(&before[cursor..edit.range.start]);
        output.extend_from_slice(&edit.replacement_bytes);
        cursor = edit.range.end;
        previous = Some(edit.range.clone());
    }
    output.extend_from_slice(&before[cursor..]);
    Ok(output)
}

pub fn validate_metadata_resource_rename(
    snapshot: &PreparedMetadataRenameSnapshot,
    operation: &MetadataResourceRename,
) -> Result<(), MetadataPlanProblem> {
    let root = snapshot
        .roots
        .iter()
        .find(|root| root.id == operation.root_id)
        .ok_or(MetadataPlanProblem::InvalidPath)?;
    if !root.writable {
        return Err(MetadataPlanProblem::ReadonlyRoot);
    }
    if snapshot.files.iter().any(|file| {
        file.root_id == operation.root_id
            && file.kind == MetadataPlanFileKind::ProjectInput
            && file.relative_path.starts_with(&operation.from_relative)
    }) {
        return Err(MetadataPlanProblem::ReadonlyContextInput);
    }
    if !relative_path(&operation.from_relative, false)
        || !relative_path(&operation.to_relative, false)
        || operation.from_relative.parent() != operation.to_relative.parent()
    {
        return Err(MetadataPlanProblem::InvalidPath);
    }
    if operation.kind == MetadataDirectoryEntryKind::Symlink {
        return Err(MetadataPlanProblem::Symlink);
    }
    let parent = operation.from_relative.parent().unwrap_or(Path::new(""));
    let directory = snapshot
        .directories
        .iter()
        .find(|directory| directory.root_id == root.id && directory.relative_path == parent)
        .ok_or(MetadataPlanProblem::NamespaceIncomplete)?;
    let Some(from) = operation.from_relative.file_name().and_then(|name| name.to_str()) else {
        return Err(MetadataPlanProblem::InvalidPath);
    };
    let Some(to) = operation.to_relative.file_name().and_then(|name| name.to_str()) else {
        return Err(MetadataPlanProblem::InvalidPath);
    };
    if !directory.entries.iter().any(|entry| entry.name == from && entry.kind == operation.kind) {
        return Err(MetadataPlanProblem::NamespaceIncomplete);
    }
    if directory.entries.iter().any(|entry| {
        entry.name != from
            && if root.case_sensitive {
                entry.name == to
            } else {
                stdx::case::eq_ignore_case(&entry.name, to)
            }
    }) {
        return Err(MetadataPlanProblem::ResourceCollision);
    }
    Ok(())
}

/// A Designer object has a descriptor and, when present, a same-stem source container.
pub fn metadata_object_resource_renames(
    snapshot: &PreparedMetadataRenameSnapshot,
    root_id: &str,
    descriptor: &Path,
    old_name: &str,
    new_name: &str,
) -> Result<Vec<MetadataResourceRename>, MetadataPlanProblem> {
    if descriptor.extension().and_then(|extension| extension.to_str())
        != Some(bsl_conventions::XML_EXTENSION)
        || !descriptor
            .file_stem()
            .and_then(|stem| stem.to_str())
            .is_some_and(|stem| stdx::case::eq_ignore_case(stem, old_name))
        || !crate::rename::is_valid_identifier(new_name)
    {
        return Err(MetadataPlanProblem::UnsupportedResourceLayout);
    }
    let renamed =
        descriptor.with_file_name(new_name).with_extension(bsl_conventions::XML_EXTENSION);
    let mut operations = vec![MetadataResourceRename {
        root_id: root_id.into(),
        from_relative: descriptor.into(),
        to_relative: renamed,
        kind: MetadataDirectoryEntryKind::File,
    }];
    let container = descriptor.with_extension("");
    if snapshot
        .directories
        .iter()
        .any(|directory| directory.root_id == root_id && directory.relative_path == container)
    {
        operations.push(MetadataResourceRename {
            root_id: root_id.into(),
            from_relative: container.clone(),
            to_relative: container.with_file_name(new_name),
            kind: MetadataDirectoryEntryKind::Directory,
        });
    }
    for operation in &operations {
        validate_metadata_resource_rename(snapshot, operation)?;
    }
    Ok(operations)
}

/// This comparison includes unchanged read inputs and the full source namespace.
pub fn validate_metadata_plan_snapshot(
    plan: &MetadataRenamePlan,
    snapshot: &PreparedMetadataRenameSnapshot,
) -> Vec<MetadataPlanProblem> {
    let mut problems = validate_metadata_rename_roster(snapshot);
    if plan.roots != snapshot.roots || plan.context_sha256 != snapshot.context_sha256 {
        problems.push(MetadataPlanProblem::StaleContext);
    }
    if plan.read_set.len() != snapshot.files.len() {
        problems.push(MetadataPlanProblem::StaleSource);
    }
    for read in &plan.read_set {
        let same = snapshot
            .files
            .iter()
            .find(|file| file.root_id == read.root_id && file.relative_path == read.relative_path)
            .is_some_and(|file| {
                file.file_id == read.file_id
                    && bsl_metadata::rename_xml::metadata_source_fingerprint(&file.bytes)
                        == read.before_sha256
                    && file.open_document == read.open_document
            });
        if !same {
            problems.push(MetadataPlanProblem::StaleSource);
        }
    }
    if plan.directories.len() != snapshot.directories.len() {
        problems.push(MetadataPlanProblem::StaleDirectory);
    }
    for directory in &plan.directories {
        let same = snapshot
            .directories
            .iter()
            .find(|current| {
                current.root_id == directory.root_id
                    && current.relative_path == directory.relative_path
            })
            .is_some_and(|current| {
                let mut expected = directory.entries.clone();
                let mut actual = current.entries.clone();
                expected.sort_by(|left, right| left.name.cmp(&right.name));
                actual.sort_by(|left, right| left.name.cmp(&right.name));
                expected == actual
            });
        if !same {
            problems.push(MetadataPlanProblem::StaleDirectory);
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_relative_paths_accept_separators_and_refuse_escapes_or_alternate_streams() {
        assert!(relative_path(&Path::new("Catalogs").join("Partners").join("Module.bsl"), false));
        assert!(relative_path(Path::new(""), true));
        assert!(!relative_path(Path::new(""), false));
        for path in ["../escape.xml", "/absolute.xml", "Module.bsl:stream", "bad\0name"] {
            assert!(!relative_path(Path::new(path), false), "{path:?}");
        }
        #[cfg(unix)]
        assert!(!relative_path(Path::new("literal\\backslash.xml"), false));
    }

    fn snapshot() -> PreparedMetadataRenameSnapshot {
        PreparedMetadataRenameSnapshot {
            roots: vec![MetadataPlanRoot {
                id: "main".into(),
                source_path: stdx::test_temp::root().join("metadata-plan-fixture"),
                canonical_path: stdx::test_temp::root().join("metadata-plan-fixture"),
                role: MetadataRootRole::Main,
                writable: true,
                case_sensitive: true,
            }],
            files: vec![
                PreparedMetadataRenameFile {
                    file_id: Some(FileId(1)),
                    root_id: "main".into(),
                    relative_path: "Partners.xml".into(),
                    kind: MetadataPlanFileKind::Xml,
                    bytes: Arc::from(b"<Name>Partners</Name>".as_slice()),
                    open_document: None,
                    effective_form_attributes: None,
                },
                PreparedMetadataRenameFile {
                    file_id: Some(FileId(2)),
                    root_id: "main".into(),
                    relative_path: "Partners/Module.bsl".into(),
                    kind: MetadataPlanFileKind::Bsl,
                    bytes: Arc::from(b"A = 1;".as_slice()),
                    open_document: Some(MetadataOpenDocument {
                        version: 7,
                        document_text_sha256: bsl_metadata::rename_xml::metadata_source_fingerprint(
                            b"A = 1;",
                        ),
                        dirty: false,
                    }),
                    effective_form_attributes: None,
                },
            ],
            directories: vec![
                MetadataDirectorySnapshot {
                    root_id: "main".into(),
                    relative_path: "".into(),
                    entries: vec![
                        MetadataDirectoryEntry {
                            name: "Partners.xml".into(),
                            kind: MetadataDirectoryEntryKind::File,
                        },
                        MetadataDirectoryEntry {
                            name: "Partners".into(),
                            kind: MetadataDirectoryEntryKind::Directory,
                        },
                    ],
                },
                MetadataDirectorySnapshot {
                    root_id: "main".into(),
                    relative_path: "Partners".into(),
                    entries: vec![MetadataDirectoryEntry {
                        name: "Module.bsl".into(),
                        kind: MetadataDirectoryEntryKind::File,
                    }],
                },
            ],
            context_sha256: [1; 32],
            unindexed_paths: Vec::new(),
        }
    }

    #[test]
    fn raw_replacements_preserve_every_unrelated_byte() {
        let before = b"\xef\xbb\xbf<Name>Caption</Name>\r\n<Title>Caption</Title>\r\n";
        let mut edits = vec![MetadataRawEdit {
            range: 9..16,
            expected_bytes: b"Caption".to_vec(),
            replacement_bytes: b"Heading".to_vec(),
        }];
        assert_eq!(
            render_metadata_edits(before, &mut edits).unwrap(),
            b"\xef\xbb\xbf<Name>Heading</Name>\r\n<Title>Caption</Title>\r\n"
        );
        edits[0].expected_bytes = b"Changed".to_vec();
        assert_eq!(
            render_metadata_edits(before, &mut edits),
            Err(MetadataPlanProblem::ExpectedBytesMismatch)
        );
    }

    #[test]
    fn conflicting_replacements_are_rejected_before_rendering() {
        let mut edits = vec![
            MetadataRawEdit {
                range: 0..2,
                expected_bytes: b"ab".to_vec(),
                replacement_bytes: b"A".to_vec(),
            },
            MetadataRawEdit {
                range: 1..3,
                expected_bytes: b"bc".to_vec(),
                replacement_bytes: b"B".to_vec(),
            },
        ];
        assert_eq!(
            render_metadata_edits(b"abcd", &mut edits),
            Err(MetadataPlanProblem::OverlappingEdits)
        );
    }

    #[test]
    fn source_roster_requires_all_files_and_directories() {
        let mut snapshot = snapshot();
        assert!(validate_metadata_rename_roster(&snapshot).is_empty());
        snapshot.files.pop();
        assert!(validate_metadata_rename_roster(&snapshot)
            .contains(&MetadataPlanProblem::NamespaceIncomplete));
        snapshot.directories[0].entries.push(MetadataDirectoryEntry {
            name: "link".into(),
            kind: MetadataDirectoryEntryKind::Symlink,
        });
        assert!(validate_metadata_rename_roster(&snapshot).contains(&MetadataPlanProblem::Symlink));
    }

    #[test]
    fn object_rename_moves_descriptor_and_existing_container() {
        let snapshot = snapshot();
        let operations = metadata_object_resource_renames(
            &snapshot,
            "main",
            Path::new("Partners.xml"),
            "Partners",
            "Customers",
        )
        .unwrap();
        assert_eq!(operations.len(), 2);
        assert_eq!(operations[0].to_relative, Path::new("Customers.xml"));
        assert_eq!(operations[1].to_relative, Path::new("Customers"));
        assert_eq!(&*snapshot.files[1].bytes, b"A = 1;");
    }

    #[test]
    fn resource_collisions_readonly_roots_and_escaping_paths_are_rejected() {
        let mut snapshot = snapshot();
        snapshot.directories[0].entries.push(MetadataDirectoryEntry {
            name: "Customers.xml".into(),
            kind: MetadataDirectoryEntryKind::File,
        });
        assert_eq!(
            metadata_object_resource_renames(
                &snapshot,
                "main",
                Path::new("Partners.xml"),
                "Partners",
                "Customers"
            ),
            Err(MetadataPlanProblem::ResourceCollision)
        );
        snapshot.roots[0].writable = false;
        assert_eq!(
            metadata_object_resource_renames(
                &snapshot,
                "main",
                Path::new("Partners.xml"),
                "Partners",
                "NewPartner"
            ),
            Err(MetadataPlanProblem::ReadonlyRoot)
        );
        snapshot.files[0].relative_path = "../escape.xml".into();
        assert!(
            validate_metadata_rename_roster(&snapshot).contains(&MetadataPlanProblem::InvalidPath)
        );
    }

    #[test]
    fn resource_rename_cannot_move_an_analyzer_context_input() {
        let mut snapshot = snapshot();
        snapshot.files[1].kind = MetadataPlanFileKind::ProjectInput;
        let operation = MetadataResourceRename {
            root_id: "main".into(),
            from_relative: "Partners".into(),
            to_relative: "Customers".into(),
            kind: MetadataDirectoryEntryKind::Directory,
        };
        assert_eq!(
            validate_metadata_resource_rename(&snapshot, &operation),
            Err(MetadataPlanProblem::ReadonlyContextInput)
        );
    }

    #[test]
    fn stale_check_covers_unchanged_inputs_open_versions_and_namespace() {
        let snapshot = snapshot();
        let plan = MetadataRenamePlan {
            target: MetadataSymbolIdentity {
                declaration_file: FileId(1),
                owner_uuid: bsl_metadata::Uuid::from_u128(1),
                declaration_uuid: bsl_metadata::Uuid::from_u128(2),
            },
            path: MetadataSymbolPath {
                owner_kind: bsl_metadata::MdoType::Catalog,
                owner_name: hir::Name::new("Partners"),
                members: Vec::new(),
            },
            new_name: "Customers".into(),
            roots: snapshot.roots.clone(),
            context_sha256: snapshot.context_sha256,
            read_set: snapshot
                .files
                .iter()
                .map(|file| MetadataFileRead {
                    file_id: file.file_id,
                    root_id: file.root_id.clone(),
                    relative_path: file.relative_path.clone(),
                    before_sha256: bsl_metadata::rename_xml::metadata_source_fingerprint(
                        &file.bytes,
                    ),
                    open_document: file.open_document.clone(),
                })
                .collect(),
            directories: snapshot.directories.clone(),
            files: Vec::new(),
            resources: Vec::new(),
            problems: Vec::new(),
        };
        assert!(validate_metadata_plan_snapshot(&plan, &snapshot).is_empty());
        let mut changed = snapshot.clone();
        changed.files[1].bytes = Arc::from(b"A = 2;".as_slice());
        assert!(validate_metadata_plan_snapshot(&plan, &changed)
            .contains(&MetadataPlanProblem::StaleSource));
        let mut changed = snapshot.clone();
        changed.files[1].open_document.as_mut().unwrap().version += 1;
        assert!(validate_metadata_plan_snapshot(&plan, &changed)
            .contains(&MetadataPlanProblem::StaleSource));
        let mut changed = snapshot.clone();
        changed.directories[0].entries.push(MetadataDirectoryEntry {
            name: "new.xml".into(),
            kind: MetadataDirectoryEntryKind::File,
        });
        assert!(validate_metadata_plan_snapshot(&plan, &changed)
            .contains(&MetadataPlanProblem::StaleDirectory));
        let mut changed = snapshot;
        changed.context_sha256 = [2; 32];
        assert!(validate_metadata_plan_snapshot(&plan, &changed)
            .contains(&MetadataPlanProblem::StaleContext));
    }
}
