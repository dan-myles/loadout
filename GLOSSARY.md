# Loadout

Loadout brings a machine to the setup its owner declared in a Git repository, on macOS, Linux, and Windows.

## Language

**Loadout**:
A user-owned Git repository that declares what a machine should carry. The only input the loadout CLI takes.
_Avoid_: loadout repo, config repo, dotfiles repo

**loadout CLI**:
The generic tool that reads a loadout and brings a machine in line with it. Works with anyone's loadout.
_Avoid_: Loadout (for the tool), sync tool

**Machine**:
One computer a loadout is brought onto.
_Avoid_: host, device

**Override**:
A declaration that makes one machine differ from the loadout's global baseline.
_Avoid_: patch, exception

### What a loadout declares

**User file**:
A file in the user's home or user config area, such as shell, git, or editor config.
_Avoid_: dotfile, config file

**Skill**:
An agent skill folder, placed where each agent reads skills.

**Global agent instructions**:
User-level instructions an agent loads in every project, such as a user-level `CLAUDE.md` or `AGENTS.md`.
_Avoid_: system prompt, memory

**MCP server definition**:
A user-level declaration of an MCP server an agent can start or reach. Holds no authorization state.
_Avoid_: MCP config, MCP auth

**Program**:
Software installed on the machine: OS packages, language-tool globals, and desktop apps such as browsers.
_Avoid_: package, app, tool
