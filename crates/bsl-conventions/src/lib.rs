//! The single policy point for the configurator dump's conventional names.
//!
//! A configurator dump spells its structural vocabulary — extensions, service
//! directories, module file names, service XML names — in a fixed canonical
//! form, but tools that produced a tree may have written any ASCII case. Every
//! comparison of a conventional name in the workspace therefore goes through
//! this crate: ASCII-case-insensitive, with the canonical spelling as the one
//! source of truth for construction. Object NAMES (the segment an author chose)
//! are never compared case-insensitively — that distinction is positional, so
//! the caller decides which segments are conventional, never this crate from
//! the spelling alone (an object may legally be named `Ext`).
//!
//! Collection directory names (`Catalogs`, `Отчёты`, …) are NOT here: they are
//! bilingual and their equivalence — including `ё` variants and Unicode
//! composition — is owned by `bsl_metadata`'s module-path spec.

mod probe;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod tree;

pub use probe::{
    find_child_ci, find_child_stem_exact, find_child_stem_exact_in, find_listed_child_stem_exact,
    find_stored_child_ci_in, resolve_chain_ci, resolve_stored_chain_ci_in,
};
pub use tree::{DirTree, EntryKind, PathSetTree, RealFs, TreeEntry};

/// Every conventional name the dump layout uses and the workspace compares.
///
/// The variants are the dictionary: the completeness gate and the table tests
/// both iterate [`ConventionalName::ALL`], so a name missing here is missing
/// everywhere visibly, not silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConventionalName {
    // Module file names.
    Module,
    ObjectModule,
    ManagerModule,
    FormModule,
    CommandModule,
    RecordSetModule,
    ValueManagerModule,
    SessionModule,
    ExternalConnectionModule,
    ManagedApplicationModule,
    OrdinaryApplicationModule,
    ApplicationModule,
    /// The protected-module container next to a missing `Module.bsl`.
    ModuleBin,
    // Service directories.
    Ext,
    Form,
    Forms,
    Commands,
    // Service XML files.
    ConfigurationXml,
    PredefinedXml,
    RightsXml,
    FormXml,
    /// The optional description of an ORDINARY form's dialog (`Ext/Form/Form.json`):
    /// the dialog itself is the binary `Ext/Form.bin`, so a tool that can read it may
    /// leave this JSON next to the module (`docs/configuration/ordinary-form-sidecar.md`).
    FormJson,
    /// The binary dialog of an ORDINARY form (`Ext/Form.bin`), next to `Ext/Form/`.
    FormBin,
    /// The vendor support map under the configuration's `Ext`.
    ParentConfigurationsBin,
}

impl ConventionalName {
    pub const ALL: &'static [ConventionalName] = &[
        ConventionalName::Module,
        ConventionalName::ObjectModule,
        ConventionalName::ManagerModule,
        ConventionalName::FormModule,
        ConventionalName::CommandModule,
        ConventionalName::RecordSetModule,
        ConventionalName::ValueManagerModule,
        ConventionalName::SessionModule,
        ConventionalName::ExternalConnectionModule,
        ConventionalName::ManagedApplicationModule,
        ConventionalName::OrdinaryApplicationModule,
        ConventionalName::ApplicationModule,
        ConventionalName::ModuleBin,
        ConventionalName::Ext,
        ConventionalName::Form,
        ConventionalName::Forms,
        ConventionalName::Commands,
        ConventionalName::ConfigurationXml,
        ConventionalName::PredefinedXml,
        ConventionalName::RightsXml,
        ConventionalName::FormXml,
        ConventionalName::FormJson,
        ConventionalName::FormBin,
        ConventionalName::ParentConfigurationsBin,
    ];

    /// The canonical spelling the configurator writes. Constructing a path or
    /// URI uses this; comparing an on-disk name never does directly — that is
    /// what [`conventional_of`] is for.
    pub const fn canonical(self) -> &'static str {
        match self {
            ConventionalName::Module => "Module.bsl",
            ConventionalName::ObjectModule => "ObjectModule.bsl",
            ConventionalName::ManagerModule => "ManagerModule.bsl",
            ConventionalName::FormModule => "FormModule.bsl",
            ConventionalName::CommandModule => "CommandModule.bsl",
            ConventionalName::RecordSetModule => "RecordSetModule.bsl",
            ConventionalName::ValueManagerModule => "ValueManagerModule.bsl",
            ConventionalName::SessionModule => "SessionModule.bsl",
            ConventionalName::ExternalConnectionModule => "ExternalConnectionModule.bsl",
            ConventionalName::ManagedApplicationModule => "ManagedApplicationModule.bsl",
            ConventionalName::OrdinaryApplicationModule => "OrdinaryApplicationModule.bsl",
            ConventionalName::ApplicationModule => "ApplicationModule.bsl",
            ConventionalName::ModuleBin => "Module.bin",
            ConventionalName::Ext => "Ext",
            ConventionalName::Form => "Form",
            ConventionalName::Forms => "Forms",
            ConventionalName::Commands => "Commands",
            ConventionalName::ConfigurationXml => "Configuration.xml",
            ConventionalName::PredefinedXml => "Predefined.xml",
            ConventionalName::RightsXml => "Rights.xml",
            ConventionalName::FormXml => "Form.xml",
            ConventionalName::FormJson => "Form.json",
            ConventionalName::FormBin => "Form.bin",
            ConventionalName::ParentConfigurationsBin => "ParentConfigurations.bin",
        }
    }

    /// The canonical stem, for the callers that compare after `file_stem()`.
    pub fn canonical_stem(self) -> &'static str {
        let canonical = self.canonical();
        match canonical.rsplit_once('.') {
            Some((stem, _)) => stem,
            None => canonical,
        }
    }
}

/// Recognize a conventional name in any ASCII case. `None` means the name is
/// not conventional — which for a path segment usually means it is an object
/// name and must be treated exactly.
///
/// `Form`/`Form.xml` and `Module.bsl`-vs-stem ambiguity is on the caller: pass
/// a full file name to get file-name variants, a directory name to get
/// directory variants; this function only answers "which dictionary entry does
/// this spelling collapse into", preferring the first match in
/// [`ConventionalName::ALL`] order (the canonical spellings are distinct, so
/// at most one matches).
pub fn conventional_of(name: &str) -> Option<ConventionalName> {
    ConventionalName::ALL.iter().copied().find(|c| c.canonical().eq_ignore_ascii_case(name))
}

/// The source extension of a `.bsl` module body.
pub const BSL_EXTENSION: &str = "bsl";
/// The extension of metadata descriptors.
pub const XML_EXTENSION: &str = "xml";
/// The extension of protected (compiled) module containers.
pub const BIN_EXTENSION: &str = "bin";

/// Case-insensitive extension check on a path, the one way the workspace asks
/// "is this a `.bsl` / `.xml` file".
pub fn has_extension(path: &std::path::Path, extension: &str) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(extension))
}

/// [`has_extension`] for extensions already carried as strings (event keys,
/// URIs), matching on the suffix after the last dot.
pub fn str_has_extension(name: &str, extension: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, e)| e.eq_ignore_ascii_case(extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use expect_test::expect;

    #[test]
    fn every_spelling_of_a_conventional_name_collapses_into_its_entry() {
        for &name in ConventionalName::ALL {
            let canonical = name.canonical();
            assert_eq!(conventional_of(canonical), Some(name), "{canonical}");
            assert_eq!(conventional_of(&canonical.to_ascii_uppercase()), Some(name));
            assert_eq!(conventional_of(&canonical.to_ascii_lowercase()), Some(name));
        }
    }

    #[test]
    fn an_object_name_is_not_conventional() {
        for name in ["Товары", "Alpha", "ExtModule", "ModuleX", "Configuration"] {
            assert_eq!(conventional_of(name), None, "{name}");
        }
    }

    /// The dictionary is data other checks are built from, so its composition
    /// is pinned independently: an accidental removal or rename shows up as a
    /// snapshot diff here even though the gate and the table tests would have
    /// silently followed the change.
    #[test]
    fn the_dictionary_composition_is_pinned() {
        let listing =
            ConventionalName::ALL.iter().map(|c| c.canonical()).collect::<Vec<_>>().join("\n");
        expect![[r#"
            Module.bsl
            ObjectModule.bsl
            ManagerModule.bsl
            FormModule.bsl
            CommandModule.bsl
            RecordSetModule.bsl
            ValueManagerModule.bsl
            SessionModule.bsl
            ExternalConnectionModule.bsl
            ManagedApplicationModule.bsl
            OrdinaryApplicationModule.bsl
            ApplicationModule.bsl
            Module.bin
            Ext
            Form
            Forms
            Commands
            Configuration.xml
            Predefined.xml
            Rights.xml
            Form.xml
            Form.json
            Form.bin
            ParentConfigurations.bin"#]]
        .assert_eq(&listing);
    }

    #[test]
    fn extension_checks_ignore_ascii_case_only() {
        use std::path::Path;
        assert!(has_extension(Path::new("a/Module.BSL"), BSL_EXTENSION));
        assert!(has_extension(Path::new("a/Meta.XML"), XML_EXTENSION));
        assert!(!has_extension(Path::new("a/Module.bs"), BSL_EXTENSION));
        assert!(!has_extension(Path::new("a/Module"), BSL_EXTENSION));
        assert!(str_has_extension("Meta.XML", XML_EXTENSION));
        assert!(!str_has_extension("Meta", XML_EXTENSION));
    }

    #[test]
    fn a_stem_maps_to_its_file_name_entry() {
        assert_eq!(ConventionalName::ObjectModule.canonical_stem(), "ObjectModule");
        assert_eq!(ConventionalName::Ext.canonical_stem(), "Ext");
    }
}

/// How one path component of a CONSTRUCTED candidate matches a real component.
/// The caller decides per position — deriving the mode from the spelling would
/// swallow an object legally named `Ext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegmentMatch {
    /// An object/form/command NAME: its case is significant.
    Exact,
    /// A wholly conventional segment (`Ext`, `Forms`, `Module.bsl`, a
    /// collection directory): ASCII-case-insensitive.
    Ci,
    /// A `{name}.{ext}` file built from an object name: the stem is exact,
    /// only the extension folds.
    StemExactExtCi,
}

impl SegmentMatch {
    /// Whether a real path component satisfies this mode against the
    /// constructed candidate component.
    pub fn matches(self, real: &str, candidate: &str) -> bool {
        match self {
            SegmentMatch::Exact => real == candidate,
            SegmentMatch::Ci => real.eq_ignore_ascii_case(candidate),
            SegmentMatch::StemExactExtCi => {
                match (real.rsplit_once('.'), candidate.rsplit_once('.')) {
                    (Some((real_stem, real_ext)), Some((cand_stem, cand_ext))) => {
                        real_stem == cand_stem && real_ext.eq_ignore_ascii_case(cand_ext)
                    }
                    (None, None) => real == candidate,
                    _ => false,
                }
            }
        }
    }
}

/// Whether the path's tail is `…/Ext/<name>` in any ASCII case, with either
/// separator — the way diagnostics classify a module by its final segments.
/// The `Ext` HERE is positional: two segments from the end is the service
/// level, so an object named `Ext` (one level higher) cannot match.
pub fn path_ends_with_ext_child(path: &str, name: ConventionalName) -> bool {
    let mut segments = path.rsplit(['/', '\\']);
    let (Some(last), Some(prev)) = (segments.next(), segments.next()) else {
        return false;
    };
    conventional_of(last) == Some(name) && conventional_of(prev) == Some(ConventionalName::Ext)
}
