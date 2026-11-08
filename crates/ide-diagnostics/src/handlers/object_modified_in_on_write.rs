use hir::{Body, Expr, ExprId, IdConversion, LocalRange, Stmt};
use rustc_hash::FxHashSet;
use stdx::case::fold_lower_per_char;

use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::CodeSmell,
    severity: DiagnosticSeverityLevel::Major,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::ObjectModule],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Standard, MetadataTag::Badpractice],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

/// Lowercased handler names (Russian and English spelling).
const HANDLER_NAMES: &[&str] = &["призаписи", "onwrite"];

/// Lowercased spellings of the object reference (`ЭтотОбъект` / `ThisObject`).
const THIS_OBJECT_NAMES: &[&str] = &["этотобъект", "thisobject"];

/// Reports an assignment to the object's own data field inside the `ПриЗаписи` /
/// `OnWrite` handler of an object module (#std465: the object is already written
/// to the database at that point, so the change never reaches it).
///
/// The "own data field" test is a property, not a name list: the assigned name
/// must be an attribute, a common attribute or a tabular section of the module's
/// own metadata object. Platform properties of the object (`ДополнительныеСвойства`,
/// `Движения`, `ОбменДанными`, ...) are absent from that set, so they stay silent
/// without any hand-written exclusions.
///
/// A name declared with `Перем` inside the procedure shadows the attribute: this
/// was measured on the live platform, `Перем Склад;` inside the procedure compiles
/// and the following assignment writes the local, leaving the attribute untouched.
/// At module level the platform refuses such a declaration outright
/// («Переменная с указанным именем уже определена»), so only the procedure body
/// needs the shadowing check. A procedure parameter shadows the attribute too.
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::ObjectModifiedInOnWrite;

    if ctx.is_disabled_with_metadata(code) {
        return;
    }
    if ctx.is_module_code() {
        return;
    }
    let metadata = ctx.module_metadata();
    if metadata.module_type != bsl_metadata::ModuleType::ObjectModule {
        return;
    }
    let Some(decl) = ctx.decl() else { return };
    if decl.is_function
        || !HANDLER_NAMES.contains(&fold_lower_per_char(decl.name.as_str()).as_str())
    {
        return;
    }
    let Some(mdo) = metadata.mdo.as_ref() else { return };

    let params: FxHashSet<String> =
        decl.params.iter().map(|p| fold_lower_per_char(p.name.as_str())).collect();
    let body = ctx.body();
    let locals = declared_locals(body);

    for (_, stmt) in body.stmts_iter() {
        let Stmt::Assign { target, .. } = stmt else { continue };
        let Some(name) = assigned_name(body, *target) else { continue };
        let folded = fold_lower_per_char(name);
        if params.contains(&folded) || locals.contains(&folded) {
            continue;
        }
        let Some(kind) = own_field_kind(mdo, &folded) else { continue };
        let Some(range) = ctx.source_map().expr_range(ExprId::from_idx(*target)) else {
            continue;
        };
        let message = match kind {
            FieldKind::Attribute => format!(
                "Реквизит «{name}» изменяется в обработчике ПриЗаписи: объект уже записан в базу, и изменение в неё не попадёт. Заполнять реквизиты объекта нужно в ПередЗаписью (#std465)"
            ),
            FieldKind::TabularSection => format!(
                "Табличная часть «{name}» изменяется в обработчике ПриЗаписи: объект уже записан в базу, и изменение в неё не попадёт. Заполнять данные объекта нужно в ПередЗаписью (#std465)"
            ),
        };
        acc.push(Diagnostic {
            code,
            message,
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

enum FieldKind {
    Attribute,
    TabularSection,
}

/// The name written by an assignment target: a bare name or `ЭтотОбъект.<Имя>`.
fn assigned_name(body: &Body, target: hir::ExprIdx) -> Option<&str> {
    match body.expr_idx(target) {
        Expr::Path(name) => Some(name.as_str()),
        Expr::Field { base, field } => match body.expr_idx(*base) {
            Expr::Path(p)
                if THIS_OBJECT_NAMES.contains(&fold_lower_per_char(p.as_str()).as_str()) =>
            {
                Some(field.as_str())
            }
            _ => None,
        },
        _ => None,
    }
}

/// Folded names declared by `Перем` anywhere in the body.
fn declared_locals(body: &Body) -> FxHashSet<String> {
    let mut locals = FxHashSet::default();
    for (_, stmt) in body.stmts_iter() {
        if let Stmt::VarDecl { bindings } = stmt {
            for &id in bindings.iter() {
                locals.insert(fold_lower_per_char(body.binding_idx(id).name.as_str()));
            }
        }
    }
    locals
}

fn matches_name(folded: &str, name: &str, name_en: Option<&str>) -> bool {
    fold_lower_per_char(name) == folded
        || name_en.is_some_and(|en| !en.is_empty() && fold_lower_per_char(en) == folded)
}

fn own_field_kind(mdo: &bsl_metadata::MetadataObject, folded: &str) -> Option<FieldKind> {
    if mdo.attributes.iter().any(|a| matches_name(folded, &a.name, a.name_en.as_deref()))
        || mdo.common_attributes.iter().any(|a| matches_name(folded, &a.name, None))
    {
        return Some(FieldKind::Attribute);
    }
    if mdo.tabular_sections.iter().any(|ts| matches_name(folded, ts.name(), ts.name_en())) {
        return Some(FieldKind::TabularSection);
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::DiagnosticCode;

    fn run(code: &str) -> Vec<crate::Diagnostic> {
        let mut metadata = crate::test_utils::make_non_common_module_metadata(
            bsl_metadata::ModuleType::ObjectModule,
        );
        let mut mdo = bsl_metadata::MetadataObject::new(bsl_metadata::MdoType::Catalog, "Товары");
        mdo.attributes.push(bsl_metadata::Attribute {
            name: "Склад".to_string(),
            name_en: Some("Warehouse".to_string()),
            attr_type: bsl_metadata::AttributeType::String { length: Some(100) },
        });
        mdo.attributes.push(bsl_metadata::Attribute {
            name: "Основная".to_string(),
            name_en: None,
            attr_type: bsl_metadata::AttributeType::String { length: Some(10) },
        });
        mdo.tabular_sections
            .push(bsl_metadata::TabularSection::new(uuid::Uuid::new_v4(), "Состав"));
        metadata.mdo = Some(std::sync::Arc::new(mdo));
        crate::test_utils::check_metadata_diagnostic(metadata, code, |_m, ctx| {
            crate::diagnostics(ctx)
        })
        .into_iter()
        .filter(|d| d.code == DiagnosticCode::ObjectModifiedInOnWrite)
        .collect()
    }

    #[test]
    fn fires_on_bare_attribute_assignment() {
        let d = run("Процедура ПриЗаписи(Отказ)\n    Склад = 1;\nКонецПроцедуры");
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].message.starts_with("Реквизит «Склад» изменяется в обработчике ПриЗаписи"));
        assert!(d[0].message.contains("ПередЗаписью (#std465)"));
    }

    #[test]
    fn fires_on_this_object_field() {
        let d = run("Процедура ПриЗаписи(Отказ)\n    ЭтотОбъект.Склад = 1;\nКонецПроцедуры");
        assert_eq!(d.len(), 1, "{d:?}");
    }

    #[test]
    fn fires_on_english_body() {
        let d = run("Procedure OnWrite(Cancel)\n    ThisObject.Warehouse = 1;\nEndProcedure");
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].message.starts_with("Реквизит «Warehouse»"));
    }

    #[test]
    fn fires_on_tabular_section_with_its_own_wording() {
        let d = run("Процедура ПриЗаписи(Отказ)\n    Состав = Неопределено;\nКонецПроцедуры");
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].message.starts_with("Табличная часть «Состав» изменяется"));
        assert!(d[0].message.contains("Заполнять данные объекта нужно в ПередЗаписью"));
    }

    #[test]
    fn silent_for_parameter() {
        let d = run("Процедура ПриЗаписи(Отказ)\n    Отказ = Истина;\nКонецПроцедуры");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn silent_for_attribute_of_another_object() {
        let d = run(
            "Процедура ПриЗаписи(Отказ)\n    ЧужойОбъект = Справочники.Товары.СоздатьЭлемент();\n    ЧужойОбъект.Основная = Ложь;\nКонецПроцедуры",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn silent_for_platform_properties() {
        let d = run(
            "Процедура ПриЗаписи(Отказ)\n    ДополнительныеСвойства.Удалить(\"X\");\n    Движения.Записать();\n    Если ОбменДанными.Загрузка Тогда\n        Возврат;\n    КонецЕсли;\nКонецПроцедуры",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn silent_for_ordinary_local() {
        let d = run("Процедура ПриЗаписи(Отказ)\n    Запрос = Новый Запрос;\nКонецПроцедуры");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn silent_in_before_write() {
        let d = run("Процедура ПередЗаписью(Отказ)\n    Склад = 1;\nКонецПроцедуры");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn silent_for_attribute_read() {
        let d = run(
            "Процедура ПриЗаписи(Отказ)\n    Если Склад = 1 Тогда\n        Возврат;\n    КонецЕсли;\nКонецПроцедуры",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn silent_when_local_var_shadows_attribute() {
        let d = run("Процедура ПриЗаписи(Отказ)\n    Перем Склад;\n    Склад = 1;\nКонецПроцедуры");
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn fires_inside_if_block() {
        let d = run("Процедура ПриЗаписи(Отказ)
    Если ДополнительныеСвойства.Свойство(\"Данные\") Тогда
        Склад = 1;
        Основная = Ложь;
        Записать();
    КонецЕсли;
КонецПроцедуры");
        assert_eq!(d.len(), 2, "{d:?}");
        assert!(d.iter().all(|x| x.message.starts_with("Реквизит")), "{d:?}");
    }

    #[test]
    fn fires_inside_for_each_body() {
        let d = run("Процедура ПриЗаписи(Отказ)
    Для Каждого Элемент Из Коллекция Цикл
        Склад = Элемент;
    КонецЦикла;
КонецПроцедуры");
        assert_eq!(d.len(), 1, "{d:?}");
    }
}
