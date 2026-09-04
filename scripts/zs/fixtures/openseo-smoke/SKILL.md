---
name: openseo-smoke
description: "Pre-approval fixture skill for scripts/zs/openseo-smoke.sh. Names the fake MCP server the smoke test pins, so each runtime's skill catalog can be asserted without any vendor call."
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

## The only tool this fixture expects

The smoke test registers the repository's `fake-mcp` test binary
(`crates/buzz-agent/tests/bin/fake_mcp.rs`) as an extra MCP server. Its default
tool is `tool_0`. Listing that tool is the whole assertion.
