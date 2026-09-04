//! Spawn-boundary tests for the `trusted` flag on MCP servers.
//!
//! `BUZZ_ACP_EXTRA_MCP_COMMANDS` lets an operator attach third-party MCP
//! servers to an agent session. Those servers are marked untrusted, and
//! `mcp::spawn_one` must withhold the four Buzz identity variables
//! (`BUZZ_PRIVATE_KEY`, `NOSTR_PRIVATE_KEY`, `BUZZ_RELAY_URL`,
//! `BUZZ_AUTH_TAG`) from their process environment, while the built-in
//! `buzz-dev-mcp` server — the only one marked trusted — still receives them.
//!
//! These tests bind the production seam end to end: a real `buzz-agent`
//! child, a real `session/new` with `mcpServers` off the wire, a real MCP
//! subprocess, and the tool result the agent feeds back to the LLM. The fake
//! server reports the *names* of the variables it was spawned with, never
//! their values, so a leak cannot reach the test output.

mod common;

use serde_json::{json, Value};

use common::{openai_text, openai_tool_call, spawn_capturing_llm, Harness};

/// The four identity variables the spawn boundary withholds from untrusted
/// MCP servers.
const IDENTITY_VARS: &[&str] = &[
    "BUZZ_PRIVATE_KEY",
    "NOSTR_PRIVATE_KEY",
    "BUZZ_RELAY_URL",
    "BUZZ_AUTH_TAG",
];

/// Values handed to the agent process so the passthrough allowlist has
/// something to pass. They are never asserted on — only the variable names
/// travel back through the tool result.
fn identity_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("BUZZ_PRIVATE_KEY", "nsec-test-private-key"),
        ("NOSTR_PRIVATE_KEY", "nsec-test-nostr-key"),
        ("BUZZ_RELAY_URL", "wss://relay.test.invalid"),
        ("BUZZ_AUTH_TAG", "auth-tag-test"),
        ("BUZZ_ACP_DISPLAY_NAME", "Test Agent"),
    ]
}

/// Declare one fake MCP server on the wire. `trusted` is emitted only when
/// true so the untrusted case exercises the serde default, exactly as
/// `buzz-acp` writes it.
fn server_decl(name: &str, trusted: bool) -> Value {
    let mut decl = json!({
        "name": name,
        "command": env!("CARGO_BIN_EXE_fake-mcp"),
        "args": [],
        "env": [
            { "name": "FAKE_MCP_TOOL_COUNT", "value": "1" },
            { "name": "FAKE_MCP_ENV_REPORT", "value": "1" },
        ],
    });
    if trusted {
        decl["trusted"] = json!(true);
    }
    decl
}

/// Open a session over the given server declarations and return its id.
async fn new_session(h: &mut Harness, servers: Vec<Value>) -> String {
    h.send(
        "initialize",
        json!({"protocolVersion":1,"clientCapabilities":{}}),
    )
    .await;
    let _ = h.recv().await;
    h.send(
        "session/new",
        json!({ "cwd": "/tmp", "mcpServers": servers }),
    )
    .await;
    let r = h
        .recv_until(|v| v.get("result").is_some() || v.get("error").is_some())
        .await;
    assert!(r.get("error").is_none(), "session/new failed: {r}");
    r["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_owned()
}

/// The text of the `role: "tool"` message in the `n`th captured LLM request.
fn tool_result_text(captured: &[Value], n: usize) -> String {
    let msgs = captured
        .get(n)
        .and_then(|c| c["messages"].as_array())
        .unwrap_or_else(|| panic!("LLM request {n} missing or has no messages"));
    msgs.iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_else(|| panic!("no tool result message in LLM request {n}"))
        .to_owned()
}

fn env_names(report: &str) -> Vec<&str> {
    report.lines().map(str::trim).collect()
}

/// The spawn boundary withholds every Buzz identity variable from an
/// untrusted MCP server and keeps them for a trusted one — proven in a
/// single agent process, so the untrusted half cannot pass merely because
/// the parent never had the variables.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrusted_mcp_env_withheld_from_untrusted_kept_for_trusted() {
    let llm = spawn_capturing_llm(vec![
        openai_tool_call("tc1", "extra__tool_0", json!({})),
        openai_tool_call("tc2", "devmcp__tool_0", json!({})),
        openai_text("done"),
    ])
    .await;
    let mut h = Harness::spawn_with_env(&llm.url, &identity_env()).await;
    let sid = new_session(
        &mut h,
        vec![server_decl("devmcp", true), server_decl("extra", false)],
    )
    .await;

    let p = h
        .send(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type":"text","text":"go"}]}),
        )
        .await;
    let r = h.recv_until_approving(|v| v["id"] == json!(p)).await;
    assert!(r.get("error").is_none(), "prompt errored: {r}");

    let captured = llm.captured.lock().await;
    assert_eq!(
        captured.len(),
        3,
        "expected 3 LLM calls (initial + 2 tool rounds), got {}",
        captured.len()
    );

    let untrusted = tool_result_text(&captured, 1);
    let untrusted_names = env_names(&untrusted);
    for var in IDENTITY_VARS {
        assert!(
            !untrusted_names.contains(var),
            "untrusted MCP server was spawned with {var}"
        );
    }

    let trusted = tool_result_text(&captured, 2);
    let trusted_names = env_names(&trusted);
    for var in IDENTITY_VARS {
        assert!(
            trusted_names.contains(var),
            "trusted MCP server lost {var}; \
             the untrusted assertion above would pass vacuously"
        );
    }

    h.shutdown().await;
}

/// The filter is narrow: an untrusted server still receives the rest of the
/// passthrough allowlist. An over-broad filter would silently break every
/// third-party server that needs `PATH` or `HOME`, and no other test would
/// catch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrusted_mcp_env_keeps_non_identity_passthrough() {
    let llm = spawn_capturing_llm(vec![
        openai_tool_call("tc1", "extra__tool_0", json!({})),
        openai_text("done"),
    ])
    .await;
    let mut h = Harness::spawn_with_env(&llm.url, &identity_env()).await;
    let sid = new_session(&mut h, vec![server_decl("extra", false)]).await;

    let p = h
        .send(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type":"text","text":"go"}]}),
        )
        .await;
    let r = h.recv_until_approving(|v| v["id"] == json!(p)).await;
    assert!(r.get("error").is_none(), "prompt errored: {r}");

    let captured = llm.captured.lock().await;
    let report = tool_result_text(&captured, 1);
    let names = env_names(&report);

    // PATH and HOME are unconditional passthrough entries; the display name
    // is a Buzz-owned but non-secret one, set on the parent above.
    for var in ["PATH", "HOME", "BUZZ_ACP_DISPLAY_NAME"] {
        assert!(
            names.contains(&var),
            "untrusted MCP server lost non-identity passthrough {var}: {report}"
        );
    }
    // The wire-declared env still arrives — that is the operator's own
    // declaration, not ambient parent state.
    assert!(
        names.contains(&"FAKE_MCP_ENV_REPORT"),
        "wire-declared env did not reach the server: {report}"
    );

    h.shutdown().await;
}
