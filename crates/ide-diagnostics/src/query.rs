use crate::slab::{self, OwnedBlock, SLAB_DIAGNOSTICS};
use crate::{
    body, normalize_diagnostics, scope_gate, AnalysisContext, BodyContext, Diagnostic,
    DiagnosticCode, DiagnosticTag, DiagnosticsConfig, DiagnosticsContext, Fix, TextEdit,
};
use base_db::{DiagnosticsConfigId, FileIdInput};
use hir::{DefWithBodyId, LocalRange, MethodIdInput, MethodOffset, ModuleId};
use ide_db::RootDatabase;
use std::sync::Arc;
use stdx::heap::vec_bytes;

/// Approximate live heap of a memoised diagnostics list, for salsa's
/// `heap_size` hook: the backing vec plus each diagnostic's owned message,
/// tags, and fix payloads. New heap-owning fields in [`Diagnostic`] or
/// [`Fix`] must be added here too.
fn diagnostics_heap<R>(v: &Arc<Vec<Diagnostic<R>>>) -> usize {
    let mut bytes = vec_bytes::<Diagnostic<R>>(v.len());
    for d in v.iter() {
        bytes += d.message.capacity();
        bytes += vec_bytes::<DiagnosticTag>(d.tags.len());
        bytes += vec_bytes::<Fix<R>>(d.fixes.len());
        for fix in &d.fixes {
            bytes += fix.label.capacity();
            bytes += vec_bytes::<TextEdit<R>>(fix.edits.len());
            for edit in &fix.edits {
                bytes += edit.new_text.capacity();
            }
        }
    }
    bytes
}

/// Approximate live heap of a memoised configuration: the code lists and the
/// per-code tables. Parameter values are JSON trees whose exact footprint is
/// not worth walking for a cache of sixteen entries; each counts as its
/// serialised length.
fn config_heap(v: &Arc<DiagnosticsConfig>) -> usize {
    use crate::DiagnosticCode;
    let mut bytes = std::mem::size_of::<DiagnosticsConfig>();
    bytes += vec_bytes::<DiagnosticCode>(v.disabled.len());
    bytes += vec_bytes::<DiagnosticCode>(v.enabled.len());
    bytes += v.only_enabled.as_ref().map_or(0, |codes| vec_bytes::<DiagnosticCode>(codes.len()));
    bytes += v.parameters.capacity() * std::mem::size_of::<(DiagnosticCode, serde_json::Value)>();
    bytes += v.parameters.values().map(|value| value.to_string().len()).sum::<usize>();
    bytes += v.metadata_overrides.capacity()
        * std::mem::size_of::<(DiagnosticCode, crate::config::MetadataOverride)>();
    bytes
}

/// The runtime configuration behind an interned key, built once per key
/// rather than once per body that reads it.
#[salsa::tracked(lru = 16, heap_size = config_heap, returns(clone))]
fn diagnostics_config_query<'db>(
    db: &'db dyn RootDatabase,
    config_id: DiagnosticsConfigId<'db>,
) -> Arc<DiagnosticsConfig> {
    Arc::new(DiagnosticsConfig::from_input(config_id.config(db)))
}

/// One method's diagnostics in its own coordinates. Reads the method's syntax
/// and lowering and the module's position-free interface, so an edit
/// elsewhere in the file leaves this memo valid; retained at the cap of the
/// per-method chain it sits on top of.
#[salsa::tracked(lru = 8192, heap_size = diagnostics_heap, returns(clone))]
pub fn method_diagnostics_query<'db>(
    db: &'db dyn RootDatabase,
    method: MethodIdInput<'db>,
    config_id: DiagnosticsConfigId<'db>,
) -> Arc<Vec<Diagnostic<LocalRange>>> {
    let _span = tracing::info_span!("method_diagnostics", ?method).entered();
    let method_id = method.method_id(db);
    let (Some(syntax), Some(lower)) =
        (hir::method_syntax_query(db, method), db.method_lower(method))
    else {
        return Arc::new(Vec::new());
    };
    let file_id = method_id.module.file_id;
    let config = diagnostics_config_query(db, config_id);
    let provider = ide_db::SalsaProvider::new(db, ide_db::configuration_path_for_file(db, file_id));
    let analysis = AnalysisContext::new(&config, file_id, &provider);
    let ctx = BodyContext::new(
        &analysis,
        DefWithBodyId::Method(method_id.local_id),
        syntax.detached_root(),
        &lower,
    );
    Arc::new(body::body_diagnostics(&ctx))
}

/// The module-level code's diagnostics, in file coordinates already: its
/// root is the file root.
#[salsa::tracked(lru = 256, heap_size = diagnostics_heap, returns(clone))]
pub fn module_code_diagnostics_query<'db>(
    db: &'db dyn RootDatabase,
    file_id_input: FileIdInput<'db>,
    config_id: DiagnosticsConfigId<'db>,
) -> Arc<Vec<Diagnostic<LocalRange>>> {
    let file_id = file_id_input.file_id(db);
    let _span = tracing::info_span!("module_code_diagnostics", ?file_id).entered();
    let lower = hir::module_code_lower_query(db, file_id_input);
    let config = diagnostics_config_query(db, config_id);
    let provider = ide_db::SalsaProvider::new(db, ide_db::configuration_path_for_file(db, file_id));
    let analysis = AnalysisContext::new(&config, file_id, &provider);
    let root = db.parse(file_id).syntax_node();
    let ctx = BodyContext::new(&analysis, DefWithBodyId::ModuleCode, root, lower);
    Arc::new(body::body_diagnostics(&ctx))
}

fn lines_heap(v: &Arc<[u32]>) -> usize {
    vec_bytes::<u32>(v.len())
}

/// Строки описания методов по эталону `LineLength` — по всему файлу, слепо
/// к узлам. Исполняется только при выключенном `checkMethodDescription`.
#[salsa::tracked(lru = 128, heap_size = lines_heap, returns(ref))]
fn module_description_lines_query<'db>(
    db: &'db dyn RootDatabase,
    file_id_input: FileIdInput<'db>,
) -> Arc<[u32]> {
    let file_id = file_id_input.file_id(db);
    let _span = tracing::info_span!("module_description_lines", file_id = file_id.0).entered();
    let parse = db.parse(file_id);
    let layout = hir::module_slab_layout_query(db, file_id_input);
    crate::handlers::line_length::find_method_description_lines(
        &parse.syntax_node(),
        layout.line_index(),
    )
    .into()
}

/// Строки описания внутри плиты метода, от её начала: проекция файлового
/// множества, которая не меняется, пока не изменились сами строки.
#[salsa::tracked(lru = 8192, heap_size = lines_heap, returns(ref))]
fn method_description_lines_query<'db>(
    db: &'db dyn RootDatabase,
    method: MethodIdInput<'db>,
) -> Arc<[u32]> {
    let method_id = method.method_id(db);
    let file_id_input = FileIdInput::new(db, method_id.module.file_id);
    let layout = hir::module_slab_layout_query(db, file_id_input);
    let Some(span) = layout.span(method_id.local_id) else { return Arc::from([]) };
    let all = module_description_lines_query(db, file_id_input);
    slab::project_lines(all, span.first_line, span.last_line).into()
}

/// Строчные диагностики одной плиты в её координатах: читает плиту метода
/// (текст её строк и контекст соседа) и конфиг — ничего позиционного, так
/// что правка в другом методе оставляет мемо в силе.
#[salsa::tracked(lru = 8192, heap_size = diagnostics_heap, returns(clone))]
pub fn method_line_diagnostics_query<'db>(
    db: &'db dyn RootDatabase,
    method: MethodIdInput<'db>,
    config_id: DiagnosticsConfigId<'db>,
) -> Arc<Vec<Diagnostic<LocalRange>>> {
    let method_id = method.method_id(db);
    let file_id = method_id.module.file_id;
    let _span =
        tracing::info_span!("method_line_diagnostics", file_id = file_id.0, local_id = ?method_id.local_id)
            .entered();
    let Some(slab_value) = hir::method_slab_query(db, method) else { return Arc::new(Vec::new()) };
    let config = diagnostics_config_query(db, config_id);
    if !config.any_enabled(SLAB_DIAGNOSTICS) {
        return Arc::new(Vec::new());
    }
    let provider = ide_db::SalsaProvider::new(db, ide_db::configuration_path_for_file(db, file_id));
    let analysis = AnalysisContext::new(&config, file_id, &provider);
    let described: Arc<[u32]> =
        if analysis.config_bool(DiagnosticCode::LineLength, "checkMethodDescription", true) {
            Arc::from([])
        } else {
            method_description_lines_query(db, method).clone()
        };
    let owned = OwnedBlock::new(&slab_value.text);
    let block = owned.block(&described, slab_value.leading);
    Arc::new(slab::check_block_all(&analysis, &block))
}

// Sized for a whole workspace of answers: a file's list is small next to the chains
// it was computed from, and a sweep that trims those chains away keeps its answers,
// so a repeated sweep over the same files is a lookup, not a recomputation.
#[salsa::tracked(lru = 32768, heap_size = diagnostics_heap, returns(clone))]
pub fn file_diagnostics_query<'db>(
    db: &'db dyn RootDatabase,
    file_id_input: FileIdInput<'db>,
    config_id: DiagnosticsConfigId<'db>,
) -> Arc<Vec<Diagnostic>> {
    let total_start = std::time::Instant::now();
    let file_id = file_id_input.file_id(db);
    let config = diagnostics_config_query(db, config_id);

    let _span = tracing::info_span!("file_diagnostics_query", file_id = file_id.0,).entered();

    if !scope_gate::file_in_scope(db, None, file_id, &config) {
        return Arc::new(Vec::new());
    }
    let config_path_input = ide_db::configuration_path_for_file(db, file_id);
    let provider = ide_db::SalsaProvider::new(db, config_path_input);
    let ctx = DiagnosticsContext::new(&config, file_id, &provider);

    // The per-body and per-slab memos, lifted by the positional half of the
    // file: the only place the method offsets are read.
    let module_id = ModuleId::new(file_id);
    let item_tree = db.item_tree(file_id);
    let layout = config
        .any_enabled(SLAB_DIAGNOSTICS)
        .then(|| hir::module_slab_layout_query(db, file_id_input));
    let mut standalone = Vec::new();
    for decl in db.module_interface_ref(module_id).methods() {
        db.unwind_if_revision_cancelled();
        let Some((range, _)) = item_tree.method_at(decl.id.local_id) else { continue };
        let base = MethodOffset::new(range.start());
        let method = MethodIdInput::new(db, decl.id);
        let local = method_diagnostics_query(db, method, config_id);
        standalone.extend(local.iter().cloned().map(|d| d.lift(base)));
        if let Some(layout) = layout {
            if let Some(span) = layout.span(decl.id.local_id) {
                let base = MethodOffset::new(layout.line_index().line_start(span.first_line));
                let lines = method_line_diagnostics_query(db, method, config_id);
                standalone.extend(lines.iter().cloned().map(|d| d.lift(base)));
            }
        }
    }
    let module_code = module_code_diagnostics_query(db, file_id_input, config_id);
    standalone.extend(module_code.iter().cloned().map(|d| d.lift(MethodOffset::ZERO)));

    standalone.extend(crate::module_diagnostics(&ctx));
    if let Some(layout) = layout {
        standalone.extend(slab::collect_remainder(&ctx, layout));
        if slab::slab_verify_enabled() {
            slab::verify_assembled(&ctx, &standalone);
        }
    }
    normalize_diagnostics(&mut standalone);

    let diagnostics =
        crate::apply_extension_merge(db, file_id, &config, config_path_input, None, standalone);
    tracing::info!(
        file_id = file_id.0,
        diagnostic_count = diagnostics.len(),
        elapsed_ms = total_start.elapsed().as_millis() as u64,
        "file diagnostics query complete",
    );

    Arc::new(diagnostics)
}

/// Switch the per-method diagnostics cap between the interactive profile and
/// the sweep profile, in step with the lowering and dataflow chains.
pub fn set_diagnostics_lru_sweep_mode(db: &mut dyn RootDatabase, sweep: bool) {
    const METHOD_INTERACTIVE: usize = 8192;
    const METHOD_SWEEP: usize = 2048;
    let cap = if sweep { METHOD_SWEEP } else { METHOD_INTERACTIVE };
    method_diagnostics_query::set_lru_capacity(db, cap);
    method_line_diagnostics_query::set_lru_capacity(db, cap);
    method_description_lines_query::set_lru_capacity(db, cap);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiagnosticCode, Severity};
    use ide_db::TextRange;

    #[test]
    fn diagnostics_heap_counts_messages_and_fixes() {
        let message = "переменная не используется: ОченьДлинноеИмяПеременной".to_string();
        let fix_label = "Удалить переменную".to_string();
        let new_text = String::new();
        let owned = message.capacity() + fix_label.capacity() + new_text.capacity();
        let diagnostics = Arc::new(vec![Diagnostic {
            code: DiagnosticCode::UnreachableCode,
            message,
            severity: Severity::Warning,
            range: TextRange::new(0.into(), 10.into()),
            tags: vec![DiagnosticTag::Unnecessary],
            fixes: vec![Fix::safe(
                fix_label,
                vec![TextEdit { range: TextRange::new(0.into(), 10.into()), new_text }],
            )],
        }]);
        let bytes = diagnostics_heap(&diagnostics);
        // At least the owned string payloads plus the vec backing stores; well
        // under a kilobyte for a single small diagnostic.
        assert!(bytes > owned);
        assert!(bytes < 1024);
    }
}
