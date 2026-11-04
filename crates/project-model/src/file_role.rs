use std::path::Path;

pub const SOURCE_EXTENSIONS: &[&str] = &[bsl_conventions::BSL_EXTENSION];

/// The ordinary-form sidecar (`Ext/Form/Form.json`) is JSON, so JSON is watched too -
/// but only that file counts as metadata (see [`is_form_sidecar_path`]); any other
/// JSON under a source root is ignored.
pub const FORM_SIDECAR_EXTENSION: &str = "json";

// The binary dialog's mtime determines whether its JSON description is usable.
pub const METADATA_WATCHED_EXTENSIONS: &[&str] =
    &[bsl_conventions::XML_EXTENSION, FORM_SIDECAR_EXTENSION, bsl_conventions::BIN_EXTENSION];

/// Whether `path` is an ordinary form's dialog sidecar - `.../Ext/Form/Form.json`
/// next to the module. The analyzer reads the form's controls, attributes and
/// bindings from it, so a change is a metadata change of that form module.
pub fn is_form_sidecar_path(path: &Path) -> bool {
    use bsl_conventions::{conventional_of, ConventionalName};
    let Some(file) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if conventional_of(file) != Some(ConventionalName::FormJson) {
        return false;
    }
    let Some(form_dir) = path.parent() else { return false };
    let form_name = form_dir.file_name().and_then(|n| n.to_str());
    let ext_name = form_dir.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str());
    form_name.is_some_and(|n| conventional_of(n) == Some(ConventionalName::Form))
        && ext_name.is_some_and(|n| conventional_of(n) == Some(ConventionalName::Ext))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRole {
    Source,
    MetadataWatched,
    Ignored,
}

pub fn file_role(path: &Path) -> FileRole {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return FileRole::Ignored;
    };
    if SOURCE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
        if is_derived_queries_extract(path) {
            FileRole::Ignored
        } else {
            FileRole::Source
        }
    } else if ext.eq_ignore_ascii_case(FORM_SIDECAR_EXTENSION) {
        if is_form_sidecar_path(path) {
            FileRole::MetadataWatched
        } else {
            FileRole::Ignored
        }
    } else if ext.eq_ignore_ascii_case(bsl_conventions::BIN_EXTENSION) {
        use bsl_conventions::{conventional_of, ConventionalName};
        let file = path.file_name().and_then(|n| n.to_str());
        let parent = path.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str());
        if file.is_some_and(|n| conventional_of(n) == Some(ConventionalName::FormBin))
            && parent.is_some_and(|n| conventional_of(n) == Some(ConventionalName::Ext))
        {
            FileRole::MetadataWatched
        } else {
            FileRole::Ignored
        }
    } else if ext.eq_ignore_ascii_case(bsl_conventions::XML_EXTENSION) {
        FileRole::MetadataWatched
    } else {
        FileRole::Ignored
    }
}

/// `Ext/Template.Queries.bsl` and `Ext/Form.Queries.bsl` are query extracts that indexing
/// tools write next to the metadata; the real query lives in the XML, nothing compiles
/// these files, and parsing them as BSL only yields `ParseError` noise.
fn is_derived_queries_extract(path: &Path) -> bool {
    let Some(stem) = path.file_stem().and_then(|n| n.to_str()) else {
        return false;
    };
    stem.rsplit_once('.').is_some_and(|(_, tail)| tail.eq_ignore_ascii_case("Queries"))
}

pub fn is_bsl_source_path(path: &Path) -> bool {
    matches!(file_role(path), FileRole::Source)
}

pub fn is_metadata_path(path: &Path) -> bool {
    matches!(file_role(path), FileRole::MetadataWatched)
}

/// Whether `path` is a common module's body source — the
/// `…/CommonModules/<Name>/Ext/Module.bsl` layout the designer export uses.
///
/// Such a file is ordinary BSL source (so it flows through the source-change path,
/// not the metadata-XML one), yet creating or deleting it changes a common module's
/// per-MDO structure listing (its `module_file` reverse-index entry). Callers use
/// this to also refresh the metadata substrate on a common-module body add/remove.
pub fn is_common_module_body_path(path: &Path) -> bool {
    substrate_listed_family(path).is_some_and(|f| f.eq_ignore_ascii_case("CommonModules"))
}

/// Whether `path` is a module body whose creation or deletion changes a per-MDO
/// structure listing's `module_file` reverse-index entry: common modules and the three
/// service families. Their bodies are ordinary BSL source (they flow through the
/// source-change path, not the metadata-XML one), yet the substrate stores a back-link
/// to the body's `FileId`, so a structural body change must also refresh the substrate.
/// Other module kinds (object/manager/form/command modules) resolve their owner through
/// the directory layout at query time and need no listing refresh.
pub fn is_substrate_listed_body_path(path: &Path) -> bool {
    substrate_listed_family(path).is_some_and(|family| {
        ["CommonModules", "HTTPServices", "WebServices", "IntegrationServices"]
            .iter()
            .any(|f| family.eq_ignore_ascii_case(f))
    })
}

/// The `<Family>` directory name for a `<Family>/<Name>/Ext/Module.bsl` layout, if
/// `path` matches it.
fn substrate_listed_family(path: &Path) -> Option<&str> {
    use bsl_conventions::{conventional_of, ConventionalName};
    let file = path.file_name().and_then(|n| n.to_str())?;
    if conventional_of(file) != Some(ConventionalName::Module) {
        return None;
    }
    let ext_dir = path.parent()?;
    let ext = ext_dir.file_name().and_then(|n| n.to_str())?;
    if conventional_of(ext) != Some(ConventionalName::Ext) {
        return None;
    }
    ext_dir.parent()?.parent()?.file_name().and_then(|n| n.to_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_ordinary_form_sidecar_json_is_metadata() {
        let sidecar = Path::new("cf/Catalogs/Справочник/Forms/Ф/Ext/Form/Form.json");
        assert_eq!(file_role(sidecar), FileRole::MetadataWatched);
        assert!(is_form_sidecar_path(sidecar));
        assert!(is_form_sidecar_path(Path::new("x/Forms/Ф/ext/form/FORM.JSON")));
        assert_eq!(file_role(Path::new("cf/package.json")), FileRole::Ignored);
        assert_eq!(file_role(Path::new("cf/Catalogs/Справочник/Form.json")), FileRole::Ignored);
        assert!(!is_form_sidecar_path(Path::new("cf/Catalogs/Справочник/Forms/Ф/Ext/Form.json")));
        assert!(!is_form_sidecar_path(Path::new("cf/CommonModules/Библиотека/Form/Form.json")));
    }

    #[test]
    fn binary_dialog_changes_invalidate_sidecar_metadata() {
        let dialog = Path::new("cf/Catalogs/C/Forms/F/Ext/Form.bin");
        assert_eq!(file_role(dialog), FileRole::MetadataWatched);
        assert!(METADATA_WATCHED_EXTENSIONS.contains(&"bin"));
        assert_eq!(
            file_role(Path::new("cf/Catalogs/C/Forms/F/ext/FORM.BIN")),
            FileRole::MetadataWatched
        );
        assert_eq!(file_role(Path::new("cf/Other/Form.bin")), FileRole::Ignored);
        assert_eq!(file_role(Path::new("cf/Other/Ext/Other.bin")), FileRole::Ignored);
    }

    #[test]
    fn sidecar_requires_the_ext_directory() {
        assert_eq!(file_role(Path::new("cf/Other/Form/Form.json")), FileRole::Ignored);
    }

    #[test]
    fn a_case_variant_body_is_still_substrate_listed() {
        assert!(is_substrate_listed_body_path(Path::new("/ws/CommonModules/X/EXT/MODULE.BSL")));
        assert!(is_common_module_body_path(Path::new("/ws/CommonModules/X/EXT/MODULE.BSL")));
    }

    #[test]
    fn substrate_listed_bodies_cover_common_modules_and_services() {
        assert!(is_substrate_listed_body_path(Path::new("/ws/CommonModules/X/Ext/Module.bsl")));
        assert!(is_substrate_listed_body_path(Path::new("/ws/HTTPServices/S/Ext/Module.bsl")));
        assert!(is_substrate_listed_body_path(Path::new("/ws/WebServices/S/Ext/Module.bsl")));
        assert!(is_substrate_listed_body_path(Path::new(
            "/ws/IntegrationServices/S/Ext/Module.bsl"
        )));
        // Object/manager/form modules resolve their owner by layout at query time.
        assert!(!is_substrate_listed_body_path(Path::new("/ws/Catalogs/C/Ext/ObjectModule.bsl")));
        assert!(!is_substrate_listed_body_path(Path::new(
            "/ws/Catalogs/C/Forms/F/Ext/Form/Module.bsl"
        )));
        assert!(!is_substrate_listed_body_path(Path::new("/ws/Documents/D/Ext/Module.bsl")));
    }

    #[test]
    fn classifies_bsl_as_source() {
        assert_eq!(file_role(Path::new("/ws/CommonModules/X/Ext/Module.bsl")), FileRole::Source);
        assert!(is_bsl_source_path(Path::new("/ws/a.bsl")));
    }

    #[test]
    fn derived_queries_extracts_are_ignored() {
        for path in [
            "/ws/Reports/R/Templates/T/Ext/Template.Queries.bsl",
            "/ws/Catalogs/C/Forms/F/Ext/Form.Queries.bsl",
            "/ws/Ext/template.queries.BSL",
        ] {
            assert_eq!(file_role(Path::new(path)), FileRole::Ignored, "{path}");
            assert!(!is_bsl_source_path(Path::new(path)), "{path}");
        }
        assert_eq!(file_role(Path::new("/ws/Catalogs/C/Ext/Module.bsl")), FileRole::Source);
        // A directory named `Queries` does not disqualify a real module.
        assert_eq!(file_role(Path::new("/ws/Queries/Ext/Module.bsl")), FileRole::Source);
        assert_eq!(file_role(Path::new("/ws/Queries.bsl/Module.bsl")), FileRole::Source);
        // Not a BSL file at all: guards against matching by substring.
        assert_eq!(file_role(Path::new("/ws/Queries.bsl.txt")), FileRole::Ignored);
        // A bare `Queries.bsl` has no `.Queries` suffix component; it is an ordinary module.
        assert_eq!(file_role(Path::new("/ws/Queries.bsl")), FileRole::Source);
    }

    #[test]
    fn classifies_xml_as_metadata_watched() {
        assert_eq!(file_role(Path::new("/ws/Configuration.xml")), FileRole::MetadataWatched,);
        assert!(is_metadata_path(Path::new("/ws/Roles/R/Ext/Rights.xml")));
        assert!(!is_bsl_source_path(Path::new("/ws/Roles/R/Ext/Rights.xml")));
    }

    #[test]
    fn classifies_other_extensions_as_ignored() {
        for path in &[
            "/ws/README.md",
            "/ws/notes.txt",
            "/ws/scripts/build.sh",
            "/ws/.bsl-analyzer.json",
            "/ws/binary.dat",
        ] {
            assert_eq!(file_role(Path::new(path)), FileRole::Ignored, "{path}");
            assert!(!is_bsl_source_path(Path::new(path)), "{path}");
            assert!(!is_metadata_path(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn classifies_paths_without_extension_as_ignored() {
        assert_eq!(file_role(Path::new("/ws/NoExtensionAtAll")), FileRole::Ignored);
        assert!(!is_bsl_source_path(Path::new("/ws/NoExtensionAtAll")));
    }

    #[test]
    fn extension_match_is_case_insensitive() {
        assert_eq!(file_role(Path::new("/ws/M.BSL")), FileRole::Source);
        assert_eq!(file_role(Path::new("/ws/M.Bsl")), FileRole::Source);
        assert_eq!(file_role(Path::new("/ws/Form.XML")), FileRole::MetadataWatched);
        assert_eq!(file_role(Path::new("/ws/Form.Xml")), FileRole::MetadataWatched);
    }

    #[test]
    fn recognizes_common_module_body_layout() {
        assert!(is_common_module_body_path(Path::new(
            "/ws/cf/CommonModules/ОбщегоНазначения/Ext/Module.bsl"
        )));
        // Extension roots use the same layout.
        assert!(is_common_module_body_path(Path::new(
            "/ws/cfe/A/CommonModules/Расш/Ext/Module.bsl"
        )));
        // Not a common module body: object module, wrong filename, or wrong nesting.
        assert!(!is_common_module_body_path(Path::new(
            "/ws/cf/Catalogs/Товары/Ext/ManagerModule.bsl"
        )));
        assert!(!is_common_module_body_path(Path::new("/ws/cf/CommonModules/М/Ext/Form.bsl")));
        assert!(!is_common_module_body_path(Path::new("/ws/cf/CommonModules/Module.bsl")));
        assert!(!is_common_module_body_path(Path::new("/ws/cf/Reports/CommonModules.bsl")));
    }

    #[test]
    fn extension_lists_have_no_overlap() {
        for src in SOURCE_EXTENSIONS {
            assert!(
                !METADATA_WATCHED_EXTENSIONS.iter().any(|m| m.eq_ignore_ascii_case(src)),
                "{src} appears in both source and metadata extension lists",
            );
        }
    }
}
