# CI and releasing

Two workflows, one packaging script. Nothing here needs tools beyond rustup/cargo, git, and a
standard-library Python 3.8+.

## CI (`.github/workflows/ci.yml`)

Runs on pushes to `main`, every pull request, and manual dispatch.

| Job | Runner | Command |
|-----|--------|---------|
| rustfmt | ubuntu-24.04 | `cargo fmt --all --check` |
| clippy and test | ubuntu-24.04, ubuntu-24.04-arm, macos-15 (arm64), macos-15-intel, windows-2025, windows-11-arm | `cargo clippy --locked --workspace --all-targets -- -D warnings`, then `cargo nextest run --locked --workspace --no-fail-fast` and `cargo test --locked --workspace --doc --no-fail-fast` |
| release packaging tests | ubuntu-24.04, Python 3.8 and 3.12 | `python -m unittest discover -s scripts/release -v` |

`--workspace` also lints and tests `crates/loadout-winproc`, the Windows-only helper that holds
the workspace's only `unsafe` code; elsewhere it compiles to an empty crate. Without
`--workspace`, cargo selects only the root `loadout` package.

Every job runs `rustup toolchain install --no-self-update`, which installs the exact toolchain
and components pinned in `rust-toolchain.toml`. Bump that file to move CI and releases to a new
Rust. `--locked` means a stale
`Cargo.lock` fails the job instead of being rewritten silently.

Only the clippy and test jobs cache, through `Swatinem/rust-cache`: the Cargo registry and the
compiled dependencies, keyed by runner OS and architecture, `rustc -vV`, the `CARGO*`/`RUST*`
environment, `Cargo.lock` and the manifests. Without an exact match it restores the newest entry
for the same job and environment, and a first run on a ref, a changed key or an evicted entry
starts colder; entry sizes follow the dependency set. It prunes the workspace crates before
saving, so each run compiles the checked-out source and runs every test; a cache entry never
stands in for a test result. Debuginfo is off (`CARGO_PROFILE_DEV_DEBUG=0`), which makes the test
binaries quicker to link and the cache smaller. `cargo nextest` runs the test binaries
concurrently, which `cargo test` does one at a time; it skips doctests, so `cargo test --doc` runs
them, also after a nextest failure. Release builds start clean so a poisoned cache can't reach a
shipped binary.

`scripts/ci/check.sh` runs the same gates locally (rustfmt, clippy, tests, release-script tests)
with the same build settings, keeps going past a failed gate, and prints each gate's exit status.
It uses nextest when installed and `cargo test` otherwise. Pass `--fresh DIR` to build in a
directory the script creates, outside the checkout, when the result is evidence for a review.
The result names the commit and a fingerprint of the checkout's tracked and untracked file
contents, index and HEAD, taken before and after the gates; uncommitted changes are allowed. It
exits 1 if the fingerprint changed or cannot be read again, and 2 without running anything if
Git cannot describe the checkout. Ignored files and anything outside the checkout are not covered,
and an edit undone before the gates finish goes unseen. `scripts/ci/test_check.sh` exercises
these rules in scratch repositories with a fake `cargo`; it compiles and tests nothing.

## Release artifacts (`.github/workflows/release.yml`)

| Target | Runner |
|--------|--------|
| `x86_64-unknown-linux-musl` | ubuntu-24.04 |
| `aarch64-unknown-linux-musl` | ubuntu-24.04-arm |
| `x86_64-apple-darwin` | macos-15-intel |
| `aarch64-apple-darwin` | macos-15 |
| `x86_64-pc-windows-msvc` | windows-2025 |
| `aarch64-pc-windows-msvc` | windows-11-arm |

The Windows targets build and ship two binaries: `loadout.exe` and `loadout-background.exe`,
the windowless launcher U8's scheduled task starts (`cargo build --bin loadout --bin
loadout-background`). `package.py` requires `loadout-background --version` to print exactly
`loadout-background <version>`, and `smoke.py` checks it again from the archive. The musl jobs
install `musl-tools` and set `CC_<target>=musl-gcc`, because `ring` (the crypto behind rustls,
which self-update uses) compiles C.

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

## Installing from a release

`scripts/release/install.sh` (macOS, Linux) and `scripts/release/install.ps1` (Windows) install a
published release. Until one is published, the default download fails with an HTTP error and
nothing is installed.

```sh
curl -fsSL https://raw.githubusercontent.com/dan-myles/loadout/main/scripts/release/install.sh | sh
curl -fsSL https://raw.githubusercontent.com/dan-myles/loadout/main/scripts/release/install.sh \
  | sh -s -- https://github.com/you/your-loadout.git --name laptop
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/dan-myles/loadout/main/scripts/release/install.ps1)))
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/dan-myles/loadout/main/scripts/release/install.ps1))) `
  -LoadoutUrl https://github.com/you/your-loadout.git -InitArgs '--name','desktop'
```

Prerequisites:
- `install.sh` needs `git`, `curl`, `tar`, `cmp`, `sha256sum` or `shasum`, and `flock` or `perl` (to
  lock the install folder; Linux has `flock`, macOS has `perl`). It needs `iconv` only when the home
  path or an existing install record isn't plain ASCII.
- `install.ps1` needs `git`, plus Windows PowerShell 5.1 on .NET Framework 4.7.2 or later, or
  PowerShell 7.

Neither script installs git; each names the per-OS command instead.

### What each script does

1. **Target.** On macOS and Linux, `uname -s`/`uname -m` pick it, and a shell running under Rosetta
   gets the aarch64 build. On Windows, the registry's native `PROCESSOR_ARCHITECTURE` picks it, so
   x64 PowerShell emulated on ARM64 gets the ARM64 build. Any other OS or CPU is refused.
2. **Destination check, before downloading** (repeated under the install lock, step 7).
   - A destination that is a symlink, junction, or folder is refused, and so is a record that is one.
   - An existing binary is replaced only if the install record next to it matches (see below).
   - With no binary there, a leftover record is simply overwritten.
3. **Download, https at every hop.**
   - Sources: `releases/latest/download/` by default, `releases/download/vX.Y.Z/` with
     `--version`/`-Version`, or `--base-url`/`-BaseUrl` for a mirror.
   - The archive and its `.sha256` come over https only, with at most 5 redirects, each of them
     https. `curl` runs with `-q` (ignore `~/.curlrc`), `--proto =https --proto-redir =https
     --tlsv1.2`.
   - `install.ps1` turns off automatic redirects and checks every hop itself. It adds TLS 1.2 to the
     process's allowed protocols (older .NET Framework defaults lack it); it doesn't force it
     exclusively.
   - wget is not a fallback, because it can't refuse an http redirect.
   - The one exception is a `--base-url` on `http://127.0.0.1`, `http://localhost`, or `http://[::1]`
     (any port), for tests. It gets plain http and no redirects at all.
   - Base URLs with credentials, a query, a fragment, spaces, or quotes are refused, as are http
     hosts that merely start with a loopback name.
4. **Checksum.** The `.sha256` file must be exactly the ASCII bytes `<64 lowercase hex>  <asset>`
   followed by one LF. That excludes a byte order mark, UTF-16, CR, NUL, extra lines, and other
   names. `install.sh` compares it byte for byte with `cmp`; `install.ps1` checks the raw bytes
   before decoding them. The archive's sha256 must equal that value before anything is extracted or
   run.
5. **Archive layout.** The archive is checked before extraction. Every entry must be
   `loadout-<target>/` or a plain name directly inside it (letters, digits, `.`, `_`, `-`), and
   there must be exactly one `loadout-<target>/loadout`. A Windows zip must hold exactly one
   `loadout.exe` and exactly one `loadout-background.exe`; besides `README.md` and `LICENSE*`,
   any other entry is refused rather than ignored.
   - Only the binaries are extracted, into the script's own temporary directory.
   - Tar: every entry's type must be a regular file or a directory. `install.sh` reads the type
     from the first character of `tar -tv` output, which GNU and BSD tar print ls-style (`h` hard
     link, `l` symlink), so links and devices are refused before extraction. That check depends on
     those tar implementations' listing format and hasn't been run on either yet. As a second check,
     the extracted binary must be a regular file and not a symlink.
   - Zip: `install.ps1` refuses the binary entry if its Unix mode bits mark anything other than a
     regular file, and other entries are never extracted.
   - This is a layout check for official release archives, not a sandbox for hostile archives.
6. **Version check.** The extracted binary must print `loadout <version>` from `--version`. With a
   requested tag, `v<version>` must equal it exactly, case-sensitively. On Windows,
   `loadout-background.exe --version` must then print exactly `loadout-background <version>`.
7. **Lock and final check.** The script takes the install folder's lock, `.loadout.install.lock`,
   without waiting, and holds it until the record is in place and its own staging files are gone.
   `install.sh` takes `flock(2)` on the file through `flock` or else `perl`, on file descriptor 9;
   `install.ps1` opens it unshared. Loadout self-update takes the same lock the same way. If another
   writer holds it, nothing changes and the script says so. Under the lock the destination check
   runs again, and the binaries and record must also still be exactly what the first check saw
   (their sha256, or absent): an install that finished during this one's download is never
   overwritten. The lock file stays in place.
8. **Staging.** Each staging file is created exclusively, under a fresh unique name in the
   destination directory: `mktemp` for `.loadout.new.XXXXXX` and `.loadout.install.toml.new.XXXXXX`,
   or a GUID name opened with `CreateNew`.
   - The script deletes only names it created and hasn't yet published.
   - Other files, including other runs' staging files, are never overwritten or deleted.
   - No backup file is kept.
9. **Publishing.** The binary is published first, with a same-directory rename (`mv -f`). Then the
   record is published the same way. Every failure before the first rename leaves the previous
   binary and record untouched.
   - On Windows the launcher goes first, then `loadout.exe`. Each existing binary, which may be
     running, is first moved aside to `.<name>.old.<guid>`, and if a later move fails every
     completed move is undone in reverse, each attempted even when another can't be. Files that
     can't be moved back stay where they are and are named in the error. Then the record is
     replaced (`File.Replace`, no backup).
   - The script deletes its own aside copies only once the record is in place. If the record
     can't be written they stay, and the error names them, so the previous binaries can be
     recovered. One that is still running can't be deleted and stays; the script never deletes an
     aside it didn't make in that run, whatever its name. Self-update keeps a ledger of its own
     (below).
10. **PATH.** Each script says how to add the directory to `PATH` if it isn't on it. Neither edits
    shell profiles or `PATH`.
11. **Init.** With a loadout URL, the script runs `<installed binary> init <URL>` followed by the
    remaining arguments. The URL must not start with `-`. The script's own message shows the URL
    with any userinfo as `***`, as loadout does, and never the init arguments; `init` itself
    refuses a URL carrying a password before showing anything.
    - `install.sh` passes `"$@"`.
    - `install.ps1` encodes each `-InitArgs` element as exactly one argument, using the
      `CommandLineToArgvW` / MSVC / Rust rules: quotes, backslashes before a quote or at the end,
      empty strings, whitespace, and Unicode all round-trip.
    - It starts the binary through `ProcessStartInfo` (no shell, `UseShellExecute = $false`, same
      console) and returns its exit code.
    - NUL can't appear on a Windows command line, so it is refused before installing.
    - If the binary can't be started or waited on (for example, the command line is longer than
      Windows allows), the script reports the error and completes with exit code 1. It never leaves
      an earlier `$LASTEXITCODE` standing.

Writers that take the lock are serialized: of two installs or self-updates that both downloaded,
the second to take the lock finds the files changed and replaces nothing. A writer that doesn't
take it (a hand copy, another tool, or a copy of either script from before the lock) is not
serialized, so it can still race one publication. If that leaves a record describing another
binary, the hash fails, and self-update and both installers refuse to replace that binary until the
user removes the binary and the record and installs again. There is no automatic adopt or force
option.

When the script runs as an in-memory script block, it never calls `exit`, so the console stays
open; it sets `$LASTEXITCODE` instead. Run with `-File`, it exits with the code. It decides which
from its own `$MyInvocation`, never from `$PSCommandPath`, which a script block could inherit from
a calling script.

### Install record, schemas 1 and 2 (read by self-update)

The record sits next to the binary as `loadout.install.toml`: `~/.local/bin/loadout.install.toml`
on macOS and Linux, `%LOCALAPPDATA%\Programs\loadout\loadout.install.toml` on Windows. Producers
write exactly this, in UTF-8 with no byte order mark, `\n` line endings, and a final newline:

```toml
# loadout install record. Written by install.sh or install.ps1 and read by loadout self-update. Do not edit.
schema = 1
installer = "install.sh"
repository = "dan-myles/loadout"
source = "https://github.com/dan-myles/loadout/releases/latest/download/loadout-x86_64-unknown-linux-musl.tar.gz"
target = "x86_64-unknown-linux-musl"
version = "0.1.0"
archive_sha256 = "<the verified archive's sha256>"
binary = "/home/me/.local/bin/loadout"
binary_sha256 = "<sha256 of that binary>"
```

- **Framing.** The whole file is UTF-8 with no byte order mark, no NUL or other control bytes
  except LF, and a final LF.
- **Lines.** Each line is a `#` comment, exactly `schema = 1` or `schema = 2` (an integer, never a
  string), or `<key> = "<value>"`. The keys are exactly
  `installer` (`"install.sh"` or `"install.ps1"`), `repository`, `source`, `target`, `version`,
  `archive_sha256`, `binary` and `binary_sha256`, each once.
- **Schema 2** (Windows) adds exactly `background`, the launcher's path written like `binary`, and
  `background_sha256`. `install.ps1` and Windows self-update write schema 2. `install.sh` and Unix
  self-update write schema 1. A Windows schema-1 record, from the one-binary `install.ps1`, is still
  read: it is managed only while no `loadout-background.exe` sits beside it, and the next install
  or self-update upgrades it to schema 2 with the launcher.
- **Values.** They never contain control characters, and only `\\` and `\"` are escaped.
  Producers refuse a value they can't encode that way before changing anything: a control
  character, invalid UTF-8, or an unpaired UTF-16 surrogate. So the file is always valid TOML.
- **`binary`.**
  - `install.sh` writes the physical path: `pwd -P` of `~/.local/bin`, plus `/loadout`.
  - `install.ps1` writes `GetFullPath` of `%LOCALAPPDATA%\Programs\loadout\loadout.exe`. It doesn't
    resolve junctions, and Windows compares it case-insensitively.
  - Consumers must canonicalize both this path and their own executable before comparing.
- **Anything else is invalid.** That includes unknown keys, duplicates, a quoted or other schema,
  CR, a BOM, invalid UTF-8, a missing final LF, or other TOML syntax. Both installers read the record as data only, with no `eval` and no
  dot-sourcing.

**When a binary counts as installer-managed:**
- the record is valid
- `installer` names this platform's script
- `target` equals the target being installed or running
- `binary` names the same file
- that file's current sha256 equals `binary_sha256`
- for schema 2, `background` names the `loadout-background.exe` beside it, and that file's current
  sha256 equals `background_sha256`

If any check fails, the binary is unmanaged. Both installers refuse to replace an unmanaged binary
and give no force option. To reinstall, the user removes the binary and record themselves.

The files are not published as one atomic step, and the binaries and record are separate
renames. A failure or power loss between them leaves new binaries with the previous record, or
with no record, and the hash check then fails closed: the binaries are unmanaged until the user
reinstalls them.

### Self-update (`src/release/`)

`loadout update` (U8) calls `release::latest` after its apply when the resolved `[update] cli` is
true, then `release::install` when a strictly newer stable release exists. Self-update applies
exactly the rule above, with no token:
- **Latest tag.** It reads the tag from the `releases/latest` redirect.
- **Download.** It downloads over HTTPS only (rustls, max 5 redirects, size limits), and requires
  the exact sidecar and a matching hash.
- **Archive.** It reads the expected binaries from memory, checking each name, type and size. A
  `.tar.gz` is read entry by raw entry, so every header, metadata included, counts against the entry,
  size and total limits before any content is read. Only USTAR folder and regular-file entries are
  accepted; a GNU long-name or PAX entry is refused from its header, since `package.py` writes plain
  USTAR.
- **Lock.** It takes the install folder's lock as the scripts do, without waiting; a held lock is
  reported (`Busy`) and nothing changes.
- **Staging.** Under the lock it stages each binary with `create_new`, and requires its exact
  `--version` line.
- **Final check.** Still under the lock, the binaries and record must be exactly the ones it checked
  when it found the release. Otherwise it replaces nothing (`Changed`).
- **Publishing.** It publishes the binaries as above, then rewrites the record in the same schema
  rules, and releases the lock after its own cleanup.
- **Moved-aside executables (Windows).** Once the record is in place it deletes the executables it
  moved aside. The running `loadout.exe` can't be deleted, so it is listed in
  `.loadout.install.asides` with its sha256, and a later completed self-update deletes it, under the
  lock, only if a regular file with exactly that name and sha256 is still there. Nothing else is
  ever deleted, whatever its name: not another writer's aside, not a file merely named like one. A
  malformed or linked ledger is left alone and lists nothing.

Anything else, such as `cargo install`, a package manager, a hand copy, or a `LOADOUT_HOME`
isolated run, is never replaced. `doctor` reports which case applies (`cli install`).

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
