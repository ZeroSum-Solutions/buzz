//! Tests for the managed-agent runtime config generator.
//!
//! Every ceiling and charset rule below has its own test, so removing the guard
//! turns a test red rather than only widening what is accepted.

use super::*;

fn stdio_server() -> McpServerSpec {
    McpServerSpec::stdio(
        "openseo-fake",
        "/opt/fake-mcp",
        &["--stdio".to_string()],
        &["OPENSEO_TOKEN".to_string()],
    )
    .expect("valid stdio server")
}

fn skill(name: &str) -> PinnedSkill {
    PinnedSkill::new(name, "---\nname: probe\n---\n\n# Probe\n").expect("valid skill")
}

fn spec_with(servers: Vec<McpServerSpec>, skills: Vec<PinnedSkill>) -> AgentRuntimeConfigSpec {
    AgentRuntimeConfigSpec::new(servers, skills).expect("valid spec")
}

fn long(n: usize) -> String {
    "a".repeat(n)
}

// ---------------------------------------------------------------- count caps

#[test]
fn servers_over_the_cap_are_rejected() {
    let servers: Vec<McpServerSpec> = (0..=MAX_SERVERS)
        .map(|i| McpServerSpec::http(&format!("s{i}"), "https://example.test/mcp").expect("http"))
        .collect();
    let err = AgentRuntimeConfigSpec::new(servers, Vec::new()).expect_err("cap must reject");
    assert_eq!(
        err,
        ConfigGenError::TooMany {
            field: "servers",
            limit: MAX_SERVERS,
            got: MAX_SERVERS + 1
        }
    );
}

#[test]
fn servers_at_the_cap_are_accepted() {
    let servers: Vec<McpServerSpec> = (0..MAX_SERVERS)
        .map(|i| McpServerSpec::http(&format!("s{i}"), "https://example.test/mcp").expect("http"))
        .collect();
    assert!(AgentRuntimeConfigSpec::new(servers, Vec::new()).is_ok());
}

#[test]
fn skills_over_the_cap_are_rejected() {
    let skills: Vec<PinnedSkill> = (0..=MAX_SKILLS).map(|i| skill(&format!("s{i}"))).collect();
    let err = AgentRuntimeConfigSpec::new(Vec::new(), skills).expect_err("cap must reject");
    assert_eq!(
        err,
        ConfigGenError::TooMany {
            field: "skills",
            limit: MAX_SKILLS,
            got: MAX_SKILLS + 1
        }
    );
}

#[test]
fn args_over_the_cap_are_rejected() {
    let args: Vec<String> = (0..=MAX_ARGS).map(|i| i.to_string()).collect();
    let err = McpServerSpec::stdio("s", "/bin/true", &args, &[]).expect_err("cap must reject");
    assert!(matches!(
        err,
        ConfigGenError::TooMany {
            field: "server.args",
            ..
        }
    ));
}

#[test]
fn env_passthrough_over_the_cap_is_rejected() {
    let keys: Vec<String> = (0..=MAX_ENV_PASSTHROUGH).map(|i| format!("K{i}")).collect();
    let err = McpServerSpec::stdio("s", "/bin/true", &[], &keys).expect_err("cap must reject");
    assert!(matches!(
        err,
        ConfigGenError::TooMany {
            field: "server.env_passthrough",
            ..
        }
    ));
}

// ----------------------------------------------------------------- byte caps

#[test]
fn over_long_server_name_is_rejected() {
    let err = McpServerSpec::http(&long(MAX_NAME_BYTES + 1), "https://example.test/mcp")
        .expect_err("cap must reject");
    assert!(matches!(
        err,
        ConfigGenError::TooLong {
            field: "server.name",
            ..
        }
    ));
}

#[test]
fn over_long_command_is_rejected() {
    let err =
        McpServerSpec::stdio("s", &long(MAX_COMMAND_BYTES + 1), &[], &[]).expect_err("cap rejects");
    assert!(matches!(
        err,
        ConfigGenError::TooLong {
            field: "server.command",
            ..
        }
    ));
}

#[test]
fn over_long_argument_is_rejected() {
    let err = McpServerSpec::stdio("s", "/bin/true", &[long(MAX_ARG_BYTES + 1)], &[])
        .expect_err("cap rejects");
    assert!(matches!(
        err,
        ConfigGenError::TooLong {
            field: "server.args",
            index: 0,
            ..
        }
    ));
}

#[test]
fn over_long_url_is_rejected() {
    let url = format!("https://example.test/{}", long(MAX_URL_BYTES));
    let err = McpServerSpec::http("s", &url).expect_err("cap rejects");
    assert!(matches!(
        err,
        ConfigGenError::TooLong {
            field: "server.url",
            ..
        }
    ));
}

#[test]
fn over_long_env_name_is_rejected() {
    let err = McpServerSpec::stdio("s", "/bin/true", &[], &[long(MAX_ENV_NAME_BYTES + 1)])
        .expect_err("cap rejects");
    assert!(matches!(
        err,
        ConfigGenError::TooLong {
            field: "server.env_passthrough",
            ..
        }
    ));
}

#[test]
fn over_long_skill_body_is_rejected() {
    let err = PinnedSkill::new("probe", &long(MAX_SKILL_BODY_BYTES + 1)).expect_err("cap rejects");
    assert!(matches!(
        err,
        ConfigGenError::TooLong {
            field: "skill.body",
            ..
        }
    ));
}

// ------------------------------------------------------------------ charsets

#[test]
fn server_name_charset_is_enforced() {
    for bad in ["../evil", "a b", "a.b", "-lead", "", "sérver"] {
        assert!(
            McpServerSpec::http(bad, "https://example.test/mcp").is_err(),
            "{bad:?} must be rejected"
        );
    }
    assert_eq!(
        McpServerSpec::http("openseo-fake_1", "https://example.test/mcp")
            .expect("valid name")
            .name(),
        "openseo-fake_1"
    );
}

#[test]
fn skill_name_cannot_traverse_or_shadow_the_nest_skill() {
    for bad in ["../evil", "a/b", ".", "..", "buzz-cli"] {
        assert!(
            PinnedSkill::new(bad, "body").is_err(),
            "{bad:?} must be rejected"
        );
    }
}

#[test]
fn env_name_charset_is_enforced() {
    for bad in ["1LEAD", "with-dash", "with space", ""] {
        assert!(
            McpServerSpec::stdio("s", "/bin/true", &[], &[bad.to_string()]).is_err(),
            "{bad:?} must be rejected"
        );
    }
}

#[test]
fn non_http_url_is_rejected() {
    assert!(McpServerSpec::http("s", "file:///etc/passwd").is_err());
    assert!(McpServerSpec::http("s", "ws://example.test").is_err());
}

/// The URL is parsed, not prefix-matched. Replacing `check_http_url` with a
/// `starts_with` test leaves every rejection below red.
#[test]
fn an_endpoint_url_must_be_a_usable_secure_endpoint() {
    for bad in [
        // plaintext to somewhere other than this machine
        "http://app.openseo.so/mcp",
        "http://10.0.0.5:8080/mcp",
        // a credential smuggled into a file this generator writes no secret to
        "https://user:token@app.openseo.so/mcp",
        "https://user@app.openseo.so/mcp",
        // no host at all
        "http:///mcp",
        "http://",
        // the same, over https: `url` normalizes this to `https://mcp/`, so the
        // raw authority is what refuses it
        "https:///mcp",
        // an empty userinfo, which parsing drops entirely
        "https://@app.openseo.so/mcp",
        "https://:@app.openseo.so/mcp",
        // a fragment the server never receives
        "https://app.openseo.so/mcp#anchor",
        // not a URL at all
        "not a url",
        "/mcp",
    ] {
        assert!(
            McpServerSpec::http("s", bad).is_err(),
            "{bad:?} must be rejected"
        );
    }
    for good in [
        "http://127.0.0.1:8080/mcp",
        "http://localhost:8080/mcp",
        "http://[::1]:8080/mcp",
        "https://app.openseo.so/mcp",
    ] {
        assert!(
            McpServerSpec::http("s", good).is_ok(),
            "{good:?} must be accepted"
        );
    }
}

#[test]
fn empty_command_and_empty_body_are_rejected() {
    assert!(McpServerSpec::stdio("s", "   ", &[], &[]).is_err());
    assert!(PinnedSkill::new("probe", "").is_err());
}

#[test]
fn duplicate_names_are_rejected() {
    let dup_servers = vec![
        McpServerSpec::http("same", "https://a.test/mcp").expect("http"),
        McpServerSpec::http("same", "https://b.test/mcp").expect("http"),
    ];
    assert!(matches!(
        AgentRuntimeConfigSpec::new(dup_servers, Vec::new()),
        Err(ConfigGenError::Duplicate {
            field: "servers",
            index: 1
        })
    ));
    let dup_skills = vec![skill("same"), skill("same")];
    assert!(matches!(
        AgentRuntimeConfigSpec::new(Vec::new(), dup_skills),
        Err(ConfigGenError::Duplicate {
            field: "skills",
            index: 1
        })
    ));
    assert!(McpServerSpec::stdio(
        "s",
        "/bin/true",
        &[],
        &["DUP".to_string(), "DUP".to_string()]
    )
    .is_err());
}

// ------------------------------------------------------- structural emission

#[test]
fn claude_json_parses_back_to_the_declared_structure() {
    let spec = spec_with(vec![stdio_server()], vec![skill("probe")]);
    let rendered = render_claude_mcp_json(&spec).expect("render");
    let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid JSON");
    let entry = &parsed["mcpServers"]["openseo-fake"];
    assert_eq!(entry["type"], "stdio");
    assert_eq!(entry["command"], "/opt/fake-mcp");
    assert_eq!(entry["args"][0], "--stdio");
    let env = entry["env"].as_object().expect("env object");
    assert_eq!(
        env.keys().collect::<Vec<_>>(),
        vec!["OPENSEO_TOKEN"],
        "env carries names only"
    );
    assert_eq!(env["OPENSEO_TOKEN"], "${OPENSEO_TOKEN}");
}

#[test]
fn claude_http_entry_uses_the_published_openseo_shape() {
    let spec = spec_with(
        vec![McpServerSpec::http("openseo", "https://app.openseo.so/mcp").expect("http")],
        Vec::new(),
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&render_claude_mcp_json(&spec).expect("render")).expect("JSON");
    assert_eq!(parsed["mcpServers"]["openseo"]["type"], "http");
    assert_eq!(
        parsed["mcpServers"]["openseo"]["url"],
        "https://app.openseo.so/mcp"
    );
}

#[test]
fn codex_toml_parses_back_to_the_declared_structure() {
    let spec = spec_with(vec![stdio_server()], vec![skill("probe")]);
    let rendered = render_codex_config_toml(&spec).expect("render");
    let table: toml::Table = rendered.parse().expect("valid TOML");
    let entry = &table["mcp_servers"]["openseo-fake"];
    assert_eq!(entry["command"].as_str(), Some("/opt/fake-mcp"));
    assert_eq!(
        entry["args"].as_array().expect("args")[0].as_str(),
        Some("--stdio")
    );
    assert_eq!(
        entry["env_vars"]
            .as_array()
            .expect("env_vars array")
            .iter()
            .map(|v| v.as_str().expect("a name"))
            .collect::<Vec<_>>(),
        vec!["OPENSEO_TOKEN"]
    );
}

/// Codex reads `env` values as literal strings and interpolates nothing
/// (`codex-rs/config/src/mcp_types.rs`, `RawMcpServerConfig::env:
/// Option<HashMap<String, String>>`), so the `${NAME}` placeholder Claude
/// expands would reach the server as those characters. Forwarding is `env_vars`
/// — a list of NAMES Codex takes from its own environment. Reinstating the env
/// table, or writing a placeholder into it, fails here.
#[test]
fn codex_forwards_environment_by_name_rather_than_by_placeholder() {
    let spec = spec_with(
        vec![McpServerSpec::stdio(
            "probe",
            "/bin/true",
            &[],
            &["ALPHA_TOKEN".to_string(), "BETA_TOKEN".to_string()],
        )
        .expect("stdio")],
        Vec::new(),
    );
    let rendered = render_codex_config_toml(&spec).expect("render");
    let table: toml::Table = rendered.parse().expect("valid TOML");
    let entry = &table["mcp_servers"]["probe"];
    assert_eq!(
        entry["env_vars"]
            .as_array()
            .expect("env_vars array")
            .iter()
            .map(|v| v.as_str().expect("a name"))
            .collect::<Vec<_>>(),
        vec!["ALPHA_TOKEN", "BETA_TOKEN"],
        "the exact names, in the order the spec declared them"
    );
    assert!(
        entry.get("env").is_none(),
        "no env table: Codex would pass its values through verbatim"
    );
    assert!(
        !rendered.contains("${"),
        "no placeholder anywhere in the Codex document: {rendered}"
    );
}

/// The generated Claude document is one Buzz's own Claude reader understands.
/// The reader resolves `mcpServers` from `<dir>/.claude.json`; a project
/// `.mcp.json` uses the identical object, so the document is copied under that
/// name to run the production parser over it.
#[test]
fn generated_claude_document_round_trips_through_the_production_reader() {
    let dir = tempfile::tempdir().expect("tempdir");
    let spec = spec_with(
        vec![
            stdio_server(),
            McpServerSpec::http("openseo", "https://app.openseo.so/mcp").expect("http"),
        ],
        Vec::new(),
    );
    std::fs::write(
        dir.path().join(".claude.json"),
        render_claude_mcp_json(&spec).expect("render"),
    )
    .expect("write");
    let cfg = crate::managed_agents::config_bridge::read_claude_config_at(dir.path())
        .expect("reader accepts the generated document");
    let mut names: Vec<&str> = cfg.extensions.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["openseo", "openseo-fake"]);
}

/// The generated Codex document is one Buzz's own Codex parser understands.
///
/// The names are all this can assert: `config_bridge::codex::parse_mcp_servers`
/// discards each server's table and returns an `ExtensionEntry` of name, kind
/// and enabled, so no reader-side seam exists to bind `env_vars` through. The
/// value assertion lives in
/// [`codex_forwards_environment_by_name_rather_than_by_placeholder`].
#[test]
fn generated_codex_document_round_trips_through_the_production_reader() {
    let spec = spec_with(
        vec![
            stdio_server(),
            McpServerSpec::http("openseo", "https://app.openseo.so/mcp").expect("http"),
        ],
        Vec::new(),
    );
    let cfg = crate::managed_agents::config_bridge::parse_codex_config_str(
        &render_codex_config_toml(&spec).expect("render"),
    )
    .expect("parser accepts the generated document");
    let mut names: Vec<&str> = cfg.extensions.iter().map(|e| e.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["openseo", "openseo-fake"]);
}

#[test]
fn no_generated_file_carries_an_environment_value() {
    let spec = spec_with(
        vec![
            McpServerSpec::stdio("s", "/bin/true", &[], &["SECRET_TOKEN".to_string()])
                .expect("stdio"),
        ],
        Vec::new(),
    );
    let claude = render_claude_mcp_json(&spec).expect("json");
    assert!(
        claude.contains("\"${SECRET_TOKEN}\""),
        "Claude gets the placeholder it expands, never a value: {claude}"
    );
    let codex = render_codex_config_toml(&spec).expect("toml");
    assert!(
        codex.contains("env_vars = [\"SECRET_TOKEN\"]"),
        "Codex gets the name to forward, never a value: {codex}"
    );
    assert!(
        !codex.contains(".env]"),
        "and no env table, whose values Codex reads literally: {codex}"
    );
}

// ------------------------------------------------------- placement and paths

#[test]
fn skill_directories_come_from_the_runtime_catalog() {
    assert_eq!(runtime_skill_dir("claude"), Ok(".claude/skills"));
    assert_eq!(runtime_skill_dir("codex"), Ok(".codex/skills"));
    assert!(matches!(
        runtime_skill_dir("not-a-runtime"),
        Err(ConfigGenError::UnknownRuntime { .. })
    ));
}

/// The project `.mcp.json` must land in the directory `runtime.rs` gives the
/// child. Both resolvers start from the same candidate — `nest_dir()` — but
/// `default_agent_workdir` falls back to `$HOME` when the nest is absent and
/// this one deliberately does not, so comparing the two resolved values is only
/// meaningful where a nest exists. A machine without `~/.buzz` (every CI runner
/// is one) would otherwise compare `None` against `$HOME` and fail on the
/// fallback rather than on anything this generator does.
///
/// So the PUBLIC resolver is asserted directly, on both branches, against a
/// nest this test owns: the nest lookup it consults is injected for the length
/// of each closure (`with_test_nest`, a `#[cfg(test)]` seam — production
/// compiles only the real lookup). Both branches run in this one process, and
/// neither reads or writes the operator's home. Making
/// `claude_project_config_root` return `None` unconditionally leaves the first
/// assertion red.
#[test]
fn claude_project_root_is_the_spawn_working_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let nest = dir.path().join(".buzz");
    std::fs::create_dir(&nest).expect("mkdir");

    // A nest that exists is the root, unchanged: the spawn path's working
    // directory is the nest itself, so this generator writes into that exact
    // directory and never a parent, a child or a resolved target.
    with_test_nest(Some(nest.clone()), || {
        assert_eq!(
            claude_project_config_root(),
            Some(nest.clone()),
            "an existing nest is the write root, verbatim"
        );
    });

    // A nest that is absent yields nothing. `default_agent_workdir` falls back
    // to `$HOME` in this branch; this generator must not, or a generation on a
    // machine with no nest would write the agent's config — and its skills —
    // into the operator's own home directory.
    std::fs::remove_dir(&nest).expect("rmdir");
    with_test_nest(Some(nest.clone()), || {
        assert_eq!(
            claude_project_config_root(),
            None,
            "a missing nest is not a reason to fall back to $HOME"
        );
    });

    // And a machine that names no nest at all resolves to nothing either.
    with_test_nest(None, || {
        assert_eq!(
            claude_project_config_root(),
            None,
            "no nest candidate is no write root"
        );
    });

    // Where the machine running this does have a nest, the two resolvers must
    // name the same directory. This is the end-to-end agreement, on top of the
    // branch assertions above rather than instead of them.
    if let Some(root) = claude_project_config_root() {
        assert_eq!(
            Some(root),
            crate::managed_agents::default_agent_workdir(),
            "with a nest present, the generator's root is the spawn working directory"
        );
    }
}

#[test]
fn claude_write_places_skills_then_the_project_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let spec = spec_with(vec![stdio_server()], vec![skill("probe")]);

    let planned = plan_claude_paths(root, &spec).expect("plan");
    let written = write_claude_project_config(root, &spec).expect("write");
    assert_eq!(planned, written, "the plan is exactly what is written");
    assert_eq!(
        written,
        vec![
            root.join(".claude/skills/probe/SKILL.md"),
            root.join(".claude/settings.local.json"),
            root.join(".mcp.json"),
        ],
        "skills first, then the approval, the config that activates them last"
    );
    assert!(root.join(".claude/skills/probe/SKILL.md").is_file());
    let on_disk = std::fs::read_to_string(root.join(".mcp.json")).expect("read");
    assert_eq!(on_disk, render_claude_mcp_json(&spec).expect("render"));
}

#[test]
fn codex_write_places_skills_then_the_codex_home_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("workdir");
    let codex_home = dir.path().join("codex-home");
    let spec = spec_with(vec![stdio_server()], vec![skill("probe")]);

    let planned = plan_codex_paths(&root, &codex_home, &spec).expect("plan");
    let written = write_codex_config(&root, &codex_home, &spec).expect("write");
    assert_eq!(planned, written);
    assert_eq!(
        written,
        vec![
            root.join(".codex/skills/probe/SKILL.md"),
            codex_home.join("config.toml"),
        ]
    );
    assert!(codex_home.join("config.toml").is_file());
}

/// The catalog places one named pinned skill where each runtime discovers it.
#[test]
fn each_runtime_discovers_its_pinned_skill_at_the_catalog_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let spec = spec_with(Vec::new(), vec![skill("openseo-smoke")]);
    write_claude_project_config(root, &spec).expect("claude write");
    write_codex_config(root, &root.join("codex-home"), &spec).expect("codex write");
    for runtime in ["claude", "codex"] {
        let skill_dir = runtime_skill_dir(runtime).expect("catalog skill dir");
        let listed: Vec<String> = std::fs::read_dir(root.join(skill_dir))
            .expect("skill dir exists")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            listed,
            vec!["openseo-smoke".to_string()],
            "{runtime} catalog lists exactly the pinned skill"
        );
        assert!(root
            .join(skill_dir)
            .join("openseo-smoke")
            .join("SKILL.md")
            .is_file());
    }
}

// ------------------------------------------------------------- torn state

/// A failure part-way through must leave a consistent prefix: the config file
/// that grants the agent the servers is never written when a skill write fails.
#[test]
fn a_failed_skill_write_leaves_no_mcp_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    // Occupy the second skill's directory path with a regular file so
    // `create_dir_all` for it fails inside the production writer.
    std::fs::create_dir_all(root.join(".claude/skills")).expect("mkdir");
    std::fs::write(root.join(".claude/skills/second"), b"blocker").expect("write blocker");

    let spec = spec_with(vec![stdio_server()], vec![skill("first"), skill("second")]);
    let err = write_claude_project_config(root, &spec).expect_err("must fail");
    assert!(
        matches!(&err, ConfigGenError::Io { path, .. } if path.contains("second")),
        "the failure names the path it failed on: {err}"
    );
    assert!(
        root.join(".claude/skills/first/SKILL.md").is_file(),
        "the prefix written before the failure is complete"
    );
    assert!(
        !root.join(".mcp.json").exists(),
        "no server is activated by a partial write"
    );
}

#[test]
fn no_temporary_file_survives_a_successful_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_claude_project_config(root, &spec_with(vec![stdio_server()], vec![skill("probe")]))
        .expect("write");
    let leftovers: Vec<String> = std::fs::read_dir(root)
        .expect("read root")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains("buzz-config-gen"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

// -------------------------------------------- the operator's config is safe

/// Nothing this module writes can land in the operator's own Claude or Codex
/// configuration. The planners report the complete write set, and every path in
/// it is under the caller's root.
#[test]
fn generation_never_targets_the_operators_own_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let codex_home = root.join("codex-home");
    let spec = spec_with(vec![stdio_server()], vec![skill("probe")]);

    let home = dirs::home_dir().expect("home");
    let forbidden = [
        home.join(".claude.json"),
        home.join(".claude").join("settings.json"),
        home.join(".codex").join("config.toml"),
        home.join(".mcp.json"),
    ];
    let home_state: Vec<bool> = forbidden.iter().map(|p| p.exists()).collect();

    let mut all = plan_claude_paths(root, &spec).expect("claude plan");
    all.extend(plan_codex_paths(root, &codex_home, &spec).expect("codex plan"));
    all.extend(write_claude_project_config(root, &spec).expect("claude write"));
    all.extend(write_codex_config(root, &codex_home, &spec).expect("codex write"));

    for path in &all {
        assert!(
            path.starts_with(root),
            "{} escapes the caller's root",
            path.display()
        );
        assert!(
            !forbidden.contains(path),
            "{} is the operator's own config",
            path.display()
        );
    }
    assert_eq!(
        home_state,
        forbidden.iter().map(|p| p.exists()).collect::<Vec<bool>>(),
        "generation changed whether an operator config file exists"
    );
}

// ------------------------------------------------- case-insensitive collisions

/// APFS and NTFS are case-insensitive, so a case variant of a reserved name is
/// the same directory. Pure logic — the host filesystem does not decide it.
#[test]
fn a_reserved_skill_name_is_refused_in_any_case() {
    for bad in ["buzz-cli", "BUZZ-CLI", "Buzz-Cli"] {
        assert!(
            PinnedSkill::new(bad, "body").is_err(),
            "{bad:?} resolves to the nest's own buzz-cli skill directory"
        );
    }
}

/// Two skills differing only in case are one directory on the filesystems this
/// ships on, so the second would silently overwrite the first.
#[test]
fn skill_names_that_differ_only_in_case_are_a_duplicate() {
    let skills = vec![skill("Audit"), skill("audit")];
    assert!(matches!(
        AgentRuntimeConfigSpec::new(Vec::new(), skills),
        Err(ConfigGenError::Duplicate {
            field: "skills",
            index: 1
        })
    ));
}

/// Server names collide the same way in the qualified `<server>__<tool>` name.
#[test]
fn server_names_that_differ_only_in_case_are_a_duplicate() {
    let servers = vec![
        McpServerSpec::http("Audit", "https://a.test/mcp").expect("http"),
        McpServerSpec::http("audit", "https://b.test/mcp").expect("http"),
    ];
    assert!(matches!(
        AgentRuntimeConfigSpec::new(servers, Vec::new()),
        Err(ConfigGenError::Duplicate {
            field: "servers",
            index: 1
        })
    ));
}

// ------------------------------------------------------- identity env deny-list

/// Forwarding one of these would expand the agent's own signing key into an
/// arbitrary stdio MCP child, which could then sign as the agent on the relay.
#[test]
fn the_agents_identity_variables_cannot_be_forwarded() {
    assert_eq!(
        DENIED_ENV_NAMES,
        [
            "BUZZ_PRIVATE_KEY",
            "NOSTR_PRIVATE_KEY",
            "BUZZ_RELAY_URL",
            "BUZZ_AUTH_TAG"
        ]
    );
    for denied in DENIED_ENV_NAMES {
        for spelling in [
            (*denied).to_string(),
            denied.to_ascii_lowercase(),
            format!("{}{}", &denied[..1], denied[1..].to_ascii_lowercase()),
        ] {
            let err = McpServerSpec::stdio("s", "/bin/true", &[], std::slice::from_ref(&spelling))
                .expect_err("an identity variable must be refused");
            assert!(
                matches!(
                    err,
                    ConfigGenError::Invalid {
                        field: "server.env_passthrough",
                        ..
                    }
                ),
                "{spelling} was accepted: {err}"
            );
        }
    }
    // A neighbouring name is still allowed, so the rule is a deny-list and not
    // a prefix ban.
    assert!(McpServerSpec::stdio("s", "/bin/true", &[], &["BUZZ_PUBLIC_KEY".to_string()]).is_ok());
}

// --------------------------------------------- the write root is Buzz-owned

/// `default_agent_workdir` falls back to `$HOME`; this root must not. A
/// missing nest yields `None` — the caller writes nothing — and a symlinked
/// nest is refused rather than resolved.
#[test]
fn the_project_root_is_the_nest_or_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let real = dir.path().join("nest");
    std::fs::create_dir(&real).expect("mkdir");
    assert_eq!(buzz_owned_root(Some(real.clone())), Some(real.clone()));

    let missing = dir.path().join("absent");
    assert_eq!(
        buzz_owned_root(Some(missing.clone())),
        None,
        "a missing nest must not fall back to the operator's home directory"
    );

    #[cfg(unix)]
    {
        let link = dir.path().join("linked-nest");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert!(link.is_dir(), "the symlink does resolve to a directory");
        assert_eq!(
            buzz_owned_root(Some(link)),
            None,
            "a symlinked nest is a redirect, not a root"
        );
    }

    assert_eq!(buzz_owned_root(None), None);
}

#[test]
fn the_project_root_is_never_the_operators_home_directory() {
    if let Some(root) = claude_project_config_root() {
        assert_eq!(
            Some(root.clone()),
            crate::managed_agents::nest_dir(),
            "the only root this generator offers is the Buzz nest"
        );
        assert_ne!(
            Some(root),
            dirs::home_dir(),
            "the operator's home directory is never a write root"
        );
    }
}

// ------------------------------------------- symlinks and directory permissions

/// A managed agent can write in the shared root, so it can plant a symlink
/// where a skill directory belongs. The write must fail there rather than
/// resolve through it.
#[cfg(unix)]
#[test]
fn a_planted_symlink_in_the_skill_path_is_refused_not_followed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(root.join(".claude/skills")).expect("mkdir");
    std::fs::create_dir_all(&outside).expect("mkdir outside");
    std::fs::write(outside.join("SKILL.md"), b"the victim's own skill").expect("seed");
    std::os::unix::fs::symlink(&outside, root.join(".claude/skills/victim")).expect("symlink");
    let outside_mode = mode_of(&outside);

    let spec = spec_with(vec![stdio_server()], vec![skill("victim")]);
    let err = write_claude_project_config(&root, &spec).expect_err("must refuse the symlink");
    assert!(
        matches!(&err, ConfigGenError::Io { path, source }
            if path.contains("victim") && source.contains("symlink")),
        "the failure names the planted path: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("SKILL.md")).expect("read"),
        "the victim's own skill",
        "nothing was written through the symlink"
    );
    assert_eq!(
        mode_of(&outside),
        outside_mode,
        "the symlink target was not re-permissioned"
    );
    assert!(
        !root.join(".mcp.json").exists(),
        "no server is activated by a refused write"
    );
}

#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o7777
}

/// The guard that keeps a generated file private. Removing the `0o600` leaves
/// this red.
#[cfg(unix)]
#[test]
fn generated_files_are_owner_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let spec = spec_with(vec![stdio_server()], vec![skill("probe")]);
    for path in write_claude_project_config(root, &spec).expect("write") {
        assert_eq!(
            mode_of(&path),
            0o600,
            "{} is not owner-only",
            path.display()
        );
    }
}

/// The guard that keeps a directory this module creates private — and the one
/// that stops it re-permissioning a directory it did not create. Removing
/// either leaves this red.
#[cfg(unix)]
#[test]
fn created_directories_are_owner_only_and_the_caller_s_root_is_untouched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    std::fs::create_dir(&root).expect("mkdir");
    set_mode(&root, 0o755);
    let root_mode = mode_of(&root);

    let spec = spec_with(vec![stdio_server()], vec![skill("probe")]);
    write_claude_project_config(&root, &spec).expect("write");

    for created in [".claude", ".claude/skills", ".claude/skills/probe"] {
        assert_eq!(
            mode_of(&root.join(created)),
            0o700,
            "{created} is not owner-only"
        );
    }
    assert_eq!(
        mode_of(&root),
        root_mode,
        "the caller's root was re-permissioned; it belongs to whoever made it"
    );
}

/// The `.mcp.json` parent *is* the root, so the previous
/// `set_permissions(parent, 0o700)` chmod-ed the caller's root on every plain
/// successful run. This pins that it no longer does, on a root that is not a
/// tempdir default.
#[cfg(unix)]
#[test]
fn a_pre_existing_directory_in_the_chain_keeps_its_permissions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join(".claude/skills")).expect("mkdir");
    set_mode(&root.join(".claude"), 0o755);
    let claude_mode = mode_of(&root.join(".claude"));

    let spec = spec_with(vec![stdio_server()], vec![skill("probe")]);
    write_claude_project_config(&root, &spec).expect("write");
    assert_eq!(mode_of(&root.join(".claude")), claude_mode);
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A symlink standing where a generated config file belongs is reported, not
/// replaced — even though `rename` would replace it safely.
#[cfg(unix)]
#[test]
fn a_symlink_where_the_config_file_belongs_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    std::fs::create_dir(&root).expect("mkdir");
    let decoy = dir.path().join("decoy.json");
    std::fs::write(&decoy, b"{}").expect("seed");
    std::os::unix::fs::symlink(&decoy, root.join(".mcp.json")).expect("symlink");

    let err = write_claude_project_config(&root, &spec_with(vec![stdio_server()], Vec::new()))
        .expect_err("must refuse");
    assert!(
        matches!(&err, ConfigGenError::Io { source, .. } if source.contains("symlink")),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&decoy).expect("read"), "{}");
}

/// The staging name must not be derivable from the process id: another writer
/// in the shared root could compute it and pre-plant a symlink there.
///
/// Asserted on the name function itself. A staging file exists only between
/// `create_new` and `rename`, so walking the tree after a successful write sees
/// only renamed-away files and would stay green if a `<pid>.<counter>` name
/// came back — that check proves the writes finished, not that the name is
/// unpredictable. Restoring a pid-derived name fails this test.
#[test]
fn the_staging_name_is_not_derived_from_the_process_id() {
    let pid = std::process::id().to_string();
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..8 {
        let name = super::write::temp_name();
        assert!(
            !name.contains(&pid),
            "the staging name carries this process's id: {name}"
        );
        assert!(
            seen.insert(name.clone()),
            "two staging names collided, so the name is predictable: {name}"
        );
    }
}

/// And no staging file survives a run, anywhere under the root: each is renamed
/// into place or cleaned up. The recursive counterpart of
/// [`no_temporary_file_survives_a_successful_write`], which reads the root
/// directory only and so cannot see one left in a skill directory.
#[test]
fn no_staging_file_survives_anywhere_under_the_root() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    for i in 0..4 {
        let spec = spec_with(Vec::new(), vec![skill(&format!("probe{i}"))]);
        write_claude_project_config(root, &spec).expect("write");
    }
    for path in walk(root) {
        let name = path.to_string_lossy().into_owned();
        assert!(
            !name.contains("buzz-config-gen"),
            "staging file left: {name}"
        );
    }
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path);
        }
    }
    out
}

// ------------------------------------------ Claude's scoped MCP approval list

/// Claude ignores a project-scoped MCP server the project has not approved, so
/// `.mcp.json` alone is a silent no-tool run. Removing the approval write
/// leaves this red.
#[test]
fn claude_generation_approves_exactly_the_servers_it_declares() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let spec = spec_with(
        vec![
            stdio_server(),
            McpServerSpec::http("openseo", "https://app.openseo.so/mcp").expect("http"),
        ],
        vec![skill("probe")],
    );
    let written = write_claude_project_config(root, &spec).expect("write");
    assert_eq!(
        written,
        vec![
            root.join(".claude/skills/probe/SKILL.md"),
            root.join(".claude/settings.local.json"),
            root.join(".mcp.json"),
        ],
        "skills, then the approval, then the config that activates them"
    );
    let doc: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(".claude/settings.local.json")).expect("read"),
    )
    .expect("JSON");
    let approved: Vec<&str> = doc["enabledMcpjsonServers"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(approved, vec!["openseo-fake", "openseo"]);
    assert!(
        doc.get("enableAllProjectMcpServers").is_none(),
        "the list is scoped to these servers, never a blanket approval"
    );
}

#[test]
fn the_approval_list_preserves_other_settings_and_is_absent_without_servers() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".claude")).expect("mkdir");
    std::fs::write(
        root.join(".claude/settings.local.json"),
        b"{\"permissions\":{\"allow\":[\"Bash\"]},\"enabledMcpjsonServers\":[\"stale\"]}",
    )
    .expect("seed");

    write_claude_project_config(root, &spec_with(vec![stdio_server()], Vec::new())).expect("write");
    let doc: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join(".claude/settings.local.json")).expect("read"),
    )
    .expect("JSON");
    assert_eq!(doc["permissions"]["allow"][0], "Bash", "other keys survive");
    assert_eq!(doc["enabledMcpjsonServers"][0], "openseo-fake");
    assert_eq!(
        doc["enabledMcpjsonServers"]
            .as_array()
            .expect("array")
            .len(),
        1,
        "the list names this spec's servers, not a stale one"
    );

    let skills_only = tempfile::tempdir().expect("tempdir");
    let written =
        write_claude_project_config(skills_only.path(), &spec_with(Vec::new(), vec![skill("s")]))
            .expect("write");
    assert!(
        !written
            .iter()
            .any(|p| p.ends_with(".claude/settings.local.json")),
        "no servers, nothing to approve"
    );
}

/// A regenerated spec that declares no server must not leave a previous
/// approval list live: `.mcp.json` is rewritten with no servers, so an
/// `enabledMcpjsonServers` naming the old ones would keep approving what this
/// generation does not declare. Restoring the `if !spec.servers().is_empty()`
/// guard around the settings write leaves this red.
#[test]
fn a_zero_server_generation_clears_a_previous_approval_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    write_claude_project_config(root, &spec_with(vec![stdio_server()], Vec::new())).expect("write");
    let settings = root.join(".claude/settings.local.json");
    let seeded: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).expect("read")).expect("JSON");
    assert_eq!(seeded["enabledMcpjsonServers"][0], "openseo-fake");

    // Regenerate the same root from a spec that declares no server at all.
    let written = write_claude_project_config(root, &spec_with(Vec::new(), vec![skill("probe")]))
        .expect("write");
    assert!(
        written.contains(&settings),
        "the existing settings file is rewritten, not skipped: {written:?}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).expect("read")).expect("JSON");
    assert!(
        doc.get("enabledMcpjsonServers").is_none(),
        "no servers, so no approval survives: {doc}"
    );
    let mcp: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join(".mcp.json")).expect("read"))
            .expect("JSON");
    assert!(
        mcp["mcpServers"].as_object().expect("object").is_empty(),
        "and the config it approved is gone too"
    );
}

/// A settings file this module cannot parse is reported, never replaced.
#[test]
fn an_unparseable_settings_file_is_reported_not_clobbered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".claude")).expect("mkdir");
    std::fs::write(root.join(".claude/settings.local.json"), b"not json").expect("seed");
    let err = write_claude_project_config(root, &spec_with(vec![stdio_server()], Vec::new()))
        .expect_err("must refuse");
    assert!(
        matches!(&err, ConfigGenError::Io { source, .. } if source.contains("not valid JSON")),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".claude/settings.local.json")).expect("read"),
        "not json"
    );
    assert!(!root.join(".mcp.json").exists(), "and nothing is activated");
}

/// An empty or truncated settings file is not an empty object: it is a file
/// whose content this module cannot account for. Treating whitespace as `{}`
/// silently replaces whatever the file used to hold.
#[test]
fn a_whitespace_only_settings_file_is_reported_not_clobbered() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".claude")).expect("mkdir");
    std::fs::write(root.join(".claude/settings.local.json"), b"   \n\t\n").expect("seed");
    let err = write_claude_project_config(root, &spec_with(vec![stdio_server()], Vec::new()))
        .expect_err("must refuse");
    assert!(
        matches!(&err, ConfigGenError::Io { source, .. } if source.contains("not valid JSON")),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join(".claude/settings.local.json")).expect("read"),
        "   \n\t\n",
        "the file is left exactly as it was"
    );
    assert!(!root.join(".mcp.json").exists(), "and nothing is activated");
}

/// A settings path that cannot be read is an error, never "no settings file
/// here". Reading the failure as absence would skip the approval rewrite and
/// generate anyway. A directory standing where the file belongs is the cheapest
/// portable way to make both the metadata and the read disagree with the
/// expected shape.
#[test]
fn an_unreadable_settings_path_fails_closed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join(".claude/settings.local.json")).expect("mkdir");
    let err = write_claude_project_config(root, &spec_with(vec![stdio_server()], Vec::new()))
        .expect_err("must refuse");
    assert!(
        matches!(&err, ConfigGenError::Io { path, .. } if path.contains("settings.local.json")),
        "{err}"
    );
    assert!(
        root.join(".claude/settings.local.json").is_dir(),
        "the path is untouched"
    );
    assert!(!root.join(".mcp.json").exists(), "and nothing is activated");
}
