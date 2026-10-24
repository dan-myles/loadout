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
- Planning only until 2026-10-24, when Dan authorized building v1 and delegated every remaining product choice to the build. The open tickets were resolved under that delegation; [architecture.md](../../architecture.md) is the v1 contract.
- Every session consults the grilling and domain-modeling skills; research tickets use the research skill. Offer an ADR in `docs/adr/` only for hard-to-reverse, surprising trade-offs.
- Tracker: this folder. One file per ticket in [tickets/](./tickets/), with `Type:`, `Status:` (`open` / `claimed` / `resolved`), and `Blocked by:` lines. The frontier is every `open` ticket whose blockers are all `resolved`; lowest number first. Claim by setting `Status: claimed` before any work. Resolve by appending `## Answer`, setting `Status: resolved`, and adding a gist and link under Decisions so far.
- Refer to tickets by their title, linked to the file.

## Decisions so far

- [Override model](./tickets/03-override-model.md): layers run global → OS (macos/linux/windows) → tag → machine, and the narrower layer wins. The loadout lists each machine's tags, and the later tag wins a clash. Overrides add, remove, replace, or change fields. Items may carry conditions on machine facts. Only marked files are templates, using layer variables and machine facts, and a missing variable is an error.
- [Agent config surfaces per OS](./tickets/01-agent-config-surfaces.md): every candidate agent reads skills, global instructions, and MCP definitions from its own user paths, and most read `~/.agents/skills`. Claude Code and Codex mix agent state into their MCP definition files. Auth stays in keychains or separate files, and Windows symlinks need admin or Developer Mode.
- [Program installation per OS](./tickets/02-program-installation.md): per-user installs are possible only through Homebrew formulae, Scoop, Flatpak `--user`, and language-tool globals. Most Linux managers and winget browser installs need admin, output formats and already-installed behaviour vary by manager, package names differ per OS, and no Rust library wraps them together.
- [Apply lifecycle](./tickets/04-apply-lifecycle.md): one way, copy never link, ownership by hash, drift reported and never overwritten without `--force`, journal, backups, and rollback.
- [Update trigger and fetch](./tickets/05-update-trigger-and-fetch.md): automatic per-user background updates by default, the CLI's own clone, system git with existing credentials.
- [Secrets in a loadout](./tickets/06-secrets.md): env-variable references only, written in each agent's native syntax; no secret values anywhere.
- [Bootstrap and distribution](./tickets/07-bootstrap-and-distribution.md): public GitHub release binaries plus install scripts, with `git` as the only prerequisite.
- [Supported agents in v1](./tickets/08-supported-agents.md): T3's eight named drivers as built-in adapters, plus declared custom agents for ACP registry agents.
- [Program model](./tickets/09-program-model.md): one table maps managers to package ids; no pinning, no uninstall, and elevation only on request.
- [Loadout layout and manifest](./tickets/10-layout-and-manifest.md): a strict TOML layer per file, keyed items, and `remove`/`replace`/list-edit overrides.

## Specified by the build (2026-10-24)

- Command surface and exit codes: [architecture §1 and §10](../../architecture.md#10-commands).
- Previewing a run: `loadout plan`, plan as data ([§5](../../architecture.md#5-apply-lifecycle-frozen-u3--u4)).
- Local state per OS: [§4](../../architecture.md#4-local-state-frozen-u3).
- Testing: unit and CLI tests on native CI runners for all three OSes, plus end-to-end runs in disposable VMs.
- CLI self-update: [§8](../../architecture.md#8-updates-frozen-u8--u9).
- Still open, out of v1: composing loadouts (including another loadout, or sharing parts between people).

## Out of scope

- System-level settings: macOS defaults, the Windows registry, services, and other admin changes beyond installing programs. Ruled out with Dan while charting.
- Project-specific MCP configuration. It stays with its project.
- Migrating Dan's machines off Skills Manager, or any rollout. That is a separate effort needing explicit approval.
