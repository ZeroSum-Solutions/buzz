//! Shell-callable seam into `managed_agents::agent_config_gen`.
//!
//! `scripts/zs/openseo-smoke.sh` drives this so the smoke test generates its
//! configuration with the same code the app links, instead of a shell
//! reimplementation that could drift from it. It is an example target, not a
//! shipped binary: nothing in the app depends on it, and it is built by
//! `just desktop-tauri-clippy` (which passes `--all-targets`) so it cannot rot.
//!
//! ```text
//! openseo-config-emit generate --runtime claude|codex --root DIR
//!     [--codex-home DIR] [--server-name NAME]
//!     [--server-command PATH | --server-url URL]
//!     [--server-arg ARG]... [--server-env NAME]...
//!     [--skill NAME=FILE]...
//! openseo-config-emit verify --runtime claude|codex --root DIR [--codex-home DIR]
//! ```
//!
//! `verify` parses the generated file back and prints one tab-separated record
//! per server (`server<TAB>kind<TAB>target<TAB>args<TAB>env-names`) and one per
//! discovered skill (`skill<TAB>name`), so the caller asserts on structure
//! rather than on the file's text.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};

use buzz_lib::agent_config_gen::{
    plan_claude_paths, plan_codex_paths, runtime_skill_dir, write_claude_project_config,
    write_codex_config, AgentRuntimeConfigSpec, McpServerSpec, PinnedSkill,
};

type Res<T> = Result<T, Box<dyn Error>>;

#[derive(Default)]
struct Args {
    command: String,
    runtime: String,
    root: Option<PathBuf>,
    codex_home: Option<PathBuf>,
    server_name: Option<String>,
    server_command: Option<String>,
    server_url: Option<String>,
    server_args: Vec<String>,
    server_env: Vec<String>,
    skills: Vec<(String, PathBuf)>,
}

fn main() -> Res<()> {
    let args = parse_args()?;
    match args.command.as_str() {
        "generate" => generate(&args),
        "verify" => verify(&args),
        other => Err(format!("unknown command {other:?}; expected generate or verify").into()),
    }
}

fn parse_args() -> Res<Args> {
    let mut raw = std::env::args().skip(1);
    let mut args = Args {
        command: raw.next().unwrap_or_default(),
        ..Args::default()
    };
    while let Some(flag) = raw.next() {
        let mut value = || -> Res<String> {
            raw.next()
                .ok_or_else(|| format!("{flag} needs a value").into())
        };
        match flag.as_str() {
            "--runtime" => args.runtime = value()?,
            "--root" => args.root = Some(PathBuf::from(value()?)),
            "--codex-home" => args.codex_home = Some(PathBuf::from(value()?)),
            "--server-name" => args.server_name = Some(value()?),
            "--server-command" => args.server_command = Some(value()?),
            "--server-url" => args.server_url = Some(value()?),
            "--server-arg" => args.server_args.push(value()?),
            "--server-env" => args.server_env.push(value()?),
            "--skill" => {
                let pair = value()?;
                let (name, file) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--skill wants NAME=FILE, got {pair:?}"))?;
                args.skills.push((name.to_string(), PathBuf::from(file)));
            }
            other => return Err(format!("unknown flag {other:?}").into()),
        }
    }
    if args.runtime != "claude" && args.runtime != "codex" {
        return Err(format!("--runtime must be claude or codex, got {:?}", args.runtime).into());
    }
    Ok(args)
}

fn root_of(args: &Args) -> Res<PathBuf> {
    args.root
        .clone()
        .ok_or_else(|| "--root is required; this tool never guesses a path".into())
}

fn codex_home_of(args: &Args, root: &Path) -> PathBuf {
    args.codex_home
        .clone()
        .unwrap_or_else(|| root.join("codex-home"))
}

fn build_spec(args: &Args) -> Res<AgentRuntimeConfigSpec> {
    let mut servers = Vec::new();
    if let Some(name) = &args.server_name {
        let server = match (&args.server_command, &args.server_url) {
            (Some(_), Some(_)) => {
                return Err("pass --server-command or --server-url, not both".into())
            }
            (Some(command), None) => {
                McpServerSpec::stdio(name, command, &args.server_args, &args.server_env)?
            }
            (None, Some(url)) => McpServerSpec::http(name, url)?,
            (None, None) => {
                return Err("--server-name needs --server-command or --server-url".into())
            }
        };
        servers.push(server);
    }
    let mut skills = Vec::new();
    for (name, file) in &args.skills {
        let body = std::fs::read_to_string(file)
            .map_err(|e| format!("read skill body {}: {e}", file.display()))?;
        skills.push(PinnedSkill::new(name, &body)?);
    }
    Ok(AgentRuntimeConfigSpec::new(servers, skills)?)
}

fn generate(args: &Args) -> Res<()> {
    let root = root_of(args)?;
    let spec = build_spec(args)?;
    let codex_home = codex_home_of(args, &root);
    let planned = if args.runtime == "claude" {
        plan_claude_paths(&root, &spec)?
    } else {
        plan_codex_paths(&root, &codex_home, &spec)?
    };
    let written = if args.runtime == "claude" {
        write_claude_project_config(&root, &spec)?
    } else {
        write_codex_config(&root, &codex_home, &spec)?
    };
    if planned != written {
        return Err("the planned write set and the written set disagree".into());
    }
    for path in written {
        println!("wrote\t{}", path.display());
    }
    Ok(())
}

fn verify(args: &Args) -> Res<()> {
    let root = root_of(args)?;
    let servers = if args.runtime == "claude" {
        read_claude(&root.join(".mcp.json"))?
    } else {
        read_codex(&codex_home_of(args, &root).join("config.toml"))?
    };
    for (name, record) in servers {
        println!("server\t{name}\t{record}");
    }
    if args.runtime == "claude" {
        for name in read_claude_approvals(&root.join(".claude/settings.local.json"))? {
            println!("approved\t{name}");
        }
    }
    let skill_root = root.join(runtime_skill_dir(&args.runtime)?);
    if skill_root.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&skill_root)
            .map_err(|e| format!("read {}: {e}", skill_root.display()))?
            .filter_map(Result::ok)
            .filter(|e| e.path().join("SKILL.md").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for name in names {
            println!("skill\t{name}");
        }
    }
    Ok(())
}

/// The server names Claude's project settings approve, in file order.
///
/// Claude ignores a project-scoped MCP server the project has not approved, so
/// an unapproved `.mcp.json` is a silent no-tool run. Reporting the list makes
/// the caller able to assert the approval, not just the declaration.
fn read_claude_approvals(path: &Path) -> Res<Vec<String>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", path.display()).into()),
    };
    let doc: serde_json::Value = serde_json::from_str(&raw)?;
    Ok(doc
        .get("enabledMcpjsonServers")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

/// `kind<TAB>target<TAB>args<TAB>env-names`, with `,` between list entries.
fn record(kind: &str, target: &str, args: &[String], env: &[String]) -> String {
    format!("{kind}\t{target}\t{}\t{}", args.join(","), env.join(","))
}

fn read_claude(path: &Path) -> Res<BTreeMap<String, String>> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc: serde_json::Value = serde_json::from_str(&raw)?;
    // A spec that pins only skills declares no servers. Absent is zero
    // servers, not a malformed document.
    let empty = serde_json::Map::new();
    let servers = doc
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let mut out = BTreeMap::new();
    for (name, entry) in servers {
        let strings = |key: &str| -> Vec<String> {
            entry
                .get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let env: Vec<String> = entry
            .get("env")
            .and_then(|v| v.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        let (kind, target) = match entry.get("url").and_then(|v| v.as_str()) {
            Some(url) => ("http", url.to_string()),
            None => (
                "stdio",
                entry
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            ),
        };
        out.insert(name.clone(), record(kind, &target, &strings("args"), &env));
    }
    Ok(out)
}

fn read_codex(path: &Path) -> Res<BTreeMap<String, String>> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let doc: toml::Table = raw.parse()?;
    // A spec that pins only skills declares no servers. Absent is zero
    // servers, not a malformed document.
    let empty = toml::map::Map::new();
    let servers = doc
        .get("mcp_servers")
        .and_then(toml::Value::as_table)
        .unwrap_or(&empty);
    let mut out = BTreeMap::new();
    for (name, entry) in servers {
        let args: Vec<String> = entry
            .get("args")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        // Codex forwards by name through `env_vars`, not through the `env`
        // table, whose values it reads literally. Reading the names back from
        // `env_vars` is what makes the caller's env-name assertion bind the
        // shape Codex actually honours.
        let env: Vec<String> = entry
            .get("env_vars")
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let (kind, target) = match entry.get("url").and_then(toml::Value::as_str) {
            Some(url) => ("http", url.to_string()),
            None => (
                "stdio",
                entry
                    .get("command")
                    .and_then(toml::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
        };
        out.insert(name.clone(), record(kind, &target, &args, &env));
    }
    Ok(out)
}
