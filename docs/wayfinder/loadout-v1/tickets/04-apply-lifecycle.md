# Apply lifecycle

Type: grilling
Status: resolved
Blocked by: none

## Question

What happens to the machine when the CLI applies a loadout, and over time after that?

Settle: one-way (loadout to machine) or also capturing local changes back, including whether the CLI ever writes to the loadout itself (for example, registering a new machine name that the loadout doesn't list yet); how files are placed (symlink into the clone, copy, or rendered) and what that means on Windows, where symlinks need admin rights or Developer Mode; what happens to local edits and drift on the next run; what happens on first run when an unmanaged file or program already sits where the loadout wants one (adopt, back up, refuse); and whether removing an item from the loadout removes it from the machine.

## Answer

Resolved 2026-10-24 under Dan's delegation of remaining choices to the build ("Build the full Rust loadout CLI now"). Full contract: [architecture §5](../../../architecture.md#5-apply-lifecycle-frozen-u3--u4).

- One way, loadout to machine. The CLI never writes, commits, or pushes to the loadout. A new machine name is added by editing `loadout.toml`.
- Copy everywhere, never link: no Windows symlink privilege needed, and the machine doesn't depend on the clone.
- `state.json` records each written target with its SHA-256, per key for entries inside shared files.
- Drift is reported and left alone (exit 4) unless `apply --force`, which backs up first. An unmanaged file in the way is backed up and replaced on an interactive apply, never by the background service.
- Removing an item removes the owned target if it is unchanged since loadout wrote it. A drifted target is released and left in place. Programs are never uninstalled in v1.
- Each run keeps a journal and backups; `loadout rollback` restores them.
