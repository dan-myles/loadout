# CI and releasing

Two workflows, one packaging script. Nothing here needs tools beyond rustup/cargo, git, and a
standard-library Python 3.8+.

## CI (`.github/workflows/ci.yml`)

Runs on pushes to `main`, every pull request, and manual dispatch.

| Job | Runner | Command |
|-----|--------|---------|
| rustfmt | ubuntu-24.04 | `cargo fmt --all --check` |
| clippy and test | ubuntu-24.04, ubuntu-24.04-arm, macos-15 (arm64), macos-15-intel, windows-2025, windows-11-arm | `cargo clippy --locked --all-targets -- -D warnings`, then `cargo test --locked` |
| release packaging tests | ubuntu-24.04, Python 3.8 and 3.12 | `python -m unittest discover -s scripts/release -v` |

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
or the job fails. If GitHub retires its Intel macOS images (`macos-15-intel`, `macos-26-intel`),
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

The script runs `cargo build --release --locked --target <target> --bin loadout`, runs
`loadout --version` when the host can execute the target, and writes the archive and checksum.
Pass `--out-dir DIR` to write somewhere else, `--expect-version X.Y.Z` to repeat the tag check, and
`--require-smoke` to fail instead of skipping the smoke test.

Prerequisites per host:

- All: rustup, git, Python 3.8+. Release jobs install Python 3.13 with `actions/setup-python`, the
  oldest line it ships as binaries for every runner above, Windows ARM64 included.
- Linux: nothing beyond rustup while the crate is pure Rust. Rust's musl targets bring their own
  C runtime. Cross-linking aarch64 from an x86_64 host also works for pure Rust, with
  `export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld`.
- Linux with a C dependency (anything built through the `cc` crate) needs a C compiler that targets
  musl, for the target architecture. Natively, that's Ubuntu's `musl-tools` (`musl-gcc`). For an
  x86_64-to-aarch64 cross build, it's an `aarch64-linux-musl` cross toolchain with
  `CC_aarch64_unknown_linux_musl` and the linker pointed at it. Ubuntu's `gcc-aarch64-linux-gnu` is
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

The same commit built with the same `rustc` gives the same binary, wherever it's checked out. The
script prints `rustc -vV` at the top of every build log. `rust-toolchain.toml` pins the toolchain,
so rebuilding a tag on the same target uses the same `rustc`. One thing is not pinned:

- **Python's zlib.** It can compress the archive differently across Python builds. The binary
  inside stays byte-identical.

## Validation status

Verified locally on 2026-10-24 with a disposable std-only `loadout` fixture crate (not the real CLI):

- `actionlint` 1.7.12 with shellcheck 0.11.0: 0 errors on both workflows and on `publish.sh`. The
  new runner labels are accepted, and actionlint rejects unknown labels.
- Release-script unit tests: 22/22 pass on Python 3.8.20, 3.9.25 and 3.10.12. They cover:
  - deterministic tar.gz and zip, fixed metadata, checksum bytes
  - the exact `--version` token and `--require-smoke`
  - symlinked README/LICENSE rejection
  - `publish.sh` against a fake `gh`: prerelease and stable create, draft reclassify, published
    refusal

  Each regression test was confirmed to fail against the code it fixes.
- `x86_64-unknown-linux-musl`: built from two checkout paths, one containing spaces, with
  `--require-smoke`. Same binary SHA256 (`a9961644…`) and same archive SHA256 (`6764a121…`).
- Path remapping, checked with a `regex-syntax` dependency: a plain `cargo build` embeds 4 `~/.cargo`
  paths. The script's build embeds none.
- A `0.1.0-beta.1` fixture packages with `--expect-version 0.1.0-beta.1`, and `publish.sh` creates it
  with `--prerelease --latest=false`. The `gh` binary was faked; no real release was created.
- Linux VM slot 3 (Ubuntu 24.04.5 x86_64, no Rust installed): `sha256sum --check --strict` passed, the
  archive unpacked, the static binary ran `--version`, and a tampered archive was rejected.
- Error paths: a tag/version mismatch, a missing `loadout` package, a missing `Cargo.toml`, a
  symlinked README, and `--require-smoke` on a target the host can't run each exit 1 with a clear
  message.

Pending until the real crate is merged and Actions runs:

- Every Rust job on the six real runners: clippy, tests, native builds, `/Brepro`, the Windows zip,
  and the `--version` smoke test against the real CLI. The 3.8 packaging-test job's first real run
  is also pending.
- The tag-push draft release path against real GitHub. It needs a tag, which is the maintainer's call.
- VM execution of the real archive. Only Linux x86_64 VMs are available here. The other five
  platforms are exercised only by the smoke step on GitHub's native runners.
