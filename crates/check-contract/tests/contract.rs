use check_contract::{
    parse_request, request_schema, response_schema, CheckResponse, ContractError, ResponseError,
};
use jsonschema::validator_for;
use serde_json::{json, Value};

const VALID_REQUEST: &str = include_str!("../fixtures/request.json");
const RESPONSE_FIXTURES: &[(&str, &str)] = &[
    ("valid", include_str!("../fixtures/response-valid.json")),
    ("invalid", include_str!("../fixtures/response-invalid.json")),
    ("context_required", include_str!("../fixtures/response-context-required.json")),
    ("unsupported", include_str!("../fixtures/response-unsupported.json")),
    ("error", include_str!("../fixtures/response-error.json")),
];

#[test]
fn fixtures_parse_and_agree_with_the_request() {
    let request = parse_request(VALID_REQUEST.as_bytes(), 1024).unwrap();
    for (name, fixture) in RESPONSE_FIXTURES {
        let response: CheckResponse = serde_json::from_str(fixture).unwrap();
        let status = match response.status {
            check_contract::Status::Valid => "valid",
            check_contract::Status::Invalid => "invalid",
            check_contract::Status::ContextRequired => "context_required",
            check_contract::Status::Unsupported => "unsupported",
            check_contract::Status::Error => "error",
        };
        assert_eq!(status, *name);
        response.validate_for(&request).unwrap();
    }
}

#[test]
fn schemas_match_the_types_and_require_nullable_fields() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/mcp/check-contract/v1");
    let request = request_schema();
    let response = response_schema();
    assert!(request == read_schema(&root.join("request.schema.json")), "request schema drift");
    assert!(response == read_schema(&root.join("response.schema.json")), "response schema drift");
    for key in [
        "valid",
        "failure",
        "compilation_status",
        "compiler",
        "cleanup_status",
        "module_type",
        "context",
        "backend_info",
    ] {
        assert!(response["required"].as_array().unwrap().iter().any(|v| v == key));
    }
    assert!(!request["required"].as_array().unwrap().iter().any(|v| v == "input_kind"));
    assert_eq!(response["properties"]["backend_info"]["additionalProperties"], true);
    assert_eq!(response["$defs"]["CompilerInfo"]["required"].as_array().unwrap().len(), 3);

    let request_validator = validator_for(&request).unwrap();
    let response_validator = validator_for(&response).unwrap();
    assert!(request_validator.is_valid(&serde_json::from_str::<Value>(VALID_REQUEST).unwrap()));
    for (_, fixture) in RESPONSE_FIXTURES {
        assert!(response_validator.is_valid(&serde_json::from_str::<Value>(fixture).unwrap()));
    }
    let mut unknown = serde_json::from_str::<Value>(VALID_REQUEST).unwrap();
    unknown["limits"]["extra"] = json!(true);
    assert!(!request_validator.is_valid(&unknown));
    for key in [
        "valid",
        "failure",
        "compilation_status",
        "compiler",
        "cleanup_status",
        "module_type",
        "context",
        "backend_info",
    ] {
        let mut missing = serde_json::from_str::<Value>(RESPONSE_FIXTURES[0].1).unwrap();
        missing.as_object_mut().unwrap().remove(key);
        assert!(!response_validator.is_valid(&missing), "missing {key}");
    }
    for key in ["build", "compatibility", "fingerprint"] {
        let mut missing = serde_json::from_str::<Value>(RESPONSE_FIXTURES[0].1).unwrap();
        missing["compiler"].as_object_mut().unwrap().remove(key);
        assert!(!response_validator.is_valid(&missing), "missing compiler.{key}");
    }
    let mut nested_unknown = serde_json::from_str::<Value>(RESPONSE_FIXTURES[0].1).unwrap();
    nested_unknown["compiler"]["extra"] = json!(null);
    assert!(!response_validator.is_valid(&nested_unknown));
    let mut context_unknown = serde_json::from_str::<Value>(RESPONSE_FIXTURES[0].1).unwrap();
    context_unknown["context"] = json!({
        "kind":"metadata", "owner":"CommonModule.Fixture",
        "origin":{"kind":"configuration", "name":"not-allowed"}
    });
    assert!(!response_validator.is_valid(&context_unknown));
}

#[test]
fn request_parser_enforces_frames_duplicates_and_semantics() {
    let mut request: Value = serde_json::from_str(VALID_REQUEST).unwrap();
    assert_eq!(
        parse_request(VALID_REQUEST.as_bytes(), VALID_REQUEST.len() - 1),
        Err(ContractError::FrameTooLarge)
    );
    assert_eq!(parse_request(&[0xff], 10), Err(ContractError::MalformedJson));
    assert_eq!(
        parse_request(br#"{"schema_version":"1","schema_version":"1"}"#, 128),
        Err(ContractError::MalformedJson)
    );
    let trailing = format!("{VALID_REQUEST} {{}}");
    assert_eq!(parse_request(trailing.as_bytes(), 2048), Err(ContractError::MalformedJson));
    request["request_id"] = json!("é");
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::InvalidRequestId)
    );
    request["request_id"] = json!("x".repeat(65));
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::InvalidRequestId)
    );
    request["request_id"] = json!("ok");
    request["limits"]["timeout_ms"] = json!(0);
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::InvalidLimits)
    );
    request["limits"]["timeout_ms"] = json!(1);
    request["limits"]["max_code_bytes"] = json!(u64::MAX);
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::FrameOverflow)
    );
    request["limits"]["max_code_bytes"] = json!(1);
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::CodeTooLarge)
    );
    request["limits"]["max_code_bytes"] = json!(2097152);
    request["unexpected"] = json!(true);
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::MalformedJson)
    );
}

#[test]
fn response_validation_separates_identity_and_rejects_bad_envelopes() {
    let request = parse_request(VALID_REQUEST.as_bytes(), 1024).unwrap();
    let mut response: CheckResponse = serde_json::from_str(RESPONSE_FIXTURES[0].1).unwrap();
    response.request_id = "other".into();
    assert_eq!(response.validate_for(&request), Err(ResponseError::RequestIdMismatch));
    response.request_id = request.request_id.clone();
    response.valid = Some(false);
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));
    response.valid = Some(true);
    response.contexts_checked.clear();
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));
}

#[test]
fn missing_nullable_unknown_and_duplicate_contract_fields_fail_deserialization() {
    let base: Value = serde_json::from_str(RESPONSE_FIXTURES[0].1).unwrap();
    for key in ["compiler", "context", "backend_info"] {
        let mut missing = base.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(serde_json::from_value::<CheckResponse>(missing).is_err(), "{key}");
    }
    let mut nested_missing = base.clone();
    nested_missing["compiler"].as_object_mut().unwrap().remove("build");
    assert!(serde_json::from_value::<CheckResponse>(nested_missing).is_err());
    let mut nested_unknown = base.clone();
    nested_unknown["compiler"]["extra"] = json!(null);
    assert!(serde_json::from_value::<CheckResponse>(nested_unknown).is_err());
    let mut unknown = base;
    unknown["unexpected"] = json!(true);
    assert!(serde_json::from_value::<CheckResponse>(unknown).is_err());
    assert!(serde_json::from_str::<CheckResponse>(
        r#"{"schema_version":"1","schema_version":"1"}"#
    )
    .is_err());
}

#[test]
fn response_checks_coordinates_codes_context_coverage_and_echo() {
    let request = parse_request(VALID_REQUEST.as_bytes(), 1024).unwrap();
    let mut response: CheckResponse = serde_json::from_str(RESPONSE_FIXTURES[1].1).unwrap();
    response.issues[0].line = Some(0);
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));
    response.issues[0].line = Some(1);
    response.issues[0].column = None;
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));
    response.issues[0].column = Some(1);
    response.failure = Some(check_contract::Failure {
        code: "transport_timeout".into(),
        message: "caller-owned".into(),
    });
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));
    response.failure = None;
    response = serde_json::from_str(RESPONSE_FIXTURES[0].1).unwrap();
    response.contexts_checked.clear();
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));
    response.contexts_checked.push(check_contract::CompilerContext::Server);
    response.connection = "other".into();
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));
    response.connection = request.connection.clone();
    for backend in ["/0.1.0", "native/"] {
        response.backend = backend.into();
        assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed), "{backend}");
    }
    response.backend = "native/custom-build".into();
    response.validate_for(&request).unwrap();
}

#[test]
fn module_result_must_repeat_the_requested_owner_and_origin() {
    let mut value: Value = serde_json::from_str(VALID_REQUEST).unwrap();
    value["input_kind"] = json!("module");
    value["module_type"] = json!("common");
    value["context"] = json!({
        "kind": "metadata",
        "owner": "CommonModule.Fixture",
        "origin": {"kind": "extension", "name": "FixtureExtension"}
    });
    let request = parse_request(value.to_string().as_bytes(), 2048).unwrap();
    let mut response: CheckResponse = serde_json::from_str(RESPONSE_FIXTURES[0].1).unwrap();
    response.module_type = Some(check_contract::ModuleType::Common);
    response.context = Some(request.context.clone().unwrap());
    response.validate_for(&request).unwrap();
    response.context = Some(
        serde_json::from_value(json!({
            "kind": "metadata",
            "owner": "CommonModule.Other",
            "origin": {"kind": "extension", "name": "FixtureExtension"}
        }))
        .unwrap(),
    );
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));

    let mut value: Value = serde_json::from_str(VALID_REQUEST).unwrap();
    value["input_kind"] = json!("module");
    value["module_type"] = json!("common");
    let request = parse_request(value.to_string().as_bytes(), 2048).unwrap();
    let response: CheckResponse = serde_json::from_str(RESPONSE_FIXTURES[0].1).unwrap();
    assert_eq!(response.validate_for(&request), Err(ResponseError::Malformed));
}

#[test]
fn request_schema_version_context_and_owner_bounds_are_checked() {
    let mut request: Value = serde_json::from_str(VALID_REQUEST).unwrap();
    request["schema_version"] = json!("2");
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::UnknownSchemaVersion)
    );
    request["schema_version"] = json!("1");
    request["input_kind"] = json!("module");
    request["context"] = json!({
        "kind": "metadata",
        "owner": "  ",
        "origin": {"kind": "extension", "name": "Fixture"}
    });
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::InvalidName)
    );
    request["context"]["owner"] = json!("CommonModule.Fixture");
    request["context"]["origin"]["name"] = json!("x".repeat(129));
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::InvalidName)
    );
    request["context"]["origin"]["name"] = json!("Fixture");
    request["context"]["origin"]["unexpected"] = json!(true);
    assert_eq!(
        parse_request(request.to_string().as_bytes(), 2048),
        Err(ContractError::MalformedJson)
    );
}

#[test]
fn table_fixture_suite_rejects_required_protocol_cases() {
    let valid_request: Value = serde_json::from_str(VALID_REQUEST).unwrap();
    let valid_response: Value = serde_json::from_str(RESPONSE_FIXTURES[0].1).unwrap();
    let request = parse_request(VALID_REQUEST.as_bytes(), 1024).unwrap();
    let mut request_cases: Vec<(&str, Value, ContractError)> = Vec::new();

    let mut missing_limits = valid_request.clone();
    missing_limits.as_object_mut().unwrap().remove("limits");
    request_cases.push(("missing limits", missing_limits, ContractError::MalformedJson));

    let mut byte_limited = valid_request.clone();
    byte_limited["code"] = json!("é");
    byte_limited["limits"]["max_code_bytes"] = json!(1);
    request_cases.push((
        "UTF-8 bytes exceed code limit",
        byte_limited,
        ContractError::CodeTooLarge,
    ));

    for (label, value, expected) in request_cases {
        let bytes = value.to_string();
        assert_eq!(parse_request(bytes.as_bytes(), 4096), Err(expected), "{label}");
    }

    let mut missing_response_fields = Vec::new();
    for key in [
        "valid",
        "failure",
        "compilation_status",
        "compiler",
        "cleanup_status",
        "module_type",
        "context",
        "backend_info",
    ] {
        let mut value = valid_response.clone();
        value.as_object_mut().unwrap().remove(key);
        missing_response_fields.push((format!("missing {key}"), value));
    }
    for key in ["build", "compatibility", "fingerprint"] {
        let mut value = valid_response.clone();
        value["compiler"].as_object_mut().unwrap().remove(key);
        missing_response_fields.push((format!("missing compiler.{key}"), value));
    }
    for key in ["line", "column"] {
        let mut value: Value = serde_json::from_str(RESPONSE_FIXTURES[1].1).unwrap();
        value["issues"][0].as_object_mut().unwrap().remove(key);
        missing_response_fields.push((format!("missing issue.{key}"), value));
    }
    for (label, value) in missing_response_fields {
        assert!(serde_json::from_value::<CheckResponse>(value).is_err(), "{label}");
    }

    let mut top_owner = valid_response.clone();
    top_owner["owner"] = json!("CommonModule.Forged");
    assert!(serde_json::from_value::<CheckResponse>(top_owner).is_err(), "top-level owner");

    let mut unknown_context = valid_response.clone();
    unknown_context["contexts_checked"][0] = json!("compiler_context_future");
    assert!(
        serde_json::from_value::<CheckResponse>(unknown_context).is_err(),
        "unknown compiler context"
    );

    for bad in [json!(-1), json!(0)] {
        let mut value: Value = serde_json::from_str(RESPONSE_FIXTURES[1].1).unwrap();
        let negative = bad == json!(-1);
        value["issues"][0]["line"] = bad;
        if negative {
            assert!(serde_json::from_value::<CheckResponse>(value).is_err(), "negative issue line");
        } else {
            let response: CheckResponse = serde_json::from_value(value).unwrap();
            assert_eq!(
                response.validate_for(&request),
                Err(ResponseError::Malformed),
                "zero issue line"
            );
        }
    }
    let mut null_position: Value = serde_json::from_str(RESPONSE_FIXTURES[1].1).unwrap();
    null_position["issues"][0]["line"] = Value::Null;
    assert_eq!(
        serde_json::from_value::<CheckResponse>(null_position).unwrap().validate_for(&request),
        Err(ResponseError::Malformed),
        "mixed null coordinates"
    );
    let mut mixed_position: CheckResponse = serde_json::from_str(RESPONSE_FIXTURES[1].1).unwrap();
    mixed_position.issues[0].column = None;
    assert_eq!(mixed_position.validate_for(&request), Err(ResponseError::Malformed));

    let mut status_mismatch: CheckResponse = serde_json::from_str(RESPONSE_FIXTURES[0].1).unwrap();
    status_mismatch.valid = Some(false);
    assert_eq!(status_mismatch.validate_for(&request), Err(ResponseError::Malformed));

    for code in [
        "executor_not_found",
        "spawn_failed",
        "transport_timeout",
        "transport_output_limit",
        "transport_malformed_response",
        "transport_request_id_mismatch",
        "transport_exit_status",
        "cancelled",
    ] {
        let mut value: CheckResponse = serde_json::from_str(RESPONSE_FIXTURES[2].1).unwrap();
        value.failure =
            Some(check_contract::Failure { code: code.into(), message: "reserved".into() });
        assert_eq!(value.validate_for(&request), Err(ResponseError::Malformed), "{code}");
    }

    let mut truncated_invalid: CheckResponse =
        serde_json::from_str(RESPONSE_FIXTURES[1].1).unwrap();
    truncated_invalid.truncated = true;
    truncated_invalid.validate_for(&request).unwrap();

    let mut module_request = valid_request;
    module_request["input_kind"] = json!("module");
    module_request["module_type"] = json!("common");
    module_request["context"] = json!({
        "kind": "metadata",
        "owner": "CommonModule.Fixture",
        "origin": {"kind": "extension", "name": "Fixture"}
    });
    let module_request_json = module_request.to_string();
    let module_request = parse_request(module_request_json.as_bytes(), 4096).unwrap();
    let mut module_response: CheckResponse = serde_json::from_str(RESPONSE_FIXTURES[0].1).unwrap();
    module_response.module_type = Some(check_contract::ModuleType::Common);
    module_response.context = module_request.context.clone();
    module_response.validate_for(&module_request).unwrap();
    module_response.module_type = Some(check_contract::ModuleType::Manager);
    assert_eq!(
        module_response.validate_for(&module_request),
        Err(ResponseError::Malformed),
        "mismatched type"
    );
    module_response.module_type = Some(check_contract::ModuleType::Common);
    module_response.context = Some(
        serde_json::from_value(json!({
            "kind": "metadata",
            "owner": "CommonModule.Fixture",
            "origin": {"kind": "configuration"}
        }))
        .unwrap(),
    );
    assert_eq!(
        module_response.validate_for(&module_request),
        Err(ResponseError::Malformed),
        "mismatched origin"
    );

    let config_origin_request = serde_json::json!({
        "schema_version":"1", "request_id":"check-001", "connection":"demo", "code":"Procedure X() EndProcedure",
        "limits":{"timeout_ms":120000,"max_code_bytes":2097152,"max_response_bytes":262144,"max_stderr_bytes":65536,"stop_grace_ms":2000},
        "input_kind":"module", "context":{"kind":"metadata","owner":"CommonModule.Fixture","origin":{"kind":"configuration","name":"unexpected"}}
    });
    assert!(
        serde_json::from_value::<check_contract::CheckRequest>(config_origin_request).is_err(),
        "unit origin rejects extra field"
    );
}

#[test]
fn duplicate_keys_fail_only_after_starting_from_complete_payloads() {
    let request_text =
        serde_json::to_string(&serde_json::from_str::<Value>(VALID_REQUEST).unwrap()).unwrap();
    assert!(serde_json::from_str::<check_contract::CheckRequest>(&request_text).is_ok());
    for (label, payload) in [
        ("request top-level", duplicate_key(&request_text, "\"schema_version\":\"1\"")),
        ("request limits", duplicate_key(&request_text, "\"timeout_ms\":120000")),
    ] {
        assert!(serde_json::from_str::<check_contract::CheckRequest>(&payload).is_err(), "{label}");
    }

    let mut module_request: Value = serde_json::from_str(VALID_REQUEST).unwrap();
    module_request["input_kind"] = json!("module");
    module_request["context"] = json!({
        "kind":"metadata", "owner":"CommonModule.Fixture", "origin":{"kind":"extension","name":"Fixture"}
    });
    let module_request = module_request.to_string();
    assert!(serde_json::from_str::<check_contract::CheckRequest>(&module_request).is_ok());
    for (label, payload) in [
        ("context owner", duplicate_key(&module_request, "\"owner\":\"CommonModule.Fixture\"")),
        ("origin kind", duplicate_key(&module_request, "\"kind\":\"extension\"")),
    ] {
        assert!(serde_json::from_str::<check_contract::CheckRequest>(&payload).is_err(), "{label}");
    }

    let response_text =
        serde_json::to_string(&serde_json::from_str::<Value>(RESPONSE_FIXTURES[0].1).unwrap())
            .unwrap();
    assert!(serde_json::from_str::<CheckResponse>(&response_text).is_ok());
    for (label, payload) in [
        ("response top-level", duplicate_key(&response_text, "\"status\":\"valid\"")),
        ("compiler", duplicate_key(&response_text, "\"build\":\"8.3.27.1989\"")),
    ] {
        assert!(serde_json::from_str::<CheckResponse>(&payload).is_err(), "{label}");
    }

    let mut module_response: Value = serde_json::from_str(&response_text).unwrap();
    module_response["module_type"] = json!("common");
    module_response["context"] = json!({
        "kind":"metadata", "owner":"CommonModule.Fixture", "origin":{"kind":"extension","name":"Fixture"}
    });
    let module_response = module_response.to_string();
    assert!(serde_json::from_str::<CheckResponse>(&module_response).is_ok());
    for (label, payload) in [
        ("response context", duplicate_key(&module_response, "\"owner\":\"CommonModule.Fixture\"")),
        ("response origin", duplicate_key(&module_response, "\"kind\":\"extension\"")),
    ] {
        assert!(serde_json::from_str::<CheckResponse>(&payload).is_err(), "{label}");
    }
}

fn duplicate_key(json: &str, field: &str) -> String {
    assert!(json.contains(field), "fixture field not found: {field}");
    json.replacen(field, &format!("{field},{field}"), 1)
}

fn read_schema(path: &std::path::Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn configuration_origin_rejects_any_name_field() {
    for name in [Value::Null, json!("ignored")] {
        let origin = json!({"kind": "configuration", "name": name});
        assert!(serde_json::from_value::<check_contract::Origin>(origin).is_err());
    }
}
