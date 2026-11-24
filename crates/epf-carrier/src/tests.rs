use check_contract::{
    CheckRequest, InputKind, ModuleContext, ModuleType, Origin, Status, WireLimits, SCHEMA_VERSION,
};

use crate::{admit, container, AdmissionReason, SourcePosition, MAX_MODULE_BYTES};

fn request(source: &str) -> CheckRequest {
    CheckRequest {
        schema_version: SCHEMA_VERSION.to_owned(),
        request_id: "epf-carrier-test".to_owned(),
        connection: "synthetic".to_owned(),
        code: source.to_owned(),
        limits: WireLimits {
            timeout_ms: 1000,
            max_code_bytes: MAX_MODULE_BYTES as u64,
            max_response_bytes: 65536,
            max_stderr_bytes: 1024,
            stop_grace_ms: 100,
        },
        input_kind: InputKind::Module,
        module_type: Some(ModuleType::Object),
        context: Some(ModuleContext::Synthetic { module_type: ModuleType::Object }),
    }
}

#[test]
fn packages_declarations_without_invoking_their_bodies() {
    let source = include_str!("../fixtures/declarations.bsl");
    let request = request(source);
    let admitted = admit(&request).unwrap();
    assert_eq!(admitted.source().as_bytes(), source.as_bytes());
    let bytes = admitted.pack().unwrap();
    assert_eq!(container::module_text(&bytes), source.as_bytes());
}

#[test]
fn admits_the_native_declaration_reference() {
    let request = request(include_str!("../fixtures/reference-source.bsl"));
    let admitted = admit(&request).unwrap();
    assert_eq!(container::module_text(&admitted.pack().unwrap()), request.code.as_bytes());
}

#[test]
fn preserves_newlines_and_only_adds_the_utf8_signature() {
    for newline in ["\n", "\r\n", "\r"] {
        let source = ["Procedure Check()", "    Return;", "EndProcedure"].join(newline);
        let request = request(&source);
        let admitted = admit(&request).unwrap();
        let payload = container::module_text(&admitted.pack().unwrap());
        assert_eq!(&payload[..3], b"\xef\xbb\xbf");
        assert_eq!(&payload[3..], source.as_bytes());
        assert_eq!(admitted.source(), source);
    }
}

#[test]
fn keeps_original_utf16_positions_with_bom_crlf_and_emoji() {
    let source = "\u{feff}Процедура Проверка()\r\n    X = \"😀\";\r\nКонецПроцедуры";
    let request = request(source);
    let admitted = admit(&request).unwrap();
    let emoji = source.find('😀').unwrap();
    assert_eq!(admitted.position(3), Some(SourcePosition { line: 1, column: 2 }));
    assert_eq!(admitted.position(emoji), Some(SourcePosition { line: 2, column: 10 }));
    assert_eq!(admitted.position(emoji + 4), Some(SourcePosition { line: 2, column: 12 }));
    assert_eq!(admitted.position(emoji + 1), None);
    assert_eq!(container::module_text(&admitted.pack().unwrap()), source.as_bytes());
}

#[test]
fn rejects_every_main_program_branch_before_packaging() {
    for source in [
        include_str!("../fixtures/main-body.bsl"),
        include_str!("../fixtures/main-body-in-conditional.bsl"),
    ] {
        let parsed = parser::parse(source);
        assert!(!parsed.has_errors(), "control must have a complete AST: {:?}", parsed.errors());
        let request = request(source);
        let error = admit(&request).unwrap_err();
        assert_eq!(error.reason, AdmissionReason::MainProgram);
        assert_eq!(error.status(), Status::Unsupported);
        assert!(error.position.is_some());
    }
}

#[test]
fn rejects_incomplete_parses_instead_of_claiming_native_invalid() {
    let request = request(include_str!("../fixtures/truncated.bsl"));
    let error = admit(&request).unwrap_err();
    assert_eq!(error.reason, AdmissionReason::IncompleteSyntax);
    assert_eq!(error.status(), Status::Unsupported);
}

#[test]
fn requires_explicit_synthetic_object_context() {
    let source = include_str!("../fixtures/semantic-invalid.bsl");
    for context in [
        None,
        Some(ModuleContext::Metadata {
            owner: "Catalog.Items".to_owned(),
            origin: Origin::Configuration {},
        }),
    ] {
        let mut request = request(source);
        request.context = context;
        let error = admit(&request).unwrap_err();
        assert_eq!(error.reason, AdmissionReason::ContextRequired);
        assert_eq!(error.status(), Status::ContextRequired);
    }
    for kind in
        [ModuleType::Common, ModuleType::Manager, ModuleType::ManagedForm, ModuleType::OrdinaryForm]
    {
        let mut request = request(source);
        request.module_type = Some(kind);
        request.context = Some(ModuleContext::Synthetic { module_type: kind });
        assert_eq!(admit(&request).unwrap_err().reason, AdmissionReason::ModuleType);
    }
}

#[test]
fn does_not_accept_snippets_or_invalid_wire_requests() {
    let mut request = request(include_str!("../fixtures/reference-source.bsl"));
    request.input_kind = InputKind::Snippet;
    request.module_type = None;
    request.context = None;
    assert_eq!(admit(&request).unwrap_err().reason, AdmissionReason::InputKind);
    request.input_kind = InputKind::Module;
    request.schema_version = "unknown".to_owned();
    assert_eq!(admit(&request).unwrap_err().reason, AdmissionReason::InvalidRequest);
}

#[test]
fn rejects_client_extension_and_unknown_directives() {
    let cases = [
        (include_str!("../fixtures/client-branch.bsl"), AdmissionReason::UnsupportedDirective),
        ("&НаКлиенте\nПроцедура Проверка()\nКонецПроцедуры", AdmissionReason::ClientContext),
        (
            "&Перед(\"Метод\")\nПроцедура Проверка()\nКонецПроцедуры",
            AdmissionReason::UnsupportedDirective,
        ),
        (
            "#Вставка\nПроцедура Проверка()\nКонецПроцедуры\n#КонецВставки",
            AdmissionReason::UnsupportedDirective,
        ),
        (
            "&НеизвестнаяАннотация\nПроцедура Проверка()\nКонецПроцедуры",
            AdmissionReason::UnsupportedDirective,
        ),
    ];
    for (source, reason) in cases {
        assert_eq!(admit(&request(source)).unwrap_err().reason, reason, "{source}");
    }
}

#[test]
fn keeps_owner_requirements_distinct_from_synthetic_compiler_errors() {
    let source = include_str!("../fixtures/owner-field.bsl");
    let error = admit(&request(source)).unwrap_err();
    assert_eq!(error.reason, AdmissionReason::OwnerContext);
    assert_eq!(error.status(), Status::ContextRequired);
    let semantic_error = request(include_str!("../fixtures/semantic-invalid.bsl"));
    // AST admission cannot decide whether a procedure may return a value.
    assert!(admit(&semantic_error).unwrap().pack().is_ok());
}

#[test]
fn accepts_parameter_properties_and_its_own_methods_without_inventing_an_owner() {
    let source = "Функция Поле(Значение)\nВозврат Значение.Поле;\nКонецФункции\nФункция ВызватьСвойМетод(Значение)\nВозврат Поле(Значение);\nКонецФункции";
    assert!(admit(&request(source)).is_ok());
    assert_eq!(
        admit(&request("Функция Поле()\nВозврат ЭтотОбъект.Поле;\nКонецФункции"))
            .unwrap_err()
            .reason,
        AdmissionReason::OwnerContext
    );
}

#[test]
fn enforces_caller_and_fixed_input_limits() {
    let mut request = request(include_str!("../fixtures/reference-source.bsl"));
    request.limits.max_code_bytes = 1;
    assert_eq!(admit(&request).unwrap_err().reason, AdmissionReason::SourceTooLarge);
    request.code = " ".repeat(MAX_MODULE_BYTES + 1);
    request.limits.max_code_bytes = (MAX_MODULE_BYTES + 10) as u64;
    assert_eq!(admit(&request).unwrap_err().reason, AdmissionReason::SourceTooLarge);
}

#[test]
fn a_local_from_another_method_does_not_provide_owner_context() {
    let source = "Функция Первая()\nЛокальная = 1;\nВозврат Локальная;\nКонецФункции\nФункция Вторая()\nВозврат Локальная;\nКонецФункции";
    assert_eq!(admit(&request(source)).unwrap_err().reason, AdmissionReason::OwnerContext);
}

#[test]
fn native_platform_constructors_need_a_known_server_type() {
    assert!(admit(&request("Функция Проверка()\nВозврат Новый Структура;\nКонецФункции")).is_ok());
    assert_eq!(
        admit(&request("Функция Проверка()\nВозврат Новый НеизвестныйТипВладельца;\nКонецФункции"))
            .unwrap_err()
            .reason,
        AdmissionReason::OwnerContext
    );
    assert_eq!(
        admit(&request("Функция Проверка(Тип)\nВозврат Новый(Тип);\nКонецФункции"))
            .unwrap_err()
            .reason,
        AdmissionReason::UnsupportedDeclaration
    );
}

#[test]
fn keywords_inside_literals_and_comments_remain_plain_method_text() {
    let source =
        "// #If Client Then\nProcedure Check()\nText = \"Execute #If Client Then\";\nEndProcedure";
    let request = request(source);
    let admitted = admit(&request).unwrap();
    assert_eq!(&container::module_text(&admitted.pack().unwrap())[3..], source.as_bytes());
}

#[test]
fn a_bom_inside_the_module_is_an_unsupported_declaration_form() {
    let source = "Procedure Check()\n\u{feff}Return;\nEndProcedure";
    assert_eq!(
        admit(&request(source)).unwrap_err().reason,
        AdmissionReason::UnsupportedDeclaration
    );
}
