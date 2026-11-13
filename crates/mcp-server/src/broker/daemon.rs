//! The shared backend.
//!
//! Binds the per-project rendezvous *before* building the heavy [`SharedState`], so
//! a process that loses the launch race exits without ever starting a competing
//! build against the same per-project databases. The winner builds once and serves
//! every connecting proxy from it.
//!
//! Lifetime is **idle-driven**, so the warm backend survives the connection churn of
//! editors (opencode, Zed, …) that restart or cycle their MCP link: it stays up as long
//! as a session is connected, and after the last one leaves it lingers for an idle TTL
//! so a reconnecting client reuses the resident state instead of paying the multi-second
//! cold rebuild. Two graces apply depending on whether the backend was ever *used*: a
//! backend that has seen real MCP traffic idles out after `idle_ttl`; one that never did
//! (e.g. the launching proxy died before its first request, or only liveness probes
//! connected) gives up after the much shorter `orphan_grace`. "Used" is gated on the
//! first byte a session sends, so a no-data liveness probe never counts as traffic.
//!
//! [`SharedState`]: crate::SharedState

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::tokio::Listener as TokioListener;
use interprocess::local_socket::tokio::Stream as TokioStream;
use interprocess::local_socket::ListenerOptions;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{interval, Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::broker::name::{backend_name, shared_backend_name, BackendKey, SharedBackendKey};
#[cfg(windows)]
use crate::broker::security::pipe_security_descriptor_for_current_user;
use crate::broker::transport::{
    connect_endpoint, read_session_context, server_handshake, AdapterKind, Handshake, LspChannel,
    HANDSHAKE_TIMEOUT,
};
use crate::{serve_stream_with_shutdown, McpServer};

/// Cap on connections held while the resident state builds. The listener keeps draining
/// past this (so a concurrent liveness probe still succeeds), but excess connections are
/// dropped rather than parked, bounding memory against a runaway local client.
const MAX_PARKED_DURING_BUILD: usize = 128;
const MAX_SHARED_HANDSHAKES: usize = 128;

/// How long a superseded backend lets its answers in flight reach their clients, once its
/// graph reads are done, before it closes the sessions.
const SUPERSEDED_DRAIN: Duration = Duration::from_secs(2);

/// Run the backend for `key`. `build` is invoked only after this process wins the
/// bind, so the expensive state construction (which spawns background builds
/// touching the project DBs) never runs in a race loser.
///
/// `orphan_grace` bounds how long a backend that has never seen real MCP traffic waits
/// (with no active connections) before giving up; `idle_ttl` is the longer window a
/// backend that *has* served traffic stays warm after its last session leaves, so a
/// reconnecting client reuses it instead of triggering a cold rebuild.
///
/// Returns `Ok(())` both when this process served as the backend and exited, and when
/// another live backend already owned the name (nothing to do).
pub async fn run<F>(
    build: F,
    key: BackendKey,
    orphan_grace: Duration,
    idle_ttl: Duration,
) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<McpServer> + Send + 'static,
{
    let Some(listener) = bind(&key).await? else {
        tracing::info!("backend already serving this project; nothing to do");
        return Ok(());
    };

    // Build off the async runtime, and start accepting immediately. A bound socket that
    // isn't being accepted looks dead to a second daemon's liveness probe during the
    // multi-minute cold build — which would make that daemon reclaim (steal) our socket
    // and split the project across two backends. Draining the backlog from the first
    // accept keeps the probe honest; connections that arrive mid-build are parked and
    // served once the resident state is ready.
    let mut build_handle = tokio::task::spawn_blocking(build);
    let mut parked: Vec<TokioStream> = Vec::new();
    let server = loop {
        tokio::select! {
            built = &mut build_handle => {
                break built.map_err(|e| anyhow::anyhow!("backend build task panicked: {e}"))??;
            }
            accepted = listener.accept() => {
                let conn = accepted?;
                if !listener.peer_authorized(&conn) {
                    tracing::warn!("rejected backend connection from an unauthorized peer");
                } else if parked.len() >= MAX_PARKED_DURING_BUILD {
                    // Keep draining the backlog past the cap so a concurrent liveness
                    // probe still succeeds, but drop the excess instead of parking it —
                    // bounding memory against a runaway local client during a long build.
                    tracing::warn!(
                        cap = MAX_PARKED_DURING_BUILD,
                        "too many connections during build; dropping excess"
                    );
                } else {
                    parked.push(conn);
                }
            }
        }
    };

    let guard = server.clone();
    // Flush/persist resident state on the way out (success or failure), mirroring
    // the stdio path, before the process exits.
    let result = serve(server, listener, parked, orphan_grace, idle_ttl).await;
    guard.shutdown();
    result
}

/// A connection accepted by the mixed-protocol broker.
pub trait BrokerIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> BrokerIo for T {}

pub type BoxedBrokerStream = Box<dyn BrokerIo>;

pub enum SharedSession {
    Mcp(BoxedBrokerStream),
    Lsp(LspChannel),
}

/// Bind and serve adapter sessions as soon as the lightweight owner callback exists.
/// `stop` ends serving the way idleness does, so the owner still shuts down.
#[allow(clippy::too_many_arguments)] // Each callback belongs to the upper adapter owner.
pub async fn run_shared<F, Fut>(
    key: SharedBackendKey,
    on_session: F,
    scope_retired: CancellationToken,
    stop: CancellationToken,
    orphan_grace: Duration,
    idle_ttl: Duration,
    lease_state: impl Fn() -> (bool, bool) + Send + 'static,
    on_shutdown: impl FnOnce() + Send + 'static,
) -> anyhow::Result<()>
where
    F: Fn(Handshake, Vec<u8>, SharedSession, CancellationToken, CancellationToken) -> Fut
        + Send
        + Sync
        + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let Some(listener) = bind_shared(&key).await? else {
        tracing::info!("shared backend already serving this workspace");
        return Ok(());
    };
    serve_shared(
        key,
        listener,
        Arc::new(on_session),
        scope_retired,
        stop,
        orphan_grace,
        idle_ttl,
        lease_state,
        on_shutdown,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // Each callback belongs to the upper adapter owner.
async fn serve_shared<L, F, Fut>(
    key: SharedBackendKey,
    listener: L,
    on_session: Arc<F>,
    scope_retired: CancellationToken,
    stop: CancellationToken,
    orphan_grace: Duration,
    idle_ttl: Duration,
    lease_state: impl Fn() -> (bool, bool) + Send + 'static,
    on_shutdown: impl FnOnce() + Send + 'static,
) -> anyhow::Result<()>
where
    L: BrokerListener,
    F: Fn(Handshake, Vec<u8>, SharedSession, CancellationToken, CancellationToken) -> Fut
        + Send
        + Sync
        + 'static,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let active = Arc::new(AtomicU64::new(0));
    let warmed = Arc::new(AtomicBool::new(false));
    let shutdown = CancellationToken::new();
    let _shutdown_guard = shutdown.clone().drop_guard();
    let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_SHARED_HANDSHAKES));
    let mut sessions = tokio::task::JoinSet::new();
    let poll =
        (orphan_grace.min(idle_ttl) / 4).clamp(Duration::from_millis(100), Duration::from_secs(15));
    let mut ticker = interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await;
    let mut idle_since = Some(Instant::now());
    let mut result = Ok(());
    let mut superseded = false;
    let mut reservation: Option<L::Reservation> = None;
    let session_retirement = CancellationToken::new();

    tracing::info!(pid = std::process::id(), "shared broker listening");
    loop {
        tokio::select! {
            biased;
            _ = scope_retired.cancelled() => {
                tracing::info!("shared workspace scope retired; closing broker endpoint");
                break;
            }
            _ = stop.cancelled() => {
                tracing::info!("shared backend stopped; closing broker endpoint");
                break;
            }
            accepted = listener.accept() => {
                let conn = match accepted {
                    Ok(conn) => conn,
                    Err(error) => { result = Err(error.into()); break; }
                };
                if !listener.peer_authorized(&conn) {
                    tracing::warn!("rejected shared broker connection from an unauthorized peer");
                    continue;
                }
                let Ok(handshake_slot) = Arc::clone(&handshakes).try_acquire_owned() else {
                    tracing::warn!("shared broker handshake limit reached");
                    continue;
                };
                let active_guard = ActiveGuard::new(Arc::clone(&active));
                let callback = Arc::clone(&on_session);
                let key = key.clone();
                let shutdown = shutdown.clone();
                let scope_retired_session = session_retirement.clone();
                let warmed = Arc::clone(&warmed);
                sessions.spawn(async move {
                    let _active_guard = active_guard;
                    let mut conn = conn;
                    let handshake = match server_handshake(&mut conn, &key).await {
                        Ok(handshake) => handshake,
                        Err(_) => return,
                    };
                    let context = match read_session_context(&mut conn).await {
                        Ok(context) => context,
                        Err(_) => return,
                    };
                    drop(handshake_slot);
                    let session = match handshake.adapter {
                        AdapterKind::Mcp => {
                            let warmed = Arc::clone(&warmed);
                            SharedSession::Mcp(Box::new(FirstByteProbe::new(conn, move || {
                                warmed.store(true, Ordering::SeqCst)
                            })))
                        }
                        AdapterKind::Lsp => {
                            let warmed = Arc::clone(&warmed);
                            SharedSession::Lsp(LspChannel::new(conn, move || warmed.store(true, Ordering::SeqCst)))
                        }
                    };
                    tokio::select! {
                        _ = shutdown.cancelled() => {},
                        result = callback(handshake, context, session, shutdown.clone(), scope_retired_session) => {
                            if result.is_err() {
                                tracing::warn!("shared broker adapter session ended with error");
                            }
                        }
                    }
                });
            }
            _ = ticker.tick() => {
                while let Some(result) = sessions.try_join_next() {
                    if let Err(error) = result {
                        tracing::warn!(%error, "shared broker session task failed");
                    }
                }
                if lease_state().0 {
                    tracing::info!("shared backend superseded; closing endpoint and waiting for graph reads");
                    superseded = true;
                    break;
                }
                if active.load(Ordering::SeqCst) != 0 {
                    idle_since = None;
                } else {
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    let grace = if warmed.load(Ordering::SeqCst) { idle_ttl } else { orphan_grace };
                    if since.elapsed() >= grace {
                        if let Some(held) = reserve_for_idle_exit(&listener) {
                            reservation = Some(held);
                            break;
                        }
                    }
                }
            }
        }
    }

    let retired = scope_retired.is_cancelled();
    drop(listener);
    drop(reservation);
    // Sessions hear of a retirement only now: their client reconnects at once, and a connection
    // waiting in the backlog of a listener about to close would be reset instead of launching the
    // next backend.
    let announce_retirement = tokio::spawn(async move {
        scope_retired.cancelled().await;
        session_retirement.cancel();
    });
    if superseded {
        // A stop cuts graph reads short, as dying by the signal would, but the owner
        // still shuts down and releases the workspace lease.
        while !lease_state().1 && !stop.is_cancelled() {
            tokio::time::sleep(poll.min(Duration::from_millis(200))).await;
        }
        if !stop.is_cancelled() {
            tokio::time::sleep(SUPERSEDED_DRAIN).await;
        }
    }
    if retired {
        let drain = async { while sessions.join_next().await.is_some() {} };
        if tokio::time::timeout(SUPERSEDED_DRAIN, drain).await.is_err() {
            shutdown.cancel();
            while let Some(result) = sessions.join_next().await {
                if let Err(error) = result {
                    tracing::warn!(%error, "shared broker session task failed during retirement");
                }
            }
        }
    } else {
        shutdown.cancel();
        while let Some(result) = sessions.join_next().await {
            if let Err(error) = result {
                tracing::warn!(%error, "shared broker session task failed during shutdown");
            }
        }
    }
    announce_retirement.abort();
    on_shutdown();
    result
}

async fn serve<L: BrokerListener>(
    server: McpServer,
    listener: L,
    parked: Vec<L::Stream>,
    orphan_grace: Duration,
    idle_ttl: Duration,
) -> anyhow::Result<()> {
    tracing::info!(
        pid = std::process::id(),
        orphan_grace_secs = orphan_grace.as_secs(),
        idle_ttl_secs = idle_ttl.as_secs(),
        parked = parked.len(),
        "broker backend listening"
    );

    // `active` counts in-flight sessions. `warmed` flips once any session sends its first
    // byte of real MCP traffic; from then on the backend uses the long `idle_ttl` after it
    // falls idle, so a reconnecting client reuses the resident state. A no-data liveness
    // probe never flips it, so the short `orphan_grace` still reaps a never-used backend.
    let active = Arc::new(AtomicU64::new(0));
    let warmed = Arc::new(AtomicBool::new(false));
    // Live session tasks, reaped once finished so the vector can't grow without bound
    // across a long-lived backend's many short sessions.
    let mut sessions: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let session_shutdown = CancellationToken::new();
    // Cancellation of the daemon future must also release the sessions it spawned.
    let _session_shutdown_guard = session_shutdown.clone().drop_guard();

    // Serve the connections that arrived during the build first.
    for conn in parked {
        sessions.push(spawn_session(&server, &active, &warmed, conn, session_shutdown.clone()));
    }

    // `idle_since` marks when the current connection-less stretch began; it is cleared
    // whenever a session is active and (re)started when the backend falls idle. Poll at a
    // fraction of the *shorter* grace so the exit lands close to the intended window, not
    // up to 2× late, even when a test drives a tiny TTL.
    let poll =
        (orphan_grace.min(idle_ttl) / 4).clamp(Duration::from_millis(100), Duration::from_secs(15));
    let scope_shutdown = server.scope_transport_stop();
    let mut idle_since = Some(Instant::now());
    let mut superseded = false;
    let mut ticker = interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await; // consume the immediate first tick
    let mut reservation: Option<L::Reservation> = None;

    let result = loop {
        tokio::select! {
            // Poll accept before the idle tick so a connection already waiting is taken in
            // first. That alone does not keep an idle exit from dropping a client: on Windows a
            // client's connect completes on the listener's free pipe instance before accept can
            // see it, so the exit below first reserves that instance. This cannot starve the
            // idle check: accept is only continuously ready while connections keep arriving.
            biased;
            _ = scope_shutdown.cancelled() => {
                tracing::info!("workspace cache scope retired; closing the broker endpoint");
                break Ok(());
            }
            accepted = listener.accept() => {
                let conn = match accepted {
                    Ok(conn) => conn,
                    Err(error) => break Err(error),
                };
                if !listener.peer_authorized(&conn) {
                    tracing::warn!("rejected backend connection from an unauthorized peer");
                    continue;
                }
                sessions.push(spawn_session(&server, &active, &warmed, conn, session_shutdown.clone()));
            }
            _ = ticker.tick() => {
                sessions.retain(|h| !h.is_finished());
                // A newer generation owns this workspace's derived caches, so this backend is
                // terminally unable to maintain them — a transient ownership refusal is not
                // enough. It leaves whether or not sessions are connected: a connected client
                // waiting on it holds the graph file away from the owner that can work.
                if server.superseded() {
                    tracing::info!(
                        sessions = active.load(Ordering::SeqCst),
                        "backend superseded by a newer daemon generation; closing its endpoint \
                         and leaving once its graph reads finish"
                    );
                    superseded = true;
                    break Ok(());
                }
                // Reset the idle clock while any session is connected; otherwise count down
                // against the grace that fits the backend's history — the long `idle_ttl`
                // once it has served real traffic, the short `orphan_grace` before then.
                if active.load(Ordering::SeqCst) != 0 || server.background_work_active() {
                    idle_since = None;
                } else {
                    let grace = if warmed.load(Ordering::SeqCst) { idle_ttl } else { orphan_grace };
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= grace {
                        if let Some(held) = reserve_for_idle_exit(&listener) {
                            tracing::info!(
                                warmed = warmed.load(Ordering::SeqCst),
                                grace_secs = grace.as_secs(),
                                "backend idle past its grace; shutting down"
                            );
                            reservation = Some(held);
                            break Ok(());
                        }
                    }
                }
            }
        }
    };

    // Teardown cascade: stop accepting and sever every still-connected session, so each
    // proxy gets an EOF and exits. Dropping the listener frees the rendezvous name;
    // each RMCP service must finish closing its transport before the daemon returns.
    // An idle exit's reservation outlives the listener, so no client can attach to it while
    // it closes.
    drop(listener);
    drop(reservation);
    if superseded {
        // The name is free for the current owner already. The graph reads in flight finish
        // first — none is cut, however long — and whatever else is being answered gets a
        // moment to reach its client before the sessions close.
        while !server.graph_released() {
            tokio::time::sleep(poll.min(Duration::from_millis(200))).await;
        }
        tokio::time::sleep(SUPERSEDED_DRAIN).await;
    }
    session_shutdown.cancel();
    for handle in sessions {
        if let Err(error) = handle.await {
            tracing::warn!(%error, "broker session task failed during shutdown");
        }
    }

    result.map_err(Into::into)
}

/// Serve one accepted connection on its own task. The [`ActiveGuard`] decrements the
/// active-session count on drop, so a panicking session can never strand the count.
///
/// The connection is wrapped in a [`FirstByteProbe`] that flips `warmed` the first time the
/// peer sends any data, so a session carrying real MCP traffic promotes the backend to the
/// long idle TTL while a no-data liveness probe (connect-then-close) never does. Returns the
/// task handle so the serve loop can await transport cleanup during shutdown.
fn spawn_session<S>(
    server: &McpServer,
    active: &Arc<AtomicU64>,
    warmed: &Arc<AtomicBool>,
    conn: S,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let guard = ActiveGuard::new(Arc::clone(active));
    tracing::debug!(active = active.load(Ordering::SeqCst), "broker accepted a session");
    let session = server.clone();
    let warmed = Arc::clone(warmed);
    tokio::spawn(async move {
        let _guard = guard;
        // First byte on this connection marks the backend as having served real traffic. A
        // liveness probe connects and closes without sending, so it never reaches here.
        let probe = FirstByteProbe::new(conn, move || warmed.store(true, Ordering::SeqCst));
        if let Err(e) = serve_stream_with_shutdown(session, probe, shutdown).await {
            tracing::warn!(error = %e, "broker session ended with error");
        }
    })
}

/// Wraps a backend connection and fires a one-shot callback the first time the peer
/// sends any data. A liveness probe connects and closes without writing, so it never
/// fires — only a real MCP session (which sends `initialize` immediately) does. This is
/// what lets the idle lifetime distinguish a backend that has served real traffic from
/// one that has only been probed.
struct FirstByteProbe<S> {
    inner: S,
    on_first_byte: Option<Box<dyn FnOnce() + Send>>,
}

impl<S> FirstByteProbe<S> {
    fn new(inner: S, on_first_byte: impl FnOnce() + Send + 'static) -> Self {
        Self { inner, on_first_byte: Some(Box::new(on_first_byte)) }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for FirstByteProbe<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            if buf.filled().len() > before {
                if let Some(cb) = self.on_first_byte.take() {
                    cb();
                }
            }
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for FirstByteProbe<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Decrements the active-session count on drop (including unwind), so the accounting
/// stays correct even if a session task panics.
struct ActiveGuard(Arc<AtomicU64>);

impl ActiveGuard {
    fn new(count: Arc<AtomicU64>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The owner loops' view of their endpoint: production serves a bound [`Endpoint`], and the
/// same loops run in tests against a scripted listener.
trait BrokerListener: Send + Sync {
    type Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static;
    /// Held from an idle exit's decision until the listener is gone.
    type Reservation: Send;

    fn accept(&self) -> impl Future<Output = io::Result<Self::Stream>> + Send;
    fn peer_authorized(&self, conn: &Self::Stream) -> bool;
    /// Claim the right to close the listener for idleness. Called only after accept was found
    /// pending, so a granted reservation means no client is waiting and none can still attach.
    fn reserve_idle_exit(&self) -> IdleReservation<Self::Reservation>;
}

// Only the Windows reservation can be refused: a Unix production build grants every idle exit.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Debug)]
enum IdleReservation<G> {
    Held(G),
    /// A client is attached and accept takes it next.
    Busy,
    /// Nothing proves the endpoint is empty.
    Failed(io::Error),
}

/// `None` keeps the loop serving with its idle clock as it is, so a later tick asks again.
fn reserve_for_idle_exit<L: BrokerListener>(listener: &L) -> Option<L::Reservation> {
    match listener.reserve_idle_exit() {
        IdleReservation::Held(held) => Some(held),
        IdleReservation::Busy => {
            tracing::debug!("a client attached as the backend idled out; serving it");
            None
        }
        IdleReservation::Failed(error) => {
            tracing::warn!(%error, "could not reserve the endpoint for an idle exit; still serving");
            None
        }
    }
}

/// A bound rendezvous.
struct Endpoint {
    listener: TokioListener,
    #[cfg(windows)]
    pipe_path: String,
}

#[cfg(windows)]
type PipeReservation = interprocess::os::windows::named_pipe::DuplexPipeStream<
    interprocess::os::windows::named_pipe::pipe_mode::Bytes,
>;

impl BrokerListener for Endpoint {
    type Stream = TokioStream;
    #[cfg(windows)]
    type Reservation = PipeReservation;
    // A Unix client's connect lands in the backlog, which closes with the listener; the
    // backlog is drained by the accept polled just before.
    #[cfg(not(windows))]
    type Reservation = ();

    fn accept(&self) -> impl Future<Output = io::Result<TokioStream>> + Send {
        self.listener.accept()
    }

    fn peer_authorized(&self, conn: &TokioStream) -> bool {
        peer_authorized(conn)
    }

    fn reserve_idle_exit(&self) -> IdleReservation<Self::Reservation> {
        #[cfg(windows)]
        {
            reserve_pipe_instance(&self.pipe_path)
        }
        #[cfg(not(windows))]
        {
            IdleReservation::Held(())
        }
    }
}

/// Connect once, without waiting, to the listener's one free pipe instance. A client that
/// attached to it first makes this busy; otherwise the instance stays ours, and no client can
/// complete a connect to the listener, until the reservation is dropped. Accept is not
/// called again, so the listener never creates another free instance.
#[cfg(windows)]
fn reserve_pipe_instance(path: &str) -> IdleReservation<PipeReservation> {
    match PipeReservation::connect_by_path_with_wait_mode(
        path,
        interprocess::ConnectWaitMode::Timeout(Duration::ZERO),
    ) {
        Ok(stream) => {
            let peer = stream.peer_process_id();
            own_instance(stream, peer)
        }
        // A zero wait makes exactly one attempt, and interprocess reports a busy pipe as this.
        Err(error) if error.kind() == io::ErrorKind::TimedOut => IdleReservation::Busy,
        Err(error) => IdleReservation::Failed(error),
    }
}

/// Only an instance this process serves reserves its listener: a connection to another
/// server of the same name says nothing about ours.
#[cfg(any(windows, test))]
fn own_instance<S>(stream: S, peer_pid: io::Result<u32>) -> IdleReservation<S> {
    match peer_pid {
        Ok(pid) if pid == std::process::id() => IdleReservation::Held(stream),
        Ok(pid) => IdleReservation::Failed(io::Error::other(format!(
            "the endpoint's free instance is served by process {pid}"
        ))),
        Err(error) => IdleReservation::Failed(error),
    }
}

/// Bind the rendezvous, winner-takes-all.
///
/// - `Ok(Some(listener))` — we own the name and should serve.
/// - `Ok(None)` — another live backend already owns it; defer to it.
/// - `Err(..)` — a real failure.
///
/// When the name is already taken we probe with a connect: a successful connect means
/// a live owner (defer); otherwise the name is stale. On unix that stale name is a
/// leftover socket file from a crashed backend, which we reclaim and rebind once (and if
/// a concurrent cold-starter beats us to the rebind, we defer to it). On Windows the
/// named pipe instance vanishes with its owner, so a stale name just means the pipe is
/// already gone and we rebind directly.
async fn bind(key: &BackendKey) -> anyhow::Result<Option<Endpoint>> {
    let endpoint = |listener| Endpoint {
        listener,
        #[cfg(windows)]
        pipe_path: crate::broker::name::backend_pipe_path(key),
    };
    Ok(bind_listener(key).await?.map(endpoint))
}

async fn bind_listener(key: &BackendKey) -> anyhow::Result<Option<TokioListener>> {
    match listener_options(key)?.create_tokio() {
        Ok(listener) => Ok(Some(listener)),
        Err(e) if is_name_in_use(&e) => {
            if probe_live(key).await? {
                return Ok(None);
            }
            #[cfg(unix)]
            {
                let path = key.socket_path()?;
                tracing::info!(path = %path.display(), "reclaiming stale backend socket");
                let _ = std::fs::remove_file(&path);
                match listener_options(key)?.create_tokio() {
                    Ok(listener) => Ok(Some(listener)),
                    Err(e2) if is_name_in_use(&e2) => {
                        // A concurrent cold-starter rebound first; defer to it.
                        if probe_live(key).await? {
                            Ok(None)
                        } else {
                            Err(e2.into())
                        }
                    }
                    Err(e2) => Err(e2.into()),
                }
            }
            #[cfg(not(unix))]
            {
                // No file to unlink: a non-live probe means the previous pipe owner is
                // gone, so the name is free. Rebind once; if a concurrent starter won the
                // race, defer to it rather than erroring.
                match listener_options(key)?.create_tokio() {
                    Ok(listener) => Ok(Some(listener)),
                    Err(e2) if is_name_in_use(&e2) => {
                        if probe_live(key).await? {
                            Ok(None)
                        } else {
                            Err(e2.into())
                        }
                    }
                    Err(e2) => Err(e2.into()),
                }
            }
        }
        Err(e) => Err(e.into()),
    }
}

async fn bind_shared(key: &SharedBackendKey) -> anyhow::Result<Option<Endpoint>> {
    let endpoint = |listener| Endpoint {
        listener,
        #[cfg(windows)]
        pipe_path: crate::broker::name::shared_backend_pipe_path(key),
    };
    Ok(bind_shared_listener(key).await?.map(endpoint))
}

async fn bind_shared_listener(key: &SharedBackendKey) -> anyhow::Result<Option<TokioListener>> {
    match listener_options_shared(key)?.create_tokio() {
        Ok(listener) => Ok(Some(listener)),
        Err(error) if is_name_in_use(&error) => {
            if probe_shared_live(key).await? {
                return Ok(None);
            }
            #[cfg(unix)]
            {
                let path = key.socket_path()?;
                let _ = std::fs::remove_file(path);
            }
            match listener_options_shared(key)?.create_tokio() {
                Ok(listener) => Ok(Some(listener)),
                Err(error) if is_name_in_use(&error) && probe_shared_live(key).await? => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn listener_options_shared(key: &SharedBackendKey) -> io::Result<ListenerOptions<'static>> {
    let options = ListenerOptions::new().name(shared_backend_name(key)?);
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        Ok(options.security_descriptor(pipe_security_descriptor_for_current_user()?))
    }
    #[cfg(not(windows))]
    {
        Ok(options)
    }
}

async fn probe_shared_live(key: &SharedBackendKey) -> anyhow::Result<bool> {
    match connect_endpoint(shared_backend_name(key)?, HANDSHAKE_TIMEOUT).await {
        Ok(stream) => {
            #[cfg(windows)]
            {
                let trusted = crate::broker::security::verify_pipe_server_trusted(&stream);
                drop(stream);
                Ok(trusted)
            }
            #[cfg(not(windows))]
            {
                drop(stream);
                Ok(true)
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Ok(false)
        }
        Err(error) => {
            #[cfg(windows)]
            {
                if windows_pipe_busy_or_timeout(&error) {
                    return Ok(true);
                }
                const ERROR_ACCESS_DENIED: i32 = 5;
                if error.kind() == io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(ERROR_ACCESS_DENIED)
                {
                    return Ok(true);
                }
            }
            Err(error.into())
        }
    }
}

fn listener_options(key: &BackendKey) -> io::Result<ListenerOptions<'static>> {
    let options = ListenerOptions::new().name(backend_name(key)?);
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;

        Ok(options.security_descriptor(pipe_security_descriptor_for_current_user()?))
    }
    #[cfg(not(windows))]
    {
        Ok(options)
    }
}

/// Whether a failed bind means the rendezvous name is already taken (so we should probe
/// and defer/reclaim rather than error out). Unix reports `AddrInUse`; Windows fails the
/// `CreateNamedPipe` of an already-existing instance with `ERROR_ACCESS_DENIED` (5)
/// instead, so we map that to the same "name in use" decision.
fn is_name_in_use(e: &std::io::Error) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;

    e.kind() == std::io::ErrorKind::AddrInUse
        || (cfg!(windows) && e.raw_os_error() == Some(ERROR_ACCESS_DENIED))
}

/// Is a backend actually accepting on this name? A successful connect proves a live
/// listener (queued in its backlog even mid-build); a refused/not-found connect
/// means the name is stale. On Windows, a finite busy/timeout result is inconclusive:
/// keep the name reserved and defer rather than risk rebinding an existing pipe.
///
/// On Windows the connect succeeding does not by itself prove the listener is a
/// backend we trust: the pipe name is deterministic and a hostile local user
/// can pre-create it with their own DACL. After a successful connect we
/// therefore call `security::verify_pipe_server_trusted`, which checks the
/// server PID's image path and owner SID via `sysinfo` (no project-local
/// unsafe, no handwritten Win32 FFI). A trusted live backend returns `true`
/// (defer to it); an unverified pipe returns `false` so the caller attempts a
/// fresh rebind — and if that rebind loses too (the squatter still holds the
/// name) `bind` returns Err and the launching proxy falls back to in-process
/// stdio.
async fn probe_live(key: &BackendKey) -> anyhow::Result<bool> {
    match connect_endpoint(backend_name(key)?, HANDSHAKE_TIMEOUT).await {
        Ok(control) => {
            // The probe connection is never used to exchange MCP bytes; it is
            // just a liveness + identity check. Drop it deterministically.
            #[cfg(windows)]
            {
                let trusted = crate::broker::security::verify_pipe_server_trusted(&control);
                drop(control);
                if trusted {
                    Ok(true)
                } else {
                    tracing::info!(
                        "windows named pipe held by an unverified server; treating as unavailable"
                    );
                    Ok(false)
                }
            }
            #[cfg(not(windows))]
            {
                drop(control);
                Ok(true)
            }
        }
        // Only a clearly-absent listener counts as stale. Any other (transient) connect
        // error is treated as live, so we never unlink+rebind a backend that is actually
        // up — the conservative choice for the reclaim decision.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            Ok(false)
        }
        Err(e) => {
            #[cfg(windows)]
            {
                if windows_pipe_busy_or_timeout(&e) {
                    return Ok(true);
                }
                const ERROR_ACCESS_DENIED: i32 = 5;
                if e.kind() == std::io::ErrorKind::PermissionDenied
                    || e.raw_os_error() == Some(ERROR_ACCESS_DENIED)
                {
                    tracing::info!(
                        error = %e,
                        "windows named-pipe probe was denied; treating existing server as untrusted"
                    );
                    return Ok(false);
                }
            }
            tracing::warn!(error = %e, "liveness probe inconclusive; assuming the backend is live");
            Ok(true)
        }
    }
}

#[cfg(windows)]
fn windows_pipe_busy_or_timeout(error: &io::Error) -> bool {
    const ERROR_SEM_TIMEOUT: i32 = 121;
    const ERROR_PIPE_BUSY: i32 = 231;
    matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
        || matches!(error.raw_os_error(), Some(ERROR_SEM_TIMEOUT) | Some(ERROR_PIPE_BUSY))
}

/// Reject a peer running as a different user. On unix this reads `SO_PEERCRED`; the
/// runtime dir is already 0700/owned-by-us, so this is defense in depth (and covers
/// the abstract-namespace case where there is no socket file to permission). When
/// the platform cannot report a euid we allow and rely on the directory/pipe ACL.
#[cfg(unix)]
fn peer_authorized(conn: &TokioStream) -> bool {
    match conn.peer_creds() {
        Ok(creds) => {
            creds.euid().map(|uid| uid == crate::broker::name::current_euid()).unwrap_or(true)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not read peer credentials; relying on dir/pipe ACL");
            true
        }
    }
}

/// Non-unix transports do not provide peer credentials. On Windows the named-pipe
/// listener is created with an explicit current-user-only security descriptor.
#[cfg(not(unix))]
fn peer_authorized(_conn: &TokioStream) -> bool {
    true
}

/// The idle exits of both owner loops, driven through a scripted listener whose one free
/// instance behaves as a Windows named pipe keeps it: a client's connect completes on it before
/// accept hands it out, and a held reservation keeps every later connect from completing.
#[cfg(test)]
mod idle_boundary_tests {
    use super::*;
    use crate::broker::transport::{client_handshake, write_session_context, AdapterKind};
    use crate::broker::{SharedBackendIdentity, SharedBackendKey};
    use crate::{McpProfile, SharedState};
    use rmcp::service::RunningService;
    use rmcp::{RoleClient, ServiceExt};
    use std::collections::VecDeque;
    use tokio::io::DuplexStream;
    use tokio::sync::watch;

    const GRACE: Duration = Duration::from_secs(1);
    /// The loops tick at a quarter of the shorter grace.
    const POLL: Duration = Duration::from_millis(250);

    type Client = RunningService<RoleClient, ()>;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        AcceptPending,
        ClientAttached,
        ClientRefused,
        Accepted,
        ReserveBusy,
        ReserveFailed,
        Reserved,
        ListenerDropped,
        ReservationDropped,
        SessionRetired,
    }

    enum Slot {
        Free,
        /// A client's connect completed; the server half waits for accept.
        Attached(DuplexStream),
        Reserved,
        Closed,
    }

    /// What the next reservation answers instead of the instance's own state.
    enum Refusal {
        Busy,
        Error,
        ForeignProcess,
    }

    struct Pipe {
        slot: Slot,
        accept_open: bool,
        refusals: VecDeque<Refusal>,
        attach_as_listener_closes: bool,
        events: Vec<(Event, Instant)>,
    }

    impl Pipe {
        fn log(&mut self, event: Event) {
            self.events.push((event, Instant::now()));
        }

        fn attach(&mut self) -> io::Result<DuplexStream> {
            if !matches!(self.slot, Slot::Free) {
                self.log(Event::ClientRefused);
                return Err(io::Error::new(io::ErrorKind::ConnectionRefused, "no free instance"));
            }
            let (client, server) = tokio::io::duplex(1 << 20);
            self.slot = Slot::Attached(server);
            self.log(Event::ClientAttached);
            Ok(client)
        }
    }

    #[derive(Clone)]
    struct FakePipe(Arc<watch::Sender<Pipe>>);

    impl FakePipe {
        fn new() -> Self {
            Self(Arc::new(watch::Sender::new(Pipe {
                slot: Slot::Free,
                accept_open: false,
                refusals: VecDeque::new(),
                attach_as_listener_closes: false,
                events: Vec::new(),
            })))
        }

        fn listener(&self) -> FakeListener {
            FakeListener(self.clone())
        }

        fn connect(&self) -> io::Result<DuplexStream> {
            let mut attached = None;
            self.0.send_modify(|pipe| attached = Some(pipe.attach()));
            attached.expect("send_modify runs its closure")
        }

        fn open_accept(&self) {
            self.0.send_modify(|pipe| pipe.accept_open = true);
        }

        fn refuse(&self, refusals: impl IntoIterator<Item = Refusal>) {
            self.0.send_modify(|pipe| pipe.refusals.extend(refusals));
        }

        fn attach_as_listener_closes(&self) {
            self.0.send_modify(|pipe| pipe.attach_as_listener_closes = true);
        }

        fn log(&self, event: Event) {
            self.0.send_modify(|pipe| pipe.log(event));
        }

        fn events(&self) -> Vec<Event> {
            self.0.borrow().events.iter().map(|(event, _)| *event).collect()
        }

        fn count(&self, wanted: &[Event]) -> usize {
            self.0.borrow().events.iter().filter(|(event, _)| wanted.contains(event)).count()
        }

        fn times_of(&self, wanted: &[Event]) -> Vec<Instant> {
            let pipe = self.0.borrow();
            pipe.events
                .iter()
                .filter(|(event, _)| wanted.contains(event))
                .map(|(_, at)| *at)
                .collect()
        }

        /// Wait until one of `wanted` has happened `count` times in all.
        async fn wait_for(&self, wanted: &[Event], count: usize) {
            let mut events = self.0.subscribe();
            tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(60)),
                events.wait_for(|pipe| {
                    pipe.events.iter().filter(|(event, _)| wanted.contains(event)).count() >= count
                }),
            )
            .await
            .unwrap_or_else(|_| panic!("{wanted:?} never happened; events: {:?}", self.events()))
            .expect("the pipe outlives its observers");
        }
    }

    struct FakeListener(FakePipe);

    impl BrokerListener for FakeListener {
        type Stream = DuplexStream;
        type Reservation = FakeReservation;

        fn accept(&self) -> impl Future<Output = io::Result<DuplexStream>> + Send {
            let pipe = self.0.clone();
            async move {
                let mut changes = pipe.0.subscribe();
                let mut announced = false;
                loop {
                    changes.borrow_and_update();
                    let mut taken = None;
                    pipe.0.send_if_modified(|state| {
                        if state.accept_open && matches!(state.slot, Slot::Attached(_)) {
                            if let Slot::Attached(server) =
                                std::mem::replace(&mut state.slot, Slot::Free)
                            {
                                taken = Some(server);
                            }
                            state.log(Event::Accepted);
                            return true;
                        }
                        if announced {
                            return false;
                        }
                        state.log(Event::AcceptPending);
                        true
                    });
                    if let Some(server) = taken {
                        return Ok(server);
                    }
                    announced = true;
                    if changes.changed().await.is_err() {
                        return Err(io::ErrorKind::BrokenPipe.into());
                    }
                }
            }
        }

        fn peer_authorized(&self, _conn: &DuplexStream) -> bool {
            true
        }

        fn reserve_idle_exit(&self) -> IdleReservation<FakeReservation> {
            let mut answer = None;
            self.0 .0.send_modify(|pipe| {
                let decision = match pipe.refusals.pop_front() {
                    Some(Refusal::Busy) => IdleReservation::Busy,
                    Some(Refusal::Error) => {
                        IdleReservation::Failed(io::Error::other("probe failed"))
                    }
                    Some(Refusal::ForeignProcess) => {
                        own_instance((), Ok(std::process::id().wrapping_add(1)))
                    }
                    None => match pipe.slot {
                        Slot::Free => {
                            pipe.slot = Slot::Reserved;
                            IdleReservation::Held(())
                        }
                        Slot::Attached(_) => IdleReservation::Busy,
                        Slot::Reserved | Slot::Closed => {
                            IdleReservation::Failed(io::Error::other("no instance to reserve"))
                        }
                    },
                };
                pipe.log(match decision {
                    IdleReservation::Held(()) => Event::Reserved,
                    IdleReservation::Busy => Event::ReserveBusy,
                    IdleReservation::Failed(_) => Event::ReserveFailed,
                });
                answer = Some(match decision {
                    IdleReservation::Held(()) => {
                        IdleReservation::Held(FakeReservation(self.0.clone()))
                    }
                    IdleReservation::Busy => IdleReservation::Busy,
                    IdleReservation::Failed(error) => IdleReservation::Failed(error),
                });
            });
            answer.expect("send_modify runs its closure")
        }
    }

    impl Drop for FakeListener {
        fn drop(&mut self) {
            self.0 .0.send_modify(|pipe| {
                if pipe.attach_as_listener_closes {
                    let _ = pipe.attach();
                }
                // An attached instance nobody accepted closes with the listener: its client
                // reads EOF.
                pipe.slot = Slot::Closed;
                pipe.log(Event::ListenerDropped);
            });
        }
    }

    struct FakeReservation(FakePipe);

    impl Drop for FakeReservation {
        fn drop(&mut self) {
            self.0.log(Event::ReservationDropped);
        }
    }

    fn reference_server() -> McpServer {
        McpServer::new(McpProfile::Reference, SharedState::shared())
    }

    fn spawn_serve(
        pipe: &FakePipe,
        orphan_grace: Duration,
        idle_ttl: Duration,
    ) -> tokio::task::JoinHandle<anyhow::Result<()>> {
        tokio::spawn(serve(reference_server(), pipe.listener(), Vec::new(), orphan_grace, idle_ttl))
    }

    struct Shared {
        _source: tempfile::TempDir,
        identity: SharedBackendIdentity,
        retired: CancellationToken,
        stop: CancellationToken,
        sessions: Arc<AtomicU64>,
        daemon: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    fn spawn_serve_shared(pipe: &FakePipe, orphan_grace: Duration, idle_ttl: Duration) -> Shared {
        spawn_serve_shared_leased(pipe, orphan_grace, idle_ttl, || (false, false))
    }

    /// The shared loop with an adapter that serves MCP and records when its session hears of
    /// a retirement. `lease_state` answers as the workspace lease does: superseded, and its
    /// graph reads released.
    fn spawn_serve_shared_leased(
        pipe: &FakePipe,
        orphan_grace: Duration,
        idle_ttl: Duration,
        lease_state: impl Fn() -> (bool, bool) + Send + 'static,
    ) -> Shared {
        let source = tempfile::tempdir().unwrap();
        let identity = SharedBackendIdentity::new(
            source.path(),
            source.path().join("cache"),
            [1; 32],
            [2; 32],
            [3; 32],
            [4; 32],
        );
        let retired = CancellationToken::new();
        let stop = CancellationToken::new();
        let sessions = Arc::new(AtomicU64::new(0));
        let (adapter_pipe, adapter_sessions) = (pipe.clone(), Arc::clone(&sessions));
        let daemon = tokio::spawn(serve_shared(
            SharedBackendKey::new(identity.clone()),
            pipe.listener(),
            Arc::new(move |_handshake, _context, session, shutdown, retired: CancellationToken| {
                adapter_sessions.fetch_add(1, Ordering::SeqCst);
                let pipe = adapter_pipe.clone();
                async move {
                    let SharedSession::Mcp(stream) = session else {
                        anyhow::bail!("MCP adapter expected")
                    };
                    tokio::select! {
                        served = serve_stream_with_shutdown(reference_server(), stream, shutdown) => served,
                        _ = retired.cancelled() => {
                            pipe.log(Event::SessionRetired);
                            Ok(())
                        }
                    }
                }
            }),
            retired.clone(),
            stop.clone(),
            orphan_grace,
            idle_ttl,
            lease_state,
            || {},
        ));
        Shared { _source: source, identity, retired, stop, sessions, daemon }
    }

    async fn initialize(stream: DuplexStream) -> anyhow::Result<Client> {
        Ok(().serve(stream).await?)
    }

    async fn initialize_shared(
        mut stream: DuplexStream,
        identity: &SharedBackendIdentity,
    ) -> anyhow::Result<Client> {
        client_handshake(
            &mut stream,
            &Handshake { identity: identity.clone(), adapter: AdapterKind::Mcp },
        )
        .await?;
        write_session_context(&mut stream, &[]).await?;
        initialize(stream).await
    }

    fn assert_served(client: &anyhow::Result<Client>) {
        assert!(
            client.as_ref().is_ok_and(|client| client.peer_info().is_some()),
            "a client whose connect succeeded must be served: {:?}",
            client.as_ref().err()
        );
    }

    async fn join(daemon: tokio::task::JoinHandle<anyhow::Result<()>>) {
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(60)), daemon)
            .await
            .expect("the daemon leaves")
            .expect("the daemon task joins")
            .expect("the daemon exits cleanly");
    }

    /// The client attaches after accept was found pending and before the expired grace is
    /// acted on, and accept learns of it only after that decision.
    async fn attach_as_the_grace_runs_out(pipe: &FakePipe) -> DuplexStream {
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let client = pipe.connect().expect("the free instance takes the client");
        tokio::time::advance(GRACE).await;
        pipe.wait_for(&[Event::ReserveBusy, Event::ListenerDropped], 1).await;
        pipe.open_accept();
        client
    }

    /// The tail every idle exit must have: the reservation was held when the listener closed.
    fn assert_reserved_exit(pipe: &FakePipe) {
        let events = pipe.events();
        let tail: Vec<_> = events
            .iter()
            .copied()
            .filter(|event| {
                matches!(
                    event,
                    Event::Reserved | Event::ListenerDropped | Event::ReservationDropped
                )
            })
            .collect();
        assert_eq!(
            tail,
            [Event::Reserved, Event::ListenerDropped, Event::ReservationDropped],
            "events: {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn serve_serves_a_client_that_attached_as_the_grace_ran_out() {
        let pipe = FakePipe::new();
        let daemon = spawn_serve(&pipe, GRACE, GRACE);
        let stream = attach_as_the_grace_runs_out(&pipe).await;
        let client = initialize(stream).await;
        assert_served(&client);
        client.unwrap().cancel().await.unwrap();
        join(daemon).await;
        assert_reserved_exit(&pipe);
    }

    #[tokio::test(start_paused = true)]
    async fn serve_shared_serves_a_client_that_attached_as_the_grace_ran_out() {
        let pipe = FakePipe::new();
        let shared = spawn_serve_shared(&pipe, GRACE, GRACE);
        let stream = attach_as_the_grace_runs_out(&pipe).await;
        let client = initialize_shared(stream, &shared.identity).await;
        assert_served(&client);
        client.unwrap().cancel().await.unwrap();
        join(shared.daemon).await;
        assert_reserved_exit(&pipe);
    }

    /// Nothing reaches the loop between the reservation and the listener closing: a connect
    /// made as it closes is refused, no second accept runs and the reservation never reaches
    /// a session.
    fn assert_no_client_reached_the_closing_listener(pipe: &FakePipe) {
        let events = pipe.events();
        let served: Vec<_> =
            events.iter().copied().filter(|event| *event != Event::AcceptPending).collect();
        assert_eq!(
            served,
            [
                Event::Reserved,
                Event::ClientRefused,
                Event::ListenerDropped,
                Event::ReservationDropped
            ],
            "events: {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn serve_holds_its_free_instance_until_the_listener_closes() {
        let pipe = FakePipe::new();
        pipe.attach_as_listener_closes();
        join(spawn_serve(&pipe, GRACE, GRACE)).await;
        assert_no_client_reached_the_closing_listener(&pipe);
    }

    #[tokio::test(start_paused = true)]
    async fn serve_shared_holds_its_free_instance_until_the_listener_closes() {
        let pipe = FakePipe::new();
        pipe.attach_as_listener_closes();
        let shared = spawn_serve_shared(&pipe, GRACE, GRACE);
        join(shared.daemon).await;
        assert_no_client_reached_the_closing_listener(&pipe);
        assert_eq!(shared.sessions.load(Ordering::SeqCst), 0, "the reservation is no session");
    }

    /// A refused reservation leaves the loop serving with its idle state as it was: it asks
    /// again on the next tick, not after the grace anew, and not after the longer orphan grace
    /// of a backend that never served. A client is then served, and the backend still idles out.
    async fn refused_reservations_keep_serving(
        pipe: &FakePipe,
        connect: impl AsyncFn(DuplexStream) -> anyhow::Result<Client>,
    ) {
        pipe.open_accept();
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let warm = connect(pipe.connect().unwrap()).await;
        assert_served(&warm);
        warm.unwrap().cancel().await.unwrap();

        pipe.refuse([Refusal::Busy, Refusal::Error, Refusal::ForeignProcess]);
        let refusals = [Event::ReserveBusy, Event::ReserveFailed];
        pipe.wait_for(&refusals, 3).await;
        let asked = pipe.times_of(&refusals);
        assert_eq!(
            asked.windows(2).map(|pair| pair[1] - pair[0]).collect::<Vec<_>>(),
            [POLL, POLL],
            "events: {:?}",
            pipe.events()
        );

        let client = connect(pipe.connect().expect("the listener still serves")).await;
        assert_served(&client);
        client.unwrap().cancel().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn serve_keeps_serving_when_the_reservation_is_refused() {
        let pipe = FakePipe::new();
        let daemon = spawn_serve(&pipe, 4 * GRACE, GRACE);
        refused_reservations_keep_serving(&pipe, initialize).await;
        join(daemon).await;
        assert_reserved_exit(&pipe);
    }

    #[tokio::test(start_paused = true)]
    async fn serve_shared_keeps_serving_when_the_reservation_is_refused() {
        let pipe = FakePipe::new();
        let shared = spawn_serve_shared(&pipe, 4 * GRACE, GRACE);
        let identity = shared.identity.clone();
        refused_reservations_keep_serving(&pipe, async |stream| {
            initialize_shared(stream, &identity).await
        })
        .await;
        join(shared.daemon).await;
        assert_reserved_exit(&pipe);
    }

    /// A retirement closes the endpoint before any session hears of it, and is no idle exit.
    #[tokio::test(start_paused = true)]
    async fn serve_shared_retirement_closes_the_listener_before_sessions_hear_of_it() {
        let pipe = FakePipe::new();
        let long = Duration::from_secs(60);
        let shared = spawn_serve_shared(&pipe, long, long);
        pipe.open_accept();
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let client = initialize_shared(pipe.connect().unwrap(), &shared.identity).await;
        assert_served(&client);
        shared.retired.cancel();
        join(shared.daemon).await;
        let events = pipe.events();
        let order: Vec<_> = events
            .iter()
            .copied()
            .filter(|event| {
                matches!(
                    event,
                    Event::ListenerDropped
                        | Event::SessionRetired
                        | Event::Reserved
                        | Event::ReserveBusy
                        | Event::ReserveFailed
                )
            })
            .collect();
        assert_eq!(order, [Event::ListenerDropped, Event::SessionRetired], "events: {events:?}");
    }

    const ASKED_TO_RESERVE: [Event; 3] =
        [Event::Reserved, Event::ReserveBusy, Event::ReserveFailed];

    /// Loop turns, a tick of virtual time each, that outlast the grace three times over.
    const TURNS_PAST_THE_GRACE: usize = 12;

    /// A grace no run reaches: only the shorter orphan grace can end a backend that has this
    /// idle TTL.
    const NEVER: Duration = Duration::from_secs(24 * 60 * 60);

    /// Let the owner loop turn `turns` more times. Every turn polls a fresh accept, which the
    /// pipe records, so the wait ends on the loop having acted on that many ticks.
    async fn let_the_loop_turn(pipe: &FakePipe, turns: usize) {
        let seen = pipe.count(&[Event::AcceptPending]);
        pipe.wait_for(&[Event::AcceptPending], seen + turns).await;
    }

    /// What tells a forced exit from an idle one: the listener closed and nobody asked to
    /// reserve its instance.
    fn forced_exit(pipe: &FakePipe) -> Vec<Event> {
        pipe.events()
            .into_iter()
            .filter(|event| *event == Event::ListenerDropped || ASKED_TO_RESERVE.contains(event))
            .collect()
    }

    /// A connected session holds the backend through every grace that passes, and the idle exit
    /// comes only once it has left.
    async fn a_connected_session_holds_the_backend(
        pipe: &FakePipe,
        connect: impl AsyncFn(DuplexStream) -> anyhow::Result<Client>,
    ) {
        pipe.open_accept();
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let client = connect(pipe.connect().unwrap()).await;
        assert_served(&client);
        let client = client.unwrap();
        let_the_loop_turn(pipe, TURNS_PAST_THE_GRACE).await;
        assert_eq!(pipe.count(&ASKED_TO_RESERVE), 0, "events: {:?}", pipe.events());
        client.cancel().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn serve_holds_the_backend_while_a_session_is_connected() {
        let pipe = FakePipe::new();
        let daemon = spawn_serve(&pipe, GRACE, GRACE);
        a_connected_session_holds_the_backend(&pipe, initialize).await;
        join(daemon).await;
        assert_reserved_exit(&pipe);
    }

    #[tokio::test(start_paused = true)]
    async fn serve_shared_holds_the_backend_while_a_session_is_connected() {
        let pipe = FakePipe::new();
        let shared = spawn_serve_shared(&pipe, GRACE, GRACE);
        let identity = shared.identity.clone();
        a_connected_session_holds_the_backend(&pipe, async |stream| {
            initialize_shared(stream, &identity).await
        })
        .await;
        join(shared.daemon).await;
        assert_reserved_exit(&pipe);
    }

    /// Background work the state accounts for holds the backend without any session, and the
    /// idle exit comes once the work is done.
    #[tokio::test(start_paused = true)]
    async fn serve_holds_the_backend_while_background_work_runs() {
        let pipe = FakePipe::new();
        let state = SharedState::shared();
        let mut pass = state.index_progress().begin_pass();
        let daemon = tokio::spawn(serve(
            McpServer::new(McpProfile::Reference, state.clone()),
            pipe.listener(),
            Vec::new(),
            GRACE,
            GRACE,
        ));
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let_the_loop_turn(&pipe, TURNS_PAST_THE_GRACE).await;
        assert_eq!(pipe.count(&ASKED_TO_RESERVE), 0, "events: {:?}", pipe.events());
        pass.finish(bsl_search::IndexPassState::Ready);
        join(daemon).await;
        assert_reserved_exit(&pipe);
    }

    /// A connection that sends no payload leaves the backend unwarmed, so the orphan grace
    /// ends it even though its idle TTL is out of reach.
    #[tokio::test(start_paused = true)]
    async fn serve_is_not_warmed_by_a_connection_that_sends_nothing() {
        let pipe = FakePipe::new();
        let daemon = spawn_serve(&pipe, GRACE, NEVER);
        pipe.open_accept();
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        drop(pipe.connect().unwrap());
        pipe.wait_for(&[Event::Accepted], 1).await;
        join(daemon).await;
        assert_reserved_exit(&pipe);
    }

    /// A private handshake that no MCP payload follows is a probe, and warms nothing.
    #[tokio::test(start_paused = true)]
    async fn serve_shared_is_not_warmed_by_a_probe_that_sends_nothing() {
        let pipe = FakePipe::new();
        let shared = spawn_serve_shared(&pipe, GRACE, NEVER);
        pipe.open_accept();
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let mut probe = pipe.connect().unwrap();
        client_handshake(
            &mut probe,
            &Handshake { identity: shared.identity.clone(), adapter: AdapterKind::Mcp },
        )
        .await
        .unwrap();
        write_session_context(&mut probe, &[]).await.unwrap();
        drop(probe);
        join(shared.daemon).await;
        assert_reserved_exit(&pipe);
    }

    /// A retired workspace scope closes the endpoint with a client still connected, and no
    /// idle reservation is asked for.
    #[tokio::test(start_paused = true)]
    async fn serve_scope_retirement_closes_the_listener_without_reserving() {
        let pipe = FakePipe::new();
        let state = SharedState::shared();
        let long = Duration::from_secs(60);
        let daemon = tokio::spawn(serve(
            McpServer::new(McpProfile::Reference, state.clone()),
            pipe.listener(),
            Vec::new(),
            long,
            long,
        ));
        pipe.open_accept();
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let client = initialize(pipe.connect().unwrap()).await;
        assert_served(&client);
        state.scope_transport_stop().cancel();
        join(daemon).await;
        assert_eq!(forced_exit(&pipe), [Event::ListenerDropped], "events: {:?}", pipe.events());
    }

    #[tokio::test(start_paused = true)]
    async fn serve_supersession_closes_the_listener_without_reserving() {
        let pipe = FakePipe::new();
        let state = SharedState::shared();
        let long = Duration::from_secs(60);
        let daemon = tokio::spawn(serve(
            McpServer::new(McpProfile::Reference, state.clone()),
            pipe.listener(),
            Vec::new(),
            long,
            long,
        ));
        pipe.open_accept();
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let client = initialize(pipe.connect().unwrap()).await;
        assert_served(&client);
        state.supersede_for_test();
        join(daemon).await;
        assert_eq!(forced_exit(&pipe), [Event::ListenerDropped], "events: {:?}", pipe.events());
    }

    async fn serve_shared_with_a_session(pipe: &FakePipe, shared: &Shared) -> Client {
        pipe.open_accept();
        pipe.wait_for(&[Event::AcceptPending], 1).await;
        let client = initialize_shared(pipe.connect().unwrap(), &shared.identity).await;
        assert_served(&client);
        client.unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn serve_shared_stop_closes_the_listener_without_reserving() {
        let pipe = FakePipe::new();
        let long = Duration::from_secs(60);
        let shared = spawn_serve_shared(&pipe, long, long);
        let _client = serve_shared_with_a_session(&pipe, &shared).await;
        shared.stop.cancel();
        join(shared.daemon).await;
        assert_eq!(forced_exit(&pipe), [Event::ListenerDropped], "events: {:?}", pipe.events());
    }

    #[tokio::test(start_paused = true)]
    async fn serve_shared_supersession_closes_the_listener_without_reserving() {
        let pipe = FakePipe::new();
        let long = Duration::from_secs(60);
        let superseded = Arc::new(AtomicBool::new(false));
        let lease = Arc::clone(&superseded);
        let shared = spawn_serve_shared_leased(&pipe, long, long, move || {
            (lease.load(Ordering::SeqCst), true)
        });
        let _client = serve_shared_with_a_session(&pipe, &shared).await;
        superseded.store(true, Ordering::SeqCst);
        join(shared.daemon).await;
        assert_eq!(forced_exit(&pipe), [Event::ListenerDropped], "events: {:?}", pipe.events());
    }

    /// A reservation that cannot be made says why, while a busy one is routine: a client is
    /// attached and accept takes it next.
    #[test]
    fn a_failed_reservation_is_diagnosed_and_a_busy_one_is_not() {
        use std::sync::Mutex;
        use tracing_subscriber::prelude::*;

        struct Warnings(Arc<Mutex<Vec<String>>>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                if *event.metadata().level() > tracing::Level::WARN {
                    return;
                }
                struct Fields(String);
                impl tracing::field::Visit for Fields {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        self.0.push_str(&format!("{}={value:?} ", field.name()));
                    }
                }
                let mut fields = Fields(String::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
        }

        let pipe = FakePipe::new();
        pipe.refuse([Refusal::Busy, Refusal::Error, Refusal::ForeignProcess]);
        let listener = pipe.listener();
        let warnings = Arc::new(Mutex::new(Vec::new()));
        test_utils::with_subscriber(
            tracing_subscriber::registry().with(Warnings(warnings.clone())),
            || {
                for _ in 0..3 {
                    assert!(reserve_for_idle_exit(&listener).is_none(), "a refusal keeps serving");
                }
            },
        );
        let warnings = warnings.lock().unwrap();
        assert_eq!(warnings.len(), 2, "only the failures warn: {warnings:?}");
        assert!(warnings[0].contains("probe failed"), "{warnings:?}");
        assert!(
            warnings[1]
                .contains(&format!("served by process {}", std::process::id().wrapping_add(1))),
            "{warnings:?}"
        );
    }

    /// Both endpoint kinds, each bound for real, with the spelling of its name that clients of
    /// earlier releases connect by.
    #[cfg(windows)]
    struct BoundEndpoint {
        _source: tempfile::TempDir,
        endpoint: Endpoint,
        earlier_name: interprocess::local_socket::Name<'static>,
        rebind: std::pin::Pin<Box<dyn Future<Output = Option<Endpoint>>>>,
    }

    #[cfg(windows)]
    async fn bound_endpoints() -> Vec<BoundEndpoint> {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};

        let backend_source = tempfile::tempdir().unwrap();
        let backend = crate::broker::BackendKey::new(
            backend_source.path(),
            backend_source.path().join("cache"),
            McpProfile::Workspace,
            0,
            0,
            std::collections::BTreeSet::new(),
        );
        let shared_source = tempfile::tempdir().unwrap();
        let shared = SharedBackendKey::new(SharedBackendIdentity::new(
            shared_source.path(),
            shared_source.path().join("cache"),
            [1; 32],
            [2; 32],
            [3; 32],
            [4; 32],
        ));
        vec![
            BoundEndpoint {
                endpoint: bind(&backend).await.unwrap().expect("a fresh name binds"),
                earlier_name: format!("bsl-mcp-{}.sock", backend.digest())
                    .to_ns_name::<GenericNamespaced>()
                    .unwrap(),
                rebind: Box::pin(async move { bind(&backend).await.unwrap() }),
                _source: backend_source,
            },
            BoundEndpoint {
                endpoint: bind_shared(&shared).await.unwrap().expect("a fresh name binds"),
                earlier_name: format!("bsl-shared-{}.sock", shared.digest())
                    .to_ns_name::<GenericNamespaced>()
                    .unwrap(),
                rebind: Box::pin(async move { bind_shared(&shared).await.unwrap() }),
                _source: shared_source,
            },
        ]
    }

    /// A client whose connect completed before accept holds the free instance: the reservation
    /// is refused at once, and accept then hands that client over.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_client_attached_before_accept_makes_the_pipe_reservation_busy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for bound in bound_endpoints().await {
            let mut client = connect_endpoint(
                bound.earlier_name.clone(),
                test_utils::wait::hang_guard(Duration::from_secs(5)),
            )
            .await
            .expect("a client of an earlier release reaches the endpoint");
            let asked = std::time::Instant::now();
            let reservation = bound.endpoint.reserve_idle_exit();
            let waited = asked.elapsed();
            assert!(
                matches!(reservation, IdleReservation::Busy),
                "an attached client makes the reservation busy: {reservation:?}"
            );
            if cfg!(timing_contract) {
                assert!(waited < Duration::from_millis(500), "the reservation waited {waited:?}");
            }
            let mut server = tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                bound.endpoint.accept(),
            )
            .await
            .expect("accept takes the attached client")
            .unwrap();
            client.write_all(b"ping").await.unwrap();
            let mut ping = [0; 4];
            server.read_exact(&mut ping).await.unwrap();
            assert_eq!(&ping, b"ping");
            server.write_all(b"pong").await.unwrap();
            let mut pong = [0; 4];
            client.read_exact(&mut pong).await.unwrap();
            assert_eq!(&pong, b"pong");
        }
    }

    /// A held reservation keeps the listener's free instance: no client completes a connect
    /// until the listener is gone, and once the reservation is dropped too the name binds again.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_pipe_reservation_holds_the_free_instance_until_the_listener_is_gone() {
        for bound in bound_endpoints().await {
            let reservation = match bound.endpoint.reserve_idle_exit() {
                IdleReservation::Held(reservation) => reservation,
                other => panic!("an empty listener is reserved: {other:?}"),
            };
            let second = PipeReservation::connect_by_path_with_wait_mode(
                bound.endpoint.pipe_path.as_str(),
                interprocess::ConnectWaitMode::Timeout(Duration::ZERO),
            );
            assert!(
                second.as_ref().is_err_and(|error| error.kind() == io::ErrorKind::TimedOut),
                "a second connect finds the reserved instance busy: {second:?}"
            );
            drop(bound.endpoint);
            drop(reservation);
            assert!(bound.rebind.await.is_some(), "the closed name binds again");
        }
    }

    #[test]
    fn only_an_instance_of_this_process_reserves_the_endpoint() {
        assert!(matches!(own_instance((), Ok(std::process::id())), IdleReservation::Held(())));
        assert!(matches!(
            own_instance((), Ok(std::process::id().wrapping_add(1))),
            IdleReservation::Failed(_)
        ));
        assert!(matches!(
            own_instance((), Err(io::Error::other("no server process"))),
            IdleReservation::Failed(_)
        ));
    }
}

#[cfg(test)]
mod shared_tests {
    use super::*;
    use crate::broker::name::shared_backend_name;
    use crate::broker::transport::{
        client_handshake, write_session_context, AdapterKind, MAX_HANDSHAKE_BYTES,
    };
    use crate::broker::{SharedBackendIdentity, SharedBackendKey};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn incomplete_shared_handshakes_and_private_probe_do_not_warm_or_start_adapter() {
        let source = tempfile::tempdir().unwrap();
        let identity = SharedBackendIdentity::new(
            source.path(),
            crate::WorkspaceCacheLayout::for_workspace(source.path()).root(),
            [1; 32],
            [2; 32],
            [3; 32],
            [4; 32],
        );
        let key = SharedBackendKey::new(identity.clone());
        let listener = bind_shared(&key).await.unwrap().unwrap();
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&adapter_calls);
        let shutdown_called = Arc::new(AtomicBool::new(false));
        let shutdown_seen = Arc::clone(&shutdown_called);
        let (attached_tx, mut attached_rx) = tokio::sync::mpsc::channel(1);
        let (finished_tx, mut finished_rx) = tokio::sync::mpsc::channel(1);
        let daemon = tokio::spawn(serve_shared(
            key.clone(),
            listener,
            Arc::new(
                move |_hello, _context, mut session, shutdown: CancellationToken, _retired| {
                    let calls = Arc::clone(&callback_calls);
                    let attached = attached_tx.clone();
                    let finished = finished_tx.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        attached.send(()).await.unwrap();
                        let SharedSession::Mcp(stream) = &mut session else {
                            anyhow::bail!("MCP adapter expected")
                        };
                        tokio::select! {
                            _ = shutdown.cancelled() => {},
                            _ = stream.read_u8() => {},
                        }
                        finished.send(()).await.unwrap();
                        Ok(())
                    }
                },
            ),
            CancellationToken::new(),
            CancellationToken::new(),
            Duration::from_secs(3),
            // An idle TTL no run reaches: only the orphan grace can end this daemon, so a
            // private-only attach that counted as a warm session would keep it running.
            test_utils::wait::hang_guard(Duration::from_secs(60)),
            || (false, false),
            move || shutdown_seen.store(true, Ordering::SeqCst),
        ));

        // Keep the new daemon active until its real five-second handshake deadline.
        // The server must close the accepted stream without invoking the adapter.
        let mut partial =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        partial.write_u8(0).await.unwrap();
        partial.flush().await.unwrap();
        let partial_eof = tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(7)),
            partial.read_u8(),
        )
        .await
        .expect("partial handshake is bounded by the server deadline");
        assert!(partial_eof.is_err(), "slow handshake closes without an accept record");
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);

        let mut malformed =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        malformed.write_u32(1).await.unwrap();
        malformed.write_u8(b'{').await.unwrap();
        malformed.flush().await.unwrap();
        assert_eq!(malformed.read_u8().await.unwrap(), 1);
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);

        let mut oversized =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        oversized.write_u32((MAX_HANDSHAKE_BYTES as u32) + 1).await.unwrap();
        oversized.flush().await.unwrap();
        assert_eq!(oversized.read_u8().await.unwrap(), 1);
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);

        let probe =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        drop(probe);
        tokio::task::yield_now().await;
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);

        // Positive control: private handshake/context reach the callback, but no public
        // MCP byte is sent, so this session must still be reaped by the orphan grace.
        let mut client =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        client_handshake(&mut client, &Handshake { identity, adapter: AdapterKind::Mcp })
            .await
            .unwrap();
        write_session_context(&mut client, &[]).await.unwrap();
        tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(3)),
            attached_rx.recv(),
        )
        .await
        .expect("valid handshake starts the adapter")
        .expect("adapter attached");
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 1);
        drop(client);
        tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(3)),
            finished_rx.recv(),
        )
        .await
        .expect("the adapter observes peer EOF")
        .expect("adapter finished");

        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(10)), daemon)
            .await
            .expect("private-only attach must not extend the orphan grace to the idle TTL")
            .unwrap()
            .unwrap();
        assert!(shutdown_called.load(Ordering::SeqCst));
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn shared_endpoint_rejects_each_identity_mismatch_before_adapter_admission() {
        let source = tempfile::tempdir().unwrap();
        let other_source = tempfile::tempdir().unwrap();
        let cache = source.path().join("cache");
        let identity = |source_dir: &std::path::Path,
                        cache_dir: &std::path::Path,
                        scope: u8,
                        config: u8,
                        platform: u8,
                        build: u8| {
            SharedBackendIdentity::new(
                source_dir,
                cache_dir,
                [scope; 32],
                [config; 32],
                [platform; 32],
                [build; 32],
            )
        };
        let expected = identity(source.path(), &cache, 1, 2, 3, 4);
        let key = SharedBackendKey::new(expected.clone());
        let listener = bind_shared(&key).await.unwrap().unwrap();
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&adapter_calls);
        let retired = CancellationToken::new();
        let daemon_retired = retired.clone();
        let (attached_tx, mut attached_rx) = tokio::sync::mpsc::channel(1);
        let daemon = tokio::spawn(serve_shared(
            key.clone(),
            listener,
            Arc::new(
                move |_hello, _context, mut session, shutdown: CancellationToken, _retired| {
                    let calls = Arc::clone(&calls);
                    let attached = attached_tx.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        attached.send(()).await.unwrap();
                        let SharedSession::Mcp(stream) = &mut session else {
                            anyhow::bail!("MCP adapter expected")
                        };
                        tokio::select! {
                            _ = shutdown.cancelled() => {},
                            _ = stream.read_u8() => {},
                        }
                        Ok(())
                    }
                },
            ),
            daemon_retired,
            CancellationToken::new(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            || (false, false),
            || {},
        ));

        // Every variant is created by this same package/build; only the listed identity axis
        // changes, so the differing build fingerprint is independent of package version.
        let mismatches = [
            identity(other_source.path(), &cache, 1, 2, 3, 4),
            identity(source.path(), &source.path().join("other-cache"), 1, 2, 3, 4),
            identity(source.path(), &cache, 9, 2, 3, 4),
            identity(source.path(), &cache, 1, 9, 3, 4),
            identity(source.path(), &cache, 1, 2, 9, 4),
            identity(source.path(), &cache, 1, 2, 3, 9),
        ];
        for mismatch in mismatches {
            let mut stream =
                connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT)
                    .await
                    .unwrap();
            let result = tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                client_handshake(
                    &mut stream,
                    &Handshake { identity: mismatch, adapter: AdapterKind::Mcp },
                ),
            )
            .await
            .expect("identity mismatch handshake is bounded");
            assert!(result.is_err(), "incompatible identity must be rejected before attach");
            assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);
        }

        let mut compatible =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        client_handshake(
            &mut compatible,
            &Handshake { identity: expected, adapter: AdapterKind::Mcp },
        )
        .await
        .expect("exact identity remains admissible after mismatches");
        write_session_context(&mut compatible, &[]).await.unwrap();
        tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(3)),
            attached_rx.recv(),
        )
        .await
        .expect("exact identity reaches adapter")
        .expect("adapter attached");
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 1);
        drop(compatible);
        retired.cancel();
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(5)), daemon)
            .await
            .expect("endpoint exits after explicit shutdown")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn shared_client_handshake_rejects_a_legacy_raw_mcp_server() {
        let source = tempfile::tempdir().unwrap();
        let identity = SharedBackendIdentity::new(
            source.path(),
            source.path().join("cache"),
            [1; 32],
            [2; 32],
            [3; 32],
            [4; 32],
        );
        let key = SharedBackendKey::new(identity.clone());
        let listener = listener_options_shared(&key).unwrap().create_tokio().unwrap();
        let legacy = tokio::spawn(async move {
            let mut stream = tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                listener.accept(),
            )
            .await
            .expect("new client reaches fake legacy endpoint before deadline")
            .unwrap();
            let length = tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                stream.read_u32(),
            )
            .await
            .expect("new client private handshake header is bounded")
            .unwrap() as usize;
            assert!(
                length <= MAX_HANDSHAKE_BYTES,
                "new client handshake stays under the protocol's advertised bound"
            );
            let mut received = vec![0; length];
            tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                stream.read_exact(&mut received),
            )
            .await
            .expect("new client private handshake body is bounded")
            .unwrap();
            stream.write_all(b"Content-Length: 2\r\n\r\n{}").await.unwrap();
            stream.flush().await.unwrap();
            received
        });

        let mut stream = crate::broker::proxy::connect_or_launch_stream(
            key.clone(),
            std::process::Command::new("unused-daemon-command"),
        )
        .await
        .expect("new shared client connects to the existing endpoint without launching a daemon");
        let attempt = tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(5)),
            client_handshake(&mut stream, &Handshake { identity, adapter: AdapterKind::Mcp }),
        )
        .await
        .expect("legacy endpoint response is bounded");
        assert!(attempt.is_err(), "raw MCP endpoint is not a compatible shared backend");
        drop(stream);
        let private_handshake = legacy.await.unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&private_handshake).unwrap();
        assert_eq!(wire["version"], crate::broker::SHARED_PROTOCOL_VERSION);
        assert_eq!(wire["adapter"], "mcp");
        assert!(
            !private_handshake
                .windows(b"initialize-marker".len())
                .any(|w| w == b"initialize-marker"),
            "no public initialize payload is part of the rejected private handshake"
        );
    }

    /// Whether a client reaches the endpoint at this instant, without waiting for it to accept.
    /// A Unix socket connect never waits. A Windows connect to a pipe whose instances are all
    /// held by connected sessions would sit in `WaitNamedPipe` on a blocking thread that outlives
    /// its caller's timeout and the session's drain, so the pipe is tried once instead.
    async fn endpoint_reachable_now(key: &SharedBackendKey) -> bool {
        #[cfg(unix)]
        {
            connect_endpoint(shared_backend_name(key).unwrap(), HANDSHAKE_TIMEOUT).await.is_ok()
        }
        #[cfg(windows)]
        {
            use interprocess::os::windows::named_pipe::{pipe_mode, tokio::DuplexPipeStream};

            DuplexPipeStream::<pipe_mode::Bytes>::connect_by_path_with_wait_mode(
                crate::broker::name::shared_backend_pipe_path(key),
                interprocess::ConnectWaitMode::Timeout(Duration::ZERO),
            )
            .await
            .is_ok()
        }
    }

    /// A client told its scope retired reconnects at once. While the listener lives, that
    /// connection only waits in its backlog, and closing the listener resets it: the rebind then
    /// fails instead of launching the next backend. So a session must not hear of the retirement
    /// before the endpoint is closed. The second session makes the accept loop register for the
    /// retirement after the first one, which is woken first when nothing else orders them.
    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "необязательный тест на Windows: a client still reaches the named pipe when a session hears of the retirement, so the endpoint closing first is not observable there; запуск: cargo test -p mcp-server --lib broker::daemon::shared_tests::a_session_learns_of_retirement_only_after_the_endpoint_closed -- --exact --ignored"
    )]
    async fn a_session_learns_of_retirement_only_after_the_endpoint_closed() {
        let source = tempfile::tempdir().unwrap();
        let identity = SharedBackendIdentity::new(
            source.path(),
            source.path().join("cache"),
            [5; 32],
            [6; 32],
            [7; 32],
            [8; 32],
        );
        let key = SharedBackendKey::new(identity.clone());
        let listener = bind_shared(&key).await.unwrap().unwrap();
        let retired = CancellationToken::new();
        let (attached_tx, mut attached_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reconnect_tx, mut reconnect_rx) = tokio::sync::mpsc::unbounded_channel();
        let endpoint_key = key.clone();
        let daemon = tokio::spawn(serve_shared(
            key.clone(),
            listener,
            Arc::new(
                move |_handshake: Handshake,
                      _context,
                      _session,
                      shutdown: CancellationToken,
                      retired: CancellationToken| {
                    let attached = attached_tx.clone();
                    let reconnect = reconnect_tx.clone();
                    let key = endpoint_key.clone();
                    async move {
                        attached.send(()).unwrap();
                        tokio::select! {
                            _ = shutdown.cancelled() => {}
                            _ = retired.cancelled() => {
                                reconnect.send(endpoint_reachable_now(&key).await).unwrap();
                            }
                        }
                        Ok(())
                    }
                },
            ),
            retired.clone(),
            CancellationToken::new(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            || (false, false),
            || {},
        ));

        let mut clients = Vec::new();
        for _ in 0..2 {
            let mut client =
                connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT)
                    .await
                    .unwrap();
            tokio::time::timeout(test_utils::wait::hang_guard(HANDSHAKE_TIMEOUT), async {
                client_handshake(
                    &mut client,
                    &Handshake { identity: identity.clone(), adapter: AdapterKind::Mcp },
                )
                .await
                .unwrap();
                write_session_context(&mut client, &[]).await.unwrap();
            })
            .await
            .expect("the client's handshake and session context are bounded");
            tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                attached_rx.recv(),
            )
            .await
            .expect("the session reaches the adapter")
            .expect("the adapter callback is alive");
            clients.push(client);
        }

        retired.cancel();
        for _ in 0..2 {
            let reached = tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                reconnect_rx.recv(),
            )
            .await
            .expect("a session hears of the retirement")
            .expect("the session reports before the daemon cuts it off");
            assert!(!reached, "the endpoint closes before a session hears of the retirement");
        }
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(5)), daemon)
            .await
            .expect("the daemon finishes once its sessions have ended")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn supersession_closes_shared_endpoint_then_waits_for_graph_reads_before_sessions() {
        let source = tempfile::tempdir().unwrap();
        let identity = SharedBackendIdentity::new(
            source.path(),
            crate::WorkspaceCacheLayout::for_workspace(source.path()).root(),
            [1; 32],
            [2; 32],
            [3; 32],
            [4; 32],
        );
        let key = SharedBackendKey::new(identity.clone());
        let listener = bind_shared(&key).await.unwrap().unwrap();
        let superseded = Arc::new(AtomicBool::new(false));
        let graph_released = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let (attached_tx, mut attached_rx) = tokio::sync::mpsc::channel(1);
        let (lease, graph, shutdown_seen) =
            (superseded.clone(), graph_released.clone(), stopped.clone());
        let retired = CancellationToken::new();
        let daemon = tokio::spawn(serve_shared(
            key.clone(),
            listener,
            Arc::new(
                move |_hello, _context, mut session, shutdown: CancellationToken, _retired| {
                    let attached = attached_tx.clone();
                    async move {
                        let SharedSession::Mcp(stream) = &mut session else {
                            panic!("MCP session")
                        };
                        attached.send(()).await.unwrap();
                        tokio::select! {
                            _ = shutdown.cancelled() => {},
                            _ = stream.read_u8() => {},
                        }
                        Ok(())
                    }
                },
            ),
            retired.clone(),
            CancellationToken::new(),
            Duration::from_millis(400),
            Duration::from_secs(60),
            move || (lease.load(Ordering::SeqCst), graph.load(Ordering::SeqCst)),
            move || {
                shutdown_seen.store(true, Ordering::SeqCst);
            },
        ));
        let mut client =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        client_handshake(&mut client, &Handshake { identity, adapter: AdapterKind::Mcp })
            .await
            .unwrap();
        tokio::time::timeout(
            test_utils::wait::hang_guard(HANDSHAKE_TIMEOUT),
            write_session_context(&mut client, &[]),
        )
        .await
        .expect("test session context write is bounded")
        .unwrap();
        tokio::time::timeout(test_utils::wait::hang_guard(HANDSHAKE_TIMEOUT), attached_rx.recv())
            .await
            .expect("adapter admission is bounded")
            .expect("the session attached");
        superseded.store(true, Ordering::SeqCst);
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(3)), async {
            loop {
                match connect_endpoint(
                    shared_backend_name(&key).unwrap(),
                    Duration::from_millis(50),
                )
                .await
                {
                    Ok(probe) => drop(probe),
                    Err(error) => {
                        #[cfg(windows)]
                        assert!(
                            windows_pipe_busy_or_timeout(&error),
                            "the still-connected session makes the named pipe busy: {error:?}"
                        );
                        #[cfg(not(windows))]
                        assert!(
                            matches!(
                                error.kind(),
                                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                            ),
                            "the superseded Unix endpoint is no longer listening: {error:?}"
                        );
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("supersession closes endpoint while the client remains attached");
        assert!(!daemon.is_finished(), "in-flight graph reads still own the file");
        assert!(!stopped.load(Ordering::SeqCst));
        assert!(!retired.is_cancelled(), "lease loss does not authorize scope rebind");
        // An open session sends nothing, so only a window can say it is still open.
        if cfg!(timing_contract) {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), client.read_u8()).await.is_err(),
                "the session stays open until graph reads return"
            );
        }
        graph_released.store(true, Ordering::SeqCst);
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(5)), daemon)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(test_utils::wait::hang_guard(HANDSHAKE_TIMEOUT), client.read_u8())
                .await
                .expect("old session closes after bounded read handover")
                .is_err(),
            "old session ends after read handover"
        );
        assert!(stopped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_stop_ends_the_superseded_wait_for_graph_reads() {
        let source = tempfile::tempdir().unwrap();
        let identity = SharedBackendIdentity::new(
            source.path(),
            crate::WorkspaceCacheLayout::for_workspace(source.path()).root(),
            [1; 32],
            [2; 32],
            [3; 32],
            [4; 32],
        );
        let key = SharedBackendKey::new(identity.clone());
        let listener = bind_shared(&key).await.unwrap().unwrap();
        let superseded = Arc::new(AtomicBool::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let (attached_tx, mut attached_rx) = tokio::sync::mpsc::channel(1);
        let (waiting_tx, mut waiting_rx) = tokio::sync::mpsc::unbounded_channel();
        let (lease, shutdown_seen) = (superseded.clone(), stopped.clone());
        let calls_since_supersession = Arc::new(AtomicU64::new(0));
        let stop = CancellationToken::new();
        let daemon = tokio::spawn(serve_shared(
            key.clone(),
            listener,
            Arc::new(
                move |_hello, _context, mut session, shutdown: CancellationToken, _retired| {
                    let attached = attached_tx.clone();
                    async move {
                        let SharedSession::Mcp(stream) = &mut session else {
                            panic!("MCP session")
                        };
                        attached.send(()).await.unwrap();
                        tokio::select! {
                            _ = shutdown.cancelled() => {},
                            _ = stream.read_u8() => {},
                        }
                        Ok(())
                    }
                },
            ),
            CancellationToken::new(),
            stop.clone(),
            Duration::from_millis(400),
            Duration::from_secs(60),
            // The graph is never released, so only the stop can end the superseded wait. Once
            // supersession is raised, the first call is the serving loop's own, which leaves
            // on it; the second is the wait for graph reads asking for the first time.
            move || {
                let raised = lease.load(Ordering::SeqCst);
                if raised && calls_since_supersession.fetch_add(1, Ordering::SeqCst) == 1 {
                    let _ = waiting_tx.send(());
                }
                (raised, false)
            },
            move || {
                shutdown_seen.store(true, Ordering::SeqCst);
            },
        ));
        let mut client =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        client_handshake(&mut client, &Handshake { identity, adapter: AdapterKind::Mcp })
            .await
            .unwrap();
        tokio::time::timeout(
            test_utils::wait::hang_guard(HANDSHAKE_TIMEOUT),
            write_session_context(&mut client, &[]),
        )
        .await
        .expect("test session context write is bounded")
        .unwrap();
        tokio::time::timeout(test_utils::wait::hang_guard(HANDSHAKE_TIMEOUT), attached_rx.recv())
            .await
            .expect("adapter admission is bounded")
            .expect("the session attached");
        superseded.store(true, Ordering::SeqCst);
        // The wait for graph reads has asked once, so the loop has left through supersession
        // and the stop below reaches that wait rather than the loop.
        tokio::time::timeout(
            test_utils::wait::hang_guard(Duration::from_secs(3)),
            waiting_rx.recv(),
        )
        .await
        .expect("supersession reaches the wait for graph reads while the client remains attached")
        .expect("the wait for graph reads asked");
        assert!(!daemon.is_finished(), "the superseded daemon waits for graph reads");
        stop.cancel();
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(5)), daemon)
            .await
            .expect("a stop ends the superseded wait")
            .unwrap()
            .unwrap();
        assert!(stopped.load(Ordering::SeqCst), "the stopped daemon runs its owner shutdown");
    }

    #[tokio::test]
    async fn mixed_sessions_are_admitted_and_one_can_disconnect_independently() {
        let source = tempfile::tempdir().unwrap();
        let identity = SharedBackendIdentity::new(
            source.path(),
            crate::WorkspaceCacheLayout::for_workspace(source.path()).root(),
            [1; 32],
            [2; 32],
            [3; 32],
            [4; 32],
        );
        let key = SharedBackendKey::new(identity.clone());
        let listener = bind_shared(&key).await.unwrap().unwrap();
        let retired = CancellationToken::new();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let callback_events = events_tx.clone();
        let server_retired = retired.clone();
        let daemon = tokio::spawn(serve_shared(
            key.clone(),
            listener,
            Arc::new(
                move |handshake: Handshake,
                      _context,
                      mut session,
                      shutdown: CancellationToken,
                      retired: CancellationToken| {
                    let events = callback_events.clone();
                    async move {
                        events.send(handshake.adapter).unwrap();
                        match &mut session {
                            SharedSession::Mcp(stream) => loop {
                                tokio::select! {
                                    _ = shutdown.cancelled() => break,
                                    _ = retired.cancelled() => break,
                                    byte = stream.read_u8() => if byte.is_err() { break; },
                                }
                            },
                            SharedSession::Lsp(channel) => loop {
                                tokio::select! {
                                    _ = shutdown.cancelled() => break,
                                    _ = retired.cancelled() => break,
                                    byte = channel.io.read_u8() => match byte {
                                        Ok(_) => { let _ = events.send(AdapterKind::Lsp); },
                                        Err(_) => break,
                                    },
                                }
                            },
                        }
                        Ok(())
                    }
                },
            ),
            server_retired,
            CancellationToken::new(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            || (false, false),
            || {},
        ));

        let mut mcp =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        client_handshake(
            &mut mcp,
            &Handshake { identity: identity.clone(), adapter: AdapterKind::Mcp },
        )
        .await
        .unwrap();
        write_session_context(&mut mcp, &[]).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                events_rx.recv()
            )
            .await
            .unwrap(),
            Some(AdapterKind::Mcp)
        ));

        let lsp_wire =
            connect_endpoint(shared_backend_name(&key).unwrap(), HANDSHAKE_TIMEOUT).await.unwrap();
        let mut lsp_wire = lsp_wire;
        client_handshake(&mut lsp_wire, &Handshake { identity, adapter: AdapterKind::Lsp })
            .await
            .unwrap();
        write_session_context(&mut lsp_wire, &[]).await.unwrap();
        let mut lsp = LspChannel::new(lsp_wire, || {});
        assert!(matches!(
            tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                events_rx.recv()
            )
            .await
            .unwrap(),
            Some(AdapterKind::Lsp)
        ));

        mcp.shutdown().await.unwrap();
        lsp.io.write_all(b"still alive").await.unwrap();
        assert!(matches!(
            tokio::time::timeout(
                test_utils::wait::hang_guard(Duration::from_secs(5)),
                events_rx.recv()
            )
            .await
            .unwrap(),
            Some(AdapterKind::Lsp)
        ));
        assert!(!daemon.is_finished());

        retired.cancel();
        tokio::time::timeout(test_utils::wait::hang_guard(Duration::from_secs(5)), daemon)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
