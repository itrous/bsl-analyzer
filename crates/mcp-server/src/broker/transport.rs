//! Private mixed-protocol framing for the shared LSP/MCP broker.

use std::time::Duration;

use interprocess::local_socket::tokio::Stream as TokioStream;
use interprocess::local_socket::{ConnectOptions, Name};
use interprocess::ConnectWaitMode;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;

use super::name::{SharedBackendIdentity, SharedBackendKey, SHARED_PROTOCOL_VERSION};

pub const MAX_HANDSHAKE_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_CONTEXT_BYTES: usize = 1024 * 1024;
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DATA_CHUNK: usize = 16 * 1024;
const MAX_CONTROL_BYTES: usize = 4 * 1024;

/// Connect to a local broker endpoint without leaving an unbounded Windows pipe waiter.
///
/// Interprocess implements the Windows unbounded wait with a `spawn_blocking` loop around
/// `WaitNamedPipeW(FOREVER)`. Cancelling the async future cannot stop that worker, so every
/// broker attach and liveness probe supplies a finite wait here.
pub(crate) async fn connect_endpoint(
    name: Name<'static>,
    timeout: Duration,
) -> std::io::Result<TokioStream> {
    if timeout < Duration::from_millis(1) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "broker endpoint connect budget expired",
        ));
    }
    let options = ConnectOptions::new().name(name).wait_mode(ConnectWaitMode::Timeout(timeout));
    let connect = options.connect_tokio();
    match tokio::time::timeout(timeout, connect).await {
        Ok(result) => result,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "broker endpoint connect timed out",
        )),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Mcp,
    Lsp,
}

pub struct Handshake {
    pub identity: SharedBackendIdentity,
    pub adapter: AdapterKind,
}

#[derive(Serialize, Deserialize)]
struct WireHandshake {
    version: u16,
    identity: [u8; 32],
    scope: [u8; 32],
    config: [u8; 32],
    platform: [u8; 32],
    build: [u8; 32],
    adapter: AdapterKind,
}

/// Send and validate the private attach handshake before any public protocol bytes.
pub async fn client_handshake<S>(io: &mut S, hello: &Handshake) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let [scope, config, platform, build] = hello.identity.handshake_fingerprints();
        let wire = WireHandshake {
            version: SHARED_PROTOCOL_VERSION,
            identity: hello.identity.digest(),
            scope,
            config,
            platform,
            build,
            adapter: hello.adapter,
        };
        let bytes = serde_json::to_vec(&wire)?;
        write_bounded_record(io, &bytes, MAX_HANDSHAKE_BYTES).await?;
        let accepted = io.read_u8().await?;
        anyhow::ensure!(accepted == 0, "shared broker rejected the private handshake");
        anyhow::Ok(())
    })
    .await?
}

/// Read a bounded handshake and reject incompatible clients without starting an adapter.
pub async fn server_handshake<S>(
    io: &mut S,
    expected: &SharedBackendKey,
) -> anyhow::Result<Handshake>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let result = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let len = io.read_u32().await? as usize;
        anyhow::ensure!(len <= MAX_HANDSHAKE_BYTES, "private handshake exceeds 4 KiB");
        let mut bytes = vec![0; len];
        io.read_exact(&mut bytes).await?;
        let wire: WireHandshake = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            wire.version == SHARED_PROTOCOL_VERSION,
            "private protocol version mismatch"
        );
        anyhow::ensure!(
            wire.identity == expected.identity_fingerprint(),
            "shared backend identity mismatch"
        );
        anyhow::ensure!(
            [wire.scope, wire.config, wire.platform, wire.build]
                == expected.identity().handshake_fingerprints(),
            "shared backend configuration or build mismatch"
        );
        anyhow::Ok(Handshake { identity: expected.identity().clone(), adapter: wire.adapter })
    })
    .await?;
    io.write_u8(u8::from(result.is_err())).await?;
    io.flush().await?;
    result
}

/// Write session-only context as checked chunks. The receiver allocates only after
/// validating the advertised total against the 1 MiB cap.
pub async fn write_session_context<S>(io: &mut S, context: &[u8]) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    anyhow::ensure!(context.len() <= MAX_SESSION_CONTEXT_BYTES, "session context exceeds 1 MiB");
    io.write_u8(0x20).await?;
    io.write_u32(context.len() as u32).await?;
    for chunk in context.chunks(MAX_DATA_CHUNK) {
        io.write_u32(chunk.len() as u32).await?;
        io.write_all(chunk).await?;
    }
    io.write_u32(0).await?;
    io.flush().await?;
    Ok(())
}

pub async fn read_session_context<S>(io: &mut S) -> anyhow::Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        anyhow::ensure!(io.read_u8().await? == 0x20, "expected session context record");
        let total = io.read_u32().await? as usize;
        anyhow::ensure!(total <= MAX_SESSION_CONTEXT_BYTES, "session context exceeds 1 MiB");
        let mut context = Vec::with_capacity(total);
        loop {
            let len = io.read_u32().await? as usize;
            if len == 0 {
                break;
            }
            anyhow::ensure!(
                len <= MAX_DATA_CHUNK && context.len() + len <= total,
                "invalid session context chunk"
            );
            let old_len = context.len();
            context.resize(old_len + len, 0);
            io.read_exact(&mut context[old_len..]).await?;
        }
        anyhow::ensure!(context.len() == total, "incomplete session context");
        anyhow::Ok(context)
    })
    .await?
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum LspControl {
    ScopeRetired { epoch: u64, reason: String },
    ResumeLsp(Vec<u8>),
}

/// LSP data appears as an ordinary byte stream to the adapter. Private controls stay
/// on the side channel and document bytes are split into bounded records without a size cap.
pub struct LspChannel {
    pub io: DuplexStream,
    incoming_controls: mpsc::Receiver<LspControl>,
    outgoing_controls: mpsc::Sender<LspControl>,
}

impl LspChannel {
    pub fn new<S>(stream: S, on_first_data: impl FnOnce() + Send + 'static) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (io, pump) = tokio::io::duplex(64 * 1024);
        let (app_read, app_write) = tokio::io::split(pump);
        let (wire_read, wire_write) = tokio::io::split(stream);
        let (control_tx, control_rx) = mpsc::channel(16);
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel(16);

        tokio::spawn(async move {
            let mut app_read = app_read;
            let mut wire_write = wire_write;
            let mut buf = vec![0; MAX_DATA_CHUNK];
            let mut controls_open = true;
            loop {
                tokio::select! {
                    biased;
                    control = outgoing_rx.recv(), if controls_open => match control {
                        Some(control) => if write_control(&mut wire_write, control).await.is_err() { break; },
                        // The adapter may have queued its final response before closing controls.
                        None => controls_open = false,
                    },
                    read = app_read.read(&mut buf) => match read {
                        Ok(0) => { let _ = write_frame(&mut wire_write, 6, &[]).await; break; },
                        Ok(len) => if write_frame(&mut wire_write, 1, &buf[..len]).await.is_err() { break; },
                        Err(_) => break,
                    },
                }
            }
            let _ = wire_write.shutdown().await;
        });

        tokio::spawn(async move {
            let mut wire_read = wire_read;
            let mut app_write = app_write;
            let mut first_data = Some(on_first_data);
            loop {
                let Ok((kind, payload)) = read_frame(&mut wire_read).await else { break };
                match kind {
                    1 => {
                        if let Some(callback) = first_data.take() {
                            callback();
                        }
                        if app_write.write_all(&payload).await.is_err() {
                            break;
                        }
                    }
                    2 => {
                        if let Ok(control) = serde_json::from_slice(&payload) {
                            if control_tx.send(control).await.is_err() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    3 => {
                        let control = match read_resume(&mut wire_read, payload).await {
                            Ok(context) => LspControl::ResumeLsp(context),
                            Err(_) => break,
                        };
                        if control_tx.send(control).await.is_err() {
                            break;
                        }
                    }
                    6 => break,
                    _ => break,
                }
            }
            let _ = app_write.shutdown().await;
        });

        Self { io, incoming_controls: control_rx, outgoing_controls: outgoing_tx }
    }

    pub async fn recv_control(&mut self) -> Option<LspControl> {
        self.incoming_controls.recv().await
    }

    pub async fn send_control(&self, control: LspControl) -> anyhow::Result<()> {
        self.outgoing_controls
            .send(control)
            .await
            .map_err(|_| anyhow::anyhow!("LSP transport closed"))
    }

    pub fn into_parts(
        self,
    ) -> (DuplexStream, mpsc::Receiver<LspControl>, mpsc::Sender<LspControl>) {
        (self.io, self.incoming_controls, self.outgoing_controls)
    }
}

async fn write_control<W: AsyncWrite + Unpin>(
    wire: &mut W,
    control: LspControl,
) -> anyhow::Result<()> {
    match control {
        LspControl::ScopeRetired { epoch, reason } => {
            let payload = serde_json::to_vec(&LspControl::ScopeRetired { epoch, reason })?;
            anyhow::ensure!(payload.len() <= MAX_CONTROL_BYTES, "LSP control exceeds 4 KiB");
            write_frame(wire, 2, &payload).await?;
        }
        LspControl::ResumeLsp(context) => {
            anyhow::ensure!(
                context.len() <= MAX_SESSION_CONTEXT_BYTES,
                "resume context exceeds 1 MiB"
            );
            write_frame(wire, 3, &(context.len() as u32).to_le_bytes()).await?;
            for chunk in context.chunks(MAX_DATA_CHUNK) {
                write_frame(wire, 4, chunk).await?;
            }
            write_frame(wire, 5, &[]).await?;
        }
    }
    Ok(())
}

async fn read_resume<R: AsyncRead + Unpin>(
    wire: &mut R,
    prefix: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(prefix.len() == 4, "invalid resume context header");
    let total = u32::from_le_bytes(prefix.try_into().unwrap()) as usize;
    anyhow::ensure!(total <= MAX_SESSION_CONTEXT_BYTES, "resume context exceeds 1 MiB");
    let mut out = Vec::with_capacity(total);
    loop {
        let (kind, payload) = read_frame(wire).await?;
        match kind {
            4 if payload.len() <= MAX_DATA_CHUNK && out.len() + payload.len() <= total => {
                out.extend(payload)
            }
            5 => break,
            _ => anyhow::bail!("invalid resume context chunk"),
        }
    }
    anyhow::ensure!(out.len() == total, "incomplete resume context");
    Ok(out)
}

async fn write_frame<W: AsyncWrite + Unpin>(
    wire: &mut W,
    kind: u8,
    payload: &[u8],
) -> anyhow::Result<()> {
    wire.write_u32((payload.len() + 1) as u32).await?;
    wire.write_u8(kind).await?;
    wire.write_all(payload).await?;
    wire.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(wire: &mut R) -> anyhow::Result<(u8, Vec<u8>)> {
    let len = wire.read_u32().await? as usize;
    anyhow::ensure!((1..=MAX_DATA_CHUNK + 1).contains(&len), "invalid LSP transport frame size");
    let mut bytes = vec![0; len];
    wire.read_exact(&mut bytes).await?;
    Ok((bytes[0], bytes.split_off(1)))
}

async fn write_bounded_record<S>(io: &mut S, bytes: &[u8], cap: usize) -> anyhow::Result<()>
where
    S: AsyncWrite + Unpin,
{
    anyhow::ensure!(bytes.len() <= cap, "private handshake exceeds 4 KiB");
    io.write_u32(bytes.len() as u32).await?;
    io.write_all(bytes).await?;
    io.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::super::name::shared_backend_name;
    use super::*;

    fn identity(root: &TempDir, config: u8) -> SharedBackendIdentity {
        SharedBackendIdentity::new(
            root.path(),
            root.path().join("cache"),
            [1; 32],
            [config; 32],
            [3; 32],
            [4; 32],
        )
    }

    #[tokio::test]
    async fn endpoint_connect_rejects_zero_and_submillisecond_budgets() {
        let dir = TempDir::new().unwrap();
        let key = SharedBackendKey::new(identity(&dir, 1));
        for timeout in [Duration::ZERO, Duration::from_micros(999)] {
            let result = connect_endpoint(shared_backend_name(&key).unwrap(), timeout).await;
            assert!(matches!(result, Err(error) if error.kind() == std::io::ErrorKind::TimedOut));
        }
    }

    #[tokio::test]
    async fn handshake_rejects_identity_mismatch_before_attach() {
        let dir = TempDir::new().unwrap();
        let key = SharedBackendKey::new(identity(&dir, 1));
        let hello = Handshake { identity: identity(&dir, 2), adapter: AdapterKind::Lsp };
        let (mut client, mut server) = tokio::io::duplex(4096);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, &key).await });
        assert!(client_handshake(&mut client, &hello).await.is_err());
        assert!(server_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn handshake_rejects_oversized_record_before_allocation() {
        let dir = TempDir::new().unwrap();
        let key = SharedBackendKey::new(identity(&dir, 1));
        let (mut client, mut server) = tokio::io::duplex(16);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, &key).await });
        client.write_u32((MAX_HANDSHAKE_BYTES as u32) + 1).await.unwrap();
        client.flush().await.unwrap();
        assert_eq!(client.read_u8().await.unwrap(), 1);
        assert!(server_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn handshake_rejects_malformed_payload() {
        let dir = TempDir::new().unwrap();
        let key = SharedBackendKey::new(identity(&dir, 1));
        let (mut client, mut server) = tokio::io::duplex(16);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, &key).await });
        client.write_u32(1).await.unwrap();
        client.write_u8(b'{').await.unwrap();
        assert_eq!(client.read_u8().await.unwrap(), 1);
        assert!(server_task.await.unwrap().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn slow_handshake_expires_at_its_deadline() {
        let dir = TempDir::new().unwrap();
        let key = SharedBackendKey::new(identity(&dir, 1));
        let (_client, mut server) = tokio::io::duplex(16);
        let server_task = tokio::spawn(async move { server_handshake(&mut server, &key).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(server_task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn session_context_rejects_oversize_and_round_trips_chunks() {
        let (mut writer, mut reader) = tokio::io::duplex(32 * 1024);
        let payload = vec![0x5a; MAX_DATA_CHUNK * 3 + 17];
        let expected = payload.clone();
        let write = tokio::spawn(async move { write_session_context(&mut writer, &payload).await });
        assert_eq!(read_session_context(&mut reader).await.unwrap(), expected);
        write.await.unwrap().unwrap();

        let mut sink = tokio::io::sink();
        assert!(write_session_context(&mut sink, &vec![0; MAX_SESSION_CONTEXT_BYTES + 1])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn lsp_channel_chunks_large_public_data_and_keeps_controls_private() {
        let (client_wire, server_wire) = tokio::io::duplex(64 * 1024);
        let client = LspChannel::new(client_wire, || {});
        let mut server = LspChannel::new(server_wire, || {});
        let LspChannel { io: mut client_io, incoming_controls: mut client_controls, .. } = client;
        let expected = vec![0xA5; 2 * 1024 * 1024 + 31];
        let source = expected.clone();
        let writer = tokio::spawn(async move { client_io.write_all(&source).await.unwrap() });
        let mut actual = vec![0; expected.len()];
        tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(10)),
            server.io.read_exact(&mut actual),
        )
        .await
        .expect("chunked data reaches the reader before the hang guard")
        .unwrap();
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(10)), writer)
            .await
            .expect("chunk writer finishes before the hang guard")
            .unwrap();
        assert_eq!(actual, expected);

        tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(2)),
            server.send_control(LspControl::ScopeRetired { epoch: 17, reason: "scope".into() }),
        )
        .await
        .expect("scope-retired control send is bounded")
        .unwrap();
        let control = tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(2)),
            client_controls.recv(),
        )
        .await
        .expect("scope-retired control reaches the peer");
        assert!(matches!(
            control,
            Some(LspControl::ScopeRetired { epoch: 17, reason }) if reason == "scope"
        ));

        let resume = vec![0x3c; MAX_DATA_CHUNK * 7 + 3];
        let expected_resume = resume.clone();
        tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(2)),
            server.send_control(LspControl::ResumeLsp(resume)),
        )
        .await
        .expect("resume control send is bounded")
        .unwrap();
        let control = tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(2)),
            client_controls.recv(),
        )
        .await
        .expect("resume control reaches the peer");
        assert!(matches!(
            control,
            Some(LspControl::ResumeLsp(context)) if context == expected_resume
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lsp_channel_drains_queued_response_when_last_control_sender_closes() {
        let (wire, mut peer) = tokio::io::duplex(4096);
        let mut channel = LspChannel::new(wire, || {});
        let response = br#"{"jsonrpc":"2.0","id":8,"result":null}"#;
        // The current-thread runtime cannot poll the pump before this ready write and drop.
        channel.io.write_all(response).await.unwrap();
        drop(channel);

        let (kind, payload) = tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(2)),
            read_frame(&mut peer),
        )
        .await
        .unwrap()
        .expect("queued shutdown response reaches the peer before EOF");
        assert_eq!(kind, 1);
        assert_eq!(payload, response);
        assert_eq!(
            tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(2)),
                read_frame(&mut peer)
            )
            .await
            .unwrap()
            .unwrap(),
            (6, Vec::new())
        );
    }

    #[tokio::test]
    async fn lsp_control_preempts_a_continuous_data_producer() {
        let (client_wire, server_wire) = tokio::io::duplex(64 * 1024);
        let (mut client_io, _, client_controls) = LspChannel::new(client_wire, || {}).into_parts();
        let (mut server_io, mut server_controls, _) =
            LspChannel::new(server_wire, || {}).into_parts();
        let drain = tokio::spawn(async move {
            let mut sink = tokio::io::sink();
            tokio::io::copy(&mut server_io, &mut sink).await.unwrap();
        });
        let producer = tokio::spawn(async move {
            let chunk = [0x61; MAX_DATA_CHUNK];
            loop {
                client_io.write_all(&chunk).await.unwrap();
                tokio::task::yield_now().await;
            }
        });

        let context = vec![0x2a; MAX_DATA_CHUNK + 7];
        let expected = context.clone();
        tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(2)),
            client_controls.send(LspControl::ResumeLsp(context)),
        )
        .await
        .unwrap()
        .unwrap();
        let control = tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(2)),
            server_controls.recv(),
        )
        .await
        .unwrap();
        assert!(matches!(control, Some(LspControl::ResumeLsp(context)) if context == expected));
        assert!(!producer.is_finished(), "producer should still be writing when control arrives");

        producer.abort();
        let _ =
            tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(2)), producer)
                .await
                .expect("aborted producer stops");
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(2)), drain)
            .await
            .unwrap()
            .unwrap();
    }
}
