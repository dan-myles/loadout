# Apply lifecycle

Type: grilling
Status: open
Blocked by: none

## Question

What happens to the machine when the CLI applies a loadout, and over time after that?

Settle: one-way (loadout to machine) or also capturing local changes back, including whether the CLI ever writes to the loadout itself (for example, registering a new machine name that the loadout doesn't list yet); how files are placed (symlink into the clone, copy, or rendered) and what that means on Windows, where symlinks need admin rights or Developer Mode; what happens to local edits and drift on the next run; what happens on first run when an unmanaged file or program already sits where the loadout wants one (adopt, back up, refuse); and whether removing an item from the loadout removes it from the machine.
