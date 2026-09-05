//! End-to-end binding of `BUZZ_ACP_EXTRA_MCP_COMMANDS` — from the environment
//! variable an operator sets on the harness through to the tools `buzz-agent`
//! offers the model.
//!
//! The two halves of this seam used to be tested apart: `buzz-acp` proved the
//! variable becomes an `mcpServers` array, and `buzz-agent` proved an
//! `mcpServers` array spawns a server. Nothing proved the array `buzz-acp`
//! writes is one `buzz-agent` accepts, which is exactly how a server name one
//! byte over `McpRegistry`'s limit reached `session/new` unnoticed.
//!
//! This test runs the real chain in one process: the real environment
//! variable, the real `buzz-acp` argument parser, the real
//! `build_mcp_servers`, a real `buzz-agent` child, a real `session/new` off
//! the wire, two real MCP subprocesses, and the tool list the agent sends to
//! the model.
//!
//! It lives in its own test binary because it writes a process-global
//! environment variable, which must not race the process spawns of other
//! tests in the same binary.

mod common;

use clap::Parser;
use serde_json::{json, Value};

use common::{openai_text, openai_tool_call, spawn_capturing_llm, Harness};

/// A throwaway secp256k1 secret key (32 bytes of 0x11). `buzz-acp` requires a
/// parseable key to build a config; nothing in this test signs anything.
const TEST_PRIVATE_KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// A fixed channel id, so the git-origin assertions below name an exact value.
const CHANNEL_ID: uuid::Uuid = uuid::uuid!("00000000-0000-4000-8000-0000000000aa");

/// Every tool name the agent offered the model in its `n`th request, however
/// the provider nests it (`{type,function:{name}}` or a flat `{type,name}`).
fn offered_tool_names(captured: &[Value], n: usize) -> Vec<String> {
    let req = captured
        .get(n)
        .unwrap_or_else(|| panic!("no LLM request {n}"));
    req["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("LLM request {n} carries no tools array: {req}"))
        .iter()
        .filter_map(|t| {
            t.get("name")
                .or_else(|| t.get("function").and_then(|f| f.get("name")))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

/// `BUZZ_ACP_EXTRA_MCP_COMMANDS` reaches the model's tool list.
///
/// The extra command names the same executable as the primary MCP server, so
/// the derived name collides and `buzz-acp` must disambiguate it — and the
/// disambiguated name must be one `McpRegistry` accepts, spawns, and lists
/// tools for. A name the registry rejects fails `session/new` outright, so
/// this test goes red for any name-shaping bug on either side of the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untrusted_mcp_env_extra_command_reaches_tool_list() {
    let fake_mcp = env!("CARGO_BIN_EXE_fake-mcp");

    // The operator's half: the environment variable, parsed by buzz-acp's own
    // clap definition (`env = "BUZZ_ACP_EXTRA_MCP_COMMANDS"`, newline-split).
    std::env::set_var("BUZZ_ACP_EXTRA_MCP_COMMANDS", fake_mcp);
    // `--agent-command buzz-agent` is not decoration: buzz-acp refuses extra
    // MCP servers under any other adapter, because only buzz-agent honours the
    // `trusted` marker this whole test is about.
    let args = buzz_acp::CliArgs::try_parse_from([
        "buzz-acp",
        "--private-key",
        TEST_PRIVATE_KEY,
        "--agent-command",
        "buzz-agent",
        "--mcp-command",
        fake_mcp,
    ])
    .expect("buzz-acp CLI args parse");
    let config = buzz_acp::Config::from_args(args).expect("buzz-acp config");
    // The whole request path, not just the parser: `mcp_servers_wire_json`
    // runs `McpServerSet::from_config` and the per-session git-origin step the
    // pool applies on the way out, which is the last hop before `session/new`.
    let origin = buzz_acp::SessionOrigin {
        channel_id: Some(CHANNEL_ID),
        channel_type: Some("dm"),
        agent_name: Some("Builder"),
    };
    let servers = buzz_acp::mcp_servers_wire_json(&config, origin).expect("mcpServers wire JSON");

    let decls = servers.as_array().expect("mcpServers is an array");
    assert_eq!(decls.len(), 2, "primary + 1 extra: {servers}");
    assert_eq!(
        decls[0]["trusted"],
        json!(true),
        "the primary server is the trusted one: {servers}"
    );
    assert!(
        decls[1].get("trusted").is_none(),
        "an extra server is untrusted by omission: {servers}"
    );
    // The git-origin hop: a private (non-stream) channel contributes the agent
    // name, and only to the trusted server. Breaking either half of
    // `McpServerSet::for_session` fails here.
    let env_names = |decl: &Value| -> Vec<String> {
        decl["env"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|e| e["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(
        env_names(&decls[0]).contains(&"BUZZ_GIT_ORIGIN_AGENT_NAME".to_string()),
        "the trusted server lost its git origin: {servers}"
    );
    for var in ["BUZZ_GIT_ORIGIN_AGENT_NAME", "BUZZ_GIT_ORIGIN_CHANNEL_ID"] {
        assert!(
            !env_names(&decls[1]).contains(&var.to_string()),
            "the untrusted extra server was handed {var}: {servers}"
        );
    }

    let primary_name = decls[0]["name"].as_str().expect("primary name").to_owned();
    let extra_name = decls[1]["name"].as_str().expect("extra name").to_owned();
    assert_ne!(
        primary_name, extra_name,
        "the colliding derived name must be disambiguated: {servers}"
    );
    let extra_qname = format!("{extra_name}__tool_0");

    // The agent's half: the same array, off the wire, into a real session.
    let llm = spawn_capturing_llm(vec![
        openai_tool_call("tc1", &extra_qname, json!({})),
        openai_text("done"),
    ])
    .await;
    let mut h = Harness::spawn(&llm.url).await;
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
    assert!(
        r.get("error").is_none(),
        "session/new rejected the mcpServers array buzz-acp produced: {r}"
    );
    let sid = r["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_owned();

    let p = h
        .send(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type":"text","text":"go"}]}),
        )
        .await;
    let done = h.recv_until_approving(|v| v["id"] == json!(p)).await;
    assert!(done.get("error").is_none(), "prompt errored: {done}");

    let captured = llm.captured.lock().await;
    let offered = offered_tool_names(&captured, 0);
    assert!(
        offered.contains(&extra_qname),
        "the extra server's tool was not offered to the model: {offered:?}"
    );
    assert!(
        offered.contains(&format!("{primary_name}__tool_0")),
        "the primary server's tool was not offered to the model: {offered:?}"
    );

    // Listed is not enough — the tool must also be callable through the
    // disambiguated name, which proves the registry routed it to a live
    // process rather than merely accepting the declaration.
    let followup = captured
        .get(1)
        .and_then(|c| c["messages"].as_array())
        .expect("second LLM request carries the tool result");
    let tool_msg = followup
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .unwrap_or_else(|| panic!("no tool result in the second LLM request: {followup:?}"));
    assert_eq!(
        tool_msg["content"], "ok",
        "the extra server did not answer the call: {tool_msg}"
    );

    h.shutdown().await;
}

/// buzz-acp's startup-side server cap is the registry's cap, not a number that
/// drifted from it. Mirroring the bound is what keeps an over-long
/// configuration from producing a harness that starts, announces itself, and
/// then fails every `session/new`.
#[test]
fn mcp_server_cap_mirrors_buzz_agent() {
    assert_eq!(
        buzz_acp::MAX_MCP_SERVERS,
        buzz_agent::MAX_MCP_SERVERS,
        "buzz-acp's mirrored MCP server cap drifted from McpRegistry's"
    );
}

/// The registry chain, from the file the desktop writes to the environment the
/// server is spawned with.
///
/// The desktop stages a generation, names it in `BUZZ_ACP_MCP_REGISTRY`, and
/// puts `BUZZ_MCP_CAPABILITY` in the harness's spawn environment. buzz-acp
/// turns the file into `mcpServers` entries; buzz-agent spawns them. The
/// capability has to arrive at the child, because the child in production is
/// `buzz-mcp-launch`, which reads the variable from its own environment and
/// exits 1 with "BUZZ_MCP_CAPABILITY is not set" when it is absent -- so a
/// break anywhere on this path leaves the operator with a registry server that
/// silently never runs.
///
/// Everything but the launcher itself is real here: the real registry file
/// shape, the real `McpServerSet::from_config`, the real wire JSON, a real
/// buzz-agent child, a real `session/new`, and a real MCP subprocess reporting
/// the names of the variables it was spawned with.
///
/// The launcher stand-in is `fake-mcp` copied to the name and directory
/// `buzz-acp` resolves the bundled launcher at, because `buzz-acp` now refuses
/// any entry naming a different command: the `registry_launched` marker is
/// what hands a child the capability, so it may only ride the launcher.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_file_capability_reaches_the_spawned_server() {
    let fake_mcp = env!("CARGO_BIN_EXE_fake-mcp");
    let dir = tempfile::tempdir().expect("tempdir");

    // The launcher stand-in, beside this test binary — the directory
    // `buzz_acp::mcp_registry::bundled_launcher` resolves. Written through a
    // rename so a concurrent test binary never observes a partial copy.
    let exe = std::env::current_exe().expect("test binary path");
    let bin_dir = exe.parent().expect("test binary directory").to_path_buf();
    let launcher = bin_dir.join(buzz_acp::mcp_registry::LAUNCHER_FILE_NAME);
    let staging = bin_dir.join(format!("buzz-mcp-launch.{}.tmp", std::process::id()));
    std::fs::copy(fake_mcp, &staging).expect("copy the launcher stand-in");
    std::fs::rename(&staging, &launcher).expect("install the launcher stand-in");

    // The path the desktop stages, which is the only one buzz-acp accepts:
    // this agent's directory inside the adopted generation.
    const AGENT: &str = "a1b2";
    const GENERATION: u64 = 7;
    let nonce = "ab".repeat(16);
    let staged = dir
        .path()
        .join(buzz_acp::mcp_registry::GENERATIONS_DIR)
        .join(GENERATION.to_string())
        .join(buzz_acp::mcp_registry::AGENTS_DIR)
        .join(AGENT);
    std::fs::create_dir_all(&staged).expect("staged directory");
    let registry_path = staged.join(buzz_acp::mcp_registry::REGISTRY_FILE_NAME);
    // The shape `render_buzz_acp_registry` writes: launcher invocations whose
    // declared variables ride argv as references, and no `env` block at all —
    // a block would land on the launcher's own process, which is where the
    // capability lives.
    std::fs::write(
        &registry_path,
        serde_json::to_string(&json!({
            "version": 1,
            "servers": [{
                "name": "registry",
                "command": launcher.to_str().expect("utf-8 path"),
                "args": [
                    "--env-report",
                    "--secret", "TOKEN=mcp:registry-token",
                    "--set", "DYLD_INSERT_LIBRARIES=/tmp/evil.dylib",
                ],
            }],
        }))
        .expect("serializes"),
    )
    .expect("registry file written");

    // The desktop's half of the capability chain, in this process because
    // `build_mcp_servers` reads it from the process and nowhere else.
    std::env::set_var(
        "BUZZ_MCP_CAPABILITY",
        format!("v1.{AGENT}.{GENERATION}.{nonce}"),
    );

    let args = buzz_acp::CliArgs::try_parse_from([
        "buzz-acp",
        "--private-key",
        TEST_PRIVATE_KEY,
        "--agent-command",
        "buzz-agent",
        "--mcp-command",
        fake_mcp,
        "--mcp-registry",
        registry_path.to_str().expect("utf-8 path"),
    ])
    .expect("buzz-acp CLI args parse");
    let config = buzz_acp::Config::from_args(args).expect("buzz-acp config");
    let servers = buzz_acp::mcp_servers_wire_json(
        &config,
        buzz_acp::SessionOrigin {
            channel_id: Some(CHANNEL_ID),
            channel_type: Some("dm"),
            agent_name: Some("Builder"),
        },
    )
    .expect("mcpServers wire JSON");
    let decls = servers.as_array().expect("mcpServers is an array");
    let registry_decl = decls
        .iter()
        .find(|decl| decl["name"] == json!("registry"))
        .unwrap_or_else(|| panic!("the registry entry did not reach the wire: {servers}"));
    assert_eq!(
        registry_decl["registry_launched"],
        json!(true),
        "the registry entry crossed the wire without its marker: {servers}"
    );

    let llm = spawn_capturing_llm(vec![
        openai_tool_call("tc1", "registry__tool_0", json!({})),
        openai_text("done"),
    ])
    .await;
    // The desktop's half: the capability is in the harness's environment, and
    // buzz-acp passes its own environment to the agent it spawns.
    let capability = format!("v1.{AGENT}.{GENERATION}.{nonce}");
    let mut h = Harness::spawn_with_env(&llm.url, &[("BUZZ_MCP_CAPABILITY", &capability)]).await;
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
    assert!(
        r.get("error").is_none(),
        "session/new rejected the mcpServers array buzz-acp produced: {r}"
    );
    let sid = r["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_owned();

    let p = h
        .send(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type":"text","text":"go"}]}),
        )
        .await;
    let done = h.recv_until_approving(|v| v["id"] == json!(p)).await;
    assert!(done.get("error").is_none(), "prompt errored: {done}");

    let captured = llm.captured.lock().await;
    let report = captured
        .get(1)
        .and_then(|c| c["messages"].as_array())
        .expect("second LLM request carries the tool result")
        .iter()
        .rev()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .expect("tool result text")
        .to_owned();
    // Names only, never values: the fake server never echoes a variable's
    // contents, so a capability cannot reach the test output.
    let names: Vec<&str> = report.lines().map(str::trim).collect();
    assert!(
        names.contains(&"BUZZ_MCP_CAPABILITY"),
        "the registry server was spawned without the capability, so the real \
         launcher would have exited 1: {names:?}"
    );
    // The other half of memo decision 5's boundary: a declared variable rides
    // the launcher's argv and never its environment. `buzz-agent` applies
    // `McpServer.env` to this very process, so a duplicated block would put a
    // loader variable here — inside the process holding the capability, before
    // `main`. Restoring that duplication in the desktop's generator, or
    // accepting an `env` block in `buzz-acp`, fails these two assertions.
    for var in ["TOKEN", "DYLD_INSERT_LIBRARIES"] {
        assert!(
            !names.contains(&var),
            "a declared variable reached the launcher's own environment: {var} in {names:?}"
        );
    }
    for var in ["BUZZ_PRIVATE_KEY", "NOSTR_PRIVATE_KEY", "BUZZ_AUTH_TAG"] {
        assert!(
            !names.contains(&var),
            "a registry server is untrusted and must not receive {var}: {names:?}"
        );
    }

    h.shutdown().await;
}
