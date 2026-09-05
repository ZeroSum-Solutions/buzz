//! `inherited_environment` must survive a non-UTF-8 process environment.
//!
//! The launcher reads its own environment at `main` before it filters anything
//! or spawns a server, and on Unix an environment entry is a byte string, not a
//! `String`. One invalid entry anywhere in the harness's environment used to
//! take the process down there.
//!
//! This lives in its own integration binary because it drives the real entry
//! point, which means mutating the process environment: `std::env::set_var` is
//! process-wide and would race any other test sharing a binary with it.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

/// A lone continuation byte: a legal Unix environment byte, and not valid UTF-8.
fn invalid_bytes() -> OsString {
    OsString::from_vec(vec![0x80])
}

const BAD_VALUE: &str = "BUZZ_MCP_LAUNCH_PROBE_BAD_VALUE";
const BAD_NAME_PREFIX: &str = "BUZZ_MCP_LAUNCH_PROBE_BAD_NAME_";
const CARRIED: &str = "BUZZ_MCP_LAUNCH_PROBE_CARRIED";

#[test]
fn a_non_utf8_environment_entry_is_skipped_rather_than_fatal() {
    let bad_name = {
        let mut name = OsString::from(BAD_NAME_PREFIX);
        name.push(invalid_bytes());
        name
    };
    std::env::set_var(BAD_VALUE, invalid_bytes());
    std::env::set_var(&bad_name, "value");
    std::env::set_var(CARRIED, "carried");

    // Reaching the next line at all is half the assertion: the launcher must
    // return an environment here, not die enumerating one.
    let environment = buzz_mcp_launch::inherited_environment();

    std::env::remove_var(BAD_VALUE);
    std::env::remove_var(&bad_name);
    std::env::remove_var(CARRIED);

    assert_eq!(
        environment.get(CARRIED).map(String::as_str),
        Some("carried"),
        "the probe must read the real process environment, or it proves nothing"
    );
    assert!(
        !environment.contains_key(BAD_VALUE),
        "a non-UTF-8 value must be skipped, not carried: {environment:?}"
    );
    assert!(
        !environment
            .keys()
            .any(|name| name.starts_with(BAD_NAME_PREFIX)),
        "a non-UTF-8 name must be skipped too: {environment:?}"
    );
}
