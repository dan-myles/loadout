# Bootstrap and distribution

Type: grilling
Status: resolved
Blocked by: none

## Question

How does a fresh macOS, Linux, or Windows machine go from nothing to an applied loadout, given only the loadout's URL?

Settle: how the CLI itself gets onto the machine (install script, GitHub release binaries, a package manager, `cargo install`); what must already be present (Git? a package manager?) and whether the CLI installs missing prerequisites; and what the one-command first run looks like for the user.

## Answer

Resolved 2026-10-24 under delegation. Full contract: [architecture §8–9](../../../architecture.md#9-bootstrap-and-release-frozen-u9--release-ci).

- GitHub release archives `loadout-<target>.tar.gz|zip` with `.sha256` files, for x86_64 and aarch64 on Linux (static musl), macOS, and Windows. The repo is public, so downloads need no token.
- `install.sh` and `install.ps1` verify the archive, install per user, and, given a loadout URL, run `loadout init <url>`. That is the one-command first run.
- The only prerequisite is `git`. The CLI never installs git or package managers; `loadout doctor` names what is missing.
- The CLI updates itself from releases when the install script installed it (`[update] cli = true`).
