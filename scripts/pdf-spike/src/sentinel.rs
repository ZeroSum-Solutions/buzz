//! Sentinel HTTP server for T8 (spike/pdf): accepts and logs every
//! connection, then refuses it with `403 Forbidden`. The offline fixture's
//! remote `<img>` points at this server instead of a live URL, so the
//! offline PDF run leaves durable, measured proof (a log line) that the
//! renderer actually attempted the remote fetch, rather than inferring an
//! attempt from a broken-image placeholder and a size delta that are
//! equally consistent with the request never being issued. Not production
//! code — thrown away with the `spike/pdf` branch.
//!
//! Usage: sentinel <port> <logfile>
//! Runs until killed (the caller backgrounds it and reads the log after the
//! render completes).
//!
//! Log lines are one of four kinds, distinguished by their leading tag so a
//! caller can grep for the one that actually proves a fetch happened:
//! - `refused peer=<addr> request=<line>` — a well-formed `GET ... HTTP/…`
//!   request line was read before the timeout. This is the only tag that
//!   proves the renderer issued an HTTP request; a caller matching for
//!   evidence of a fetch must grep this tag (ideally including the fixture's
//!   nonce in `<line>`), never merely check the log file is non-empty — a
//!   TCP connect that sends nothing produces `empty-request`, and a partial
//!   or garbled exchange produces `malformed-request`, neither of which is
//!   evidence the renderer requested the image.
//! - `empty-request peer=<addr>` — the client closed (or sent nothing) before
//!   a full line arrived.
//! - `malformed-request peer=<addr> request=<line>` — a line was read but it
//!   is not a well-formed `GET <path> HTTP/<version>` request.
//! - `read-error peer=<addr> err=<err>` — the read itself failed, including
//!   hitting the per-connection read timeout below.

use std::env;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Per-connection read deadline. Bounds how long one stalled client (connects,
/// never sends a newline) can hold up the accept loop before the next
/// connection is served, since this is a single-threaded loop with no
/// per-connection timeout otherwise.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Caps the request line read from one connection so a client that never
/// sends a newline cannot grow the buffer without bound while the read
/// timeout above is still pending.
const MAX_REQUEST_LINE_BYTES: u64 = 8192;

/// True only for a syntactically well-formed HTTP GET request line, e.g.
/// `GET /remote-image.png?nonce=... HTTP/1.1`. Deliberately strict: this is
/// the gate between "refused" (proof of a fetch) and every other outcome.
fn is_well_formed_get(line: &str) -> bool {
    line.starts_with("GET /") && line.contains(" HTTP/")
}

fn log_line(log_path: &str, line: &str) -> std::io::Result<()> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    writeln!(log, "{now} {line}")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: sentinel <port> <logfile>");
        process::exit(2);
    }
    let port: u16 = args[1].parse()?;
    let log_path = &args[2];

    let listener = TcpListener::bind(("127.0.0.1", port))?;
    // Signal readiness on stdout so the caller can synchronize before
    // starting Chrome.
    println!("sentinel listening on 127.0.0.1:{port}");
    std::io::stdout().flush()?;

    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        if let Err(e) = stream.set_read_timeout(Some(READ_TIMEOUT)) {
            if let Err(log_err) = log_line(
                log_path,
                &format!("read-error peer={peer} err=set_read_timeout failed: {e}"),
            ) {
                eprintln!("sentinel: failed to write log line: {log_err}");
            }
            continue;
        }

        let mut request_line = String::new();
        let read_result = {
            let mut limited = BufReader::new(&stream).take(MAX_REQUEST_LINE_BYTES);
            limited.read_line(&mut request_line)
        };

        let outcome = match read_result {
            Ok(0) => format!("empty-request peer={peer}"),
            Ok(_) => {
                let line = request_line.trim_end();
                if is_well_formed_get(line) {
                    format!("refused peer={peer} request={line}")
                } else {
                    format!("malformed-request peer={peer} request={line}")
                }
            }
            Err(e) => format!("read-error peer={peer} err={e}"),
        };
        if let Err(log_err) = log_line(log_path, &outcome) {
            eprintln!("sentinel: failed to write log line: {log_err}");
        }

        let response = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        if let Err(e) = stream.write_all(response) {
            eprintln!("sentinel: write to peer={peer} failed: {e}");
        } else if let Err(e) = stream.flush() {
            eprintln!("sentinel: flush to peer={peer} failed: {e}");
        }
    }
    Ok(())
}
