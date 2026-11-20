use hir::{Expr, ExprId, ExprIdx, IdConversion, LocalRange};
use stdx::case::fold_lower_per_char;

use crate::define_metadata;
use crate::metadata::*;
use crate::{BodyContext, Diagnostic, DiagnosticCode};

pub const METADATA: DiagnosticMetadata = define_metadata! {
    diagnostic_type: DiagnosticType::Error,
    severity: DiagnosticSeverityLevel::Critical,
    scope: DiagnosticScope::Bsl,
    modules: &[bsl_metadata::ModuleType::FormModule],
    minutes_to_fix: 5,
    activated_by_default: true,
    compatibility_mode: DiagnosticCompatibilityMode::Undefined,
    tags: &[MetadataTag::Error],
    can_locate_on_project: false,
    extra_min_for_complexity: 0.0,
    lsp_severity_override: "",
};

const MESSAGE: &str = "У данных формы нет метода Метаданные(): получать метаданные на сервере - Объект.Ссылка.Метаданные() в &НаСервере либо Метаданные.<Коллекция>[ИмяОбъекта]";

/// Lowercased spellings (Russian and English) of the method the predicate matches.
const METADATA_METHOD: &[&str] = &["метаданные", "metadata"];

/// Reports a call of `Метаданные` / `Metadata` on a receiver whose inferred type is the data of
/// a form's object attribute (`ДанныеФормыСтруктура`): that type has no such method, so the
/// call raises an unknown-method error at runtime.
///
/// The receiver is judged by its TYPE, never by its name: a parameter called `Объект` that holds
/// a real reference keeps the method, and a receiver of unknown or union type yields nothing.
pub fn check_body(ctx: &BodyContext, acc: &mut Vec<Diagnostic<LocalRange>>) {
    let code = DiagnosticCode::FormDataHasNoMetadataMethod;

    if ctx.is_disabled_with_metadata(code)
        || ctx.module_metadata().module_type != bsl_metadata::ModuleType::FormModule
    {
        return;
    }
    let body = ctx.body();
    if body.is_lowering_incomplete() {
        return;
    }
    let mut calls: Vec<ExprId> = Vec::new();
    for (id, expr) in body.exprs_iter() {
        let (receiver, method) = match expr {
            Expr::MethodCall { receiver, method, args } if args.is_empty() => {
                (*receiver, method.as_str())
            }
            Expr::Call { callee, args } if args.is_empty() => {
                let Expr::Field { base, field } = body.expr_idx(*callee) else { continue };
                (*base, field.as_str())
            }
            _ => continue,
        };
        if is_metadata_method(method) && receiver_is_form_data_structure(ctx, receiver) {
            calls.push(id);
        }
    }

    for call in calls {
        let Some(range) = ctx.source_map().expr_range(call) else {
            continue;
        };
        acc.push(Diagnostic {
            code,
            message: MESSAGE.to_string(),
            severity: ctx.severity(code),
            range,
            tags: ctx.tags(code),
            fixes: vec![],
        });
    }
}

fn is_metadata_method(name: &str) -> bool {
    METADATA_METHOD.contains(&fold_lower_per_char(name).as_str())
}

/// The receiver is typed by the inference engine and must come out as exactly form data of an
/// object attribute (not a union, not unknown).
fn receiver_is_form_data_structure(ctx: &BodyContext, receiver: ExprIdx) -> bool {
    ctx.infer()
        .type_id_of_expr(ExprId::from_idx(receiver))
        .is_some_and(|ty| ctx.kernel_type_is_form_data_structure(ty))
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{check_diagnostics_for, check_form_with_owner_xml, range_to_line_col};
    use crate::DiagnosticCode;

    const OWNER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <DataProcessor uuid="00000000-0000-0000-0000-000000000101">
        <Properties><Name>Товары</Name></Properties>
        <ChildObjects>
            <Attribute uuid="00000000-0000-0000-0000-000000000104">
                <Properties><Name>Реквизит</Name><Type><v8:Type>xs:string</v8:Type></Type></Properties>
            </Attribute>
        </ChildObjects>
    </DataProcessor>
</MetaDataObject>"#;

    const FORM_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Form xmlns="http://v8.1c.ru/8.3/xcf/logform" xmlns:cfg="http://v8.1c.ru/8.1/data/enterprise/current-config" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema" version="2.10">
    <Attributes>
        <Attribute name="Объект" id="1">
            <Type><v8:Type>cfg:DataProcessorObject.Товары</v8:Type></Type>
            <MainAttribute>true</MainAttribute>
        </Attribute>
    </Attributes>
</Form>"#;

    fn run_in_form(body: &str) -> Vec<crate::Diagnostic> {
        check_form_with_owner_xml(body, FORM_XML, OWNER_XML)
            .into_iter()
            .filter(|d| d.code == DiagnosticCode::FormDataHasNoMetadataMethod)
            .collect()
    }

    #[test]
    fn metadata_call_on_the_object_attribute_of_a_form_is_flagged() {
        let source = "&НаКлиенте
Процедура Тест()
    Имя = Объект.Метаданные().Имя;
КонецПроцедуры
";
        let d = run_in_form(source);
        assert_eq!(d.len(), 1, "{d:?}");
        assert_eq!(
            d[0].message,
            "У данных формы нет метода Метаданные(): получать метаданные на сервере - Объект.Ссылка.Метаданные() в &НаСервере либо Метаданные.<Коллекция>[ИмяОбъекта]"
        );
        let (line, start, _, end) = range_to_line_col(source, d[0].range);
        assert_eq!((line, start, end), (2, 10, 29), "{:?}", d[0]);
    }

    #[test]
    // Not flagged because the receiver is a reference, not form data. Whether this call WORKS
    // depends on where it runs: `Метаданные()` is a server-side method (refused on the thin client).
    fn metadata_call_on_a_reference_receiver_is_outside_the_scope_of_the_diagnostic() {
        let d = run_in_form(
            "&НаСервере
Процедура Тест()
    Имя = Объект.Ссылка.Метаданные().Имя;
КонецПроцедуры
",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn parameter_named_object_holding_a_reference_is_not_flagged_but_the_attribute_still_is() {
        let source = "&НаКлиентеНаСервереБезКонтекста
Функция ИмяОбъекта(Объект)
    Возврат Объект.Метаданные().Имя;
КонецФункции

&НаКлиенте
Процедура Тест()
    Имя = Объект.Метаданные().Имя;
КонецПроцедуры
";
        let d = run_in_form(source);
        assert_eq!(d.len(), 1, "{d:?}");
        let (line, _, _, _) = range_to_line_col(source, d[0].range);
        assert_eq!(line, 7, "only the call on the form attribute is reported: {:?}", d[0]);
    }

    #[test]
    fn metadata_call_on_a_receiver_of_unknown_type_is_not_flagged() {
        let d = run_in_form(
            "&НаКлиенте
Процедура Тест(Параметр)
    Параметр.Метаданные();
    Неизвестно.Метаданные();
    Чужой().Метаданные();
КонецПроцедуры
",
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn metadata_call_outside_a_form_module_is_not_flagged() {
        let d = check_diagnostics_for(
            "Процедура Тест()
    Имя = Объект.Метаданные().Имя;
КонецПроцедуры
",
            DiagnosticCode::FormDataHasNoMetadataMethod,
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn english_spelling_of_the_method_is_flagged_the_same_way() {
        let d = run_in_form(
            "&AtClient
Procedure Test()
    Name = Объект.Metadata().Name;
EndProcedure
",
        );
        assert_eq!(d.len(), 1, "{d:?}");
    }
}
