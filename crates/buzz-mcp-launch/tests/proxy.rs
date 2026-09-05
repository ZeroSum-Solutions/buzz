//! Proxy-mode tests through the shipped transport and the shipped bounds.
//!
//! Each drives `Proxy`, the same type `buzz-mcp-launch proxy` runs, against a
//! real HTTP upstream on loopback. The credential comes from the secret store
//! through the capability, so these also prove no generated file has to hold
//! one.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use buzz_mcp_launch::proxy::limits::ProxyLimits;
use buzz_mcp_launch::proxy::upstream::AuthScheme;
use buzz_mcp_launch::proxy::{Proxy, ProxyConfig, ProxyError};
use buzz_secret_store::capability::NONCE_LEN;
use buzz_secret_store::testing::MemoryBlobSource;
use buzz_secret_store::{AgentCapability, McpSecretRef};

/// What the fixture upstream saw and what it should answer with.
#[derive(Default)]
struct Upstream {
    requests: AtomicUsize,
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    auth_headers: Mutex<Vec<String>>,
    behaviour: Mutex<Behaviour>,
}

#[derive(Clone, Default)]
enum Behaviour {
    #[default]
    Echo,
    Redirect,
    Oversized(usize),
    Hang(Duration),
}

async fn handler(
    State(state): State<Arc<Upstream>>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.peak_in_flight.fetch_max(now, Ordering::SeqCst);
    if let Some(value) = headers
        .get("authorization")
        .or_else(|| headers.get("x-api-key"))
    {
        if let Ok(value) = value.to_str() {
            state
                .auth_headers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(value.to_string());
        }
    }
    let behaviour = state
        .behaviour
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let response = match behaviour {
        Behaviour::Echo => {
            // A tiny wait makes concurrent callers actually overlap.
            tokio::time::sleep(Duration::from_millis(20)).await;
            (StatusCode::OK, body).into_response()
        }
        Behaviour::Redirect => (
            StatusCode::FOUND,
            [("location", "https://elsewhere.example/mcp")],
            String::new(),
        )
            .into_response(),
        Behaviour::Oversized(len) => (StatusCode::OK, "x".repeat(len)).into_response(),
        Behaviour::Hang(duration) => {
            tokio::time::sleep(duration).await;
            (StatusCode::OK, body).into_response()
        }
    };
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    response
}

async fn start_upstream() -> (Arc<Upstream>, SocketAddr) {
    let state = Arc::new(Upstream::default());
    let app = Router::new()
        .route("/mcp", post(handler))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (state, address)
}

fn store() -> MemoryBlobSource {
    let source = MemoryBlobSource::default();
    let bound = capability();
    source.insert(
        &buzz_secret_store::binding_key(&bound),
        bound.binding_value(),
    );
    source.insert("mcp:agent-a:1:api-token", "S3CRET-VALUE");
    source
}

fn capability() -> AgentCapability {
    AgentCapability::mint("agent-a", 1, [9u8; NONCE_LEN]).expect("valid agent id")
}

fn proxy_with(address: SocketAddr, limits: ProxyLimits) -> Proxy<MemoryBlobSource> {
    Proxy::new(ProxyConfig {
        url: url::Url::parse(&format!("http://{address}/mcp")).expect("loopback url"),
        auth: Some(AuthScheme::Bearer),
        secret: Some(McpSecretRef::parse("mcp:api-token").expect("valid reference")),
        capability: Some(capability()),
        secrets: store(),
        limits,
    })
    .expect("proxy builds")
}

#[tokio::test]
async fn the_credential_comes_from_the_store_and_never_from_a_file() {
    let (state, address) = start_upstream().await;
    let proxy = proxy_with(address, ProxyLimits::default());
    let reply = proxy
        .forward(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .await
        .expect("forwards")
        .expect("a reply");
    assert!(String::from_utf8_lossy(&reply).contains("\"ping\""));
    let headers = state.auth_headers.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(headers.as_slice(), ["Bearer S3CRET-VALUE"]);
}

/// A listener that counts the connections made to it and answers nothing,
/// standing in for the proxy an ambient `HTTP_PROXY` would name.
async fn start_proxy_sentinel() -> (Arc<AtomicUsize>, SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    let connections = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&connections);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            // Drop it immediately: a proxied request should fail fast rather
            // than sit on the request timeout.
            drop(stream);
        }
    });
    (connections, address)
}

#[tokio::test]
async fn an_ambient_proxy_never_sees_the_credential() {
    // reqwest discovers a system proxy from the environment by default, and
    // this client is the one in the process that carries a resolved keychain
    // secret to an upstream `validate_upstream` allows to be plain `http://`
    // on loopback. `Proxy::new`'s `.no_proxy()` is what keeps the request on
    // the origin the registry named.
    let (connections, sentinel) = start_proxy_sentinel().await;
    let (state, address) = start_upstream().await;

    // Set process-wide and never unset: unsetting would race the other tests
    // in this binary, and with `.no_proxy()` in place no client here reads
    // these at all. Every spelling reqwest consults is covered.
    std::env::set_var("HTTP_PROXY", format!("http://{sentinel}"));
    std::env::set_var("HTTPS_PROXY", format!("http://{sentinel}"));
    std::env::set_var("ALL_PROXY", format!("http://{sentinel}"));
    std::env::remove_var("NO_PROXY");
    std::env::remove_var("no_proxy");

    let proxy = proxy_with(address, ProxyLimits::default());
    let result = proxy
        .forward(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .await;

    // Checked before the reply, so a client that honours the ambient proxy
    // reports that rather than the connection error the sentinel produces by
    // hanging up.
    assert_eq!(
        connections.load(Ordering::SeqCst),
        0,
        "the credential-carrying request was routed through the ambient proxy"
    );
    assert_eq!(
        state.requests.load(Ordering::SeqCst),
        1,
        "the named upstream did not receive the request"
    );
    let reply = result
        .expect("the call goes straight to the named upstream")
        .expect("a reply");
    assert!(String::from_utf8_lossy(&reply).contains("\"ping\""));
}

#[tokio::test]
async fn a_redirect_is_an_error_and_issues_no_second_request() {
    let (state, address) = start_upstream().await;
    *state.behaviour.lock().unwrap_or_else(|e| e.into_inner()) = Behaviour::Redirect;
    let proxy = proxy_with(address, ProxyLimits::default());
    let error = proxy
        .forward(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .await
        .expect_err("a 30x must not be followed");
    assert!(matches!(error, ProxyError::Redirect(302)), "{error:?}");
    assert_eq!(
        state.requests.load(Ordering::SeqCst),
        1,
        "following the redirect would have made a second request, carrying the credential to a new origin"
    );
}

#[tokio::test]
async fn a_response_past_the_cap_aborts_the_call() {
    let (state, address) = start_upstream().await;
    let limits = ProxyLimits {
        max_response_bytes: 4096,
        ..ProxyLimits::default()
    };
    *state.behaviour.lock().unwrap_or_else(|e| e.into_inner()) = Behaviour::Oversized(64 * 1024);
    let proxy = proxy_with(address, limits);
    let error = proxy
        .forward(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .await
        .expect_err("an oversized body must be refused");
    assert!(
        matches!(error, ProxyError::ResponseTooLarge { cap: 4096 }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_request_past_the_cap_is_refused_before_it_is_sent() {
    let (state, address) = start_upstream().await;
    let limits = ProxyLimits {
        max_request_bytes: 1024,
        ..ProxyLimits::default()
    };
    let proxy = proxy_with(address, limits);
    let error = proxy
        .forward(&vec![b'x'; 2048])
        .await
        .expect_err("an oversized request must be refused");
    assert!(
        matches!(error, ProxyError::RequestTooLarge { cap: 1024, .. }),
        "{error:?}"
    );
    assert_eq!(state.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_hung_upstream_trips_the_request_timeout() {
    let (state, address) = start_upstream().await;
    let limits = ProxyLimits {
        request_timeout: Duration::from_millis(200),
        ..ProxyLimits::default()
    };
    *state.behaviour.lock().unwrap_or_else(|e| e.into_inner()) =
        Behaviour::Hang(Duration::from_secs(30));
    let proxy = proxy_with(address, limits);
    let error = proxy
        .forward(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .await
        .expect_err("a hung upstream must not block forever");
    assert!(matches!(error, ProxyError::Upstream(_)), "{error:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixty_four_callers_are_served_under_the_in_flight_cap() {
    let (state, address) = start_upstream().await;
    let limits = ProxyLimits {
        max_in_flight: 4,
        // Keep the byte ceiling well clear so the in-flight cap is what binds
        // here; `one_request_always_fits_inside_the_buffer_ceiling` covers the
        // other direction.
        max_response_bytes: 64 * 1024,
        max_buffered_bytes: 32 * 1024 * 1024,
        ..ProxyLimits::default()
    };
    let proxy = Arc::new(proxy_with(address, limits));

    let mut input = Vec::new();
    for id in 0..64 {
        input.extend_from_slice(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}\n").as_bytes(),
        );
    }
    let output: Vec<u8> = Vec::new();
    Arc::clone(&proxy)
        .run(&input[..], output)
        .await
        .expect("the transport drains");

    assert_eq!(state.requests.load(Ordering::SeqCst), 64);
    assert!(
        state.peak_in_flight.load(Ordering::SeqCst) <= 4,
        "peak concurrency was {}, over the in-flight cap",
        state.peak_in_flight.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn an_oversized_inbound_frame_stops_the_transport() {
    let (_state, address) = start_upstream().await;
    let limits = ProxyLimits {
        max_inbound_frame_bytes: 4096,
        ..ProxyLimits::default()
    };
    let proxy = Arc::new(proxy_with(address, limits));
    let mut input = vec![b'x'; 8192];
    input.push(b'\n');
    let error = proxy
        .run(&input[..], Vec::new())
        .await
        .expect_err("an oversized frame must stop the transport");
    assert!(matches!(error, ProxyError::Transport(_)), "{error:?}");
}

#[tokio::test]
async fn an_upstream_failure_reaches_the_caller_as_a_json_rpc_error() {
    let (state, address) = start_upstream().await;
    *state.behaviour.lock().unwrap_or_else(|e| e.into_inner()) = Behaviour::Redirect;
    let proxy = Arc::new(proxy_with(address, ProxyLimits::default()));
    let output: Vec<u8> = Vec::new();
    let written = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = SharedSink(Arc::clone(&written));
    let _ = output;
    Arc::clone(&proxy)
        .run(
            &b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\n"[..],
            sink,
        )
        .await
        .expect("the transport drains");
    let reply =
        String::from_utf8_lossy(&written.lock().unwrap_or_else(|e| e.into_inner())).to_string();
    assert!(reply.contains("\"error\""), "no error surfaced: {reply}");
    assert!(reply.contains("\"id\":7"), "the error lost its id: {reply}");
}

/// An `AsyncWrite` that collects everything written, so a test can read what
/// the proxy sent back to the model.
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl tokio::io::AsyncWrite for SharedSink {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// An `AsyncWrite` that fails every write, standing in for a model whose stdio
/// has gone away.
struct FailingSink;

impl tokio::io::AsyncWrite for FailingSink {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        _buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "the model is gone",
        )))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_failure_stops_the_proxy_before_it_calls_upstream_again() {
    // A proxy that can no longer deliver a reply must not keep issuing
    // authenticated — possibly mutating — upstream calls, and must not report
    // success. The in-flight cap of one makes the first failure observable to
    // the read loop before it can spawn much more work; the reaping in `run`
    // is what carries it there.
    const FRAMES: usize = 64;

    let (state, address) = start_upstream().await;
    let limits = ProxyLimits {
        max_in_flight: 1,
        ..ProxyLimits::default()
    };
    let proxy = Arc::new(proxy_with(address, limits));

    let mut input = Vec::new();
    for id in 0..FRAMES {
        input.extend_from_slice(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"ping\"}}\n").as_bytes(),
        );
    }

    let error = Arc::clone(&proxy)
        .run(&input[..], FailingSink)
        .await
        .expect_err("a proxy that cannot answer must not report success");
    assert!(matches!(error, ProxyError::Write(_)), "{error:?}");

    let requests = state.requests.load(Ordering::SeqCst);
    assert!(
        requests < FRAMES,
        "every one of the {FRAMES} frames still reached the upstream after the write failed"
    );
    assert!(
        requests <= 4,
        "{requests} upstream calls were issued after the proxy lost its writer"
    );
}
