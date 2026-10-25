use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::LocalRange;
use hir::Name;
use hir::UninitializedPath;

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

pub fn from_hir(
    name: &Name,
    path: UninitializedPath,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    use base_db::Locale::{En, Ru};
    use UninitializedPath::{Branch, FirstIteration, LoopNotRun, OtherPath};

    let name = name.as_str();
    let message = match (path, ctx.locale()) {
        (FirstIteration, Ru) => format!(
            "Переменная '{name}' может быть не инициализирована: на первой итерации цикла значение ей ещё не присвоено, присваивание в цикле действует только на следующих итерациях"
        ),
        (FirstIteration, En) => format!(
            "Variable '{name}' may be uninitialized: on the first iteration of the loop it is not assigned yet; the assignment in the loop takes effect only on later iterations"
        ),
        (LoopNotRun, Ru) => format!(
            "Переменная '{name}' может быть не инициализирована: на пути, где цикл не выполняется ни разу, значение ей не присваивается"
        ),
        (LoopNotRun, En) => format!(
            "Variable '{name}' may be uninitialized: it is not assigned on a path where the loop runs zero times"
        ),
        (Branch, Ru) => format!(
            "Переменная '{name}' может быть не инициализирована: на пути через ветвь условия значение ей не присваивается"
        ),
        (Branch, En) => format!(
            "Variable '{name}' may be uninitialized: it is not assigned on a path through a branch of a condition"
        ),
        (OtherPath, Ru) => format!(
            "Переменная '{name}' может быть не инициализирована: на одном из путей к этому чтению значение ей не присваивается"
        ),
        (OtherPath, En) => {
            format!("Variable '{name}' may be uninitialized: it is not assigned on some path to this read")
        }
    };
    crate::simple_hir_diagnostic(DiagnosticCode::UninitializedLocalVariable, message, range, ctx)
}
