//! The thin client-facing proxy.
//!
//! Spawned by the MCP client (Claude/Codex) on stdio exactly like the plain server,
//! but instead of building any analysis state it connects to the shared per-project
//! backend — launching it detached if absent — and relays the MCP byte stream
//! between the client's stdio and the backend socket. Holds no state; cheap to
//! start and stop, so per-client/per-review churn costs a socket connect, not a
//! multi-gigabyte rebuild.

use std::fs::OpenOptions;
use std::time::Duration;

#[cfg(unix)]
use std::process::Child as BrokerChild;
#[cfg(unix)]
pub use std::process::Command as BrokerCommand;
#[cfg(windows)]
use windows_spawn::BrokerChild;
#[cfg(windows)]
pub use windows_spawn::BrokerCommand;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::Stream as TokioStream;
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;

use crate::broker::name::{backend_name, shared_backend_name, BackendKey, SharedBackendKey};
use crate::broker::transport::{connect_endpoint, HANDSHAKE_TIMEOUT};

#[cfg(windows)]
#[path = "proxy/windows_spawn.rs"]
mod windows_spawn;

/// Upper bound on waiting for a backend to become reachable. The backend binds
/// before its heavy build, so this only needs to cover process startup — but we
/// keep it generous to ride out a slow cold start.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a launched backend before assuming it died (crashed during
/// startup) and launching another. Re-launches are safe: a redundant backend loses
/// the bind race and exits immediately.
const RESPAWN_INTERVAL: Duration = Duration::from_secs(3);

/// Windows-only: how long to keep draining backend→client after the client closes
/// stdin before dropping the connection outright. Windows named pipes have no
/// half-close — `AsyncWrite::shutdown` is a no-op there — so the backend never
/// observes the client leaving via the stdin-EOF half-close, and both sides would
/// wait on each other forever. After stdin closes we drain for at most this long,
/// then drop the stream; closing the pipe handle is the disconnect the backend reads
/// as the end of that session. The unix path keeps draining to backend EOF (the
/// half-close delivers it), so this bound never applies there.
#[cfg(windows)]
const STDIN_CLOSED_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Outcome of a proxy attempt, separating a pre-session connect failure from a
/// mid-session relay failure — they need different handling by the caller.
pub enum ProxyOutcome {
    /// The session was served to completion (or the client disconnected normally).
    Served,
    /// The backend could not be reached or launched. No stdin was consumed yet, so the
    /// caller may safely fall back to serving the client directly over stdio.
    Unavailable(anyhow::Error),
}

/// Connect to the backend for `key`, launching it via `daemon_cmd` if it is not yet
/// reachable, then relay stdio to it until either side closes.
///
/// A connect-phase failure is returned as [`ProxyOutcome::Unavailable`] (stdin
/// untouched → safe to fall back). A relay-phase failure propagates as `Err`: by then
/// the stdin pump has consumed bytes, so re-serving on the same stream would be wrong.
pub async fn connect_or_launch(
    key: BackendKey,
    daemon_cmd: BrokerCommand,
) -> anyhow::Result<ProxyOutcome> {
    let stream = match connect_with_launch(&key, daemon_cmd).await {
        Ok(stream) => stream,
        Err(e) => return Ok(ProxyOutcome::Unavailable(e)),
    };
    relay_stdio(stream).await?;
    Ok(ProxyOutcome::Served)
}

/// Connect only to the exact backend process launched by a supervisor.
///
/// Unlike [`connect_or_launch`], this performs one connection attempt, never
/// starts a daemon, and never returns an outcome that permits stdio fallback.
pub async fn connect_required(key: BackendKey, expected_pid: u32) -> anyhow::Result<()> {
    let stream = connect_existing(&key, expected_pid).await?;
    relay_stdio(stream).await
}

/// Connect to the shared LSP/MCP backend, launching it if absent, after the existing
/// platform peer-trust checks. The caller performs the private protocol handshake next.
pub async fn connect_or_launch_stream(
    key: SharedBackendKey,
    daemon_cmd: BrokerCommand,
) -> anyhow::Result<TokioStream> {
    connect_with_launch(&key, daemon_cmd).await
}

/// Connect to the exact supervised shared backend process without launching a fallback.
pub async fn connect_required_stream(
    key: &SharedBackendKey,
    expected_pid: u32,
) -> anyhow::Result<TokioStream> {
    connect_existing_key(key, expected_pid).await
}

/// Relay stdio over an already connected and verified broker stream.
pub async fn relay_stream(stream: TokioStream) -> anyhow::Result<()> {
    relay(tokio::io::stdin(), tokio::io::stdout(), stream).await
}

/// Connection half of [`connect_required`], split out so the identity check can be exercised
/// without the stdio relay. Crate-private: the transport's contract is the whole operation, and
/// a raw stream handed to a caller would let it skip the relay the mode exists to guarantee.
#[cfg(any(unix, windows))]
pub(crate) async fn connect_existing(
    key: &BackendKey,
    expected_pid: u32,
) -> anyhow::Result<TokioStream> {
    connect_existing_key(key, expected_pid).await
}

trait EndpointKey {
    fn endpoint_name(&self) -> std::io::Result<interprocess::local_socket::Name<'static>>;
    fn log_path(&self) -> std::io::Result<std::path::PathBuf>;
    fn digest(&self) -> String;
}

impl EndpointKey for BackendKey {
    fn endpoint_name(&self) -> std::io::Result<interprocess::local_socket::Name<'static>> {
        backend_name(self)
    }
    fn log_path(&self) -> std::io::Result<std::path::PathBuf> {
        self.runtime_log_path()
    }
    fn digest(&self) -> String {
        BackendKey::digest(self)
    }
}

impl EndpointKey for SharedBackendKey {
    fn endpoint_name(&self) -> std::io::Result<interprocess::local_socket::Name<'static>> {
        shared_backend_name(self)
    }
    fn log_path(&self) -> std::io::Result<std::path::PathBuf> {
        self.runtime_log_path()
    }
    fn digest(&self) -> String {
        SharedBackendKey::digest(self)
    }
}

async fn connect_existing_key<K: EndpointKey>(
    key: &K,
    expected_pid: u32,
) -> anyhow::Result<TokioStream> {
    let stream = connect_endpoint(key.endpoint_name()?, HANDSHAKE_TIMEOUT)
        .await
        .map_err(|error| anyhow::anyhow!("required broker backend is unavailable: {error}"))?;
    // Naming the owner is what tells a supervisor which of two different things happened: its
    // daemon lost the bind to a backend that was already serving this workspace (and exited),
    // or the socket is answered by something it should look at.
    if let Err(mismatch) = crate::broker::security::verify_supervised_backend(&stream, expected_pid)
    {
        use crate::broker::security::SupervisedMismatch;
        return Err(match mismatch {
            SupervisedMismatch::OtherPid(actual) => anyhow::anyhow!(
                "required broker backend is pid {actual}, not the supervised pid {expected_pid}: \
                 this workspace already had a backend, and the supervised daemon exited without \
                 becoming one"
            ),
            SupervisedMismatch::Unknown => anyhow::anyhow!(
                "required broker backend identity could not be established for supervised pid \
                 {expected_pid}"
            ),
        });
    }
    tracing::info!(
        backend_pid = expected_pid,
        backend_key = %key.digest(),
        "connected to supervised broker backend"
    );
    Ok(stream)
}

async fn connect_with_launch<K: EndpointKey>(
    key: &K,
    mut daemon_cmd: BrokerCommand,
) -> anyhow::Result<TokioStream> {
    let deadline = Instant::now() + LAUNCH_TIMEOUT;
    let initial_budget = deadline.saturating_duration_since(Instant::now()).min(HANDSHAKE_TIMEOUT);
    // Initial direct connect.
    //
    // On Unix the per-user runtime dir is the trust boundary (mode 0700, owner
    // checked), so a connect succeeding against a socket in that dir proves the
    // peer is ours — return the stream directly.
    //
    // On Windows the deterministic pipe name can be raced by a hostile local
    // user pre-creating the pipe with their own DACL, so any successful connect
    // must pass the same `verify_pipe_server_trusted` gate used by the polling
    // loop below. A trusted already-running backend is reused; an unverified
    // pipe is dropped and we fall through to launch + verify our own child.
    #[cfg(unix)]
    {
        if !initial_budget.is_zero() {
            if let Ok(stream) = connect_endpoint(key.endpoint_name()?, initial_budget).await {
                return Ok(stream);
            }
        }
    }
    #[cfg(windows)]
    {
        if !initial_budget.is_zero() {
            if let Ok(stream) = connect_endpoint(key.endpoint_name()?, initial_budget).await {
                if let Some(trusted) = verify_or_drop_peer(stream) {
                    return Ok(trusted);
                }
            }
        }
    }

    if Instant::now() >= deadline {
        return Err(anyhow::anyhow!(
            "broker backend did not become reachable within {}s: connect budget expired",
            LAUNCH_TIMEOUT.as_secs()
        ));
    }

    // No backend yet: launch one detached, then poll-connect. If the backend never
    // becomes reachable (e.g. it crashed during startup), relaunch periodically —
    // bind-wins makes a redundant launch self-correcting.
    let mut children: Vec<BrokerChild> = Vec::new();
    children.push(spawn_detached(key, &mut daemon_cmd)?);

    let mut next_respawn = Instant::now() + RESPAWN_INTERVAL;
    let mut delay = Duration::from_millis(25);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            reap(&mut children);
            return Err(anyhow::anyhow!(
                "broker backend did not become reachable within {}s: connect budget expired",
                LAUNCH_TIMEOUT.as_secs()
            ));
        }
        let last_err =
            match connect_endpoint(key.endpoint_name()?, remaining.min(HANDSHAKE_TIMEOUT)).await {
                Ok(stream) => {
                    if let Some(trusted) = verify_or_drop_peer(stream) {
                        reap(&mut children);
                        return Ok(trusted);
                    }
                    // Windows: the pre-existing pipe did not pass the trust gate.
                    // Keep polling — our own launched backend may win the bind on a
                    // later attempt; surface the last real connect error on timeout.
                    None
                }
                Err(e) => Some(e),
            };
        if Instant::now() >= deadline {
            reap(&mut children);
            let detail = last_err
                .map(|e| format!("{e}"))
                .unwrap_or_else(|| "all successful connects were from unverified pipes".to_owned());
            return Err(anyhow::anyhow!(
                "broker backend did not become reachable within {}s: {detail}",
                LAUNCH_TIMEOUT.as_secs()
            ));
        }
        if Instant::now() >= next_respawn {
            reap(&mut children);
            children.push(spawn_detached(key, &mut daemon_cmd)?);
            next_respawn = Instant::now() + RESPAWN_INTERVAL;
        }
        tokio::time::sleep(delay.min(deadline.saturating_duration_since(Instant::now()))).await;
        delay = (delay * 2).min(Duration::from_millis(500));
    }
}

/// Verify a connected peer is a trusted backend, returning the stream if
/// trusted or `None` if it must be dropped.
///
/// On Unix the runtime dir + DACL is the trust boundary, so any successful
/// connect is accepted unchanged. On Windows the deterministic pipe name is
/// raceable, so `security::verify_pipe_server_trusted` introspects the server
/// PID's image path and owner SID via `sysinfo` and compares them against the
/// current process — the same gate used by `daemon::probe_live`. No
/// project-local `unsafe`, no handwritten Win32 FFI.
#[cfg(unix)]
fn verify_or_drop_peer(stream: TokioStream) -> Option<TokioStream> {
    Some(stream)
}

#[cfg(windows)]
fn verify_or_drop_peer(stream: TokioStream) -> Option<TokioStream> {
    if crate::broker::security::verify_pipe_server_trusted(&stream) {
        Some(stream)
    } else {
        None
    }
}

/// Reap any spawned backend that has already exited (a race loser), clearing the
/// zombie. The live winner reports `None` and keeps running; its handle is dropped
/// without killing the process, which reparents to init when this proxy exits.
fn reap(children: &mut Vec<BrokerChild>) {
    children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
}

/// Relay the client's stdio to the backend: the production entry point, a thin wrapper
/// over [`relay`] with the process's own `stdin`/`stdout` as the client side.
async fn relay_stdio(stream: TokioStream) -> anyhow::Result<()> {
    relay(tokio::io::stdin(), tokio::io::stdout(), stream).await
}

/// Relay bytes both ways between a client (`client_in`/`client_out`) and the backend
/// `stream`. The backend→client direction is authoritative: it is drained and flushed
/// before returning, so a final response is not truncated by the client closing its
/// input first. The client→backend pump runs concurrently and half-closes the write
/// side on input EOF.
///
/// On unix that half-close delivers EOF to the backend, ending this session there; the
/// backend closes its side of the connection, so the relay drains to backend EOF — a slow
/// final response is never cut off. The backend process itself stays warm for the next
/// client.
///
/// Windows named pipes have no half-close (`AsyncWrite::shutdown` is a no-op), so the
/// backend never sees that EOF. There the relay drains until the backend closes or,
/// once the client input has closed, a bounded grace elapses, then returns — dropping
/// the stream closes the pipe handle, the disconnect the backend reads as the end of
/// this session.
///
/// Split out from [`relay_stdio`] (which binds it to process stdio) so the teardown
/// behavior can be exercised by tests with in-memory client streams.
pub async fn relay<R, W>(client_in: R, mut client_out: W, stream: TokioStream) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (mut from_backend, mut to_backend) = stream.split();

    let (input_closed_tx, input_closed_rx) = tokio::sync::oneshot::channel();
    let pump_in = tokio::spawn(async move {
        let mut client_in = client_in;
        let _ = tokio::io::copy(&mut client_in, &mut to_backend).await;
        let _ = to_backend.shutdown().await;
        let _ = input_closed_tx.send(());
    });

    // Unix: the half-close delivers EOF to the backend, which ends this session and
    // closes its side; draining to backend EOF is correct and never truncates a slow
    // final response.
    #[cfg(not(windows))]
    let copied = {
        drop(input_closed_rx);
        tokio::io::copy(&mut from_backend, &mut client_out).await
    };

    // Windows: named pipes have no half-close, so the backend never sees the input EOF
    // and `copy_fut` would never complete on its own. Drain until either the backend
    // closes or, once the client input has closed, the grace elapses — then return so
    // the dropped stream closes the handle and ends this backend session.
    // Scoped so `copy_fut`'s borrow of `client_out` is released before the flush below.
    #[cfg(windows)]
    let copied = {
        let copy_fut = tokio::io::copy(&mut from_backend, &mut client_out);
        tokio::pin!(copy_fut);
        tokio::select! {
            copied = &mut copy_fut => copied,
            _ = input_closed_rx => {
                match tokio::time::timeout(STDIN_CLOSED_DRAIN_GRACE, &mut copy_fut).await {
                    Ok(copied) => copied,
                    Err(_) => {
                        tracing::debug!("client closed input; drain grace elapsed, closing backend pipe");
                        Ok(0)
                    }
                }
            }
        }
    };

    let _ = client_out.flush().await;

    pump_in.abort();
    copied?;
    Ok(())
}

/// Spawn the backend so it outlives this proxy: a new process group (unix) /
/// detached, new-group process (windows), with stdio closed and a daemon-owned
/// bounded log in the runtime dir. Returns the child
/// handle so a fast-exiting race loser can be reaped.
fn spawn_detached<K: EndpointKey>(key: &K, cmd: &mut BrokerCommand) -> anyhow::Result<BrokerChild> {
    let log_path = key.log_path()?;
    OpenOptions::new().create(true).append(true).open(&log_path)?;
    // A raw inherited file bypasses rotation for the whole lifetime of the broker.
    cmd.env(super::DETACHED_LOG_PATH_ENV, &log_path);

    #[cfg(unix)]
    let child = {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?
    };
    #[cfg(windows)]
    let child = windows_spawn::spawn(cmd)?;
    tracing::info!(log = %log_path.display(), "launched broker backend");
    Ok(child)
}

#[cfg(test)]
#[cfg(windows)]
#[path = "proxy/windows_lifetime_tests.rs"]
mod windows_lifetime_tests;

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::broker::security::cfg_supervised_pid;
    use crate::broker::BackendKey;
    use crate::McpProfile;

    fn key_for(src: &TempDir) -> BackendKey {
        BackendKey::new(
            src.path(),
            crate::WorkspaceCacheLayout::for_workspace(src.path()).root(),
            McpProfile::Workspace,
            0,
            0,
            std::collections::BTreeSet::new(),
        )
    }

    async fn connect(key: &BackendKey) -> std::io::Result<TokioStream> {
        connect_endpoint(backend_name(key)?, HANDSHAKE_TIMEOUT).await
    }

    #[tokio::test]
    async fn the_supervised_connect_neither_launches_nor_falls_back() {
        let src = TempDir::new().unwrap();
        let key = key_for(&src);

        let error = connect_existing(&key, std::process::id())
            .await
            .expect_err("required mode must not launch or fall back when no backend exists");

        assert!(error.to_string().contains("unavailable"), "{error}");
        assert!(connect(&key).await.is_err(), "required connect must not auto-launch a daemon");
    }

    cfg_supervised_pid! { gate
        /// Only builds where peer credentials carry the peer's PID — the platforms named by
        /// [`crate::broker::security::SUPERVISED_PID_PLATFORMS`], expanded here from that
        /// one spelling rather than mirrored. Darwin's `xucred` and DragonFly's carry no
        /// PID, so supervised mode is refused up front there and the identity asserted
        /// below cannot be established at all.
        ///
        /// The gate sits on the module so the daemon-only helpers go with it, rather than
        /// lingering as dead code wherever the test cannot run.
        mod supervised_pid {
            use std::time::Duration;

            use rmcp::ServiceExt;
            use tempfile::TempDir;

            use super::{connect, connect_existing, key_for, TokioStream};
            use crate::broker::{self, BackendKey};
            use crate::{McpProfile, McpServer, SharedState};

            fn reference_server() -> McpServer {
                McpServer::new(McpProfile::Reference, SharedState::reference(None))
            }

            async fn connect_within(key: &BackendKey, budget: Duration) -> TokioStream {
                let deadline = tokio::time::Instant::now() + budget;
                loop {
                    if let Ok(s) = connect(key).await {
                        return s;
                    }
                    assert!(tokio::time::Instant::now() < deadline, "backend never became reachable");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn the_supervised_connect_takes_only_the_named_daemon() {
                let src = TempDir::new().unwrap();
                let key = key_for(&src);
                let backend = tokio::spawn(broker::daemon::run(
                    || Ok(reference_server()),
                    key_for(&src),
                    Duration::from_secs(30),
                    Duration::from_secs(30),
                ));
                let _ = connect_within(&key, Duration::from_secs(10)).await;

                let wrong_pid = std::process::id().checked_add(1).unwrap();
                let mismatch = connect_existing(&key, wrong_pid)
                    .await
                    .expect_err("a live but different backend PID must be rejected");
                assert!(mismatch.to_string().contains("not the supervised pid"), "{mismatch}");

                let stream = connect_existing(&key, std::process::id())
                    .await
                    .expect("the exact supervised daemon PID is trusted");
                let client = ().serve(stream).await.expect("supervised daemon serves MCP");
                assert!(client.peer_info().is_some(), "session saw the supervised backend");
                client.cancel().await.ok();
                backend.abort();
            }
        }
    }
}
