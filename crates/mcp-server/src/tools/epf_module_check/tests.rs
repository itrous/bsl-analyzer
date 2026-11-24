use super::*;

const SOURCE: &str = "Procedure Check()\n    Return;\nEndProcedure";

fn request(source: &str) -> CheckRequest {
    CheckRequest {
        schema_version: SCHEMA_VERSION.into(),
        request_id: "epf-unit".into(),
        connection: "test".into(),
        code: source.into(),
        input_kind: InputKind::Module,
        module_type: Some(ModuleType::Object),
        context: Some(ModuleContext::Synthetic { module_type: ModuleType::Object }),
        limits: WireLimits {
            timeout_ms: 1000,
            max_code_bytes: 1048576,
            max_response_bytes: 65536,
            max_stderr_bytes: 1024,
            stop_grace_ms: 100,
        },
    }
}

fn peer(created: bool) -> onec_client::EpfModuleCheckResult {
    onec_client::EpfModuleCheckResult {
        carrier_format: "bsl-analyzer-synthetic-object/1".into(),
        request_id: "epf-unit".into(),
        create_attempted: true,
        safe_mode_requested: true,
        created,
        native_error: (!created).then(|| "Native compilation failed".into()),
        failure_code: None,
        cleanup_complete: true,
        cleanup_error: None,
        duration_ms: 1,
        owned_module_position: (!created)
            .then_some(onec_client::EpfModulePosition { line: 2, column: 5 }),
    }
}

fn structured(result: CallToolResult) -> CheckResponse {
    serde_json::from_value(result.structured_content.unwrap()).unwrap()
}

#[test]
fn successful_native_create_reports_only_synthetic_server_object() {
    let request = request(SOURCE);
    let module = epf_carrier::admit(&request).unwrap();
    let response = from_peer(&request, &module, peer(true));
    response.validate_for(&request).unwrap();
    assert_eq!(response.status, Status::Valid);
    assert_eq!(response.valid, Some(true));
    assert_eq!(response.compilation_status, Some(CompilationStatus::Valid));
    assert_eq!(response.contexts_checked, vec![CompilerContext::Server]);
    assert_eq!(response.module_type, Some(ModuleType::Object));
    assert_eq!(response.context, request.context);
    assert!(response.compiler.is_none());
    assert_eq!(response.backend_info.unwrap()["scope"], "synthetic_server_object");
}

#[test]
fn native_invalid_requires_attributable_in_range_position_and_create_attempt() {
    let request = request(SOURCE);
    let module = epf_carrier::admit(&request).unwrap();
    let response = from_peer(&request, &module, peer(false));
    response.validate_for(&request).unwrap();
    assert_eq!(response.status, Status::Invalid);
    assert_eq!(response.issues[0].line, Some(2));
    assert_eq!(response.issues[0].column, Some(5));
    for position in [
        None,
        Some(onec_client::EpfModulePosition { line: 999, column: 1 }),
        Some(onec_client::EpfModulePosition { line: 1, column: 999 }),
    ] {
        let mut failed = peer(false);
        failed.owned_module_position = position;
        let response = from_peer(&request, &module, failed);
        response.validate_for(&request).unwrap();
        assert_eq!(response.status, Status::Error);
        assert_eq!(response.valid, None);
        assert_eq!(response.compilation_status, None);
        assert!(response.contexts_checked.is_empty());
        assert_eq!(response.failure.unwrap().code, "epf_native_context_unconfirmed");
    }
    let mut not_attempted = peer(false);
    not_attempted.create_attempted = false;
    not_attempted.safe_mode_requested = false;
    not_attempted.failure_code = Some("write_failed".into());
    let response = from_peer(&request, &module, not_attempted);
    response.validate_for(&request).unwrap();
    assert!(response.contexts_checked.is_empty());
    assert_eq!(response.context, None);
    assert_eq!(response.compilation_status, None);
}

#[test]
fn cleanup_failure_preserves_compilation_but_never_claims_success() {
    let request = request(SOURCE);
    let module = epf_carrier::admit(&request).unwrap();
    for created in [true, false] {
        let mut failed = peer(created);
        failed.cleanup_complete = false;
        failed.cleanup_error = Some("Owned cleanup failed".into());
        let response = from_peer(&request, &module, failed);
        response.validate_for(&request).unwrap();
        assert_eq!(response.status, Status::Error);
        assert_eq!(response.valid, None);
        assert_eq!(response.cleanup_status, Some(CleanupStatus::Incomplete));
        assert_eq!(
            response.compilation_status,
            Some(if created { CompilationStatus::Valid } else { CompilationStatus::Invalid })
        );
        assert_eq!(response.failure.unwrap().code, "epf_cleanup_failed");
    }
    let mut late = peer(true);
    late.failure_code = Some("deadline_exceeded".into());
    let response = from_peer(&request, &module, late);
    response.validate_for(&request).unwrap();
    assert_eq!(response.status, Status::Error);
    assert_eq!(response.compilation_status, Some(CompilationStatus::Valid));
    assert_eq!(response.cleanup_status, Some(CleanupStatus::Complete));
}

#[test]
fn maps_original_utf16_coordinates_without_newline_conversion() {
    let source = "\u{feff}Procedure Check()\r\n    X = \"😀\";\r\nEndProcedure";
    let request = request(source);
    let module = epf_carrier::admit(&request).unwrap();
    assert_eq!(
        original_position(&module, onec_client::EpfModulePosition { line: 1, column: 1 }),
        Some(epf_carrier::SourcePosition { line: 1, column: 2 })
    );
    assert_eq!(
        original_position(&module, onec_client::EpfModulePosition { line: 2, column: 10 }),
        Some(epf_carrier::SourcePosition { line: 2, column: 10 })
    );
    assert_eq!(
        original_position(&module, onec_client::EpfModulePosition { line: 2, column: 11 }),
        None
    );
    assert_eq!(
        original_position(&module, onec_client::EpfModulePosition { line: 2, column: 12 }),
        Some(epf_carrier::SourcePosition { line: 2, column: 12 })
    );
    assert_eq!(
        original_position(&module, onec_client::EpfModulePosition { line: 0, column: 1 }),
        None
    );
}

#[test]
fn bounded_output_keeps_valid_contract_and_utf8_failure() {
    let request = request(SOURCE);
    let response = remote_failure(&request, "epf_backend_error", &"Ошибка😀".repeat(5000));
    let result = format_response(response, &request, 400).unwrap();
    let text_bytes =
        result.content.iter().map(|block| block.as_text().unwrap().text.len()).sum::<usize>();
    let value = result.structured_content.as_ref().unwrap();
    assert!(serde_json::to_vec(value).unwrap().len() + text_bytes <= 1600);
    let response = structured(result);
    response.validate_for(&request).unwrap();
    assert!(response.truncated);
    assert_eq!(response.cleanup_status, None);
    assert_eq!(response.backend_info.unwrap()["cleanup_unknown"], true);
    assert!(ensure_budget(&request, 1).is_err());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(complete(base_response(&request), &request, 6000, &cancelled).is_err());
}

fn state(
    url: &str,
    enabled: bool,
    profile: Option<crate::state::CheckExecutorProfile>,
) -> crate::SharedState {
    let mut state = crate::SharedState::shared();
    state.add_onec_connection(
        "test".into(),
        crate::state::OnecConnection::with_optional_http(
            Some(onec_client::Client::new(url, "", "")),
            false,
            profile,
        )
        .with_epf_module_check(enabled),
    );
    state
}

async fn route(
    state: &crate::SharedState,
    source: &str,
    kind: InputKind,
    context: Option<ModuleContext>,
    cancel: CancellationToken,
    budget: usize,
) -> Result<CallToolResult, McpError> {
    crate::tools::execution::check_syntax(
        state,
        source,
        Some("test"),
        budget,
        kind,
        (kind == InputKind::Module).then_some(ModuleType::Object),
        context,
        cancel,
    )
    .await
}

fn listening() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (listener, url)
}

#[tokio::test]
async fn opt_in_and_ast_admission_precede_any_http_request() {
    let (listener, url) = listening();
    let context = Some(ModuleContext::Synthetic { module_type: ModuleType::Object });
    let disabled = state(&url, false, None);
    assert_eq!(
        structured(
            route(
                &disabled,
                SOURCE,
                InputKind::Module,
                context.clone(),
                CancellationToken::new(),
                6000
            )
            .await
            .unwrap()
        )
        .status,
        Status::Unsupported
    );
    let enabled = state(&url, true, None);
    for source in [
        "X = 1;",
        "Procedure Broken(",
        "&AtClient\nProcedure Check()\nEndProcedure",
        "Procedure Check()\nX = ThisObject.Code;\nEndProcedure",
        "#If Client Then\nX = 1;\n#EndIf",
    ] {
        let response = structured(
            route(
                &enabled,
                source,
                InputKind::Module,
                context.clone(),
                CancellationToken::new(),
                6000,
            )
            .await
            .unwrap(),
        );
        assert!(matches!(response.status, Status::Unsupported | Status::ContextRequired));
        assert_eq!(response.valid, None);
        assert_eq!(response.cleanup_status, Some(CleanupStatus::Complete));
        assert!(response.contexts_checked.is_empty());
    }
    assert_eq!(
        structured(
            route(&enabled, SOURCE, InputKind::Module, None, CancellationToken::new(), 6000)
                .await
                .unwrap()
        )
        .status,
        Status::ContextRequired
    );
    assert!(route(
        &enabled,
        SOURCE,
        InputKind::Module,
        context.clone(),
        CancellationToken::new(),
        1
    )
    .await
    .is_err());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(route(&enabled, SOURCE, InputKind::Module, context, cancel, 6000).await.is_err());
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[tokio::test]
async fn selected_executor_failure_never_falls_back_to_epf() {
    let (listener, url) = listening();
    let temp = tempfile::tempdir().unwrap();
    let profile = crate::state::CheckExecutorProfile {
        program: temp.path().join("nonexistent-executor"),
        args: Vec::new(),
        timeout: Duration::from_secs(1),
        max_code_bytes: 1048576,
        max_response_bytes: 65536,
        max_stderr_bytes: 1024,
        stop_grace: Duration::from_millis(10),
        reap_timeout: Duration::from_millis(100),
        max_concurrent: 1,
        handles_snippet: true,
        semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
    };
    let state = state(&url, true, Some(profile));
    for (code, kind, context) in [
        (
            SOURCE,
            InputKind::Module,
            Some(ModuleContext::Synthetic { module_type: ModuleType::Object }),
        ),
        ("X = 1;", InputKind::Snippet, None),
    ] {
        let response = structured(
            route(&state, code, kind, context, CancellationToken::new(), 6000).await.unwrap(),
        );
        assert_eq!(response.status, Status::Error);
        assert_ne!(response.backend, BACKEND);
    }
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

async fn http_peer(response: serde_json::Value) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let boundary = loop {
            let mut part = [0; 4096];
            let count = stream.read(&mut part).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&part[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let header = String::from_utf8(bytes[..boundary].to_vec()).unwrap();
        let length: usize = header
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(|value| value.parse().unwrap())
            })
            .unwrap();
        while bytes.len() - boundary < length {
            let mut part = [0; 4096];
            let count = stream.read(&mut part).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&part[..count]);
        }
        let frame: serde_json::Value = serde_json::from_slice(&bytes[boundary..]).unwrap();
        let mut response = response;
        if header.starts_with("POST /check-module ") {
            response["request_id"] = frame["request_id"].clone();
        }
        let body = response.to_string();
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        header.lines().next().unwrap().to_owned()
    });
    (url, handle)
}

#[tokio::test]
async fn opt_in_uses_epf_for_modules_and_preserves_legacy_snippets() {
    for (kind, created) in
        [(InputKind::Module, true), (InputKind::Module, false), (InputKind::Snippet, true)]
    {
        let ack = if kind == InputKind::Module {
            serde_json::json!({"carrier_format":"bsl-analyzer-synthetic-object/1","request_id":"echo","create_attempted":true,"safe_mode_requested":true,"created":created,"native_error":if created { None } else { Some("{ExternalDataProcessor.BSLProbe_valid.ObjectModule(2,5)}: compilation failed") },"failure_code":null,"cleanup_complete":true,"cleanup_error":null,"duration_ms":1})
        } else {
            serde_json::json!({"valid":true,"error":null})
        };
        let (url, server) = http_peer(ack).await;
        let state = state(&url, true, None);
        let context = (kind == InputKind::Module)
            .then_some(ModuleContext::Synthetic { module_type: ModuleType::Object });
        let source = match (kind, created) {
            (InputKind::Module, false) => "Procedure Check()\n    Return 1;\nEndProcedure",
            (InputKind::Module, true) => SOURCE,
            (InputKind::Snippet, _) => "X = 1;",
        };
        let response = structured(
            route(&state, source, kind, context, CancellationToken::new(), 6000).await.unwrap(),
        );
        assert_eq!(response.status, if created { Status::Valid } else { Status::Invalid });
        assert_eq!(response.backend == BACKEND, kind == InputKind::Module);
        assert_eq!(
            server.await.unwrap(),
            if kind == InputKind::Module {
                "POST /check-module HTTP/1.1"
            } else {
                "POST /check-syntax HTTP/1.1"
            }
        );
    }
}

#[test]
fn producer_uses_safe_create_and_releases_only_its_own_carrier() {
    use syntax::{SyntaxKind, SyntaxNode};
    let parsed = parser::parse(include_str!(
        "../../../../../extension/src/HTTPServices/BSLAnalyzerService/Ext/Module.bsl"
    ));
    assert!(parsed.errors().is_empty(), "{:?}", parsed.errors());
    let root = parsed.syntax_node();
    let handler = root
        .children()
        .find(|node| {
            node.kind() == SyntaxKind::FUNCTION_DEF
                && node
                    .children_with_tokens()
                    .filter_map(|item| item.into_token())
                    .any(|token| token.text() == "ПроверитьМодульPOST")
        })
        .expect("HTTP handler exists");
    fn callee(call: &SyntaxNode) -> String {
        call.children().next().unwrap().text().to_string().trim().to_owned()
    }
    fn arguments(call: &SyntaxNode) -> Vec<String> {
        call.children()
            .find(|node| node.kind() == SyntaxKind::ARG_LIST)
            .unwrap()
            .children()
            .map(|node| node.text().to_string().trim().to_owned())
            .collect()
    }
    let calls: Vec<_> =
        handler.descendants().filter(|node| node.kind() == SyntaxKind::CALL_EXPR).collect();
    let create: Vec<_> =
        calls.iter().filter(|call| callee(call) == "ВнешниеОбработки.Создать").collect();
    assert_eq!(create.len(), 1);
    assert_eq!(arguments(create[0]), ["ИмяНосителя82747", "Истина"]);
    let delete: Vec<_> = calls.iter().filter(|call| callee(call) == "УдалитьФайлы").collect();
    assert_eq!(delete.len(), 1);
    assert_eq!(arguments(delete[0]), ["ИмяНосителя82747"]);
    let temp = calls.iter().find(|call| callee(call) == "ПолучитьИмяВременногоФайла").unwrap();
    assert_eq!(arguments(temp), ["\"epf\""]);
    let write = calls.iter().find(|call| callee(call) == "ДанныеНосителя82747.Записать").unwrap();
    assert_eq!(arguments(write), ["ИмяНосителя82747"]);
    assert!(temp.text_range().start() < write.text_range().start());
    assert!(write.text_range().start() < create[0].text_range().start());
    let release = handler
        .descendants()
        .filter(|node| node.kind() == SyntaxKind::ASSIGN_STMT)
        .find(|node| {
            node.text_range().start() > create[0].text_range().end()
                && node
                    .children()
                    .map(|child| child.text().to_string().trim().to_owned())
                    .collect::<Vec<_>>()
                    == ["ОбъектНосителя82747", "Неопределено"]
        })
        .expect("Object is released before cleanup");
    assert!(release.text_range().end() < delete[0].text_range().start());
    assert!(calls.iter().all(|call| !callee(call).starts_with("ОбъектНосителя82747.")));
    assert!(handler
        .descendants_with_tokens()
        .filter_map(|item| item.into_token())
        .all(|token| !matches!(token.kind(), SyntaxKind::KW_EXECUTE)));
}
