# CI and releasing

Two workflows, one packaging script. Nothing here needs tools beyond rustup/cargo, git, and a
standard-library Python 3.8+.

## CI (`.github/workflows/ci.yml`)

Runs on pushes to `main`, every pull request, and manual dispatch.

| Job | Runner | Command |
|-----|--------|---------|
| rustfmt | ubuntu-24.04 | `cargo fmt --all --check` |
| clippy and test | ubuntu-24.04, macos-15 (arm64), windows-2025 | `cargo clippy --locked --all-targets -- -D warnings`, then `cargo test --locked` |
| release packaging tests | ubuntu-24.04 | `python3 -m unittest discover -s scripts/release -v` |

Every job installs the latest stable Rust through rustup. If the repo adds `rust-toolchain.toml`,
rustup uses that toolchain instead, with no change to the workflow. `--locked` means a stale
`Cargo.lock` fails the job instead of being rewritten silently. Only the CI jobs cache Cargo
downloads and `target/`. Release builds start clean so a poisoned cache can't reach a shipped binary.

## Release artifacts (`.github/workflows/release.yml`)

| Target | Runner | Built how | Executed in CI |
|--------|--------|-----------|----------------|
| `x86_64-unknown-linux-musl` | ubuntu-24.04 | native, static | yes, `--version` smoke |
| `aarch64-unknown-linux-musl` | ubuntu-24.04 | cross, linked by the toolchain's `rust-lld` | no |
| `x86_64-apple-darwin` | macos-15 (arm64) | cross with the Xcode SDK | no |
| `aarch64-apple-darwin` | macos-15 | native | yes |
| `x86_64-pc-windows-msvc` | windows-2025 | native, static CRT | yes |
| `aarch64-pc-windows-msvc` | windows-2025 | cross with MSVC ARM64 tools, static CRT | no |

The Linux binaries are statically linked against musl, so they run on any distribution and need
no glibc. The Windows binaries link the C runtime statically, so they don't need the VC++
redistributable. The repo is private, so GitHub's free ARM runners aren't available. That's why
three targets are cross-built and never executed in CI.

Each target produces two assets:

- `loadout-<target>.tar.gz` (`.zip` on Windows), containing `loadout-<target>/loadout[.exe]`, plus
  `README.md` and any `LICENSE*` files at the repo root.
- `loadout-<target>.tar.gz.sha256`, one line in `sha256sum` format.

The `checksums` job checks all six, then combines them into `SHA256SUMS`. Asset names carry no
version, so bootstrap and self-update can resolve
`https://github.com/dan-myles/loadout/releases/download/<tag>/loadout-<target>.tar.gz` (or
`releases/latest/download/...`) directly from the target triple.
While the repo is private, those URLs need a GitHub token to download.

Triggers:

- **Manual dispatch** and **pull requests that touch packaging**: build all six targets and upload
  them as workflow artifacts (`loadout-<target>` and `release-dist`). Nothing is published.
- **Pushing a `vX.Y.Z` tag**: builds as above, fails if the tag doesn't match the `version` in
  `Cargo.toml`, then creates a **draft** GitHub release with every asset and `SHA256SUMS`. Only this
  job gets `contents: write`. Rerunning it replaces the draft's assets. It refuses to touch a release
  that is already published. `scripts/release/publish.sh` does the GitHub side.
- **Prerelease tags** (a SemVer pre-release such as `v0.2.0-beta.1`, the tag still equal to the
  `Cargo.toml` version) become drafts marked `--prerelease --latest=false`. That keeps them out of
  `releases/latest`. Every other tag, including build metadata only (`v1.0.0+build.5`), is marked
  `--prerelease=false`. On a rerun, the existing draft is reclassified before its assets are replaced.

## Cutting a release

1. Set `version` in `Cargo.toml`, commit it with the matching `Cargo.lock`, and merge to `main`.
2. `git tag vX.Y.Z <commit> && git push origin vX.Y.Z`
3. Wait for the Release workflow to finish. Review the draft release on GitHub, then publish it.
   Self-update and `releases/latest` only see published, non-prerelease releases. Don't untick
   "pre-release" on a beta draft.

## Building a release archive locally

```sh
rustup target add x86_64-unknown-linux-musl
python3 scripts/release/package.py --target x86_64-unknown-linux-musl
# -> target/dist/loadout-x86_64-unknown-linux-musl.tar.gz and .sha256
```

The script runs `cargo build --release --locked --target <target> --bin loadout`, runs
`loadout --version` when the host can execute the target, and writes the archive and checksum.
Pass `--out-dir DIR` to write somewhere else, and `--expect-version X.Y.Z` to repeat the tag check.

Prerequisites per host:

- All: rustup, git, Python 3.8+ (`python` on Windows runners comes from `actions/setup-python`).
- Linux cross to aarch64: `export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld`. No apt
  packages needed while the crate has no C dependencies. If one is added, install `musl-tools` and
  `gcc-aarch64-linux-gnu` and point the linker at the latter.
- macOS: Xcode Command Line Tools. Both architectures build from either kind of Mac.
- Windows: Visual Studio Build Tools with the MSVC x64 (and ARM64, for that target) components.

## Reproducibility

The script pins everything it controls:

- `SOURCE_DATE_EPOCH` is the commit time, and every archive entry uses that mtime.
- Archive entries are sorted, with owner 0/0, no user or group names, fixed modes (0755 for the
  binary, 0644 for docs), and gzip with no filename or timestamp.
- `--remap-path-prefix` rewrites the checkout path to `/build` and `CARGO_HOME` to `/cargo`, so
  neither leaks into the binary.
- On MSVC targets, `/Brepro` and `/PDBALTPATH:%_PDB%` remove the link timestamp and the absolute
  PDB path.

The same commit built with the same `rustc` gives the same binary, wherever it's checked out. The
script prints `rustc -vV` at the top of every build log. Two things are not pinned:

- **The toolchain.** CI uses whatever `stable` is on the day. For bit-for-bit rebuilds of an old
  tag, pin a release in `rust-toolchain.toml` (for example `channel = "1.98.1"`) and use the
  `rustc` shown in that tag's build log.
- **Python's zlib.** It can compress the archive differently across Python builds. The binary
  inside stays byte-identical.

## Validation status

Verified locally on 2026-10-24 with a disposable std-only `loadout` fixture crate (not the real CLI):

- `actionlint` 1.7.12 with shellcheck 0.11.0: 0 errors on both workflows.
- Packaging unit tests: 5/5 pass (deterministic tar.gz and zip, fixed metadata, checksum format).
- `x86_64-unknown-linux-musl`: built from two different checkout paths, giving the same binary SHA256
  (`a996164…`) and the same archive SHA256 (`e4583e2…`).
- Path remapping, checked with a `regex-syntax` dependency: a plain `cargo build` embeds 4 `~/.cargo`
  paths. The script's build embeds none (they become `/cargo/registry/...`).
- Linux VM (Ubuntu 24.04.5 x86_64, no Rust installed): `sha256sum -c` passed, the archive unpacked, the
  static binary ran `--version`, and a tampered archive was rejected.
- `aarch64-unknown-linux-musl`: cross-built with `rust-lld` and confirmed as a static AArch64 ELF.
  Not executed.
- Error paths: a tag/version mismatch, a missing `loadout` package, and a missing `Cargo.toml` each
  exit 1 with a clear message.

Pending until the real crate is merged and Actions runs:

- Every job on real GitHub runners: macOS and Windows builds, `/Brepro` behaviour, and the Windows
  zip and smoke test.
- Clippy, tests, and the `--version` smoke test against the real CLI.
- The tag-push draft release path. It needs a tag, which is the maintainer's call.
- Executing the aarch64 Linux, aarch64 Windows, and x86_64 macOS binaries. That needs native
  hardware or VMs.
