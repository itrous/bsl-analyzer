use rustc_hash::FxHasher;
use vfs::file_set::FileSet;
use vfs::FileId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceRootId(pub u32);

/// The source root holding `.bsl` files. BSL iterators (workspace symbols,
/// directory removal, graph build) scan this root exclusively.
pub const BSL_SOURCE_ROOT: SourceRootId = SourceRootId(0);

/// The source root holding watched metadata XML. Kept separate from
/// [`BSL_SOURCE_ROOT`] so the BSL iterators never see metadata files, while each
/// XML still has a root through which `file_text`'s disk read resolves its path.
pub const METADATA_SOURCE_ROOT: SourceRootId = SourceRootId(1);

/// What kind of files a root holds, which decides the salsa durability of the
/// file inputs registered under it. Durability is the shield that lets memos
/// skip deep dependency verification when only lower-durability inputs changed,
/// so the kinds are ordered by how often their contents actually change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceRootKind {
    /// Workspace `.bsl` sources: edited on every keystroke.
    Local,
    /// Read-only library roots: effectively frozen for the session.
    Library,
    /// Configuration metadata XML: changes only on a designer/EDT export —
    /// rare batches against a continuous stream of `.bsl` edits, so metadata
    /// memos should not be re-verified per keystroke.
    Metadata,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRoot {
    kind: SourceRootKind,

    file_set: FileSet,
}

impl SourceRoot {
    pub fn new_local(file_set: FileSet) -> Self {
        SourceRoot { kind: SourceRootKind::Local, file_set }
    }

    pub fn new_library(file_set: FileSet) -> Self {
        SourceRoot { kind: SourceRootKind::Library, file_set }
    }

    pub fn new_metadata(file_set: FileSet) -> Self {
        SourceRoot { kind: SourceRootKind::Metadata, file_set }
    }

    pub fn is_library(&self) -> bool {
        self.kind == SourceRootKind::Library
    }

    pub fn file_set(&self) -> &FileSet {
        &self.file_set
    }

    pub fn iter(&self) -> impl Iterator<Item = FileId> + '_ {
        self.file_set.iter()
    }

    pub fn durability(&self) -> salsa::Durability {
        match self.kind {
            SourceRootKind::Local => salsa::Durability::LOW,
            SourceRootKind::Library => salsa::Durability::HIGH,
            SourceRootKind::Metadata => salsa::Durability::MEDIUM,
        }
    }

    /// Approximate live heap bytes owned by this source root: delegates to its
    /// `FileSet`. See [`FileSet::estimated_heap_size`] for the counting
    /// convention, including its Arc-sharing over-count caveat.
    pub fn estimated_heap_size(&self) -> usize {
        self.file_set.estimated_heap_size()
    }
}

/// `heap_size` estimators for Salsa's `memory_usage` introspection over this
/// module's `#[salsa::input]` / `#[salsa::interned]` structs. Each hook receives
/// a reference to the tuple of ALL declared fields in declaration order (a
/// 1-tuple for a single-field struct), per the fork's `heap_size = path`
/// convention for input/interned structs.
pub(crate) mod heap_estimate {
    use super::{DiagnosticsConfigInput, SourceRoot};

    pub(crate) fn file_text_input_heap((text,): &(String,)) -> usize {
        text.capacity()
    }

    pub(crate) fn source_root_input_heap((root,): &(SourceRoot,)) -> usize {
        root.estimated_heap_size()
    }

    /// New heap-owning fields on [`DiagnosticsConfigInput`] must be added here too.
    pub(crate) fn diagnostics_config_id_heap((config,): &(DiagnosticsConfigInput,)) -> usize {
        stdx::heap::vec_bytes::<String>(config.disabled.len())
            + config.disabled.iter().map(String::capacity).sum::<usize>()
            + stdx::heap::vec_bytes::<String>(config.enabled.len())
            + config.enabled.iter().map(String::capacity).sum::<usize>()
            + stdx::heap::vec_bytes::<(String, String)>(config.parameters.len())
            + config.parameters.iter().map(|(k, v)| k.capacity() + v.capacity()).sum::<usize>()
            + config.scope.as_deref().map_or(0, crate::scope::scope_heap_size)
            + config.only_enabled.as_ref().map_or(0, |codes| {
                stdx::heap::vec_bytes::<String>(codes.len())
                    + codes.iter().map(String::capacity).sum::<usize>()
            })
            + stdx::heap::vec_bytes::<(String, String)>(config.metadata_overrides.len())
            + config
                .metadata_overrides
                .iter()
                .map(|(k, v)| k.capacity() + v.capacity())
                .sum::<usize>()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::input::DiagnosticsConfigInput;

        #[test]
        fn diagnostics_config_id_heap_counts_string_payloads() {
            let config = DiagnosticsConfigInput::from_raw(
                vec!["Code1".to_string(), "Code2".to_string()],
                vec!["Code3".to_string()],
                vec![("Code4".to_string(), "some-long-parameter-value".to_string())],
                false,
                10,
                crate::Locale::Ru,
                false,
            );
            let strings_only: usize = config.disabled.iter().map(String::capacity).sum::<usize>()
                + config.enabled.iter().map(String::capacity).sum::<usize>()
                + config.parameters.iter().map(|(k, v)| k.capacity() + v.capacity()).sum::<usize>();

            let bytes = diagnostics_config_id_heap(&(config,));
            assert!(bytes >= strings_only);
            assert!(bytes < 1024);
        }
    }
}

#[salsa::input(debug, heap_size = heap_estimate::file_text_input_heap)]
pub struct FileTextInput {
    pub text: String,
}

/// Content revision of a file: a 64-bit content hash with the byte length folded
/// in. Used as the salsa invalidation key for `file_text_query` and as the
/// verification token a disk re-read must match before its bytes may be returned
/// (so an evicted+rederived text is sound). The model is probabilistic — a hash
/// collision at the same length would alias two contents — which is acceptable
/// for analysis caching but not a cryptographic guarantee.
///
/// `unreadable` rides here rather than on [`FileTextInput`] because this input is
/// the one every registered file has: the disk-backed path stores no text input at
/// all, so "the file became unreadable" would be the APPEARANCE of a text input —
/// a plain map insert that Salsa cannot see, leaving a memo that read "no entry"
/// unrevalidated. The revision is already the trigger `file_text_query` keys on for
/// the same reason.
#[salsa::input(debug, heap_size = stdx::heap::zero)]
pub struct FileRevisionInput {
    #[returns(copy)]
    pub revision: u64,

    /// The file exists but its bytes could not be read. Distinct from an empty
    /// file: both carry an empty text, and nothing else tells them apart — the
    /// source root holds the file either way, and the VFS never forgets a FileId.
    #[returns(copy)]
    pub unreadable: bool,
}

/// Compute the [`FileRevisionInput`] value for some text. Folds the byte length
/// into the hash so same-hash-different-length contents cannot alias.
pub fn content_revision(text: &str) -> u64 {
    use std::hash::Hasher;
    let mut hasher = FxHasher::default();
    hasher.write_usize(text.len());
    hasher.write(text.as_bytes());
    hasher.finish()
}

#[salsa::input(debug, heap_size = heap_estimate::source_root_input_heap)]
pub struct SourceRootInput {
    pub root: SourceRoot,
}

#[salsa::input(debug, heap_size = stdx::heap::zero)]
pub struct FileSourceRootInput {
    #[returns(copy)]
    pub source_root_id: SourceRootId,
}

#[salsa::interned(debug, heap_size = stdx::heap::zero)]
pub struct FileIdInput {
    #[returns(copy)]
    pub file_id: vfs::FileId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiagnosticsConfigInput {
    pub disabled: Vec<String>,

    pub enabled: Vec<String>,

    pub parameters: Vec<(String, String)>,

    pub ordinary_app_support: bool,

    pub dataflow_max_iterations: usize,

    pub locale: crate::Locale,

    pub bslls_suppression_compat: bool,

    /// Restricts diagnostics to files/lines changed relative to a reference
    /// state (vendor-diff filter). `None` = no restriction. Part of the
    /// interned config so replacing the scope re-keys `file_diagnostics_query`.
    pub scope: Option<std::sync::Arc<crate::AnalysisScope>>,

    /// Codes the run is restricted to (the CLI `--only` filter); `None` when
    /// every enabled code runs. Sorted and deduplicated.
    pub only_enabled: Option<Vec<String>>,

    /// Per-code metadata overrides as `(code, JSON)`, sorted by code. Part of
    /// the key because they change a diagnostic's tags and severity — two
    /// configurations that differ only here must not share a memo.
    pub metadata_overrides: Vec<(String, String)>,

    /// Whether findings carry their quick fixes. Part of the key: the fixes are
    /// part of the memoised answer.
    pub quick_fixes: bool,
}

impl DiagnosticsConfigInput {
    pub fn from_raw(
        disabled: impl IntoIterator<Item = String>,
        enabled: impl IntoIterator<Item = String>,
        parameters: impl IntoIterator<Item = (String, String)>,
        ordinary_app_support: bool,
        dataflow_max_iterations: usize,
        locale: crate::Locale,
        bslls_suppression_compat: bool,
    ) -> Self {
        let mut disabled: Vec<String> = disabled.into_iter().collect();
        disabled.sort();
        disabled.dedup();

        let mut enabled: Vec<String> = enabled.into_iter().collect();
        enabled.sort();
        enabled.dedup();

        let mut parameters: Vec<(String, String)> = parameters.into_iter().collect();
        parameters.sort_by(|a, b| a.0.cmp(&b.0));
        parameters.dedup_by(|a, b| a.0 == b.0);

        Self {
            disabled,
            enabled,
            parameters,
            ordinary_app_support,
            dataflow_max_iterations,
            locale,
            bslls_suppression_compat,
            scope: None,
            only_enabled: None,
            metadata_overrides: Vec::new(),
            quick_fixes: true,
        }
    }

    pub fn with_filters(
        mut self,
        only_enabled: Option<impl IntoIterator<Item = String>>,
        metadata_overrides: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        self.only_enabled = only_enabled.map(|codes| {
            let mut codes: Vec<String> = codes.into_iter().collect();
            codes.sort();
            codes.dedup();
            codes
        });
        let mut overrides: Vec<(String, String)> = metadata_overrides.into_iter().collect();
        overrides.sort_by(|a, b| a.0.cmp(&b.0));
        overrides.dedup_by(|a, b| a.0 == b.0);
        self.metadata_overrides = overrides;
        self
    }

    pub fn with_scope(mut self, scope: Option<std::sync::Arc<crate::AnalysisScope>>) -> Self {
        self.scope = scope;
        self
    }

    pub fn with_quick_fixes(mut self, quick_fixes: bool) -> Self {
        self.quick_fixes = quick_fixes;
        self
    }

    pub fn is_disabled(&self, code: &str) -> bool {
        self.disabled.binary_search_by(|c| c.as_str().cmp(code)).is_ok()
    }

    pub fn get_parameters(&self, code: &str) -> Option<&str> {
        self.parameters
            .binary_search_by(|(c, _)| c.as_str().cmp(code))
            .ok()
            .map(|idx| self.parameters[idx].1.as_str())
    }
}

#[salsa::interned(debug, heap_size = heap_estimate::diagnostics_config_id_heap)]
pub struct DiagnosticsConfigId {
    pub config: DiagnosticsConfigInput,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use vfs::VfsPath;

    #[test]
    fn test_source_root_local() {
        let mut file_set = FileSet::new();
        file_set.insert(FileId(0), VfsPath::from(PathBuf::from("/test.bsl")));

        let root = SourceRoot::new_local(file_set);
        assert!(!root.is_library());
        assert_eq!(root.durability(), salsa::Durability::LOW);
        assert_eq!(root.file_set().len(), 1);
    }

    #[test]
    fn test_source_root_library() {
        let mut file_set = FileSet::new();
        file_set.insert(FileId(0), VfsPath::from(PathBuf::from("/lib.bsl")));

        let root = SourceRoot::new_library(file_set);
        assert!(root.is_library());
        assert_eq!(root.durability(), salsa::Durability::HIGH);
        assert_eq!(root.file_set().len(), 1);
    }

    #[test]
    fn test_source_root_metadata() {
        let mut file_set = FileSet::new();
        file_set.insert(FileId(0), VfsPath::from(PathBuf::from("/Catalogs/Товары.xml")));

        let root = SourceRoot::new_metadata(file_set);
        assert!(!root.is_library());
        assert_eq!(root.durability(), salsa::Durability::MEDIUM);
        assert_eq!(root.file_set().len(), 1);
    }

    #[test]
    fn test_source_root_iter() {
        let mut file_set = FileSet::new();
        let id1 = FileId(0);
        let id2 = FileId(1);
        file_set.insert(id1, VfsPath::from(PathBuf::from("/test1.bsl")));
        file_set.insert(id2, VfsPath::from(PathBuf::from("/test2.bsl")));

        let root = SourceRoot::new_local(file_set);
        let ids: Vec<_> = root.iter().collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }

    #[test]
    fn test_source_root_id_eq() {
        let id1 = SourceRootId(0);
        let id2 = SourceRootId(0);
        let id3 = SourceRootId(1);

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }
}
