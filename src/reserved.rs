use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::Serialize;

use crate::context::{Base, Context, split_base};
use crate::platform::Os;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Auth,
    SharedConfig,
    LoadoutState,
    Private,
}

impl Kind {
    pub fn reason(self) -> &'static str {
        match self {
            Kind::Auth => "an auth or OAuth store, which loadout never reads, copies, or writes",
            Kind::SharedConfig => {
                "an agent-owned shared config that loadout only key-merges MCP servers into"
            }
            Kind::LoadoutState => "loadout's own state, identity, or clone",
            Kind::Private => "a private or generated folder that loadout never touches",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchor {
    Home,
    ClaudeRoot,
    ClaudeDir,
    CodexHome,
    GrokHome,
    PiDir,
    GeminiHome,
    XdgConfig,
    XdgData,
    State,
}

impl Anchor {
    fn defaults(self) -> &'static [&'static str] {
        match self {
            Anchor::Home | Anchor::ClaudeRoot => &[""],
            Anchor::ClaudeDir => &[".claude"],
            Anchor::CodexHome => &[".codex"],
            Anchor::GrokHome => &[".grok"],
            Anchor::PiDir => &[".pi/agent"],
            Anchor::GeminiHome => &[".gemini"],
            Anchor::XdgConfig => &[".config"],
            Anchor::XdgData => &[".local/share"],
            Anchor::State => &[
                ".local/state/loadout",
                "Library/Application Support/loadout",
                "AppData/Local/loadout",
            ],
        }
    }

    fn roots(self, context: &Context) -> Vec<PathBuf> {
        let home = |relative: &str| join(&context.home, relative);
        let overridable = |var: &str, default: &str| {
            context
                .agent_dir(var)
                .map(Path::to_path_buf)
                .into_iter()
                .chain([home(default)])
                .collect()
        };
        let state_default = match context.os {
            Os::Linux => ".local/state/loadout",
            Os::Macos => "Library/Application Support/loadout",
            Os::Windows => "AppData/Local/loadout",
        };
        let mut roots: Vec<PathBuf> = match self {
            Anchor::Home => vec![context.home.clone()],
            Anchor::ClaudeRoot => overridable("CLAUDE_CONFIG_DIR", ""),
            Anchor::ClaudeDir => overridable("CLAUDE_CONFIG_DIR", ".claude"),
            Anchor::CodexHome => overridable("CODEX_HOME", ".codex"),
            Anchor::GrokHome => overridable("GROK_HOME", ".grok"),
            Anchor::PiDir => overridable("PI_CODING_AGENT_DIR", ".pi/agent"),
            Anchor::GeminiHome => overridable("GEMINI_HOME", ".gemini"),
            Anchor::XdgConfig => vec![context.xdg_config.clone(), home(".config")],
            Anchor::XdgData => vec![context.xdg_data.clone(), home(".local/share")],
            Anchor::State => vec![context.state.clone(), home(state_default)],
        };
        roots.dedup();
        roots
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub anchor: Anchor,
    pub path: &'static str,
    pub kind: Kind,
    pub merge: Option<Merge<'static>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Merge<'a> {
    pub map: &'a str,
    pub format: &'a str,
    pub shape: &'a str,
}

const fn entry(anchor: Anchor, path: &'static str, kind: Kind) -> Entry {
    Entry {
        anchor,
        path,
        kind,
        merge: None,
    }
}

const fn shared(
    anchor: Anchor,
    path: &'static str,
    map: &'static str,
    format: &'static str,
    shape: &'static str,
) -> Entry {
    Entry {
        anchor,
        path,
        kind: Kind::SharedConfig,
        merge: Some(Merge { map, format, shape }),
    }
}

pub const TABLE: &[Entry] = &[
    entry(Anchor::ClaudeDir, ".credentials.json", Kind::Auth),
    entry(Anchor::ClaudeDir, "mcp-needs-auth-cache.json", Kind::Auth),
    entry(Anchor::ClaudeDir, "backups", Kind::Private),
    shared(
        Anchor::ClaudeRoot,
        ".claude.json",
        "/mcpServers",
        "json",
        "claude-json",
    ),
    entry(Anchor::CodexHome, "auth.json", Kind::Auth),
    entry(Anchor::CodexHome, ".credentials.json", Kind::Auth),
    entry(Anchor::CodexHome, "mcp-oauth-locks", Kind::Auth),
    shared(
        Anchor::CodexHome,
        "config.toml",
        "/mcp_servers",
        "toml",
        "codex-toml",
    ),
    entry(Anchor::Home, ".cursor/auth.json", Kind::Auth),
    entry(Anchor::Home, ".config/cursor/auth.json", Kind::Auth),
    entry(Anchor::Home, "AppData/Roaming/Cursor/auth.json", Kind::Auth),
    shared(
        Anchor::Home,
        ".cursor/mcp.json",
        "/mcpServers",
        "json",
        "cursor-json",
    ),
    entry(Anchor::GrokHome, "auth.json", Kind::Auth),
    entry(Anchor::GrokHome, "mcp_credentials.json", Kind::Auth),
    entry(Anchor::GrokHome, ".config-init.lock", Kind::Private),
    shared(
        Anchor::GrokHome,
        "config.toml",
        "/mcp_servers",
        "toml",
        "grok-toml",
    ),
    entry(Anchor::XdgData, "opencode", Kind::Auth),
    shared(
        Anchor::XdgConfig,
        "opencode/opencode.json",
        "/mcp",
        "json",
        "opencode-json",
    ),
    shared(
        Anchor::XdgConfig,
        "opencode/opencode.jsonc",
        "/mcp",
        "jsonc",
        "opencode-json",
    ),
    entry(Anchor::PiDir, "auth.json", Kind::Auth),
    entry(Anchor::PiDir, "mcp-auth.json", Kind::Auth),
    shared(Anchor::PiDir, "mcp.json", "/mcpServers", "json", "pi-json"),
    entry(Anchor::XdgConfig, "muse/auth.json", Kind::Auth),
    entry(Anchor::XdgConfig, "muse/trust.json", Kind::Auth),
    shared(
        Anchor::XdgConfig,
        "muse/settings.json",
        "/mcp_servers",
        "json",
        "muse-json",
    ),
    entry(
        Anchor::GeminiHome,
        "antigravity/mcp_oauth_tokens.json",
        Kind::Auth,
    ),
    entry(Anchor::GeminiHome, "antigravity-cli/mcp", Kind::Private),
    shared(
        Anchor::GeminiHome,
        "config/mcp_config.json",
        "/mcpServers",
        "json",
        "antigravity-json",
    ),
    entry(Anchor::Home, ".t3", Kind::Private),
    entry(Anchor::Home, ".ssh", Kind::Auth),
    entry(Anchor::XdgConfig, "gh/hosts.yml", Kind::Auth),
    entry(
        Anchor::Home,
        "AppData/Roaming/GitHub CLI/hosts.yml",
        Kind::Auth,
    ),
    entry(Anchor::Home, ".git-credentials", Kind::Auth),
    entry(Anchor::XdgConfig, "git/credentials", Kind::Auth),
    entry(Anchor::Home, ".netrc", Kind::Auth),
    entry(Anchor::Home, "_netrc", Kind::Auth),
    entry(Anchor::Home, ".aws/credentials", Kind::Auth),
    entry(Anchor::State, "", Kind::LoadoutState),
    entry(Anchor::XdgConfig, "loadout", Kind::LoadoutState),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use<'a> {
    Write,
    WriteInto,
    KeyMerge(Merge<'a>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    Reserved { entry: Entry, reserved: String },
    Unverifiable { path: PathBuf, error: String },
}

impl Conflict {
    pub fn message(&self) -> String {
        match self {
            Conflict::Reserved { entry, reserved } => {
                format!("overlaps {reserved}, {}", entry.kind.reason())
            }
            Conflict::Unverifiable { path, error } => format!(
                "cannot verify {} is not a reserved destination ({error}); refusing it",
                path.display()
            ),
        }
    }

    pub fn kind(&self) -> Option<Kind> {
        match self {
            Conflict::Reserved { entry, .. } => Some(entry.kind),
            Conflict::Unverifiable { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Relation {
    Equal,
    Inside,
    Ancestor,
}

fn relation(target: &[String], reserved: &[String]) -> Option<Relation> {
    if target == reserved {
        Some(Relation::Equal)
    } else if target.len() > reserved.len() && target[..reserved.len()] == *reserved {
        Some(Relation::Inside)
    } else if reserved.len() > target.len() && reserved[..target.len()] == *target {
        Some(Relation::Ancestor)
    } else {
        None
    }
}

fn blocked(entry: &Entry, relation: Relation, how: Use<'_>) -> bool {
    match how {
        Use::Write => true,
        Use::WriteInto => relation != Relation::Ancestor,
        Use::KeyMerge(merge) => !(relation == Relation::Equal && entry.merge == Some(merge)),
    }
}

fn conflicts(target: &[String], reserved: &[String], entry: &Entry, how: Use<'_>) -> bool {
    relation(target, reserved).is_some_and(|relation| blocked(entry, relation, how))
}

pub fn check_literal(path: &str, how: Use<'_>) -> Option<Conflict> {
    let (base, rest) = split_base(path)?;
    let candidates: Vec<String> = match base {
        Base::Home => vec![rest.to_owned()],
        Base::Config => [".config", "Library/Application Support", "AppData/Roaming"]
            .iter()
            .map(|dir| format!("{dir}/{rest}"))
            .collect(),
    };
    TABLE.iter().find_map(|entry| {
        entry.anchor.defaults().iter().find_map(|anchor| {
            let reserved = [*anchor, entry.path]
                .iter()
                .filter(|part| !part.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("/");
            candidates
                .iter()
                .any(|candidate| conflicts(&parts(candidate), &parts(&reserved), entry, how))
                .then(|| Conflict::Reserved {
                    entry: *entry,
                    reserved: format!("~/{reserved}"),
                })
        })
    })
}

const ACTIVE_LOADOUT: Entry = entry(Anchor::Home, "", Kind::LoadoutState);

pub fn check_path(
    context: &Context,
    loadout: &Path,
    path: &Path,
    how: Use<'_>,
) -> Option<Conflict> {
    let unverifiable = |path: &Path, error: io::Error| Conflict::Unverifiable {
        path: path.to_path_buf(),
        error: error.to_string(),
    };
    let targets = match normalize(path) {
        Ok(real) => [path_parts(path), path_parts(&real)],
        Err(error) => return Some(unverifiable(path, error)),
    };
    let active = (&ACTIVE_LOADOUT, loadout.to_path_buf());
    let table = TABLE.iter().flat_map(|entry| {
        entry
            .anchor
            .roots(context)
            .into_iter()
            .map(move |root| (entry, join(&root, entry.path)))
    });
    for (entry, reserved) in table.chain([active]) {
        let real = match normalize(&reserved) {
            Ok(real) => real,
            Err(error) => return Some(unverifiable(&reserved, error)),
        };
        let forms = [path_parts(&reserved), path_parts(&real)];
        let hit = targets
            .iter()
            .any(|target| forms.iter().any(|form| conflicts(target, form, entry, how)));
        if hit {
            return Some(Conflict::Reserved {
                entry: *entry,
                reserved: reserved.display().to_string(),
            });
        }
    }
    None
}

const MAX_LINKS: usize = 40;

pub fn normalize(path: &Path) -> io::Result<PathBuf> {
    let clean = path.is_absolute()
        && path
            .components()
            .all(|part| !matches!(part, Component::CurDir | Component::ParentDir));
    if !clean {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path is not absolute and clean",
        ));
    }
    resolve(path)
}

enum Step {
    Root(PathBuf),
    Name(OsString),
    Parent,
}

fn steps(path: &Path) -> Vec<Step> {
    let mut root = PathBuf::new();
    let mut steps = Vec::new();
    for part in path.components() {
        match part {
            Component::Prefix(_) | Component::RootDir => root.push(part),
            Component::CurDir => {}
            Component::ParentDir => steps.push(Step::Parent),
            Component::Normal(name) => steps.push(Step::Name(name.to_owned())),
        }
    }
    if !root.as_os_str().is_empty() {
        steps.insert(0, Step::Root(root));
    }
    steps
}

fn resolve(path: &Path) -> io::Result<PathBuf> {
    let mut queue: VecDeque<Step> = steps(path).into();
    let mut out = PathBuf::new();
    let mut links = MAX_LINKS;
    while let Some(step) = queue.pop_front() {
        let name = match step {
            Step::Root(root) => {
                out = root;
                continue;
            }
            Step::Parent => {
                out.pop();
                continue;
            }
            Step::Name(name) => name,
        };
        let candidate = out.join(&name);
        match fs::canonicalize(&candidate) {
            Ok(real) => out = real,
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            Err(error) => {
                if fs::symlink_metadata(&candidate).is_ok_and(|meta| meta.file_type().is_symlink())
                {
                    if links == 0 {
                        return Err(io::Error::other("too many levels of symbolic links"));
                    }
                    links -= 1;
                    let target = fs::read_link(&candidate)?;
                    for step in steps(&target).into_iter().rev() {
                        queue.push_front(step);
                    }
                    continue;
                }
                if queue.iter().any(|step| matches!(step, Step::Parent)) {
                    return Err(io::Error::new(
                        error.kind(),
                        "a .. follows a missing component, so the destination is unknowable",
                    ));
                }
                out.push(name);
                for step in queue.drain(..) {
                    match step {
                        Step::Name(name) => out.push(name),
                        Step::Root(root) => out = root,
                        Step::Parent => {}
                    }
                }
            }
        }
    }
    Ok(out)
}

fn parts(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn path_parts(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| match component {
            Component::Prefix(prefix) => prefix_name(prefix.kind()),
            other => other.as_os_str().to_string_lossy().to_ascii_lowercase(),
        })
        .collect()
}

fn prefix_name(prefix: Prefix<'_>) -> String {
    match prefix {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
            format!("{}:", char::from(drive).to_ascii_lowercase())
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
            "//{}/{}",
            server.to_string_lossy().to_ascii_lowercase(),
            share.to_string_lossy().to_ascii_lowercase()
        ),
        Prefix::Verbatim(name) | Prefix::DeviceNS(name) => {
            format!("//?/{}", name.to_string_lossy().to_ascii_lowercase())
        }
    }
}

fn join(base: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::platform::Os;

    fn abs(path: &str) -> String {
        if cfg!(windows) {
            format!("C:{path}")
        } else {
            path.to_owned()
        }
    }

    fn no_loadout() -> PathBuf {
        PathBuf::from(abs("/nonexistent/loadout-checkout"))
    }

    const CLAUDE_MCP: Use<'static> = Use::KeyMerge(Merge {
        map: "/mcpServers",
        format: "json",
        shape: "claude-json",
    });

    fn conflict(path: &str, how: Use<'_>) -> Option<Kind> {
        check_literal(path, how).and_then(|conflict| conflict.kind())
    }

    #[test]
    fn auth_stores_their_subtrees_and_ancestors_are_blocked() {
        for path in [
            "~/.codex/auth.json",
            "~/.claude/.credentials.json",
            "~/.claude/.Credentials.json",
            "~/.codex/mcp-oauth-locks/x",
            "~/.local/share/opencode/mcp-auth.json",
            "~/.codex",
            "~/.claude",
            "{config}/muse/auth.json",
            "{config}/Cursor/auth.json",
        ] {
            assert_eq!(conflict(path, Use::Write), Some(Kind::Auth), "{path}");
        }
    }

    #[test]
    fn shared_configs_take_only_their_known_mcp_map_format_and_shape() {
        let merge = |map, format, shape| Use::KeyMerge(Merge { map, format, shape });
        for (path, ok) in [
            (
                "~/.claude.json",
                merge("/mcpServers", "json", "claude-json"),
            ),
            (
                "~/.codex/config.toml",
                merge("/mcp_servers", "toml", "codex-toml"),
            ),
            (
                "{config}/opencode/opencode.jsonc",
                merge("/mcp", "jsonc", "opencode-json"),
            ),
            (
                "~/.gemini/config/mcp_config.json",
                merge("/mcpServers", "json", "antigravity-json"),
            ),
        ] {
            assert_eq!(
                conflict(path, Use::Write),
                Some(Kind::SharedConfig),
                "{path}"
            );
            assert_eq!(conflict(path, ok), None, "{path}");
        }
        for bad in [
            merge("/oauthAccount", "json", "claude-json"),
            merge("/projects", "json", "claude-json"),
            merge("/mcpServers", "jsonc", "claude-json"),
            merge("/mcpServers", "json", "cursor-json"),
        ] {
            assert_eq!(
                conflict("~/.claude.json", bad),
                Some(Kind::SharedConfig),
                "{bad:?}"
            );
        }
        assert_eq!(
            conflict(
                "~/.codex/config.toml",
                merge("/projects", "toml", "codex-toml")
            ),
            Some(Kind::SharedConfig)
        );
        assert_eq!(conflict("~/.codex/auth.json", CLAUDE_MCP), Some(Kind::Auth));
        assert_eq!(conflict("~/.claude", CLAUDE_MCP), Some(Kind::Auth));
        assert_eq!(
            conflict(
                "~/.test/mcp.json",
                merge("/customMap", "json", "cursor-json")
            ),
            None
        );
    }

    #[test]
    fn loadout_state_and_private_folders_are_blocked() {
        for path in [
            "~/.local/state/loadout/state.json",
            "{config}/loadout/machine.toml",
            "~/AppData/Local/loadout",
            "~/.local/state",
        ] {
            assert_eq!(
                conflict(path, Use::Write),
                Some(Kind::LoadoutState),
                "{path}"
            );
        }
        assert_eq!(
            conflict("~/.t3/settings.json", Use::Write),
            Some(Kind::Private)
        );
    }

    #[test]
    fn siblings_and_skill_dirs_beside_protected_files_are_allowed() {
        for path in [
            "~/.claude/CLAUDE.md",
            "~/.codex/AGENTS.md",
            "~/.gitconfig",
            "~/.config/git/config",
            "{config}/Code/User/settings.json",
            "~/.claudette.json",
        ] {
            assert_eq!(conflict(path, Use::Write), None, "{path}");
        }
        assert_eq!(conflict("~/.claude/skills", Use::WriteInto), None);
        assert_eq!(conflict("~/.claude", Use::WriteInto), None);
        assert_eq!(
            conflict("~/.codex/mcp-oauth-locks", Use::WriteInto),
            Some(Kind::Auth)
        );
    }

    #[test]
    fn expanded_paths_follow_agent_overrides_and_the_real_state_dir() {
        let vars = |name: &str| match name {
            "CODEX_HOME" => Some(OsString::from(abs("/alt/codex"))),
            "CLAUDE_CONFIG_DIR" => Some(OsString::from(abs("/alt/claude"))),
            _ => None,
        };
        let context = Context::resolve(Os::Linux, vars, Some(abs("/h").into())).unwrap();
        let kind = |path: &str| {
            check_path(&context, &no_loadout(), Path::new(&abs(path)), Use::Write)
                .and_then(|conflict| conflict.kind())
        };
        assert_eq!(kind("/alt/codex/auth.json"), Some(Kind::Auth));
        assert_eq!(kind("/alt/claude/.claude.json"), Some(Kind::SharedConfig));
        assert_eq!(
            kind("/h/.local/state/loadout/state.json"),
            Some(Kind::LoadoutState)
        );
        assert_eq!(kind("/h/.codex/auth.json"), Some(Kind::Auth));
        assert_eq!(kind("/h/.gitconfig"), None);
    }

    #[cfg(unix)]
    #[test]
    fn both_sides_are_normalized_through_symlinks_and_missing_tails() {
        use std::os::unix::fs::symlink;

        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let at = |relative: &str| root.join(relative);
        for dir in [
            "home/.grok",
            "real/codex",
            "real/default-codex",
            "real/state",
            "vault",
        ] {
            fs::create_dir_all(at(dir)).unwrap();
        }
        fs::write(at("real/codex/auth.json"), "synthetic").unwrap();
        fs::write(at("vault/token"), "synthetic").unwrap();
        symlink(at("real/codex"), at("codex-link")).unwrap();
        symlink(at("real"), at("home/ordinary-alias")).unwrap();
        symlink(at("real/default-codex"), at("home/.codex")).unwrap();
        symlink(at("vault/token"), at("home/.grok/auth.json")).unwrap();
        symlink(at("real/state"), at("state-link")).unwrap();
        symlink(at("home/.codex/auth.json"), at("home/dangling")).unwrap();

        let codex = at("codex-link").into_os_string();
        let state = at("state-link").into_os_string();
        let vars = move |name: &str| match name {
            "CODEX_HOME" => Some(codex.clone()),
            "LOADOUT_STATE_DIR" => Some(state.clone()),
            _ => None,
        };
        let context = Context::resolve(Os::Linux, vars, Some(at("home"))).unwrap();
        let kind = |relative: &str| {
            check_path(&context, &no_loadout(), &at(relative), Use::Write)
                .and_then(|conflict| conflict.kind())
        };

        assert_eq!(
            kind("home/ordinary-alias/codex/auth.json"),
            Some(Kind::Auth)
        );
        assert_eq!(kind("real/codex/auth.json"), Some(Kind::Auth));
        assert_eq!(kind("real/default-codex/auth.json"), Some(Kind::Auth));
        assert_eq!(kind("vault/token"), Some(Kind::Auth));
        assert_eq!(kind("vault"), Some(Kind::Auth));
        assert_eq!(kind("home/dangling"), Some(Kind::Auth));
        assert_eq!(
            kind("home/ordinary-alias/codex/mcp-oauth-locks/new/file"),
            Some(Kind::Auth)
        );
        assert_eq!(kind("real/state/state.json"), Some(Kind::LoadoutState));

        assert_eq!(kind("real/codex/AGENTS.md"), None);
        assert_eq!(kind("home/.gitconfig"), None);
        assert_eq!(kind("vault-sibling/file"), None);
        assert_eq!(
            check_path(
                &context,
                &no_loadout(),
                &at("home/ordinary-alias/codex/skills"),
                Use::WriteInto
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn metadata_errors_fail_closed() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = tempfile::tempdir().unwrap();
        let locked = scratch.path().join("locked");
        fs::create_dir_all(locked.join("inner")).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let context =
            Context::resolve(Os::Linux, |_| None, Some(scratch.path().join("home"))).unwrap();
        let result = check_path(
            &context,
            &no_loadout(),
            &locked.join("inner/file"),
            Use::Write,
        );
        let readable = fs::read_dir(&locked).is_ok();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            eprintln!("running with permission to read a 000 folder (root?); skipped");
            return;
        }
        assert!(
            matches!(result, Some(Conflict::Unverifiable { .. })),
            "{result:?}"
        );
        assert!(
            check_path(&context, &no_loadout(), Path::new("relative/x"), Use::Write).is_some(),
            "unclean paths fail closed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_and_plain_drive_prefixes_compare_equal() {
        assert_eq!(
            path_parts(Path::new(r"\\?\C:\Users\a\.codex\auth.json")),
            path_parts(Path::new(r"c:\Users\a\.codex\auth.json"))
        );
    }

    #[test]
    fn prefix_names_fold_verbatim_forms() {
        assert_eq!(
            prefix_name(Prefix::VerbatimDisk(b'C')),
            prefix_name(Prefix::Disk(b'c'))
        );
        assert_eq!(
            prefix_name(Prefix::VerbatimUNC("Srv".as_ref(), "Share".as_ref())),
            prefix_name(Prefix::UNC("srv".as_ref(), "share".as_ref()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_active_loadout_checkout_is_reserved_wherever_it_lives() {
        use std::os::unix::fs::symlink;

        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let checkout = root.join("src/my-loadout");
        fs::create_dir_all(checkout.join("files")).unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        symlink(&checkout, root.join("home/alias")).unwrap();
        let context = Context::resolve(Os::Linux, |_| None, Some(root.join("home"))).unwrap();
        let kind = |path: PathBuf, how: Use<'_>| {
            check_path(&context, &checkout, &path, how).and_then(|conflict| conflict.kind())
        };
        for path in [
            checkout.join("loadout.toml"),
            checkout.join("files/new"),
            root.join("src"),
            root.join("home/alias/loadout.toml"),
        ] {
            assert_eq!(
                kind(path.clone(), Use::Write),
                Some(Kind::LoadoutState),
                "{path:?}"
            );
        }
        assert_eq!(
            kind(checkout.clone(), Use::WriteInto),
            Some(Kind::LoadoutState)
        );
        assert_eq!(kind(root.join("src"), Use::WriteInto), None);
        assert_eq!(kind(root.join("src/other-project/x"), Use::Write), None);
        assert_eq!(
            kind(checkout.join("mcp.json"), CLAUDE_MCP),
            Some(Kind::LoadoutState)
        );
    }

    #[test]
    fn xdg_data_and_gemini_home_move_their_protected_files() {
        let vars = |name: &str| match name {
            "XDG_DATA_HOME" => Some(OsString::from(abs("/alt/data"))),
            "GEMINI_HOME" => Some(OsString::from(abs("/alt/gemini"))),
            _ => None,
        };
        let context = Context::resolve(Os::Linux, vars, Some(abs("/h").into())).unwrap();
        let kind = |path: &str| {
            check_path(&context, &no_loadout(), Path::new(&abs(path)), Use::Write)
                .and_then(|conflict| conflict.kind())
        };
        assert_eq!(kind("/alt/data/opencode/mcp-auth.json"), Some(Kind::Auth));
        assert_eq!(
            kind("/h/.local/share/opencode/mcp-auth.json"),
            Some(Kind::Auth)
        );
        assert_eq!(
            kind("/alt/gemini/antigravity/mcp_oauth_tokens.json"),
            Some(Kind::Auth)
        );
        assert_eq!(
            kind("/alt/gemini/config/mcp_config.json"),
            Some(Kind::SharedConfig)
        );
        assert_eq!(
            kind("/h/.gemini/antigravity/mcp_oauth_tokens.json"),
            Some(Kind::Auth)
        );
        assert_eq!(kind("/alt/data/other-app/state"), None);
        assert_eq!(kind("/alt/gemini/AGENTS.md"), None);

        let isolated = Context::resolve(
            Os::Linux,
            |name| match name {
                "LOADOUT_HOME" => Some(OsString::from(abs("/sandbox"))),
                other => vars(other),
            },
            Some(abs("/h").into()),
        )
        .unwrap();
        let isolated_kind = |path: &str| {
            check_path(&isolated, &no_loadout(), Path::new(&abs(path)), Use::Write)
                .and_then(|conflict| conflict.kind())
        };
        assert_eq!(isolated_kind("/alt/data/opencode/mcp-auth.json"), None);
        assert_eq!(
            isolated_kind("/sandbox/.local/share/opencode/mcp-auth.json"),
            Some(Kind::Auth)
        );
    }

    #[test]
    fn well_known_non_agent_credentials_are_reserved_but_their_siblings_are_not() {
        for path in [
            "~/.ssh/id_ed25519",
            "~/.ssh/config",
            "~/.ssh",
            "~/.SSH/known_hosts",
            "{config}/gh/hosts.yml",
            "~/AppData/Roaming/GitHub CLI/hosts.yml",
            "~/.git-credentials",
            "{config}/git/credentials",
            "~/.netrc",
            "~/_netrc",
            "~/.aws/credentials",
            "~/.aws",
        ] {
            assert_eq!(conflict(path, Use::Write), Some(Kind::Auth), "{path}");
        }
        for path in [
            "~/.gitconfig",
            "~/.config/git/ignore",
            "{config}/gh/config.yml",
            "~/.aws/config",
            "~/.sshrc",
            "~/.netrc.d/x",
        ] {
            assert_eq!(conflict(path, Use::Write), None, "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn credential_aliases_are_caught_through_links() {
        use std::os::unix::fs::symlink;

        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("dotfiles/ssh")).unwrap();
        symlink(root.join("dotfiles/ssh"), root.join("home/.ssh")).unwrap();
        let context = Context::resolve(Os::Linux, |_| None, Some(root.join("home"))).unwrap();
        let kind = |path: PathBuf| {
            check_path(&context, &no_loadout(), &path, Use::Write)
                .and_then(|conflict| conflict.kind())
        };
        assert_eq!(kind(root.join("dotfiles/ssh/id_ed25519")), Some(Kind::Auth));
        assert_eq!(kind(root.join("dotfiles/zshrc")), None);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_links_resolve_symlinks_before_parent_steps() {
        use std::os::unix::fs::symlink;

        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path().canonicalize().unwrap();
        let at = |relative: &str| root.join(relative);
        fs::create_dir_all(at("nested/else/deep")).unwrap();
        fs::create_dir_all(at("nested/else/missing")).unwrap();
        fs::create_dir_all(at("nested/home/.grok")).unwrap();
        symlink(at("nested/else/deep"), at("nested/jump")).unwrap();
        symlink(
            at("nested/jump/../missing/token"),
            at("nested/home/.grok/auth.json"),
        )
        .unwrap();
        symlink(
            at("nested/gone/../x"),
            at("nested/home/.grok/mcp_credentials.json"),
        )
        .unwrap();

        assert_eq!(
            normalize(&at("nested/home/.grok/auth.json")).unwrap(),
            at("nested/else/missing/token")
        );
        assert!(normalize(&at("nested/home/.grok/mcp_credentials.json")).is_err());

        let context = Context::resolve(Os::Linux, |_| None, Some(at("nested/home"))).unwrap();
        let conflict = check_path(
            &context,
            &no_loadout(),
            &at("nested/else/missing/token"),
            Use::Write,
        );
        assert!(
            conflict.is_some(),
            "the future auth destination must be refused"
        );

        fs::remove_file(at("nested/home/.grok/mcp_credentials.json")).unwrap();
        assert_eq!(
            check_path(
                &context,
                &no_loadout(),
                &at("nested/else/missing/token"),
                Use::Write
            )
            .and_then(|conflict| conflict.kind()),
            Some(Kind::Auth)
        );
        assert_eq!(
            check_path(
                &context,
                &no_loadout(),
                &at("nested/missing/token"),
                Use::Write
            ),
            None,
            "the lexically cancelled path is not the protected destination"
        );
    }
}
