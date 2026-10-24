# Loadout layout and manifest

Type: grilling
Status: resolved
Blocked by: 03, 08, 09

## Question

What does a loadout look like on disk?

Settle: the directory structure; the manifest file format and its schema for every item kind (user file, skill, global agent instructions, MCP server definition, program); where layers, tags, and machine entries live; the concrete syntax for overrides, list edits, conditions, and templates (including how a file is marked as a template); and how the CLI validates a loadout before touching the machine. Builds on [Override model](./03-override-model.md), [Supported agents in v1](./08-supported-agents.md), and [Program model](./09-program-model.md).

## Answer

Resolved 2026-10-24 and implemented in the foundation unit (`loadout validate`). Full contract: [architecture §3](../../../architecture.md#3-the-loadout-on-disk-foundation).

- `loadout.toml` holds the global layer plus `[loadout] version = 1` and `[machines.<name>] tags = [...]`. `os/<os>.toml`, `tags/<tag>.toml`, and `machines/<name>.toml` are the narrower layers.
- Items are keyed tables (`[files.<id>]`, `[skills.<id>]`, `[instructions.<id>]`, `[mcp.<id>]`, `[programs.<id>]`). `remove = true`, `replace = true`, and `{ add, remove }` list edits carry the settled override operations.
- `when = { os, distro, arch, machine }` conditions. A `.tmpl` source suffix marks a template.
- Validation is strict: unknown keys, bad ids, missing or escaping sources, and bad targets are errors, reported with file and key, before the CLI touches the machine.
