use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use bsl_metadata::rename_xml::metadata_source_fingerprint;
use ide::metadata_rename_plan::{MetadataPlanRoot, MetadataRootRole};
use project_model::{ManagedBaselineDirectory, Project};
use serde_json::json;

pub(crate) struct MetadataProjectContext {
    pub project: Project,
    pub roots: Vec<MetadataPlanRoot>,
    pub inputs: Vec<MetadataContextInput>,
    pub context_sha256: [u8; 32],
}

#[derive(Clone, Debug)]
pub(crate) struct MetadataContextInput {
    pub path: PathBuf,
    pub canonical_path: Option<PathBuf>,
    pub text_sha256: Option<[u8; 32]>,
    pub case_sensitive: bool,
    snapshot: serde_json::Value,
}

pub(crate) fn capture_project_context(project: &Project) -> Result<MetadataProjectContext> {
    if !project.root.is_absolute() {
        bail!("metadata rename requires an absolute project root");
    }
    project.root.to_str().context("metadata project path is not Unicode")?;
    let mut roots = Vec::new();
    for path in project.source_roots() {
        let role = if project.configuration_path() == Some(path.as_path()) {
            MetadataRootRole::Main
        } else if project.extension_paths().iter().any(|(_, root)| root == &path) {
            MetadataRootRole::Extension
        } else if project.external_paths().iter().any(|(_, root)| root == &path) {
            MetadataRootRole::External
        } else {
            bail!("metadata rename requires an identified configuration or external root");
        };
        roots.push(capture_root(path, role)?);
    }
    for path in project.semantic_dependency_roots() {
        roots.push(capture_root(path, MetadataRootRole::Dependency)?);
    }
    if roots.is_empty() {
        bail!("metadata rename has no source roots");
    }
    roots.sort_by(|left, right| left.id.cmp(&right.id));
    for (index, root) in roots.iter().enumerate() {
        if roots[..index].iter().any(|other| {
            root.id == other.id
                || root.canonical_path.starts_with(&other.canonical_path)
                || other.canonical_path.starts_with(&root.canonical_path)
        }) {
            bail!("metadata source roots overlap or address the same directory");
        }
    }
    let required: BTreeSet<_> = project
        .config
        .config_file_path()
        .map(Path::to_path_buf)
        .into_iter()
        .chain(project.config.extends_chain().iter().cloned())
        .collect();
    let inputs: BTreeSet<_> = project_model::PROJECT_INPUT_FILE_NAMES
        .iter()
        .map(|name| project.root.join(name))
        .chain(required.iter().cloned())
        .collect();
    let inputs = inputs
        .iter()
        .map(|path| capture_input(path, required.contains(path)))
        .collect::<Result<Vec<_>>>()?;
    let roots_json: Vec<_> = roots
        .iter()
        .map(|root| {
            json!({
                "id": root.id, "source": root.source_path, "canonical": root.canonical_path,
                "role": match root.role {
                    MetadataRootRole::Main => "main",
                    MetadataRootRole::Extension => "extension",
                    MetadataRootRole::Dependency => "dependency",
                    MetadataRootRole::External => "external",
                },
                "writable": root.writable, "caseSensitive": root.case_sensitive,
            })
        })
        .collect();
    let config = &project.config;
    let context = json!({
        "schema": 1, "project": project.root, "roots": roots_json,
        "inputs": inputs.iter().map(|input| &input.snapshot).collect::<Vec<_>>(),
        "compilation": project.compilation_fingerprint(),
        "sourceAnchor": config.source_anchor, "configurationRoot": config.configuration_root,
        "configurationDependency": config.configuration_dependency,
        "exclude": config.source_exclude, "excludeGlobs": config.source_exclude_globs,
        "targetPlatform": config.target_platform_version, "minPlatform": config.min_platform_version,
        "compatibility": config.compatibility_mode, "typeNarrowing": config.features.type_narrowing,
        "checkedEnvironments": config.features.checked_environments,
    });
    Ok(MetadataProjectContext {
        project: project.clone(),
        roots,
        inputs,
        context_sha256: metadata_source_fingerprint(&serde_json::to_vec(&context)?),
    })
}

fn capture_root(source_path: PathBuf, role: MetadataRootRole) -> Result<MetadataPlanRoot> {
    source_path.to_str().context("metadata source path is not Unicode")?;
    let canonical_path = std::fs::canonicalize(&source_path)?;
    let case_sensitive = capture_case_sensitivity(&canonical_path)?;
    let canonical_name = canonical_path.to_str().context("metadata source path is not Unicode")?;
    let identity = if case_sensitive {
        canonical_name.to_owned()
    } else {
        canonical_name.chars().flat_map(char::to_lowercase).collect()
    };
    let id = metadata_source_fingerprint(identity.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if std::fs::canonicalize(&source_path)? != canonical_path {
        bail!("metadata source root binding changed");
    }
    let writable = role != MetadataRootRole::Dependency
        && !std::fs::metadata(&canonical_path)?.permissions().readonly();
    Ok(MetadataPlanRoot { id, source_path, canonical_path, role, writable, case_sensitive })
}

fn capture_case_sensitivity(canonical_path: &Path) -> Result<bool> {
    let directory = ManagedBaselineDirectory::open_project_root(canonical_path)?;
    let entries = directory.read_directory("")?;
    let mut case_sensitive = None;
    for entry in &entries {
        let Some(name) = entry.name.to_str() else {
            bail!("metadata source path is not Unicode");
        };
        let Some((index, letter)) = name.char_indices().find(|(_, ch)| ch.is_ascii_alphabetic())
        else {
            continue;
        };
        let mut alternate = name.to_owned();
        alternate.replace_range(index..index + 1, &char::from(letter as u8 ^ 32).to_string());
        if entries.iter().any(|candidate| candidate.name == std::ffi::OsStr::new(&alternate)) {
            continue;
        }
        case_sensitive = Some(match std::fs::symlink_metadata(canonical_path.join(&alternate)) {
            Ok(_) => false,
            Err(error) if error.kind() == ErrorKind::NotFound => true,
            Err(error) => return Err(error.into()),
        });
        break;
    }
    case_sensitive.context("cannot establish source path case semantics")
}

fn capture_input(path: &Path, required: bool) -> Result<MetadataContextInput> {
    path.to_str().context("metadata project input path is not Unicode")?;
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error)
            if !required
                && error.kind() == ErrorKind::NotFound
                && std::fs::symlink_metadata(path)
                    .is_err_and(|error| error.kind() == ErrorKind::NotFound) =>
        {
            let parent =
                path.parent().context("metadata context input has no parent")?.canonicalize()?;
            let case_sensitive = capture_case_sensitivity(&parent)?;
            return Ok(MetadataContextInput {
                path: path.to_path_buf(),
                canonical_path: None,
                text_sha256: None,
                case_sensitive,
                snapshot: json!({"path": path, "missing": true, "caseSensitive": case_sensitive}),
            });
        }
        Err(error) => return Err(error.into()),
    };
    canonical.to_str().context("metadata project input path is not Unicode")?;
    if !std::fs::metadata(&canonical)?.is_file() {
        bail!("metadata project input is not a regular file");
    }
    let bytes = std::fs::read(&canonical)?;
    let fingerprint = metadata_source_fingerprint(&bytes);
    let text_sha256 = base_db::decode_disk_bytes(&bytes)
        .map(|text| ide::metadata_rename_plan::metadata_document_fingerprint(&text));
    let case_sensitive = capture_case_sensitivity(
        canonical.parent().context("metadata context input has no parent")?,
    )?;
    if std::fs::canonicalize(path)? != canonical {
        bail!("metadata project input binding changed");
    }
    let snapshot = json!({"path": path, "canonical": canonical, "sha256": fingerprint,
        "caseSensitive": case_sensitive});
    Ok(MetadataContextInput {
        path: path.to_path_buf(),
        canonical_path: Some(canonical),
        text_sha256,
        case_sensitive,
        snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use project_model::ProjectConfig;

    fn configuration(path: &Path, name: &str, extension: bool) {
        std::fs::create_dir_all(path).unwrap();
        let purpose = if extension {
            "<ConfigurationExtensionPurpose>Customization</ConfigurationExtensionPurpose>"
        } else {
            ""
        };
        std::fs::write(
            path.join("Configuration.xml"),
            format!("<MetaDataObject><Configuration uuid=\"00000000-0000-0000-0000-000000000001\"><Properties><Name>{name}</Name>{purpose}</Properties></Configuration></MetaDataObject>"),
        )
        .unwrap();
    }

    fn project(path: &Path) -> Project {
        configuration(&path.join("cf"), "Main", false);
        Project::with_config(
            path,
            ProjectConfig {
                configuration_root: Some("cf".into()),
                extensions: Some(Vec::new()),
                externals: Some(Vec::new()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn unchanged_context_is_stable_and_effective_settings_are_bound() {
        let temporary = tempfile::tempdir().unwrap();
        let mut project = project(temporary.path());
        let first = capture_project_context(&project).unwrap();
        assert_eq!(first.context_sha256, capture_project_context(&project).unwrap().context_sha256);
        assert_eq!(first.roots.len(), 1);
        assert_eq!(first.roots[0].role, MetadataRootRole::Main);
        project.config.compatibility_mode = Some("Version8_2_16".into());
        assert_ne!(first.context_sha256, capture_project_context(&project).unwrap().context_sha256);
    }

    #[test]
    fn raw_config_extends_and_missing_input_presence_change_the_context() {
        let temporary = tempfile::tempdir().unwrap();
        let mut project = project(temporary.path());
        let config = temporary.path().join("bsl-analyzer.toml");
        let parent = temporary.path().join("shared.toml");
        std::fs::write(&config, "[source]\nroot = 'cf'\n").unwrap();
        std::fs::write(&parent, "# original\n").unwrap();
        project.config.config_file_path = Some(config.clone());
        project.config.extends_chain = vec![parent.clone()];
        let original = capture_project_context(&project).unwrap().context_sha256;
        std::fs::write(&parent, "# changed without reloading the project\n").unwrap();
        let parent_changed = capture_project_context(&project).unwrap().context_sha256;
        assert_ne!(original, parent_changed);
        std::fs::write(temporary.path().join(".bsl-analyzer.json"), "{}").unwrap();
        let input_created = capture_project_context(&project).unwrap().context_sha256;
        assert_ne!(parent_changed, input_created);
        std::fs::write(&config, "# changed\n[source]\nroot = 'cf'\n").unwrap();
        assert_ne!(input_created, capture_project_context(&project).unwrap().context_sha256);
    }

    #[test]
    fn project_environment_bytes_are_hashed_without_retaining_their_contents() {
        let temporary = tempfile::tempdir().unwrap();
        let project = project(temporary.path());
        let path = temporary.path().join(project_model::PROJECT_ENV_FILE_NAME);
        std::fs::write(&path, "TEST_VALUE=first\n").unwrap();
        let first = capture_project_context(&project).unwrap();
        std::fs::write(&path, "TEST_VALUE=second\n").unwrap();
        assert_ne!(first.context_sha256, capture_project_context(&project).unwrap().context_sha256);
    }

    #[test]
    fn explicit_external_owner_is_a_readonly_dependency_not_an_editable_subject() {
        let temporary = tempfile::tempdir().unwrap();
        configuration(&temporary.path().join("cf"), "Main", false);
        configuration(&temporary.path().join("owner"), "Owner", false);
        configuration(&temporary.path().join("extension"), "Extension", true);
        let external = temporary.path().join("external");
        std::fs::create_dir_all(external.join("Tool/Ext")).unwrap();
        std::fs::write(external.join("Tool.xml"), "<MetaDataObject><ExternalDataProcessor uuid=\"00000000-0000-0000-0000-000000000002\"><Properties><Name>Tool</Name></Properties></ExternalDataProcessor></MetaDataObject>").unwrap();
        let config: ProjectConfig = serde_json::from_value(serde_json::json!({
            "configurationRoot": "cf", "extensions": ["extension"],
            "externals": [{"name": "Tool", "path": "external", "configuration_root": "owner"}]
        }))
        .unwrap();
        let project = Project::with_config(temporary.path(), config).unwrap();
        let context = capture_project_context(&project).unwrap();
        assert_eq!(context.roots.len(), 4);
        for role in [
            MetadataRootRole::Main,
            MetadataRootRole::Extension,
            MetadataRootRole::External,
            MetadataRootRole::Dependency,
        ] {
            let root = context.roots.iter().find(|root| root.role == role).unwrap();
            assert_eq!(root.writable, role != MetadataRootRole::Dependency);
        }
        assert!(!project.source_roots().contains(&temporary.path().join("owner")));
    }

    #[cfg(unix)]
    #[test]
    fn changing_a_config_symlink_binding_is_stale_even_with_identical_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let mut project = project(temporary.path());
        let first = temporary.path().join("first.toml");
        let second = temporary.path().join("second.toml");
        let link = temporary.path().join("parent.toml");
        std::fs::write(&first, "# same\n").unwrap();
        std::fs::write(&second, "# same\n").unwrap();
        std::os::unix::fs::symlink(&first, &link).unwrap();
        project.config.extends_chain.push(link.clone());
        let before = capture_project_context(&project).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&second, &link).unwrap();
        assert_ne!(
            before.context_sha256,
            capture_project_context(&project).unwrap().context_sha256
        );
    }

    #[test]
    fn a_directory_at_a_config_input_is_an_error() {
        let temporary = tempfile::tempdir().unwrap();
        let project = project(temporary.path());
        std::fs::create_dir(temporary.path().join(".bsl-analyzer.json")).unwrap();
        assert!(capture_project_context(&project).is_err());
    }

    #[test]
    fn a_missing_previously_loaded_parent_config_is_refused() {
        let temporary = tempfile::tempdir().unwrap();
        let mut project = project(temporary.path());
        project.config.extends_chain.push(temporary.path().join("missing-parent.toml"));
        assert!(capture_project_context(&project).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_config_input_is_refused_without_serialization_panic() {
        use std::os::unix::ffi::OsStringExt;
        let temporary = tempfile::tempdir().unwrap();
        let mut project = project(temporary.path());
        project
            .config
            .extends_chain
            .push(temporary.path().join(std::ffi::OsString::from_vec(vec![255])));
        assert!(capture_project_context(&project).is_err());
    }
}
