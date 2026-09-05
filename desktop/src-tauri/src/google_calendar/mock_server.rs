//! A mock Google Calendar server for the tests.
//!
//! It speaks enough HTTP/1.1 for [`super::client::HttpTransport`] to drive it:
//! two principals holding different roles on one shared calendar, an ACL-loss
//! switch, paging, `If-Match` on mutations, a duplicate-id create, an oversized
//! page and a redirect. Everything the tests assert therefore runs through the
//! shipped transport, the shipped walk and the shipped parser.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// The calendar both principals see.
pub(crate) const CALENDAR_ID: &str = "team@example.com";
/// Alice's bearer token. She owns the calendar.
pub(crate) const ALICE_TOKEN: &str = "alice-access-token";
/// Bob's bearer token. He reads it.
pub(crate) const BOB_TOKEN: &str = "bob-access-token";

/// One principal's view of the shared calendar.
#[derive(Debug, Clone)]
pub(crate) struct Principal {
    /// The `accessRole` the calendar reports to them.
    pub access_role: String,
    /// Whether Google still lets them read it.
    pub acl_lost: bool,
}

/// What the server will answer with.
#[derive(Debug)]
pub(crate) struct MockState {
    /// Bearer token to principal.
    pub principals: HashMap<String, Principal>,
    /// Event id to the stored event object.
    pub events: HashMap<String, serde_json::Value>,
    /// Event ids in list order, one page each when `page_size` is 1.
    pub order: Vec<String>,
    /// Events per `events.list` page.
    pub page_size: usize,
    /// Extra filler bytes added to each page, to exercise the byte cap.
    pub page_filler_bytes: usize,
    /// Requests seen, as `(method, path, query)`.
    pub requests: Vec<(String, String, String)>,
}

/// A running mock server.
pub(crate) struct MockGoogle {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockGoogle {
    /// Start a server with two principals and `event_count` timed events, one
    /// hour apart from `first_start_ms`.
    pub(crate) fn start(event_count: usize, first_start_ms: i64) -> Self {
        let mut events = HashMap::new();
        let mut order = Vec::new();
        for index in 0..event_count {
            let id = format!("event-{index}");
            let start = first_start_ms + (index as i64) * 3_600_000;
            events.insert(id.clone(), timed_event(&id, start));
            order.push(id);
        }
        let state = Arc::new(Mutex::new(MockState {
            principals: HashMap::from([
                (
                    ALICE_TOKEN.to_string(),
                    Principal {
                        access_role: "owner".to_string(),
                        acl_lost: false,
                    },
                ),
                (
                    BOB_TOKEN.to_string(),
                    Principal {
                        access_role: "reader".to_string(),
                        acl_lost: false,
                    },
                ),
            ]),
            events,
            order,
            page_size: 250,
            page_filler_bytes: 0,
            requests: Vec::new(),
        }));

        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("the mock server binds a loopback port");
        let addr = listener
            .local_addr()
            .expect("the mock server has an address");
        listener
            .set_nonblocking(true)
            .expect("the mock server polls for shutdown");
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_state = Arc::clone(&state);
        let worker_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    // One thread per connection: a client that keeps a pooled
                    // connection open must not block the next test request.
                    Ok((stream, _)) => {
                        let connection_state = Arc::clone(&worker_state);
                        std::thread::spawn(move || serve(stream, &connection_state));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    // `accept` can fail transiently (an aborted connection, an
                    // interrupted call) under load. Breaking here would stop
                    // the server for the rest of the test with no message —
                    // the client would only see a refused connection.
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        });

        Self {
            addr,
            state,
            shutdown,
            handle: Some(handle),
        }
    }

    /// The base URL to point a transport at.
    pub(crate) fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.addr.port())
    }

    /// Mutate the server's state.
    pub(crate) fn with_state<R>(&self, edit: impl FnOnce(&mut MockState) -> R) -> R {
        let mut guard = self
            .state
            .lock()
            .expect("the mock state lock is not poisoned");
        edit(&mut guard)
    }
}

impl Drop for MockGoogle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One timed event, one hour long.
fn timed_event(id: &str, start_ms: i64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "etag": format!("\"etag-{id}\""),
        "status": "confirmed",
        "summary": format!("Event {id}"),
        "eventType": "default",
        "organizer": { "self": true },
        "start": { "dateTime": rfc3339(start_ms), "timeZone": "America/New_York" },
        "end": { "dateTime": rfc3339(start_ms + 3_600_000), "timeZone": "America/New_York" },
    })
}

fn rfc3339(instant_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(instant_ms)
        .expect("test instants are representable")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Serve one connection until the client closes it or goes idle.
///
/// Keep-alive on purpose: the shipped transport pools connections, so a server
/// that answered one request per socket would make a second request through the
/// same client fail on a dead pooled connection — a fixture defect that looks
/// exactly like a transport bug.
fn serve(stream: TcpStream, state: &Arc<Mutex<MockState>>) {
    // Well past any gap between two requests in one test: a server that closed
    // an idle keep-alive connection sooner would fail the client's next request
    // on a connection the client still believed usable.
    let idle = std::time::Duration::from_secs(60);
    let _ = stream.set_read_timeout(Some(idle));
    let _ = stream.set_write_timeout(Some(idle));
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut writer = stream;
    while serve_one(&mut reader, &mut writer, state) {}
}

/// Read one request and answer it. Returns whether the connection may carry
/// another.
fn serve_one(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    state: &Arc<Mutex<MockState>>,
) -> bool {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 || request_line.trim().is_empty() {
        return false;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut bearer = String::new();
    let mut if_match = String::new();
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
            break;
        }
        let lowered = header.to_ascii_lowercase();
        if let Some(value) = lowered.strip_prefix("authorization: bearer ") {
            bearer = header[header.len() - value.len()..].trim().to_string();
        } else if lowered.starts_with("if-match:") {
            if_match = header["if-match:".len()..].trim().to_string();
        } else if let Some(value) = lowered.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).is_err() {
        return false;
    }

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };
    state
        .lock()
        .expect("the mock state lock is not poisoned")
        .requests
        .push((method.clone(), path.clone(), query.clone()));

    let (status, payload) = route(&method, &path, &query, &bearer, &if_match, &body, state);
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}\r\n{payload}",
        payload.len(),
        if status == 302 {
            "Location: https://evil.example/stolen\r\n"
        } else {
            ""
        }
    );
    if writer.write_all(response.as_bytes()).is_err() {
        return false;
    }
    writer.flush().is_ok()
}

fn route(
    method: &str,
    path: &str,
    query: &str,
    bearer: &str,
    if_match: &str,
    body: &[u8],
    state: &Arc<Mutex<MockState>>,
) -> (u16, String) {
    if path == "/redirect-probe" {
        return (302, String::new());
    }
    let mut guard = state.lock().expect("the mock state lock is not poisoned");
    let Some(principal) = guard.principals.get(bearer).cloned() else {
        return (401, error_body("authError"));
    };
    if principal.acl_lost {
        return (403, error_body("forbidden"));
    }
    // Compare on the decoded path so a URL library that normalizes
    // percent-encoding differently from the client cannot make a test pass or
    // fail for the wrong reason.
    let path = decoded(path);
    let path = path.as_str();
    let events_path = format!("/calendars/{CALENDAR_ID}/events");
    if path == events_path && method == "GET" {
        return list_page(&guard, &principal, query);
    }
    if path == events_path && method == "POST" {
        let parsed: serde_json::Value = match serde_json::from_slice(body) {
            Ok(parsed) => parsed,
            Err(_) => return (400, error_body("badRequest")),
        };
        let id = parsed
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            return (400, error_body("required"));
        }
        if guard.events.contains_key(&id) {
            return (409, error_body("duplicate"));
        }
        let mut stored = parsed.clone();
        stored["etag"] = serde_json::Value::String(format!("\"etag-{id}\""));
        stored["status"] = serde_json::Value::String("confirmed".to_string());
        stored["eventType"] = serde_json::Value::String("default".to_string());
        stored["organizer"] = serde_json::json!({ "self": true });
        guard.events.insert(id.clone(), stored.clone());
        guard.order.push(id);
        return (200, stored.to_string());
    }
    if let Some(id) = path.strip_prefix(&format!("{events_path}/")) {
        let id = decoded(id);
        let Some(existing) = guard.events.get(&id).cloned() else {
            return (404, error_body("notFound"));
        };
        let etag = existing
            .get("etag")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if if_match != etag {
            return (412, error_body("conditionNotMet"));
        }
        if method == "DELETE" {
            guard.events.remove(&id);
            guard.order.retain(|stored| stored != &id);
            return (204, String::new());
        }
        if method == "PATCH" {
            let patch: serde_json::Value = match serde_json::from_slice(body) {
                Ok(parsed) => parsed,
                Err(_) => return (400, error_body("badRequest")),
            };
            let mut updated = existing;
            if let Some(fields) = patch.as_object() {
                for (key, value) in fields {
                    updated[key] = value.clone();
                }
            }
            updated["etag"] = serde_json::Value::String(format!("\"etag-{id}-2\""));
            guard.events.insert(id, updated.clone());
            return (200, updated.to_string());
        }
    }
    (404, error_body("notFound"))
}

fn list_page(state: &MockState, principal: &Principal, query: &str) -> (u16, String) {
    let offset: usize = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("pageToken=page-"))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let end = (offset + state.page_size).min(state.order.len());
    let items: Vec<serde_json::Value> = state.order[offset.min(state.order.len())..end]
        .iter()
        .filter_map(|id| state.events.get(id).cloned())
        .collect();
    let mut page = serde_json::json!({
        "kind": "calendar#events",
        "accessRole": principal.access_role,
        "timeZone": "America/New_York",
        "items": items,
    });
    if end < state.order.len() {
        page["nextPageToken"] = serde_json::Value::String(format!("page-{end}"));
    }
    if state.page_filler_bytes > 0 {
        page["filler"] = serde_json::Value::String("x".repeat(state.page_filler_bytes));
    }
    (200, page.to_string())
}

fn error_body(reason: &str) -> String {
    serde_json::json!({
        "error": { "errors": [{ "reason": reason }], "message": "mock failure" }
    })
    .to_string()
}

fn decoded(raw: &str) -> String {
    percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}
