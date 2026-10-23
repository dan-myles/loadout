# Map: Loadout v1 spec

Type: map
Started: 2026-10-23

## Destination

An implementation-ready v1 spec for the loadout CLI, written in Rust. Given only a loadout (the user's Git repo), the CLI brings a macOS, Linux, or Windows machine to the declared state: user files, skills, global agent instructions, user-level MCP server definitions, and programs including desktop apps, with a global baseline and per-machine overrides. Done when a builder can start with no open product calls.

## Notes

- Domain language lives in [GLOSSARY.md](../../../GLOSSARY.md). Use its terms; update it the moment a term settles.
- Settled while charting (2026-10-23, with Dan):
  - The loadout CLI is written in Rust.
  - A loadout is a separate, user-owned Git repo, and the only input the CLI takes. The CLI is generic: anyone can point it at their own loadout.
  - macOS, Linux, and Windows are all required; Windows is first-class.
  - Very customizable: a global baseline plus per-machine overrides, for programs, skills, and everything else a loadout declares.
  - Programs include OS packages, language-tool globals, and desktop apps such as browsers.
  - User files in general (shell, git, editor config) are in scope, not only agent config.
  - The spec is meant to replace Skills Manager for skills later.
  - Authorization and OAuth state stay local by default. Syncing an MCP server definition is not authorizing it.
- Planning only. Tickets resolve decisions, never build work.
- Every session consults the grilling and domain-modeling skills; research tickets use the research skill. Offer an ADR in `docs/adr/` only for hard-to-reverse, surprising trade-offs.
- Tracker: this folder. One file per ticket in [tickets/](./tickets/), with `Type:`, `Status:` (`open` / `claimed` / `resolved`), and `Blocked by:` lines. The frontier is every `open` ticket whose blockers are all `resolved`; lowest number first. Claim by setting `Status: claimed` before any work. Resolve by appending `## Answer`, setting `Status: resolved`, and adding a gist and link under Decisions so far.
- Refer to tickets by their title, linked to the file.

## Decisions so far

- [Agent config surfaces per OS](./tickets/01-agent-config-surfaces.md): every candidate agent reads skills, global instructions, and MCP definitions from its own user paths, and most read `~/.agents/skills`. Claude Code and Codex mix agent state into their MCP definition files. Auth stays in keychains or separate files, and Windows symlinks need admin or Developer Mode.
- [Program installation per OS](./tickets/02-program-installation.md): per-user installs are possible only through Homebrew formulae, Scoop, Flatpak `--user`, and language-tool globals. Most Linux managers and winget browser installs need admin, output formats and already-installed behaviour vary by manager, package names differ per OS, and no Rust library wraps them together.

## Not yet specified

- The CLI's command surface and output: commands, flags, what a run prints.
- Previewing a run before it changes anything (dry run / plan).
- Where the CLI keeps local state on each OS, and what that state holds.
- How v1 is tested across three operating systems.
- How the CLI updates itself.
- Composing loadouts: including another loadout, or sharing parts between people.

## Out of scope

- System-level settings: macOS defaults, the Windows registry, services, and other admin changes beyond installing programs. Ruled out with Dan while charting.
- Project-specific MCP configuration. It stays with its project.
- Migrating Dan's machines off Skills Manager, or any rollout. That is a separate effort needing explicit approval.
- Building v1. This map ends at the spec.
