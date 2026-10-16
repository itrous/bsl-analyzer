#![cfg(unix)]

use std::{collections::BTreeSet, os::unix::fs::PermissionsExt, time::Duration};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_check_has_no_wire_response_after_cleanup() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let marker = temp.path().join("fake-executor.started");
    let response_marker = temp.path().join("fake-executor.responded");
    let program = temp.path().join("fake-executor.py");
    std::fs::write(
        &program,
        format!(
            "#!/usr/bin/env python3\nimport json,pathlib,signal,sys,time\nrequest=json.loads(sys.stdin.buffer.read())\npathlib.Path({:?}).touch()\ndef answer(*_):\n time.sleep(0.1)\n response={{'schema_version':'1','request_id':request['request_id'],'connection':request['connection'],'backend':'fake/1','status':'valid','valid':True,'issues':[],'contexts_required':[],'contexts_checked':['server'],'truncated':False,'failure':None,'compilation_status':'valid','compiler':None,'cleanup_status':'complete','module_type':None,'context':None,'backend_info':{{'padding':'x'*131072}}}}\n payload=json.dumps(response)\n print(payload,flush=True)\n pathlib.Path({:?}).write_text('written')\n sys.exit(0)\nsignal.signal(signal.SIGTERM,answer)\ntime.sleep(20)\n",
            marker.to_string_lossy(),
            response_marker.to_string_lossy()
        ),
    )?;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))?;

    let mut state = crate::SharedState::shared();
    let profile = crate::CheckExecutorProfile {
        program,
        args: Vec::new(),
        timeout: Duration::from_secs(10),
        max_code_bytes: 1024,
        max_response_bytes: 262_144,
        max_stderr_bytes: 1024,
        stop_grace: Duration::from_millis(500),
        reap_timeout: Duration::from_millis(100),
        max_concurrent: 1,
        handles_snippet: true,
        semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
    };
    state.add_onec_connection(
        "wire-test".to_owned(),
        crate::OnecConnection::with_optional_http(None, false, Some(profile)),
    );
    let server = crate::McpServer::new(crate::McpProfile::Workspace, state);
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(crate::serve_stream(server, server_io));
    let (read_half, mut writer) = tokio::io::split(client_io);
    let mut reader = BufReader::new(read_half);

    send_json(
        &mut writer,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "caller-cancel-test", "version": "0"}
            }
        }),
    )
    .await?;
    let initialized = collect_until_id(&mut reader, 1, Duration::from_secs(10)).await?;
    assert!(initialized.contains(&1));
    send_json(&mut writer, &json!({"jsonrpc":"2.0", "method":"notifications/initialized"})).await?;
    send_json(
        &mut writer,
        &json!({
            "jsonrpc":"2.0", "id":2, "method":"tools/call",
            "params":{"name":"execute", "arguments":{"action":"check", "code":"x = 1;"}}
        }),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    send_json(
        &mut writer,
        &json!({
            "jsonrpc":"2.0", "method":"notifications/cancelled",
            "params":{"requestId":2, "reason":"test cancellation"}
        }),
    )
    .await?;
    send_json(&mut writer, &json!({"jsonrpc":"2.0", "id":3, "method":"ping"})).await?;

    let seen = collect_until_id(&mut reader, 3, Duration::from_secs(10)).await?;
    assert!(
        response_marker.exists(),
        "fake executor produced valid JSON during cancellation grace"
    );
    assert!(seen.contains(&3), "ping proves the MCP session remained live");
    assert!(!seen.contains(&2), "cancelled check must not answer after native cleanup");
    Ok(())
}

async fn send_json<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> anyhow::Result<()> {
    writer.write_all(serde_json::to_string(value)?.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

async fn collect_until_id<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    target: u64,
    timeout: Duration,
) -> anyhow::Result<BTreeSet<u64>> {
    let mut ids = BTreeSet::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mut line = String::new();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("timed out waiting for JSON-RPC id {target}");
        }
        if tokio::time::timeout(remaining, reader.read_line(&mut line)).await?? == 0 {
            break;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&line) {
            if let Some(id) = value.get("id").and_then(Value::as_u64) {
                ids.insert(id);
                if id == target {
                    tokio::time::sleep(Duration::from_millis(700)).await;
                    loop {
                        line.clear();
                        let Ok(Ok(count)) = tokio::time::timeout(
                            Duration::from_millis(10),
                            reader.read_line(&mut line),
                        )
                        .await
                        else {
                            break;
                        };
                        if count == 0 {
                            break;
                        }
                        if let Ok(value) = serde_json::from_str::<Value>(&line) {
                            if let Some(id) = value.get("id").and_then(Value::as_u64) {
                                ids.insert(id);
                            }
                        }
                    }
                    return Ok(ids);
                }
            }
        }
    }
    Ok(ids)
}

#[tokio::test]
async fn collect_until_id_reads_later_jsonrpc_lines_independently() -> anyhow::Result<()> {
    let (mut writer, read_half) = tokio::io::duplex(1024);
    writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n")
        .await?;
    drop(writer);

    let mut reader = BufReader::new(read_half);
    let seen = collect_until_id(&mut reader, 3, Duration::from_secs(2)).await?;

    assert_eq!(seen, BTreeSet::from([2, 3]));
    Ok(())
}
