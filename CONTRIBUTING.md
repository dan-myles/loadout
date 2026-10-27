# Contributing to Loadout

This page is for people and agents changing Loadout's own code. To *use* Loadout, read the
[README](README.md) and the [agent guide](docs/agent-guide.md).

## Read first

| Doc | What it settles |
| --- | --- |
| [GLOSSARY.md](GLOSSARY.md) | the domain words: loadout, machine, layer, tag, item, and the words to avoid |
| [FERN.md](FERN.md) | how `src/` is laid out: commands are routes, subjects own their types and rules, imports point down |
| [docs/architecture.md](docs/architecture.md) | the v1 contract: data flow, exit codes, the manifest, state, apply, agents, programs, sync, release, and each module's signatures |
| [docs/wayfinder/loadout-v1/](docs/wayfinder/loadout-v1/map.md) | the settled product decisions behind the contract |
| [docs/releasing.md](docs/releasing.md) | CI, release artifacts, the install scripts, self-update, reproducibility |

## Build and run

You need [rustup](https://rustup.rs), `git`, and Python 3.8+ for the release-script tests.
`rust-toolchain.toml` pins Rust `1.98.1` with `rustfmt` and `clippy`, and rustup installs it on
first use.

```sh
cargo build
cargo run -- --help
cargo run -- validate tests/foundation/complete
```

## Checks

`scripts/ci/check.sh` runs the same gates CI would: rustfmt, clippy with `-D warnings`, every
test, and the release-script tests. Every gate runs even after one fails.

```sh
scripts/ci/check.sh                 # every gate
scripts/ci/check.sh fmt clippy      # only some: fmt, clippy, test, python
scripts/ci/check.sh --fresh /tmp/loadout-fresh-build   # build in a new target dir, for evidence you hand on
```

It uses `cargo nextest` when installed, and falls back to `cargo test` otherwise. The individual
commands are:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo nextest run --locked --workspace --no-fail-fast   # or: cargo test --locked --workspace --no-fail-fast
cargo test --locked --workspace --doc --no-fail-fast
python3 -m unittest discover -s scripts/release
```

`--workspace` also covers `crates/loadout-winproc`, the Windows-only helper that holds the
workspace's only `unsafe` code.

**The GitHub workflows are disabled for now**, so nothing runs on a push or a pull request. Run
`scripts/ci/check.sh` locally and report its result in the pull request. Don't re-enable a
workflow as part of an unrelated change.

## Never test against your own home

Loadout writes into home directories, agent configs, and package managers. Tests and manual runs
must stay inside a throwaway directory:

- Set `LOADOUT_HOME` and `LOADOUT_STATE_DIR` to a temporary directory. Every path Loadout itself
  reads or writes then derives from it, and package managers, the scheduler, and self-update are
  refused.
- Tests check behavior through the public API or the real binary in such a directory, never
  through a developer's real `HOME`.
- Real package installs, real schedulers, and native acceptance runs belong in a disposable VM
  guest, through `tests/vm-acceptance/`.

## Code rules

- Follow [FERN.md](FERN.md). A command file turns flags into calls on subjects and returns an
  `Exit`. Logic belongs to a subject.
- Expected outcomes are `Exit` values with the codes in
  [architecture.md](docs/architecture.md#exit-codes-foundation-srcclirs). Anything unexpected is an
  `anyhow::Error` with context.
- Comments say why, never what.
- A new dependency needs a reason in its pull request. Release builds are static, so use rustls
  only, never native OpenSSL.
- When a change alters a contract, update [docs/architecture.md](docs/architecture.md) in the
  same pull request.

## Docs

Keep each doc to its audience:

| Doc | Audience | Update it when |
| --- | --- | --- |
| [README.md](README.md) | anyone deciding whether to use Loadout | the pitch, install, or command list changes |
| [docs/agent-guide.md](docs/agent-guide.md) | agents and people using the CLI | a command, flag, exit code, message, or JSON field changes |
| [docs/loadout-format.md](docs/loadout-format.md) | loadout authors | the manifest schema, override rules, or agent and manager tables change |
| [examples/starter/](examples/starter) | loadout authors | it must keep passing `loadout validate examples/starter` |
| [docs/architecture.md](docs/architecture.md) | contributors | a contract changes |

User-facing docs describe what the CLI does today, checked against `--help` and the source. They
never present a test prerelease as stable or a planned feature as shipped.

## Commits and pull requests

Commit titles name the area first, such as `fix(sync): ...`, `test(state): ...`,
`release: ...`, or `docs: ...`. Keep each commit to one reviewable step. A pull request says why,
what changed, how it was verified (with the `check.sh` result), and anything it leaves out.
