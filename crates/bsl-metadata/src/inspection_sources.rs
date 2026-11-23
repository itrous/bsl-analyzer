//! Discovery of form/template declarations and readable content within a source root.

use std::path::{Path, PathBuf};

use bsl_conventions::{ConventionalName, DirTree, EntryKind, XML_EXTENSION};

const COLLECTIONS: &[&str] = &[
    "Catalogs",
    "Documents",
    "BusinessProcesses",
    "Tasks",
    "ExchangePlans",
    "ChartsOfAccounts",
    "ChartsOfCharacteristicTypes",
    "ChartsOfCalculationTypes",
    "DataProcessors",
    "Reports",
    "InformationRegisters",
    "AccumulationRegisters",
    "CalculationRegisters",
    "AccountingRegisters",
    "ExternalDataSources",
];

/// Descriptor identity is separate from its content and from a same-named base object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InspectionIdentity {
    pub uuid: uuid::Uuid,
    pub name: String,
    pub object_belonging: crate::ObjectBelonging,
    pub extends: Option<uuid::Uuid>,
    pub form_type: Option<String>,
    pub template_type: Option<String>,
}

impl InspectionIdentity {
    /// Read a descriptor using the standard default for absent ownership.
    pub fn from_xml(xml: &str, kind: &str) -> crate::Result<Self> {
        Self::read_xml(xml, kind, false)
    }

    /// A native extension form can omit ownership when it declares no adoption.
    pub fn from_extension_form_xml(xml: &str, kind: &str) -> crate::Result<Self> {
        Self::read_xml(xml, kind, true)
    }

    fn read_xml(xml: &str, kind: &str, extension_form: bool) -> crate::Result<Self> {
        use crate::xml_parser::helpers::{child_text, find_child, parse_extension_ownership};
        let document = roxmltree::Document::parse(xml.trim_start_matches('\u{feff}'))
            .map_err(|e| crate::MetadataError::InvalidFormat(e.to_string()))?;
        let root = document.root_element();
        if root.tag_name().name() != "MetaDataObject"
            || root.tag_name().namespace() != Some("http://v8.1c.ru/8.3/MDClasses")
        {
            return Err(crate::MetadataError::InvalidFormat("Expected metadata descriptor".into()));
        }
        let node = find_child(root, kind).ok_or_else(|| {
            crate::MetadataError::InvalidFormat(format!("Expected {kind} descriptor"))
        })?;
        let props = find_child(node, "Properties").ok_or_else(|| {
            crate::MetadataError::InvalidFormat("Descriptor properties unavailable".into())
        })?;
        let (mut object_belonging, extends) = parse_extension_ownership(props);
        if extension_form
            && object_belonging == crate::ObjectBelonging::Own
            && find_child(props, "ExtendedConfigurationObject").is_some()
        {
            object_belonging = crate::ObjectBelonging::Unknown;
        }
        Ok(Self {
            uuid: node.attribute("uuid").and_then(|s| s.parse().ok()).unwrap_or_default(),
            name: child_text(props, "Name").unwrap_or_default().into(),
            object_belonging,
            extends,
            form_type: child_text(props, "FormType").map(str::to_owned),
            template_type: child_text(props, "TemplateType").map(str::to_owned),
        })
    }

    pub fn adopts(&self, base: &Self) -> bool {
        let base_origin = match base.object_belonging {
            crate::ObjectBelonging::Adopted => base.extends.unwrap_or(base.uuid),
            _ => base.uuid,
        };
        self.object_belonging == crate::ObjectBelonging::Adopted
            && !base.uuid.is_nil()
            && !base_origin.is_nil()
            && stdx::case::eq_ignore_case(&self.name, &base.name)
            && self.extends == Some(base_origin)
    }
}

/// Only declared form/template containers are traversed; arbitrary subtrees are not.
pub fn discover(root: &Path, tree: &dyn DirTree) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for collection in COLLECTIONS {
        let Some(directory) = bsl_conventions::find_stored_child_ci_in(tree, root, collection)
        else {
            continue;
        };
        for object in tree.entries(&directory).into_iter().filter(|e| e.kind == EntryKind::Dir) {
            collect_container(
                &object.path,
                ConventionalName::Forms.canonical(),
                true,
                tree,
                &mut result,
            );
            collect_container(&object.path, "Templates", false, tree, &mut result);
        }
    }
    collect_container(root, "CommonForms", true, tree, &mut result);
    collect_container(root, "CommonTemplates", false, tree, &mut result);
    result.sort();
    result.dedup();
    result
}

fn collect_container(
    parent: &Path,
    name: &str,
    form: bool,
    tree: &dyn DirTree,
    result: &mut Vec<PathBuf>,
) {
    let Some(container) = bsl_conventions::find_stored_child_ci_in(tree, parent, name) else {
        return;
    };
    for entry in tree.entries(&container) {
        if entry.kind == EntryKind::File
            && bsl_conventions::has_extension(&entry.path, XML_EXTENSION)
        {
            result.push(entry.path);
        } else if entry.kind == EntryKind::Dir {
            let paths: &[&[&str]] = if form {
                &[
                    &[ConventionalName::Ext.canonical(), ConventionalName::FormXml.canonical()],
                    &[ConventionalName::Ext.canonical(), ConventionalName::FormBin.canonical()],
                    &[
                        ConventionalName::Ext.canonical(),
                        ConventionalName::Form.canonical(),
                        ConventionalName::FormJson.canonical(),
                    ],
                ]
            } else {
                &[
                    &[ConventionalName::Ext.canonical(), ConventionalName::TemplateXml.canonical()],
                    &[ConventionalName::Ext.canonical(), ConventionalName::TemplateBin.canonical()],
                ]
            };
            for parts in paths {
                if let Some(file) =
                    bsl_conventions::resolve_stored_chain_ci_in(tree, &entry.path, parts)
                {
                    result.push(file);
                }
            }
        }
    }
}

/// Does a changed path name one of the supported inputs, including a removed file?
pub fn is_input(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else { return false };
    let parts: Vec<_> = relative.iter().filter_map(|p| p.to_str()).collect();
    let tail = match parts.as_slice() {
        [collection, tail @ ..]
            if collection.eq_ignore_ascii_case("CommonForms")
                || collection.eq_ignore_ascii_case("CommonTemplates") =>
        {
            tail
        }
        [collection, _, container, tail @ ..]
            if COLLECTIONS.iter().any(|c| c.eq_ignore_ascii_case(collection))
                && (container.eq_ignore_ascii_case(ConventionalName::Forms.canonical())
                    || container.eq_ignore_ascii_case("Templates")) =>
        {
            tail
        }
        _ => return false,
    };
    match tail {
        [descriptor] => Path::new(descriptor)
            .extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case(XML_EXTENSION)),
        [_, ext, file] => {
            ext.eq_ignore_ascii_case(ConventionalName::Ext.canonical())
                && [
                    ConventionalName::FormXml.canonical(),
                    ConventionalName::FormBin.canonical(),
                    ConventionalName::TemplateXml.canonical(),
                    ConventionalName::TemplateBin.canonical(),
                ]
                .iter()
                .any(|name| file.eq_ignore_ascii_case(name))
        }
        [_, ext, form, file] => {
            ext.eq_ignore_ascii_case(ConventionalName::Ext.canonical())
                && form.eq_ignore_ascii_case(ConventionalName::Form.canonical())
                && file.eq_ignore_ascii_case(ConventionalName::FormJson.canonical())
        }
        _ => false,
    }
}

/// Binary inputs are enrolled as immutable fingerprints, never decoded as source text.
pub fn is_binary_fingerprint(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.to_string_lossy().eq_ignore_ascii_case(ConventionalName::FormBin.canonical())
            || name
                .to_string_lossy()
                .eq_ignore_ascii_case(ConventionalName::TemplateBin.canonical())
    })
}

/// Capture content identity and legacy sidecar freshness once in the input owner.
pub fn binary_fingerprint(path: &Path) -> std::io::Result<BinaryFingerprint> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)?;
    let sha256 = format!("{:x}", Sha256::digest(bytes));
    let mut stale = None;
    if path.file_name().is_some_and(|name| {
        name.to_string_lossy().eq_ignore_ascii_case(ConventionalName::FormBin.canonical())
    }) {
        if let Some(parent) = path.parent() {
            let sidecar = bsl_conventions::resolve_chain_ci(
                parent,
                &[ConventionalName::Form.canonical(), ConventionalName::FormJson.canonical()],
            );
            let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
            stale =
                sidecar.as_deref().and_then(|sidecar| Some(modified(path)? > modified(sidecar)?));
        }
    }
    Ok(BinaryFingerprint { sha256, legacy_sidecar_stale: stale })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BinaryFingerprint {
    pub sha256: String,
    pub legacy_sidecar_stale: Option<bool>,
}

impl BinaryFingerprint {
    pub fn estimated_heap_size(&self) -> usize {
        self.sha256.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_discovery_is_scoped_and_retains_stored_spelling() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        for file in [
            "documents/Order/forms/Dialog.xml",
            "documents/Order/forms/Dialog/EXT/FORM.XML",
            "documents/Order/forms/Legacy/Ext/Form.bin",
            "documents/Order/forms/Legacy/Ext/Form/Form.json",
            "documents/Order/templates/Print/Ext/Template.xml",
            "CommonForms/Shared.xml",
            "CommonForms/Shared/Ext/Form.xml",
            "Elsewhere/Forms/NotAForm/Ext/Form.xml",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let files = discover(root, &bsl_conventions::RealFs);
        assert_eq!(files.len(), 7);
        assert!(files.iter().all(|p| is_input(root, p)));
        assert!(is_input(root, &root.join("Documents/Removed/Forms/Gone/Ext/Form.xml")));
        assert!(!is_input(root, &root.join("Elsewhere/Forms/Gone/Ext/Form.xml")));
    }

    #[test]
    fn binary_fingerprint_changes_even_when_no_readable_form_exists() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("Form.bin");
        std::fs::write(&path, b"first").unwrap();
        let first = binary_fingerprint(&path).unwrap();
        std::fs::write(&path, b"second").unwrap();
        assert_ne!(first, binary_fingerprint(&path).unwrap());
        assert_eq!(first.sha256.len(), 64);
    }
}
