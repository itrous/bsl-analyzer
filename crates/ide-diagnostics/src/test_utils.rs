use crate::{Diagnostic, DiagnosticCode, Severity};
use hir::DefDatabase;
use ide_db::TextRange;
use stdx::case::CaseExt;

pub fn range_to_line_col(text: &str, range: TextRange) -> (u32, u32, u32, u32) {
    let start_offset: usize = range.start().into();
    let end_offset: usize = range.end().into();

    let mut line = 0u32;
    let mut col = 0u32;
    let mut byte_pos = 0usize;
    let mut start_line = 0u32;
    let mut start_col = 0u32;
    let mut end_line = 0u32;
    let mut end_col = 0u32;

    for ch in text.chars() {
        if byte_pos == start_offset {
            start_line = line;
            start_col = col;
        }
        if byte_pos == end_offset {
            end_line = line;
            end_col = col;
            break;
        }

        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
        byte_pos += ch.len_utf8();
    }

    if byte_pos == end_offset {
        end_line = line;
        end_col = col;
    }

    (start_line, start_col, end_line, end_col)
}

pub fn assert_diagnostic_message_at_line(
    code: &str,
    diagnostics: &[&Diagnostic],
    expected_line: u32,
    expected_message_part: &str,
) {
    let matching = diagnostics.iter().find(|d| {
        let start: u32 = d.range.start().into();
        let line = code[..start as usize].matches('\n').count() as u32;
        line == expected_line && d.message.contains(expected_message_part)
    });

    assert!(
        matching.is_some(),
        "No diagnostic with message containing '{}' at line {}.\nDiagnostics at that line: {:?}",
        expected_message_part,
        expected_line,
        diagnostics
            .iter()
            .filter(|d| {
                let start: u32 = d.range.start().into();
                code[..start as usize].matches('\n').count() as u32 == expected_line
            })
            .map(|d| &d.message)
            .collect::<Vec<_>>()
    );
}

pub fn assert_diagnostic_range(
    code: &str,
    diagnostic: &Diagnostic,
    expected_line: u32,
    expected_start_col: u32,
    expected_end_col: u32,
) {
    let (start_line, start_col, end_line, end_col) = range_to_line_col(code, diagnostic.range);

    assert_eq!(
        start_line, expected_line,
        "Diagnostic at wrong line. Expected line {}, got line {}.\nMessage: {}",
        expected_line, start_line, diagnostic.message
    );

    assert_eq!(
        end_line, expected_line,
        "Diagnostic spans multiple lines unexpectedly. Expected single line {}.\nMessage: {}",
        expected_line, diagnostic.message
    );

    assert_eq!(
        start_col, expected_start_col,
        "Diagnostic start column mismatch on line {}. Expected col {}, got col {}.\nMessage: {}",
        expected_line, expected_start_col, start_col, diagnostic.message
    );

    assert_eq!(
        end_col, expected_end_col,
        "Diagnostic end column mismatch on line {}. Expected col {}, got col {}.\nMessage: {}",
        expected_line, expected_end_col, end_col, diagnostic.message
    );
}

pub fn assert_diagnostic_range_multiline(
    code: &str,
    diagnostic: &Diagnostic,
    expected_start_line: u32,
    expected_start_col: u32,
    expected_end_line: u32,
    expected_end_col: u32,
) {
    let (start_line, start_col, end_line, end_col) = range_to_line_col(code, diagnostic.range);

    assert_eq!(
        start_line, expected_start_line,
        "Diagnostic start line mismatch. Expected line {}, got line {}.\nMessage: {}",
        expected_start_line, start_line, diagnostic.message
    );

    assert_eq!(
        start_col, expected_start_col,
        "Diagnostic start column mismatch on line {}. Expected col {}, got col {}.\nMessage: {}",
        expected_start_line, expected_start_col, start_col, diagnostic.message
    );

    assert_eq!(
        end_line, expected_end_line,
        "Diagnostic end line mismatch. Expected line {}, got line {}.\nMessage: {}",
        expected_end_line, end_line, diagnostic.message
    );

    assert_eq!(
        end_col, expected_end_col,
        "Diagnostic end column mismatch on line {}. Expected col {}, got col {}.\nMessage: {}",
        expected_end_line, expected_end_col, end_col, diagnostic.message
    );
}

pub(crate) fn create_test_db(code: &str) -> (ide_db::RootDatabaseImpl, vfs::FileId) {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use test_fixture::Fixture;

    let fixture_text = format!("//- /test.bsl\n{}", code);
    let fixture = Fixture::parse(&fixture_text);
    let file_id = fixture.first_file().expect("fixture should have at least one file");

    let mut db = RootDatabaseImpl::new();

    let mut file_set = vfs::FileSet::default();
    file_set.insert(file_id, vfs::VfsPath::new("/test.bsl"));
    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(file_id, SourceRootId(0));

    for (fid, file) in &fixture.files {
        db.set_file_text(*fid, &file.content);
    }

    (db, file_id)
}

pub fn check_ast_diagnostic<F>(code: &str, check_fn: F) -> Vec<Diagnostic>
where
    F: Fn(&crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    let config = crate::DiagnosticsConfig::all_enabled();
    check_ast_diagnostic_with_config(code, config, check_fn)
}

pub fn check_ast_diagnostic_with_config<F>(
    code: &str,
    config: crate::DiagnosticsConfig,
    check_fn: F,
) -> Vec<Diagnostic>
where
    F: Fn(&crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    let (db, file_id) = create_test_db(code);
    let provider = ide_db::SalsaProvider::new(&db, None);
    let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);
    check_fn(&ctx)
}

/// Run one body check over every body of `code` (methods and module code),
/// lifted into the file — the per-body counterpart of [`check_ast_diagnostic`].
pub fn check_body_diagnostic<F>(code: &str, check: F) -> Vec<Diagnostic>
where
    F: Fn(&crate::BodyContext, &mut Vec<Diagnostic<hir::LocalRange>>),
{
    check_body_diagnostic_with_config(code, crate::DiagnosticsConfig::all_enabled(), check)
}

pub fn check_body_diagnostic_with_config<F>(
    code: &str,
    config: crate::DiagnosticsConfig,
    check: F,
) -> Vec<Diagnostic>
where
    F: Fn(&crate::BodyContext, &mut Vec<Diagnostic<hir::LocalRange>>),
{
    let (db, file_id) = create_test_db(code);
    let provider = ide_db::SalsaProvider::new(&db, None);
    let ctx = crate::DiagnosticsContext::new(&config, file_id, &provider);
    let mut diagnostics =
        crate::body::for_each_body_context(&ctx, |body_ctx, acc| check(body_ctx, acc));
    diagnostics.sort_by_key(|d| (d.range.start(), d.range.end()));
    diagnostics
}

/// One code's findings from the production pipeline — the path a handler
/// really runs on (single pass, body memo, file collector), not a driver
/// the test picks for it.
pub fn check_diagnostics_for(code: &str, diagnostic: crate::DiagnosticCode) -> Vec<Diagnostic> {
    check_diagnostics_for_with_config(code, crate::DiagnosticsConfig::all_enabled(), diagnostic)
}

pub fn check_diagnostics_for_with_config(
    code: &str,
    config: crate::DiagnosticsConfig,
    diagnostic: crate::DiagnosticCode,
) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = check_file_diagnostics_with_config(code, config)
        .into_iter()
        .filter(|d| d.code == diagnostic)
        .collect();
    diagnostics.sort_by_key(|d| (d.range.start(), d.range.end()));
    diagnostics
}

pub fn check_hir_diagnostic(code: &str) -> Vec<Diagnostic> {
    check_ast_diagnostic(code, crate::diagnostics)
}

/// Like [`check_hir_diagnostic`] but through the full `file_diagnostics` pipeline, so the
/// `apply_extension_merge` stage — where in-code suppression directives are applied — runs.
pub fn check_file_diagnostics(code: &str) -> Vec<Diagnostic> {
    check_file_diagnostics_with_config(code, crate::DiagnosticsConfig::all_enabled())
}

pub fn check_file_diagnostics_with_config(
    code: &str,
    config: crate::DiagnosticsConfig,
) -> Vec<Diagnostic> {
    let (db, file_id) = create_test_db(code);
    crate::file_diagnostics(&db, file_id, &config)
}

pub fn check_file_diagnostics_snapshot(source: &str, expected: expect_test::Expect) {
    let diagnostics = check_file_diagnostics(source);
    expected.assert_eq(&format_diags(source, &diagnostics));
}

pub fn format_diags(source: &str, diags: &[Diagnostic]) -> String {
    let mut entries = diags
        .iter()
        .map(|diag| {
            let (start_line, start_col, end_line, end_col) = range_to_line_col(source, diag.range);
            FormattedDiag {
                diag,
                start_line: start_line + 1,
                start_col: start_col + 1,
                end_line: end_line + 1,
                end_col: end_col + 1,
            }
        })
        .collect::<Vec<_>>();

    entries.sort_by(|left, right| {
        (
            left.start_line,
            left.start_col,
            left.end_line,
            left.end_col,
            left.diag.code as u16,
            severity_sort_key(left.diag.severity),
            left.diag.message.as_str(),
        )
            .cmp(&(
                right.start_line,
                right.start_col,
                right.end_line,
                right.end_col,
                right.diag.code as u16,
                severity_sort_key(right.diag.severity),
                right.diag.message.as_str(),
            ))
    });

    let mut output = String::new();
    for (idx, entry) in entries.iter().enumerate() {
        if idx > 0 {
            output.push('\n');
        }
        use std::fmt::Write as _;
        writeln!(
            output,
            "{} @ {}:{}..{}:{}",
            entry.diag.code.as_str(),
            entry.start_line,
            entry.start_col,
            entry.end_line,
            entry.end_col
        )
        .expect("writing to String should not fail");
        writeln!(output, "  message: {}", entry.diag.message)
            .expect("writing to String should not fail");
        write!(output, "  severity: {}", entry.diag.severity.as_str())
            .expect("writing to String should not fail");
    }
    output
}

pub fn check_diagnostics_snapshot(source: &str, expected: expect_test::Expect) {
    let diagnostics = check_hir_diagnostic(source);
    expected.assert_eq(&format_diags(source, &diagnostics));
}

pub fn check_diagnostics_snapshot_for(
    source: &str,
    code_filter: DiagnosticCode,
    expected: expect_test::Expect,
) {
    let diagnostics = check_hir_diagnostic(source);
    expected.assert_eq(&format_diags_for(source, &diagnostics, code_filter));
}

/// Apply a fix's edits to `source` and return the resulting text. Edits are spliced
/// left-to-right by their byte ranges (fixes never emit overlapping edits).
pub fn apply_edits(source: &str, edits: &[crate::TextEdit]) -> String {
    let mut sorted: Vec<&crate::TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|edit| edit.range.start());

    let mut result = String::new();
    let mut cursor = 0usize;
    for edit in sorted {
        let start: usize = edit.range.start().into();
        let end: usize = edit.range.end().into();
        result.push_str(&source[cursor..start]);
        result.push_str(&edit.new_text);
        cursor = end;
    }
    result.push_str(&source[cursor..]);
    result
}

/// Snapshot every quick fix attached to diagnostics matching `code_filter`: the fix
/// label, its `safe_for_fix_all` flag, and the full source after applying that fix in
/// isolation. Proves the fix payload — which [`format_diags`] deliberately omits.
pub fn check_fix_snapshot_for(
    source: &str,
    code_filter: DiagnosticCode,
    expected: expect_test::Expect,
) {
    use std::fmt::Write as _;
    let diagnostics = check_hir_diagnostic(source);
    let mut output = String::new();
    for diag in diagnostics.iter().filter(|diag| diag.code == code_filter) {
        let (start_line, start_col, end_line, end_col) = range_to_line_col(source, diag.range);
        for fix in &diag.fixes {
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            writeln!(
                output,
                "{} @ {}:{}..{}:{} — {} [fix_all={}]",
                diag.code.as_str(),
                start_line + 1,
                start_col + 1,
                end_line + 1,
                end_col + 1,
                fix.label,
                fix.safe_for_fix_all,
            )
            .expect("writing to String should not fail");
            write!(output, "{}", apply_edits(source, &fix.edits))
                .expect("writing to String should not fail");
        }
    }
    expected.assert_eq(&output);
}

struct FormattedDiag<'a> {
    diag: &'a Diagnostic,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
}

fn format_diags_for(source: &str, diags: &[Diagnostic], code_filter: DiagnosticCode) -> String {
    let filtered =
        diags.iter().filter(|diag| diag.code == code_filter).cloned().collect::<Vec<_>>();
    format_diags(source, &filtered)
}

fn severity_sort_key(severity: Severity) -> u8 {
    match severity {
        Severity::Blocker => 0,
        Severity::Critical => 1,
        Severity::Major => 2,
        Severity::Error => 3,
        Severity::Warning => 4,
        Severity::Information => 5,
        Severity::Hint => 6,
    }
}

pub fn check_hir_diagnostic_with_fixtures(fixture_text: &str) -> Vec<Diagnostic> {
    check_hir_diagnostic_with_unreadable(fixture_text, &[])
}

/// [`check_hir_diagnostic_with_fixtures`], with the named fixture files registered
/// as bodies that exist but could not be read.
///
/// The state is set through the database rather than by making a real file
/// unreadable on disk: this fixture never touches the disk at all, and the file
/// permissions that would express it are ignored by root anyway, which would leave
/// the stand green in a container no matter what the code did.
pub fn check_hir_diagnostic_with_unreadable(
    fixture_text: &str,
    unreadable: &[&str],
) -> Vec<Diagnostic> {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use test_fixture::Fixture;

    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = vfs::FileSet::default();
    let mut marked = 0usize;
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        let path = file.path.as_path().to_string_lossy().replace('\\', "/");
        if unreadable.iter().any(|u| path.ends_with(u)) {
            db.set_file_unreadable(*file_id);
            marked += 1;
        } else {
            db.set_file_text(*file_id, &file.content);
        }
    }
    assert_eq!(
        marked,
        unreadable.len(),
        "every named body must exist in the fixture, or the stand proves nothing"
    );

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);

    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    let test_file = *fixture.files.keys().last().expect("Fixture should have at least one file");

    let config = crate::DiagnosticsConfig::all_enabled();
    let provider = ide_db::SalsaProvider::new(&db, None);
    let ctx = crate::DiagnosticsContext::new(&config, test_file, &provider);

    crate::diagnostics(&ctx)
}

pub fn check_metadata_diagnostic_with_fixtures<F>(
    metadata: hir::ModuleMetadata,
    fixture_text: &str,
    check_fn: F,
) -> Vec<Diagnostic>
where
    F: Fn(&hir::ModuleMetadata, &crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    use std::rc::Rc;
    use std::sync::Arc;

    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use test_fixture::Fixture;

    let fixture = Fixture::parse(fixture_text);
    let mut db = RootDatabaseImpl::new();

    let mut file_set = vfs::FileSet::default();
    for (file_id, file) in &fixture.files {
        file_set.insert(*file_id, file.path.clone());
        db.set_file_text(*file_id, &file.content);
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);

    for file_id in fixture.files.keys() {
        db.set_file_source_root(*file_id, SourceRootId(0));
    }

    let test_file = *fixture.files.keys().last().expect("Fixture should have at least one file");
    let metadata_arc = Arc::new(metadata);
    let config_rc = Rc::new(crate::DiagnosticsConfig::all_enabled());
    let provider_impl =
        MetadataTestProvider { db, metadata: Arc::clone(&metadata_arc), configuration: None };
    let ctx = crate::DiagnosticsContext::new(
        &config_rc,
        test_file,
        &provider_impl as &dyn ide_db::provider::AnalysisProvider,
    );

    check_fn(&metadata_arc, &ctx)
}

pub fn check_sdbl_diagnostic<F>(code: &str, check_fn: F) -> Vec<Diagnostic>
where
    F: Fn(&crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    check_ast_diagnostic(code, check_fn)
}

pub fn check_dataflow_diagnostic<F>(code: &str, check_fn: F) -> Vec<Diagnostic>
where
    F: Fn(&crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    check_ast_diagnostic(code, check_fn)
}

pub fn check_hir_diagnostic_with_config<F>(
    code: &str,
    config: crate::DiagnosticsConfig,
    check_fn: F,
) -> Vec<Diagnostic>
where
    F: Fn(&crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    check_ast_diagnostic_with_config(code, config, check_fn)
}

struct MetadataTestProvider {
    db: ide_db::RootDatabaseImpl,
    metadata: std::sync::Arc<hir::ModuleMetadata>,
    configuration: Option<std::sync::Arc<bsl_metadata::Configuration>>,
}

impl ide_db::provider::AnalysisProvider for MetadataTestProvider {
    fn configuration(&self) -> Option<std::sync::Arc<bsl_metadata::Configuration>> {
        self.configuration.clone()
    }

    fn visible_configurations(
        &self,
        _file_id: vfs::FileId,
    ) -> Vec<ide_db::provider::VisibleConfigWithRoot> {
        match &self.configuration {
            Some(cfg) => vec![ide_db::provider::VisibleConfigWithRoot {
                config: bsl_config::VisibleConfig {
                    name: None,
                    configuration: std::sync::Arc::clone(cfg),
                },
                root: std::path::PathBuf::from("/test"),
            }],
            None => Vec::new(),
        }
    }

    fn module_members(
        &self,
        source_root_id: ide_db::base_db::SourceRootId,
    ) -> std::sync::Arc<hir::WorkspaceMembers> {
        self.db.module_members(source_root_id)
    }

    fn module_index(
        &self,
        source_root_id: ide_db::base_db::SourceRootId,
    ) -> std::sync::Arc<hir::ModuleIndex> {
        self.db.module_index(source_root_id)
    }

    fn kernel_type_display(&self, id: hir::TypeId, locale: base_db::Locale) -> String {
        hir::kernel_type_label(&self.db, id, locale, false)
    }

    fn parse(&self, file_id: vfs::FileId) -> syntax::Parse<syntax::SyntaxNode> {
        use ide_db::base_db::RootQueryDb;
        self.db.parse(file_id)
    }

    fn file_text(&self, file_id: vfs::FileId) -> String {
        use ide_db::base_db::SourceDatabase;
        self.db.file_text(file_id).to_string()
    }

    fn item_tree(&self, file_id: vfs::FileId) -> std::sync::Arc<hir::ItemTree> {
        use hir::DefDatabase;
        self.db.item_tree(file_id)
    }

    fn symbol_tree(&self, module_id: hir::ModuleId) -> std::sync::Arc<hir::SymbolTree> {
        use hir::DefDatabase;
        self.db.symbol_tree(module_id)
    }

    fn region_tree(&self, file_id: vfs::FileId) -> std::sync::Arc<hir::RegionTree> {
        use hir::DefDatabase;
        self.db.region_tree(file_id)
    }

    fn module_bodies(&self, module_id: hir::ModuleId) -> std::sync::Arc<hir::ModuleBodies> {
        use hir::DefDatabase;
        self.db.module_bodies(module_id)
    }

    fn module_metadata(&self, _module_id: hir::ModuleId) -> std::sync::Arc<hir::ModuleMetadata> {
        std::sync::Arc::clone(&self.metadata)
    }

    fn call_summary(&self, module_id: hir::ModuleId) -> std::sync::Arc<hir::ModuleCallSummary> {
        use hir::DefDatabase;

        let file_id = module_id.file_id;
        let item_tree = self.db.item_tree(file_id);
        let module_bodies = self.db.module_bodies(module_id);
        let form_handlers: &[bsl_metadata::FormEventHandler] =
            self.metadata.form.as_ref().map(|form| form.event_handlers.as_slice()).unwrap_or(&[]);

        std::sync::Arc::new(hir::call_graph::extract_call_summary(
            &item_tree,
            &module_bodies,
            form_handlers,
        ))
    }

    fn reaching_definitions(
        &self,
        method_id: hir::MethodId,
    ) -> Option<std::sync::Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        use ide_db::RootDatabase;
        self.db.reaching_definitions(method_id)
    }

    fn line_index(&self, file_id: vfs::FileId) -> std::sync::Arc<line_index::LineIndex> {
        use ide_db::base_db::SourceDatabase;
        std::sync::Arc::new(line_index::LineIndex::new(&self.db.file_text(file_id)))
    }

    fn file_path(&self, _file_id: vfs::FileId) -> Option<String> {
        None
    }

    fn file_source_root_id(&self, _file_id: vfs::FileId) -> ide_db::base_db::SourceRootId {
        ide_db::base_db::SourceRootId(0)
    }

    fn sdbl_hir_in_file(
        &self,
        file_id: vfs::FileId,
    ) -> std::sync::Arc<Vec<(hir::SdblExprId, std::sync::Arc<sdbl_hir::SdblPackage>)>> {
        use ide_db::RootDatabase;
        self.db.sdbl_hir_in_file(file_id)
    }

    fn all_sdbl_in_file(
        &self,
        file_id: vfs::FileId,
    ) -> std::sync::Arc<Vec<(hir::SdblExprId, syntax::SdblQueryInfo)>> {
        use ide_db::RootDatabase;
        self.db.all_sdbl_in_file(file_id)
    }

    fn module_data(&self, module_id: hir::ModuleId) -> std::sync::Arc<hir::ModuleData> {
        use hir::DefDatabase;
        self.db.module_data(module_id)
    }

    fn method_docs(&self, method_id: hir::MethodId) -> Option<std::sync::Arc<hir::MethodDocs>> {
        self.db.method_docs(method_id)
    }

    fn variable_docs(
        &self,
        variable_id: hir::VariableId,
    ) -> Option<std::sync::Arc<hir::VariableDocs>> {
        self.db.variable_docs(variable_id)
    }

    fn module_interface(&self, module_id: hir::ModuleId) -> std::sync::Arc<hir::ModuleInterface> {
        use hir::DefDatabase;
        self.db.module_interface(module_id)
    }

    fn method_syntax(&self, method_id: hir::MethodId) -> Option<syntax::SyntaxNode> {
        let input = hir::MethodIdInput::new(&self.db, method_id);
        hir::method_syntax_query(&self.db, input).as_ref().map(|syntax| syntax.detached_root())
    }

    fn infer_owner(
        &self,
        file_id: vfs::FileId,
        owner: hir::DefWithBodyId,
    ) -> hir::InferOwnerResult {
        hir::infer_owner(&self.db, file_id, owner)
    }

    fn arg_diagnostics_of(
        &self,
        file_id: vfs::FileId,
        owner: hir::DefWithBodyId,
    ) -> std::sync::Arc<Vec<hir::InferenceDiagnostic>> {
        use hir::HirDatabase;
        match owner {
            hir::DefWithBodyId::Method(local_id) => {
                let method_id = hir::MethodId { module: hir::ModuleId::new(file_id), local_id };
                let input = hir::MethodIdInput::new(&self.db, method_id);
                self.db.method_arg_diagnostics(input)
            }
            hir::DefWithBodyId::ModuleCode => self.db.module_code_arg_diagnostics(file_id),
        }
    }

    fn module_recursive_methods(
        &self,
        file_id: vfs::FileId,
    ) -> std::sync::Arc<rustc_hash::FxHashSet<hir::MethodKey>> {
        let input = ide_db::base_db::FileIdInput::new(&self.db, file_id);
        ide_db::queries::module_recursive_methods_query(&self.db, input)
    }

    fn module_level_cfg(&self, file_id: vfs::FileId) -> std::sync::Arc<hir::cfg::ControlFlowGraph> {
        use ide_db::RootDatabase;
        self.db.module_level_cfg(hir::ModuleId::new(file_id))
    }

    fn module_code_reaching_definitions(
        &self,
        file_id: vfs::FileId,
    ) -> Option<std::sync::Arc<hir::dataflow::reaching_defs::ReachingDefsResult>> {
        use hir::HirDatabase;
        self.db.module_code_reaching_definitions(file_id)
    }

    fn method_cfg(&self, method_id: hir::MethodId) -> std::sync::Arc<hir::cfg::ControlFlowGraph> {
        use ide_db::RootDatabase;
        self.db.method_cfg(method_id)
    }

    fn method_path_terminates(
        &self,
        method_id: hir::MethodId,
    ) -> Option<std::sync::Arc<hir::dataflow::path_terminates::PathTerminatesResult>> {
        use ide_db::RootDatabase;
        self.db.method_path_terminates(method_id)
    }

    fn file_external_refs(
        &self,
        module_id: hir::ModuleId,
    ) -> std::sync::Arc<Vec<hir::ExternalRef>> {
        use hir::DefDatabase;
        self.db.file_external_refs(module_id)
    }

    fn resolve_vfs_path(
        &self,
        source_root_id: ide_db::base_db::SourceRootId,
        path: &vfs::VfsPath,
    ) -> Option<vfs::FileId> {
        use ide_db::base_db::SourceDatabase;
        self.db.resolve_vfs_path(source_root_id, path)
    }

    fn resolve_module_file(&self, _relative_uri: &str) -> Option<vfs::FileId> {
        None
    }

    fn method_effect_summary(
        &self,
        method: hir::MethodId,
    ) -> std::sync::Arc<hir::dataflow::effect_summary::EffectSummary> {
        let method_input = hir::MethodIdInput::new(&self.db, method);
        ide_db::effects::method_effect_summary_query(&self.db, method_input)
    }

    fn method_security_state(
        &self,
        method: hir::MethodId,
    ) -> Option<
        std::sync::Arc<
            hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>,
        >,
    > {
        use ide_db::RootDatabase;
        self.db.method_security_state(method)
    }

    fn module_code_security_state(
        &self,
        file_id: vfs::FileId,
    ) -> Option<
        std::sync::Arc<
            hir::dataflow::DataflowResult<hir::dataflow::security_state::SecurityModeState>,
        >,
    > {
        use ide_db::RootDatabase;
        self.db.module_code_security_state(file_id)
    }
}

pub fn check_metadata_diagnostic<F>(
    metadata: hir::ModuleMetadata,
    file_text: &str,
    check_fn: F,
) -> Vec<Diagnostic>
where
    F: Fn(&hir::ModuleMetadata, &crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    let config = crate::DiagnosticsConfig::all_enabled();
    check_metadata_diagnostic_with_config(metadata, file_text, config, check_fn)
}

pub fn check_metadata_diagnostic_with_config<F>(
    metadata: hir::ModuleMetadata,
    file_text: &str,
    config: crate::DiagnosticsConfig,
    check_fn: F,
) -> Vec<Diagnostic>
where
    F: Fn(&hir::ModuleMetadata, &crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    use std::rc::Rc;
    use std::sync::Arc;

    let (db, file_id) = create_test_db(file_text);
    let metadata_arc = Arc::new(metadata);
    let config_rc = Rc::new(config);

    let provider_impl =
        MetadataTestProvider { db, metadata: Arc::clone(&metadata_arc), configuration: None };

    let ctx = crate::DiagnosticsContext::new(
        &config_rc,
        file_id,
        &provider_impl as &dyn ide_db::provider::AnalysisProvider,
    );

    check_fn(&metadata_arc, &ctx)
}

pub fn check_diagnostic_with_privileged_modules<F>(
    caller_code: &str,
    privileged_modules: &[(&str, &str)],
    check_fn: F,
) -> Vec<Diagnostic>
where
    F: Fn(&crate::DiagnosticsContext) -> Vec<Diagnostic>,
{
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::RootDatabaseImpl;
    use std::rc::Rc;
    use std::sync::Arc;
    use vfs::{FileId, FileSet, VfsPath};

    let mut configuration = bsl_metadata::Configuration::new("TestConfig");
    for &(name, _) in privileged_modules {
        let module =
            bsl_metadata::CommonModule::builder().name(name).privileged(true).server(true).build();
        configuration.add_common_module(module);
    }

    let mut db = RootDatabaseImpl::new();
    let mut file_set = FileSet::default();

    let caller_file_id = FileId(0);
    file_set.insert(caller_file_id, VfsPath::new("/CommonModules/Caller/Module.bsl"));

    let mut privileged_file_ids = Vec::with_capacity(privileged_modules.len());
    for (idx, (name, _body)) in privileged_modules.iter().enumerate() {
        let fid = FileId((idx + 1) as u32);
        file_set.insert(fid, VfsPath::new(format!("/CommonModules/{}/Module.bsl", name)));
        privileged_file_ids.push(fid);
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller_file_id, SourceRootId(0));
    db.set_file_text(caller_file_id, caller_code);
    for (fid, (_, body)) in privileged_file_ids.iter().zip(privileged_modules.iter()) {
        db.set_file_source_root(*fid, SourceRootId(0));
        db.set_file_text(*fid, body);
    }

    let metadata =
        make_common_module_metadata(bsl_metadata::CommonModule::builder().name("Caller").build());
    let metadata_arc = Arc::new(metadata);
    let config_rc = Rc::new(crate::DiagnosticsConfig::all_enabled());
    let provider_impl = MetadataTestProvider {
        db,
        metadata: Arc::clone(&metadata_arc),
        configuration: Some(Arc::new(configuration)),
    };
    let ctx = crate::DiagnosticsContext::new(
        &config_rc,
        caller_file_id,
        &provider_impl as &dyn ide_db::provider::AnalysisProvider,
    );

    check_fn(&ctx)
}

pub fn check_with_config_xml(
    source: &str,
    config_xml: &str,
    common_modules: &[(&str, &str)],
) -> Vec<Diagnostic> {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::metadata::intern_configuration_path;
    use ide_db::RootDatabaseImpl;
    use vfs::{FileId, FileSet, VfsPath};

    let project = ConfigXmlFixtureProject::new(config_xml, common_modules);
    let registered_modules = project.registered_modules();

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, project.root().to_path_buf())], None);

    let mut file_set = FileSet::default();
    let caller_file_id = FileId(0);
    let caller_path = project.root().join("CommonModules/Caller/Ext/Module.bsl");
    file_set.insert(caller_file_id, VfsPath::new(caller_path.to_string_lossy().into_owned()));

    let mut module_file_ids = Vec::with_capacity(registered_modules.len());
    for (idx, (name, _body)) in registered_modules.iter().enumerate() {
        let fid = FileId((idx + 1) as u32);
        let path = project.root().join(format!("CommonModules/{}/Ext/Module.bsl", name));
        file_set.insert(fid, VfsPath::new(path.to_string_lossy().into_owned()));
        module_file_ids.push(fid);
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller_file_id, SourceRootId(0));
    db.set_file_text(caller_file_id, source);
    for (fid, (_, body)) in module_file_ids.iter().zip(registered_modules.iter()) {
        db.set_file_source_root(*fid, SourceRootId(0));
        db.set_file_text(*fid, body);
    }

    let config_path = project.root().to_string_lossy();
    let config_path_input = intern_configuration_path(
        &db,
        config_path.as_ref(),
        db.config_root_revision_for_path(std::path::Path::new(config_path.as_ref())),
    );
    let provider = ide_db::SalsaProvider::new(&db, Some(config_path_input));
    let config = crate::DiagnosticsConfig::all_enabled();
    let ctx = crate::DiagnosticsContext::new(&config, caller_file_id, &provider);

    crate::diagnostics(&ctx)
}

/// Minimal `Ext/Form.xml` for a managed form. An absent `<FormType>` already
/// means `Managed`, so the empty form is enough to make `ЭтотОбъект` resolve as
/// the form itself — without it a form-module fixture is not a form at all.
fn minimal_managed_form_xml() -> &'static str {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" version="2.10">
    <Properties>
        <Name>ФормаЭлемента</Name>
    </Properties>
</Form>"#
}

/// Diagnostics of a form module calling common modules whose metadata flags
/// matter: each `(name, body, xml)` triple materializes with its OWN
/// CommonModule XML (client/server/`ВызовСервера` flags), which the in-memory
/// fixture VFS cannot provide — `module_metadata` reads them through the
/// configuration loaded from disk.
pub fn check_form_with_common_modules(
    form_source: &str,
    modules: &[(&str, &str, &str)],
) -> Vec<Diagnostic> {
    check_form_module_impl(form_source, modules, minimal_managed_form_xml(), None)
}

/// Diagnostics of a managed-form module whose `Ext/Form.xml` content matters
/// (form attributes, items): the in-memory fixture VFS cannot provide it, so
/// the configuration is materialized on disk exactly like in
/// [`check_form_with_common_modules`].
pub fn check_form_with_form_xml(form_source: &str, form_xml: &str) -> Vec<Diagnostic> {
    check_form_module_impl(form_source, &[], form_xml, None)
}

/// [`check_form_with_form_xml`] for a form whose owner matters as well: the
/// form belongs to the data processor `Товары`, registered in the configuration and
/// described by `owner_xml` (its `DataProcessors/Товары.xml`), so an attribute of type
/// `cfg:DataProcessorObject.Товары` resolves to its attributes and tabular sections.
pub fn check_form_with_owner_xml(
    form_source: &str,
    form_xml: &str,
    owner_xml: &str,
) -> Vec<Diagnostic> {
    check_form_module_impl(form_source, &[], form_xml, Some(owner_xml))
}

fn check_form_module_impl(
    form_source: &str,
    modules: &[(&str, &str, &str)],
    form_xml: &str,
    owner_xml: Option<&str>,
) -> Vec<Diagnostic> {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::metadata::intern_configuration_path;
    use ide_db::RootDatabaseImpl;
    use vfs::{FileId, FileSet, VfsPath};

    struct TempRootGuard(std::path::PathBuf);
    impl Drop for TempRootGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let root = next_config_xml_fixture_root();
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create fixture root");
    let _guard = TempRootGuard(root.clone());
    let (owner_dir, configuration_xml) = match owner_xml {
        Some(owner_xml) => {
            std::fs::create_dir_all(root.join("DataProcessors"))
                .expect("create DataProcessors directory");
            std::fs::write(root.join("DataProcessors/Товары.xml"), owner_xml)
                .expect("write DataProcessors/Товары.xml");
            let children = "</Properties>\n        <ChildObjects>\n            \
                            <DataProcessor>Товары</DataProcessor>\n        </ChildObjects>";
            ("DataProcessors", minimal_configuration_xml().replace("</Properties>", children))
        }
        None => ("Catalogs", minimal_configuration_xml()),
    };
    std::fs::write(root.join("Configuration.xml"), configuration_xml)
        .expect("write Configuration.xml");
    for (name, body, xml) in modules {
        let ext_dir = root.join(format!("CommonModules/{name}/Ext"));
        std::fs::create_dir_all(&ext_dir).expect("create CommonModule Ext directory");
        std::fs::write(ext_dir.join("Module.bsl"), body).expect("write CommonModule body");
        std::fs::write(root.join(format!("CommonModules/{name}.xml")), xml)
            .expect("write CommonModule XML");
    }

    let form_ext_dir = root.join(format!("{owner_dir}/Товары/Forms/ФормаЭлемента/Ext"));
    std::fs::create_dir_all(form_ext_dir.join("Form")).expect("create form Ext/Form directory");
    std::fs::write(form_ext_dir.join("Form.xml"), form_xml).expect("write Ext/Form.xml");

    let mut db = RootDatabaseImpl::new();
    db.set_all_config_paths(vec![(None, root.clone())], None);

    let mut file_set = FileSet::default();
    let caller_file_id = FileId(0);
    let caller_path =
        root.join(format!("{owner_dir}/Товары/Forms/ФормаЭлемента/Ext/Form/Module.bsl"));
    file_set.insert(caller_file_id, VfsPath::new(caller_path.to_string_lossy().into_owned()));
    let mut module_file_ids = Vec::with_capacity(modules.len());
    for (idx, (name, _, _)) in modules.iter().enumerate() {
        let fid = FileId((idx + 1) as u32);
        let path = root.join(format!("CommonModules/{name}/Ext/Module.bsl"));
        file_set.insert(fid, VfsPath::new(path.to_string_lossy().into_owned()));
        module_file_ids.push(fid);
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller_file_id, SourceRootId(0));
    db.set_file_text(caller_file_id, form_source);
    for (fid, (_, body, _)) in module_file_ids.iter().zip(modules.iter()) {
        db.set_file_source_root(*fid, SourceRootId(0));
        db.set_file_text(*fid, body);
    }

    let config_path = root.to_string_lossy();
    let config_path_input = intern_configuration_path(
        &db,
        config_path.as_ref(),
        db.config_root_revision_for_path(std::path::Path::new(config_path.as_ref())),
    );
    let provider = ide_db::SalsaProvider::new(&db, Some(config_path_input));
    let config = crate::DiagnosticsConfig::all_enabled();
    let ctx = crate::DiagnosticsContext::new(&config, caller_file_id, &provider);

    crate::diagnostics(&ctx)
}

pub fn check_snapshot_with_config_xml(
    source: &str,
    config_xml: &str,
    common_modules: &[(&str, &str)],
    expected: expect_test::Expect,
) {
    let diagnostics = check_with_config_xml(source, config_xml, common_modules);
    expected.assert_eq(&format_diags(source, &diagnostics));
}

pub fn check_with_cfe(source: &str, fixture: test_fixture::CfeFixture) -> Vec<Diagnostic> {
    check_with_cfe_unreadable(source, fixture, &[])
}

pub fn check_with_cfe_config(
    source: &str,
    fixture: test_fixture::CfeFixture,
    config: crate::DiagnosticsConfig,
) -> Vec<Diagnostic> {
    check_cfe_at_with_unreadable_config(
        "CommonModules/Caller/Ext/Module.bsl",
        source,
        fixture,
        &[],
        config,
        crate::diagnostics,
    )
}

/// [`check_with_cfe`], with the named module bodies registered as existing but
/// unreadable. Names are paths relative to the fixture root and are matched WHOLE, so
/// `"CommonModules/Сервер/Ext/Module.bsl"` names the base body and
/// `"Extensions/Расш/CommonModules/Сервер/Ext/Module.bsl"` its extension sibling —
/// a substring match would silently take both.
pub fn check_with_cfe_unreadable(
    source: &str,
    fixture: test_fixture::CfeFixture,
    unreadable: &[&str],
) -> Vec<Diagnostic> {
    check_cfe_at_with_unreadable(
        "CommonModules/Caller/Ext/Module.bsl",
        source,
        fixture,
        unreadable,
        crate::diagnostics,
    )
}

/// [`check_with_cfe_unreadable`] on a chosen profile, for a rule whose default
/// profile differs from `all_enabled` — an incomplete-surface stand has to run on
/// the very profile the rule ships with, or it proves nothing about it.
pub fn check_with_cfe_unreadable_config(
    source: &str,
    fixture: test_fixture::CfeFixture,
    unreadable: &[&str],
    config: crate::DiagnosticsConfig,
) -> Vec<Diagnostic> {
    check_cfe_at_with_unreadable_config(
        "CommonModules/Caller/Ext/Module.bsl",
        source,
        fixture,
        unreadable,
        config,
        crate::diagnostics,
    )
}

/// [`check_with_cfe_unreadable`], with the analyzed file placed at `caller_relative`
/// inside the fixture's first extension and its diagnostics produced by `run` rather
/// than the whole registry. A diagnostic keyed to a module TYPE (the session module,
/// say) is unreachable unless its file sits at the conventional path.
pub fn check_cfe_at_with_unreadable(
    caller_relative: &str,
    source: &str,
    fixture: test_fixture::CfeFixture,
    unreadable: &[&str],
    run: impl FnOnce(&crate::DiagnosticsContext<'_>) -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    check_cfe_at_with_unreadable_config_and_setup(
        caller_relative,
        source,
        fixture,
        unreadable,
        crate::DiagnosticsConfig::all_enabled(),
        |_| {},
        run,
    )
}

fn check_cfe_at_with_unreadable_config(
    caller_relative: &str,
    source: &str,
    fixture: test_fixture::CfeFixture,
    unreadable: &[&str],
    config: crate::DiagnosticsConfig,
    run: impl FnOnce(&crate::DiagnosticsContext<'_>) -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    check_cfe_at_with_unreadable_config_and_setup(
        caller_relative,
        source,
        fixture,
        unreadable,
        config,
        |_| {},
        run,
    )
}

pub(crate) fn check_cfe_at_with_unreadable_config_and_setup(
    caller_relative: &str,
    source: &str,
    fixture: test_fixture::CfeFixture,
    unreadable: &[&str],
    config: crate::DiagnosticsConfig,
    setup: impl FnOnce(&test_fixture::CfeFixture),
    run: impl FnOnce(&crate::DiagnosticsContext<'_>) -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    check_cfe_at_with_db(caller_relative, source, fixture, unreadable, config, setup, |_, ctx| {
        run(ctx)
    })
}

/// [`check_cfe_at_with_unreadable_config_and_setup`] whose `run` also gets the database,
/// for a stand that has to go through [`crate::file_diagnostics`] — the only entry that
/// adds the extension-merge passes.
pub(crate) fn check_cfe_at_with_db(
    caller_relative: &str,
    source: &str,
    fixture: test_fixture::CfeFixture,
    unreadable: &[&str],
    config: crate::DiagnosticsConfig,
    setup: impl FnOnce(&test_fixture::CfeFixture),
    run: impl FnOnce(&ide_db::RootDatabaseImpl, &crate::DiagnosticsContext<'_>) -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    check_cfe_at_with_db_setup(
        caller_relative,
        source,
        fixture,
        unreadable,
        config,
        setup,
        |_| {},
        run,
    )
}

/// [`check_cfe_at_with_db`], with `db_setup` run on the fresh database before any
/// file is loaded — for project-level inputs such as `min_platform_version`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_cfe_at_with_db_setup(
    caller_relative: &str,
    source: &str,
    fixture: test_fixture::CfeFixture,
    unreadable: &[&str],
    config: crate::DiagnosticsConfig,
    setup: impl FnOnce(&test_fixture::CfeFixture),
    db_setup: impl FnOnce(&mut ide_db::RootDatabaseImpl),
    run: impl FnOnce(&ide_db::RootDatabaseImpl, &crate::DiagnosticsContext<'_>) -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    use ide_db::base_db::{SourceDatabase, SourceRoot, SourceRootId};
    use ide_db::metadata::intern_configuration_path;
    use ide_db::RootDatabaseImpl;
    use vfs::{FileId, FileSet, VfsPath};

    materialize_cfe_loader_compat(&fixture);
    setup(&fixture);

    let mut db = RootDatabaseImpl::new();
    db_setup(&mut db);
    db.set_all_config_paths(fixture.config_paths(), Some(bsl_metadata::ExtensionScope::InfoBase));

    let mut file_set = FileSet::default();
    let caller_file_id = FileId(0);
    // Common modules are extension-private — a base-config file cannot see an
    // extension's common modules (the same scoping as metadata objects). So a fixture
    // that exercises an extension's module must place the analyzed caller inside that
    // extension, mirroring real usage. Fall back to the base root when there are none.
    let caller_root = fixture
        .extensions()
        .first()
        .map(|ext| ext.root().to_path_buf())
        .unwrap_or_else(|| fixture.root().to_path_buf());
    let caller_path = caller_root.join(caller_relative);
    file_set.insert(caller_file_id, VfsPath::new(caller_path.to_string_lossy().into_owned()));

    let mut module_files = Vec::new();
    let relative_to_root = |path: &std::path::Path| {
        path.strip_prefix(fixture.root())
            .expect("every fixture body lives under the fixture root")
            .to_string_lossy()
            .replace('\\', "/")
    };
    for module in fixture.base_modules() {
        let fid = FileId((module_files.len() + 1) as u32);
        let path = fixture.root().join(format!("CommonModules/{}/Ext/Module.bsl", module.name()));
        let spelling = relative_to_root(&path);
        file_set.insert(fid, VfsPath::new(path.to_string_lossy().into_owned()));
        module_files.push((fid, module.source().to_string(), spelling));
    }
    for extension in fixture.extensions() {
        for module in extension.modules() {
            let fid = FileId((module_files.len() + 1) as u32);
            let path =
                extension.root().join(format!("CommonModules/{}/Ext/Module.bsl", module.name()));
            let spelling = relative_to_root(&path);
            file_set.insert(fid, VfsPath::new(path.to_string_lossy().into_owned()));
            module_files.push((fid, module.source().to_string(), spelling));
        }
    }
    for (_, root) in fixture.config_paths() {
        for kind in hir::ApplicationModuleKind::ALL {
            let path = root.join(kind.relative_path());
            if !path.is_file() {
                continue;
            }
            let fid = FileId((module_files.len() + 1) as u32);
            let spelling = relative_to_root(&path);
            let body = std::fs::read_to_string(&path).expect("read application-module fixture");
            file_set.insert(fid, VfsPath::new(path.to_string_lossy().into_owned()));
            module_files.push((fid, body, spelling));
        }
    }

    let source_root = SourceRoot::new_local(file_set);
    db.set_source_root(SourceRootId(0), source_root);
    db.set_file_source_root(caller_file_id, SourceRootId(0));
    db.set_file_text(caller_file_id, source);
    let mut marked = 0usize;
    for (fid, body, spelling) in module_files {
        db.set_file_source_root(fid, SourceRootId(0));
        if unreadable.iter().any(|u| spelling == *u) {
            db.set_file_unreadable(fid);
            marked += 1;
        } else {
            db.set_file_text(fid, &body);
        }
    }
    assert_eq!(
        marked,
        unreadable.len(),
        "every named body must exist in the fixture, or the stand proves nothing"
    );

    let config_path = fixture.root().to_string_lossy();
    let config_path_input = intern_configuration_path(
        &db,
        config_path.as_ref(),
        db.config_root_revision_for_path(std::path::Path::new(config_path.as_ref())),
    );
    let provider = ide_db::SalsaProvider::new(&db, Some(config_path_input));
    let ctx = crate::DiagnosticsContext::new(&config, caller_file_id, &provider);

    run(&db, &ctx)
}

pub fn check_snapshot_with_cfe(
    source: &str,
    fixture: test_fixture::CfeFixture,
    expected: expect_test::Expect,
) {
    let diagnostics = check_with_cfe(source, fixture);
    expected.assert_eq(&format_diags(source, &diagnostics));
}

struct ConfigXmlFixtureProject {
    root: std::path::PathBuf,
    registered_modules: Vec<(String, String)>,
}

impl ConfigXmlFixtureProject {
    fn new(config_xml: &str, common_modules: &[(&str, &str)]) -> Self {
        let root = next_config_xml_fixture_root();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create config XML fixture root");
        let config_body = if config_xml.trim().is_empty() {
            minimal_configuration_xml()
        } else {
            config_xml.to_string()
        };
        std::fs::write(root.join("Configuration.xml"), config_body)
            .expect("write Configuration.xml");

        let declared_modules = declared_common_module_names(config_xml);
        let registered_modules = common_modules
            .iter()
            .filter(|(name, _)| {
                declared_modules.is_empty()
                    || declared_modules
                        .iter()
                        .any(|declared| declared.fold_lower() == name.fold_lower())
            })
            .map(|(name, body)| ((*name).to_string(), (*body).to_string()))
            .collect::<Vec<_>>();

        for (idx, (name, body)) in registered_modules.iter().enumerate() {
            let module_dir = root.join(format!("CommonModules/{name}"));
            let ext_dir = module_dir.join("Ext");
            std::fs::create_dir_all(&ext_dir).expect("create CommonModule Ext directory");
            std::fs::write(ext_dir.join("Module.bsl"), body).expect("write CommonModule body");
            std::fs::write(
                root.join(format!("CommonModules/{name}.xml")),
                common_module_xml(name, idx),
            )
            .expect("write CommonModule XML");
        }

        Self { root, registered_modules }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }

    fn registered_modules(&self) -> Vec<(&str, &str)> {
        self.registered_modules.iter().map(|(name, body)| (name.as_str(), body.as_str())).collect()
    }
}

impl Drop for ConfigXmlFixtureProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn next_config_xml_fixture_root() -> std::path::PathBuf {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("bsl_analyzer_config_xml_{}_{}", std::process::id(), id))
}

fn minimal_configuration_xml() -> String {
    r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>TestConfiguration</Name>
        </Properties>
    </Configuration>
</MetaDataObject>"#
        .to_string()
}

fn declared_common_module_names(config_xml: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = config_xml;
    const OPEN: &str = "<CommonModule";
    const CLOSE: &str = "</CommonModule>";

    while let Some(start) = rest.find(OPEN) {
        rest = &rest[start + OPEN.len()..];
        let Some(next) = rest.chars().next() else { break };
        if !matches!(next, '>' | '/' | ' ' | '\t' | '\n' | '\r') {
            rest = &rest[next.len_utf8()..];
            continue;
        }

        let Some(open_end) = rest.find('>') else { break };
        let tag_tail = &rest[..open_end];
        let after_open = &rest[open_end + 1..];
        if tag_tail.trim_end().ends_with('/') {
            rest = after_open;
            continue;
        }

        let Some(close_start) = after_open.find(CLOSE) else { break };
        let name = after_open[..close_start].trim();
        if !name.is_empty() && !name.contains('<') {
            names.push(name.to_string());
        }
        rest = &after_open[close_start + CLOSE.len()..];
    }

    names
}

fn common_module_xml(name: &str, idx: usize) -> String {
    common_module_xml_with_privileged(name, idx, false)
}

fn common_module_xml_with_global(name: &str, idx: usize, global: bool) -> String {
    let xml = common_module_xml_with_privileged(name, idx, false);
    if global {
        xml.replace("<Global>false</Global>", "<Global>true</Global>")
    } else {
        xml
    }
}

fn common_module_xml_with_privileged(name: &str, idx: usize, privileged: bool) -> String {
    let uuid = format!("00000000-0000-0000-0000-{:012}", idx + 1);
    format!(
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <CommonModule uuid="{uuid}">
        <Properties>
            <Name>{}</Name>
            <Global>false</Global>
            <ClientManagedApplication>false</ClientManagedApplication>
            <ClientOrdinaryApplication>false</ClientOrdinaryApplication>
            <Server>true</Server>
            <ExternalConnection>false</ExternalConnection>
            <ServerCall>false</ServerCall>
            <Privileged>{}</Privileged>
            <ReturnValuesReuse>DontUse</ReturnValuesReuse>
        </Properties>
    </CommonModule>
</MetaDataObject>"#,
        escape_xml_text(name),
        privileged
    )
}

fn materialize_cfe_loader_compat(fixture: &test_fixture::CfeFixture) {
    let mut idx = 0usize;
    for module in fixture.base_modules() {
        let common_modules_dir = fixture.root().join("CommonModules");
        let ext_dir = common_modules_dir.join(module.name()).join("Ext");
        std::fs::create_dir_all(&ext_dir).expect("create base CommonModule Ext directory");
        std::fs::write(ext_dir.join("Module.bsl"), module.source())
            .expect("write base CommonModule Ext body");
        std::fs::write(
            common_modules_dir.join(format!("{}.xml", module.name())),
            common_module_xml_with_global(module.name(), idx, module.is_global()),
        )
        .expect("write base CommonModule XML");
        idx += 1;
    }
    for extension in fixture.extensions() {
        for module in extension.modules() {
            let common_modules_dir = extension.root().join("CommonModules");
            let ext_dir = common_modules_dir.join(module.name()).join("Ext");
            std::fs::create_dir_all(&ext_dir).expect("create CFE CommonModule Ext directory");
            std::fs::write(ext_dir.join("Module.bsl"), module.source())
                .expect("write CFE CommonModule Ext body");

            let privileged = extension_config_marks_module_privileged(extension.config_xml());
            std::fs::write(
                common_modules_dir.join(format!("{}.xml", module.name())),
                common_module_xml_with_privileged(module.name(), idx, privileged),
            )
            .expect("write CFE CommonModule XML");
            idx += 1;
        }
    }
}

fn extension_config_marks_module_privileged(config_xml: &str) -> bool {
    config_xml.contains("<Privileged>true</Privileged>")
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn make_common_module_metadata(module: bsl_metadata::CommonModule) -> hir::ModuleMetadata {
    hir::ModuleMetadata {
        module_type: bsl_metadata::ModuleType::CommonModule,
        execution_context: None,
        common_module: Some(std::sync::Arc::new(module)),
        mdo: None,
        register: None,
        http_service: None,
        web_service: None,
        integration_service: None,
        form: None,
    }
}

pub fn make_common_module_metadata_with_ctx(
    module: bsl_metadata::CommonModule,
    ctx: hir::ExecutionContext,
) -> hir::ModuleMetadata {
    hir::ModuleMetadata {
        module_type: bsl_metadata::ModuleType::CommonModule,
        execution_context: Some(ctx),
        common_module: Some(std::sync::Arc::new(module)),
        mdo: None,
        register: None,
        http_service: None,
        web_service: None,
        integration_service: None,
        form: None,
    }
}

pub fn make_non_common_module_metadata(
    module_type: bsl_metadata::ModuleType,
) -> hir::ModuleMetadata {
    hir::ModuleMetadata {
        module_type,
        execution_context: None,
        common_module: None,
        mdo: None,
        register: None,
        http_service: None,
        web_service: None,
        integration_service: None,
        form: None,
    }
}

#[cfg(test)]
mod format_diags_tests {
    use super::*;

    fn diag(
        code: DiagnosticCode,
        message: &str,
        severity: Severity,
        start: u32,
        end: u32,
    ) -> Diagnostic {
        Diagnostic {
            code,
            message: message.to_string(),
            severity,
            range: TextRange::new(start.into(), end.into()),
            tags: vec![],
            fixes: vec![],
        }
    }

    #[test]
    fn format_diags_empty_list() {
        assert_eq!(format_diags("", &[]), "");
    }

    #[test]
    fn format_diags_sort_stable_across_input_permutations() {
        let source = "abc\ndef\nghi";
        let first = diag(DiagnosticCode::LineLength, "third", Severity::Warning, 8, 9);
        let second = diag(DiagnosticCode::EmptyCodeBlock, "first", Severity::Major, 0, 1);
        let third = diag(DiagnosticCode::BadWords, "second", Severity::Critical, 4, 6);
        let ordered = vec![first.clone(), second.clone(), third.clone()];
        let shuffled = vec![third, first, second];

        assert_eq!(format_diags(source, &ordered), format_diags(source, &shuffled));
    }

    #[test]
    fn format_diags_ties_break_on_end_position() {
        let source = "abcdef";
        let longer = diag(DiagnosticCode::LineLength, "longer", Severity::Warning, 0, 4);
        let shorter = diag(DiagnosticCode::LineLength, "shorter", Severity::Warning, 0, 2);

        assert_eq!(
            format_diags(source, &[longer, shorter]),
            "LineLength @ 1:1..1:3\n  message: shorter\n  severity: Warning\nLineLength @ 1:1..1:5\n  message: longer\n  severity: Warning"
        );
    }

    #[test]
    fn check_diagnostics_snapshot_for_filters_by_code() {
        let source = "abcdef";
        let matching = diag(DiagnosticCode::LineLength, "kept", Severity::Warning, 0, 2);
        let non_matching = diag(DiagnosticCode::BadWords, "dropped", Severity::Major, 3, 4);

        expect_test::expect![[r#"
            LineLength @ 1:1..1:3
              message: kept
              severity: Warning"#]]
        .assert_eq(&format_diags_for(
            source,
            &[matching, non_matching],
            DiagnosticCode::LineLength,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_range_to_line_col_simple() {
        let text = "Hello World";
        let range = TextRange::new(0.into(), 5.into());
        let (start_line, start_col, end_line, end_col) = range_to_line_col(text, range);
        assert_eq!(start_line, 0);
        assert_eq!(start_col, 0);
        assert_eq!(end_line, 0);
        assert_eq!(end_col, 5);
    }

    #[test]
    fn test_range_to_line_col_multiline() {
        let text = "Line 1\nLine 2\nLine 3";
        let range = TextRange::new(7.into(), 13.into());
        let (start_line, start_col, end_line, end_col) = range_to_line_col(text, range);
        assert_eq!(start_line, 1);
        assert_eq!(start_col, 0);
        assert_eq!(end_line, 1);
        assert_eq!(end_col, 6);
    }

    #[test]
    fn test_range_to_line_col_utf8() {
        let text = "Привет мир";
        let range = TextRange::new(6.into(), 17.into());
        let (start_line, start_col, end_line, end_col) = range_to_line_col(text, range);
        assert_eq!(start_line, 0);
        assert_eq!(start_col, 3);
        assert_eq!(end_line, 0);
        assert_eq!(end_col, 9);
    }

    #[test]
    fn check_snapshot_with_config_xml_resolves_declared_common_module() {
        let config_xml = r#"
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>TestConfiguration</Name>
        </Properties>
        <ChildObjects>
            <CommonModule>ОбщийМодульТест</CommonModule>
        </ChildObjects>
    </Configuration>
</MetaDataObject>
"#;
        let common_module = r#"
Процедура Метод() Экспорт
КонецПроцедуры
"#;
        let source = r#"
#Область ПрограммныйИнтерфейс
// Тестирует вызов общего модуля.
Процедура Тест() Экспорт
    ОбщийМодульТест.Метод();
КонецПроцедуры
#КонецОбласти
"#;

        check_snapshot_with_config_xml(
            source,
            config_xml,
            &[("ОбщийМодульТест", common_module)],
            expect_test::expect![""],
        );
    }
}
