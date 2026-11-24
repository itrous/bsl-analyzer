//! A compilation configuration selected for one external object export.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::configuration_xml_in;

/// A configuration path in the same configured/canonical address spaces as source roots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerConfigurationRoot {
    pub path: PathBuf,
    pub canonical_path: PathBuf,
}

/// How an external object obtained the configuration used to analyse its host-dependent code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalConfigurationOwner {
    /// The old project-wide analysis context. This does not prove the runtime owner.
    LegacyProjectDefault(OwnerConfigurationRoot),
    /// A declared and validated main configuration, separate from the designer topology.
    Selected(OwnerConfigurationRoot),
    /// No configuration was selected by the project or the external declaration.
    Unknown,
}

impl ExternalConfigurationOwner {
    pub fn compilation_root(&self) -> Option<&OwnerConfigurationRoot> {
        match self {
            Self::LegacyProjectDefault(root) | Self::Selected(root) => Some(root),
            Self::Unknown => None,
        }
    }

    /// Host metadata identities require an explicit declaration, not the legacy analysis default.
    pub fn proven_root(&self) -> Option<&OwnerConfigurationRoot> {
        match self {
            Self::Selected(root) => Some(root),
            Self::LegacyProjectDefault(_) | Self::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalOwnerError {
    EmptyPath,
    ConfigurationNotReadable(PathBuf),
    InvalidConfigurationHeader(PathBuf),
    ConfigurationExtension(PathBuf),
}

impl fmt::Display for ExternalOwnerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPath => formatter.write_str("configuration_root must not be empty"),
            Self::ConfigurationNotReadable(path) => {
                write!(formatter, "no readable Configuration.xml under {}", path.display())
            }
            Self::InvalidConfigurationHeader(path) => {
                write!(formatter, "{} is not a main configuration header", path.display())
            }
            Self::ConfigurationExtension(path) => write!(
                formatter,
                "{} is a configuration extension, not a host configuration",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ExternalOwnerError {}

impl crate::Project {
    /// Additional main configurations whose sources supply semantics to external exports.
    /// These roots are watched and prepared independently of diagnostic subjects.
    pub fn semantic_dependency_roots(&self) -> Vec<PathBuf> {
        let main = self.configuration_path().and_then(|path| std::fs::canonicalize(path).ok());
        let mut canonical = std::collections::HashSet::new();
        self.external_owners
            .iter()
            .filter_map(|(_, owner)| {
                let root = owner.proven_root()?;
                if main.as_ref() == Some(&root.canonical_path)
                    || !canonical.insert(root.canonical_path.clone())
                {
                    return None;
                }
                Some(root.path.clone())
            })
            .collect()
    }

    /// Topology and explicit compilation choices, preserving the legacy identity when absent.
    pub fn compilation_fingerprint(&self) -> String {
        if !self.external_owners.iter().any(|(_, owner)| owner.proven_root().is_some()) {
            return self.topology.fingerprint().to_hex();
        }
        let mut hash = blake3::Hasher::new();
        hash.update(self.topology.fingerprint().as_bytes());
        hash.update(&self.compilation_context_fingerprint());
        for (_, owner) in &self.external_owners {
            if let Some(root) = owner.proven_root() {
                let canonical = root.canonical_path.as_os_str().as_encoded_bytes();
                hash.update(&(canonical.len() as u64).to_le_bytes());
                hash.update(canonical);
            }
        }
        hash.finalize().to_hex().to_string()
    }
}

/// Resolve a project-relative selection; an invalid explicit selection never uses the default.
pub fn resolve_external_configuration_owner(
    project_root: &Path,
    configured: Option<&str>,
    project_default: Option<&Path>,
) -> Result<ExternalConfigurationOwner, ExternalOwnerError> {
    let Some(configured) = configured else {
        return Ok(match project_default {
            Some(path) => {
                ExternalConfigurationOwner::LegacyProjectDefault(OwnerConfigurationRoot {
                    path: path.to_path_buf(),
                    canonical_path: std::fs::canonicalize(path)
                        .unwrap_or_else(|_| path.to_path_buf()),
                })
            }
            None => ExternalConfigurationOwner::Unknown,
        });
    };
    if configured.trim().is_empty() {
        return Err(ExternalOwnerError::EmptyPath);
    }
    let path = project_root.join(configured);
    let header = configuration_xml_in(&path)
        .ok_or_else(|| ExternalOwnerError::ConfigurationNotReadable(path.clone()))?;
    let text = std::fs::read_to_string(&header)
        .map_err(|_| ExternalOwnerError::ConfigurationNotReadable(path.clone()))?;
    let configuration = bsl_metadata::Configuration::from_xml_str(&text)
        .map_err(|_| ExternalOwnerError::InvalidConfigurationHeader(path.clone()))?;
    if configuration.is_extension() {
        return Err(ExternalOwnerError::ConfigurationExtension(path));
    }
    let canonical_path = std::fs::canonicalize(&path)
        .map_err(|_| ExternalOwnerError::ConfigurationNotReadable(path.clone()))?;
    Ok(ExternalConfigurationOwner::Selected(OwnerConfigurationRoot { path, canonical_path }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration(root: &Path, properties: &str) {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join("Configuration.xml"), format!("<MetaDataObject><Configuration uuid=\"00000000-0000-0000-0000-000000000001\"><Properties>{properties}</Properties></Configuration></MetaDataObject>")).unwrap();
    }

    #[test]
    fn a_selected_owner_uses_project_relative_paths_and_is_not_the_default() {
        let directory = tempfile::tempdir().unwrap();
        let legacy = directory.path().join("default");
        let selected = directory.path().join("owner");
        configuration(&legacy, "<Name>Modern</Name>");
        configuration(
            &selected,
            "<Name>Legacy</Name><CompatibilityMode>Version8_2_16</CompatibilityMode>",
        );
        let owner =
            resolve_external_configuration_owner(directory.path(), Some("owner"), Some(&legacy))
                .unwrap();
        assert_eq!(owner.proven_root().unwrap().path, selected);
        assert_ne!(owner.compilation_root().unwrap().path, legacy);
    }

    #[test]
    fn the_legacy_default_is_a_compilation_choice_without_ownership_proof() {
        let directory = tempfile::tempdir().unwrap();
        let owner =
            resolve_external_configuration_owner(directory.path(), None, Some(directory.path()))
                .unwrap();
        assert!(matches!(owner, ExternalConfigurationOwner::LegacyProjectDefault(_)));
        assert!(owner.compilation_root().is_some());
        assert!(owner.proven_root().is_none());
        assert_eq!(
            resolve_external_configuration_owner(directory.path(), None, None).unwrap(),
            ExternalConfigurationOwner::Unknown
        );
    }

    #[test]
    fn an_explicit_bad_owner_never_falls_back_to_the_project() {
        let directory = tempfile::tempdir().unwrap();
        configuration(directory.path(), "<Name>Default</Name>");
        for configured in ["", "missing"] {
            assert!(resolve_external_configuration_owner(
                directory.path(),
                Some(configured),
                Some(directory.path())
            )
            .is_err());
        }
        let extension = directory.path().join("extension");
        configuration(&extension, "<Name>Extension</Name><ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>");
        assert!(matches!(
            resolve_external_configuration_owner(
                directory.path(),
                Some("extension"),
                Some(directory.path())
            ),
            Err(ExternalOwnerError::ConfigurationExtension(_))
        ));
        let malformed = directory.path().join("malformed");
        std::fs::create_dir_all(&malformed).unwrap();
        std::fs::write(
            malformed.join("Configuration.xml"),
            "<MetaDataObject><Catalog/></MetaDataObject>",
        )
        .unwrap();
        assert!(matches!(
            resolve_external_configuration_owner(
                directory.path(),
                Some("malformed"),
                Some(directory.path())
            ),
            Err(ExternalOwnerError::InvalidConfigurationHeader(_))
        ));
    }
}
