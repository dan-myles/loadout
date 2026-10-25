# Loadout v1 architecture

The contract every v1 build unit codes against. Terms follow [GLOSSARY.md](../GLOSSARY.md); code
layout follows [FERN.md](../FERN.md). Settled product decisions come from the
[Wayfinder map](./wayfinder/loadout-v1/map.md) and its tickets. Every other choice here was delegated
to the build and is marked **Default**, with its reason.

Status words: **foundation** means implemented and tested in the foundation unit (branch
`feat/loadout-foundation` until it merges). **Frozen** means specified here, with the named unit
implementing it. A command shows up in `loadout --help` only once it works.

## 1. Shape of the program

Loadout is one Cargo package, `loadout`: a library (`src/lib.rs`) and a thin binary (`src/main.rs`).
It is synchronous Rust, edition 2024, with the toolchain pinned in `rust-toolchain.toml`. **Default**:
no async runtime. Every slow step is a subprocess (git, a package manager) or local file I/O, so
async would buy v1 nothing.

| Crate | Used for |
| --- | --- |
| `clap` (derive) | the command line |
| `serde`, `toml`, `serde_json` | the manifest, state, and `--json` output |
| `thiserror` | each module's error enum |
| `anyhow` | command-level errors with context |
| `gethostname` | the default machine name |
| `minijinja` | templates (U2). Pinned `=3.0.3`, default features off; only `builtins`, `debug`, and `unstable_machinery` |
| `toml_edit` | mcp: format-preserving TOML key merges (U6) |
| `jsonc-parser` (`cst`) | mcp: comment-preserving JSON and JSONC key merges (U6) |
| `process-wrap` (std, job-object, creation-flags; Windows only) | process: each query runs in a job object, killed as a whole (U7) |
| `loadout-winproc` (local, `publish = false`; Windows only) | process: resumes a child created suspended, once loadout owns its job (U7); the background supervisor's own kill-on-close job and its process count, and the worker's cooperative-startup checks (some job, stdin is a pipe) and nonce (U8) |
| `nix` (`process`, `signal`; Unix only) | process: signals a query's or the background worker's process group and confirms it is empty (U7); the worker's handshake checks of its parent and group (U8) |
| `ureq` 3 (`rustls` only: ring, webpki roots) | release: the HTTPS-only release redirect and downloads, no token (U9) |
| `semver` | release: only a strictly newer stable release is offered (U9) |
| `flate2` (`rust_backend`), `tar` (not Windows), `zip` (`deflate-flate2`, Windows only) | release: reading the binaries out of a release archive, in memory and bounded (U9) |

`loadout-winproc` (`crates/loadout-winproc`, a workspace member) is the workspace's only `unsafe`
code, over `windows` 0.62 (already in the lock through `process-wrap`): U7's resume in two steps,
`prepare` (find, open and check the child's one thread) and `Prepared::resume_before(deadline)`;
and U8's `Job` (create with kill-on-close and no breakaway, `assign` a suspended child and
confirm membership, `terminate`, `active_processes`), `current_process_in_job`,
`stdin_is_pipe`, and `random_nonce`.
It denies `unsafe_op_in_unsafe_fn`, gives every `unsafe` block one call and a
`SAFETY` comment, and owns every handle it opens. The `loadout` crate keeps
`unsafe_code = "forbid"`. Nothing else goes in that crate: it is not a general Windows wrapper.

A unit adds a dependency only for the module that needs it, and names it in its PR. Expected
additions: `minijinja` (template), `toml_edit` plus a comment-preserving JSONC editor (mcp), `sha2`
(state), and `ureq` with rustls (release). Release builds are static (Linux musl, Windows static
CRT), so nothing may link native OpenSSL. **Use rustls only.**

### Data flow of one run

```
Loadout::load(dir)              manifest   strict parse + static checks
  -> resolve(&loadout, &facts)  resolve    layers, overrides, conditions -> Resolved
  -> template / agents / mcp / programs    -> Vec<Desired>, Vec<programs::Step>
  -> plan::build(...)           plan       desired vs disk vs state -> Plan (plain data)
  -> apply::run(&plan, ...)     apply      safe copy, journal, backups -> state
```

Nothing writes to the machine before `plan::build` returns `Ok`. If the manifest is invalid, a
resolve fails, or a template variable is missing, the run ends with zero writes.

### Exit codes (foundation, `src/cli.rs`)

| Code | Meaning |
| --- | --- |
| 0 | success |
| 1 | runtime error (I/O, git, a subprocess), printed with its context chain on stderr |
| 2 | usage error (clap) |
| 3 | the loadout is invalid (validate, resolve, template) |
| 4 | the machine is unhealthy or blocked (a failed doctor check, or drift or a conflict that stopped apply) |

Commands return `anyhow::Result<cli::Exit>`. An expected outcome is an `Exit`. Anything unexpected is
an `anyhow::Error` carrying `.context(...)`.

## 2. Code layout

This is FERN applied to a CLI; see [FERN.md](../FERN.md) for the rules and their source.
`src/commands/<command>.rs` holds clap `Args` and `run`, and nothing else. Each subject is a
top-level module whose `mod.rs` is its public API. Imports point down:
`cli -> commands -> subjects -> machine -> context -> platform`, with `diagnostic` beside them.

```
src/
  main.rs                 calls cli::main
  lib.rs                  module list                                       [foundation, contended]
  cli.rs                  Cli, Command, dispatch, Exit                      [foundation, contended]
  platform.rs             Os, Arch                                          [foundation]
  context.rs              Context: os, home, config, state; overrides; which [foundation]
  diagnostic.rs           Diagnostic, Diagnostics, Check                    [foundation]
  id.rs                   the id rule for items, tags, machines, agents     [foundation]
  reserved.rs             reserved destinations: auth, shared configs, state [foundation]
  machine.rs              Facts, machine name                               [foundation]
  manifest/               schema.rs (types), check.rs (static checks), mod.rs (load) [foundation]
  resolve/                mod.rs (API, Resolved), merge.rs (layer folding), finish.rs (agents,
                          transports, reach), paths.rs (Windows, collisions, reserved), all.rs [built, U1]
  plan/                   mod.rs (types, build, Steps), files.rs (user files and templates),
                          compare.rs (one-hash classification, removals), identity.rs (where
                          destinations lead, MCP addresses), install.rs (program actions), volume.rs
                          (read-only case proof), check.rs (Plan::check)
                                                                            [types: foundation; build: U4 source]
  commands/
    doctor.rs             loadout doctor                                    [foundation, contended]
    validate.rs           loadout validate                                  [foundation; U1/U2 extend]
    plan.rs, apply.rs     loadout plan, loadout apply                       [U4 source]
    status.rs, rollback.rs                                                  [built, U3]
    update.rs, service.rs                                                   [U8]
    init.rs                                                                 [U9 source]
  template/               mod.rs (API), strict.rs (guards), builtins.rs     [built: U2]
  state/                  hash, disk, journal, run, rollback                [built, U3]
  apply/                  mod.rs (run, run_under_lock, Options, Mode, Report)  [U4 source]
  source/                 mod.rs (the guarded checkout reader: user files, skills, instructions)
                                                                            [U5 policy, moved by U4]
  agents/                 mod.rs (materialize, checks), table.rs (adapters), collide.rs
                          (destination collisions), doctor.rs               [built, U5]
  mcp/                    server shapes, env syntax, JSON/JSONC/TOML key merge [built: U6]
  programs/               mod.rs (API), manager.rs (per-manager table), detect.rs (queries)
                                                                            [built, U7]
  process/                mod.rs (API, Containment, Cancel), query.rs (U7's bounded queries and
                          installs, moved from programs), background.rs (supervisor, handshake,
                          bounded log)                                      [core: U8]
  sync/                   git clone/fetch, background service               [U8]
  release/                mod.rs (latest, install, checks), record.rs (install record), download.rs
                          (HTTPS, sidecar), archive.rs (bounded reads), publish.rs (staging)
                                                                            [U9 source]
```

Which subject may import which:

```
platform, diagnostic, id       -> (none)
reserved                       -> context, platform
context                        -> platform, process
process                        -> (none)
machine                        -> platform, context, id
manifest                       -> platform, machine, context (path syntax), diagnostic, id, reserved
resolve                        -> manifest, machine, context, reserved, platform, diagnostic
template                       -> manifest (Loadout, Variable, Guard, source_guard), machine, context, diagnostic
mcp                            -> manifest, resolve, reserved, context
source                         -> manifest (git_metadata_guarded), reserved, plan (types), context
agents                         -> manifest, resolve, mcp, reserved, source, plan (types), context, diagnostic, platform
programs                       -> manifest, resolve, machine, context, process, diagnostic
state                          -> context, plan (types), reserved, mcp
plan                           -> manifest, machine, resolve, template, agents, programs, source, state, reserved, context, diagnostic, platform
apply                          -> plan, state, programs, context, diagnostic
sync                           -> context, manifest, diagnostic
release                        -> context, diagnostic, process (bounded --version checks)
```

`agents` and `state` import only `plan`'s data types and never call `plan::build`, which keeps
the call graph acyclic. `mcp` imports no `plan` type.

### Contended files

Several units each add a line to the files below. Add your line at the end of its list, and nothing
else, so merges stay mechanical.

| File | What a unit adds |
| --- | --- |
| `src/lib.rs` | `pub mod <subject>;` |
| `src/cli.rs` | one `Command` variant and its dispatch arm |
| `src/commands/mod.rs` | `pub mod <command>;` |
| `src/commands/doctor.rs` | one `<subject>::checks(context),` element appended to the `[foundation(context), …]` list that `run` concatenates; no `mut`, no suppression |
| `Cargo.toml`, `Cargo.lock` | the unit's dependencies. Regenerate the lock; never hand-merge it |
| `docs/architecture.md` | move its rows from **frozen** to **foundation/built**, and record any contract change |

## 3. The loadout on disk (foundation)

```
loadout.toml              global layer, plus [loadout] and [machines] (root only)
os/macos.toml             OS layers; only these three names
os/linux.toml
os/windows.toml
tags/<tag>.toml           one layer per tag
machines/<name>.toml      one layer per machine name
files/ skills/ ...        sources in any layout, referenced by relative path
```

Only `loadout.toml` is required. `os/`, `tags/`, and `machines/` may hold only `<name>.toml` files,
plus the folder metadata `.DS_Store` and `.gitkeep`, which are ignored. Other directories are
sources, and validate ignores them. An unknown key anywhere is an error.

`Loadout::load` reads only from inside the canonical loadout root:

- `loadout.toml` and every layer file and layer folder are canonicalized, and anything that resolves
  outside the root is an error. A link inside the loadout is fine.
- Every manifest must resolve to a regular file before it is opened. A FIFO, device, or folder named
  `loadout.toml` (or a layer file), or a link to one, is an error rather than a blocking read.
- A missing `os/`, `tags/`, or `machines/` folder means "no layers of that kind". Every other
  discovery failure is an error with its path, so a layer is never silently dropped: a permission
  error, a file where the folder should be, or an unreadable entry.
- `Loadout.root` holds the canonical root.
- **Two entry points** share this loader and the one Git metadata detector:
  - `Loadout::load_guarded(root, context)` is the only entry point a command on a real host uses
    (`validate`, and `plan`/`apply`/`update` once U4 and U8 land). Every manifest (`loadout.toml`
    and layer files) and every Git pointer (`.git` as a gitfile, `commondir`) passes
    `reserved::check_source_file` with the active `Context`, as reached and as its canonical
    file, **before it is opened**. A checkout file that aliases an auth store, private settings,
    or state is refused with a path-only diagnostic, never parsed. `context` decides only what may
    be read: layers, resolution, and `check_all`'s synthetic machines are unchanged.
  - `Loadout::load(root)` is context-free: the same confinement, but no host reservation policy,
    so it would open such an alias. It exists for synthetic and test use; no host command calls it.
  - `manifest::source_guard(context, checkout)` is that guard: `check_source_file` on the file as
    reached and as its canonical file, refusing with the same path-only message. Every reader of
    checkout content on a real host builds it, so there is one policy and one message.
  - `resolve::check_all` and `resolve` read no files, so a later validation pass can't reopen
    anything. The one later pass that reads checkout content, U2's template check, takes the same
    guard (`template::check_all_guarded`; see Templates below).
[`tests/foundation/complete/`](../tests/foundation/complete) is a full example that passes
validation, and [`tests/foundation/invalid/`](../tests/foundation/invalid) collects the mistakes
validate catches.

### Root-only tables

```toml
[loadout]
version = 1                        # required; v1 accepts only 1

[machines.work-laptop]             # machine name -> its tags, in order; a later tag wins
tags = ["work", "laptop"]
```

A machine name the loadout doesn't list gets the global and OS layers only. These are errors: a
`machines/<name>.toml` with no `[machines.<name>]` entry, a listed tag that isn't an id, a tag
listed twice, and a listed tag with no valid `tags/<tag>.toml` layer. The match is exact and
case-sensitive against the tag layers the confined loader actually loaded. It is never a path probe
or a second directory read, so `../x`, `Work`, or a tag file reached only through a rejected
outside `tags/` link can't satisfy it. A tag whose file failed to parse gets both errors: the
parse error and "no valid layer file". A tag file that no machine lists is a warning.

### Layer tables (allowed in every layer file)

```toml
[variables]                        # template variables; the narrower layer wins per key
email = "dan@example.com"          # string, integer, or boolean; `machine` is reserved
work = false

[update]                           # automatic updates; the narrower layer wins per field
auto = true                        # Default true: run the background update service
interval_minutes = 60              # Default 60, minimum 15
cli = true                         # Default true: also update the loadout CLI

[agents]
enabled = ["claude", "codex", "devin"]   # list; tags and machines may edit it. Default: none

[agents.custom.devin]              # an agent with no built-in adapter (ACP registry or local)
evidence = "https://docs.example/devin-config"   # required: where these paths come from
t3_acp_agent = "devin"             # optional: the T3 acpRegistry agent id
skills = { macos = "~/.devin/skills", linux = "~/.devin/skills" }   # an OS left out = unsupported there
instructions = { linux = "~/.devin/AGENTS.md" }

[agents.custom.devin.mcp]
linux = "~/.devin/mcp.json"
format = "json"                    # json | jsonc | toml
map = "/mcpServers"                # pointer to the servers table inside the file
shape = "cursor-json"              # one of the shapes in section 6
env_syntax = "${VAR}"              # ${VAR} | ${env:VAR} | {env:VAR} | codex-env-vars | none

[files.gitconfig]                  # a user file
source = "files/gitconfig.tmpl"    # relative to the loadout root; a .tmpl suffix marks a template
target = "~/.gitconfig"            # starts with ~/ or {config}/
when = { os = ["linux", "macos"] }

[skills.review]                    # a skill folder; source holds SKILL.md
source = "skills/review"
agents = ["claude", "codex"]       # Default when unset: every enabled agent

[instructions.global]              # global agent instructions
source = "instructions/AGENTS.md"

[mcp.filesystem]                   # stdio server
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "~/Developer"]
env = { LOG_LEVEL = "warn" }

[mcp.github]                       # remote server
url = "https://api.githubcopilot.com/mcp/"
headers = { Authorization = { env = "GITHUB_TOKEN", prefix = "Bearer " } }

[programs.ripgrep]                 # one logical program, one package id per manager
brew = "ripgrep"
apt = "ripgrep"
winget = "BurntSushi.ripgrep.MSVC"
```

**Default: no agent is enabled.** If no layer sets `[agents] enabled`, the list is empty, and skills,
instructions, and MCP items reach no agent. Each agent is opted in explicitly, so Loadout never
writes into the config of an agent the user didn't name. An item's `agents` list narrows the
enabled set and never widens it. Naming an agent that isn't enabled on a machine is a resolve warning
(U1), not a write.

Built-in agents (`manifest::BuiltinAgent`) map one-to-one to T3 Code's named drivers:
`claude` (`claudeAgent`), `codex`, `cursor`, `grok`, `muse`, `pi`, `opencode`, `antigravity`. T3's
`acpRegistry` driver is open-ended, so its agents are custom agents whose paths the loadout declares
and validate checks. Loadout never guesses those paths. A custom agent declared again in a narrower
layer is replaced whole. U5 may append a built-in id when it ships an adapter backed by evidence.

Managers (`manifest::Manager`): `brew`, `cask`, `apt`, `dnf`, `pacman`, `zypper`, `flatpak`,
`snap`, `winget`, `scoop`, `choco`, `npm`, `pnpm`, `bun`, `cargo`, `uv`, `pipx`, `go`.

### Override syntax (settled in the [Override model](./wayfinder/loadout-v1/tickets/03-override-model.md))

Layers apply in order: global, then the machine's OS, then each tag in its listed order, then the
machine.

| A narrower layer writes | Effect on the item with that id |
| --- | --- |
| `[files.x]` with fields, and no `x` is inherited | adds `x`; it must be complete |
| `[files.x]` with fields, and `x` is inherited | changes only the fields written |
| `[files.x]` with `replace = true` | replaces `x` whole; it must be complete |
| `[files.x]` with `remove = true` and nothing else | removes the inherited `x` |
| `args = [...]`, `agents = [...]`, `enabled = [...]` | assigns: the list replaces the inherited one |
| `args = { add = [...], remove = [...] }` | edits the inherited list: removes first, then appends what is missing |
| `env = {...}`, `headers = {...}`, `when = {...}` | assigns: the table replaces the inherited one |

**Default**: tables assign rather than merge. Every compound value then follows the single rule
"assignment replaces", and only lists get explicit edits.

**MCP transport.** A resolved server has exactly one valid transport: `command` (stdio, with
optional `args` and `env`) or `url` (remote, with optional `headers`). A field edit can only add or
change fields, and the schema has no way to clear one. So if a field edit leaves a server with both
`command` and `url`, or with stdio fields beside `url` (or `headers` beside `command`), that is a
resolve error (U1) naming the item and its layers. To switch transports, a narrower layer writes
`replace = true` with the complete new server.

What validate enforces today, before any machine is known:

- A global-layer item must be complete, and may not use `remove`, `replace`, or list edits.
- `remove = true` stands alone.
- A `replace = true` item must be complete.
- A server is stdio (`command`, `args`, `env`) or remote (`url`, `headers`), never both.
- A remote `url` is `http://` or `https://` with a real host and an optional port. It has no
  whitespace and no `user:pass@` credentials.
  - **DNS name:** labels of 1–63 letters, digits, or `-` (not at a label's edge), at most 253
    characters in all. One trailing dot is allowed (`example.com.`).
  - **IPv4:** a dotted-decimal host must parse as `std::net::Ipv4Addr`.
  - **IPv6:** must be bracketed and parse as `std::net::Ipv6Addr` (`[2001:db8::1]`,
    `[::ffff:192.0.2.1]`). Zone ids and IPvFuture are rejected. An unbracketed IPv6 address is an
    error.
  - **Port:** ASCII digits only, from 1 to 65535.
- A custom MCP `map` is an RFC 6901 pointer with non-empty parts, where `~` appears only as `~0` or
  `~1`.
- Ids are lowercase `[a-z0-9][a-z0-9_-]*`.
- Sources exist inside the loadout, with symlinks resolved.
- `.git` metadata is never loadout content. A clone's metadata can hold remote URLs with
  credentials, hooks, and machine-local state. Validate rejects a path if any of these hold:
  - **Lexical.** A source path, or the literal target of a link inside a skill folder, has a `.git`
    component (case-insensitive). A skill folder contains any `.git` entry: a nested repo folder, or
    the worktree or submodule pointer file. `.gitconfig`, `.gitignore`, and other siblings are fine.
  - **Resolved.** Its canonical path lies under the loadout's actual metadata. For a skill folder
    this is checked for **every** entry the walk visits, before descending into it, not only for
    the source root and links. A separate Git dir placed inside a skill is therefore caught. That metadata is
    found from `<root>/.git`: a folder (or a link to one) is the metadata root itself. A gitfile (or
    a link to one), as made by `--separate-git-dir`, worktrees, and submodules, counts as metadata
    itself, and so does the folder its `gitdir:` line names. If that folder holds a `commondir`
    pointer (a worktree), that pointer file counts as metadata itself (at its canonical location, so a
    `commondir` link into a skill reserves the file it lands on), and so does the folder it names. That check runs after **either**
    form is resolved, so a `.git` link straight to a worktree's admin folder also reserves its common
    folder.
  - **Unsupported layout.** Only these one-line pointer files are read, size-capped at 4 KiB; no
    config contents are read. A pointer file is opened only when its canonical location is inside
    the loadout. Its relative contents resolve the way Git resolves them: a `gitdir:` line from the
    folder holding `.git`, and `commondir` from the gitdir, never from the folder where a linked
    pointer file really lives. A `.git` or `commondir` that resolves outside the loadout (for example, a link to
    `~/.codex/auth.json`) is an error, and the outside file is never opened. A gitdir outside the
    loadout, as in an ordinary worktree or an external separate Git dir, is only statted. Its
    `commondir` is not read, because everything outside the root is already refused as a source. A pointer that is unreadable, oversized, malformed, or names a
    missing folder is an error ("unsupported Git metadata layout"). Loadout never certifies metadata
    it can't identify.
  - The outside-the-loadout guard still applies first.
- A skill folder is walked in full. A link inside it may point only to a file inside the loadout.
  A link to a folder, a broken link, a link leaving the loadout, and any special file are errors.
  Folder links are never followed, so a link loop can't make the walk loop. An unreadable entry
  is an error.
- Targets are clean `~/` or `{config}/` paths, and two file items in one layer may not share a target.
- Targets and custom-agent paths never touch a reserved destination (section 6).
- Every agent named anywhere is built in or declared.

U1 (built) adds the checks that need a machine. `resolve` runs them for one machine's facts, and
`validate` runs them through `check_all` (section 11):

- An added item must be complete in the layer that adds it: an id no broader layer declares for
  this machine, or a `replace = true`. This includes an item re-added after a removal. A field
  edit can never clear a field, so every final item is complete.
- A server left with `command` and `url`, stdio `args` or `env` beside `url`, or `headers` beside
  `command`, is an error naming the item and every layer that touched it.
- Expanded collisions, explained below. Two writes collide when they resolve to the same path or
  when one sits inside the other. Writes are the final file targets, plus what an enabled custom
  agent receives on that OS: `<skills>/<skill id>` for each skill that reaches it, its
  instructions file when an instructions item reaches it, and its MCP file when a server reaches
  it. Paths that built-in agents receive are U5's (section 6). An MCP file is a key merge, not a
  whole-file write: several custom agents may merge into one literal path when they all declare
  the same `format`, and declaring two formats for one path is an error. Their maps and keys,
  and every alias of the file, are compared by U5 at materialize. An MCP file against any other
  write still collides as before.
- At most one instructions item reaches each agent.
- `windows_problem` runs on every final Windows write, so a target with no valid Windows path is
  an error there.
- Warnings: an item's `agents` list that names an agent the machine doesn't enable; an item that
  reaches no agent because none is enabled; a `remove = true` with nothing inherited to remove.
- An enabled custom agent that no applied layer declares is an error.

**Reserved destinations, two ways.** Validate's synthetic machines carry no `Context`, so they
read nothing from the host. Every final target is a literal that some layer declared, and
`check_literal` already checked each literal under all three `{config}` meanings, so a synthetic
re-check would add nothing. The runtime check needs a real `Context` and the active checkout.
`resolve::check_targets(resolved, context, loadout)` is a public form of it: it runs
`reserved::check_path` on every expanded file target (`Write`) and every enabled custom agent
path for `context.os` (`WriteInto`, `Write`, `KeyMerge`), and it errors when a target has no
valid path on that OS. Tests call it, but no command does, and it is not on U4's host pipeline.
That pipeline checks each destination as `plan::build` produces it (section 11), so it checks
the destinations it would write rather than every declared custom-agent path. A user-file target
with no valid path on this OS is an error, and one that `reserved::check_path(Write)` refuses
becomes an unsupported action. `agents::materialize` checks each skill folder and instructions
file an agent would receive with `Write`, and each MCP file with `KeyMerge`, and a refusal is
again an unsupported action. `state::observe` then repeats the reserved check before it reads
any target (`Write` for a path, `KeyMerge` for an entry), and `plan::compare` turns that refusal
into an unsupported or blocked action. None of this replaces the per-read, per-backup, and
per-write checks in section 6.

`check_all` resolves every listed machine, every unlisted machine name that a `when.machine`
uses, and one other unlisted machine. Each runs on macOS, Linux, and Windows, on both
architectures, and on Linux both with no distro and with each distro a `when.distro` names.
Facts are synthetic. Each diagnostic is reported once, with its machines and OSes appended, for
example `(on studio: linux; tower: macos, windows)`.

Collisions compare **expanded** paths (`~/.config/x` and `{config}/x` collide on Linux) under the
target file system's real case behaviour. By default that is case-insensitive on macOS and Windows
and case-sensitive on Linux. U4 confirms it at plan time by probing the nearest existing ancestor,
so a case-insensitive volume on Linux, or a case-sensitive APFS volume, is handled. Never assume it
from the OS alone.

### Conditions

`when` keys:

- `os`: `macos`, `linux`, or `windows`.
- `distro`: the lowercase `ID` from `/etc/os-release`.
- `arch`: `x86_64` or `aarch64`.
- `machine`: a machine name.

Each key takes a string or a list, which matches any of its values. Every key present must match.
An item whose condition fails is absent on that machine, as if a layer had removed it.
`Condition::matches(&Facts)` is foundation code.

### Templates (built, U2, `src/template/mod.rs`)

A user file whose `source` ends in `.tmpl` is a template, and nothing else is rendered. **Default**:
the suffix, not a field, marks the template. The mark is then visible in the repo tree, and no layer
can switch rendering on or off by accident. A `.tmpl` source on a skill or instructions item gets a
warning, because only user files are rendered. Skills and instructions are never parsed.

The syntax is minijinja's Jinja subset: `{{ email }}`, `{% if work %}…{% elif … %}…{% else %}…{% endif %}`,
`{% if machine.os == "windows" %}`, `{% for %}`, `{% set %}`, and the builtin filters and tests.

- **Values.** The resolved `[variables]` sit at top level with their TOML types: a string, an
  integer (`{{ size + 1 }}`), or a boolean (`{% if work %}`, where `"false"` as a string is
  truthy). Facts sit under `machine`: `name`, `os` (`macos`/`linux`/`windows`), `distro`, `arch`
  (`x86_64`/`aarch64`), and `home` (the home path as text; a home that isn't UTF-8 is an error,
  never a lossy path). `distro` is none on a machine without one: it compares unequal to every
  string and is falsy, and printing it is an error, so a template writes
  `{{ machine.distro or "unknown" }}`.
- **Strict.** A missing value is an error wherever it is used, so a typo never renders, never
  picks a branch, and never hides in a container. Strict undefined alone doesn't do this:
  minijinja lets an undefined value pass through `pprint`, `is string`, a list or dict literal, a
  comparison, `in`, or an assignment. So `render` does what minijinja's own compile does
  (`parse`, `CodeGenerator`, `eval`) with one extra pass, `strict::rewrite`, between parse and
  codegen. It rebuilds the AST and wraps every lookup, filter result, and call result that computes
  a value in an internal guard filter. A guard that sees an undefined value fails with
  `Problem::Missing` (a lookup: `typo`, `machine.hostname`, `email.domain`, `items[3]`) or
  `Problem::Render` (an undefined filter or call result, such as `[] | first`). Only these stay
  unguarded:
  - the direct operand of `is defined`, `is undefined`, `default(...)`, and its alias `d(...)`,
    the explicit escapes. Their operand's receiver and arguments are still guarded, so
    `typo.attr is defined` is an error, as in minijinja's strict mode;
  - a name being assigned (`set`, `for`, `with` targets);
  - a called function's name and a called method's attribute (its receiver is guarded);
  - inside a `{% filter %}` or `{% set … | f %}` header chain, only the operand of a `default`
    or `d` link. Every other result in the chain, the final one included, is guarded, so an empty
    capture through `first | pprint` fails while `first | default("empty")` works.

  Guards run only when their expression runs, so a variable in a branch that isn't taken isn't
  needed, and a Windows-only variable may live in `os/windows.toml`. The one undefined value a
  template can still produce is the explicit empty value of an `if` expression without `else`,
  which reads no missing variable.

  A guard also checks its value **whole**, except where the value is a **receiver** (of an
  attribute, an item, or a method call, including `cycle`). Only one engine object lists keys it
  has no value for: minijinja's loop object, whose `length`, `revindex`, and `revindex0` are
  undefined when its iterator has no exact length, as in `{% for c in "ab" %}`. Used whole, it
  fails with "`loop` has no value for `length` in a loop of unknown length". That covers a copy
  (`dict(loop)`, `namespace(loop)`), a `**loop` or `*[loop]` splat into any callable, an
  assignment, a container item, a filter or test operand, and printing. Read through, it works:
  `loop.index`, `loop["first"]`, `loop.changed(c)`, `loop.cycle(…)`, `loop.length |
  default("?")`, and recursive `loop(…)`. A loop of known length is used whole normally. The loop
  is recognised only by object identity (`tests::is_sameas`) with what `loop` names in the
  innermost running loop; nothing is rendered, iterated, or called. That suffices, because every
  alias, copy, or container of a loop is made from a whole value while that loop is current.
- **Namespace cycles** (a deliberate contract revision, not pinned minijinja behaviour). A namespace
  is the only object a template can change, and only `{% set ns.attr = value %}` changes it (also
  unpacked, as in `{% set ns.a, x = … %}`, and as a block, `{% set ns.attr %}…{% endset %}`).
  minijinja accepts `{% set ns.self = ns %}`, and then rendering, comparing, or sorting `ns`
  recurses until the process aborts. Loadout refuses an assignment whose value reaches its
  receiver: ``assigning to `ns.self` would make `ns` contain itself``, at the assignment's line,
  before the store. Self-reference is no longer supported.
  - The rewrite spells such a statement out with three locals, `__loadout_value`,
    `__loadout_parts`, and `__loadout_receiver` (reserved, with the whole `__loadout_` prefix;
    templates must not use it), in minijinja's own order. The value is evaluated once and bound.
    Each target list is unpacked exactly once, when it is reached (the top list first, a nested
    list only after the targets before it are stored), through `__loadout_unpack` (minijinja's
    unpacking: an object's own items, so a scalar or a function fails as `value is not iterable`,
    exactly as many as the list, with minijinja's error kinds and messages). Its items
    are kept as a snapshot in `__loadout_parts` under the list's number, so a later target reads
    that snapshot and never re-reads a live value. For each target in order, its receiver is
    evaluated once and bound, `__loadout_acyclic` checks the target's part, and only then is it
    stored. There is no limit on the number of targets or on nesting.
  - Only a **real namespace** receiver is checked. minijinja's `Namespace` type is private, and
    its global `namespace` is the only thing that makes one, so the global is wrapped: it calls the
    pinned constructor and records the new object's identity in the render's `State` (an alias of
    the global is the same wrapper). Any other receiver (a map, a sequence, a number) goes straight
    on to minijinja's own `can only assign to namespaces, not …` error, with its line.
  - The check walks what the value holds by object identity, with an explicit stack and each
    object entered once: map keys and values, and sequence items. Nothing is rendered, hashed,
    compared structurally, or called, and a lazy sequence is **never iterated**.
  - **Lazy sequences** (a third deliberate contract revision): a namespace attribute may not hold
    a lazy (`Iterable`-kind) value anywhere in what it stores, because a lazy view keeps
    references it does not yield yet (`[ns] | zip(ns)` while `ns` is empty, `[ns, 1][1:]`) that
    could reach the receiver once stored. The store is refused at its line, before it happens:
    ``` `ns.v` cannot hold a lazy sequence; store a list instead, with `| list` ```. Locals,
    loops, filters, and a namespace built with `namespace(r=range(2))` keep minijinja's lazy
    behaviour; only a store into a namespace attribute is checked.
  - Representations, from the pinned producers. **Lazy:** `range`, `zip`, `reverse` of a sequence
    or map, slicing a list or iterable, repeating a non-tuple sequence a nonzero number of times,
    `+` of sequences, a `chain` that mixes maps and sequences or iterables, and groupby's `list`
    view. **Transparent:** maps (literals, `dict`, namespaces, keyword arguments, the loop object),
    lists, tuples (a tuple slice or repeat is a new tuple), a `chain` of only sequences, groupby's
    group tuple (whose `list` is itself lazy), and `* 0` or repeating an empty sequence (an
    empty list). The loop object is transparent only because its `changed()` memory is kept out of
    it (below) and the `adjacent_loop_items` feature is off; with either one changed, it would
    hold template values it does not expose. No enabled builtin makes a one-shot iterator.
  - **Loop memory** (correction). An earlier version of this section said the loop's
    remembered `changed()` arguments could not form a reachable cycle. That was wrong:
    minijinja's `loop.changed(…)` keeps its arguments inside the loop object
    (`Loop.last_changed_value`), which the loop never lists. So `{% do loop.changed(ns) %}{% set
    ns.saved = loop %}`, or the same two steps in the other order, made a reference cycle that
    no check could see, and that is never freed. Now every for-loop body first passes its
    `loop` to `__loadout_loop`, which records the loop object in the render's `State` (the raw
    name, which minijinja reserves, so an unknown-length loop is not refused there), and every
    `X.changed(…)` call goes through `__loadout_changed`. For a loop object, recognized by its
    exact unsorted key list (a map or namespace lists its keys sorted), it answers exactly as
    minijinja does (true, and remembered, when the arguments differ from the last call's,
    keyword arguments included), with the memory held beside the record in the render's
    `State`: no template value can reach it, and it is dropped with the render. Any other
    receiver gets minijinja's own call, member fallback, or error. The loop object's own memory
    is never written. `cycle` routing passes keyword arguments through in the same way, and the
    keyword-arguments value it can return passes the guards, namespace checks, and unpacking as
    it passes minijinja. No new restriction comes with this.
  - Opaque container: minijinja's `chain` of maps is a live view that hides which maps it reads.
    `chain` is wrapped to merge maps into an ordinary map with the same precedence (the last map
    with a key wins), so nothing can hide a namespace. This is a second deliberate contract
    revision: the merged map is a snapshot and no longer follows later changes to a chained
    namespace. A `chain` of sequences or iterables is unchanged.
  - Acyclic aliases, shared values (a DAG), repeated assignments, and nested namespaces assign as
    before.

- **Builtins** (`src/template/builtins.rs`). Some builtins compute or consume values inside Rust,
  where no guard runs. Every filter, test, and global minijinja registers under the enabled
  features (47 filters (11 wrapped, `chain` included), 42 tests, and `range`, `dict`, `namespace`; `debug` is removed) was
  inventoried from `defaults.rs`, including multi-line registrations. With the replacements below,
  they keep one invariant: **a missing value is never stored in a container, and never consumed
  inside a builtin unless an explicit escape asked for it.** Builtins that only move, compare, or
  format elements (`select`, `reject`, `sum`, `join`, `chain`, `zip`, `list`, `batch`, `slice`,
  `reverse`, `pprint`, and the rest) therefore never see one and are not wrapped. The replaced
  builtins:
  - `map`. With `attribute=`, every item needs the attribute unless `default=` is given. With a
    filter name, minijinja calls that filter per item inside Rust (`map("attr", "x")`,
    `map("first")`), so every result is checked before the list exists. Nested `map("map", …)`
    reaches the same wrapper.
  - `selectattr` and `rejectattr` need the attribute on every item, except when the test is
    `defined` or `undefined`.
  - `sort` (including `"a,b"`), `groupby`, `unique`, `min`, and `max` with an attribute need it on
    every item; `groupby` accepts its `default=`.
  - For an attribute path, every part before the last must exist even under an escape, as for
    `a.b is defined`.
  - `items` and `dictsort` reject a pair whose value is undefined, as a second line behind the
    whole-loop guard.
  - `dict`, `namespace`, and `**` splats copy a map's pairs. Every map they can receive holds only
    guarded values, except a whole loop object of unknown length, which the guard refuses first.
  - `chain` of maps returns a merged ordinary map (see Namespace cycles).
  - `cycle` with no values: the running loop gets a typed error with its line; a receiver with a
    `cycle` member keeps minijinja's call of it; anything else gets minijinja's own unknown-method
    error, made without calling the receiver.
  - Messages name a non-string value by its kind, never by rendering it.
  - A wrapper collects its input once, so a one-shot iterator is never consumed twice.

  Correction: the U2 report for `fbd7579` said `selectattr` and `rejectattr` were not registered.
  minijinja 3.0.3 registers both under `builtins`, and both are wrapped here.
- **Panics.**
  - minijinja's `divisibleby` computes `a % b` unchecked on its integer path (neither operand a
    float), which panics for a zero divisor and for `i128::MIN % -1`. The replacement uses one rule
    for every numeric zero: when both operands are numbers or booleans, a divisor of `0`, `0.0`,
    `-0.0`, or `false` is `Render("divisibleby(0) divides by zero")` with its line. `i128::MIN %
    -1` is true. Every other input delegates to minijinja's test, so its coercion and results stand:
    `true is divisibleby(0.5)` and `9223372036854775808.0 is divisibleby(2)` are true, and
    `"x" is divisibleby(0)` is false.
  - The loop object's `cycle()` divides by its argument count. A method call named `cycle` goes
    through a function that never calls a receiver's `cycle` method without values: the running
    loop gets a typed error with its line (`cycle()`, `cycle(*[])`), a `cycle` member is still
    called, and anything else, an aliased outer loop included, gets minijinja's own unknown-method
    error. With values, every receiver is called unchanged.
  - Any other panic inside the engine is caught at `render` and becomes
    `Problem::Render("the template engine failed: …")` for the file, with no line, because a panic
    carries none. A panic hook installed once suppresses the report only while the current thread
    renders a template, and keeps the previous hook for everything else.
  - A builtin that asks for a vast but representable allocation (`indent(10**12)`) can still
    exhaust memory, and no in-process handler catches an allocation failure.
- **Pinned machinery.** `unstable_machinery` is minijinja's public but unstable API: `parse`, the
  AST, `CodeGenerator`, and `eval`. Loadout depends on it at exactly `=3.0.3`. An upgrade must
  re-review `strict.rs` against the new AST and codegen, not only recompile it. The rewrite's
  `match` is exhaustive, so a new node kind fails to compile, but a changed meaning would not. The
  features stay off: `multi_template`, `macros`, `loop_controls`, `json`, `serde`, and loaders.
- **Closed.** There is no loader, so no file is reachable from a template. `include`, `import`,
  `from`, `extends`, `block`, and `macro` aren't even parsed (minijinja's `multi_template` and
  `macros` features are off), so they are syntax errors that validate reports. Nothing reads the
  process environment (`{{ HOME }}` is an undefined variable). The `debug()` global, which would
  print the whole context, is removed.
- **Output.** Never auto-escaped, whatever the file name. The final newline is kept, and text with
  no tags renders byte for byte, CRLF included. Block whitespace follows Jinja (no `trim_blocks`);
  use `{%-` and `-%}` to trim.
- **Errors** are `template::Error { file, line, problem }`, shown as `file:line: problem`. A missing
  variable is `Problem::Missing(name)`, naming nested lookups in full (`machine.hostname`): for
  example ``files/gitconfig.tmpl:2: variable `signing_key` is not defined for this machine``. Its
  line is where the missing name itself sits, even inside a multi-line expression. Other problems
  are `Syntax`, `Render` (an unknown filter, a type error, printing none, an undefined result, a
  missing attribute in an attribute builtin, division by zero, an engine panic), and `Home`. Every
  error has a line except `Home` and an engine panic.
- **Validate** (`template::check_all_guarded(loadout, context)`) runs only after
  `Loadout::load_guarded` accepts the loadout with the same `Context`, so every source has already
  passed the confinement checks in section 3. It then re-canonicalizes each distinct `.tmpl`
  user-file source from every layer, requires it to sit inside the root and be a regular file, and
  only then runs `manifest::source_guard` on the source as reached and as its canonical file. A
  refused source is never opened: its one diagnostic is the guard's path-only message. Otherwise it
  reads the file, requires UTF-8, and syntax-checks it with no machine and no variables.
  `template::check_all(loadout)` is the same pass without the host guard, for synthetic and test
  use only; no host command calls it. The guard is a check before each open, not an atomic open: a
  file swapped between the check and the open, or a hard link to a reserved file, is not caught.
  Each failure is one error diagnostic on the template file, such as
  `files/gitconfig.tmpl: error: line 3: syntax error: …`, with exit 3. Unknown filters and missing
  variables are found only by `render`, because they depend on the machine.
- **Plan (U4)** reads the confined source and calls `render` with `Resolved.variables` and the
  machine's `Facts` before building any action, so a missing variable stops the run with zero
  writes. `render` itself reads and writes nothing.

### Paths

`target` and custom-agent paths start with `~/` (home) or `{config}/` (the OS user config dir).
They use `/`, and contain no `\`, `..`, `.`, empty parts, or NUL on any OS (`context::split_base`).
`{config}` is `$XDG_CONFIG_HOME` or `~/.config` on Linux, `~/Library/Application Support` on macOS,
and `~\AppData\Roaming` on Windows.

**Windows rules** (`context::windows_problem`) cover what a Windows join would otherwise reinterpret:

- a part holding `<>:"|?*` or a control character (this catches drive letters such as `C:` and
  alternate streams such as `a:b`)
- a part ending in a dot or space
- a device name such as `CON`, `NUL`, `COM1`, or `LPT9`, with or without an extension

`Context::expand` returns `None` for such a path when running on Windows, so a join can never leave
the base dir. Validate applies the rules by reach. A declaration that certainly reaches Windows (in
`os/windows.toml`, under `when.os` naming only windows, or a custom agent's `windows` path) gets an
error. One that only might (global, tag, or machine layer, with no `when.os` excluding windows) gets
a warning, because a narrower layer may still override or remove it. A declaration that never
reaches Windows keeps valid POSIX names such as `notes:2026`. U1's per-OS resolution turns the final
Windows target into an error.

`source` is relative to the loadout root and stays inside it after symlinks are resolved
(section 3, What validate enforces).

### Secret references

A loadout never holds a secret value. It holds only the name of an environment variable on the
machine: an `env` or `headers` value is either a string or `{ env = "NAME", prefix = "Bearer " }`.
**Default**: v1 resolves no secret at all. U6 writes the reference in each agent's own syntax, such
as `${GITHUB_TOKEN}` for Claude or `bearer_token_env_var` for Codex, so the agent reads the variable
when it starts. A credential's value never reaches an agent config, a journal, a backup, a plan, or
the output.

- If an agent has no documented env syntax (Antigravity, or a custom agent with
  `env_syntax = "none"`), a server that uses an env reference is an `unsupported` action for that
  agent. Loadout never guesses a syntax.
- Validate warns when a credential-looking header or env value (authorization, token, secret,
  password, api key) is written as plain text.
- Keychain and password-manager sources are deferred, because writing a resolved value into an
  agent config would put the secret on disk.
- OAuth tokens and agent auth stores are never read, copied, or written (section 6).

## 4. Local state (built, U3)

State lives in one directory per machine, never in the loadout.

| OS | Default state dir |
| --- | --- |
| Linux | `$XDG_STATE_HOME/loadout` or `~/.local/state/loadout` |
| macOS | `~/Library/Application Support/loadout` |
| Windows | `~\AppData\Local\loadout` |

Two overrides exist for tests and isolated homes (foundation, `src/context.rs`). Both must be
absolute when set; an empty value counts as unset.

- **`LOADOUT_HOME` (isolation).** When set, it replaces the home dir, and `Context.isolated` is
  `true`. Every path is then derived from that home and nothing else: `{config}`, `xdg_config`, and
  the default state dir. The ambient `XDG_CONFIG_HOME` and `XDG_STATE_HOME` are ignored, and so are
  the ambient `XDG_DATA_HOME`, and the agent dir variables (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `GROK_HOME`, `PI_CODING_AGENT_DIR`, `GEMINI_HOME`), so `Context.agent_dirs` is empty. Every path that **Loadout itself** reads or writes (targets,
  agent dirs, state) stays inside the isolated home, except an explicit `LOADOUT_STATE_DIR`.
  Isolation does not cover what a subprocess does with the real environment:
  - `git` (U8) runs against the state dir clone only.
  - **Package managers (U7) are never run under isolation.** Every program step becomes
    `blocked("programs are not installed under LOADOUT_HOME isolation")`, because brew, npm,
    cargo, uv, pipx, and flatpak write to the real `HOME` whatever `Context` says.
  - Real package installs in E2E use a disposable VM guest's actual `HOME`, without `LOADOUT_HOME`.
- **`LOADOUT_STATE_DIR`.** Replaces the state dir, isolated or not.
- **Without `LOADOUT_HOME`.** Ordinary overrides apply: an absolute `XDG_CONFIG_HOME` and
  `XDG_STATE_HOME`, and the absolute agent dir variables, which are collected into
  `Context.agent_dirs`. Units read agent overrides only through `Context::agent_dir(name)` and never
  call `std::env` themselves, so isolation is enforced in one place.
- `Context.xdg_data` is `$XDG_DATA_HOME` or `~/.local/share` on every OS, where OpenCode keeps its
  auth store. Isolation filters it the same way.
- `Context.xdg_config` is `$XDG_CONFIG_HOME` or `~/.config` on every OS, for agents such as OpenCode
  and Muse that use XDG paths even on macOS and Windows. `Context.config` is the OS-native config dir
  behind `{config}`.

```
<state>/machine.toml       name = "<machine name>", remote = "<loadout url>"   written by init (U9)
<state>/loadout/           the CLI's own clone of the loadout                  sync (U8)
<state>/state.json         ownership records                                   state (U3)
<state>/runs/<run-id>/     journal.json and backups/ for one apply             state (U3)
<state>/lock               held for one plan+apply                             state (U3)
```

`machine::name` (foundation) reads `machine.toml` only after checking that it canonicalizes inside
the canonical state dir and resolves to a regular file. A link inside the state dir is fine. A link
outside it, such as one to `~/.codex/auth.json`, is an invalid identity and is never opened. A FIFO, a folder, or a link to one is an invalid identity: doctor reports a failed check and
exits 4, and never blocks. When the file is absent, the machine name
defaults to the hostname, cut at the first `.`, lowercased, with other characters mapped to `-`.
Once the file is written, renaming the computer doesn't change the name.

**State files (built, U3, `src/state/`).** Loadout creates the state dir and `runs/` as `0700` on
Unix, and writes `state.json` and every journal as `0600`, through a temp file, `fsync`, and rename.
It reads `state.json` and journals only after checking that they canonicalize to a regular file
inside the canonical state dir, the same rule as `machine.toml`. A link planted there is an error and
is never opened. A corrupt or unknown-version `state.json` is an error (exit 1), never an empty
machine.

Writes are checked the same way. The configured state dir may itself be a link, so loadout
canonicalizes it once. Every folder it then writes into, such as `runs/`, a run folder, or
`backups/`, must be a real folder below that canonical root, never a link, a junction, or a file.
A planted `runs` link is therefore an error before anything is written. The lock records the
canonical state dir it holds, and `Run::begin` refuses a lock taken for another state dir
(`Error::WrongLock`). A lock taken through an alias of the same dir is accepted.

**Trust boundary.** The state dir is private to its owner (`0700`), and its contents are trusted
no further than that. Nothing in it is signed, because its owner can rewrite any of it, a checksum
included. Loadout instead validates the shape of everything it reads back before it acts on it:

- `state.json` targets must be absolute and clean, with valid hashes and run ids.
- A journal must name its own folder, and each entry's step must fit its change and result.
- Each backup must sit in its own slot (`backups/<n>` of the same run) with a plain tree layout.
- `created` folders must form one chain ending at the target's parent.
- Every target must lie under a target root (below).

The `--loadout DIR` checkout a run used is known only from its journal. Rollback reserves that
recorded checkout and the default `<state>/loadout` both. The state dir itself is always reserved.

**Target roots.** A file or tree target must be absolute and clean, and must lie under one of the
bases targets are built from:

- `Context.home` and `Context.config`, the bases of `~/` and `{config}/`
- `Context.xdg_config`
- each agent dir override (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, and so on), which may lie outside home

`observe`, `Run::begin`, `audit`, and rollback enforce this (`Error::OutsideRoots`), before the
reserved check and any read. Membership compares path identities by whole components, using
`reserved::identity`, which follows the same prefix rules as the reserved checks:

- **Two forms on each side.** A target belongs when its lexical form, or its physical form, equals
  or lies inside the lexical or physical form of a root. The physical form is
  `reserved::normalize`: existing ancestors are canonicalized, and it only stats and reads links.
  A path that can't be normalized contributes only its lexical form. Failed normalization never
  adds an acceptance.
- **Windows spellings.** `\\?\C:\…` and `C:\…` are one drive, and `\\?\UNC\srv\share\…` and
  `\\srv\share\…` are one share. ASCII letters in names compare case-insensitively. A configured
  UNC root works like any other root. An admin share such as `\\localhost\c$` is a share, never
  equated with the drive it maps to.
- **Unix spellings.** Names compare exactly and case-sensitively. A symlinked spelling of a root
  and its real path are one root; on macOS that covers `/var` and `/private/var`. On a
  case-insensitive macOS volume, a case variant of an *existing* root folder resolves to the same
  physical folder and is accepted through its physical form.
- **Lossless identity.** Names stay `OsString`. Two non-UTF-8 names that differ only in invalid
  bytes are different roots.
- **Links below a root.** A symlink below a root keeps its lexical membership, and the write
  follows it (section 5). A path outside every root in both its lexical and physical forms is
  refused.

One limit follows: a rollback must run with the same agent dir overrides the run had, or its
targets under an override that is no longer set are refused.

- `state.json`: `{ "version": 1, "records": [Record] }`. A `Record` is `{ target, item, sha256, run }`:
  the `Target` and `ItemRef` (serialized exactly as in `plan --json`), the hash of what loadout last
  wrote, and the id of the run that wrote it. There is one record per target, and an MCP entry is
  its own target, so ownership is per key.
- `runs/<run-id>/journal.json`: `{ version, run, started, loadout, status, entries }`. `loadout` is
  the active checkout the run used, and `status` is `running`, `finished`, or `rolled-back`. Each
  entry holds `item`, `target`, `change`, `prior`, `record` (the record before this run), `backup`
  (`{ path, kind, modes, dirs }`), `step` (`pending`, `started`, `done`, `released`, `failed`),
  `after` (the hash the action leaves), and `created` (the folders it made). No file content is in
  a journal.
- `runs/<run-id>/backups/<n>`: the prior file, or a copy of the prior folder, of entry `n`. The
  copy keeps the original Unix modes, which stay private because `backups/` is `0700`. The journal
  records the same modes, and rollback restores only when the two agree.
- A run id is the UTC start time with nanoseconds, `YYYYMMDDTHHMMSS.nnnnnnnnnZ`, so byte order is
  time order and the newest run is the greatest id. Rollback accepts only a name that matches a run
  folder exactly, so a run argument can't name a path.

## 5. Apply lifecycle (state built by U3; plan and apply source, U4)

Settles the [Apply lifecycle](./wayfinder/loadout-v1/tickets/04-apply-lifecycle.md) ticket as
delegated defaults.

- **One way.** The CLI reads the loadout and writes to the machine. It never writes, commits, or
  pushes to the loadout. The user adds a new machine name by editing `loadout.toml`; `init` prints
  the snippet to paste.
- **Copy, never link.** Files, skills, and instructions are copied, or rendered for templates, on
  every OS. Links would need admin rights or Developer Mode on Windows, and would tie the machine to
  the clone.
- **Ownership.** `state.json` records every target the CLI wrote, with the SHA-256 of what it wrote.
  An entry inside a shared file, such as an MCP server in `~/.claude.json`, gets a record per key.
- **Plan as data.** `plan::build` compares desired content, the disk, and the records, and returns
  `Action`s. Each action carries:
  - `prior: Observed`: what the plan saw at the target, from `state::observe`. It is `Absent`,
    `Present { sha256 }`, or `Installed` (for programs). This precondition travels with the action
    into the journal and into the safe write.
  - `content: Option<Content>`: `File(FileContent { bytes, executable })`, `Tree` (relative path to
    `FileContent`), or `Entry` (one JSON value). Content is never serialized.
  - one of these `Change`s:

  | Change | Meaning |
  | --- | --- |
  | `create` | the target doesn't exist yet |
  | `update` | an owned target differs from desired |
  | `unchanged` | an owned target already matches |
  | `adopt` | an unowned target already equals desired; it is recorded |
  | `replace` | an unowned target is in the way; it is backed up, then overwritten. Interactive `apply` only |
  | `drifted` | an owned target changed outside loadout |
  | `remove` | an owned target the loadout no longer declares |
  | `install` | a program to install |
  | `unsupported(reason)` | an agent can't take the item |
  | `blocked(reason)` | the action needs elevation, or an unowned MCP key with the same name exists |

  `plan --json` prints the actions without their content.
- **Targets.** `Target` is `Path { path }`, `Entry { file, format, pointer, key, shape }`,
  `Program { manager, package }`, or `None`. `Entry.shape` is required: state passes it unchanged
  to the `KeyMerge` check and `mcp::edit`, and never infers it from the agent id (a custom agent's
  shape is whatever it declared). Records written without a shape are invalid, never defaulted.
  `None` (JSON `{"type":"none"}`) exists only for an `unsupported` action whose item has no
  destination on this machine, such as Cursor instructions (`Action::unsupported` builds it with
  `prior: Absent` and no content). U4 rejects any other combination before I/O, and `None` never
  reaches observe, ownership, backup, the journal, or a write. State rejects a caller-provided
  `None` with `state::Error::NoTarget { operation }` before touching the file system:
  `Run::begin` scans every action, journaled or not, before `disk::state_root`, and `observe`,
  `Run::write`, and `Run::remove` check before anything else. The caller's already-acquired lock is
  outside that claim. `state::label` shows `None` as `no target`, and a stored record or journal
  entry holding `None` fails validation (`target_shape` is false) before any destination is
  touched, so `None` is never persisted. An unsupported action with a real destination keeps that
  target.
- **Drift.** A drifted target is left alone and reported, with exit 4. `apply --force` backs it up
  and overwrites it. The background service never forces.
- **Removal.** An owned target that hasn't changed since loadout wrote it is backed up, then removed.
  A drifted one is released from ownership and left in place, with a warning. Programs are never
  uninstalled in v1. `Run::remove` makes that call itself, from the journaled record: the plan
  marks every undeclared owned target `remove`, and the result is `Removal::Removed` or
  `Removal::Released`. A target that is already gone is released.
- **Hashing** (`state::observe`, U3) is defined once. It is domain-separated by kind, so a file can
  never hash equal to a tree or an MCP entry. `Observed::Present.sha256` is the lowercase hex of:
  - a file: `SHA-256("loadout:file:v1\n" ‖ X ‖ 0x00 ‖ u64 big-endian length ‖ bytes)`. `X` is
    the byte `x` when the file is executable and `-` otherwise. It is always `-` on Windows, and
    always `-` for any target whose execute bits loadout doesn't manage. A mode-only change
    therefore changes the hash.
  - a tree: `SHA-256("loadout:tree:v1\n" ‖ for each file in byte-sorted `/`-separated relative-path
    order: path ‖ 0x00 ‖ (`x` or `-`) ‖ 0x00 ‖ u64 big-endian length ‖ bytes)`
  - an MCP entry: `SHA-256("loadout:mcp-entry:v1\n" ‖ canonical JSON of that one key's value)`.
    The canonical JSON is produced by one helper, never by ad hoc formatting. It rebuilds the
    `serde_json::Value` with every object's keys sorted by byte order, recursively through arrays
    and nested objects. It then serializes with `serde_json::to_vec`: compact, no whitespace, and
    serde_json's own escaping and number formatting. The sort is explicit because U6 enables
    serde_json's `preserve_order`, under which `Map` keeps insertion order. A value read from TOML
    is first converted to `serde_json::Value`, so both formats hash the same way.

  Plan, journal, and records all use this definition: `state::digest(&Content)` for desired
  content, and `state::observe` for the disk. Observing a folder hashes its regular files and walks
  its subfolders without following links. A link, FIFO, socket, or device inside an owned folder,
  or as the target itself, is `state::Error::Special`, and nothing is opened. "Executable" on disk
  means any `x` bit is set.
- **Safe write.** Before the first write, the run's `journal.json` lists every action with its
  `prior` and backup path. For each write, `Run::write(action, content)`:
  1. Re-observes the target and compares the result to `action.prior`. If they differ, it aborts
     that action with `state::Error::Changed` and writes nothing, so a target that changed after the
     plan (even an unowned file planned for `replace`) is never overwritten.
  2. Resolves a symlinked target and writes the real file, so the user's links survive.
  3. Writes a temp file in the same directory, `fsync`s it, and renames it over the target.
  4. Sets modes, on Unix only. Start from the existing file's mode, or for a new file from the
     process default (`0o666 & !umask`). Keep every read and write bit as it is. Then, for each
     class (user, group, other):
     - when `FileContent.executable` is `true`, set that class's `x` bit only if the class has its
       `r` bit;
     - when it is `false`, clear that class's `x` bit.

     So `0644` becomes `0755`, `0600` becomes `0700`, and `0755` without `executable` becomes
     `0644`. Nothing is ever widened beyond the existing read bits. Windows ignores `executable`
     and execute bits entirely. Content hashes include the bit, so a mode-only change is an
     `update`.
  5. Marks the action done in the journal.

  As built (U3):

  1. The target is checked against the target roots and then
     `reserved::check_path(context, loadout, path, Use::Write)` before it is read, backed up, or
     written. A conflict is `state::Error::Reserved`.
  2. The re-observation and the backup come from one read, so the backup is exactly the observed
     `prior`.
  3. The backup is taken, and the journal marks the action `started` with its `after` hash and the
     folders it will create, before the target changes.
  4. The new content is staged privately in `.<name>.loadout-<run>-<n>.tmp`:
     - Each file that will hold bytes is created with owner-only bits (`0600`) from its first
       open. Nobody else can open it and keep a descriptor that would read the bytes after a
       later chmod. The process default for a new file or folder (the umask and any default
       ACL) comes from separate empty probes (`….mode`, `….fmode`, `….dmode`), which are created
       and removed at once and never hold data. The file gets its final mode only after its
       bytes are written and fsynced.
     - A staged tree's folders are `0700` from their creation until every file is written.
       They then get their final modes, deepest first.
     - Final modes never widen past the rule above.
  5. The target is observed once more just before the rename, and any difference aborts with
     `Changed`.
  6. A file over a file is one rename. When a folder is involved, the old target is renamed aside
     to `.<name>.loadout-<run>-<n>.old`, the new one is renamed in, and the old one is deleted. If
     the second rename fails, the old one is moved back.
  7. A scratch name that already exists is never deleted or overwritten. It belongs to something
     loadout can't vouch for, so the write fails instead.

  Other rules:
  - Through a symlink, the link's real target is written. A dangling link is created at its
    destination.
  - Unix modes for a tree come from the existing file at the same relative path. Setuid, setgid,
    and sticky bits are dropped.
  - An action whose target already equals its content (`adopt`) is recorded without a write or a
    backup.
  - A write that fails is marked `failed`, and the empty folders it made are removed. A step that
    fails after the new content is already in place is marked `done`.
  - `Run::finish` records ownership for every action that reached the disk, and must run even
    when some actions failed. A `Run` dropped without `finish` leaves an interrupted run.
    `Run::finish_with_lock` does the same and hands back the state lock, on success and on
    failure, so U4 can keep holding it; `finish` is `finish_with_lock` with the lock dropped.
  - `Lock::check(context)` tells a caller holding a lock, read-only, whether it guards
    `context`'s state dir, compared as `Run::begin` compares them. It creates no folder, opens
    no file, takes no lock, and leaves the lock held, so U4 checks the lock it was handed before
    work that may never begin a `Run` (a no-op or program-only plan). A missing or unresolvable
    state dir is an error, never created. `Run::begin` keeps its own check, after its `None`
    guard and before any other I/O.

  **Durability.** On Unix, every staged file is fsynced, and so is every staged folder. Every
  folder whose entries change is fsynced after the change: the target's parent after each rename
  and move-aside, each folder loadout creates, and the state, run, and backup folders.

  Windows has no folder fsync, so there the renames are the boundary. The recovery loadout proves
  covers a process that stops at any point: rollback and the journal handle it, and the tests and
  VM runs exercise it. Power loss is not proven, and loadout doesn't claim it.
- **Rollback.** `loadout rollback [RUN]` restores a run's backups and deletes the files that run
  created; the default is the latest run. `status` reports an interrupted run, and the same command
  rolls it back.
  - **Preflight before any change.** Rollback validates everything it will need before it touches
    a target. A problem found there leaves every target untouched. The preflight covers:
    - the journal (trust boundary, section 4) and `state.json`
    - every target's roots and reserved check
    - every created folder (the ancestor chain, plus a reserved `WriteInto` check)
    - every needed backup: its slot, its type, its layout against the journal, and its hash
      against `prior`
    - every scratch name

    An I/O error during the restore itself can still stop a rollback halfway. Running it again
    skips the targets it already restored.
  - Every `started`, `done`, or `failed` target must hold the run's `after` (it is restored) or its
    `prior` (nothing to do). Anything else blocks the whole rollback with exit 4, and nothing is
    restored. That includes a later run's write, a user edit, or a later user deletion. A `failed`
    target that holds neither is left alone, because the run never changed it. A reserved target
    blocks the same way.
  - **A folder swap that stopped between its two renames.** Here the target is absent, and its
    `.old` scratch holds the prior tree. That `.old` counts as evidence only if it is a real file
    or folder (not a link) whose content hashes to `prior`, and only for a `started` or `failed`
    action. Then the backup is restored.
    - A missing, linked, or mismatched `.old` blocks instead.
    - A `done` action already put its result in place. A `.old` left beside it, because its
      removal failed, says nothing about a later deletion of the target, so the deletion blocks
      like any other later change.
    - Rollback's own `…-rollback.old` holding `after` proves an earlier rollback stopped
      mid-swap, whatever the action's step.
  - **Rollback's own scratch names.** A restore or delete writes through rollback's own scratch
    names, which preflight checks before any change:
    - `…-rollback.tmp` must be free, or verify as the prior.
    - `…-rollback.old` must be free, or verify as `after` while the target is absent.

    Anything else there blocks the whole rollback, and the occupant is kept. A target that needs
    no restore only reports such an occupant.
  - **Scratch handling.** A scratch name is deleted only when it verifies as the run's own: `.tmp`
    must hash to `after`, and `.old` to `prior`. Rollback's own scratch, `…-rollback.tmp` and
    `.old`, is verified the same way, so a rollback that stopped can be rerun. Anything else is
    left in place and reported (`RolledBack.leftovers`, printed as a warning). A rollback scratch
    that was only partly written fails the retry until it is removed by hand.
  - A backup is restored with its exact recorded modes. Each target is re-observed just before it
    is restored.
  - Folders the run created are removed when they are empty again, and only along the validated
    ancestor chain.
  - The records of every touched or released target go back to the journaled pre-run record.
  - Rolling back a run that is already rolled back is an error (exit 1).
- **Concurrency.** `<state>/lock` (`std::fs::File::try_lock`) keeps a background update and a manual
  apply from overlapping. The second one exits 1 with a clear message. `state::lock` returns the
  `Lock`, and `Run::begin` takes it by value, so a run can't start without holding it. `rollback`
  takes it too. `status` only probes it (`state::busy`) and never creates anything.
- **`loadout status [--json]`** (built) lists each record with its health (`ok`, `drifted`,
  `missing`, or `unchecked` with a reason) and each run (`finished`, `rolled-back`, `interrupted`,
  `in-progress` when another process holds the lock, or unreadable with its error). It exits 4 when
  a record isn't `ok`, or when a run is interrupted or unreadable. The audit uses the default
  checkout `<state>/loadout` as the reserved active checkout.
- **Plan and apply (U4, source; unexecuted).**
  - **Producers.** User files are U4's.
    - Each target is expanded with `Context::expand` and checked with `reserved::check_path(Write)` before its source is read. A conflict is an `unsupported` action with that target.
    - Sources are read through `source::Sources`, U5's guarded reader moved to a shared subject. It applies the same confinement, the same Git-pointer guard, and `check_source_file` on the reached and canonical identity before any open.
    - A user file or template keeps its source's execute bit on Unix. Agent instructions clear it.
    - A `.tmpl` source must be UTF-8, and every template renders before any action exists. A missing variable, a refused source, or any producer or `materialize` error is `plan::Error::Invalid` (exit 3) with nothing written. Diagnostics carry paths, never content.
  - **One hash.** `d = state::digest(content)`, `h = state::observe(target)`, `r` = the record:

    | Disk | Record | Change |
    | --- | --- | --- |
    | absent | none | `create` |
    | absent | some | `drifted` |
    | `d` | none | `adopt` |
    | `d` | `d` | `unchanged` |
    | `d` | other | `update` |
    | other | `h` | `update` |
    | other | other | `drifted` |
    | other | none | `replace` for a path; for an MCP entry, `blocked`, never `replace`, `--force` included |

    An owned target the loadout no longer declares is `remove`. A blocked entry keeps the observation it was planned from, and carries no content.
  - **Identity.** Every concrete destination (declared writers, unsupported and blocked declarations, and owned records) is compared as written and as `reserved::normalize` resolves it (lossless, case folded only on Windows, as state's target roots are). So a stable folder link such as `~/alias -> ~/real` makes `~/alias/tool` and `~/real/tool` one place.
    - Two MCP entries that share a document stay apart only when they declare the same format and neither key's address is the other's or inside it. The address follows U5's rule exactly: the map pointer's parts decoded `~1` then `~0`, then the key, with `_` read as `-` for Pi. U4 keeps a private copy of that rule until a shared home below both callers exists. There is no shape rule.
    - Roles decide what a relation means:
      - Two declared writers that are one destination, or one inside the other, are invalid before any write (exit 3), whether or not their bytes agree. Possibly one: both `blocked`.
      - A removal that reaches, or might reach, any declared destination (unsupported and blocked ones included) or another removal is `blocked`.
      - An unsupported declaration writes nothing and conflicts with no writer.
    - **Migrations** are judged on the stored key, not on each shape's logical address. The stored key is the decoded map pointer and the key exactly as written, with nothing folded, so a Cursor `my_docs` and a Pi `my_docs` are one stored key.
      - **Every** ownership record owns its stored key, whether or not its exact target is still declared (as an ordinary or an unsupported item). A declared key with the same stored key in one document, or possibly one, under another format or shape, is a migration, and that declaration is `blocked`.
      - A record the loadout no longer declares is not removed when its file, map, and key are declared as written under another contract (one action for that identity). Reached only through a link or a case spelling, its removal is `blocked` too.

      - A record whose exact target is declared but unsupported here is kept the same way, migration or not: nothing writes its key, but the record stays, and so does what it owns. A record still declared as an ordinary writer needs no retained copy, because that writer carries its contract.

      Either way the record stays as it is (loadout never converts ownership) and is kept as **retained ownership** under its OLD format and shape. That retained ownership protects its place: a writer or removal that conflicts with it under the old contract is `blocked`, such as a sibling written in a new format beside an old JSON key. A blocked migrating declaration still counts against other writers, but its new contract never stands in for the retained one, so an old sibling compatible with the old contract is judged against that.
    - Every block, migration ones included, keeps a reason the action already has (such as an earlier refusal) and appends each new reason once. Unsupported actions are never changed. A blocked action carries no content, and its prior stays what was observed.
    - A place that can't be resolved blocks that writer, and blocks every removal, since none can be shown to stay clear of it. A retained ownership whose place can't be resolved protects nothing beyond that: writers at other places can't be related to it.
    - No hard link, bind mount, or link changed after the plan is covered.

    `observe` errors map like this. An MCP entry is observed by U3's real entry transaction, so an absent document plans a `create`:
    - `Reserved` is `unsupported` for a declared target and `blocked` for a removal.
    - `Special` is `blocked` (R1), though state's own classifier doesn't list it.
    - Then `state::Error::is_blocked` decides: a fact about the machine (an MCP config loadout won't edit, a folder in its place, Grok holding its lock, or a server name taken by a value loadout doesn't own) is `blocked`, exit 4.
    - Anything else, such as an I/O fault or a broken pointer or shape contract, is exit 1.
  - **Programs.** `Installed` becomes `unchanged` with prior `Installed`. `Missing` becomes `install`, or `blocked` when it needs `--elevated` the run lacks, with its real `Target::Program`. `Unsupported` and `Blocked` steps have no package, so they get `Target::None`. Two programs that pick one manager and package on this machine are exit 3.
  - **Case check** (Linux and macOS, where a folder can be either way). Two destinations whose written or resolved paths differ only by case (or one inside the other) are asked about at each component where their spellings differ. The nearest existing folder decides, read-only: a child with an ASCII letter is statted under both spellings.
    - Same `(dev, ino)`: insensitive.
    - The swapped spelling missing, or a different file: sensitive.
    - No usable child, or any error: unproven.
    - Insensitive at every difference: one destination. Any sensitive folder: apart. Otherwise: possibly one. The roles above then apply, and compatible distinct MCP keys stay apart either way.
    - A parent is never asked about its child, and the OS default is never taken as proof.
    - On Windows, ASCII case is already folded in the identity, and nothing read-only can prove more.
  - **`Plan::check`** runs inside `build` and again in apply:
    - `Target::None` only with `unsupported`/`blocked`, prior `Absent`, and no content.
    - A program action only with `install`, `unchanged`, `blocked`, or `unsupported`, a fitting prior, and no content.
    - A write only with content of its target's kind.
    - No entry `replace`.
    - An `unsupported` path or entry observes nothing; a `blocked` one may keep what it observed.
    - No concrete identity twice: a path, an entry's file, pointer, and key, or a manager and package. `None` has no identity.
    - A failure is an internal error, exit 1.
  - **Apply.** `run` builds a read-only preflight first, so an invalid loadout, a missing variable, or an interrupted run leaves no state dir and no lock. It then takes `state::lock` and calls `run_under_lock`, which every route uses.
    - `run_under_lock` first checks that the lock it was given is the one for `context`'s state dir (`Lock::check`), before any state, plan, manager, or target work. This covers every route, including a no-op or program-only run that never starts a `Run`.
    - An invalid loadout found under the lock leaves no destination written; only taking the lock may have created the state dir and its lock file.
    - Under the lock, it checks the interrupted-run gate, reloads state, and rebuilds and rechecks the plan. Preflight program steps are reused only when built for the same programs, facts, and context, and `programs::install` re-checks each step anyway.
    - The journal gets only path and entry actions that write: `create`, `update`, `adopt`, `remove`, plus `replace` in interactive mode and `drifted` with `--force`. Removes come first.
    - A run with nothing to journal starts no `Run` and leaves no journal.
    - After each path write, the target is read back, and anything other than what was written is `mismatch` (exit 4). This catches a file system that drops execute bits, which no read-only plan can see.
    - Every begun run is finished **before** any program install, so killing a hung installer never interrupts a journal. If finishing fails, the first failure stays reported, no program is installed, and the exit is 1.
    - In background mode, `replace` and drift are skipped and nothing is elevated.
    - **Cancellation.** Only a supervised background run, whose `Context.containment` is inherited, can be cancelled; the foreground's `OwnGroup` never is. No command starts a supervised worker yet; that is a separate unit. Before the run begins, before each journaled action, and before each install, a cancelled run starts no new work and reports the rest as `skipped`. A run that has begun is still finished through `finish_with_lock`, so its journal is complete and its lock comes back. `Report.cancelled` is a runtime field, never saved, and makes the exit 4 even with nothing to do. A failed action or finish still makes it 1, and a failed manager query is still a runtime error (1), though U8 also sets the flag then. Each check comes just before its step, so a cancellation that arrives while a step runs doesn't stop that step; U8's own refusal still guards a query or installer started after the check.
    - Exits: 1 for a failed action or finish, 3 invalid, 4 when anything is blocked, skipped, or mismatched or the run was cancelled, else 0. Unsupported actions alone exit 0, and they are always listed.
  - **Integrated from U3 and U8** (source composition, not yet built or run):
    - `Run::finish_with_lock(self, &mut State) -> (Lock, Result<(), state::Error>)` (U3) hands the lock back on success and on failure; apply holds it through the installs and the report.
    - `Lock::check(&self, &Context) -> Result<(), state::Error>` (U3) is the read-only association check `run_under_lock` calls first. It returns `WrongLock` for another state dir.
    - MCP entries are U3's real transactions, observed and mapped as above.
    - Cancellation is U8's `Context.containment`, read through `is_cancelled()`. There is no `Options.cancel`.
- **MCP entries (built, U3).** An MCP server is a `Target::Entry { file, format, pointer, key,
  shape }`: one key in an agent's shared config, owned per key. State owns the transaction and
  imports `mcp`; `mcp` never imports state. `state::Error::EntryNeedsMcp`, the interim refusal, is
  gone.
  - **Observe** (plan, status): roots, then `reserved::check_path` with `Use::KeyMerge(Merge {
    map: pointer, format, shape })` before any read, then one read of the real document,
    `mcp::parse`, and the key's `hash::entry`. It takes no lock, so plan and status never create
    Grok's lock file.
  - **Reading a document.** `disk::read_document` reads one regular file. A folder is
    `Error::NotADocument` and a FIFO, socket, or device is `Error::Special`, both from metadata
    before any listing or open. On Unix the open is non-blocking, so a file swapped for a FIFO
    after the check can't hang, and the opened handle must be the same file (device and inode)
    that was checked; Windows re-checks only that it is a regular file. The generic tree read
    never serves documents.
  - **Write and remove, one key per transaction** (several keys in one document are separate
    transactions in plan order):
    1. Roots and the `KeyMerge` check.
    2. `mcp::lock(context, loadout, file)`, after the state lock, held until the action is
       journaled done (Grok's `.config-init.lock`; nothing for other agents).
    3. One read, then `mcp::parse` and the key's `prior`, else `Changed`. A remove of a key that
       drifted from its record, or is already gone, is released and left.
    4. An equal value (an adopt, or an equal update) is done with no write: the journal keeps
       a `pending` backup and no `document`.
    5. **Ownership is the record, not the label.** A key that exists, has no record, and differs
       from the wanted value is `Error::UnownedEntry` (blocked, exit 4) whatever the action is
       labeled (`adopt`, `update`, `drifted`, `create`), before any edit, backup, or `started`.
       A key that no longer holds `prior` is `Changed` first.
    6. `mcp::edit(bytes, format, pointer, shape, set, remove)` with that one key.
    7. A backup of the owned key's value only, as canonical JSON (journal backup kind `entry`).
       The shared document is never copied, so other servers and agent-written keys such as
       Claude's `oauthAccount` never reach the state dir.
    8. The journal records `started`, `after`, and `document { base, staged }`, the
       `hash::document` of the bytes read and of the bytes about to be staged, before anything
       is staged.
    9. The staged, fsynced, atomic `disk::put`. Its verify reads the whole document again just
       before the rename and requires exactly the bytes read in step 3 (or still no document).
       An existing document keeps its exact mode and its symlink. **A newly created document is
       0600**: an explicit privacy choice, since agents later write their own state into these
       files.
  - **Replace is refused** for an entry at `Run::begin` (`WrongChange`, a contract fault, exit 1):
    a server name loadout doesn't own is never overwritten, so a planner never asks. `drifted`
    (with `--force`) applies to an owned key and backs up the drifted value. The planner labels
    an unequal unowned key `blocked`; `UnownedEntry` is state's own guard if it doesn't.
  - **Journal entries match what happened.** An entry that `started`, `failed`, or finished
    `done` with an `after` other than its `prior` actually edited its document, so it must carry
    `document`, and if its key existed before, a backup of kind `entry` (not `pending`). A
    `pending` or `released` entry, or a `done` one whose `after` equals `prior`, edited nothing
    and carries neither staged hashes nor an `entry` backup. Path entries never carry them, and
    entries never carry a `file` or `tree` backup. Anything else is `Error::Invalid` before
    rollback changes anything. Journals from before entries keep loading.
  - **Rollback**, per key: only the owned key decides. It is restored when it still holds
    `after`, kept when it already holds `prior`, and otherwise blocks the whole rollback. Other
    keys changed since the run never block and are kept, because the restore edits the
    document's current bytes.
    - **Grok locks first.** After the state lock, the validated journal, and the state file,
      rollback finds the lock of every Grok config among the entries it could restore (the
      `GROK_HOME` override and `~/.grok` are two; aliases of one config are one, by
      `mcp::LockId`), orders them by that same identity (never by the spelling a path happens
      to keep, so two rollbacks naming one home differently still take the locks in one order),
      and takes them all before preflight and before any target or scratch is touched. An
      entry failing its reserved check is skipped here and blocks in preflight. They are held
      until rollback returns, and released if any can't be taken (blocked, exit 4). Restores
      never take a lock themselves, so aliases can't deadlock. Taking a lock may create
      Grok's home and its `.config-init.lock`, even when rollback then blocks: that lock
      metadata is the one write outside the unchanged-destination guarantee. Observe, `audit`,
      and `status` take no lock.
    - **Preflight checks this rollback's restores together.** Before anything changes,
      preflight reads every entry's current document. Then, per physical document (told apart
      by resolved path identity, so every alias of one document counts as one) and in the
      order the restores will run, it checks in memory that each restore finds its key as
      preflight saw it and is an edit `mcp` will make on the document as the earlier restores
      leave it. So a document that can't take an edit (a Muse `schema_version` other than 1,
      mixed newlines, a map that is no longer a table), and a restore that only an earlier
      restore makes impossible (two Pi names that collide once both are back), both block the
      whole rollback with every target unchanged. Document and environment problems are
      blocked; I/O and contract faults, including a document path that can't be resolved,
      keep their exit 1. The simulated bytes are only checked, never written. Changes made by
      others after preflight are not covered: the restore below rechecks.
    - **The restore recomputes.** Each key's restore reads the document again, requires the key
      to hold what preflight saw (else `Changed`), edits those current bytes, journals a fresh
      `rollback { base, staged }`, and verifies only across its own read-to-rename window. A
      whole-document comparison with preflight would block on unrelated edits, so there is
      none.
    - The backup must be this run's own slot, one JSON value hashing to `prior`. A document the
      run created stays after rollback, holding an empty map (and Muse's `schema_version`).
  - **Scratch.** A scratch name beside a document is never followed: a link is refused before
    anything reads through it. The run's `.tmp` or rollback's `-rollback.tmp` counts as
    loadout's own only if it is a regular file whose exact bytes hash to the `staged` value
    journaled before it was staged. Anything else, including a document holding the same key
    with other bytes, is kept and reported. When rollback needs that name, it blocks before any
    change. These hashes check bytes against the recorded intent. They don't authenticate a
    journal the state dir's owner rewrote; a changed hash only makes loadout refuse.
  - **Errors.**
    - `Error::Mcp { target, source: mcp::Error }` and `Error::NotADocument` are blocked (exit
      4) when the document or its surroundings are the problem: unparseable, duplicate or
      unrepresentable keys, mixed newlines, a Muse schema or Pi name loadout won't write, a
      folder in its place, or Grok holding its lock.
    - Pointer, shape/format, and I/O faults are not blocked (exit 1).
    - `Error::UnownedEntry` is blocked: a fact about the machine, not a broken contract.
    - An entry with an invalid pointer, an empty key, or a shape written in another format is
      invalid in `Run::begin`, `state.json`, and journals.

## 6. Agents (built: U5 adapters, U6 MCP)

Evidence: `reports/agent-inventory.md` (final), T3 Code `4fef307`, and
[Agent config surfaces](./wayfinder/loadout-v1/tickets/01-agent-config-surfaces.md). `~` is the home
directory. Each agent env override below is read through `Context::agent_dir`, which honours it only
outside `LOADOUT_HOME` isolation (section 4). The background service records the overrides at
install time (U8).

| Agent | Skills dir | Global instructions | MCP file, map, shape | Env syntax |
| --- | --- | --- | --- | --- |
| claude | `$CLAUDE_CONFIG_DIR` or `~/.claude`, then `/skills` | `…/CLAUDE.md` | `$CLAUDE_CONFIG_DIR/.claude.json` or `~/.claude.json`, `/mcpServers`, `claude-json` | `${VAR}` |
| codex | `~/.agents/skills` | `$CODEX_HOME` or `~/.codex`, then `/AGENTS.md` | `…/config.toml`, `/mcp_servers`, `codex-toml` | `codex-env-vars` |
| cursor | `~/.cursor/skills` | unsupported (User Rules live in app settings) | `~/.cursor/mcp.json`, `/mcpServers`, `cursor-json` | `${env:VAR}` |
| grok | `$GROK_HOME` or `~/.grok`, then `/skills` | `…/AGENTS.md` | `…/config.toml`, `/mcp_servers`, `grok-toml`, under Grok's `.config-init.lock` flock | `${VAR}` |
| opencode | `<xdg_config>/opencode/skills` on every OS | `<xdg_config>/opencode/AGENTS.md` | `<xdg_config>/opencode/opencode.json` or `.jsonc`, `/mcp`, `opencode-json` | `{env:VAR}` |
| pi | `$PI_CODING_AGENT_DIR` or `~/.pi/agent`, then `/skills` | `…/AGENTS.md` | `…/mcp.json`, `/mcpServers`, `pi-json` | `${VAR}` |
| muse | `<xdg_config>/muse/skills` (macOS, Linux) | unsupported (no documented path) | `<xdg_config>/muse/settings.json` (macOS, Linux), `/mcp_servers`, `muse-json` | `${VAR}` |
| antigravity | `$GEMINI_HOME` or `~/.gemini`, then `/antigravity-cli/skills` | `…/AGENTS.md` | `…/config/mcp_config.json`, `/mcpServers`, `antigravity-json` | none (undocumented) |

`<xdg_config>` is `Context.xdg_config`: `$XDG_CONFIG_HOME` (outside isolation) or `~/.config` on
**every** OS. OpenCode and Muse use it even on macOS (not `~/Library/Application Support`) and
Windows (`%USERPROFILE%\.config`, not `%APPDATA%`), so their rows never use the OS-native
`{config}`. Muse on Windows is unsupported until its paths are documented. Windows paths for Codex, Cursor, Pi,
and Antigravity are inferred from home-directory joins in their source; Windows VM evidence confirms
or corrects them.

- **Capabilities are data** (`agents/table.rs`). `table::adapter(agent, os)` is one `match` that
  returns each kind as `Supported(place)` or `Unsupported(reason)`, with the place as a root
  (home, an override variable with its default, or `xdg_config`) plus a relative path. Each
  adapter also records the folders it reads besides its own, its T3 pickup, whether its Windows
  paths are inferred, and its evidence. An undocumented path is `Unsupported` with that reason, so
  no separate `Unknown` state exists. An item an agent can't take is an `unsupported` plan action,
  never a silent skip. There is no plugin framework.
- **`agents::materialize(resolved, context, loadout)`** (built, U5). For every skill, instructions
  item, and MCP server, and every agent in its effective `agents` list, it returns either a
  `Desired` or an `unsupported` action:
  - Skills: `Target::Path(<skills dir>/<id>)` with `Content::Tree`. Instructions:
    `Target::Path(<file>)` with `Content::File`, never executable. MCP: `Target::Entry` with the
    server id as key, the agent's map, format, and shape, and `Content::Entry(mcp::entry(..))`;
    an `mcp::Unsupported` becomes an unsupported action on that same Entry target.
  - A kind the agent lacks, or a custom agent that declares no table or no path for this OS, is
    unsupported with `Target::None`. A skill id that is a Windows device name is unsupported on
    Windows with its path.
  - `context` is the only OS input. `loadout` is the active checkout. Every destination runs
    through `reserved::check_path` (`Write` for skills and instructions, `KeyMerge` for MCP)
    before any source is read or any other stat, and a conflict is unsupported with that target.
  - OpenCode: `opencode.jsonc` (format `jsonc`) when it already exists, after its own reserved
    check, otherwise `opencode.json`.
  - Sources are read again from the checkout and copied verbatim (never rendered). Each source,
    and each file a skill walk reaches, must canonicalize inside the checkout and outside its Git
    metadata (`manifest::git_metadata_guarded`, validate's own detector with a guard). Each Git
    pointer file the detector would open (`.git` as a gitfile, `commondir`) first passes
    `reserved::check_source`, as reached and as its canonical file, so a pointer aliasing an auth
    store, private settings, or state inside the checkout is refused before any read and never
    as a parse error afterwards. Ordinary pointers, their confinement, and Git's relative bases
    are unchanged. A skill may hold
    only regular files, folders, and links to files inside the loadout; a link to a folder, a
    special file, or a `.git` entry is refused. A skill needs `SKILL.md`. Every source file then
    passes `reserved::check_source` (below), both as reached and as the file it lands on, before
    it is opened.
  - **Collisions** are errors. Every pair with at least one agent destination is compared: agent
    against user file target, and agent against agent, built-in or custom. Two writes collide when
    their paths are equal or one is inside the other, compared as written and with links resolved
    (`reserved::normalize`), with case folded by the OS default as in section 3. Two MCP entries in
    one file (as written or through an alias) collide when they declare different formats for it,
    JSON and JSONC included, since those differ in what an edit must preserve and the file's
    bytes never decide; or when one key address (map pointer plus key) equals or contains the
    other, with Pi folding `-` and `_`. Equal formats with disjoint maps or keys merge. User file
    against user file stays with U1 and U4.
  - Problems with sources or collisions return `agents::Error::Invalid(Diagnostics)`: exit 3, and
    nothing is planned or written. Materialize never writes; it reads loadout sources and stats.
- **Skills** are copied into each target agent's skills dir as `Content::Tree`. Validate has already
  confined the folder (section 3), and U5 re-checks it when copying. It copies regular files,
  dereferences file links (the copy gets the file's bytes, not a link), and keeps each file's
  executable bit (`FileContent.executable`). A skills dir loadout doesn't own is never deleted.
- **Instructions**: at most one `instructions` item may reach each agent; U1 makes a second one a
  resolve error. The file is owned whole.
- **MCP is a key merge, never a file write** (U6, built). Parse the file, change only the server
  names loadout owns, and keep every other key, comment, and format detail.
  - TOML goes through `toml_edit`. A server is written as a `[map.name]` table with nested values
    inline, or inline when the map or the replaced entry was inline. A replaced table keeps its
    place and the comment above its header.
  - JSON and JSONC go through `jsonc-parser`'s CST. `json` is strict (no comments, no trailing
    commas, no JSON5 forms), so `.claude.json` and Antigravity's file are never written into a form
    the agent rejects. `jsonc` allows comments and trailing commas; OpenCode's `opencode.jsonc` is
    supported, not refused. Inserted values follow the file's indent, newline kind, and
    trailing-comma style. A UTF-8 BOM survives. A TOML edit keeps CRLF line endings and a missing
    final newline (toml_edit itself writes LF); a TOML file that mixes CRLF and LF is refused
    (`Error::MixedNewlines`) rather than rewritten.
  - `edit` parses the whole server map first. An edit that sets nothing and removes no existing
    name returns the input bytes unchanged. A comment attached to a removed owned server goes with
    it. Missing parent tables are created only to set a server.
  - A value is read only when it converts to JSON exactly, so `parse` never retypes. These are
    errors, never a silent rewrite: a map location (or an ancestor) holding anything but a table;
    a duplicate key anywhere on the path or inside the server map, at any depth; a JSON number
    serde_json can't hold (`1e400`, or an integer past 64 bits), which jsonc-parser would turn
    into a string or a float; a TOML datetime,
    which would read the same as a string; a non-finite TOML float; and a `null` bound for TOML.
  - Agent-written state in `~/.claude.json` (`oauthAccount`, `projects`) and `~/.codex/config.toml`
    (`projects`, `tui`) is never touched.
  - An existing server name that loadout doesn't own is `blocked`, not overwritten (U4 decides;
    `mcp::edit` changes only the names it is given).
  - Muse needs `"schema_version": 1`: `edit` inserts it first when it sets a server. It refuses a
    file that declares any other version for every edit, removal included. Antigravity takes `serverUrl` and rejects SSE (the schema has no SSE
    transport). Pi treats `-` and `_` in names as the same character, so `edit` refuses a set name
    that collides with another kept name (`Error::Collision`).
  - **Entry fixtures** (`mcp::entry`), from the inventory:

    | Shape | stdio | remote |
    | --- | --- | --- |
    | `claude-json` | `type = "stdio"`, `command`, `args`, `env` | `type = "http"`, `url`, `headers` |
    | `codex-toml` | `command`, `args`, `env` (literals), `env_vars` | `url`, `bearer_token_env_var`, `http_headers` (literals) |
    | `cursor-json`, `pi-json` | `command`, `args`, `env` | `url`, `headers` |
    | `grok-toml` | `command`, `args`, `env` | `url`, `headers` |
    | `opencode-json` | `type = "local"`, `command = [command, ...args]`, `environment` | `type = "remote"`, `url`, `headers` |
    | `muse-json` | `transport = "stdio"`, `command`, `args`, `env` | `transport = "streamable_http"`, `url`, `headers` |
    | `antigravity-json` | `command`, `args`, `env` | `serverUrl`, `headers` |

    JSON shapes always write their `args` (where the shape has it), env, and headers fields, even
    when empty, as the fixtures do. TOML shapes leave out an empty table or list other than `args`. An env reference renders as
    `prefix` plus `${VAR}`, `${env:VAR}`, or `{env:VAR}`. With `none` (Antigravity, or a custom
    agent), any reference is `Unsupported::NoEnvSyntax` naming the field; literal values still work.
  - Codex's `codex-env-vars` passes a variable through only under its own name (`env_vars`), and
    takes a bearer token only as `bearer_token_env_var`. So a stdio `env = { K = { env = "OTHER" } }`
    whose key differs from the variable, or carries a prefix, has no Codex form. The same holds for a
    header other than `Authorization` (any case) with prefix `Bearer `. U6 returns
    `mcp::Unsupported` for these, and the plan shows an `unsupported` action for Codex; nothing is
    renamed or resolved. `codex-env-vars` with any shape but `codex-toml` is also `Unsupported`.
  - **Transaction boundary** (agreed with U3). `mcp` works on bytes; `state` owns the read, the
    re-observe, and the atomic rename, and imports `mcp`, never the reverse. Before reading a shared
    config, the caller runs `reserved::check_path(.., KeyMerge { map, format, shape })`;
    `mcp::read` is the guarded path form that does so before any open, and refuses a FIFO or a
    folder. For Grok's `config.toml`, `mcp::lock` runs the same check, then takes Grok's exclusive
    `flock` on `<grok home>/.config-init.lock`, with Grok's own policy (create the dir and file,
    retry 50 times 20 ms). Grok's config is recognized by identity, as `check_path` recognizes it:
    the given path and its parent-normalized form are compared with both forms of
    `<GROK_HOME or ~/.grok>/config.toml`. So a file alias, a folder alias, an override home, and
    the physical file behind a linked config all take the same lock in the owning home. A path
    that can't be resolved is an error, not `None`. It refuses a linked or special lock file. The caller holds the guard
    across read, re-observe, and rename. The lock creates a file, so it is taken only at apply time,
    never at plan.
- **Custom agents** get exactly the declared paths, their declared `format`, `map`, `shape`, and
  `env_syntax`, and copy-only skills.
- **Never touched**: OS keychains, every path in the reserved table below, and T3's private profiles
  and shadow homes.

### Reserved destinations (foundation, `src/reserved.rs`)

One table, `reserved::TABLE`, built from the agent-inventory no-touch evidence. No real auth file
was ever opened to build it. Each entry is an anchor plus a relative path, and has one of four
kinds:

| Kind | Entries (default anchor shown) | Allowed use |
| --- | --- | --- |
| `auth` | `~/.claude/.credentials.json`, `~/.claude/mcp-needs-auth-cache.json`; `~/.codex/{auth.json,.credentials.json,mcp-oauth-locks}`; `~/.cursor/auth.json`, `~/.config/cursor/auth.json`, `~/AppData/Roaming/Cursor/auth.json`; `~/.grok/{auth.json,mcp_credentials.json}`; `<xdg_data>/opencode` (the auth file and credential DB); `~/.pi/agent/{auth.json,mcp-auth.json}`; `<xdg_config>/muse/{auth.json,trust.json}`; `<gemini_home>/antigravity/mcp_oauth_tokens.json` | none: never read, backed up, copied, or written |
| `auth` (non-agent, well-known) | all of `~/.ssh` (keys and config, conservatively); `<xdg_config>/gh/hosts.yml`, `~/AppData/Roaming/GitHub CLI/hosts.yml`; `~/.git-credentials`, `<xdg_config>/git/credentials`; `~/.netrc`, `~/_netrc`; `~/.aws/credentials` | none |
| `shared-config` | `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`), `~/.codex/config.toml`, `~/.cursor/mcp.json`, `~/.grok/config.toml`, `<xdg_config>/opencode/opencode.json[c]`, `~/.pi/agent/mcp.json`, `<xdg_config>/muse/settings.json`, `<gemini_home>/config/mcp_config.json` | U6 key-merge of owned MCP server names only |
| `loadout-state` | the state dir (`Context.state`; static defaults for all three OSes), `<xdg_config>/loadout` | none: only U3, U8, and U9 manage state, the clone, and `machine.toml` |
| `private` | `~/.claude/backups`, `~/.grok/.config-init.lock`, `<gemini_home>/antigravity-cli/mcp` (generated), `~/.t3` (T3 settings with provider secrets, private profiles) | none |

The standing rule against copying credentials covers more than agents, so the table also lists
these well-known credential files. The list is finite. It does not discover arbitrary credential
files, it has no user override to back up a known store, and no file contents are ever inspected.
Reserving all of `~/.ssh` is a deliberate false positive: v1 can't manage `~/.ssh/config`.
Overrides such as `AWS_SHARED_CREDENTIALS_FILE` or `GH_CONFIG_DIR` are not modelled; their default
locations are protected.

**Anchors.** `Home`, `ClaudeRoot`/`ClaudeDir` (`CLAUDE_CONFIG_DIR`), `CodexHome` (`CODEX_HOME`),
`GrokHome` (`GROK_HOME`), `PiDir` (`PI_CODING_AGENT_DIR`), `GeminiHome` (`GEMINI_HOME`, default `~/.gemini`), `XdgConfig`
(`Context.xdg_config`), `XdgData` (`Context.xdg_data`), and `State` (`Context.state`). An
overridable anchor protects both its override root and its default root. They expand through `Context`, so isolation rules apply.

**Uses:**

- `Write`: replace a file or tree. Blocked when the target equals a reserved entry, sits inside one,
  or is an ancestor of one. Replacing `~/.codex` would replace `auth.json`.
- `WriteInto`: write children into a dir, as with a skills dir. Blocked only when the dir equals or
  sits inside an entry, so `~/.claude/skills` beside `~/.claude/.credentials.json` is fine.
- `KeyMerge(Merge { map, format, shape })`: U6 MCP merge. Allowed only when the target **equals** a
  `shared-config` entry **and** the declared map, format, and shape exactly equal that entry's known
  MCP map:

  | Shared config | Map | Format | Shape |
  | --- | --- | --- | --- |
  | `.claude.json` | `/mcpServers` | json | `claude-json` |
  | Codex `config.toml` | `/mcp_servers` | toml | `codex-toml` |
  | `~/.cursor/mcp.json` | `/mcpServers` | json | `cursor-json` |
  | Grok `config.toml` | `/mcp_servers` | toml | `grok-toml` |
  | OpenCode `opencode.json` / `opencode.jsonc` | `/mcp` | json / jsonc | `opencode-json` |
  | Pi `mcp.json` | `/mcpServers` | json | `pi-json` |
  | Muse `settings.json` | `/mcp_servers` | json | `muse-json` |
  | Antigravity `mcp_config.json` | `/mcpServers` | json | `antigravity-json` |

  `KeyMerge` is blocked in every other reserved kind (`auth`, `loadout-state`, `private`, and the
  active checkout), whatever the map. A path alone never authorizes a merge. `/oauthAccount` or `/projects` in `~/.claude.json`, or
  `/projects` in Codex `config.toml`, is blocked. A custom MCP file that isn't reserved keeps any
  valid map, subject to the pointer, format, and shape rules in section 3.

Comparison is ASCII case-insensitive, so `~/.Claude/.credentials.json` is blocked on every OS.

**Enforcement:**

- **Validate (foundation, static).** `reserved::check_literal` checks every literal `files` target
  (`Write`) and every custom-agent path: skills `WriteInto`, instructions `Write`, mcp `KeyMerge`.
  It checks `{config}/` under all three OS meanings. A conflict is an error naming the reserved path
  and its kind.
- **U1, U4, and U5 (runtime, frozen).** Before any read of a target, any backup, and any write, each
  expanded target goes through `reserved::check_path(context, loadout, path, use)`, called once.
  That one API owns normalization, and callers never canonicalize for it.
  - **The active loadout checkout is reserved.** `loadout` is the root the run reads: the default
    `<state>/loadout` clone, or the `--loadout DIR` checkout. It is a required argument and is
    treated as `loadout-state`. No target may equal it, sit inside it, or (for `Write`) be its
    ancestor, through any alias. U4 passes it before observing, backing up, or writing anything, so
    the one-way rule (section 5) holds for any checkout.
  - **Both sides are normalized.** The target is compared in its lexical and its normalized form.
    Every reserved entry is compared at every root it can live under: the override root (such as
    `CODEX_HOME`) and the default home root, `xdg_config` and `~/.config`, and `Context.state` and
    the OS default state dir. Each reserved entry is also compared in lexical and normalized form.
    So an alias of a symlinked protected root (`~/alias/codex/auth.json` where
    `CODEX_HOME -> real/codex`) or of a protected file that is itself a link
    (`~/.grok/auth.json -> vault/token`) is caught from either side.
  - **`reserved::normalize`** resolves the path one component at a time, the way the file system
    would. Each existing component is canonicalized, which resolves links. A link whose target is
    missing (a dangling link) is followed through `read_link`, up to 40 links in total, and its
    target's components replace it. A `..` is applied only after the components before it are
    resolved, so `jump/../x`, with `jump` a link, means the parent of `jump`'s target, never a text
    cancellation. A missing tail with no `..` is kept, so future descendants are probed. When a
    `..` follows a missing component, the destination can't be known, and the result is an error,
    so `Unverifiable`. Inputs must be absolute without `.` or `..`. It only stats and reads links;
    it never opens file contents.
  - **Path forms.** Comparison is per component and ASCII case-insensitive. Windows prefixes are
    folded, so `std::fs::canonicalize`'s `\\?\C:` equals `C:`, and `\\?\UNC\srv\share` equals
    `\\srv\share`. A junction or link on Windows therefore resolves to the same entry.
  - **Failing closed.** Any metadata error other than not-found, on either side, returns
    `Conflict::Unverifiable`, which callers treat like a conflict.
  - **Results.** A conflict is an `unsupported` action, never a write, and the reserved file is
    never opened, hashed, or backed up. U5 checks each `<skills dir>/<skill id>` with `Write`.
  - Static validate keeps its lexical default check (`check_literal`). Sibling paths stay allowed in
    both.
  - **Sources** (U5, `reserved::check_source(context, loadout, path)`). A loadout source that the
    caller already confined inside the canonical checkout (regular file, outside Git metadata) is
    checked against **every** table entry, at every root and in both forms. There is no first
    match, so table order can't hide a later entry. An `auth` entry that equals or holds the
    source always refuses it, wherever the checkout lives, including through an override root
    placed inside the checkout and inside a checkout kept in `~/.ssh`. Any other entry that equals
    or holds the source refuses it unless that same entry also holds the canonical checkout. A user
    may keep a checkout under `~/.t3/worktrees` or use the default `<state>/loadout` clone, but
    a reserved file inside a larger checkout (`~/.t3/settings.json`, `.claude.json`, or the
    state dir from a checkout at `~`) is still refused. The checkout itself is not a reservation
    here. A path or anchor that can't be resolved refuses (`Unverifiable`). Destination checks are
    unchanged: they still use `check_path`. `check_source_file(context, loadout, reached, real)`
    runs it on both identities of one file, and is what the guarded load (section 3) and agents
    sources both call.
  - **U6 applies the same rule.** Every MCP write calls `check_path` with
    `Use::KeyMerge(Merge { map, format, shape })`, for built-in adapters and custom agents alike, on
    expanded, canonical, and override-alias paths. A shared config reached through an alias still
    accepts only its own map. Validate already checks custom declarations statically.
- An unowned shared config is never adopted or replaced as a whole file. Only U6's owned keys change.
- **Doctor diagnostics** (built, U5, `agents::checks`): only stats and folder-name listings, never
  a file's contents, never a write, and never T3's settings.
  - `agent overrides`: which of `CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`,
    `PI_CODING_AGENT_DIR`, `GEMINI_HOME`, and `XDG_CONFIG_HOME` are set, or that isolation ignores them.
  - `agent <id>` for each built-in agent: its skills, instructions, and MCP destinations on this
    machine, unsupported kinds with their reason, its T3 pickup (sync and T3 pickup are separate
    claims), inferred Windows paths, and its evidence. T3 pickup per driver: Codex, Claude, Grok,
    OpenCode, Pi, and Muse read the native paths (Codex and Claude only for instances without a
    custom home); Cursor is inferred; Antigravity is partial, because T3 gives it a private
    `GEMINI_HOME` with only the skill folders linked in, so user MCP and instructions don't reach
    T3 sessions. A custom agent's pickup is unverified.
  - Warnings: `codex override` when `AGENTS.override.md` shadows `AGENTS.md`; `skill duplicates`
    when an agent loads one skill name from two folders it reads (for example `~/.claude/skills`
    and `~/.agents/skills` for Cursor, OpenCode, and Muse); `grok instructions` when Grok loads
    both its `AGENTS.md` and `~/.claude/CLAUDE.md`; `opencode config` when both OpenCode files
    exist.

## 7. Programs (built, U7)

- **Selection.** Loadout uses the first manager the item declares that is available on the machine,
  in this OS order:

  | OS | Order |
  | --- | --- |
  | macOS | `brew`, `cask`, then the language managers |
  | Linux | `apt`, `dnf`, `pacman`, `zypper`, `flatpak`, `snap`, `brew`, then the language managers |
  | Windows | `winget`, `scoop`, `choco`, then the language managers |

  The language managers come in the order `cargo`, `uv`, `pipx`, `npm`, `pnpm`, `bun`, `go`. A
  program with no available manager is `unsupported`. U7 returns `Vec<programs::Step>`, and
  `plan::build` turns each step into an `install` or `unchanged` action.
- **Detection.** Each manager's own installed-query decides, and only missing programs are
  installed. **Default**: no version pinning and no upgrades in v1.
- **Per user first.** Loadout prefers `flatpak --user`, non-global Scoop, and Homebrew. `apt`, `dnf`,
  `pacman`, `zypper`, `snap`, `choco`, and machine-scoped winget installers are `blocked` unless
  `apply --elevated` is passed. With it, they run through `sudo` on Unix, or the installer raises
  UAC on Windows. The background service never elevates.
- **Isolation.** Under `LOADOUT_HOME` (`Context.isolated`), every program step is
  `blocked("programs are not installed under LOADOUT_HOME isolation")` and no manager is run.
  Real installs are tested only in a disposable VM guest's actual `HOME` (section 4).
- No direct downloads and no install scripts in v1. Loadout never installs a package manager itself;
  doctor names what is missing.

### How U7 runs each manager (built)

A manager is available when every tool it needs is on PATH (`context::which`); `install` runs the
executable found there. Every subprocess gets its argv as an array (no shell), a null stdin, and
`LC_ALL=C`, so the not-found messages below are stable. A null stdin doesn't rule out every prompt:
with `--elevated`, `sudo` may ask for a password on the terminal, and a Windows installer may raise
UAC. `LC_ALL` doesn't change winget's language either, which follows Windows' display language.

| Manager | Needs | Installed-query (one per program) | Not installed when | Install |
| --- | --- | --- | --- | --- |
| `brew` | brew | `brew info --json=v2 --formula P` | the one entry named `P` (name, full name, alias, or old name) has `installed: []` | `brew install --formula P`, `HOMEBREW_NO_INSTALL_UPGRADE=1` |
| `cask` | brew | `brew info --json=v2 --cask P` | the one entry for `P` (token, full token, or old token) has `installed: null` | `brew install --cask P`, `HOMEBREW_NO_INSTALL_UPGRADE=1` |
| `apt` | apt-get, dpkg-query | `dpkg-query -W -f '${db:Status-Status} ' P` | exit 1, or no status is `installed` | `apt-get install -y --no-upgrade P`, `DEBIAN_FRONTEND=noninteractive` |
| `dnf` | dnf, rpm | `rpm -q P` | exit 1 with "is not installed" | `dnf install -y P` |
| `pacman` | pacman | `pacman -Q P` | exit 1 with "was not found" | `pacman -S --needed --noconfirm P` |
| `zypper` | zypper | `zypper --non-interactive --no-refresh search --installed-only --match-exact --type package P` | exit 104 | `zypper --non-interactive install P` |
| `flatpak` | flatpak | `flatpak info P` (any installation) | exit 1 with "not installed" | `flatpak install --user --noninteractive P` |
| `snap` | snap | `snap list P` | exit 1 with "no matching snaps installed" | `snap install P` |
| `winget` | winget | `winget list --id P --exact --source winget` | `0x8A150014` | `winget install --id P --exact --source winget --silent --accept-package-agreements --accept-source-agreements --disable-interactivity --no-upgrade --scope user\|machine` |
| `scoop` | scoop | `scoop prefix APP` (`APP` is `P` after any `bucket/`) | exit 1 with "Could not find app path" | `scoop install P` |
| `choco` | choco (2.x) | `choco list --exact P --limit-output` | no `P\|version` line, or exit 2 | `choco install P -y --no-progress`; 1641 and 3010 are success |
| `npm` | npm | `npm ls --global --depth=0 --json P` (and `npm prefix --global` after an exit 254) | the root `{"name": …}` has no `dependencies.P`, or it is `missing: true` (exit 0 or 1); or exit 254 with `ENOENT … lstat '<path>'` where `<path>` is exactly the prefix `npm prefix --global` prints | `npm install --global P` |
| `pnpm` | pnpm | `pnpm ls --global --depth=0 --json P` | no project in the list `[{"path": …, "dependencies": {…}}]` has `P` (exit 0) | `pnpm add --global P` |
| `bun` | bun | `bun pm ls --global` | no `── P@…` line | `bun add --global P` |
| `cargo` | cargo | `cargo install --list` | no `P v…:` line | `cargo install P` |
| `uv` | uv | `uv tool list` | no `P v…` line (PEP 503 names) | `uv tool install P` |
| `pipx` | pipx | `pipx list --json` | no venv's `metadata.main_package.package` is `P` (PEP 503 names) | `pipx install P` |
| `go` | go | `go env GOBIN GOPATH`, then `go version -m BIN` | `BIN` (from `GOBIN` or `GOPATH/bin`) is absent | `go install P@latest` |

- **Installed needs positive evidence.** Every row's other outcomes (an unexpected exit code,
  unreadable JSON, a missing tool) are `programs::Error::Detect`, so a broken or locked manager is
  never read as "installed", and `steps` fails before any write. A Go binary at `BIN` built from a
  different package is an error, never overwritten.
- **JSON answers must match the manager's real schema before either verdict.** npm's root is an
  object with a string `name`; pnpm's is a non-empty list of objects with a string `path`; a
  `dependencies` table is an object whose entry for `P` is an object with a string `version` (or
  `missing: true`); every pipx venv names its main package; a brew entry must answer for `P`, and
  its `installed` is a list of `{ "version": … }` records (formula) or a version string or `null`
  (cask). npm listing `P` while exiting 1 means npm found problems in the tree, and is an error. A
  configured global prefix that doesn't exist yet makes npm exit 254 with
  `{"error":{"code":"ENOENT","summary":"ENOENT: no such file or directory, lstat '<path>'"}}`
  (sampled on npm 9.2). That reads as missing only when the summary has exactly that form, the
  path holds no quote, and the same npm's `npm prefix --global` (under the same query deadline and
  cap) prints that one absolute path. Loadout creates no directory to find out. Any other error
  code, summary, exit code, or prefix answer, including a path below the prefix, is
  `Error::Detect`, with a message naming the path and npm's prefix. There is no general rule that
  `ENOENT` means missing.
  Anything else, such as `null`, a non-object entry, or an empty pnpm list, is `Error::Detect`, so a
  malformed answer never authorizes an install. The npm and pnpm empty shapes and the npm installed
  shape were sampled from real managers. The pnpm installed entry, the pipx entry, and brew's
  records follow the managers' documented output and still need a real sample.
- **Package ids are passed as one exact argument.** `steps` refuses, before any subprocess, an id
  that is empty, starts with `-`, has a character outside letters, digits, and `@ / . _ + : ~ -`,
  or pins a version (`name@1.2` for npm, pnpm, bun; any `@` for cargo, uv, pipx, go). Go is the
  one manager given `@latest`, because `go install` requires a version.
- **Scope.** `apt`, `dnf`, `pacman`, `zypper`, `snap`, and `choco` are system scope; the other
  non-winget managers are user scope. Winget's scope is per package, read from
  `winget show --id P --exact --source winget --scope user|machine`, whose answer is the line under
  its `Installer:` heading (winget 1.6 exits 0 even when it prints "No applicable installer
  found"; `0x8A150010` means the same):
  - The user scope applies only when its installer is `portable` or `portable (zip)`, a portable
    package bare or nested in a zip, which installs into the user's WinGet Packages folder (seen
    natively with `BurntSushi.ripgrep.MSVC`). Other types, other installers nested in a zip, MSIX,
    and APPX may install machine-wide or raise UAC whatever scope is asked for, so they don't
    count as per user until native evidence shows otherwise.
  - Otherwise the machine scope is asked separately. Only an installer winget shows as applicable
    there makes the package system scope, which needs `--elevated`.
  - With neither, the step is `Unsupported` with the reason. Any other text, including an
    `Installer Type:` line with no type, is `Error::Detect`, never a guessed scope, and comes
    before any install or elevation.

  The `winget` field names an id in the `winget` community source. Every winget call passes
  `--source winget`, so a broken `msstore` source (as in the Windows test guest) can't stop an
  install.
- **Elevation.** `install(step, false)` refuses a system-scope step with `Error::NeedsElevation`.
  With `elevated`, Unix runs `sudo -- [env K=V] TOOL …` (or the tool directly as root), and an
  absent `sudo` is `Error::NoSudo`. Windows runs the tool directly. User-scope managers are never
  elevated.
- **Checked again at install.** `install` re-checks the step against the machine as it is then:
  - The package id rule.
  - The manager's static scope.
  - The executable PATH resolves the manager to (a different path, or none, is `Error::Changed`).
  - The manager's own answer, the same installed-query and scope as `steps`.

  A package installed by then is `Installed::Already`, and nothing runs, so a stale step, a second
  program naming the same package, or an earlier install's dependency never reinstalls or upgrades
  it. A changed scope or a Go collision is an error before any installer runs, and this re-check
  comes before the elevation check. Two limits remain:
  - Another process can install the package between that last look and the installer starting.
    winget (`--no-upgrade`), Homebrew, apt (`--no-upgrade`), flatpak, Scoop, Chocolatey, uv, pipx,
    and snap then skip it themselves; npm, pnpm, bun, go, cargo, dnf, pacman, and zypper may
    upgrade it.
  - An equal PATH answer means the same path, not the same file, so a manager binary replaced in
    place isn't detected.

  If a winget too old for `--no-upgrade` rejects it, the install fails with winget's message.
  Loadout never retries without it.
- **Producer-only steps.** `Package` and `Missing` have private fields, and only `steps` creates
  them, so a caller (U4 included) can read a step but never fabricate a trusted one.
- **Verified.** After the manager exits with success, `install` runs the installed-query again; a
  success that left the program missing is `Error::NotInstalled`.
- **Bounded queries.** Every installed-query, scope query, and doctor query runs in its own process
  group (Unix) or job object (Windows, through `process-wrap`). `QUERY_DEADLINE` (10 minutes)
  bounds waiting for the query's own process. A query's stdout and stderr are read in full up to
  `QUERY_OUTPUT_CAP` (8 MiB each). A larger answer stops the query early, and is an error, never
  parsed truncated.
- **Every query is settled, success included.** However a query ends (it exits, overflows, times
  out, can't be waited on, or can't even start), loadout stops everything it started and
  confirms the stop. One shared `QUERY_CLEANUP_GRACE` (5 seconds) bounds all of it together, with
  no unbounded wait. If the stop can't be confirmed, the result is `QueryError::Cleanup`, which
  keeps what ended the query as its cause, and the detection is `Error::Detect`. Only then are
  the output buffers read, each under its own lock: an overflow first, then a read error, then
  the bytes. Exactly `QUERY_OUTPUT_CAP` bytes is complete; one byte more is always an error.
- **One setup deadline.** `QUERY_CLEANUP_GRACE` from just before anything is spawned bounds the
  cleanup of every setup failure, on both OSes, with no renewed budget for a later stage. A
  query's setup is its spawn, both reader threads, and on Windows its resume. A reader thread
  that can't start is a setup failure, returned as the primary `Read` error at once: the query
  is stopped (and on Windows never resumed), not watched until its deadline. Only a query that
  fully started moves on to `QUERY_DEADLINE`, and then to its own settle limit, set when its
  wait ends. The setup deadline never limits a query that started.
- **Unix: an anchor holds the process group's id.** Every query gets a loadout-owned anchor,
  `/bin/sh -c 'read -r line'`. It is the leader of a fresh process group and blocks on a pipe
  only loadout writes, with no access to loadout's own stdio; if loadout dies, the pipe closes and
  the anchor exits. The query is a plain `std::process::Child` that joins the anchor's group
  (`CommandExt::process_group`).

  While loadout hasn't reaped the anchor, alive or as a zombie, the group id can't be reused, so:
  - **Phase A (anchor unreaped)**, the only time a destructive signal is sent:
    1. `killpg(SIGKILL)` to the group. The anchor is alive and loadout's own, so **any** error,
       `ESRCH` and `EPERM` included, is a cleanup failure.
    2. Reap the query's own pid.
    3. Wait for both readers' EOF.
    4. `killpg(SIGKILL)` again, for processes forked during the first kill. The anchor may be a
       zombie by now, and on macOS a group whose members are all zombies answers `EPERM` (XNU's
       `killpg1` skips zombies, then returns `EPERM`). So `EPERM` here is **pending**, never
       success. Any other error is a failure.
    5. Kill and reap the anchor by its own pid; an anchor that ended any other way is an error.
  - **Phase B:** signal 0 only, polled within the same cleanup limit. `Ok` (members remain) and
    `EPERM` (zombies awaiting their reaper, or members loadout can't signal) mean wait. Only
    `ESRCH`, the group freed because every member has been reaped, means the group is gone, and
    only that clears a pending second-kill `EPERM`. If the limit runs out first, cleanup fails and
    says which. An earlier, unrelated failure is never cleared. A reused id can only make cleanup
    fail, never kill another group.

  Which macOS call returns `EPERM` in practice, and the zombie state behind it, come from XNU's
  source and the CI failure pattern. They still need native confirmation.

  Only each child's own pid is ever waited on, never the group, so nothing reaps the anchor
  early. A query that fails to start, after its anchor started, still has its anchor killed and
  reaped within the same bound.

  **Unix doesn't use `process-wrap`.** Its group `try_wait` reaps every child of ours in the
  group, and `ProcessGroup::attach_to` records the child's own pid as the group id; both are
  incompatible with an anchor.

  A system without `/bin/sh` can't run queries: the anchor's spawn fails, and detection is an
  error. Loadout doesn't claim every Unix has `/bin/sh`.
- **Windows: a job object.** A job is addressed through loadout's own handle, so there is no id
  to reuse. No safe API reports that a job has no processes left, so the job's
  `TerminateJobObject` (checked), the query's exit, and both readers' EOF are the confirmation.
  A descendant whose output is redirected is covered only by `TerminateJobObject`'s documented
  semantics, and `process-wrap`'s first-message job wait is not used as proof.

  **The query runs only once loadout owns its job.** Loadout passes `CREATE_SUSPENDED` itself
  (`process-wrap`'s `CreationFlags`), so `process-wrap` assigns the job and **never resumes**
  the query. Its own resume can fail after a thread already ran, and then discards the job's
  termination result. Setup, in order:
  1. Loadout spawns the `std::process::Child` itself (`spawn_with_child`) and keeps it
     (`Arc<Mutex<Child>>`) before anything fallible, handle duplication included. The innermost
     wrapper `process-wrap` sees shares that child.
  2. If anything fails before loadout holds the job (handle duplication, `process-wrap`'s
     no-handle branch, creating or assigning the job), the query never ran, so it started
     nothing. Loadout kills its one process (checked) and reaps it, through its own retained
     handle (no job is assumed yet). Only a positively exited process is not killed: a first
     observation that fails still leads to the kill and the reap, and the observation, kill
     and reap errors are all kept, in that order. The setup deadline bounds
     `process-wrap`'s wait on that process (the innermost wrapper's `wait` polls only up to it)
     and loadout's own disposal together. The result is the spawn error, or `Cleanup` with the
     spawn error as its cause.
  3. Once `process-wrap` returns the job's owner, the readers start. If one can't, the query is
     not resumed, and the whole job is stopped within the setup deadline, with the `Read` error
     as the cause.
  4. If the setup deadline has already passed, the query is **not resumed**. Loadout still
     terminates the job at once (checked) and confirms what it can within the spent limit. The
     result is the setup error, or `Cleanup` with it as the cause; never success.
  5. Otherwise `loadout_winproc::prepare` finds the child's threads in one snapshot, requires
     exactly one, opens it, and checks it still belongs to the child. Then
     `Prepared::resume_before` reads the clock once more, after all that preparation, as the
     last step before `ResumeThread`: if the setup deadline has passed, `ResumeThread` is not
     called, and the result is a setup timeout. A previous suspend count other than 1 is an
     error. Reading the clock and resuming aren't one atomic step, and no real-time guarantee
     is made: the thread can be preempted between them. So after the resume returns, loadout
     checks the deadline again, and setup succeeds only if it still hasn't passed. Otherwise it
     is a setup timeout, handled as in step 6.
  6. **Any** resume error, or a resume that returned after the deadline, is treated as "the
     query may have run": there is no `watch`, and the whole job is stopped within the **same**
     setup deadline, with that error as the cause. Only a successful resume that returned in
     time ends setup and clears the deadline. From then on, the
     innermost wrapper's blocking `wait` refuses rather than block without a bound; loadout
     itself only ever polls.

  A real `TerminateJobObject` failure can't be constructed in a test: only that its error is
  kept is unit-covered.
- **A process that leaves the group** (`setsid`) while holding the output makes cleanup fail.
  The reader blocked on it stays blocked until that process exits; that is a resource limit, not
  success.
- **Installs have no deadline, and U7 can't cancel them.** An install runs in loadout's own
  process group, so `sudo` can prompt and Ctrl-C reaches it. Errors keep the last 64 KiB of each
  output stream. After the installer exits, loadout reads its output for at most 30 more seconds.
  That drain only stops *reading*: it cancels nothing, and a process the installer started keeps
  running. `programs::install` has no timeout and no cancel handle. A hung install, a hung
  descendant, or a UAC or `sudo` prompt blocks it until the process tree ends or something outside
  loadout kills it.
- **What U8 must supply.** Before automatic updates can be claimed to finish safely in the
  background, the supervisor that starts `update --background` must bound the whole run and kill
  every descendant on all three OSes:
  - Linux: the systemd unit's time limit (`RuntimeMaxSec`), with its control-group kill.
  - Windows: Task Scheduler's `ExecutionTimeLimit` covers only the task's own process, so
    `update --background` itself needs a job object for its descendants.
  - macOS: launchd has no runtime limit, so `update --background` needs its own watchdog, which
    kills its process group.

  Until U8 proves that natively, no claim is made that a background run completes or cancels
  cleanly. Native negative tests of hung installs and UAC use an external finite watchdog.
- **Doctor** (`programs::checks`) adds three checks:
  - `programs`: this OS's managers, grouped as per user, with `--elevated`, per package, and not on
    PATH; a warning when none is available.
  - `elevation`, on macOS and Linux when a system-scope manager is present: sudo found, root, or a
    warning.
  - `flatpak`: its per-user remotes, or a warning when there is none or its query fails or times
    out.

  Under isolation it only looks on PATH and runs no manager.

## 8. Updates (frozen, U8 + U9)

Automatic updates are required, so they are on by default.

- `loadout init <url>` (U9, source) clones the loadout into `<state>/loadout` with the system `git`,
  writes `machine.toml`, plans, applies, and installs the background service unless
  `[update] auto = false`. A private loadout works through the user's existing Git credentials or
  SSH keys. Loadout stores no token. As built:
  - Everything that can refuse runs before a write: an existing clone, or a `--name` that differs
    from a saved name (a saved name never changes).
  - It holds the state lock from `sync::clone(url, context, &lock)` through
    `apply::run_under_lock(context, &sync::checkout(context), lock, …)`.
  - The machine name is `--name`, else the saved name, else in a terminal the user's pick (it lists
    the loadout's `[machines]`, defaulting to the hostname's name), else the hostname's. A name the
    loadout doesn't list is noted: it gets the global and OS layers only.
  - `machine::save` writes `name` and `remote` atomically (0600 on Unix), and refuses a remote with a
    password.
  - The service is installed with `sync::service::install(context, interval)` unless `--no-service`,
    the resolved `[update] auto` is false, or the loadout is invalid. Its warnings are printed, and
    an unhealthy refusal exits 4.
  - `init` does not self-update: the install scripts that run it have just fetched the latest
    release.
- `loadout update` fetches and fast-forwards the clone, then plans and applies. If the clone has
  local changes or the remote doesn't fast-forward, it is left untouched and reported (exit 4).
- `loadout service install|uninstall|status` manages a per-user job:

  | OS | Job |
  | --- | --- |
  | macOS | launchd agent `~/Library/LaunchAgents/dev.loadout.update.plist` |
  | Linux | systemd user timer `{config}/systemd/user/loadout-update.{service,timer}` |
  | Windows | Scheduled Task for the current user, no admin |

  The job runs `loadout update --background` every `interval_minutes`, with no prompts, no
  elevation, and no `--force`. Drift and blocked actions are recorded for `status`. A Linux machine
  without a systemd user session gets a doctor warning, and `service install` exits 4.
- **Self-update** (U9, source) runs when `[update] cli = true` and the install script installed the binary.
  `update` reads the latest public GitHub release with no token, verifies the asset's `.sha256`, and
  replaces itself. A binary from `cargo install` or a package manager is never replaced; doctor
  reports how the CLI was installed. As built in `src/release/`:
  - **Calling it.** After its apply, `update` calls `release::latest(context)`, then
    `release::install(&release)` on `Some`.
  - **Newer only.** `Some` means a release strictly newer than the running CLI; `None` means none
    is offered. The release tag comes from the `releases/latest` redirect.
  - **Skips.** `Error::skipped()` covers LOADOUT_HOME isolation, a binary without a matching
    install record, no published release, and another install or self-update holding the install
    folder's lock (`Busy`). Those are reported, not failures. Every other error fails the
    self-update.
  - **Managed binaries.** A binary counts as managed only when its install record
    (`docs/releasing.md`, schema 1 or 2) parses exactly and binds this platform's installer, the
    build's target, the canonical running executable, its current SHA-256 and the running version.
    Schema 2 also binds the Windows background launcher.
  - **Replacing it.**
    - The archive and sidecar come over HTTPS only (max 5 redirects, size limits). The sidecar must
      be the exact line, and the hash must match.
    - The expected binaries are read from memory after name, type and size checks. Tar entries are
      read raw: every header, GNU and PAX metadata included, counts against the limits before any
      content is read, and only USTAR folders and regular files are accepted.
    - Then it takes the install folder's lock, `.loadout.install.lock`, which install.sh and
      install.ps1 take too (flock on Unix, an unshared open on Windows), without waiting. Under it,
      each binary is staged with `create_new` beside its destination and must print exactly
      `loadout <version>` or `loadout-background <version>`.
    - Still under the lock, the binaries and record must be exactly the ones `latest` checked, so a
      release found before another install or self-update finished never replaces that install.
      Then the binaries are published and the record last. On Windows, running executables are
      moved aside and every completed move is undone if a later one fails (each one attempted,
      and any that can't be named in the error). The lock is held until
      publication, any undo and this run's own cleanup are done.
    - After the record, it deletes the executables it moved aside. One still running is listed
      with its sha256 in `.loadout.install.asides`, and a later completed self-update deletes
      exactly that name with exactly that hash. No file is deleted by its name pattern alone.
    - A writer that doesn't take the lock (a hand copy, another tool, or an install script from
      before the lock) is not serialized with this, so its change can still race one publication.
    - A Windows schema-1 install is upgraded to schema 2 with the launcher.
    - Interrupted publication is not atomic across files and fails closed: a binary that no longer
      matches its record is unmanaged until it is reinstalled.

## 9. Bootstrap and release (frozen, U9 + release CI)

- Release CI (PR #2) builds `loadout-<target>.tar.gz`, or `.zip` on Windows, each with a `.sha256`,
  for x86_64 and aarch64 Linux (musl), macOS, and Windows. The repo is public, so
  `releases/latest/download/` URLs need no token. The Windows zip holds exactly two binaries,
  `loadout.exe` and the windowless launcher `loadout-background.exe` (U8's bin target in this
  package), and installers and self-update install both under an install record of schema 2.
- `install.sh` (macOS, Linux) and `install.ps1` (Windows) are owned by U9 and live beside the release
  scripts. They download the right archive, verify it, and install to `~/.local/bin` or
  `%LOCALAPPDATA%\Programs\loadout`. Given a loadout URL, they then run `loadout init <url>`.
- The one prerequisite is `git`. The CLI never installs it; `doctor` names the per-OS install
  command.

## 10. Commands

| Command | Status | Owner |
| --- | --- | --- |
| `loadout --help`, `--version` | foundation | |
| `loadout doctor [--json]`: version, platform, home, config/state dirs, machine name, git, managers on PATH; exit 4 on a failed check; never writes | foundation | U5, U6, U7, U8, U9 add checks |
| `loadout validate [DIR] [--json]`: strict schema and static checks, then resolution of every machine on all three OSes (U1, built) and the syntax of every `.tmpl` user file (U2, built); exit 3 when invalid. Loads with `Loadout::load_guarded` and the active `Context`, and reads each template source through the same source guard (`template::check_all_guarded`) | foundation; resolution built (U1); template syntax built (U2); guarded load (U5) | |
| `loadout plan [--loadout DIR] [--json]`: what apply would change; never writes, locks, or creates the state dir; exit 3 invalid, 4 blocked or drifted | source, unexecuted | U4 |
| `loadout apply [--loadout DIR] [--force] [--elevated]`: read-only preflight, then the lock and a rebuilt plan; exit 1 on a failed action, 3 invalid, 4 when anything is left undone | source, unexecuted | U4 |
| `loadout status [--json]`: ownership records with drift, runs with interrupted ones; exit 4 when not healthy; never writes | built | U3 |
| `loadout rollback [RUN]`: undo the newest or the named run; exit 4 when later changes block it | built | U3 |
| `loadout update [--background]` | frozen | U8 (+U9 self-update) |
| `loadout service install\|uninstall\|status` | frozen | U8 |
| `loadout init <URL> [--name NAME] [--no-service]`: clone, name the machine, apply, install the service; exit 3 invalid, 4 unhealthy (apply or service) | source, unexecuted; builds with U8's `sync` | U9 |

`plan`, `apply`, and `update` read the loadout from `<state>/loadout`; `--loadout <DIR>` points them
at another checkout.

## 11. Module contracts

The signatures each unit implements. Items marked foundation already exist on the foundation branch.

```rust
// platform (foundation)
pub enum Os { Macos, Linux, Windows }
pub enum Arch { X86_64, Aarch64 }

// machine (foundation)
pub struct Facts { pub name: String, pub os: Os, pub distro: Option<String>, pub arch: Arch, pub home: PathBuf }
pub fn detect(context: &Context) -> Result<Facts, machine::Error>;
pub fn name(context: &Context) -> Result<Name, machine::Error>;

// context (foundation)
pub struct Context {
    pub os: Os, pub home: PathBuf, pub isolated: bool, pub config: PathBuf, pub xdg_config: PathBuf, pub xdg_data: PathBuf,
    pub state: PathBuf, pub agent_dirs: BTreeMap<&'static str, PathBuf>,
    pub containment: process::Containment,     // OwnGroup (foreground) unless supervised (U8)
}
impl Context {
    pub fn from_env() -> Result<Self, context::Error>;
    pub fn resolve(os: Os, var: impl Fn(&str) -> Option<OsString>, default_home: Option<PathBuf>) -> Result<Self, context::Error>;
    pub fn supervised(self, by: Arc<process::Supervised>) -> Self;   // after a successful process::handshake (U8)
    pub fn expand(&self, path: &str) -> Option<PathBuf>;       // None for unclean paths, and Windows-unsafe parts on Windows
    pub fn agent_dir(&self, var: &str) -> Option<&Path>;        // always None under isolation
    pub fn machine_file(&self) -> PathBuf;
}
pub const AGENT_DIR_VARS: [&str; 5];                            // CLAUDE_CONFIG_DIR, CODEX_HOME, GROK_HOME, PI_CODING_AGENT_DIR, GEMINI_HOME
pub fn split_base(path: &str) -> Option<(Base, &str)>;          // Base::{Home, Config}
pub fn windows_problem(rest: &str) -> Option<String>;
pub fn which(program: &str) -> Option<PathBuf>;                 // executable files only on Unix

// reserved (foundation)
pub const TABLE: &[Entry];                                      // Entry { anchor: Anchor, path, kind: Kind }
pub enum Kind { Auth, SharedConfig, LoadoutState, Private }
pub enum Use<'a> { Write, WriteInto, KeyMerge(Merge<'a>) }   // Merge { map, format, shape } as ids
// Entry { anchor, path, kind, merge: Option<Merge<'static>> }: shared-config entries carry their known MCP map
pub fn check_literal(path: &str, how: Use) -> Option<Conflict>;                 // validate, static
pub fn check_path(context: &Context, loadout: &Path, path: &Path, how: Use) -> Option<Conflict>; // U1/U4/U5; `loadout` = active checkout root; normalizes both sides itself
pub fn check_source(context: &Context, loadout: &Path, path: &Path) -> Option<Conflict>; // U5 sources: auth always; other kinds unless the same entry holds the checkout
pub fn check_source_file(context: &Context, loadout: &Path, reached: &Path, real: &Path) -> Option<Conflict>; // both identities of one file
pub fn normalize(path: &Path) -> io::Result<PathBuf>;                         // canonical existing ancestor + missing tail; stats only
pub enum Conflict { Reserved { entry: Entry, reserved: String }, Unverifiable { path: PathBuf, error: String } }

// commands::doctor (foundation): units extend `[foundation(context), <subject>::checks(context), …].concat()`

// diagnostic (foundation)
pub struct Diagnostic { pub severity: Severity, pub file: Option<PathBuf>, pub key: Option<String>, pub message: String }
pub struct Diagnostics(..);              // push, extend, has_errors, count, iter
pub struct Check { pub name: String, pub status: CheckStatus, pub detail: String }   // Pass | Info | Warn | Fail

// manifest (foundation)
impl Loadout { pub fn load(root: &Path) -> Result<(Loadout, Diagnostics /* warnings */), Diagnostics>; }   // context-free: synthetic/test use only
impl Loadout { pub fn load_guarded(root: &Path, context: &Context) -> Result<(Loadout, Diagnostics), Diagnostics>; } // every host command; source policy before each manifest/pointer open
pub type Guard<'a> = dyn Fn(&Path, &Path) -> Result<(), String> + 'a;   // (reached, canonical) -> refusal
pub fn source_guard<'a>(context: &'a Context, checkout: &'a Path) -> impl Fn(&Path, &Path) -> Result<(), String> + 'a; // the one host guard: check_source_file on both identities
pub fn git_metadata(root: &Path) -> Result<Vec<PathBuf>, String>;   // canonical root; validate's Git metadata identification (unguarded)
pub fn git_metadata_guarded(root: &Path, guard: &dyn Fn(&Path, &Path) -> Result<(), String>) -> Result<Vec<PathBuf>, String>; // guard(reached, canonical) runs before each pointer open; agents passes check_source
// Loadout { root, machines: BTreeMap<String, MachineEntry>, layers: Vec<LayerFile { scope: Scope, file, layer: Layer }> }
// Scope::{Global, Os(Os), Tag(String), Machine(String)}; layers come global, os, tags, machines, each sorted by name

// resolve (built, U1; Resolved types foundation)
pub fn resolve(loadout: &Loadout, facts: &Facts) -> Result<(Resolved, Diagnostics /* warnings */), Diagnostics>; // like Loadout::load
pub fn check_all(loadout: &Loadout) -> Diagnostics;   // every listed machine plus unlisted ones, on all three OSes; synthetic facts only
pub fn check_targets(resolved: &Resolved, context: &Context, loadout: &Path) -> Diagnostics; // runtime reserved::check_path; `loadout` = active checkout
// Resolved sources (File, Skill, Instructions) are absolute: the loadout root joined with the declared path.
// An item's `agents` is the effective list: its own list (an edit of an unset list edits the enabled agents) narrowed to the enabled set.

// template (U2, built)
pub const SUFFIX: &str;                                  // ".tmpl"
pub struct Error { pub file: PathBuf, pub line: Option<usize>, pub problem: Problem }
pub enum Problem { Syntax(String), Missing(String /* variable */), Render(String), Home(PathBuf) }
pub fn render(file: &Path, source: &str, variables: &BTreeMap<String, Variable>, facts: &Facts) -> Result<String, template::Error>;
pub fn check(file: &Path, source: &str) -> Result<(), template::Error>;   // syntax only; no variables needed
pub fn check_all_guarded(loadout: &Loadout, context: &Context) -> Diagnostics; // validate: every .tmpl user-file source; source_guard before each open
pub fn check_all(loadout: &Loadout) -> Diagnostics;                       // context-free: confined but no host policy; synthetic/test use only

// mcp (U6, built). The frozen path read/write became byte functions plus a guarded read; see section 6.
pub type Servers = BTreeMap<String, serde_json::Value>;
pub fn entry(server: &McpServer, shape: McpShape, env: EnvSyntax) -> Result<serde_json::Value, mcp::Unsupported>;
pub fn parse(bytes: &[u8], format: McpFormat, pointer: &str) -> Result<Servers, mcp::Error>;      // empty bytes = absent file
pub fn edit(bytes: &[u8], format: McpFormat, pointer: &str, shape: McpShape, set: &Servers, remove: &[String]) -> Result<Vec<u8>, mcp::Error>; // new bytes; preserves everything else
pub fn read(context: &Context, loadout: &Path, file: &Path, format: McpFormat, pointer: &str, shape: McpShape) -> Result<Servers, mcp::Error>; // check_path(KeyMerge) before any open
pub fn lock(context: &Context, loadout: &Path, file: &Path) -> Result<Option<mcp::Lock>, mcp::Error>; // Some only for Grok's config.toml; lock_id then acquire
pub fn lock_id(context: &Context, loadout: &Path, file: &Path) -> Result<Option<mcp::LockId>, mcp::Error>; // the same lookup and KeyMerge check, nothing created or taken
pub fn acquire(id: &mcp::LockId) -> Result<mcp::Lock, mcp::Error>;   // may create Grok's home and .config-init.lock; ~1 s of retries
// LockId: Clone + Eq + Ord, equal when two paths name one Grok home (lossless identity; case-folded on Windows only), and ordered by that same identity, never by the kept spelling; LockId::path() is its lock file
// plan::Target::Entry carries `shape: McpShape` (U5) so state runs the exact KeyMerge check and calls edit with it.

// agents (built, U5). `facts` was dropped (context is the one OS input) and `loadout`, the active checkout, added.
pub fn materialize(resolved: &Resolved, context: &Context, loadout: &Path) -> Result<(Vec<Desired>, Vec<Action /* Change::Unsupported */>), agents::Error>;
pub enum Error { Invalid(Diagnostics) }                          // source or collision problems; exit 3, nothing written
pub fn checks(context: &Context) -> Vec<Check>;

// programs (U7, built)
pub struct Step { pub program: String, pub state: State }       // Step::needs_elevation(): Missing with Scope::System
pub enum State { Installed(Package), Missing(Missing), Unsupported(String), Blocked(String) }
pub struct Package { .. }   // private; manager() -> Manager, id() -> &str, executable() -> &Path; only steps() makes one
pub struct Missing { .. }   // private; package() -> &Package, scope() -> Scope
pub enum Scope { User, System }
pub enum Installed { Now, Already }                             // Already: found installed on the re-check; nothing ran
pub const ISOLATED: &str;                                       // the Blocked reason under LOADOUT_HOME
pub fn steps(programs: &BTreeMap<String, Program>, facts: &Facts, context: &Context) -> Result<Vec<Step>, programs::Error>; // every step blocked when context.isolated
pub fn install(step: &Step, elevated: bool) -> Result<Installed, programs::Error>;   // Missing steps only; re-checks before, re-queries after
pub fn checks(context: &Context) -> Vec<Check>;
// plan::build: Installed -> unchanged (prior Installed); Missing -> install (prior Absent), or blocked when it
// needs elevation the run lacks; Unsupported and Blocked carry their reason. Order follows `programs` (by id).
// apply: Installed::Already -> report unchanged; Error::Changed -> the action failed and nothing ran (plan again).
// U4 gets steps only from steps() and never fabricates Package or Missing.

// state (built, U3)
impl State { pub fn load(context: &Context) -> Result<State, state::Error>; pub fn record(&self, target: &Target) -> Option<&Record>; pub fn records(&self) -> impl Iterator<Item = &Record>; }
pub struct Record { pub target: Target, pub item: ItemRef, pub sha256: String, pub run: String }
pub fn lock(context: &Context) -> Result<Lock, state::Error>;          // Error::Locked when another run holds it
impl Lock { pub fn check(&self, context: &Context) -> Result<(), state::Error>; } // read-only: the existing canonical state dir must be the locked one (aliases pass), else WrongLock; missing/unresolvable is Io, not a folder is Invalid; creates, opens and takes nothing, lock stays held
pub fn busy(context: &Context) -> Result<bool, state::Error>;           // probe only; never creates
pub fn digest(content: &Content) -> Result<Observed, state::Error>;     // desired side of the one hash definition
pub fn observe(context: &Context, loadout: &Path, target: &Target) -> Result<Observed, state::Error>; // reserved check first; disk side; an Entry by key, KeyMerge, no lock
impl Run {
    pub fn begin(context: &Context, loadout: &Path, lock: Lock, state: &State, plan: &Plan) -> Result<Run, state::Error>;
    pub fn id(&self) -> &str;
    pub fn write(&mut self, action: &Action, content: &Content) -> Result<(), state::Error>;   // create/update/adopt/replace/drifted; an Entry: one key, never replace
    pub fn remove(&mut self, action: &Action) -> Result<Removal, state::Error>;              // Removed | Released
    pub fn finish(self, state: &mut State) -> Result<(), state::Error>;
    pub fn finish_with_lock(self, state: &mut State) -> (Lock, Result<(), state::Error>); // the lock back on success and on error
}
// Run::write and Run::remove re-observe and return Error::Changed { target } when the result != prior
pub fn rollback(context: &Context, run: Option<&str>) -> Result<RolledBack, state::Error>; // RolledBack { run, restored: Vec<Target>, leftovers: Vec<PathBuf> }
pub fn audit<'a>(context: &Context, loadout: &Path, state: &'a State) -> Vec<(&'a Record, Health)>;
pub fn runs(context: &Context) -> Result<Vec<RunSummary>, state::Error>;
// U3 changes to the frozen signatures (Decisions made here): observe and Run::begin take the
// active checkout so the reserved check happens inside state before any read, backup, or write;
// Run::begin takes the Lock by value (refused with Error::WrongLock unless it holds the same
// canonical state dir) and the pre-run State for the journaled records; Run::remove is new;
// rollback returns the run id, the restored targets, and unverified scratch left in place;
// targets must lie under the target roots (Error::OutsideRoots). plan types gained Deserialize.

// plan (U4; Desired, Content, FileContent, Target, Observed, Change, Action, Plan types foundation)
// Target::{Path, Entry { .., shape }, Program, None}; Action::unsupported(item, target, reason) builds prior Absent, no content
// Target::None: Unsupported or Blocked only, prior Absent, no content (B2); it has no identity, so several may share a plan.
pub struct Options { pub elevated: bool }
pub struct Steps(..);                       // only build makes them; get(program); reused only for identical programs, facts, context
pub struct Built { pub plan: Plan, pub steps: Steps, pub checkout: PathBuf /* canonical */, pub machine: String, pub warnings: Diagnostics }
pub enum Error { Invalid(Diagnostics) /* 3 */, Blocked(String) /* 4 */, Runtime(anyhow::Error) /* 1 */ }
pub fn build(context: &Context, checkout: &Path, state: &State, steps: Option<Steps>, options: Options) -> Result<Built, plan::Error>;
pub fn checkout(context: &Context, chosen: Option<&Path>) -> anyhow::Result<PathBuf>; pub fn default_checkout(context: &Context) -> PathBuf;
impl Plan { pub fn check(&self) -> Result<(), String>; pub fn blocked(&self) -> bool; }
// build: interrupted-run gate, Loadout::load_guarded, machine::detect, resolve, source::Sources + template::render for user
// files, agents::materialize, programs::steps, then state::digest/observe per target. It never writes, locks, or creates
// the state dir. resolve::check_targets stays public for synthetic use but is not on this host pipeline: each user-file
// target goes through reserved::check_path(Write) and a conflict is an unsupported action.
// apply (U4)
pub enum Mode { Interactive, Background }   // Background: never replace, force, elevate, or prompt
pub struct Options { pub force: bool, pub elevated: bool, pub mode: Mode }
pub fn run(context: &Context, checkout: &Path, options: Options) -> Result<Report, apply::Error>;            // preflight, lock, run_under_lock
pub fn run_under_lock(context: &Context, checkout: &Path, lock: state::Lock, options: Options) -> Result<Report, apply::Error>;
pub struct Report { pub checkout, pub machine, pub run: Option<String>, pub outcomes: Vec<(Action, Outcome)>, pub warnings, pub finish_error: Option<String>, pub cancelled: bool /* runtime only */ }
impl Report { pub fn failed(&self) -> bool /* exit 1 */; pub fn unhealthy(&self) -> bool /* exit 4, also when cancelled */; }
pub enum Error { Plan(plan::Error), State(state::Error) }
// Run::finish_with_lock (U3) keeps the lock through program installs; Lock::check (U3) is run_under_lock's first
// call on every route; context.containment.is_cancelled() (U8) is checked before the run begins, each journaled
// action, and each install.

// sync (U8)
pub fn clone(url: &str, context: &Context) -> Result<(), sync::Error>;
pub fn fetch(context: &Context) -> Result<Fetched, sync::Error>;   // UpToDate | Advanced { from, to } | Blocked(reason)
pub mod service { pub fn install(context: &Context, interval_minutes: u32) -> Result<(), Error>; pub fn uninstall(context: &Context) -> Result<(), Error>; pub fn status(context: &Context) -> Result<Status, Error>; }

// release (U9, source)
pub struct Release { .. }                 // only latest makes one; tag(), version(), current() -> &str
pub fn latest(context: &Context) -> Result<Option<Release>, release::Error>;   // Some: strictly newer stable release of a managed binary
pub fn install(release: &Release) -> Result<(), release::Error>;              // re-verifies provenance; captured Containment for --version checks
pub fn checks(context: &Context) -> Vec<Check>;                              // doctor: how the CLI was installed
pub enum Error { Isolated, Unmanaged { reason }, NoRelease, Download { detail }, Verify { detail }, Changed { detail }, Io { .. }, Publish { detail, restored } }
impl Error { pub fn skipped(&self) -> bool; }                                // Isolated | Unmanaged | NoRelease | Busy: report, don't fail
// machine (U9 addition)
pub fn save(context: &Context, name: &str, remote: &str) -> Result<(), machine::Error>; // atomic, 0600 on Unix; refuses links, invalid names, password remotes
```

A unit that must change a foundation type or a frozen signature changes it in its own PR, updates
this section in the same PR, and lists the change under "Decisions made here".

## 12. Recorded follow-ups

Accepted from the foundation review as non-blocking. None of them changes a contract above.

- **Serde diagnostics** (review A, consider 1): the untagged `List`, `OneOrMany`, and `Value` enums
  report "data did not match any variant", and parsing stops at the first error in each file. A
  custom `Deserialize` or a pre-pass could name the expected forms. The input is already rejected
  with exit 3 and a line and column.
- **Context for validate and doctor JSON** (review A, consider 7): every command builds `Context`
  first. So a bad `LOADOUT_HOME` makes `validate` exit 1, and makes `doctor --json` print no JSON.
  Any change must keep the exit-code contract in section 1.
- **Windows folder redirection** (review A, consider 8): config and state derive from home
  (`AppData\Roaming`, `AppData\Local`), not from `%APPDATA%` and `%LOCALAPPDATA%`. Platform context
  work should read the known-folder paths (outside isolation) when Windows VM evidence lands.
