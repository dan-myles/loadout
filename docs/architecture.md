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
  resolve/mod.rs          Resolved model                                    [types: foundation; resolve(): U1]
  plan/mod.rs             Desired, Content, Target, Action, Plan            [types: foundation; build(): U4]
  commands/
    doctor.rs             loadout doctor                                    [foundation, contended]
    validate.rs           loadout validate                                  [foundation; U1/U2 extend]
    plan.rs, apply.rs                                                       [U4]
    status.rs, rollback.rs                                                  [U3]
    update.rs, service.rs                                                   [U8]
    init.rs                                                                 [U9]
  template/                                                                 [U2]
  state/                                                                    [U3]
  apply/                                                                    [U4]
  agents/                 adapter table, skills + instructions targets      [U5]
  mcp/                    server shapes, env syntax, JSON/JSONC/TOML key merge [U6]
  programs/               mod.rs (API), manager.rs (per-manager table), detect.rs (queries) [built, U7]
  sync/                   git clone/fetch, background service               [U8]
  release/                self-update                                       [U9]
```

Which subject may import which:

```
platform, diagnostic, id       -> (none)
reserved                       -> context, platform
context                        -> platform
machine                        -> platform, context, id
manifest                       -> platform, machine, context (path syntax), diagnostic, id, reserved
resolve                        -> manifest, machine, diagnostic
template                       -> resolve, machine
mcp                            -> manifest, resolve, plan (types)
agents                         -> manifest, resolve, mcp, plan (types), machine, context, diagnostic
programs                       -> manifest, resolve, machine, context, diagnostic
state                          -> context, plan (types)
plan                           -> resolve, template, agents, programs, state, context, diagnostic
apply                          -> plan, state, programs, context
sync                           -> context, manifest, diagnostic
release                        -> context, diagnostic
```

`plan/mod.rs` holds only data types until U4 adds `build`. `agents`, `mcp`, and `state` import those
types and never call `plan::build`, which keeps the graph acyclic.

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

U1 adds the checks that need a machine:

- an added item that turns out incomplete
- a server left with mixed or missing transports
- cross-layer target collisions
- one instructions item per agent
- `windows_problem` on every target that resolves for Windows
- `reserved::check_path` on every expanded target

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

### Templates (frozen, U2)

A user file whose `source` ends in `.tmpl` is a template, and nothing else is rendered. **Default**:
the suffix, not a field, marks the template. The mark is then visible in the repo tree, and no layer
can switch rendering on or off by accident. The syntax is minijinja's Jinja subset, with undefined
values as errors: `{{ email }}`, `{% if work %}…{% else %}…{% endif %}`,
`{% if machine.os == "windows" %}`. Values come from the resolved `[variables]` at top level, plus
facts under `machine` (`name`, `os`, `distro`, `arch`, `home`). There is no environment access, no
include, and no filesystem. A missing variable stops the run before any write and names the file
and the variable. A `.tmpl` source on a skill or instructions item gets a warning, because only user
files are rendered.

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

## 4. Local state (frozen, U3)

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

## 5. Apply lifecycle (frozen, U3 + U4)

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
- **Drift.** A drifted target is left alone and reported, with exit 4. `apply --force` backs it up
  and overwrites it. The background service never forces.
- **Removal.** An owned target that hasn't changed since loadout wrote it is backed up, then removed.
  A drifted one is released from ownership and left in place, with a warning. Programs are never
  uninstalled in v1.
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
    serde_json's own escaping and number formatting. The sort is explicit because U6 may enable
    serde_json's `preserve_order`, under which `Map` keeps insertion order. A value read from TOML
    is first converted to `serde_json::Value`, so both formats hash the same way.

  Plan, journal, and records all use this definition.
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
- **Rollback.** `loadout rollback [RUN]` restores a run's backups and deletes the files that run
  created; the default is the latest run. `status` reports an interrupted run, and the same command
  rolls it back.
- **Concurrency.** `<state>/lock` (`std::fs::File::try_lock`) keeps a background update and a manual
  apply from overlapping. The second one exits 1 with a clear message.

## 6. Agents (frozen, U5 adapters, U6 MCP)

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
| muse | `<xdg_config>/muse/skills` (macOS, Linux) | unsupported (no documented path) | `<xdg_config>/muse/settings.json`, `/mcp_servers`, `muse-json` | `${VAR}` |
| antigravity | `$GEMINI_HOME` or `~/.gemini`, then `/antigravity-cli/skills` | `…/AGENTS.md` | `…/config/mcp_config.json`, `/mcpServers`, `antigravity-json` | none (undocumented) |

`<xdg_config>` is `Context.xdg_config`: `$XDG_CONFIG_HOME` (outside isolation) or `~/.config` on
**every** OS. OpenCode and Muse use it even on macOS (not `~/Library/Application Support`) and
Windows (`%USERPROFILE%\.config`, not `%APPDATA%`), so their rows never use the OS-native
`{config}`. Muse on Windows is unsupported until its paths are documented. Windows paths for Codex, Cursor, Pi,
and Antigravity are inferred from home-directory joins in their source; Windows VM evidence confirms
or corrects them.

- **Capabilities are data.** U5 keeps a table of
  `Capability<T> = Supported(PerOs<T>) | Unsupported(reason) | Unknown(reason)` per agent and kind.
  An item an agent can't take is an `unsupported` plan action, never a silent skip. There is no
  plugin framework.
- **Skills** are copied into each target agent's skills dir as `Content::Tree`. Validate has already
  confined the folder (section 3), and U5 re-checks it when copying. It copies regular files,
  dereferences file links (the copy gets the file's bytes, not a link), and keeps each file's
  executable bit (`FileContent.executable`). A skills dir loadout doesn't own is never deleted.
- **Instructions**: at most one `instructions` item may reach each agent; U1 makes a second one a
  resolve error. The file is owned whole.
- **MCP is a key merge, never a file write** (U6). Parse the file, change only the server names
  loadout owns, and keep every other key, comment, and format detail.
  - TOML goes through `toml_edit`. JSONC goes through a comment-preserving editor; OpenCode's
    `opencode.jsonc` is supported, not refused.
  - Agent-written state in `~/.claude.json` (`oauthAccount`, `projects`) and `~/.codex/config.toml`
    (`projects`, `tui`) is never touched.
  - An existing server name that loadout doesn't own is `blocked`, not overwritten.
  - Muse needs `"schema_version": 1`. Antigravity takes `serverUrl` and rejects SSE.
  - Codex's `codex-env-vars` passes a variable through only under its own name (`env_vars`), and
    takes a bearer token only as `bearer_token_env_var`. So a stdio `env = { K = { env = "OTHER" } }`
    whose key differs from the variable has no Codex form. The same holds for a header other than
    `Authorization` with prefix `Bearer `. U6 returns `mcp::Unsupported` for these, and the plan
    shows an `unsupported` action for Codex; nothing is renamed or resolved.
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
  - **U6 applies the same rule.** Every MCP write calls `check_path` with
    `Use::KeyMerge(Merge { map, format, shape })`, for built-in adapters and custom agents alike, on
    expanded, canonical, and override-alias paths. A shared config reached through an alias still
    accepts only its own map. Validate already checks custom declarations statically.
- An unowned shared config is never adopted or replaced as a whole file. Only U6's owned keys change.
- **Doctor diagnostics** (U5): report which overrides are set (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `GROK_HOME`, `PI_CODING_AGENT_DIR`, `XDG_CONFIG_HOME`). Warn when Codex `AGENTS.override.md`
  shadows `AGENTS.md`. Note that a T3-launched Antigravity sees skills only: T3 gives it a private
  `GEMINI_HOME`, so user MCP and instructions don't reach it. Report double loading through
  `~/.claude` compat dirs. Doctor claims nothing about T3 profile settings it doesn't read.

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
`LC_ALL=C`, so nothing waits on a prompt and the not-found messages below are stable.

| Manager | Needs | Installed-query (one per program) | Not installed when | Install |
| --- | --- | --- | --- | --- |
| `brew` | brew | `brew info --json=v2 --formula P` | `formulae[0].installed` is `[]` | `brew install --formula P` |
| `cask` | brew | `brew info --json=v2 --cask P` | `casks[0].installed` is `null` | `brew install --cask P` |
| `apt` | apt-get, dpkg-query | `dpkg-query -W -f '${db:Status-Status} ' P` | exit 1, or no status is `installed` | `apt-get install -y --no-upgrade P`, `DEBIAN_FRONTEND=noninteractive` |
| `dnf` | dnf, rpm | `rpm -q P` | exit 1 with "is not installed" | `dnf install -y P` |
| `pacman` | pacman | `pacman -Q P` | exit 1 with "was not found" | `pacman -S --needed --noconfirm P` |
| `zypper` | zypper | `zypper --non-interactive --no-refresh search --installed-only --match-exact --type package P` | exit 104 | `zypper --non-interactive install P` |
| `flatpak` | flatpak | `flatpak info P` (any installation) | exit 1 with "not installed" | `flatpak install --user --noninteractive P` |
| `snap` | snap | `snap list P` | exit 1 with "no matching snaps installed" | `snap install P` |
| `winget` | winget | `winget list --id P --exact` | `0x8A150014` | `winget install --id P --exact --silent --accept-package-agreements --accept-source-agreements --disable-interactivity --scope user\|machine` |
| `scoop` | scoop | `scoop prefix APP` (`APP` is `P` after any `bucket/`) | exit 1 with "Could not find app path" | `scoop install P` |
| `choco` | choco (2.x) | `choco list --exact P --limit-output` | no `P\|version` line, or exit 2 | `choco install P -y --no-progress`; 1641 and 3010 are success |
| `npm`, `pnpm` | npm / pnpm | `ls --global --depth=0 --json P` | no `dependencies.P` (exit 0 or 1) | `npm install --global P` / `pnpm add --global P` |
| `bun` | bun | `bun pm ls --global` | no `── P@…` line | `bun add --global P` |
| `cargo` | cargo | `cargo install --list` | no `P v…:` line | `cargo install P` |
| `uv` | uv | `uv tool list` | no `P v…` line (PEP 503 names) | `uv tool install P` |
| `pipx` | pipx | `pipx list --json` | no `venvs` entry (PEP 503 names) | `pipx install P` |
| `go` | go | `go env GOBIN GOPATH`, then `go version -m BIN` | `BIN` (from `GOBIN` or `GOPATH/bin`) is absent | `go install P@latest` |

- **Installed needs positive evidence.** Every row's other outcomes (an unexpected exit code,
  unreadable JSON, a missing tool) are `programs::Error::Detect`, so a broken or locked manager is
  never read as "installed", and `steps` fails before any write. A Go binary at `BIN` built from a
  different package is an error, never overwritten.
- **Package ids are passed as one exact argument.** `steps` refuses, before any subprocess, an id
  that is empty, starts with `-`, has a character outside letters, digits, and `@ / . _ + : ~ -`,
  or pins a version (`name@1.2` for npm, pnpm, bun; any `@` for cargo, uv, pipx, go). Go is the
  one manager given `@latest`, because `go install` requires a version.
- **Scope.** `apt`, `dnf`, `pacman`, `zypper`, `snap`, and `choco` are system scope. A missing
  winget package is user scope when `winget show --id P --exact --scope user` finds an installer,
  and machine scope when it returns `0x8A150010` (no applicable installer). Everything else is
  user scope. `install(step, false)` refuses a system-scope step with `Error::NeedsElevation`
  before any subprocess. With `elevated`, Unix runs `sudo -- [env K=V] TOOL …` (or the tool
  directly as root), and an absent `sudo` is `Error::NoSudo`. Windows runs the tool directly.
  User-scope managers are never elevated.
- **Verified.** After the manager exits with success, `install` runs the installed-query again; a
  success that left the program missing is `Error::NotInstalled`.
- **Doctor** (`programs::checks`) adds `programs` (this OS's managers grouped as per user, with
  `--elevated`, per package, and not on PATH; a warning when none is available), `elevation` on
  macOS and Linux when a system-scope manager is present (sudo found, root, or a warning), and
  `flatpak` (its per-user remotes; a warning when there is none). Under isolation it only looks on
  PATH and runs no manager.

## 8. Updates (frozen, U8 + U9)

Automatic updates are required, so they are on by default.

- `loadout init <url>` clones the loadout into `<state>/loadout` with the system `git`, writes
  `machine.toml`, plans, applies, and installs the background service unless `[update] auto = false`.
  A private loadout works through the user's existing Git credentials or SSH keys. Loadout stores no
  token.
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
- **Self-update** (U9) runs when `[update] cli = true` and the install script installed the binary.
  `update` reads the latest public GitHub release with no token, verifies the asset's `.sha256`, and
  replaces itself. A binary from `cargo install` or a package manager is never replaced; doctor
  reports how the CLI was installed.

## 9. Bootstrap and release (frozen, U9 + release CI)

- Release CI (PR #2) builds `loadout-<target>.tar.gz`, or `.zip` on Windows, each with a `.sha256`,
  for x86_64 and aarch64 Linux (musl), macOS, and Windows. The repo is public, so
  `releases/latest/download/` URLs need no token.
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
| `loadout validate [DIR] [--json]`: strict schema and static checks; exit 3 when invalid | foundation | U1 adds resolution across every machine on all three OSes, U2 adds template syntax |
| `loadout plan [--loadout DIR] [--json]` | frozen | U4 |
| `loadout apply [--loadout DIR] [--force] [--elevated]` | frozen | U4 |
| `loadout status [--json]` | frozen | U3 |
| `loadout rollback [RUN]` | frozen | U3 |
| `loadout update [--background]` | frozen | U8 (+U9 self-update) |
| `loadout service install\|uninstall\|status` | frozen | U8 |
| `loadout init <URL> [--name NAME] [--no-service]` | frozen | U9 |

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
}
impl Context {
    pub fn from_env() -> Result<Self, context::Error>;
    pub fn resolve(os: Os, var: impl Fn(&str) -> Option<OsString>, default_home: Option<PathBuf>) -> Result<Self, context::Error>;
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
pub fn normalize(path: &Path) -> io::Result<PathBuf>;                         // canonical existing ancestor + missing tail; stats only
pub enum Conflict { Reserved { entry: Entry, reserved: String }, Unverifiable { path: PathBuf, error: String } }

// commands::doctor (foundation): units extend `[foundation(context), <subject>::checks(context), …].concat()`

// diagnostic (foundation)
pub struct Diagnostic { pub severity: Severity, pub file: Option<PathBuf>, pub key: Option<String>, pub message: String }
pub struct Diagnostics(..);              // push, extend, has_errors, count, iter
pub struct Check { pub name: String, pub status: CheckStatus, pub detail: String }   // Pass | Info | Warn | Fail

// manifest (foundation)
impl Loadout { pub fn load(root: &Path) -> Result<(Loadout, Diagnostics /* warnings */), Diagnostics>; }
// Loadout { root, machines: BTreeMap<String, MachineEntry>, layers: Vec<LayerFile { scope: Scope, file, layer: Layer }> }
// Scope::{Global, Os(Os), Tag(String), Machine(String)}; layers come global, os, tags, machines, each sorted by name

// resolve (U1; Resolved types foundation)
pub fn resolve(loadout: &Loadout, facts: &Facts) -> Result<Resolved, Diagnostics>;
pub fn check_all(loadout: &Loadout) -> Diagnostics;   // every listed machine plus an unlisted one, on all three OSes

// template (U2)
pub fn render(file: &Path, source: &str, variables: &BTreeMap<String, Variable>, facts: &Facts) -> Result<String, template::Error>;
pub fn check(file: &Path, source: &str) -> Result<(), template::Error>;

// mcp (U6)
pub fn entry(server: &McpServer, shape: McpShape, env: EnvSyntax) -> Result<serde_json::Value, mcp::Unsupported>;
pub fn read(file: &Path, format: McpFormat, pointer: &str) -> Result<BTreeMap<String, serde_json::Value>, mcp::Error>;
pub fn write(file: &Path, format: McpFormat, pointer: &str, set: &BTreeMap<String, serde_json::Value>, remove: &[String]) -> Result<Vec<u8>, mcp::Error>; // returns new bytes; preserves everything else

// agents (U5)
pub fn materialize(resolved: &Resolved, facts: &Facts, context: &Context) -> Result<(Vec<Desired>, Vec<Action /* unsupported */>), agents::Error>;
pub fn checks(context: &Context) -> Vec<Check>;

// programs (U7, built)
pub struct Step { pub program: String, pub state: State }       // Step::needs_elevation(): Missing with Scope::System
pub enum State { Installed(Package), Missing { package: Package, scope: Scope }, Unsupported(String), Blocked(String) }
pub struct Package { pub manager: Manager, pub id: String, pub executable: PathBuf }   // the selected manager and its exact id
pub enum Scope { User, System }
pub const ISOLATED: &str;                                       // the Blocked reason under LOADOUT_HOME
pub fn steps(programs: &BTreeMap<String, Program>, facts: &Facts, context: &Context) -> Result<Vec<Step>, programs::Error>; // every step blocked when context.isolated
pub fn install(step: &Step, elevated: bool) -> Result<(), programs::Error>;   // Missing steps only; re-queries after
pub fn checks(context: &Context) -> Vec<Check>;
// plan::build: Installed -> unchanged (prior Installed); Missing -> install (prior Absent), or blocked when it
// needs elevation the run lacks; Unsupported and Blocked carry their reason. Order follows `programs` (by id).

// state (U3)
impl State { pub fn load(context: &Context) -> Result<State, state::Error>; pub fn record(&self, target: &Target) -> Option<&Record>; }
pub fn lock(context: &Context) -> Result<Lock, state::Error>;
pub fn observe(target: &Target) -> Result<Observed, state::Error>;   // the one hash definition (section 5)
impl Run { pub fn begin(context: &Context, plan: &Plan) -> Result<Run, state::Error>; pub fn write(&mut self, action: &Action, content: &Content) -> Result<(), state::Error>; pub fn finish(self, state: &mut State) -> Result<(), state::Error>; }
// Run::write re-observes and returns Error::Changed { target } when the result != action.prior
pub fn rollback(context: &Context, run: Option<&str>) -> Result<Vec<Target>, state::Error>;

// plan (U4; Desired, Content, FileContent, Target, Observed, Change, Action, Plan types foundation)
pub fn build(resolved: &Resolved, facts: &Facts, context: &Context, loadout: &Path, state: &State) -> Result<Plan, Diagnostics>;
// `loadout` = the active checkout root (default clone or --loadout DIR); passed to every reserved::check_path call
// apply (U4)
pub fn run(plan: &Plan, context: &Context, loadout: &Path, options: Options) -> Result<Report, apply::Error>; // re-checks reserved paths before each backup and write

// sync (U8)
pub fn clone(url: &str, context: &Context) -> Result<(), sync::Error>;
pub fn fetch(context: &Context) -> Result<Fetched, sync::Error>;   // UpToDate | Advanced { from, to } | Blocked(reason)
pub mod service { pub fn install(context: &Context, interval_minutes: u32) -> Result<(), Error>; pub fn uninstall(context: &Context) -> Result<(), Error>; pub fn status(context: &Context) -> Result<Status, Error>; }

// release (U9)
pub fn latest(context: &Context) -> Result<Option<Release>, release::Error>;
pub fn install(release: &Release) -> Result<(), release::Error>;
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
