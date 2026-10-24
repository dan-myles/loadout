# Supported agents in v1

Type: grilling
Status: resolved
Blocked by: 01

## Question

Which agents does v1 support as first-class targets, and how does each kind of agent item (skill, global agent instructions, MCP server definition) map onto each supported agent?

Settle: the v1 agent list; whether a loadout declares an item once for all agents or per agent; how per-agent differences in format and location are expressed; and how a loadout's MCP server definitions are written into agent files that also hold agent-written state (Claude Code's `~/.claude.json`, Codex's `config.toml`). Uses the facts from [Agent config surfaces per OS](./01-agent-config-surfaces.md).

## Answer

Resolved 2026-10-24. Dan: "Support every agent T3 Code supports." Evidence: the agent-inventory report (T3 Code `4fef307`). Full contract: [architecture §3 and §6](../../../architecture.md#6-agents-frozen-u5-adapters-u6-mcp).

- Built-in adapters for T3's eight named drivers: `claude`, `codex`, `cursor`, `grok`, `muse`, `pi`, `opencode`, `antigravity`.
- T3's open-ended `acpRegistry` agents are custom agents declared in the loadout: per-OS paths, MCP format, map pointer, server shape, env syntax, and evidence. Validate checks them; nothing is guessed.
- Items are declared once and reach every enabled agent by default; `agents = [...]` narrows that per item.
- MCP definitions are merged by key into agent files. Agent-written state and formatting (TOML, JSONC) are preserved, and server names loadout doesn't own are never overwritten.
- Gaps stay visible as `unsupported` actions: Cursor has no file for global instructions, and Muse has no documented instructions path and no Windows paths.
