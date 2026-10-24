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
  programs/                                                                 [U7]
  sync/                   git clone/fetch, background service               [U8]
  release/                self-update                                       [U9]
```

Which subject may import which:

```
platform, diagnostic, id       -> (none)
context                        -> platform
machine                        -> platform, context, id
manifest                       -> platform, machine, context (path syntax), diagnostic, id
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
| `src/commands/doctor.rs` | one `checks.extend(<subject>::checks(context));` line |
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
listed twice, and a listed tag with no `tags/<tag>.toml` layer. The match is exact and
case-sensitive against the loaded tag layers, never a path probe, so `../x` or `Work` can't pass on
any file system. A tag file that no machine lists is a warning.

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
enabled = ["claude", "codex", "devin"]   # list; tags and machines may edit it

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

What validate enforces today, before any machine is known:

- A global-layer item must be complete, and may not use `remove`, `replace`, or list edits.
- `remove = true` stands alone.
- A `replace = true` item must be complete.
- A server is stdio (`command`, `args`, `env`) or remote (`url`, `headers`), never both.
- A remote `url` is `http://` or `https://` with a real host (a DNS name, IPv4, or `[IPv6]`) and an
  optional port from 1 to 65535. It has no whitespace and no `user:pass@` credentials.
- A custom MCP `map` is an RFC 6901 pointer with non-empty parts, where `~` appears only as `~0` or
  `~1`.
- Ids are lowercase `[a-z0-9][a-z0-9_-]*`.
- Sources exist inside the loadout, with symlinks resolved.
- A skill folder is walked in full. A link inside it may point only to a file inside the loadout.
  A link to a folder, a broken link, a link leaving the loadout, and any special file are errors.
  Folder links are never followed, so a link loop can't make the walk loop. An unreadable entry
  is an error.
- Targets are clean `~/` or `{config}/` paths, and two file items in one layer may not share a target.
- Every agent named anywhere is built in or declared.

U1 adds the checks that need a machine: an added item that turns out incomplete, cross-layer target
collisions, one instructions item per agent, and `windows_problem` on every target that resolves for
Windows.

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
  the agent dir variables (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`, `PI_CODING_AGENT_DIR`),
  so `Context.agent_dirs` is empty. Nothing a run reads or writes can fall outside the isolated
  home, except an explicit `LOADOUT_STATE_DIR`.
- **`LOADOUT_STATE_DIR`.** Replaces the state dir, isolated or not.
- **Without `LOADOUT_HOME`.** Ordinary overrides apply: an absolute `XDG_CONFIG_HOME` and
  `XDG_STATE_HOME`, and the absolute agent dir variables, which are collected into
  `Context.agent_dirs`. Units read agent overrides only through `Context::agent_dir(name)` and never
  call `std::env` themselves, so isolation is enforced in one place.
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

`machine::name` (foundation) reads `machine.toml`. When the file is absent, the machine name
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
- **Hashing** (`state::observe`, U3) is defined once:
  - a file: SHA-256 of its bytes
  - a tree: SHA-256 over its sorted relative paths, each with its executable bit and bytes
  - an entry: SHA-256 of the canonical JSON of that one key

  Plan, journal, and records all use this definition.
- **Safe write.** Before the first write, the run's `journal.json` lists every action with its
  `prior` and backup path. For each write, `Run::write(action, content)`:
  1. Re-observes the target and compares the result to `action.prior`. If they differ, it aborts
     that action with `state::Error::Changed` and writes nothing, so a target that changed after the
     plan (even an unowned file planned for `replace`) is never overwritten.
  2. Resolves a symlinked target and writes the real file, so the user's links survive.
  3. Writes a temp file in the same directory, `fsync`s it, and renames it over the target.
  4. Sets modes. An existing file keeps its permission bits and is never widened (for example, it
     keeps `0600`); only its execute bits follow `FileContent.executable`. A new file gets the
     default mode, plus execute bits when `executable` is set. Execute bits mean nothing on Windows
     and are ignored there.
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
| opencode | `{config}/opencode/skills` (XDG on every OS) | `…/opencode/AGENTS.md` | `…/opencode/opencode.json` or `.jsonc`, `/mcp`, `opencode-json` | `{env:VAR}` |
| pi | `$PI_CODING_AGENT_DIR` or `~/.pi/agent`, then `/skills` | `…/AGENTS.md` | `…/mcp.json`, `/mcpServers`, `pi-json` | `${VAR}` |
| muse | `~/.config/muse/skills` (macOS, Linux) | unsupported (no documented path) | `~/.config/muse/settings.json`, `/mcp_servers`, `muse-json` | `${VAR}` |
| antigravity | `~/.gemini/antigravity-cli/skills` | `~/.gemini/AGENTS.md` | `~/.gemini/config/mcp_config.json`, `/mcpServers`, `antigravity-json` | none (undocumented) |

Muse on Windows is unsupported until its paths are documented. Windows paths for Codex, Cursor, Pi,
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
- **Custom agents** get exactly the declared paths, their declared `format`, `map`, `shape`, and
  `env_syntax`, and copy-only skills.
- **Never touched**: OAuth and auth stores (Claude Keychain and `.credentials.json`; Codex
  `auth.json`, `.credentials.json`, and keyring; Cursor `auth.json`; Grok `auth.json` and
  `mcp_credentials.json`; OpenCode `mcp-auth.json`; Pi `auth.json` and `mcp-auth.json`; Muse
  `auth.json` and `trust.json`; Antigravity `mcp_oauth_tokens.json`), T3's `settings.json` (it holds
  provider secrets), and T3's private profiles and shadow homes.
- **Doctor diagnostics** (U5): report which overrides are set (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `GROK_HOME`, `PI_CODING_AGENT_DIR`, `XDG_CONFIG_HOME`). Warn when Codex `AGENTS.override.md`
  shadows `AGENTS.md`. Note that a T3-launched Antigravity sees skills only: T3 gives it a private
  `GEMINI_HOME`, so user MCP and instructions don't reach it. Report double loading through
  `~/.claude` compat dirs. Doctor claims nothing about T3 profile settings it doesn't read.

## 7. Programs (frozen, U7)

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
- No direct downloads and no install scripts in v1. Loadout never installs a package manager itself;
  doctor names what is missing.

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
pub struct Context { pub os: Os, pub home: PathBuf, pub config: PathBuf, pub state: PathBuf }
impl Context { pub fn from_env() -> Result<Self, context::Error>; pub fn expand(&self, path: &str) -> Option<PathBuf>; }
pub fn which(program: &str) -> Option<PathBuf>;

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

// programs (U7)
pub fn steps(programs: &BTreeMap<String, Program>, facts: &Facts) -> Result<Vec<Step>, programs::Error>;
pub fn install(step: &Step, elevated: bool) -> Result<(), programs::Error>;
pub fn checks(context: &Context) -> Vec<Check>;

// state (U3)
impl State { pub fn load(context: &Context) -> Result<State, state::Error>; pub fn record(&self, target: &Target) -> Option<&Record>; }
pub fn lock(context: &Context) -> Result<Lock, state::Error>;
pub fn observe(target: &Target) -> Result<Observed, state::Error>;   // the one hash definition (section 5)
impl Run { pub fn begin(context: &Context, plan: &Plan) -> Result<Run, state::Error>; pub fn write(&mut self, action: &Action, content: &Content) -> Result<(), state::Error>; pub fn finish(self, state: &mut State) -> Result<(), state::Error>; }
// Run::write re-observes and returns Error::Changed { target } when the result != action.prior
pub fn rollback(context: &Context, run: Option<&str>) -> Result<Vec<Target>, state::Error>;

// plan (U4; Desired, Content, FileContent, Target, Observed, Change, Action, Plan types foundation)
pub fn build(resolved: &Resolved, facts: &Facts, context: &Context, state: &State) -> Result<Plan, Diagnostics>;
// apply (U4)
pub fn run(plan: &Plan, context: &Context, options: Options) -> Result<Report, apply::Error>;

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
