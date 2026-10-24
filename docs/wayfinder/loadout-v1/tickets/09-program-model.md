# Program model

Type: grilling
Status: resolved
Blocked by: 02, 03

## Question

How does a loadout declare a program so it installs correctly on every OS?

Settle: how one logical program maps to per-OS package ids and managers; which managers v1 supports; whether direct downloads or install scripts are allowed for programs no manager carries (desktop apps included); version pinning versus latest; whether removing a program from the loadout uninstalls it; and how a run handles installs that need admin or sudo (most Linux managers, machine-scoped winget installers such as browsers). Uses the facts from [Program installation per OS](./02-program-installation.md) and the layers from [Override model](./03-override-model.md).

## Answer

Resolved 2026-10-24 under delegation. Full contract: [architecture §7](../../../architecture.md#7-programs-frozen-u7).

- One `[programs.<id>]` table maps managers to package ids (`brew`, `cask`, `apt`, `dnf`, `pacman`, `zypper`, `flatpak`, `snap`, `winget`, `scoop`, `choco`, `npm`, `pnpm`, `bun`, `cargo`, `uv`, `pipx`, `go`). The first available manager in a fixed per-OS order wins.
- No version pinning, upgrades, direct downloads, or install scripts in v1. Removing a program from the loadout never uninstalls it.
- Installs that need admin are `blocked` unless `apply --elevated` is passed. The background service never elevates.
