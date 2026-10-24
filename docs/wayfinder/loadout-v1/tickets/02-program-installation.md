# Program installation per OS

Type: research
Status: resolved
Research branch: research/program-installation
Blocked by: none

## Question

How can a Rust CLI install, detect, and remove programs on macOS, Linux, and Windows without system-level configuration?

Cover: OS package managers (Homebrew formulae and casks; winget, scoop, Chocolatey; apt, dnf, pacman, zypper, Flatpak, Snap), including which need admin or sudo and which install per-user; language-tool globals (npm/pnpm/bun global, `cargo install` and cargo-binstall, `uv tool`, pipx, `go install`); desktop apps such as browsers on each OS; reliable "is it already installed?" checks for each; and machine-readable output or APIs each manager offers.

Also survey prior art for declaring one program across OSes: Brewfile, winget configuration / DSC, Nix home-manager, mise, pkgx, Repology's cross-distro package naming, and any Rust crates that wrap these managers.

Facts only, each cited to primary docs or source. The decision of how loadouts declare programs belongs to [Program model](./09-program-model.md).

## Answer

Resolved 2026-10-23 by research. Findings, cited to primary sources: [program-installation.md](https://github.com/dan-myles/loadout/blob/research/program-installation/docs/wayfinder/loadout-v1/research/program-installation.md) on the throwaway `research/program-installation` branch (commit 27e2e0b, never merged).

- Per-user installs without elevation: Homebrew formulae, Scoop (non-global), Flatpak `--user`, and language-tool globals (`~/.cargo/bin`, uv/pipx `~/.local/bin`, `GOBIN`, `~/.bun`). apt, dnf, zypper, pacman, snap, Chocolatey, and Scoop `--global` need root or admin.
- Browsers: on macOS the Chrome, Firefox, Arc, and Zen casks are plain app copies. On Windows the winget manifests for Chrome, Firefox, and Zen are machine-scoped (Chrome self-elevates), and Arc is MSIX.
- Machine-readable output is uneven. JSON from brew, scoop export, pipx, pnpm, and winget export; XML from zypper; none from `winget list`, apt, dnf, pacman, or `uv tool list`. Several managers document exit codes, including winget's already-installed `0x8A150061`.
- Already-installed behaviour differs: Scoop, Flatpak, and pacman `--needed` skip, while apt, Homebrew, and zypper upgrade an outdated package.
- Package names differ per repo (Firefox is `firefox-esr` on Debian 12 and `MozillaFirefox` on openSUSE). Repology maps names but is rate-limited.
- Prior art: Brewfile, mise `os = [...]`, pkgx, DSC v3, and home-manager (Nix-only). No maintained Rust library wraps the major managers together.
- 20 facts the docs left unconfirmed are in section 6 of the findings.
