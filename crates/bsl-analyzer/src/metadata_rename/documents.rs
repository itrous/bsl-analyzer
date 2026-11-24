use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, PathBuf};

use anyhow::{bail, Context, Result};
use bsl_metadata::rename_xml::metadata_source_fingerprint;
use ide::metadata_rename_plan::{MetadataOpenDocument, MetadataPlanRoot};
use lsp_types::Url;
use serde::{Deserialize, Serialize};

use super::context::MetadataContextInput;
use crate::mem_docs::FrozenMemDocs;

pub(crate) fn freeze_context_manifest(
    inputs: &[MetadataContextInput],
    reported: &[MetadataDocumentState],
) -> Result<BTreeMap<PathBuf, MetadataOpenDocument>> {
    let mut result = BTreeMap::new();
    for document in reported {
        let Ok(path) = document.uri.to_file_path() else { continue };
        let Some(input) = inputs.iter().find(|input| matches_context_input(input, &path)) else {
            continue;
        };
        let hash = parse_fingerprint(&document.text_sha256)?;
        let state = MetadataOpenDocument {
            version: document.version,
            document_text_sha256: hash,
            dirty: document.dirty || input.text_sha256 != Some(hash),
        };
        if result.insert(input.path.clone(), state).is_some() {
            bail!("multiple open documents address the same metadata context input");
        }
    }
    Ok(result)
}

pub(super) fn matches_context_input(input: &MetadataContextInput, path: &std::path::Path) -> bool {
    let equals = |left: &std::path::Path, right: &std::path::Path| {
        if input.case_sensitive {
            left == right
        } else {
            stdx::case::eq_ignore_case(&left.to_string_lossy(), &right.to_string_lossy())
        }
    };
    if equals(path, &input.path) {
        return true;
    }
    if let (Some(expected), Ok(actual)) = (&input.canonical_path, path.canonicalize()) {
        if equals(expected, &actual) {
            return true;
        }
    }
    let parents = input
        .path
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .zip(path.parent().and_then(|parent| parent.canonicalize().ok()));
    parents.is_some_and(|(expected, actual)| {
        equals(&expected, &actual)
            && input.path.file_name().zip(path.file_name()).is_some_and(|(expected, actual)| {
                equals(std::path::Path::new(expected), std::path::Path::new(actual))
            })
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct MetadataDocumentState {
    pub uri: Url,
    pub version: i32,
    pub text_sha256: String,
    pub dirty: bool,
}

pub(crate) fn freeze_document_manifest(
    roots: &[MetadataPlanRoot],
    server_documents: &FrozenMemDocs,
    reported: &[MetadataDocumentState],
) -> Result<BTreeMap<PathBuf, MetadataOpenDocument>> {
    let mut result = BTreeMap::new();
    for document in reported {
        let Some(path) = document_path(roots, &document.uri)? else { continue };
        let state = MetadataOpenDocument {
            version: document.version,
            document_text_sha256: parse_fingerprint(&document.text_sha256)?,
            dirty: document.dirty,
        };
        if result.insert(path, state).is_some() {
            bail!("multiple open documents address the same metadata source");
        }
    }
    let mut server_paths = BTreeSet::new();
    for (uri, document) in server_documents.iter() {
        let Some(path) = document_path(roots, uri)? else { continue };
        let reported = result.get(&path).context("client omitted an open BSL document")?;
        if !server_paths.insert(path)
            || reported.version != document.version()
            || reported.document_text_sha256
                != metadata_source_fingerprint(document.text().as_bytes())
        {
            bail!("client and server document versions or text differ");
        }
    }
    if result
        .keys()
        .any(|path| project_model::is_bsl_source_path(path) && !server_paths.contains(path))
    {
        bail!("an open BSL document has not been synchronized with the server");
    }
    Ok(result)
}

pub(super) fn document_path(roots: &[MetadataPlanRoot], uri: &Url) -> Result<Option<PathBuf>> {
    let Ok(path) = uri.to_file_path() else { return Ok(None) };
    let mut ancestor = path.as_path();
    let mut missing = Vec::new();
    let canonical = loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for part in missing.into_iter().rev() {
                    canonical.push(part);
                }
                break canonical;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = ancestor.file_name() else { return Err(error.into()) };
                missing.push(name.to_os_string());
                ancestor = ancestor.parent().context("metadata document path has no parent")?;
            }
            Err(error) => return Err(error.into()),
        }
    };
    for root in roots {
        if let Some(relative) =
            relative_document_path(&canonical, &root.canonical_path, root.case_sensitive)
        {
            if !relative.components().all(|component| matches!(component, Component::Normal(_))) {
                bail!("metadata document path contains non-relative components");
            }
            return Ok(Some(root.source_path.join(relative)));
        }
    }
    Ok(None)
}

fn relative_document_path(
    path: &std::path::Path,
    root: &std::path::Path,
    case_sensitive: bool,
) -> Option<PathBuf> {
    let mut remaining = path.components();
    for expected in root.components() {
        let actual = remaining.next()?;
        if if case_sensitive {
            actual != expected
        } else {
            !stdx::case::eq_ignore_case(
                &actual.as_os_str().to_string_lossy(),
                &expected.as_os_str().to_string_lossy(),
            )
        } {
            return None;
        }
    }
    Some(remaining.as_path().to_path_buf())
}

fn parse_fingerprint(text: &str) -> Result<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("document fingerprint must be a SHA-256 hex string");
    }
    let mut result = [0; 32];
    for (index, byte) in result.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_docs::MemDocs;
    use ide::metadata_rename_plan::MetadataRootRole;
    use std::path::Path;

    fn root(path: &Path) -> MetadataPlanRoot {
        MetadataPlanRoot {
            id: "main".into(),
            source_path: path.to_path_buf(),
            canonical_path: path.canonicalize().unwrap(),
            role: MetadataRootRole::Main,
            writable: true,
            case_sensitive: !cfg!(windows),
        }
    }

    fn state(path: &Path, text: &str, version: i32, dirty: bool) -> MetadataDocumentState {
        MetadataDocumentState {
            uri: Url::from_file_path(path).unwrap(),
            version,
            text_sha256: metadata_source_fingerprint(text.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            dirty,
        }
    }

    #[test]
    fn xml_versions_join_the_frozen_bsl_manifest_without_entering_bsl_mem_docs() {
        let temporary = tempfile::tempdir().unwrap();
        let bsl = state(&temporary.path().join("Module.bsl"), "A = 1;", 7, false);
        let xml = state(&temporary.path().join("Form.xml"), "<Form/>", 12, false);
        let mut server = MemDocs::new();
        server.insert(bsl.uri.clone(), "A = 1;".into(), 7);
        let frozen = server.freeze();
        let manifest =
            freeze_document_manifest(&[root(temporary.path())], &frozen, &[bsl, xml]).unwrap();
        assert_eq!(manifest.len(), 2);
        assert_eq!(manifest[&temporary.path().join("Form.xml")].version, 12);
        assert_eq!(frozen.len(), 1);
    }

    #[test]
    fn omitted_stale_or_unsynchronized_bsl_documents_are_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let mut bsl = state(&temporary.path().join("Module.bsl"), "A = 1;", 7, false);
        let roots = [root(temporary.path())];
        let mut server = MemDocs::new();
        server.insert(bsl.uri.clone(), "A = 1;".into(), 7);
        assert!(freeze_document_manifest(&roots, &server.freeze(), &[]).is_err());
        bsl.version = 8;
        assert!(freeze_document_manifest(&roots, &server.freeze(), &[bsl.clone()]).is_err());
        bsl.version = 7;
        bsl.text_sha256 = "0".repeat(64);
        assert!(freeze_document_manifest(&roots, &server.freeze(), &[bsl.clone()]).is_err());
        assert!(freeze_document_manifest(&roots, &MemDocs::new().freeze(), &[bsl]).is_err());
    }

    #[test]
    fn duplicate_inputs_and_malformed_fingerprints_are_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let mut xml = state(&temporary.path().join("Form.xml"), "<Form/>", 1, false);
        let roots = [root(temporary.path())];
        let server = MemDocs::new().freeze();
        assert!(freeze_document_manifest(&roots, &server, &[xml.clone(), xml.clone()]).is_err());
        xml.text_sha256 = "not a fingerprint".into();
        assert!(freeze_document_manifest(&roots, &server, &[xml]).is_err());
    }

    #[test]
    fn dirty_xml_is_preserved_for_the_common_snapshot_validator() {
        let temporary = tempfile::tempdir().unwrap();
        let xml = state(&temporary.path().join("Form.xml"), "<Form/>", 1, true);
        let manifest =
            freeze_document_manifest(&[root(temporary.path())], &MemDocs::new().freeze(), &[xml])
                .unwrap();
        assert!(manifest[&temporary.path().join("Form.xml")].dirty);
    }

    #[test]
    fn unrelated_documents_do_not_enter_the_source_snapshot() {
        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let xml = state(&outside.path().join("Form.xml"), "<Form/>", 1, false);
        let manifest =
            freeze_document_manifest(&[root(temporary.path())], &MemDocs::new().freeze(), &[xml])
                .unwrap();
        assert!(manifest.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn another_editor_alias_is_bound_to_the_same_source_and_cannot_duplicate_it() {
        let temporary = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = temporary.path().join("Form.xml");
        std::fs::write(&path, "<Form/>").unwrap();
        let alias = outside.path().join("alias");
        std::os::unix::fs::symlink(temporary.path(), &alias).unwrap();
        let direct = state(&path, "<Form/>", 1, false);
        let aliased = state(&alias.join("Form.xml"), "<Form/>", 1, false);
        let roots = [root(temporary.path())];
        let server = MemDocs::new().freeze();
        let manifest =
            freeze_document_manifest(&roots, &server, std::slice::from_ref(&aliased)).unwrap();
        assert!(manifest.contains_key(&path));
        assert!(freeze_document_manifest(&roots, &server, &[direct, aliased]).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn missing_paths_through_editor_aliases_are_bound_before_create_or_rename() {
        let temporary = tempfile::tempdir().unwrap();
        let alias_parent = tempfile::tempdir().unwrap();
        let alias = alias_parent.path().join("alias");
        std::os::unix::fs::symlink(temporary.path(), &alias).unwrap();
        let missing = alias.join("NewObject/Module.bsl");
        let roots = [root(temporary.path())];
        let uri = Url::from_file_path(&missing).unwrap();
        assert_eq!(
            document_path(&roots, &uri).unwrap(),
            Some(temporary.path().join("NewObject/Module.bsl"))
        );
    }

    #[test]
    fn case_insensitive_missing_path_uses_bound_root_case_semantics() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("Source");
        std::fs::create_dir(&source).unwrap();
        let mut bound = root(&source);
        bound.case_sensitive = false;
        let uri = Url::from_file_path(temporary.path().join("SOURCE/New.xml")).unwrap();
        assert_eq!(document_path(&[bound], &uri).unwrap(), Some(source.join("New.xml")));
    }
    #[cfg(unix)]
    #[test]
    fn alias_parent_cannot_reclassify_an_outside_target_as_a_source_document() {
        let source = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = source.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let uri = Url::from_file_path(link.join("Missing.xml")).unwrap();
        assert_eq!(document_path(&[root(source.path())], &uri).unwrap(), None);
    }
}
