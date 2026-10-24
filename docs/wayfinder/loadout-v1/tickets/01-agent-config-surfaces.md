# Agent config surfaces per OS

Type: research
Status: resolved
Research branch: research/agent-config-surfaces
Blocked by: none

## Question

For each candidate agent, on macOS, Linux, and Windows: where does it read user-level skills, global agent instructions, and user-level MCP server definitions, in what file format, and with what precedence against project-level config?

Candidates: Claude Code, Codex, OpenCode, and Pi (all present on Dan's Mac), the shared `~/.agents/skills` convention, plus other widely used coding agents worth considering (for example Gemini CLI, GitHub Copilot CLI, Cursor, Amp).

Also find, per agent: whether it follows symlinked skill folders and files, whether it reloads config without a restart, and where it keeps auth or OAuth state for MCP servers, so a definition can be written without touching authorization. Include Windows symlink and junction constraints (Developer Mode, admin rights) that affect placing these files.

Facts only, each cited to primary docs or source. The decision of which agents to support belongs to [Supported agents in v1](./08-supported-agents.md).

## Answer

Resolved 2026-10-23 by research. Findings, cited to primary sources: [agent-config-surfaces.md](https://github.com/dan-myles/loadout/blob/research/agent-config-surfaces/docs/wayfinder/loadout-v1/research/agent-config-surfaces.md) on the throwaway `research/agent-config-surfaces` branch (commit bca2a5f, never merged). Covers Claude Code, Codex, OpenCode, Pi, Gemini CLI, Copilot CLI, Cursor, Amp, and the `~/.agents/skills` convention.

- `~/.agents/skills` is a widely adopted convention that the agentskills.io spec does not require. Codex, OpenCode, Pi, Gemini CLI, Copilot CLI, Cursor, and Amp read it. Claude Code's docs don't list it, and that couldn't be ruled out either way.
- Each agent reads user MCP definitions from its own file: `~/.claude.json`, `~/.codex/config.toml`, `~/.gemini/settings.json`, `~/.config/opencode/opencode.json`, `~/.config/amp/settings.json`, `~/.pi/agent/mcp.json`, `~/.copilot/mcp-config.json`, and `~/.cursor/mcp.json`. Claude Code and Codex keep agent-written state in the same file as the definitions.
- Global instructions are one file per agent (`CLAUDE.md`, `AGENTS.md`, `GEMINI.md`, `copilot-instructions.md`). Cursor has no file-based user-level location.
- The docs or source confirm symlinked skill folders for Claude Code, Codex, OpenCode, Pi, and Copilot CLI. The docs for Gemini CLI, Cursor, and Amp don't say.
- Auth state lives apart from definitions: the OS keychain or a separate auth file per agent. The docs give no location for some agents.
- Reload ranges from live to a command to a restart, depending on the agent.
- Windows: creating a symlink needs admin rights or Developer Mode, and enabling Developer Mode needs admin. Junctions work for directories on the local volume only, and no agent documents them.
- 16 facts are still open or conflicting; they are listed at the end of the findings.
