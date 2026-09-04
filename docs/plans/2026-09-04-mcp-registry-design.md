# MCP registry — design memo (T7)

Date: 2026-09-04. Answers the checklist in `docs/plans/2026-09-04-zs-implementation-plan.md` § T7
before any code lands. Every `file:line` was read on `zs/main` at `cc6b3ee43`.

**1 · Server classes and the environment each receives.**
Decision: two classes — `stdio` (command, args, `env` block) and `http` (Streamable HTTP URL, no `env`). A stdio server's child environment is platform essentials plus its own approved `env` values and nothing else; an http upstream gets no environment, and its credential travels as a request header injected at call time.
Reason: the ACP wire carries the stdio shape alone (`crates/buzz-acp/src/acp.rs:40`, `crates/buzz-agent/src/types.rs:537`), and an http upstream has no child process to hand variables to.

**2 · Runtime capability matrix.**
Decision: buzz-agent — stdio only; an http entry is refused at load with a named error, not dropped. Claude and Codex — stdio and http, http reached through the proxy of decision 4.
Reason: `McpServerStdio` is the only server variant buzz-agent deserializes (`crates/buzz-agent/src/types.rs:537`), while Claude and Codex read native config files Buzz writes (`desktop/src-tauri/src/managed_agents/config_bridge/claude.rs:14-26`, `desktop/src-tauri/src/managed_agents/config_bridge/codex.rs:126`).

**3 · Process boundary for stdio servers under Claude and Codex.**
Decision: every generated stdio entry names a bundled launcher, `buzz-mcp-launch`, as its command. The launcher clears the inherited environment, adds only platform essentials and the server's approved values, then `exec`s the server on Unix and supervises it as a child on Windows.
Reason: the adapter is spawned with no `env_clear` and inherits the harness environment including provider keys and user-defined values (`crates/buzz-acp/src/acp.rs:470`, `desktop/src-tauri/src/managed_agents/runtime.rs:693-695`), so any MCP child it spawns inherits them too. The pattern is buzz-agent's own (`crates/buzz-agent/src/mcp.rs:798,808`); Windows has no `exec`.

**4 · HTTP credentials.**
Decision: the same binary in proxy mode runs as a local stdio MCP server, resolves the keychain reference at first use, and forwards to the Streamable HTTP upstream with the header attached. No generated JSON or TOML holds a secret value.
Reason: no launcher can inject an environment into a remote server, and Claude's `.mcp.json` and Codex's `config.toml` are plaintext files on disk that the config panel also reads back.

**5 · Where secrets live.**
Decision: extract a workspace crate `crates/buzz-secret-store` carrying the read side of today's keychain blob; the desktop keeps the write side. Registry `env` values and http credentials are stored as `keychain:<name>` references, never literals.
Reason: `secret_store` is a private Tauri module (`desktop/src-tauri/src/lib.rs:43`) that a workspace binary cannot call, and the blob is one keychain entry costing one OS prompt per process (`desktop/src-tauri/src/secret_store.rs:3-6`).

**6 · Launcher crate path and packaging.**
Decision: `crates/buzz-mcp-launch`, added to `Cargo.toml:2-34`, the stub list (`justfile:167-181`), both release build lists (`justfile:275-289`, `justfile:306-309`), `SIDECARS` in `scripts/bundle-sidecars.sh:4`, and `externalBin` in `desktop/src-tauri/tauri.conf.json:55-62` — the bundler already appends the Windows `.exe` suffix. Generated config names the resolved absolute path of the bundled binary, never a bare name.
Reason: those five lists must agree or the Tauri build fails at compile time on `externalBin` validation, and a bare name would resolve through a `PATH` the launcher's own clear removes.

**7 · Name collisions with built-ins.**
Decision: the loader rejects at load any server name equal to a built-in server's name or carrying the reserved `buzz-` prefix, naming the conflict. It never renames silently.
Reason: buzz-acp resolves a collision by appending a hash suffix (`crates/buzz-acp/src/lib.rs:6162-6167`), which changes the qualified tool name the model calls — a correct last resort for a parsed env var, wrong for a registry the operator can edit.

**8 · Per-agent toggle storage.**
Decision: the enabled-server list is a new field on the agent record in `managed-agents.json`, and it is deliberately not added to the kind:30177 projection.
Reason: that projection is an opt-in, field-by-field allowlist (`desktop/src-tauri/src/managed_agents/agent_events.rs:70-80`, `desktop/src-tauri/src/managed_agents/reconcile.rs:59-61`), so a new field stays machine-local unless someone adds it — and this design does not.

**9 · Config roots per agent, with the T6 login caveat.**
Decision: Claude gets a project `.mcp.json` written into the agent working directory (`~/.buzz`, `desktop/src-tauri/src/managed_agents/runtime.rs:564`); `CLAUDE_CONFIG_DIR` stays unset. Codex gets a per-agent `CODEX_HOME` only after a Dock launch proves its login survives.
Reason: a custom `CLAUDE_CONFIG_DIR` creates a fresh keychain namespace and leaves Claude logged out unless `CLAUDE_SECURESTORAGE_CONFIG_DIR` is managed too (`desktop/src-tauri/src/managed_agents/config_bridge/types.rs:191-197`, `desktop/src/features/agents/ui/AgentConfigPanel.tsx:292-312`); Codex has the same shape and no evidence yet.

**10 · Per-server environment on the buzz-agent path.**
Decision: buzz-agent servers are declared from the registry with a resolved `env` block; the registry never generates a `BUZZ_ACP_EXTRA_MCP_COMMANDS` argv carrying a secret.
Reason: an entry's argv is readable by `ps` and by any crash dump, which the parser itself records (`crates/buzz-acp/src/lib.rs:5975-5977`), and the declared `env` block is filtered on the same identity rule as the ambient one (`crates/buzz-agent/src/mcp.rs:835-846`).

**11 · Capability facts.**
Decision: add `mcp_transports` and `mcp_config_root_env` to `KnownAcpRuntime`, expose them on `AcpRuntimeCatalogEntry`, project them through the core, and update the feature guide in the same PR.
Reason: the guide makes the Rust runtime catalog the single source of harness capability facts and forbids a rival TypeScript table (`desktop/src/features/agents/AGENTS.md:13,34`, `desktop/src-tauri/src/managed_agents/discovery/runtime_metadata.rs:68`).

## Open verifications

Only a run can settle these; each blocks the decision it names, not the memo.

- Codex login under a per-agent `CODEX_HOME` from a Dock launch — the T6 caveat carried over from Claude (decision 9). Until it passes, Codex keeps the operator's default `CODEX_HOME`.
- Whether Claude 2.1.x loads a project `.mcp.json` from `~/.buzz` without a first-spawn trust prompt (decision 9).
- That `exec` through the launcher leaves the server's stdio pipes intact end to end on Unix (decision 3).
- That killing the adapter on Windows also reaps the launcher's grandchild server (decision 3).
- That the extracted crate reads the same keychain blob a signed release build wrote — macOS splits SecKeychain and DPK (`desktop/src-tauri/src/secret_store.rs:8-12`, decision 5).

## Risks accepted

- The launcher is not process isolation: the server still runs under the user's UID and keeps `HOME`, so it reads whatever the user can — the limit buzz-agent already documents (`crates/buzz-agent/src/mcp.rs:812-814`).
- A third-party ACP adapter that spawns its own MCP children stays out of reach; only servers Buzz generates config for get the launcher.
- The proxy holds a resolved credential in memory for the session's life, so a dump of the proxy exposes it.
- Generated command lines remain readable by anything running as the user; the design moves secret values out of them, not the commands themselves.
- The config bridge reads back only name, kind and enabled (`desktop/src-tauri/src/managed_agents/config_bridge/types.rs:226-233`), so the panel cannot detect a wrong generated command; tests assert the written files structurally instead.
