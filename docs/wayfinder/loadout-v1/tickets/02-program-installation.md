# Program installation per OS

Type: research
Status: claimed
Research branch: research/program-installation
Blocked by: none

## Question

How can a Rust CLI install, detect, and remove programs on macOS, Linux, and Windows without system-level configuration?

Cover: OS package managers (Homebrew formulae and casks; winget, scoop, Chocolatey; apt, dnf, pacman, zypper, Flatpak, Snap), including which need admin or sudo and which install per-user; language-tool globals (npm/pnpm/bun global, `cargo install` and cargo-binstall, `uv tool`, pipx, `go install`); desktop apps such as browsers on each OS; reliable "is it already installed?" checks for each; and machine-readable output or APIs each manager offers.

Also survey prior art for declaring one program across OSes: Brewfile, winget configuration / DSC, Nix home-manager, mise, pkgx, Repology's cross-distro package naming, and any Rust crates that wrap these managers.

Facts only, each cited to primary docs or source. The decision of how loadouts declare programs belongs to [Program model](./09-program-model.md).
