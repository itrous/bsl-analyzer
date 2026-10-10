use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    body::Body,
    extract::{Request, State},
    http::{
        header::{CONNECTION, CONTENT_LENGTH, HOST},
        uri::Authority,
        HeaderValue, StatusCode,
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::{McpProfile, McpServer};

/// Maximum accepted HTTP request body. MCP requests are small JSON-RPC
/// envelopes; a finite limit prevents unbounded buffering before dispatch.
pub const MAX_HTTP_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// Authorities accepted when the operator passed no explicit allowlist. Mirrors
/// rmcp's own default, so an unconfigured server answers the same set of names on
/// both routes.
const DEFAULT_ALLOWED_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// How long a cancelled server waits for requests already in flight. Reading a
/// request body observes no cancellation token, so a client that sends headers and
/// then stalls would otherwise hold the process — and its single-instance lock —
/// open indefinitely.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct HealthState {
    profile: McpProfile,
    address: SocketAddr,
    started_at: Instant,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    profile: &'static str,
    mode: &'static str,
    host: String,
    port: u16,
    pid: u32,
    uptime_seconds: u64,
}

fn normalize_host(host: &str) -> String {
    host.trim_matches('[').trim_matches(']').to_ascii_lowercase()
}

/// Parse a `Host` header strictly, as rmcp does. Leniency belongs to the allowlist,
/// never to the request: normalization strips brackets, so accepting a malformed value
/// as a bare name would let `[[localhost]]` match an allowed `localhost`, widening the
/// very gate that is supposed to narrow things.
fn parse_request_authority(value: &str) -> Option<(String, Option<u16>)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let authority = Authority::try_from(value).ok()?;
    Some((normalize_host(authority.host()), authority.port_u16()))
}

/// Parse one allowlist entry, which the operator writes by hand and may give as a bare
/// name rather than a full authority.
fn parse_allowed_authority(value: &str) -> Option<(String, Option<u16>)> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match Authority::try_from(value) {
        Ok(authority) => Some((normalize_host(authority.host()), authority.port_u16())),
        Err(_) => Some((normalize_host(value), None)),
    }
}

/// Whether a request's `Host` is in the allowlist, matching rmcp's rule: an entry
/// without a port accepts any port. rmcp keeps its copy of this check private to
/// the MCP route, so `/health` needs its own to sit behind the same anti-DNS-rebinding
/// gate rather than beside it.
fn host_is_allowed(host_header: &str, allowed_hosts: &[String]) -> bool {
    let Some((host, port)) = parse_request_authority(host_header) else {
        return false;
    };
    allowed_hosts.iter().filter_map(|allowed| parse_allowed_authority(allowed)).any(
        |(allowed_host, allowed_port)| {
            allowed_host == host && allowed_port.is_none_or(|expected| port == Some(expected))
        },
    )
}

/// Allowlist entries naming a wildcard bind address. `0.0.0.0` and `::` tell a listener
/// "every interface", but a `Host` header carries the authority the client dialed, so such
/// an entry authorizes a spelling almost nothing sends — the allowlist looks configured
/// while accepting nothing.
pub fn wildcard_allowed_hosts(allowed_hosts: &[String]) -> Vec<&str> {
    allowed_hosts
        .iter()
        .filter(|entry| {
            parse_allowed_authority(entry)
                .and_then(|(host, _)| host.parse::<IpAddr>().ok())
                .is_some_and(|ip| ip.is_unspecified())
        })
        .map(String::as_str)
        .collect()
}

/// Answer a request the server refuses before reading its body, announcing that the
/// connection ends here.
///
/// hyper cannot keep a connection whose request body was never drained: it closes the
/// read side, and the socket dies right after the response. Without this header the
/// response says nothing about that, so an HTTP/1.1 client is entitled to return the
/// connection to its pool and send the next request into a socket the server has already
/// closed — surfacing as `IncompleteMessage` on Linux and `WSAECONNABORTED` on Windows
/// instead of the refusal the server actually sent.
fn refuse(status: StatusCode) -> Response {
    let mut response = status.into_response();
    response.headers_mut().insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

/// Refuse a disallowed `Host` before anything reads the request body.
///
/// rmcp applies this check inside its own service, which reaches only `/mcp` and only
/// once the body has been collected. Gating the whole router here puts the readiness
/// endpoint behind the same anti-DNS-rebinding rule, and stops a refused client from
/// holding a connection open by never finishing the body it announced.
async fn host_gate(
    State(allowed_hosts): State<Arc<[String]>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    if parse_request_authority(&host).is_none() {
        tracing::warn!(host, "rejected request with a malformed Host header");
        return refuse(StatusCode::BAD_REQUEST);
    }
    if !host_is_allowed(&host, &allowed_hosts) {
        // The refusal is a mismatch between two values, so naming only one of them leaves
        // the operator guessing which side is wrong.
        tracing::warn!(
            host,
            allowed = %allowed_hosts.join(", "),
            "rejected request with disallowed Host header"
        );
        return refuse(StatusCode::FORBIDDEN);
    }
    next.run(request).await
}

async fn health(State(state): State<HealthState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        profile: state.profile.as_str(),
        mode: "http",
        host: state.address.ip().to_string(),
        port: state.address.port(),
        pid: std::process::id(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
    })
}

/// The authorities this server answers. One list drives both routes, so the MCP
/// endpoint and the readiness endpoint cannot drift apart on what they accept.
///
/// With no operator allowlist the bound address joins the defaults: the CLI treats
/// every address in 127.0.0.0/8 as safe to bind without one, while rmcp's defaults name
/// only `127.0.0.1`, so a server on `127.0.0.2` would otherwise refuse its own address.
fn effective_allowed_hosts(address: SocketAddr, allowed_hosts: Vec<String>) -> Arc<[String]> {
    if allowed_hosts.is_empty() {
        DEFAULT_ALLOWED_HOSTS
            .iter()
            .map(|host| (*host).to_owned())
            .chain(std::iter::once(address.ip().to_string()))
            .collect()
    } else {
        allowed_hosts.into()
    }
}

/// Enforce the body limit ourselves so the answer does not depend on framing.
///
/// An announced oversize is refused from `Content-Length` before a byte is read, which
/// is what `RequestBodyLimitLayer` used to provide; collecting under the same bound then
/// catches a chunked body of the same size, which the layer never saw and which reached
/// the client as rmcp's generic 500. Both paths now leave through one refusal, so the
/// status, the body and the connection outcome cannot differ by framing.
async fn limit_request_body(request: Request, next: Next) -> Response {
    if announces_oversize(&request) {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_HTTP_REQUEST_BODY_BYTES).await else {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE);
    };
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// Whether the request's own `Content-Length` already exceeds the limit, so the refusal
/// costs no reading at all. A body that announces nothing is still bounded by the collect
/// above, and so is one whose length does not parse.
fn announces_oversize(request: &Request) -> bool {
    request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|announced| announced > MAX_HTTP_REQUEST_BODY_BYTES as u64)
}

/// Serve stateful MCP sessions and a readiness endpoint on an already-bound
/// listener. Binding remains the caller's responsibility so startup can fail
/// before expensive shared state is constructed.
pub async fn serve_http(
    listener: TcpListener,
    server: McpServer,
    profile: McpProfile,
    address: SocketAddr,
    allowed_hosts: Vec<String>,
    cancellation: CancellationToken,
) -> anyhow::Result<()> {
    let allowed_hosts = effective_allowed_hosts(address, allowed_hosts);
    let scope_shutdown = server.scope_transport_stop();
    let relay_cancellation = cancellation.clone();
    let scope_relay = tokio::spawn(async move {
        scope_shutdown.cancelled().await;
        relay_cancellation.cancel();
    });
    let released = server.clone();
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancellation.clone())
        .with_allowed_hosts(allowed_hosts.iter().cloned());
    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let health_state = HealthState { profile, address, started_at: Instant::now() };
    // A superseded server stops listening once its graph reads are done — the address is the
    // current owner's to serve — and the requests still in flight drain under the usual grace.
    let release_watch = cancellation.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = release_watch.cancelled() => return,
                () = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
            }
            if released.graph_released() {
                tracing::info!(
                    "MCP HTTP server superseded and its graph released; stopping to listen"
                );
                release_watch.cancel();
                return;
            }
        }
    });

    let app = Router::new()
        .route_service("/mcp", mcp)
        .route("/health", get(health))
        .with_state(health_state)
        .layer(middleware::from_fn(limit_request_body))
        .layer(middleware::from_fn_with_state(Arc::clone(&allowed_hosts), host_gate));

    // `IntoFuture` rather than a bare await: the shutdown grace below needs to select
    // against this future, and drop it if the grace runs out.
    let serve = std::future::IntoFuture::into_future(
        axum::serve(listener, app).with_graceful_shutdown(cancellation.clone().cancelled_owned()),
    );
    tokio::pin!(serve);

    let result = tokio::select! {
        result = &mut serve => result,
        () = grace_after_cancel(&cancellation) => {
            tracing::warn!(
                grace_seconds = SHUTDOWN_GRACE.as_secs(),
                "MCP HTTP shutdown grace elapsed with requests still in flight; dropping them"
            );
            Ok(())
        }
    };
    scope_relay.abort();
    result?;
    Ok(())
}

async fn grace_after_cancel(cancellation: &CancellationToken) {
    cancellation.cancelled().await;
    tokio::time::sleep(SHUTDOWN_GRACE).await;
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, SocketAddr};

    use super::{
        effective_allowed_hosts, host_is_allowed, wildcard_allowed_hosts, CancellationToken,
    };

    #[tokio::test]
    async fn workspace_cache_scope_stops_the_http_listener() {
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = crate::SharedState::shared();
        let server = crate::McpServer::new(crate::McpProfile::Reference, state.clone());
        let task = tokio::spawn(super::serve_http(
            listener,
            server,
            crate::McpProfile::Reference,
            address,
            Vec::new(),
            CancellationToken::new(),
        ));

        state.scope_transport_stop().cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("scope retirement closes HTTP within a finite grace")
            .expect("HTTP task joins")
            .expect("HTTP shutdown succeeds");
    }

    fn address(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse::<IpAddr>().expect("test address should parse"), port)
    }

    /// The names that merely look like a wildcard carry the load here: a check that only
    /// matched the literal `0.0.0.0` would also flag `0.0.0.0.example.test`, a perfectly
    /// dialable name, and stay green on `[::]:8021`.
    #[test]
    fn only_an_unspecified_address_counts_as_a_wildcard_allowlist_entry() {
        let entries: Vec<String> = [
            "0.0.0.0",
            "0.0.0.0:8021",
            "::",
            "[::]",
            "[::]:8021",
            "0.0.0.0.example.test",
            "localhost",
            "127.0.0.1",
            "[::1]:8021",
            "mcp.example.test",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();

        assert_eq!(
            wildcard_allowed_hosts(&entries),
            ["0.0.0.0", "0.0.0.0:8021", "::", "[::]", "[::]:8021"]
        );
        assert!(wildcard_allowed_hosts(&[]).is_empty());
    }

    #[test]
    fn a_server_on_a_secondary_loopback_address_answers_its_own_host() {
        let allowed = effective_allowed_hosts(address("127.0.0.2", 8021), Vec::new());

        assert!(
            host_is_allowed("127.0.0.2:8021", &allowed),
            "a server the CLI let bind without an allowlist must answer its own address"
        );
        assert!(host_is_allowed("127.0.0.1:8021", &allowed), "the defaults must survive");
        assert!(!host_is_allowed("attacker.example", &allowed));
    }

    #[test]
    fn an_explicit_allowlist_replaces_the_defaults() {
        let allowed =
            effective_allowed_hosts(address("0.0.0.0", 8021), vec!["mcp.example.test".to_owned()]);

        assert!(host_is_allowed("mcp.example.test", &allowed));
        assert!(host_is_allowed("mcp.example.test:8021", &allowed), "a bare name accepts any port");
        assert!(
            !host_is_allowed("127.0.0.1:8021", &allowed),
            "naming an allowlist must not silently keep loopback"
        );
    }

    #[test]
    fn an_allowlist_entry_with_a_port_binds_that_port_only() {
        let allowed = effective_allowed_hosts(
            address("0.0.0.0", 8021),
            vec!["mcp.example.test:8021".to_owned()],
        );

        assert!(host_is_allowed("mcp.example.test:8021", &allowed));
        assert!(!host_is_allowed("mcp.example.test:9021", &allowed));
    }

    #[test]
    fn a_missing_or_unparsable_host_is_refused() {
        let allowed = effective_allowed_hosts(address("127.0.0.1", 8021), Vec::new());

        assert!(!host_is_allowed("", &allowed));
        assert!(!host_is_allowed("   ", &allowed));
    }

    #[test]
    fn a_malformed_host_cannot_normalize_into_an_allowed_one() {
        let allowed = effective_allowed_hosts(address("127.0.0.1", 8021), Vec::new());

        // None of these is a valid authority. Parsing the request leniently — treating
        // an unparsable value as a bare name — would strip the brackets off the first
        // two and match them against a legitimate allowlist entry.
        for malformed in ["[[localhost]]", "[[::1]]", "::1", "local host"] {
            assert!(
                !host_is_allowed(malformed, &allowed),
                "a malformed Host must not be normalized into an allowed one: {malformed}"
            );
        }

        // The well-formed spellings of the same authorities still pass. `[localhost]`
        // parses as an address literal and normalizes to `localhost`, which is rmcp's
        // rule for the MCP route; matching it here keeps the two gates from disagreeing.
        assert!(host_is_allowed("localhost", &allowed));
        assert!(host_is_allowed("[::1]:8021", &allowed));
        assert!(host_is_allowed("[localhost]", &allowed));
    }

    #[test]
    fn ipv6_loopback_matches_regardless_of_brackets() {
        let allowed = effective_allowed_hosts(address("::1", 8021), Vec::new());

        assert!(host_is_allowed("[::1]:8021", &allowed));
    }
}
