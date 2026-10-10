use crate::{DefDatabase, Name};
use base_db::{FileIdInput, SourceRootInput};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use stdx::case::CaseExt;
use vfs::FileId;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNameUsage {
    names: FxHashSet<Name>,
}

impl FileNameUsage {
    pub fn iter(&self) -> impl Iterator<Item = &Name> {
        self.names.iter()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn contains(&self, lowercase_name: &Name) -> bool {
        self.names.contains(lowercase_name)
    }
}

/// Start offsets of every name token in one file, keyed by the folded name.
///
/// References/rename resolve popular names (standard event handlers exist in
/// hundreds of modules) against every candidate file the aggregated index
/// returns. Walking each candidate's syntax tree on every request repeats the
/// same token scan on warm paths; this projection pays the walk once per file
/// revision and lets consumers jump straight to the matching tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileNameOffsets {
    by_name: FxHashMap<Name, Box<[u32]>>,
}

impl FileNameOffsets {
    /// Token start offsets for `match_name` (a [`normalize_match_name`] key),
    /// in document order.
    pub fn offsets(&self, match_name: &Name) -> &[u32] {
        self.by_name.get(match_name).map(|v| v.as_ref()).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceRootNameUsage {
    by_name: FxHashMap<Name, Vec<FileId>>,
}

impl SourceRootNameUsage {
    pub fn files_with(&self, lowercase_name: &Name) -> &[FileId] {
        self.by_name.get(lowercase_name).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

pub fn normalize_name(name: &Name) -> Name {
    Name::new(&name.as_str().fold_lower())
}

/// Match-bucket key: same key iff `Name::eq_ignore_case` holds. `fold_lower`
/// is NOT that key — its contextual `to_lowercase` fallback (Greek final
/// sigma) splits `eq_ignore_case`-equal names into different keys, and the
/// lexer admits any `\p{L}` identifier. Offset buckets must agree with the
/// token-level comparison or lookups silently miss occurrences.
pub fn normalize_match_name(name: &Name) -> Name {
    Name::new(&stdx::case::fold_lower_per_char(name.as_str()))
}

/// Approximate live heap bytes for Salsa's `memory_usage` report: the `names`
/// hashbrown set plus each name's non-inlined `SmolStr` payload.
fn file_name_usage_heap(v: &Arc<FileNameUsage>) -> usize {
    use crate::heap_estimate::{map_table_bytes, name_bytes};

    let mut bytes = map_table_bytes::<Name, ()>(v.names.len());
    for name in &v.names {
        bytes += name_bytes(name);
    }
    bytes
}

// The cap holds a whole workspace: the aggregated index folds every file's set, and a
// resident that warms them in chunks with trims between must find them still memoised.
#[salsa::tracked(lru = 65536, heap_size = file_name_usage_heap, returns(ref))]
pub fn file_name_usage_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<FileNameUsage> {
    let file_id = file_id_input.file_id(db);
    let _span = tracing::debug_span!("file_name_usage_query", ?file_id).entered();

    let parse = db.parse_ref(file_id);
    let root = parse.syntax_node();

    let mut names: FxHashSet<Name> = FxHashSet::default();
    for token in root.descendants_with_tokens().filter_map(|e| e.into_token()) {
        if !token.kind().is_name_token() {
            continue;
        }
        names.insert(Name::new(&token.text().fold_lower()));
    }

    Arc::new(FileNameUsage { names })
}

/// Approximate live heap bytes for Salsa's `memory_usage` report: the map
/// table, each name's non-inlined `SmolStr` payload, and the offset slices.
fn file_name_offsets_heap(v: &Arc<FileNameOffsets>) -> usize {
    use crate::heap_estimate::{map_table_bytes, name_bytes};

    let mut bytes = map_table_bytes::<Name, Box<[u32]>>(v.by_name.len());
    for (name, offsets) in &v.by_name {
        bytes += name_bytes(name) + std::mem::size_of_val(offsets.as_ref());
    }
    bytes
}

// Kept separate from `file_name_usage_query` on purpose: the aggregated index
// touches every workspace file cold and only needs name *presence*, while the
// offsets are needed only for files that references/rename actually visit.
// Folding them into one query would materialise offset maps for all 25k+
// files during the cold index build for no reader.
//
// The `lru` cap is not a same-request ceiling: salsa evicts at revision
// boundaries and explicit trims, so one request over N candidate files keeps
// N maps resident until then. The `heap_size` hook makes that residency
// visible to `memory_report`.
#[salsa::tracked(lru = 1024, heap_size = file_name_offsets_heap, returns(ref))]
pub fn file_name_offsets_query<'db>(
    db: &'db dyn DefDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<FileNameOffsets> {
    let file_id = file_id_input.file_id(db);
    let _span = tracing::debug_span!("file_name_offsets_query", ?file_id).entered();

    let parse = db.parse_ref(file_id);
    let root = parse.syntax_node();

    let mut by_name: FxHashMap<Name, Vec<u32>> = FxHashMap::default();
    for token in root.descendants_with_tokens().filter_map(|e| e.into_token()) {
        if !token.kind().is_name_token() {
            continue;
        }
        by_name
            .entry(Name::new(&stdx::case::fold_lower_per_char(token.text())))
            .or_default()
            .push(u32::from(token.text_range().start()));
    }

    let by_name = by_name.into_iter().map(|(name, offsets)| (name, offsets.into())).collect();
    Arc::new(FileNameOffsets { by_name })
}

/// Approximate live heap bytes for Salsa's `memory_usage` report: the
/// `by_name` table, each name's non-inlined `SmolStr` payload, and each
/// name's file-id posting list. This is the aggregated inverted index over
/// the whole source root, so it is typically the largest single memoised
/// entry in the report. New heap-owning fields must be added here too.
fn source_root_name_usage_heap(v: &Arc<SourceRootNameUsage>) -> usize {
    use crate::heap_estimate::{map_table_bytes, name_bytes, vec_bytes};

    let u = &**v;
    let mut bytes = map_table_bytes::<Name, Vec<FileId>>(u.by_name.len());
    for (name, files) in &u.by_name {
        bytes += name_bytes(name) + vec_bytes::<FileId>(files.len());
    }
    bytes
}

#[salsa::tracked(lru = 4, heap_size = source_root_name_usage_heap, returns(clone))]
pub fn source_root_name_usage_query(
    db: &dyn DefDatabase,
    source_root_input: SourceRootInput,
) -> Arc<SourceRootNameUsage> {
    let source_root = source_root_input.root(db);
    let file_set = source_root.file_set();

    let files: Vec<FileId> = source_root
        .iter()
        .filter(|&file_id| crate::workspace::is_bsl_source(file_set, file_id))
        .collect();

    let _span =
        tracing::info_span!("source_root_name_usage_query", file_count = files.len()).entered();

    let mut by_name: FxHashMap<Name, Vec<FileId>> = FxHashMap::default();
    for file_id in files {
        db.unwind_if_revision_cancelled();
        let input = FileIdInput::new(db, file_id);
        let usage = file_name_usage_query(db, input);
        for name in usage.names.iter() {
            by_name.entry(name.clone()).or_default().push(file_id);
        }
    }

    for entry in by_name.values_mut() {
        entry.sort_unstable();
        entry.dedup();
    }

    tracing::info!(names = by_name.len(), "SourceRootNameUsage built");

    Arc::new(SourceRootNameUsage { by_name })
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn normalize_name_lowercases_cyrillic() {
        assert_eq!(normalize_name(&Name::new("Тест")), normalize_name(&Name::new("ТЕСТ")));
        assert_eq!(normalize_name(&Name::new("Тест")), normalize_name(&Name::new("тест")));
    }

    #[test]
    fn normalize_name_lowercases_latin() {
        assert_eq!(normalize_name(&Name::new("Test")), normalize_name(&Name::new("TEST")));
        assert_eq!(normalize_name(&Name::new("Test")), normalize_name(&Name::new("test")));
    }

    #[test]
    fn file_name_usage_membership_uses_normalized_keys() {
        let mut names = FxHashSet::default();
        names.insert(normalize_name(&Name::new("Тест")));
        let usage = FileNameUsage { names };

        assert!(usage.contains(&normalize_name(&Name::new("ТЕСТ"))));
        assert!(usage.contains(&normalize_name(&Name::new("тест"))));
        assert!(!usage.contains(&normalize_name(&Name::new("Другое"))));
    }

    #[test]
    fn source_root_name_usage_returns_empty_slice_on_miss() {
        let aggregator = SourceRootNameUsage::default();
        assert!(aggregator.files_with(&normalize_name(&Name::new("чтонибудь"))).is_empty());
    }

    #[test]
    fn source_root_name_usage_heap_counts_table_names_and_postings() {
        let long_name = "ОченьДлинноеИмяПроцедурыБольше23Символов".to_string();
        let name_capacity = long_name.len();
        let files = vec![FileId(1), FileId(2), FileId(3)];
        let mut by_name = FxHashMap::default();
        by_name.insert(Name::new(&long_name), files.clone());
        let usage = Arc::new(SourceRootNameUsage { by_name });

        let bytes = source_root_name_usage_heap(&usage);
        // At least the spilled name payload plus the file-id posting list;
        // well under a kilobyte for a single entry.
        assert!(bytes > name_capacity + files.len() * std::mem::size_of::<FileId>());
        assert!(bytes < 1024);
    }
}
