# Starter loadout

A small, complete loadout that passes `loadout validate`. Copy this folder into its own Git
repository to start yours.

| Path | Shows |
| --- | --- |
| `loadout.toml` | the global layer: variables, two enabled agents, a templated user file, a skill, global instructions, a remote MCP server whose token comes from `GITHUB_TOKEN`, and a program |
| `tags/work.toml` | the `work` profile, for machines that choose it: a different email, the MCP server narrowed to Claude, and one extra program |
| `files/gitconfig.tmpl` | a template using variables and `machine.os` |
| `skills/code-review/SKILL.md` | a skill folder |
| `instructions/AGENTS.md` | global agent instructions, which Claude gets as `CLAUDE.md` and Codex as `AGENTS.md` |

```sh
loadout validate examples/starter
```

A machine picks the `work` profile when `loadout init` asks, or with `loadout init <URL> --profile work`.
The loadout never lists machines; `[machines]` is there for the rare machine that needs a layer of
its own (see [the loadout format](../../docs/loadout-format.md#machines)).

This README sits outside the layer folders, so `loadout validate` ignores it. See
[the loadout format](../../docs/loadout-format.md) for every table, and the
[agent guide](../../docs/agent-guide.md#4-rehearse-in-a-sandbox) to rehearse it in a sandbox.
