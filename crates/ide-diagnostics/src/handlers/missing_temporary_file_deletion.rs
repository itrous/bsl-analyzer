use hir::cfg::ControlFlowGraph;
use hir::dataflow::temp_resource::{analyze_open_resources, ResourceEvent, ResourceProvider};
use hir::LocalRange;
use hir::{Body, BodySourceMap, Expr, ExprId, ExprIdx, IdConversion, Stmt, StmtId};
use regex::Regex;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;
use stdx::case::CaseExt;

use crate::define_metadata;
use crate::metadata::*;
use crate::utils::regex_cache::cached_regex;
use crate::{BodyContext, Diagnostic, DiagnosticCode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Badpractice, MetadataTag::Standard],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "warning",
};

const DEFAULT_SEARCH_DELETE_FILE_METHOD: &str =
    "УдалитьФайлы|DeleteFiles|НачатьУдалениеФайлов|BeginDeletingFiles|ПереместитьФайл|MoveFile";

const INLINE_MESSAGE: &str = "Нужно добавить удаление временного файла после использования";

#[derive(Debug, Clone)]
struct Config {
    deletion_methods: Arc<Regex>,
}

impl Config {
    fn from_context(ctx: &BodyContext) -> Self {
        let pattern = ctx
            .config
            .get_string(DiagnosticCode::MissingTemporaryFileDeletion, "searchDeleteFileMethod")
            .unwrap_or(DEFAULT_SEARCH_DELETE_FILE_METHOD);
        let regex_pattern = format!("(?i)^({})$", pattern);
        let deletion_methods = cached_regex(&regex_pattern).unwrap_or_else(|| {
            tracing::warn!(
                pattern = %pattern,
                "Invalid searchDeleteFileMethod regex, using default"
            );
            cached_regex(&format!("(?i)^({})$", DEFAULT_SEARCH_DELETE_FILE_METHOD))
                .expect("Default regex must be valid")
        });
        tracing::debug!(pattern = %pattern, "MissingTemporaryFileDeletion config loaded");
        Self { deletion_methods }
    }
}

struct AssignedGet {
    original_name: String,
    lower_name: String,
}

pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::MissingTemporaryFileDeletion;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }

    let config = Config::from_context(ctx);

    let cfg = ctx.cfg();
    acc.extend(check_body_resources(ctx.body(), ctx.source_map(), &cfg, &config, code, ctx));
}

fn check_body_resources(
    body: &Body,
    source_map: &BodySourceMap,
    cfg: &ControlFlowGraph,
    config: &Config,
    code: DiagnosticCode,
    ctx: &BodyContext,
) -> Vec<Diagnostic<LocalRange>> {
    let (tracked, suppressed) = collect_assigned_gets(body);

    let mut diagnostics = Vec::new();

    for (expr_id, expr) in body.exprs_iter() {
        let Expr::Call { callee, .. } = expr else { continue };
        let Expr::Path(name) = body.expr_idx(*callee) else { continue };
        if !is_get_temp_filename(name.as_str()) {
            continue;
        }
        let idx: ExprIdx = expr_id.to_idx();
        if tracked.contains_key(&idx) || suppressed.contains(&idx) {
            continue;
        }
        if let Some(range) = source_map.expr_range(expr_id) {
            diagnostics.push(Diagnostic {
                code,
                message: INLINE_MESSAGE.to_string(),
                severity: ctx.severity(code),
                range,
                tags: ctx.tags(code),
                fixes: vec![],
            });
        }
    }

    if tracked.is_empty() {
        return diagnostics;
    }

    let known_vars: FxHashSet<String> = tracked.values().map(|ag| ag.lower_name.clone()).collect();
    let returned = collect_returned_names(body, &known_vars);
    let provider = TempFileProvider {
        deletion_methods: &config.deletion_methods,
        tracked: &tracked,
        known_vars: &known_vars,
        returned: &returned,
    };
    let Some(result) = analyze_open_resources::<_, String>(body, cfg, provider) else {
        return diagnostics;
    };

    for sites in result.open_at_exit().values() {
        for &site in sites {
            let Some(ag) = tracked.get(&site) else { continue };
            let Some(range) = source_map.expr_range(ExprId::from_idx(site)) else { continue };
            diagnostics.push(Diagnostic {
                code,
                message: format!(
                    "Нужно добавить удаление временного файла '{}' после использования",
                    ag.original_name
                ),
                severity: ctx.severity(code),
                range,
                tags: ctx.tags(code),
                fixes: vec![],
            });
        }
    }

    diagnostics
}

fn collect_assigned_gets(body: &Body) -> (FxHashMap<ExprIdx, AssignedGet>, FxHashSet<ExprIdx>) {
    let mut tracked: FxHashMap<ExprIdx, AssignedGet> = FxHashMap::default();
    let mut suppressed: FxHashSet<ExprIdx> = FxHashSet::default();

    for (_, stmt) in body.stmts_iter() {
        let Stmt::Assign { target, value } = stmt else { continue };
        if !is_get_temp_filename_call(body, *value) {
            continue;
        }
        match body.expr_idx(*target) {
            Expr::Path(name) => {
                tracked.insert(
                    *value,
                    AssignedGet {
                        original_name: name.as_str().to_string(),
                        lower_name: name.as_str().fold_lower(),
                    },
                );
            }
            _ => {
                suppressed.insert(*value);
            }
        }
    }

    (tracked, suppressed)
}

/// `Возврат ИмяФайла` hands the name to the caller, which becomes responsible
/// for deleting the file; only the bare variable counts, not an expression
/// that merely mentions it.
fn collect_returned_names(
    body: &Body,
    known_vars: &FxHashSet<String>,
) -> FxHashMap<ExprIdx, String> {
    let mut returned = FxHashMap::default();
    for (_, stmt) in body.stmts_iter() {
        let Stmt::Return { value: Some(value) } = stmt else { continue };
        let Expr::Path(name) = body.expr_idx(*value) else { continue };
        let lower = name.as_str().fold_lower();
        if known_vars.contains(&lower) {
            returned.insert(*value, lower);
        }
    }
    returned
}

struct TempFileProvider<'a> {
    deletion_methods: &'a Regex,
    tracked: &'a FxHashMap<ExprIdx, AssignedGet>,
    known_vars: &'a FxHashSet<String>,
    returned: &'a FxHashMap<ExprIdx, String>,
}

impl TempFileProvider<'_> {
    fn deleted_vars(&self, body: &Body, expr_idx: ExprIdx) -> FxHashSet<String> {
        let mut closed: FxHashSet<String> = FxHashSet::default();
        let (callee_path, args) = match body.expr_idx(expr_idx) {
            Expr::Call { callee, args } => (extract_call_path(body, *callee), args),
            Expr::MethodCall { method, args, .. } => (method.as_str().to_string(), args),
            _ => return closed,
        };
        if !self.deletion_methods.is_match(&callee_path) {
            return closed;
        }
        for &arg in args.iter() {
            collect_referenced_vars(body, arg, self.known_vars, &mut closed);
        }
        closed
    }
}

impl<'a> ResourceProvider<String> for TempFileProvider<'a> {
    fn classify(&self, _body: &Body, expr_idx: ExprIdx) -> Option<ResourceEvent<String>> {
        self.tracked.get(&expr_idx).map(|ag| ResourceEvent::Open(ag.lower_name.clone()))
    }

    fn classify_many(&self, body: &Body, expr_idx: ExprIdx) -> Vec<ResourceEvent<String>> {
        if let Some(open) = self.classify(body, expr_idx) {
            return vec![open];
        }
        if let Some(var) = self.returned.get(&expr_idx) {
            return vec![ResourceEvent::Close(var.clone())];
        }
        self.deleted_vars(body, expr_idx).into_iter().map(ResourceEvent::Close).collect()
    }

    /// `Попытка УдалитьФайлы(Имя); Исключение ... КонецПопытки;`: when the
    /// deletion is the first protected statement, every exception the handler
    /// can receive is raised by the deletion attempt itself or after it, so
    /// the handler path does not leave the file undeleted by the code.
    fn closed_at_try_entry(&self, body: &Body, try_stmt: StmtId) -> Vec<String> {
        let Stmt::Try { body: protected, .. } = body.stmt(try_stmt) else { return Vec::new() };
        let Some(&first) = protected.first() else { return Vec::new() };
        let Stmt::Expr(call) = body.stmt_idx(first) else { return Vec::new() };
        self.deleted_vars(body, *call).into_iter().collect()
    }
}

fn extract_call_path(body: &Body, callee: ExprIdx) -> String {
    match body.expr_idx(callee) {
        Expr::Path(name) => name.as_str().to_string(),
        Expr::Field { base, field } => {
            let base_path = extract_call_path(body, *base);
            if base_path.is_empty() {
                field.as_str().to_string()
            } else {
                format!("{}.{}", base_path, field.as_str())
            }
        }
        _ => String::new(),
    }
}

fn collect_referenced_vars(
    body: &Body,
    expr_idx: ExprIdx,
    known_vars: &FxHashSet<String>,
    out: &mut FxHashSet<String>,
) {
    match body.expr_idx(expr_idx) {
        Expr::Path(name) => {
            let lower = name.as_str().fold_lower();
            if known_vars.contains(&lower) {
                out.insert(lower);
            }
        }
        Expr::Call { callee, args } => {
            collect_referenced_vars(body, *callee, known_vars, out);
            for &a in args.iter() {
                collect_referenced_vars(body, a, known_vars, out);
            }
        }
        Expr::MethodCall { receiver, args, .. } => {
            collect_referenced_vars(body, *receiver, known_vars, out);
            for &a in args.iter() {
                collect_referenced_vars(body, a, known_vars, out);
            }
        }
        Expr::Field { base, .. } => collect_referenced_vars(body, *base, known_vars, out),
        Expr::Index { base, index } => {
            collect_referenced_vars(body, *base, known_vars, out);
            collect_referenced_vars(body, *index, known_vars, out);
        }
        Expr::BinaryOp { lhs, rhs, .. } => {
            collect_referenced_vars(body, *lhs, known_vars, out);
            collect_referenced_vars(body, *rhs, known_vars, out);
        }
        Expr::UnaryOp { expr, .. } => collect_referenced_vars(body, *expr, known_vars, out),
        Expr::New { args, .. } => {
            for &a in args.iter() {
                collect_referenced_vars(body, a, known_vars, out);
            }
        }
        Expr::Ternary { .. }
        | Expr::Array(_)
        | Expr::Await { .. }
        | Expr::Literal(_)
        | Expr::Missing => {}
    }
}

fn is_get_temp_filename_call(body: &Body, expr_idx: ExprIdx) -> bool {
    let Expr::Call { callee, .. } = body.expr_idx(expr_idx) else { return false };
    let Expr::Path(name) = body.expr_idx(*callee) else { return false };
    is_get_temp_filename(name.as_str())
}

fn is_get_temp_filename(name: &str) -> bool {
    let lower = name.fold_lower();
    lower == "получитьимявременногофайла" || lower == "gettempfilename"
}

#[cfg(test)]
mod tests {
    use super::check_body;
    use crate::test_utils::*;
    use crate::{DiagnosticCode, DiagnosticsConfig};
    use expect_test::expect;

    const FIXTURE: &str = "\nПроцедура ПроверкаДиагностики()\n\n    Путь = \"12345.xml\";\n\n    Данные = Base64Значение(\"12345\");\n    ИмяПромежуточногоФайла = ПолучитьИмяВременногоФайла(\"xml\"); // ошибка\n    Данные.Записать(ИмяПромежуточногоФайла);\n\n    ИмяПромежуточногоФайла2 = ПолучитьИмяВременногоФайла(\"xml\"); \n    Данные.Записать(ИмяПромежуточногоФайла2);\n    УдалитьФайлы(ИмяПромежуточногоФайла2);\n\n    ИмяПромежуточногоФайла3 = ПолучитьИмяВременногоФайла(\"xml\"); \n    Данные.Записать(ИмяПромежуточногоФайла3);\n    ПереместитьФайл(ИмяПромежуточногоФайла3, Путь);\n\n    // ошибка, если нет поиска\n    // РаботаСФайламиСлужебныйКлиент.УдалитьФайл\n    ИмяПромежуточногоФайла4 = ПолучитьИмяВременногоФайла(\"xml\"); // ошибка, если нет исключения\n    Данные.Записать(ИмяПромежуточногоФайла4);\n    РаботаСФайламиСлужебныйКлиент.УдалитьФайл(Неопределено, ИмяПромежуточногоФайла4);\n\n    // ошибка, если нет поиска\n    // РандомнаяПроцедураУдаленияФайла\n    ИмяПромежуточногоФайла5 = ПолучитьИмяВременногоФайла(\"xml\");\n    Данные.Записать(ИмяПромежуточногоФайла5);\n    РандомнаяПроцедураУдаленияФайла(ИмяПромежуточногоФайла5);\n\n    // ошибка, если нет \"НачатьУдалениеФайлов\"\n    ИмяПромежуточногоФайла6 = ПолучитьИмяВременногоФайла(\"xml\");\n    НачатьУдалениеФайлов(, ИмяПромежуточногоФайла6);\n\n    // ошибка, если нет \"BeginDeletingFiles\"\n    TempFile7 = GetTempFileName(\"xml\");\n    BeginDeletingFiles(, TempFile7);\n\nКонецПроцедуры\n\nПроцедура РандомнаяПроцедураУдаленияФайла(ИмяФайла)\n    УдалитьФайлы(ИмяФайла);\nКонецПроцедуры\n\nПроцедура ПроверкаДиагностикиСОбщимМодулем()\n\n    ИмяПромежуточногоФайла = ПолучитьИмяВременногоФайла(\"xml\"); // <-- Ошибки нет, ниже удаление\n    Данные.Записать(ИмяПромежуточногоФайла);\n\n\n    ИмяПромежуточногоФайла2 = ПолучитьИмяВременногоФайла(\"txt\"); // <-- Ошибка, удаления файла нет\n    Данные.Записать(ИмяПромежуточногоФайла2);\n\n    ОбщийМодуль.УдалитьВсеФайлы2(ИмяПромежуточногоФайла);\n    Обработки.ДляУдаления.УдалитьВсеФайлы(ИмяПромежуточногоФайла);\n    Статус = Справочники.ОбщийМодуль.УдалитьВсеФайлы(ИмяПромежуточногоФайла);\n\nКонецПроцедуры\n\nПроцедура ПроверкаДиагностикиСОбщимМодулем_Модуль()\n\n    ИмяПромежуточногоФайла = ПолучитьИмяВременногоФайла(); // <-- Ошибки нет, ниже удаление\n    ДвоичныеДанные = Модуль(\"РаботаСФайлами\").ДвоичныеДанныеФайла(ИмяПромежуточногоФайла);\n    УдалитьФайлы(ИмяПромежуточногоФайла);\n\n    ИмяПромежуточногоФайла3 = ПолучитьИмяВременногоФайла(); // <-- Ошибка, удаления файла нет\n    ДвоичныеДанные = Модуль(\"РаботаСФайлами\").ДвоичныеДанныеФайла(ИмяПромежуточногоФайла3);\n\nКонецПроцедуры\n\nФункция Тест()\n    Если Условие Тогда\n        ИмяФайлаНаДиске = ПолучитьИмяВременногоФайла(); // ошибка, удаления файла нет\n        ПолучитьИзВременногоХранилища(ИмяФайла).Записать(ИмяФайлаНаДиске);\n     Иначе\n        ИмяФайлаНаДиске = ИмяФайла;\n    КонецЕсли;\n\n    Возврат ТекстИзФайла;\nКонецФункции";

    #[test]
    fn test_default_config() {
        let code = FIXTURE;
        let config = DiagnosticsConfig::default();
        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);

        expect![[r#"
            MissingTemporaryFileDeletion @ 7:30..7:63
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 20:31..20:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла4' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 26:31..26:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла5' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 46:30..46:63
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 50:31..50:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла2' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 65:31..65:59
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла3' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 72:27..72:55
              message: Нужно добавить удаление временного файла 'ИмяФайлаНаДиске' после использования
              severity: Warning"#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_extended_config() {
        let code = FIXTURE;

        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MissingTemporaryFileDeletion,
            serde_json::json!({
                "searchDeleteFileMethod": "УдалитьФайлы|DeleteFiles|НачатьУдалениеФайлов|BeginDeletingFiles|ПереместитьФайл|MoveFile|РаботаСФайламиСлужебныйКлиент.УдалитьФайл|Справочники.ОбщийМодуль.УдалитьВсеФайлы"
            }),
        );

        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);

        expect![[r#"
            MissingTemporaryFileDeletion @ 7:30..7:63
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 26:31..26:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла5' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 50:31..50:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла2' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 65:31..65:59
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла3' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 72:27..72:55
              message: Нужно добавить удаление временного файла 'ИмяФайлаНаДиске' после использования
              severity: Warning"#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_restrictive_config() {
        let code = FIXTURE;

        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MissingTemporaryFileDeletion,
            serde_json::json!({
                "searchDeleteFileMethod": "УдалитьФайл|DeleteFile|НачатьУдалениеФайловВсех|ОбщийМодуль.УдалитьВсеФайлы"
            }),
        );

        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);

        expect![[r#"
            MissingTemporaryFileDeletion @ 7:30..7:63
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 10:31..10:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла2' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 14:31..14:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла3' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 20:31..20:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла4' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 26:31..26:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла5' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 31:31..31:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла6' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 35:17..35:39
              message: Нужно добавить удаление временного файла 'TempFile7' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 46:30..46:63
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 50:31..50:64
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла2' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 61:30..61:58
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 65:31..65:59
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла3' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 72:27..72:55
              message: Нужно добавить удаление временного файла 'ИмяФайлаНаДиске' после использования
              severity: Warning"#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_range_debug() {
        let code = r#"
Процедура Тест()
    ИмяПромежуточногоФайла = ПолучитьИмяВременногоФайла("xml"); // ошибка
КонецПроцедуры
        "#;
        let config = DiagnosticsConfig::default();
        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);

        expect![[r#"
            MissingTemporaryFileDeletion @ 3:30..3:63
              message: Нужно добавить удаление временного файла 'ИмяПромежуточногоФайла' после использования
              severity: Warning"#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_inline_usage() {
        let code = r#"
            Процедура Тест()
                Записать(GetTempFileName("txt"));
                ПолучитьИмяВременногоФайла("xml");  // standalone call
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);

        expect![[r#"
            MissingTemporaryFileDeletion @ 3:26..3:48
              message: Нужно добавить удаление временного файла после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 4:17..4:50
              message: Нужно добавить удаление временного файла после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));

        let code2 = r#"
            Процедура Тест()
                Файл = Новый Файл(ПолучитьИмяВременногоФайла("xml"));
            КонецПроцедуры
        "#;
        let diagnostics2 = check_body_diagnostic(code2, check_body);

        expect![[r#"
            MissingTemporaryFileDeletion @ 3:35..3:68
              message: Нужно добавить удаление временного файла после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code2, &diagnostics2));
    }

    #[test]
    fn test_comprehensive_java_compatibility() {
        let code = r#"
            Процедура ТестВсехКейсов()
                // Case 1: Normal assignment with deletion - OK
                Файл1 = ПолучитьИмяВременногоФайла("xml");
                УдалитьФайлы(Файл1);

                // Case 2: Normal assignment without deletion - ERROR
                Файл2 = ПолучитьИмяВременногоФайла("xml");

                // Case 3: Inline usage in function call - ERROR
                Записать(GetTempFileName("txt"));

                // Case 4: Inline usage in expression - ERROR
                Файл3 = Новый Файл(ПолучитьИмяВременногоФайла("doc"));

                // Case 5: Standalone call - ERROR
                ПолучитьИмяВременногоФайла("tmp");

                // Case 6: Assignment with move (not deletion) - OK with default config
                Файл4 = ПолучитьИмяВременногоФайла("xml");
                ПереместитьФайл(Файл4, "новое_имя.xml");
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);

        expect![[r#"
            MissingTemporaryFileDeletion @ 8:25..8:58
              message: Нужно добавить удаление временного файла 'Файл2' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 11:26..11:48
              message: Нужно добавить удаление временного файла после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 14:36..14:69
              message: Нужно добавить удаление временного файла после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 17:17..17:50
              message: Нужно добавить удаление временного файла после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_simple_cases() {
        let code = r#"
            Процедура Тест()
                ИмяФайла = ПолучитьИмяВременногоФайла("xml");
                УдалитьФайлы(ИмяФайла);
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));

        let code = r#"
            Процедура Тест()
                ИмяФайла = ПолучитьИмяВременногоФайла("xml");
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:28..3:61
              message: Нужно добавить удаление временного файла 'ИмяФайла' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));

        let code = r#"
            Процедура Тест()
                ИмяФайла = ПолучитьИмяВременногоФайла("xml");
                ПереместитьФайл(ИмяФайла, "новое_имя.xml");
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_case_insensitive() {
        let code = r#"
            Процедура Тест()
                Файл1 = ПОЛУЧИТЬИМЯВРЕМЕННОГОФАЙЛА("xml");
                Файл2 = получитьимявременногофайла("xml");
                Файл3 = ПолучитьИмяВременногоФайла("xml");
                УдалитьФайлы(Файл3);
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:25..3:58
              message: Нужно добавить удаление временного файла 'Файл1' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 4:25..4:58
              message: Нужно добавить удаление временного файла 'Файл2' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_english_keywords() {
        let code = r#"
            Procedure Test()
                TempFile = GetTempFileName("xml");
            EndProcedure
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:28..3:50
              message: Нужно добавить удаление временного файла 'TempFile' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));

        let code = r#"
            Procedure Test()
                TempFile = GetTempFileName("xml");
                DeleteFiles(TempFile);
            EndProcedure
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_module_qualified_calls() {
        let code = r#"
            Процедура Тест()
                ИмяФайла = ПолучитьИмяВременногоФайла("xml");
                РаботаСФайламиКлиент.УдалитьФайл(Неопределено, ИмяФайла);
            КонецПроцедуры
        "#;
        let config = DiagnosticsConfig::default();
        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:28..3:61
              message: Нужно добавить удаление временного файла 'ИмяФайла' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));

        let mut config = DiagnosticsConfig::default();
        config.parameters.insert(
            DiagnosticCode::MissingTemporaryFileDeletion,
            serde_json::json!({
                "searchDeleteFileMethod": "УдалитьФайлы|DeleteFiles|РаботаСФайламиКлиент.УдалитьФайл"
            }),
        );
        let diagnostics = check_body_diagnostic_with_config(code, config, check_body);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_lattice_flags_conditional_cleanup_leak() {
        let code = r#"
            Процедура Тест(Условие)
                Файл = ПолучитьИмяВременногоФайла("xml");
                Если Условие Тогда
                    УдалитьФайлы(Файл);
                КонецЕсли;
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:24..3:57
              message: Нужно добавить удаление временного файла 'Файл' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_multi_arg_deletion_closes_each_file() {
        let code = r#"
            Процедура Тест()
                Файл1 = ПолучитьИмяВременногоФайла("xml");
                Файл2 = ПолучитьИмяВременногоФайла("txt");
                УдалитьФайлы(Файл1, Файл2);
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_get_in_branch_delete_in_separate_branch_leaks() {
        let code = r#"
            Процедура Тест(Условие1, Условие2)
                Если Условие1 Тогда
                    Файл = ПолучитьИмяВременногоФайла("xml");
                КонецЕсли;
                Если Условие2 Тогда
                    УдалитьФайлы(Файл);
                КонецЕсли;
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 4:28..4:61
              message: Нужно добавить удаление временного файла 'Файл' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    #[ignore = "Move-method destination semantics: `ПереместитьФайл(src, dst)` leaves dst as a live temp file, but the lattice (matching pre-Step-Q legacy behavior) treats every variable referenced by any matching deletion-method arg as closed. Fix needs per-method arg-role metadata. Tracked as a follow-up beyond Track 1's scope; pinned here so the fix flips this test green."]
    fn test_move_method_destination_leaks() {
        let code = r#"
            Процедура Тест()
                Файл1 = ПолучитьИмяВременногоФайла("xml");
                Файл2 = ПолучитьИмяВременногоФайла("txt");
                ПереместитьФайл(Файл1, Файл2);
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(
            diagnostics.len(),
            1,
            "Move destination must remain open until explicit cleanup"
        );
        assert!(diagnostics[0].message.contains("Файл2"));
    }

    #[test]
    #[ignore = "Variable-generation tracking: a re-assignment to the same name overwrites the prior Get-site's runtime reachability, but on disk the prior temp file is still alive — leaked. The lattice keys resources on lowercased var name only, so the later delete clears both opening sites. Pre-Step-Q's body-wide BFS had the same blindspot; preserved as legacy parity. Fix requires reaching-definitions on the temp-name binding."]
    fn test_reassigned_variable_first_get_leaks() {
        let code = r#"
            Процедура Тест()
                Файл = ПолучитьИмяВременногоФайла("a");
                Файл = ПолучитьИмяВременногоФайла("b");
                УдалитьФайлы(Файл);
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        assert_eq!(diagnostics.len(), 1, "First Get of a re-assigned name leaks");
    }

    #[test]
    fn test_ternary_deletion_arg_does_not_close_either_branch() {
        let code = r#"
            Процедура Тест(Условие)
                Файл1 = ПолучитьИмяВременногоФайла("xml");
                Файл2 = ПолучитьИмяВременногоФайла("txt");
                УдалитьФайлы(?(Условие, Файл1, Файл2));
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:25..3:58
              message: Нужно добавить удаление временного файла 'Файл1' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 4:25..4:58
              message: Нужно добавить удаление временного файла 'Файл2' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_deletion_as_first_protected_statement_is_cleanup() {
        let code = r#"
            Процедура Тест(Данные)
                ИмяФайла = ПолучитьИмяВременногоФайла("zip");
                Данные.Записать(ИмяФайла);
                Попытка
                    УдалитьФайлы(ИмяФайла);
                Исключение
                КонецПопытки;

                ИмяКаталога = ПолучитьИмяВременногоФайла();
                Попытка
                    УдалитьФайлы(ИмяКаталога);
                Исключение
                    ЗаписьЖурналаРегистрации("Тест", УровеньЖурналаРегистрации.Ошибка, , ,
                        ПодробноеПредставлениеОшибки(ИнформацияОбОшибке()));
                КонецПопытки;
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#""#]].assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_try_with_work_before_deletion_still_leaks() {
        let code = r#"
            Процедура Тест(Данные)
                ИмяФайла = ПолучитьИмяВременногоФайла("zip");
                Попытка
                    Данные.Записать(ИмяФайла);
                    УдалитьФайлы(ИмяФайла);
                Исключение
                КонецПопытки;
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:28..3:61
              message: Нужно добавить удаление временного файла 'ИмяФайла' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_second_deletion_in_same_try_still_leaks() {
        let code = r#"
            Процедура Тест()
                Файл1 = ПолучитьИмяВременногоФайла("xml");
                Файл2 = ПолучитьИмяВременногоФайла("txt");
                Попытка
                    УдалитьФайлы(Файл1);
                    УдалитьФайлы(Файл2);
                Исключение
                КонецПопытки;
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 4:25..4:58
              message: Нужно добавить удаление временного файла 'Файл2' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_try_deleting_other_file_does_not_clean_up() {
        let code = r#"
            Процедура Тест(ДругойФайл)
                ИмяФайла = ПолучитьИмяВременногоФайла("zip");
                Попытка
                    УдалитьФайлы(ДругойФайл);
                Исключение
                КонецПопытки;
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:28..3:61
              message: Нужно добавить удаление временного файла 'ИмяФайла' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_returned_name_is_left_to_caller() {
        let code = r#"
            Функция ИмяФайлаДанных(Данные)
                ИмяФайла = ПолучитьИмяВременногоФайла("xml");
                Данные.Записать(ИмяФайла);
                Возврат ИмяФайла;
            КонецФункции

            Функция РазмерДанных(Данные)
                ИмяФайла = ПолучитьИмяВременногоФайла("xml");
                Данные.Записать(ИмяФайла);
                Возврат СтрДлина(ИмяФайла);
            КонецФункции
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 9:28..9:61
              message: Нужно добавить удаление временного файла 'ИмяФайла' после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }

    #[test]
    fn test_inline_and_assigned_get_in_same_body_emit_separately() {
        let code = r#"
            Процедура Тест()
                Файл = ПолучитьИмяВременногоФайла("xml");
                Записать(GetTempFileName("txt"));
            КонецПроцедуры
        "#;
        let diagnostics = check_body_diagnostic(code, check_body);
        expect![[r#"
            MissingTemporaryFileDeletion @ 3:24..3:57
              message: Нужно добавить удаление временного файла 'Файл' после использования
              severity: Warning
            MissingTemporaryFileDeletion @ 4:26..4:48
              message: Нужно добавить удаление временного файла после использования
              severity: Warning"#]]
        .assert_eq(&format_diags(code, &diagnostics));
    }
}
