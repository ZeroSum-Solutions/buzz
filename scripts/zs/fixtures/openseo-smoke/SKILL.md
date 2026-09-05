---
name: openseo-smoke
description: "Pre-approval fixture skill for scripts/zs/openseo-smoke.sh. Exists to be discovered by name, so each runtime's skill catalog can be asserted without any vendor call."
---

# OpenSEO smoke fixture

This skill exists to be *found*, not to be followed. `scripts/zs/openseo-smoke.sh`
pins it into the runtime's skill directory (`.claude/skills` for Claude,
`.codex/skills` for Codex — `desktop/src-tauri/src/managed_agents/discovery/catalog.rs`)
and then asserts that the runtime's catalog lists exactly this one skill.

It is the stand-in for OpenSEO's published `plugins/openseo/skills/` bundle
(`https://github.com/every-app/open-seo`), which is installed the same way once
DataForSEO is an approved vendor.

## Do not

- Do not call a DataForSEO endpoint. DataForSEO is not an approved vendor.
- Do not start or contact an OpenSEO server.
- Do not send anything to a Buzz relay channel.

## This file names no tool, deliberately

The smoke test registers the repository's `fake-mcp` test binary
(`crates/buzz-agent/tests/bin/fake_mcp.rs`) as an extra MCP server, under a tool
name drawn fresh from `/dev/urandom` on every run and handed to the server
through its own argv. That name appears in no file a model can read here, so
the only way it can reach a reply is a live `tools/list` from a server that
actually started — and the run's second proof is the server's own call log.
Naming a tool in this body would let a run pass with the MCP server dead.
