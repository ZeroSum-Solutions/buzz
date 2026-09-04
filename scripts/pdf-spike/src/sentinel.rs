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

use std::env;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

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
        let mut request_line = String::new();
        let _ = BufReader::new(&stream).read_line(&mut request_line);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        writeln!(
            log,
            "{now} refused peer={peer} request={}",
            request_line.trim_end()
        )?;
        let response = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(response);
        let _ = stream.flush();
    }
    Ok(())
}
