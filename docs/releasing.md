# CI and releasing

Two workflows, one packaging script. Nothing here needs tools beyond rustup/cargo, git, and a
standard-library Python 3.8+.

## CI (`.github/workflows/ci.yml`)

Runs on pushes to `main`, every pull request, and manual dispatch.

| Job | Runner | Command |
|-----|--------|---------|
| rustfmt | ubuntu-24.04 | `cargo fmt --all --check` |
| clippy and test | ubuntu-24.04, ubuntu-24.04-arm, macos-15 (arm64), macos-15-intel, windows-2025, windows-11-arm | `cargo clippy --locked --workspace --all-targets -- -D warnings`, then `cargo test --locked --workspace` |
| release packaging tests | ubuntu-24.04, Python 3.8 and 3.12 | `python -m unittest discover -s scripts/release -v` |

`--workspace` also lints and tests `crates/loadout-winproc`, the Windows-only helper that holds
the workspace's only `unsafe` code; elsewhere it compiles to an empty crate. Without
`--workspace`, cargo selects only the root `loadout` package.

Every job runs `rustup toolchain install --no-self-update`, which installs the exact toolchain
and components pinned in `rust-toolchain.toml`. Bump that file to move CI and releases to a new
Rust. `--locked` means a stale
`Cargo.lock` fails the job instead of being rewritten silently. Only the CI jobs cache Cargo
downloads and `target/`. Release builds start clean so a poisoned cache can't reach a shipped binary.

## Release artifacts (`.github/workflows/release.yml`)

| Target | Runner |
|--------|--------|
| `x86_64-unknown-linux-musl` | ubuntu-24.04 |
| `aarch64-unknown-linux-musl` | ubuntu-24.04-arm |
| `x86_64-apple-darwin` | macos-15-intel |
| `aarch64-apple-darwin` | macos-15 |
| `x86_64-pc-windows-msvc` | windows-2025 |
| `aarch64-pc-windows-msvc` | windows-11-arm |

Every target builds natively on the matching architecture, using only
[standard GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners),
which are free for public repositories. No larger runners are used. Every build runs
`package.py --require-smoke`, so the packaged binary's `--version` must succeed on its own platform,
or the job fails. `scripts/release/smoke.py` then tests the archive as a user would receive it:
- it checks the sidecar checksum and extracts the archive
- it runs `--help`, and checks that an unknown command exits 2
- it runs `doctor --json` and `validate` on `tests/foundation/complete` (exit 0) and
  `tests/foundation/invalid` (exit 3), with `LOADOUT_HOME` and `LOADOUT_STATE_DIR` in a throwaway
  directory
- `doctor` must report no failed check when `git` is on `PATH`, and only the `git` check failing
  when it isn't

If GitHub retires its Intel macOS images (`macos-15-intel`, `macos-26-intel`),
cross-build `x86_64-apple-darwin` on an arm64 `macos-*` runner and drop `--require-smoke` for that
target.

The Linux binaries are statically linked against musl, so they run on any distribution and need
no glibc. The Windows binaries link the C runtime statically, so they don't need the VC++
redistributable.

Each target produces two assets:

- `loadout-<target>.tar.gz` (`.zip` on Windows), containing `loadout-<target>/loadout[.exe]`, plus
  `README.md` and any `LICENSE*` files at the repo root.
- `loadout-<target>.tar.gz.sha256`, one line in `sha256sum` format.

The `checksums` job checks all six, then combines them into `SHA256SUMS`. Asset names carry no
version, so bootstrap and self-update can resolve
`https://github.com/dan-myles/loadout/releases/download/<tag>/loadout-<target>.tar.gz` (or
`releases/latest/download/...`) directly from the target triple.
The repository is public, so these downloads need no GitHub token.

Triggers:

- **Manual dispatch**, and **pull requests that change release inputs**: build all six targets and
  upload them as workflow artifacts (`loadout-<target>` and `release-dist`). Nothing is published.
  Release inputs are the packaging scripts, this workflow, any `Cargo.toml`, `Cargo.lock`, the
  toolchain file, `.cargo/`, `build.rs`, `src/`, `tests/`, and the shipped `README.md`/`LICENSE*`.
  A change to the binary therefore gets a native build and smoke test on all six platforms before
  merge, not only at tag time.
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

The script runs `cargo build --release --locked --target <target> --bin loadout` (the root
package, with `loadout-winproc` built as its Windows dependency), runs
`loadout --version` when the host can execute the target, and writes the archive and checksum.
Pass `--out-dir DIR` to write somewhere else, `--expect-version X.Y.Z` to repeat the tag check, and
`--require-smoke` to fail instead of skipping the smoke test.

Prerequisites per host:

- All: rustup, git, Python 3.8+. Release jobs install Python 3.13 with `actions/setup-python`, the
  oldest line it ships as binaries for every runner above, Windows ARM64 included.
- Linux: nothing beyond rustup while the crate is pure Rust. Rust's musl targets bring their own
  C runtime, and `package.py` links them with the toolchain's `rust-lld`, never the host `ld`. That
  also makes an x86_64-to-aarch64 cross build work with no extra setup.
- Linux with a C dependency (anything built through the `cc` crate) needs a C compiler that targets
  musl, for the target architecture. Natively, that's Ubuntu's `musl-tools` (`musl-gcc`). For an
  x86_64-to-aarch64 cross build, it's an `aarch64-linux-musl` cross toolchain with
  `CC_aarch64_unknown_linux_musl`. If that dependency then needs its toolchain's own linker, revisit
  the `rust-lld` choice in `package.py`. Ubuntu's `gcc-aarch64-linux-gnu` is
  not enough: it builds against glibc. None of this is set up in CI or tested. Validate it against
  the real dependency graph when the first C dependency lands.
- macOS: Xcode Command Line Tools. To build the other architecture locally, `rustup target add` it.
  This path isn't exercised in CI, where each target builds natively.
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
- `rust-toolchain.toml` pins `rustc`, and the script prints `rustc -vV` at the top of every build
  log. Linux musl targets link with that toolchain's `rust-lld`. With the host's `cc`/`ld`, the same
  commit gave different bytes on Ubuntu 22.04 (binutils 2.38) and the 24.04 runner (2.42).

So the same commit gives the same Linux binary on any build host of the same architecture. At
d8da3c0, the x86_64 musl binary built locally on Ubuntu 22.04 matched the ubuntu-24.04 runner's
byte for byte. That doesn't hold across host architectures. An aarch64 binary cross-built on
x86_64 differs from the native ubuntu-24.04-arm build: same size, same LLD, no leaked paths, but a
different code layout. To check an aarch64 release, rebuild it on an aarch64 host.

macOS and Windows binaries link with the runner image's Xcode `ld` and MSVC `link.exe`. Rebuilds on
one runner image should match, but that isn't verified, and image updates can change the bytes.
Also not pinned:

- **Python's zlib.** It can compress the archive differently across Python builds. The binary
  inside stays byte-identical.

## Validation status

Checked on 2026-10-24 against the real crate at d8da3c0 (CI run 40060874531, Release run
40060874548):

- CI on all six native runners: rustfmt, clippy `-D warnings`, and `cargo test`. Release-script
  unit tests on Python 3.8 and 3.12.
- Release: each target built natively with rustc 1.98.1. Each passed `--require-smoke`, then 9/9
  `smoke.py` checks on its own archive. `SHA256SUMS` was assembled after strict checks on all six
  sidecars.
- Linux VM (Ubuntu 24.04.5 x86_64, no Rust installed), using the CI archive: 46/46 checks pass.
  - checksum: strict check passes, and a tampered archive is rejected
  - archive: three entries, all owned 0/0
  - binary: static-pie, no shared libraries, linked by LLD, `--version` exactly `loadout 0.1.0`
  - doctor: healthy with git; exits 4 without git, with git as the only failed check
  - validate: exit 0 on the complete fixture and exit 3 on the invalid one, with the exact counts
  - the isolated home is left untouched
- Windows VM (Windows 11 26300 x64, no Rust, no Git), using the CI x86_64 MSVC zip:
  - the sidecar matches `Get-FileHash`, and the zip holds only `loadout.exe` and `README.md`
  - `--version` prints exactly `loadout 0.1.0`
  - doctor exits 4, with `git` as the only failed check and the `winget` hint
  - validate gives 0/0/3/3, and the isolated home is left untouched
  - with official MinGit 2.56.0.3 on `PATH` (SHA-256 checked against its release notes), doctor
    exits 0 and is healthy
- Both MSVC exes import only OS DLLs: no `vcruntime`, `msvcp` or `api-ms-win-crt`, so the static CRT
  is in effect. `objdump` reports the PE timestamp as a reproducible-build hash, so `/Brepro` is in
  effect.
- `publish.sh` with a fake `gh`, run on the real 13 release files: stable and prerelease create, a
  draft is reclassified before upload, and a published release is refused with exit 1.

Not covered:

- Real tag-push and draft-release behavior on GitHub, which needs a maintainer's tag.
- macOS, aarch64 Linux and aarch64 Windows execution beyond the native runner smoke. No VMs for
  those exist here.
- Install, update, and apply behavior, which the later units own.
