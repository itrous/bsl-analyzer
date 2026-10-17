use crate::define_metadata;
use crate::metadata::*;
use crate::AnalysisContext;
use crate::{Diagnostic, DiagnosticCode};
use hir::{LocalRange, MissingMetadata};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::All,
    modules: &[],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error, MetadataTag::Suspicious],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

fn message(missing: &MissingMetadata, ctx: &AnalysisContext) -> String {
    let ru = matches!(ctx.locale(), base_db::Locale::Ru);
    let kind = |kind: &bsl_metadata::MdoType| {
        if ru {
            kind.russian_name()
        } else {
            kind.english_name()
        }
    };
    match missing {
        MissingMetadata::Object { kind: mdo, name } if ru => {
            format!("Объект метаданных \"{}.{name}\" не найден в конфигурации", kind(mdo))
        }
        MissingMetadata::Object { kind: mdo, name } => {
            format!("Metadata object \"{}.{name}\" is not found in the configuration", kind(mdo))
        }
        MissingMetadata::PredefinedValue { kind: mdo, owner, value } if ru => {
            format!("Предопределенное значение \"{value}\" не найдено у \"{}.{owner}\"", kind(mdo))
        }
        MissingMetadata::PredefinedValue { kind: mdo, owner, value } => {
            format!("Predefined value \"{value}\" is not found in \"{}.{owner}\"", kind(mdo))
        }
    }
}

/// A metadata path written as the string argument of a platform function that names
/// what the configuration does not have (`hir_ty::metadata_path`).
pub fn from_hir(
    missing: &MissingMetadata,
    range: LocalRange,
    ctx: &AnalysisContext,
) -> Option<Diagnostic<LocalRange>> {
    crate::simple_hir_diagnostic(
        DiagnosticCode::UnresolvedMetadataPath,
        message(missing, ctx),
        range,
        ctx,
    )
}
