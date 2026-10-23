# Agent config surfaces per OS

Type: research
Status: claimed
Research branch: research/agent-config-surfaces
Blocked by: none

## Question

For each candidate agent, on macOS, Linux, and Windows: where does it read user-level skills, global agent instructions, and user-level MCP server definitions, in what file format, and with what precedence against project-level config?

Candidates: Claude Code, Codex, OpenCode, and Pi (all present on Dan's Mac), the shared `~/.agents/skills` convention, plus other widely used coding agents worth considering (for example Gemini CLI, GitHub Copilot CLI, Cursor, Amp).

Also find, per agent: whether it follows symlinked skill folders and files, whether it reloads config without a restart, and where it keeps auth or OAuth state for MCP servers, so a definition can be written without touching authorization. Include Windows symlink and junction constraints (Developer Mode, admin rights) that affect placing these files.

Facts only, each cited to primary docs or source. The decision of which agents to support belongs to [Supported agents in v1](./08-supported-agents.md).
