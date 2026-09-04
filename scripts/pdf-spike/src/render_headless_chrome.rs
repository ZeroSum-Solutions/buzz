//! Spike harness for T8 (spike/pdf): renders the fixture document to PDF via
//! `headless_chrome`, which drives a real Chrome/Chromium binary's
//! `Page.printToPDF` DevTools method. Not production code — thrown away with
//! the `spike/pdf` branch. See docs/plans/2026-09-04-pdf-route.md.
//!
//! Usage: render_headless_chrome <fixture.html> <out.pdf> [--offline]
//!
//! `--offline` launches Chrome with a proxy pointed at a closed local port,
//! so every outbound request (including the fixture's remote <img>) fails
//! deterministically without needing an OS-level firewall rule or sudo.

use headless_chrome::browser::default_executable;
use headless_chrome::types::PrintToPdfOptions;
use headless_chrome::{Browser, LaunchOptionsBuilder};
use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: render_headless_chrome <fixture.html> <out.pdf> [--offline]");
        std::process::exit(2);
    }
    let fixture_path = Path::new(&args[1]);
    let out_path = Path::new(&args[2]);
    let offline = args.iter().any(|a| a == "--offline");

    let fixture_abs = fs::canonicalize(fixture_path)?;
    let fixture_url = format!("file://{}", fixture_abs.display());

    let chrome_path = default_executable().map_err(|e| format!("no Chrome found: {e}"))?;

    let mut launch_args: Vec<&OsStr> = Vec::new();
    if offline {
        // Route everything at an unreachable local port so remote fetches
        // fail fast and deterministically, without a system firewall rule.
        launch_args.push(OsStr::new("--proxy-server=127.0.0.1:1"));
    }

    let launch_options = LaunchOptionsBuilder::default()
        .path(Some(chrome_path))
        .headless(true)
        .sandbox(false)
        .args(launch_args)
        .build()?;

    let wall_start = Instant::now();
    let browser = Browser::new(launch_options)?;
    let tab = browser.new_tab()?;
    tab.navigate_to(&fixture_url)?;
    tab.wait_until_navigated()?;
    // Give the fixture's remote <img> a bounded window to resolve (or fail,
    // in --offline mode) before the page is captured.
    std::thread::sleep(std::time::Duration::from_millis(800));

    let pdf_data = tab.print_to_pdf(Some(PrintToPdfOptions {
        landscape: Some(false),
        display_header_footer: Some(false),
        print_background: Some(true),
        scale: None,
        paper_width: Some(8.5),
        paper_height: Some(11.0),
        margin_top: Some(0.0),
        margin_bottom: Some(0.0),
        margin_left: Some(0.0),
        margin_right: Some(0.0),
        page_ranges: None,
        ignore_invalid_page_ranges: None,
        header_template: None,
        footer_template: None,
        prefer_css_page_size: Some(true),
        transfer_mode: None,
        generate_document_outline: None,
        generate_tagged_pdf: None,
    }))?;
    let wall_elapsed = wall_start.elapsed();

    fs::write(out_path, &pdf_data)?;

    let mut hasher = Sha256::new();
    hasher.update(&pdf_data);
    let hash = hasher.finalize();

    println!(
        "{{\"mode\":\"{}\",\"bytes\":{},\"wall_ms\":{},\"sha256\":\"{}\"}}",
        if offline { "offline" } else { "online" },
        pdf_data.len(),
        wall_elapsed.as_millis(),
        hex::encode(hash)
    );
    Ok(())
}

mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
}
