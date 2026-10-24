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

**Machine name**:
The name a machine goes by in a loadout, chosen on its first run and kept on the machine. It defaults to the hostname and survives renaming the computer.
_Avoid_: hostname, machine ID

### Customization

**Layer**:
One level of declarations in a loadout. The layers, broadest first: global, OS, tag, machine.
_Avoid_: profile, scope, level

**Global layer**:
What every machine gets.
_Avoid_: baseline, default, common

**Tag**:
A named group of machines, such as `work` or `laptop`, with its own layer. The loadout lists each machine's tags.
_Avoid_: group, profile, role

**Override**:
A declaration in a narrower layer that adds, removes, replaces, or changes part of an item from a broader layer.
_Avoid_: patch, exception

**Machine fact**:
A built-in value describing the machine: machine name, OS, distro, CPU architecture, home path.
_Avoid_: environment, host info

**Condition**:
A rule on an item that limits it to machines whose facts match, such as `os = windows`.
_Avoid_: filter, guard, when-clause

**Template**:
A user file marked for rendering, whose variables and if/else blocks are filled in per machine. Unmarked user files are copied unchanged.
_Avoid_: dynamic file

### What a loadout declares

**Item**:
One thing a loadout declares: a user file, skill, global agent instructions, MCP server definition, or program.
_Avoid_: entry, resource, unit

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
