# The loadout format

A **loadout** is a Git repository you own. It declares what every machine should carry. The loadout
CLI reads it and never writes to it. This page covers every table and rule a loadout can use. For
the commands that act on a loadout, see the [agent guide](agent-guide.md). For the full contract,
with every edge case, see [architecture.md](architecture.md#3-the-loadout-on-disk-foundation).

**On this page:** [Layout](#layout) · [Root tables](#root-tables) · [Items](#items) ·
[Layers and overrides](#layers-and-overrides) · [Conditions](#conditions) ·
[Templates](#templates) · [Paths](#paths) · [Secrets](#secrets) · [Agents](#agents) ·
[Programs](#programs) · [Validation](#validation)

## Layout

```text
my-loadout/
├── loadout.toml          required: the global layer, plus [loadout] and [machines]
├── os/                   optional: macos.toml, linux.toml, windows.toml (only these names)
├── tags/<tag>.toml       optional: one layer per tag, such as work or laptop
├── machines/<name>.toml  optional: one layer per machine name
└── files/ skills/ ...    sources, in any layout you like, referenced by relative path
```

Only `loadout.toml` is required. `os/`, `tags/`, and `machines/` may hold only `<name>.toml`
files, plus `.DS_Store` and `.gitkeep`, which are ignored. Every other folder or file is yours:
validate ignores it unless an item references it. An unknown key anywhere is an error, so a typo
never passes silently.

[`examples/starter/`](../examples/starter) is a small complete loadout that passes `loadout validate`.

## Root tables

These appear only in `loadout.toml`.

```toml
[loadout]
version = 1                       # required; v1 accepts only 1

[machines.work-laptop]            # a machine name and its tags, in order; a later tag wins
tags = ["work", "laptop"]
```

A machine the loadout doesn't list gets the global and OS layers only. A machine name is chosen
on its first `loadout init` and saved on the machine. It defaults to the hostname, cut at the first
`.`, lowercased, and it doesn't change when the computer is renamed.

Ids (machine names, tags, item ids, agent ids) are lowercase: `[a-z0-9][a-z0-9_-]*`.

## Items

Each of these tables can appear in any layer file.

| Table | Declares | Required fields |
| --- | --- | --- |
| `[variables]` | template values: string, integer, or boolean | none (`machine` is reserved) |
| `[update]` | the background sync: `auto` (default `true`), `interval_minutes` (default `60`, minimum `15`) | none |
| `[agents]` | `enabled`: which agents receive skills, instructions, and MCP servers | none; the default is **no agent** |
| `[agents.custom.<id>]` | an agent with no built-in adapter | `evidence`, plus at least one path table |
| `[files.<id>]` | a user file copied (or rendered) into your home | `source`, `target` |
| `[skills.<id>]` | a skill folder holding `SKILL.md`, copied into each agent's skills dir | `source` |
| `[instructions.<id>]` | global agent instructions, such as a user-level `AGENTS.md` | `source` |
| `[mcp.<id>]` | an MCP server definition | `command` (stdio) or `url` (remote) |
| `[programs.<id>]` | one program, with its package id per manager | at least one manager |

```toml
[variables]
email = "ada@example.com"
work = false

[update]
auto = true
interval_minutes = 60

[agents]
enabled = ["claude", "codex"]

[files.gitconfig]
source = "files/gitconfig.tmpl"   # relative to the loadout root; .tmpl marks a template
target = "~/.gitconfig"           # starts with ~/ or {config}/
when = { os = ["linux", "macos"] }

[skills.review]
source = "skills/review"          # a folder holding SKILL.md
agents = ["claude"]               # optional: narrows the enabled agents, never widens them

[instructions.global]
source = "instructions/AGENTS.md" # at most one instructions item reaches each agent

[mcp.filesystem]                  # stdio server
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "~/Developer"]
env = { LOG_LEVEL = "warn" }

[mcp.github]                      # remote server
url = "https://api.githubcopilot.com/mcp/"
headers = { Authorization = { env = "GITHUB_TOKEN", prefix = "Bearer " } }

[programs.ripgrep]
brew = "ripgrep"
apt = "ripgrep"
winget = "BurntSushi.ripgrep.MSVC"
```

`[update] cli` is still accepted so older loadouts stay valid, but it has no effect and validate
warns about it. The CLI updates itself only when you run `loadout update`.

## Layers and overrides

Layers apply broadest first: **global** (`loadout.toml`), then the machine's **OS**, then each
**tag** in the order the machine lists them, then the **machine** itself.

```mermaid
flowchart LR
  G["loadout.toml<br/>global"] --> O["os/linux.toml<br/>OS"] --> T["tags/work.toml<br/>each tag, in order"] --> M["machines/work-laptop.toml<br/>machine"] --> R(["what this machine gets"])
```

A narrower layer that names an item id from a broader layer edits that item:

| A narrower layer writes | Effect |
| --- | --- |
| `[files.x]` with fields, and no `x` is inherited | adds `x`; it must be complete |
| `[files.x]` with fields, and `x` is inherited | changes only the fields written |
| `[files.x]` with `replace = true` | replaces `x` whole; it must be complete |
| `[files.x]` with `remove = true` and nothing else | removes the inherited `x` |
| `args = [...]`, `agents = [...]`, `enabled = [...]` | the list replaces the inherited one |
| `args = { add = [...], remove = [...] }` | edits the inherited list: removes first, then appends |
| `env = {...}`, `headers = {...}`, `when = {...}` | the table replaces the inherited one |
| `[variables]`, `[update]` | the narrower layer wins per key |

The same rules apply to every item kind (`files`, `skills`, `instructions`, `mcp`, `programs`). A
global-layer item must be complete, and may not use `remove`, `replace`, or list edits.

```toml
# tags/work.toml
[variables]
email = "ada@work.example"          # wins over the global value on work machines

[agents]
enabled = { add = ["cursor"] }      # edit the inherited list

[mcp.github]
agents = ["claude"]                 # only Claude gets this server on work machines

[skills.personal-notes]
remove = true                       # work machines don't get this skill
```

To switch an MCP server between stdio and remote in a narrower layer, write `replace = true` with
the complete new server. A field edit that would leave both `command` and `url` is an error.

## Conditions

`when` limits an item to machines whose facts match. Each key takes a string or a list (any value
matches), and every key present must match. An item whose condition fails is absent on that machine.

| Key | Values |
| --- | --- |
| `os` | `macos`, `linux`, `windows` |
| `distro` | the lowercase `ID` from `/etc/os-release`, such as `ubuntu` or `fedora` |
| `arch` | `x86_64`, `aarch64` |
| `machine` | a machine name |

```toml
[files.ghostty]
source = "files/ghostty.conf"
target = "{config}/ghostty/config"
when = { os = "macos", arch = "aarch64" }
```

## Templates

A user file whose `source` ends in `.tmpl` is rendered. Nothing else is: skills and instructions
are always copied verbatim. The syntax is [minijinja](https://docs.rs/minijinja)'s Jinja subset.

```jinja
[user]
	name = {{ name }}
	email = {{ email }}
{% if machine.os == "windows" %}
[core]
	autocrlf = true
{% endif %}
```

- Top-level names are the resolved `[variables]`, with their TOML types. `"false"` as a string is
  truthy, so use a real boolean.
- `machine` holds the facts: `machine.name`, `machine.os`, `machine.distro`, `machine.arch`,
  `machine.home`. On a machine with no distro, `distro` has no value: it is falsy, equals no
  string, and printing it is an error, so write `{{ machine.distro or "unknown" }}`.
- **Strict.** A missing variable is an error wherever it is used, so a typo never renders. A
  variable inside a branch that isn't taken isn't needed, so a Windows-only variable can live in
  `os/windows.toml`. Use `is defined` or `default(...)` for an optional value.
- Validate checks every template's syntax. Plan renders each one for the real machine before it
  builds any action, so a missing variable stops the run with nothing written.

## Paths

`target` (and custom-agent paths) must start with `~/` or `{config}/` and use `/`. They may not
contain `\`, `..`, `.`, or empty parts.

| `{config}` on | Means |
| --- | --- |
| Linux | `$XDG_CONFIG_HOME`, or `~/.config` |
| macOS | `~/Library/Application Support` |
| Windows | `~\AppData\Roaming` |

A target that reaches Windows also may not hold `<>:"|?*`, end in a dot or space, or name a device
such as `CON` or `NUL`. Validate reports that as an error when the declaration certainly reaches
Windows, and as a warning when a narrower layer might still change it.

`source` is relative to the loadout root and must stay inside it after symlinks are resolved. Git
metadata is never loadout content: a source under `.git`, or a skill folder holding a `.git` entry,
is an error.

Loadout refuses to write into reserved places: agent auth stores, OAuth tokens, shared settings it
doesn't own, and its own state dir.

## Secrets

A loadout never holds a secret value. It holds only the **name** of an environment variable on the
machine:

```toml
[mcp.github]
url = "https://api.githubcopilot.com/mcp/"
headers = { Authorization = { env = "GITHUB_TOKEN", prefix = "Bearer " } }

[mcp.search]
command = "search-mcp"
env = { SEARCH_API_KEY = { env = "SEARCH_API_KEY" }, LOG_LEVEL = "warn" }
```

Loadout writes each reference in the agent's own syntax (for example `${GITHUB_TOKEN}` for Claude,
or `bearer_token_env_var` for Codex), so the agent reads the variable when it starts. The value
never reaches an agent config, a plan, a journal, a backup, or the output.

- Validate warns when a header or env value that looks like a credential (authorization, token,
  secret, password, API key) is written as plain text.
- An agent with no documented env syntax (Antigravity, or a custom agent with
  `env_syntax = "none"`) can't take a server that uses an env reference. That shows up as an
  `unsupported` action for that agent, never a guess.
- Keychains and password managers aren't supported, because resolving a value into a config file
  would put the secret on disk.

## Agents

**No agent is enabled by default.** Name each agent in `[agents] enabled`. An item's own `agents`
list narrows that set and never widens it.

Built-in agents: `claude`, `codex`, `cursor`, `grok`, `muse`, `pi`, `opencode`, `antigravity`.

| Agent | Skills | Global instructions | MCP |
| --- | --- | --- | --- |
| claude | yes | `CLAUDE.md` | `~/.claude.json` |
| codex | yes (`~/.agents/skills`) | `AGENTS.md` | `config.toml` |
| cursor | yes | unsupported (User Rules live in app settings) | `~/.cursor/mcp.json` |
| grok | yes | `AGENTS.md` | `config.toml` |
| opencode | yes | `AGENTS.md` | `opencode.json` or `.jsonc` |
| pi | yes | `AGENTS.md` | `mcp.json` |
| muse | macOS, Linux | unsupported (no documented path) | macOS, Linux |
| antigravity | yes | `AGENTS.md` | yes, but no env references |

`loadout doctor` prints the exact path of every skill dir, instructions file, and MCP file on the
machine it runs on, with the evidence each one comes from. Windows paths for Codex, Cursor, Pi, and
Antigravity are inferred from those agents' source rather than confirmed on Windows.

An MCP server is merged into the agent's config as one key. Loadout owns that key only, and
leaves the rest of the file as it was, including the comments of JSONC and TOML files. It never overwrites a server
of the same name that it didn't write.

For an agent with no built-in adapter, declare its paths yourself:

```toml
[agents]
enabled = ["claude", "devin"]

[agents.custom.devin]
evidence = "https://docs.example/devin-config"   # required: where these paths come from
skills = { macos = "~/.devin/skills", linux = "~/.devin/skills" }   # an OS left out is unsupported
instructions = { linux = "~/.devin/AGENTS.md" }

[agents.custom.devin.mcp]
linux = "~/.devin/mcp.json"
format = "json"                    # json | jsonc | toml
map = "/mcpServers"                # JSON pointer to the servers table in the file
shape = "cursor-json"              # how one server is written
env_syntax = "${VAR}"              # ${VAR} | ${env:VAR} | {env:VAR} | codex-env-vars | none
```

Shapes are `claude-json`, `codex-toml`, `cursor-json`, `grok-toml`, `opencode-json`, `pi-json`,
`muse-json`, and `antigravity-json`. A custom agent declared again in a narrower layer is replaced
whole.

## Programs

`[programs.<id>]` maps one program to its package id under each manager you care about. Loadout
uses the first manager that is on `PATH`, in this order:

| OS | Order |
| --- | --- |
| macOS | `brew`, `cask`, then the language managers |
| Linux | `apt`, `dnf`, `pacman`, `zypper`, `flatpak`, `snap`, `brew`, then the language managers |
| Windows | `winget`, `scoop`, `choco`, then the language managers |

The language managers come in the order `cargo`, `uv`, `pipx`, `npm`, `pnpm`, `bun`, `go`.

- Only missing programs are installed. Loadout never upgrades, pins versions, or uninstalls.
- A brew or cask id from a third-party tap, such as `oven-sh/bun/bun`, trusts that tap: apply runs
  `brew tap` and `brew trust --tap` before installing it, and its installed check reads the keg in
  `Cellar` or `Caskroom` so plan never needs the trust.
- An `apt` package from a third-party repository names that repository with `apt-repo`. Naming it
  is the trust, as with a tap:

  ```toml
  [programs.tailscaled]
  brew = "tailscale"
  apt = "tailscale"
  apt-repo = { url = "https://pkgs.tailscale.com/stable/ubuntu", key = "https://pkgs.tailscale.com/stable/ubuntu/{codename}.noarmor.gpg" }
  ```

  `url` and `key` are `https://` URLs. `suite` defaults to `{codename}` and `components` to
  `["main"]`. `{codename}` is the machine's `VERSION_CODENAME` from `/etc/os-release`, and is the
  only placeholder. Only when the apt package is missing does apply download the key into
  `/etc/apt/keyrings/loadout-<id>.gpg` (`.asc` if it is ASCII-armored), write
  `/etc/apt/sources.list.d/loadout-<id>.list` signed by only that key, run `apt-get update` for
  that list alone, then install. A machine that already has the package is never touched, and a
  repository some other source already names is left as it is, since apt refuses two `signed-by`
  keys for one repository. The
  files stay after an uninstall or rollback, like the package. A PPA works the same way, with its
  `ppa.launchpadcontent.net` URL and its key from `keyserver.ubuntu.com`.
- A program can also come from a GitHub release, tried after every manager, so a machine with a
  declared manager uses that instead. Naming the repository is the trust:

  ```toml
  [programs.t3]
  github = { repo = "pingdotgg/t3code", tag = "v*-nightly.*", prerelease = true, bin = "t3", asset = { macos-aarch64 = "t3-*-darwin-arm64.tar.gz", linux-x86_64 = "t3-*-linux-x64.tar.gz" } }
  ```

  `asset` maps `<os>-<arch>` (`linux` or `macos`, then `x86_64` or `aarch64`) to a file name with
  `*` for any text; a machine with no key for it is `unsupported`. Apply takes the newest release
  (GitHub's order) that isn't a draft, is stable unless `prerelease = true`, has a tag matching
  `tag` if given, and has a matching asset. It refuses an asset without GitHub's sha256 digest,
  and checks the download against it. The suffix decides the install, always per user:

  | Asset | Installs to | Installed when |
  | --- | --- | --- |
  | `.AppImage` (linux) | `~/.local/bin/<id>`, with `~/.local/share/applications/loadout-<id>.desktop` | that file exists |
  | `.tar.gz` / `.tgz` | unpacked to `~/.local/share/loadout/github/<id>`, `bin` linked from `~/.local/bin` | `~/.local/bin/<bin>` exists |
  | `.zip` (macos) | `<app>.app` from inside it, to `~/Applications` | `~/Applications/<app>.app` or `/Applications/<app>.app` exists |

  Plan reads only those paths, so it stays offline. Loadout never looks at a newer release: the
  program updates itself, as Electron apps and self-updating CLIs do.
- Per-user managers run directly. System-wide ones (`apt`, `dnf`, `pacman`, `zypper`, `snap`,
  `choco`, machine-scoped winget installers) are `blocked` unless `loadout apply --elevated` is
  passed. The background sync never elevates.
- A program with no available manager is `unsupported` on that machine.
- `apply` and the background sync never install a package manager, and never run an install
  script. The only downloads are an `apt-repo` signing key and a `github` release asset. The install scripts set up Homebrew on macOS and Linux, and winget, Scoop, and Chocolatey
  (as an administrator) on Windows, before the first init.

## Validation

`loadout validate [DIR]` checks a loadout without touching the machine. Beyond the schema, it
resolves every listed machine (and an unlisted one) on macOS, Linux, and Windows, on both
architectures, and checks:

- sources exist inside the loadout, and skill folders hold `SKILL.md`
- targets are clean, don't collide (after `~/` and `{config}/` are expanded), and avoid reserved
  places
- each item is complete in the layer that adds it, and each MCP server has one transport
- every agent named anywhere is built in or declared, and at most one instructions item reaches
  each agent
- every template parses

Each problem is reported once, with the machines and OSes it affects. Warnings (such as a tag file
no machine lists, or a plaintext credential) don't fail validation.
[`tests/foundation/invalid/`](../tests/foundation/invalid) collects the mistakes validate catches.
