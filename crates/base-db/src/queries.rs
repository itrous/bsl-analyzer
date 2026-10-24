use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use stdx::case::CaseExt;

use syntax::{Parse, SyntaxNode, TextRange};
use vfs::{FileId, VfsPath};

use crate::input::{content_revision, FileIdInput, SourceRootInput};
use crate::{ParseOutcome, ParseSnapshot, SourceDatabase};

/// Read a file's text from disk verbatim — no BOM stripping, no normalization.
///
/// The single disk-read primitive shared by [`file_text_query`] and the
/// out-of-Salsa metadata bootstrap that computes content revisions. Routing both
/// through this function guarantees the bootstrap's `content_revision(read_disk_text(p))`
/// hash matches what `file_text_query` recomputes on a later disk re-read, so a
/// disk-backed file never spuriously fails [`assert_revision`].
pub fn read_disk_text(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// Decode in-memory file bytes to source text under the exact same contract as
/// [`read_disk_text`]: valid UTF-8 only, verbatim — no BOM strip, no newline
/// normalization. The VFS loader holds the bytes the watcher read, not a path,
/// so it cannot call [`read_disk_text`]; routing it through this keeps the text
/// it stores (and the content revision derived from it) byte-identical to what
/// [`file_text_query`] recomputes on a later disk re-read. Stripping anything
/// here desyncs the recorded revision from the on-read hash and trips
/// [`assert_revision`]. The lexer consumes a leading BOM as its own token, so
/// preserving it is correct and keeps text offsets aligned with the editor's
/// document.
pub fn decode_disk_bytes(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

/// Heap-size estimators for Salsa's `memory_usage` introspection.
pub(crate) mod heap_estimate {
    use std::collections::HashMap;
    use std::mem::size_of_val;
    use std::sync::Arc;
    use syntax::{Parse, SyntaxNode, TextRange};

    /// Rough live bytes of a parse result's rowan green tree: each token's owned
    /// text plus a small fixed per-element bookkeeping cost (child-slot +
    /// kind/len), counted by the builder while the tree is built. Green nodes
    /// are interned and shared across identical subtrees, so this
    /// **over-counts** deduplicated structure — an acceptable rough upper bound.
    pub(crate) fn parse_heap(parse: &Parse<SyntaxNode>) -> usize {
        parse.heap_bytes() + size_of_val(parse.errors())
    }

    /// Heap of a memoised file text: the `Arc<str>` payload bytes.
    pub(crate) fn file_text_heap(text: &Arc<str>) -> usize {
        text.len()
    }

    /// Heap of the API-region method map: the table itself plus the owned
    /// region-name strings. New heap-owning fields in the value type must be
    /// added here too.
    pub(crate) fn method_regions_heap(map: &Arc<HashMap<TextRange, String>>) -> usize {
        stdx::heap::map_table_bytes::<TextRange, String>(map.len())
            + map.values().map(String::capacity).sum::<usize>()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn file_text_heap_counts_payload_bytes() {
            let text: Arc<str> = Arc::from("Процедура Тест() КонецПроцедуры");
            assert_eq!(file_text_heap(&text), text.len());
        }

        #[test]
        fn method_regions_heap_counts_table_and_strings() {
            let mut map = HashMap::new();
            let name = "ПрограммныйИнтерфейс".to_string();
            let name_capacity = name.capacity();
            map.insert(TextRange::new(0.into(), 10.into()), name);
            let bytes = method_regions_heap(&Arc::new(map));
            // At least the owned string plus one table slot; well under a
            // kilobyte for a single entry.
            assert!(bytes > name_capacity);
            assert!(bytes < 1024);
        }
    }
}

#[salsa::tracked(lru = 512, heap_size = crate::queries::heap_estimate::parse_heap, returns(ref))]
pub fn parse_query<'db>(db: &'db dyn SourceDatabase, input: FileIdInput<'db>) -> Parse<SyntaxNode> {
    let _span = tracing::info_span!("parse").entered();

    let text = file_text_query(db, input);
    let file_id = input.file_id(db);
    // Снимок есть только у оверлея (открытый документ, фикстура). Проверка
    // идёт мимо salsa: она решает, как получить значение, а не какое.
    let overlay = db.try_file_text_input(file_id).is_some();
    let parse = match overlay.then(|| db.parse_snapshot(file_id)).flatten() {
        Some(snapshot) => reparse_or_full(db, &snapshot, text),
        None => {
            db.count_parse(ParseOutcome::Full);
            parser::parse_with_shared_cache(text)
        }
    };
    if overlay {
        db.store_parse_snapshot(
            file_id,
            ParseSnapshot { text: text.clone(), parse: parse.clone() },
        );
    }
    parse
}

/// Разбор нового текста по снимку предыдущего: фрагмент метода, если правка
/// целиком внутри одного метода, иначе файл целиком. Значение от выбора не
/// зависит — за это отвечают гарды переразбора; под `BSL_REPARSE_VERIFY=1`
/// вклейка сверяется с полным разбором на месте, и расхождение — ошибка в
/// журнале плюс счётчик, а в значение уходит полный разбор.
fn reparse_or_full(
    db: &dyn SourceDatabase,
    snapshot: &ParseSnapshot,
    text: &Arc<str>,
) -> Parse<SyntaxNode> {
    let Some(edit) = parser::reparse::edit_between(&snapshot.text, text) else {
        // Тот же текст под новой ревизией (переоткрытие): дерево уже есть.
        db.count_parse(ParseOutcome::Full);
        return snapshot.parse.clone();
    };
    match parser::reparse::reparse_method(&snapshot.parse, &snapshot.text, text, edit) {
        Ok(spliced) => {
            if reparse_verify_enabled() {
                let full = parser::parse_with_shared_cache(text);
                if spliced != full {
                    tracing::error!(
                        old_len = snapshot.text.len(),
                        new_len = text.len(),
                        ?edit,
                        "reparse: вклейка разошлась с полным разбором"
                    );
                    db.count_parse(ParseOutcome::Mismatched);
                    return full;
                }
            }
            db.count_parse(ParseOutcome::Spliced);
            spliced
        }
        Err(refusal) => {
            tracing::debug!(?refusal, "reparse: фрагмент не вклеен, файл разобран целиком");
            db.count_parse(ParseOutcome::Refused(refusal));
            parser::parse_with_shared_cache(text)
        }
    }
}

fn reparse_verify_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("BSL_REPARSE_VERIFY").is_some_and(|v| v == "1"))
}

/// Switch [`parse_query`]'s LRU cap between the interactive profile and a small
/// sweep profile. During a chunked whole-workspace sweep the closed files' syntax
/// trees are pure batch working set — a wide retention window only pins hundreds of
/// megabytes of green trees across chunks — so the sweep shrinks the cap and restores
/// the interactive one when it ends. The interactive value must stay equal to the
/// `lru` literal on [`parse_query`]. The new cap takes effect at the next LRU trim;
/// it evicts nothing by itself. Like any salsa write, this cancels in-flight
/// snapshots — call it only from points that may already trim.
pub fn set_parse_lru_sweep_mode(db: &mut dyn SourceDatabase, sweep: bool) {
    const INTERACTIVE: usize = 512;
    const SWEEP: usize = 64;
    parse_query::set_lru_capacity(db, if sweep { SWEEP } else { INTERACTIVE });
}

/// A file's source text, keyed on its stable [`FileIdInput`] and triggered by its
/// content revision. Returns the in-memory overlay when one is registered
/// (open editor buffers, test fixtures); otherwise reads the file from disk and
/// verifies the bytes hash to the recorded revision before returning. The
/// hash-verify makes the disk read a pure function of the revision, so the memo
/// is LRU-evictable and re-derives soundly. A mismatch (the file changed under a
/// running analysis, or a deleted/unreadable file) is a hard error, never a
/// silently-mixed result.
#[salsa::tracked(lru = 512, heap_size = crate::queries::heap_estimate::file_text_heap, returns(ref))]
pub fn file_text_query<'db>(db: &'db dyn SourceDatabase, input: FileIdInput<'db>) -> Arc<str> {
    let _span = tracing::info_span!("file_text").entered();
    Arc::from(file_text_unmemoized(db, input.file_id(db)))
}

/// The text [`file_text_query`] returns, read without leaving a memo behind: the
/// overlay when one is registered, otherwise the disk bytes verified against the
/// file's content revision. The inputs it reads become dependencies of the calling
/// query. For a query that scans every file of a source root once: routed through
/// [`file_text_query`], each scanned text stays pinned until the next LRU trim, so
/// the scan alone holds the whole root's source in memory.
pub fn file_text_unmemoized(db: &dyn SourceDatabase, file_id: FileId) -> Cow<'_, str> {
    let want = db.file_revision_input(file_id).revision(db);

    if let Some(text_input) = db.try_file_text_input(file_id) {
        let text = text_input.text(db);
        assert_revision(file_id, text, want);
        return Cow::Borrowed(text.as_str());
    }

    let path = disk_path(db, file_id);
    let text = read_disk_text(&path).unwrap_or_else(|err| {
        tracing::error!(?file_id, ?path, %err, "file_text: disk read failed");
        panic!("file_text: cannot read {path:?} for {file_id:?}: {err}")
    });
    assert_revision(file_id, &text, want);
    Cow::Owned(text)
}

fn assert_revision(file_id: FileId, text: &str, want: u64) {
    let got = content_revision(text);
    if got != want {
        panic!(
            "file_text revision mismatch for {file_id:?}: content changed under analysis \
             (recorded {want:#018x}, on-read {got:#018x})"
        );
    }
}

fn disk_path(db: &dyn SourceDatabase, file_id: FileId) -> PathBuf {
    let source_root_id = db.file_source_root_input(file_id).source_root_id(db);
    let root = db.source_root_input(source_root_id).root(db);
    let vfs_path = root.file_set().path_for_file(&file_id).unwrap_or_else(|| {
        panic!("file_text: {file_id:?} not present in its source root file set")
    });
    vfs_path.as_path().to_path_buf()
}

#[salsa::tracked(
    lru = 256,
    heap_size = crate::queries::heap_estimate::method_regions_heap,
    returns(clone)
)]
pub fn method_regions_query<'db>(
    db: &'db dyn SourceDatabase,
    input: FileIdInput<'db>,
) -> Arc<HashMap<TextRange, String>> {
    let _span = tracing::info_span!("method_regions").entered();

    let parse = parse_query(db, input);
    let root = parse.syntax_node();

    let mut map = HashMap::new();
    collect_methods_in_regions(&root, &mut map);

    tracing::debug!(count = map.len(), "Collected methods in API regions");

    Arc::new(map)
}

fn collect_methods_in_regions(root: &SyntaxNode, map: &mut HashMap<TextRange, String>) {
    use syntax::{
        ast::{self, AstNode},
        SyntaxKind,
    };

    // Region directives are flat markers; pair them with a running stack in
    // source order (preorder descendants), exactly like `hir-def::region_tree`.
    // A method's enclosing region is the one open at the method's start, so its
    // own interior (method-local) markers come later and do not affect it. A
    // region whose end marker sits inside a method body is closed there too, so
    // it cannot bleed into subsequent methods.
    let mut region_stack: Vec<String> = Vec::new();

    for node in root.descendants() {
        match node.kind() {
            SyntaxKind::PRE_REGION_DIR => {
                if let Some(region) = ast::PreRegionDir::cast(node.clone()) {
                    if region.is_end() {
                        region_stack.pop();
                    } else {
                        region_stack.push(region.name().unwrap_or_default());
                    }
                }
            }
            SyntaxKind::PROCEDURE_DEF | SyntaxKind::FUNCTION_DEF => {
                if let Some(root_region) = region_stack.first() {
                    if is_api_region(root_region) {
                        map.insert(node.text_range(), root_region.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_api_region(name: &str) -> bool {
    const API_REGIONS: &[&str] =
        &["программныйинтерфейс", "public", "служебныйпрограммныйинтерфейс", "internal"];
    API_REGIONS.contains(&name.fold_lower().as_str())
}

#[salsa::tracked(lru = 256, heap_size = stdx::heap::zero, returns(copy))]
pub fn resolve_vfs_path_query(
    db: &dyn salsa::Database,
    source_root_input: SourceRootInput,
    vfs_path_str: String,
) -> Option<FileId> {
    let source_root = source_root_input.root(db);
    let file_set = source_root.file_set();
    let vfs_path = VfsPath::new(PathBuf::from(vfs_path_str));
    file_set.file_for_path(&vfs_path).copied()
}

/// Resolve a CONSTRUCTED candidate path whose trailing components may differ
/// from the real spelling only by the caller-declared match modes. The exact
/// spelling is tried first (a canonical tree pays one map lookup); on a miss,
/// the file set is scanned once per candidate. The scan is memoised on
/// `(source root, candidate, modes)` rather than on the asking file: a fixed
/// host path such as `Ext/ApplicationModule.bsl` is asked by every file of a
/// configuration and answered by the root alone, so a per-file memo would
/// repeat the whole-set scan once per file.
pub fn resolve_vfs_path_ci_query(
    db: &dyn salsa::Database,
    source_root_input: SourceRootInput,
    vfs_path_str: String,
    tail_modes: &[bsl_conventions::SegmentMatch],
) -> Option<FileId> {
    if let Some(exact) = resolve_vfs_path_query(db, source_root_input, vfs_path_str.clone()) {
        return Some(exact);
    }
    if tail_modes.is_empty() {
        return None;
    }
    resolve_vfs_path_ci_scan_query(db, source_root_input, vfs_path_str, tail_modes.to_vec())
}

/// One scan of the file set for a candidate that missed its exact spelling,
/// comparing componentwise: every leading component exactly (it came from a
/// real path), the last `tail_modes.len()` components by their mode —
/// object-name positions stay exact there too. Reading the root records the
/// file-set dependency, so a changed set re-runs the scan.
#[salsa::tracked(lru = 256, heap_size = stdx::heap::zero, returns(copy))]
fn resolve_vfs_path_ci_scan_query(
    db: &dyn salsa::Database,
    source_root_input: SourceRootInput,
    vfs_path_str: String,
    tail_modes: Vec<bsl_conventions::SegmentMatch>,
) -> Option<FileId> {
    let candidate: Vec<String> =
        vfs_path_str.replace('\\', "/").split('/').map(str::to_owned).collect();
    let source_root = source_root_input.root(db);
    let file_set = source_root.file_set();
    for file in file_set.iter() {
        let Some(path) = file_set.path_for_file(&file) else { continue };
        let real_str = path.as_path().to_string_lossy().replace('\\', "/");
        let real: Vec<&str> = real_str.split('/').collect();
        if real.len() != candidate.len() {
            continue;
        }
        let head = candidate.len() - tail_modes.len().min(candidate.len());
        let head_ok = real[..head].iter().zip(&candidate[..head]).all(|(r, c)| *r == c);
        if !head_ok {
            continue;
        }
        let tail_ok = real[head..]
            .iter()
            .zip(&candidate[head..])
            .zip(&tail_modes)
            .all(|((r, c), mode)| mode.matches(r, c));
        if tail_ok {
            return Some(file);
        }
    }
    None
}

#[cfg(test)]
mod ci_resolution_tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use bsl_conventions::SegmentMatch as M;
    use salsa::Setter;
    use vfs::file_set::FileSet;
    use vfs::VfsPath;

    #[salsa::db]
    #[derive(Clone)]
    struct ResolveDb {
        storage: salsa::Storage<Self>,
        executed: Arc<Mutex<Vec<salsa::IngredientIndex>>>,
    }

    #[salsa::db]
    impl salsa::Database for ResolveDb {}

    impl Default for ResolveDb {
        fn default() -> Self {
            let executed = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&executed);
            let storage = salsa::Storage::builder()
                .event_callback(Box::new(move |event| {
                    if let salsa::EventKind::WillExecute { database_key } = event.kind {
                        sink.lock().unwrap().push(database_key.ingredient_index());
                    }
                }))
                .build();
            Self { storage, executed }
        }
    }

    impl ResolveDb {
        fn scans(&self) -> usize {
            use salsa::Database;
            self.executed
                .lock()
                .unwrap()
                .iter()
                .filter(|idx| {
                    self.ingredient_debug_name(**idx).contains("resolve_vfs_path_ci_scan_query")
                })
                .count()
        }
    }

    fn file_set_of(paths: &[&str]) -> FileSet {
        let mut file_set = FileSet::default();
        for (i, path) in paths.iter().enumerate() {
            file_set.insert(vfs::FileId(i as u32), VfsPath::new(*path));
        }
        file_set
    }

    fn db_with(paths: &[&str]) -> (ResolveDb, SourceRootInput) {
        let db = ResolveDb::default();
        let input = SourceRootInput::new(&db, crate::SourceRoot::new_local(file_set_of(paths)));
        (db, input)
    }

    #[test]
    fn a_case_variant_tail_resolves_with_modes() {
        let (db, root) = db_with(&["/w/cf/Roles/Admin.XML"]);
        let found = resolve_vfs_path_ci_query(
            &db,
            root,
            "/w/cf/Roles/Admin.xml".to_string(),
            &[M::Ci, M::StemExactExtCi],
        );
        assert!(found.is_some(), "расширение регистронезависимо при точном стебле");
    }

    #[test]
    fn an_object_stem_case_variant_never_resolves() {
        let (db, root) = db_with(&["/w/cf/Roles/ADMIN.XML"]);
        let found = resolve_vfs_path_ci_query(
            &db,
            root,
            "/w/cf/Roles/Admin.xml".to_string(),
            &[M::Ci, M::StemExactExtCi],
        );
        assert!(found.is_none(), "стебель — имя объекта, его регистр значим");
    }

    #[test]
    fn head_components_outside_the_mask_stay_exact() {
        let (db, root) = db_with(&["/w/CF/Roles/Admin.xml"]);
        let found = resolve_vfs_path_ci_query(
            &db,
            root,
            "/w/cf/Roles/Admin.xml".to_string(),
            &[M::Ci, M::StemExactExtCi],
        );
        assert!(found.is_none(), "головные компоненты пришли из реального пути и точны");
    }

    #[test]
    fn a_repeated_miss_scans_the_file_set_once() {
        let (db, root) = db_with(&["/w/cf/CommonModules/A/Ext/Module.bsl"]);
        let candidate = "/w/cf/Ext/ApplicationModule.bsl";
        for _ in 0..3 {
            let found =
                resolve_vfs_path_ci_query(&db, root, candidate.to_string(), &[M::Ci, M::Ci]);
            assert_eq!(found, None);
        }
        assert_eq!(db.scans(), 1, "один кандидат — один скан, сколько бы файлов ни спрашивало");
    }

    #[test]
    fn a_changed_file_set_rescans_and_finds_the_new_file() {
        let (mut db, root) = db_with(&["/w/cf/CommonModules/A/Ext/Module.bsl"]);
        let candidate = "/w/cf/Ext/ApplicationModule.bsl";
        assert_eq!(
            resolve_vfs_path_ci_query(&db, root, candidate.to_string(), &[M::Ci, M::Ci]),
            None
        );

        let grown = file_set_of(&[
            "/w/cf/CommonModules/A/Ext/Module.bsl",
            "/w/cf/EXT/APPLICATIONMODULE.BSL",
        ]);
        root.set_root(&mut db).to(crate::SourceRoot::new_local(grown));

        let found = resolve_vfs_path_ci_query(&db, root, candidate.to_string(), &[M::Ci, M::Ci]);
        assert_eq!(found, Some(vfs::FileId(1)), "новый файл виден после смены file set");
        assert_eq!(db.scans(), 2, "смена file set инвалидирует memo скана");
    }

    #[test]
    fn the_mask_is_part_of_the_memo_key() {
        let (db, root) = db_with(&["/w/cf/Ext/Товар.XML"]);
        let candidate = "/w/cf/Ext/Товар.xml";
        let lenient = resolve_vfs_path_ci_query(
            &db,
            root,
            candidate.to_string(),
            &[M::Ci, M::StemExactExtCi],
        );
        assert_eq!(lenient, Some(vfs::FileId(0)));
        let strict =
            resolve_vfs_path_ci_query(&db, root, candidate.to_string(), &[M::Ci, M::Exact]);
        assert_eq!(strict, None, "точная маска не получает ответ мягкой из memo");
        assert_eq!(db.scans(), 2, "разные маски одного кандидата — разные ключи");
    }
}
