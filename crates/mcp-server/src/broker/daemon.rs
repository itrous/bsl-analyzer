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

use crate::broker::name::{backend_name, BackendKey};
#[cfg(windows)]
use crate::broker::security::pipe_security_descriptor_for_current_user;
use crate::{serve_stream, McpServer};

/// Cap on connections held while the resident state builds. The listener keeps draining
/// past this (so a concurrent liveness probe still succeeds), but excess connections are
/// dropped rather than parked, bounding memory against a runaway local client.
const MAX_PARKED_DURING_BUILD: usize = 128;

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
                if !peer_authorized(&conn) {
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

async fn serve(
    server: McpServer,
    listener: TokioListener,
    parked: Vec<TokioStream>,
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

    // Serve the connections that arrived during the build first.
    for conn in parked {
        sessions.push(spawn_session(&server, &active, &warmed, conn));
    }

    // `idle_since` marks when the current connection-less stretch began; it is cleared
    // whenever a session is active and (re)started when the backend falls idle. Poll at a
    // fraction of the *shorter* grace so the exit lands close to the intended window, not
    // up to 2× late, even when a test drives a tiny TTL.
    let poll =
        (orphan_grace.min(idle_ttl) / 4).clamp(Duration::from_millis(100), Duration::from_secs(1));
    let mut idle_since = Some(Instant::now());
    let mut ticker = interval(poll);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            // Poll accept before the idle tick: a proxy's `connect()` returns only once the
            // connection sits in the listener backlog (so `accept()` is ready), so biasing
            // accept guarantees a client that just connected is taken in before a
            // simultaneously-expiring idle tick can drop it — the proxy treats its successful
            // connect as non-retryable. This cannot starve the idle check: accept is only
            // continuously ready while connections are actively arriving, which is not idle.
            biased;
            accepted = listener.accept() => {
                let conn = accepted?;
                if !peer_authorized(&conn) {
                    tracing::warn!("rejected backend connection from an unauthorized peer");
                    continue;
                }
                sessions.push(spawn_session(&server, &active, &warmed, conn));
            }
            _ = ticker.tick() => {
                sessions.retain(|h| !h.is_finished());
                let superseded = server.superseded();
                // Reset the idle clock while any session is connected; otherwise count down
                // against the grace that fits the backend's history — the long `idle_ttl`
                // once it has served real traffic, the short `orphan_grace` before then.
                if active.load(Ordering::SeqCst) != 0 {
                    idle_since = None;
                } else if superseded {
                    // A newer generation owns this workspace's derived caches, so this backend
                    // is terminally unable to maintain them — a transient ownership refusal is
                    // not enough. The warm hold buys a reconnecting client nothing, and holding
                    // a multi-gigabyte resident until the TTL expires starves the daemon that
                    // CAN work. No session is connected at this point, so nobody's link is cut.
                    tracing::info!(
                        "backend superseded by a newer daemon generation and idle; shutting down"
                    );
                    break;
                } else if server.background_work_active() {
                    idle_since = None;
                } else {
                    let grace = if warmed.load(Ordering::SeqCst) { idle_ttl } else { orphan_grace };
                    let since = *idle_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= grace {
                        tracing::info!(
                            warmed = warmed.load(Ordering::SeqCst),
                            grace_secs = grace.as_secs(),
                            "backend idle past its grace; shutting down"
                        );
                        break;
                    }
                }
            }
        }
    }

    // Teardown cascade: stop accepting and sever every still-connected session, so each
    // proxy gets an EOF and exits. Aborting a session task drops its socket; dropping the
    // listener frees the rendezvous name.
    drop(listener);
    for handle in &sessions {
        handle.abort();
    }

    Ok(())
}

/// Serve one accepted connection on its own task. The [`ActiveGuard`] decrements the
/// active-session count on drop, so a panicking session can never strand the count.
///
/// The connection is wrapped in a [`FirstByteProbe`] that flips `warmed` the first time the
/// peer sends any data, so a session carrying real MCP traffic promotes the backend to the
/// long idle TTL while a no-data liveness probe (connect-then-close) never does. Returns the
/// task handle so the serve loop can abort it during the shutdown cascade.
fn spawn_session(
    server: &McpServer,
    active: &Arc<AtomicU64>,
    warmed: &Arc<AtomicBool>,
    conn: TokioStream,
) -> tokio::task::JoinHandle<()> {
    let guard = ActiveGuard::new(Arc::clone(active));
    tracing::debug!(active = active.load(Ordering::SeqCst), "broker accepted a session");
    let session = server.clone();
    let warmed = Arc::clone(warmed);
    tokio::spawn(async move {
        let _guard = guard;
        // First byte on this connection marks the backend as having served real traffic. A
        // liveness probe connects and closes without sending, so it never reaches here.
        let probe = FirstByteProbe::new(conn, move || warmed.store(true, Ordering::SeqCst));
        if let Err(e) = serve_stream(session, probe).await {
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
async fn bind(key: &BackendKey) -> anyhow::Result<Option<TokioListener>> {
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

fn listener_options(key: &BackendKey) -> io::Result<ListenerOptions<'static>> {
    let options = ListenerOptions::new().name(backend_name(key)?);
    #[cfg(windows)]
    {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;

        return Ok(options.security_descriptor(pipe_security_descriptor_for_current_user()?));
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
    if e.kind() == std::io::ErrorKind::AddrInUse {
        return true;
    }
    #[cfg(windows)]
    {
        const ERROR_ACCESS_DENIED: i32 = 5;
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
            return true;
        }
    }
    false
}

/// Is a backend actually accepting on this name? A successful connect proves a live
/// listener (queued in its backlog even mid-build); a refused/not-found connect
/// means the name is stale.
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
    match TokioStream::connect(backend_name(key)?).await {
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
