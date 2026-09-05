//! The bounded loopback listener of T11 decision 1.
//!
//! The installed-app flow redirects to `http://127.0.0.1:<port>`, so this
//! process briefly accepts connections from anything running as the user. The
//! listener therefore bounds the three quantities that cost: bytes read per
//! connection, connections accepted before it gives up, and time. The time
//! bound is one deadline for the whole wait, carried into the per-connection
//! read loop: a per-read timeout alone resets on every byte, so a local process
//! that dribbles one byte per timeout would hold the flow open for hours. One
//! flow is in flight at a time; the caller drops the previous listener before
//! binding a new one, so the newest wins.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

/// Largest request the listener reads from one connection, in bytes.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// Largest number of connections one wait accepts before giving up.
pub const MAX_CONNECTIONS: usize = 8;
/// Longest one connection may go without delivering another byte.
///
/// This is the idle bound only. The whole wait is bounded by
/// [`WAIT_TIMEOUT_MS`], which is carried into the read loop, so a connection
/// can never outlive the wait it belongs to.
pub const READ_TIMEOUT_MS: u64 = 5_000;
/// Longest the whole wait may take.
pub const WAIT_TIMEOUT_MS: u64 = 5 * 60 * 1000;

/// The caps one wait runs under.
#[derive(Debug, Clone, Copy)]
pub struct ListenerLimits {
    /// Largest request read from one connection, in bytes.
    pub max_request_bytes: usize,
    /// Largest number of connections accepted.
    pub max_connections: usize,
    /// Longest one connection may go without delivering another byte, in
    /// milliseconds. Never longer than the remainder of `wait_timeout_ms`.
    pub read_timeout_ms: u64,
    /// Longest the whole wait may take, in milliseconds.
    pub wait_timeout_ms: u64,
}

impl Default for ListenerLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: MAX_REQUEST_BYTES,
            max_connections: MAX_CONNECTIONS,
            read_timeout_ms: READ_TIMEOUT_MS,
            wait_timeout_ms: WAIT_TIMEOUT_MS,
        }
    }
}

/// Why a wait ended without a callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenerError {
    /// The socket could not be bound, accepted or read.
    Io(String),
    /// The wait timed out before a usable request arrived.
    TimedOut,
    /// The connection cap was reached with no usable request.
    TooManyConnections(usize),
}

impl std::fmt::Display for ListenerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ListenerError::Io(detail) => write!(f, "the callback listener failed: {detail}"),
            ListenerError::TimedOut => write!(f, "no authorization callback arrived in time"),
            ListenerError::TooManyConnections(cap) => write!(
                f,
                "the callback listener refused after {cap} connections with no callback"
            ),
        }
    }
}

impl std::error::Error for ListenerError {}

/// A loopback listener waiting for one authorization callback.
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
}

impl CallbackListener {
    /// Bind an ephemeral port on the loopback interface.
    ///
    /// # Errors
    /// Returns [`ListenerError::Io`] when the socket cannot be bound.
    pub fn bind() -> Result<Self, ListenerError> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(|error| ListenerError::Io(error.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|error| ListenerError::Io(error.to_string()))?
            .port();
        listener
            .set_nonblocking(true)
            .map_err(|error| ListenerError::Io(error.to_string()))?;
        Ok(Self { listener, port })
    }

    /// The redirect URI to send with the authorization request.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The bound port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Wait for one callback and return its query string.
    ///
    /// Every bound is enforced: a connection that sends more than
    /// `max_request_bytes` is answered 413 and closed, one that is not a `GET`
    /// is answered 405, and both count toward the connection cap so a local
    /// process cannot hold the flow open indefinitely.
    ///
    /// # Errors
    /// See [`ListenerError`].
    pub fn wait_for_callback(&self, limits: &ListenerLimits) -> Result<String, ListenerError> {
        let deadline = Instant::now() + Duration::from_millis(limits.wait_timeout_ms);
        let mut connections = 0usize;
        while connections < limits.max_connections {
            if Instant::now() >= deadline {
                return Err(ListenerError::TimedOut);
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    connections += 1;
                    // The wait's own deadline goes with the connection, so one
                    // slow client cannot outlast the wait by dribbling bytes.
                    match read_callback(stream, limits, deadline) {
                        Ok(Callback::Received(query)) => return Ok(query),
                        // A refused connection is counted, not fatal: a browser
                        // preflight or a favicon request must not end the flow.
                        Ok(Callback::Refused) => continue,
                        Ok(Callback::WaitExpired) => return Err(ListenerError::TimedOut),
                        Err(error) => return Err(ListenerError::Io(error)),
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(ListenerError::TimedOut);
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(25)));
                }
                Err(error) => return Err(ListenerError::Io(error.to_string())),
            }
        }
        Err(ListenerError::TooManyConnections(limits.max_connections))
    }
}

/// What one accepted connection produced.
enum Callback {
    /// A usable `GET` with a query string.
    Received(String),
    /// Refused for a bound or a shape. Another connection may still arrive.
    Refused,
    /// The whole wait's deadline passed while this connection was being read.
    WaitExpired,
}

/// Read one request and answer it.
///
/// `deadline` is the whole wait's deadline, not this connection's: each read
/// waits for the shorter of the idle bound and the wait's remainder, so the
/// total time this function can consume is bounded by the wait even when the
/// client keeps sending a byte just before every idle timeout.
fn read_callback(
    mut stream: TcpStream,
    limits: &ListenerLimits,
    deadline: Instant,
) -> Result<Callback, String> {
    let idle = Duration::from_millis(limits.read_timeout_ms.max(1));
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;

    let mut buffer = vec![0u8; limits.max_request_bytes + 1];
    let mut filled = 0usize;
    loop {
        // One byte over the cap is enough to know the request is oversized, so
        // an attacker cannot make this buffer grow.
        if filled > limits.max_request_bytes {
            respond(&mut stream, 413, "Request too large.");
            return Ok(Callback::Refused);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            respond(&mut stream, 408, "Timed out.");
            return Ok(Callback::WaitExpired);
        }
        let slice = remaining.min(idle);
        stream
            .set_read_timeout(Some(slice))
            .map_err(|error| error.to_string())?;
        stream
            .set_write_timeout(Some(slice))
            .map_err(|error| error.to_string())?;
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => {
                filled += read;
                if buffer[..filled]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
                {
                    break;
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                // Distinguish the idle bound from the wait's own deadline: the
                // first refuses one connection, the second ends the wait.
                if Instant::now() >= deadline {
                    respond(&mut stream, 408, "Timed out.");
                    return Ok(Callback::WaitExpired);
                }
                respond(&mut stream, 408, "Timed out.");
                return Ok(Callback::Refused);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    if filled > limits.max_request_bytes {
        respond(&mut stream, 413, "Request too large.");
        return Ok(Callback::Refused);
    }

    let head = String::from_utf8_lossy(&buffer[..filled]);
    let Some(request_line) = head.lines().next() else {
        respond(&mut stream, 400, "Empty request.");
        return Ok(Callback::Refused);
    };
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        respond(&mut stream, 400, "Malformed request.");
        return Ok(Callback::Refused);
    };
    if method != "GET" {
        respond(&mut stream, 405, "Only GET is accepted.");
        return Ok(Callback::Refused);
    }
    let Some((_, query)) = target.split_once('?') else {
        respond(&mut stream, 400, "No authorization parameters.");
        return Ok(Callback::Refused);
    };
    respond(
        &mut stream,
        200,
        "Buzz has the authorization response. You can close this tab.",
    );
    Ok(Callback::Received(query.to_string()))
}

/// Write one fixed response. A failure here is not fatal: the request was
/// already read, and the browser tab is cosmetic.
fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        _ => "Payload Too Large",
    };
    let body = format!("<!doctype html><meta charset=\"utf-8\"><p>{message}</p>");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
