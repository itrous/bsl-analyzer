use super::*;
use check_contract::{
    CheckRequest, InputKind, ModuleContext, ModuleType, WireLimits, SCHEMA_VERSION,
};
use std::io::{Read, Write};

fn module_request() -> CheckRequest {
    CheckRequest {
        schema_version: SCHEMA_VERSION.into(),
        request_id: "epf-test".into(),
        connection: "test".into(),
        code: "Procedure Check()\nReturn;\nEndProcedure".into(),
        input_kind: InputKind::Module,
        module_type: Some(ModuleType::Object),
        context: Some(ModuleContext::Synthetic { module_type: ModuleType::Object }),
        limits: WireLimits {
            timeout_ms: 1000,
            max_code_bytes: 1024,
            max_response_bytes: 65536,
            max_stderr_bytes: 1024,
            stop_grace_ms: 100,
        },
    }
}

fn acknowledgement() -> serde_json::Value {
    serde_json::json!({"carrier_format":CARRIER_FORMAT,"request_id":"epf-test","create_attempted":true,"safe_mode_requested":true,"created":true,"native_error":null,"failure_code":null,"cleanup_complete":true,"cleanup_error":null,"duration_ms":1})
}

fn serve(
    status: &str,
    body: String,
    delay: Duration,
) -> (String, std::thread::JoinHandle<(Vec<u8>, usize)>) {
    serve_response(status, body, delay, false)
}

fn serve_response(
    status: &str,
    body: String,
    delay: Duration,
    chunked: bool,
) -> (String, std::thread::JoinHandle<(Vec<u8>, usize)>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let status = status.to_owned();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut bytes = Vec::new();
        let boundary = loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let header = String::from_utf8(bytes[..boundary].to_vec()).unwrap();
        assert!(header.starts_with("POST /check-module HTTP/1.1\r\n"));
        let length: usize = header
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(|value| value.parse().unwrap())
            })
            .unwrap();
        while bytes.len() - boundary < length {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        std::thread::sleep(delay);
        let response = if chunked {
            format!("HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{body}\r\n0\r\n\r\n", body.len())
        } else {
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        let _ = stream.write_all(response.as_bytes());
        listener.set_nonblocking(true).unwrap();
        let mut count = 1;
        let until = Instant::now() + Duration::from_millis(50);
        while Instant::now() < until {
            match listener.accept() {
                Ok(_) => count += 1,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("test server failed: {error}"),
            }
        }
        (bytes[boundary..].to_vec(), count)
    });
    (url, handle)
}

#[tokio::test]
async fn packages_only_admitted_source_and_forwards_remaining_budget() {
    let request = module_request();
    let module = epf_carrier::admit(&request).unwrap();
    let expected = module.pack().unwrap();
    let (url, server) = serve("200 OK", acknowledgement().to_string(), Duration::ZERO);
    let peer = Client::new(&url, "", "")
        .check_epf_module(
            "epf-test",
            &module,
            Instant::now() + Duration::from_millis(900),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(peer.created && peer.cleanup_complete);
    let (body, count) = server.join().unwrap();
    let frame: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(frame["carrier_format"], CARRIER_FORMAT);
    assert_eq!(frame["request_id"], "epf-test");
    assert!(frame["timeout_ms"].as_u64().is_some_and(|timeout| timeout > 0 && timeout <= 900));
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(frame["epf_base64"].as_str().unwrap())
            .unwrap(),
        expected
    );
    assert_eq!(count, 1);
}

#[tokio::test]
async fn cancellation_deadline_and_bad_id_stop_before_http() {
    let request = module_request();
    let module = epf_carrier::admit(&request).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let client = Client::new(&format!("http://{}", listener.local_addr().unwrap()), "", "");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    for (id, deadline, cancel, code) in [
        ("epf-test", Instant::now() + Duration::from_secs(1), cancelled, "epf_cancelled"),
        ("epf-test", Instant::now(), CancellationToken::new(), "epf_deadline_exceeded"),
        (
            "",
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
            "invalid_epf_request_id",
        ),
    ] {
        let error = client.check_epf_module(id, &module, deadline, &cancel).await.unwrap_err();
        assert!(
            matches!(error, Error::ModuleCheckBeforeRequest { code: actual } if actual == code)
        );
    }
    assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[tokio::test]
async fn finite_http_budget_has_one_attempt() {
    let request = module_request();
    let module = epf_carrier::admit(&request).unwrap();
    let (url, server) = serve("200 OK", acknowledgement().to_string(), Duration::from_millis(150));
    let error = Client::new(&url, "", "")
        .check_epf_module(
            "epf-test",
            &module,
            Instant::now() + Duration::from_millis(70),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Request(ref inner) if inner.is_timeout()));
    assert_eq!(server.join().unwrap().1, 1);
}

#[tokio::test]
async fn cancellation_during_http_reports_unconfirmed_remote_cleanup() {
    let request = module_request();
    let module = epf_carrier::admit(&request).unwrap();
    let (url, server) = serve("200 OK", acknowledgement().to_string(), Duration::from_millis(150));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(70)).await;
        trigger.cancel();
    });
    let error = Client::new(&url, "", "")
        .check_epf_module("epf-test", &module, Instant::now() + Duration::from_secs(1), &cancel)
        .await
        .unwrap_err();
    assert!(matches!(error, Error::ModuleCheckProtocol { code: "epf_cancelled_after_request" }));
    task.await.unwrap();
    assert_eq!(server.join().unwrap().1, 1);
}

#[test]
fn validates_acknowledgement_truth_and_never_trusts_wire_positions() {
    let valid = acknowledgement();
    let mut with_position = valid.clone();
    with_position["owned_module_position"] = serde_json::json!({"line":1,"column":1});
    assert!(serde_json::from_value::<EpfModuleCheckResult>(with_position).is_err());
    for (key, value) in [
        ("carrier_format", "other".into()),
        ("request_id", "other".into()),
        ("safe_mode_requested", false.into()),
        ("create_attempted", false.into()),
        ("native_error", "unexpected".into()),
        ("cleanup_complete", false.into()),
        ("cleanup_error", "unexpected".into()),
        ("failure_code", "HTTP://private".into()),
    ] {
        let mut invalid = valid.clone();
        invalid[key] = value;
        assert!(
            serde_json::from_value::<EpfModuleCheckResult>(invalid)
                .unwrap()
                .validate("epf-test")
                .is_err(),
            "{key}"
        );
    }
    let mut failed = valid;
    failed["created"] = false.into();
    assert!(serde_json::from_value::<EpfModuleCheckResult>(failed)
        .unwrap()
        .validate("epf-test")
        .is_err());
}

#[test]
fn attributes_only_own_object_module_and_positive_native_coordinates() {
    for frame in [
        "{ВнешняяОбработка.BSLProbe_valid.МодульОбъекта(3, 7)}: ошибка",
        "{ExternalDataProcessor.BSLProbe_valid.ObjectModule(3,7)}: error",
    ] {
        assert_eq!(owned_module_position(frame), Some(EpfModulePosition { line: 3, column: 7 }));
    }
    for frame in [
        "{ВнешняяОбработка.Other.МодульОбъекта(3,7)}",
        "{ВнешняяОбработка.BSLProbe_valid.МодульОбъекта(0,7)}",
        "{ExternalDataProcessor.BSLProbe_valid.ObjectModule(3,-1)}",
        "{ExternalDataProcessor.BSLProbe_valid.FormModule(3,7)}",
    ] {
        assert_eq!(owned_module_position(frame), None);
    }
}

#[tokio::test]
async fn decodes_bom_and_attributes_before_redacting_credentials() {
    let request = module_request();
    let module = epf_carrier::admit(&request).unwrap();
    let mut ack = acknowledgement();
    ack["created"] = false.into();
    ack["native_error"] =
        "{ExternalDataProcessor.BSLProbe_valid.ObjectModule(2,7)}: HTTP://private.invalid secret"
            .into();
    let (url, server) = serve("200 OK", format!("\u{feff}{ack}"), Duration::ZERO);
    let peer = Client::new(&url, "BSLProbe_valid", "secret")
        .check_epf_module(
            "epf-test",
            &module,
            Instant::now() + Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(peer.owned_module_position, Some(EpfModulePosition { line: 2, column: 7 }));
    let error = peer.native_error.unwrap();
    assert!(
        !error.contains("secret")
            && !error.contains("BSLProbe_valid")
            && !error.contains("private.invalid")
    );
    assert_eq!(server.join().unwrap().1, 1);
}

#[tokio::test]
async fn bounds_response_and_returns_safe_json_and_status_errors() {
    let request = module_request();
    let module = epf_carrier::admit(&request).unwrap();
    for (status, body, expected) in [
        ("200 OK", "x".repeat(EPF_MODULE_RESPONSE_BYTES + 1), "epf_response_too_large"),
        ("200 OK", "{invalid".into(), "invalid_epf_json"),
        ("200 OK", "{}".into(), "invalid_epf_json"),
    ] {
        let (url, server) = serve(status, body, Duration::ZERO);
        let error = Client::new(&url, "", "")
            .check_epf_module(
                "epf-test",
                &module,
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, Error::ModuleCheckProtocol { code } if code == expected));
        assert_eq!(server.join().unwrap().1, 1);
    }
    let (url, server) =
        serve_response("200 OK", "я".repeat(EPF_MODULE_RESPONSE_BYTES), Duration::ZERO, true);
    let error = Client::new(&url, "", "")
        .check_epf_module(
            "epf-test",
            &module,
            Instant::now() + Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::ModuleCheckProtocol { code: "epf_response_too_large" }));
    assert_eq!(server.join().unwrap().1, 1);
    let (url, server) = serve("404 Not Found", String::new(), Duration::ZERO);
    let error = Client::new(&url, "", "")
        .with_connection_name("selected")
        .check_epf_module(
            "epf-test",
            &module,
            Instant::now() + Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::EndpointUnavailable { connection } if connection == "selected"));
    assert_eq!(server.join().unwrap().1, 1);
}

#[tokio::test]
async fn does_not_follow_redirect_to_another_service() {
    let request = module_request();
    let module = epf_carrier::admit(&request).unwrap();
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let (url, server) = serve(
        &format!(
            "307 Temporary Redirect\r\nLocation: http://{}/other",
            target.local_addr().unwrap()
        ),
        String::new(),
        Duration::ZERO,
    );
    let error = Client::new(&url, "", "")
        .check_epf_module(
            "epf-test",
            &module,
            Instant::now() + Duration::from_secs(1),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Status { status: 307, .. }));
    assert_eq!(server.join().unwrap().1, 1);
    assert_eq!(target.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}
