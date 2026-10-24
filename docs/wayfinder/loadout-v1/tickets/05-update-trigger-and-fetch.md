# Update trigger and fetch

Type: grilling
Status: resolved
Blocked by: none

## Question

When and how does a machine pick up changes to its loadout?

Settle: manual runs only, or also scheduled or watched updates; where the CLI keeps its copy of the loadout on each OS; how it reaches a private loadout (existing Git credentials, SSH keys, tokens); and how it behaves when the remote has moved on or the local copy has uncommitted edits.

## Answer

Resolved 2026-10-24. Dan requires automatic updates. Full contract: [architecture §8](../../../architecture.md#8-updates-frozen-u8--u9).

- Updates are automatic by default: a per-user launchd agent, systemd user timer, or Scheduled Task runs `loadout update --background` every `[update] interval_minutes` (default 60). Manual `loadout update` also works.
- The CLI keeps its own clone in the state dir (`~/.local/state/loadout/loadout` on Linux, `~/Library/Application Support/loadout/loadout` on macOS, `%LOCALAPPDATA%\loadout\loadout` on Windows).
- A private loadout is reached with the system `git` and the user's existing credentials or SSH keys. Loadout stores no token.
- If the remote doesn't fast-forward or the clone has local edits, the run stops and reports; nothing is reset.
