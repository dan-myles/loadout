use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::context::{Base, Context, split_base};

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
            Kind::Auth => {
                "an agent auth or OAuth store, which loadout never reads, copies, or writes"
            }
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
    XdgConfig,
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
            Anchor::XdgConfig => &[".config"],
            Anchor::State => &[
                ".local/state/loadout",
                "Library/Application Support/loadout",
                "AppData/Local/loadout",
            ],
        }
    }

    fn expand(self, context: &Context) -> PathBuf {
        let home_or = |var: &str, default: &str| {
            context
                .agent_dir(var)
                .map_or_else(|| join(&context.home, default), Path::to_path_buf)
        };
        match self {
            Anchor::Home => context.home.clone(),
            Anchor::ClaudeRoot => home_or("CLAUDE_CONFIG_DIR", ""),
            Anchor::ClaudeDir => home_or("CLAUDE_CONFIG_DIR", ".claude"),
            Anchor::CodexHome => home_or("CODEX_HOME", ".codex"),
            Anchor::GrokHome => home_or("GROK_HOME", ".grok"),
            Anchor::PiDir => home_or("PI_CODING_AGENT_DIR", ".pi/agent"),
            Anchor::XdgConfig => context.xdg_config.clone(),
            Anchor::State => context.state.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub anchor: Anchor,
    pub path: &'static str,
    pub kind: Kind,
}

const fn entry(anchor: Anchor, path: &'static str, kind: Kind) -> Entry {
    Entry { anchor, path, kind }
}

pub const TABLE: &[Entry] = &[
    entry(Anchor::ClaudeDir, ".credentials.json", Kind::Auth),
    entry(Anchor::ClaudeDir, "mcp-needs-auth-cache.json", Kind::Auth),
    entry(Anchor::ClaudeDir, "backups", Kind::Private),
    entry(Anchor::ClaudeRoot, ".claude.json", Kind::SharedConfig),
    entry(Anchor::CodexHome, "auth.json", Kind::Auth),
    entry(Anchor::CodexHome, ".credentials.json", Kind::Auth),
    entry(Anchor::CodexHome, "mcp-oauth-locks", Kind::Auth),
    entry(Anchor::CodexHome, "config.toml", Kind::SharedConfig),
    entry(Anchor::Home, ".cursor/auth.json", Kind::Auth),
    entry(Anchor::Home, ".config/cursor/auth.json", Kind::Auth),
    entry(Anchor::Home, "AppData/Roaming/Cursor/auth.json", Kind::Auth),
    entry(Anchor::Home, ".cursor/mcp.json", Kind::SharedConfig),
    entry(Anchor::GrokHome, "auth.json", Kind::Auth),
    entry(Anchor::GrokHome, "mcp_credentials.json", Kind::Auth),
    entry(Anchor::GrokHome, ".config-init.lock", Kind::Private),
    entry(Anchor::GrokHome, "config.toml", Kind::SharedConfig),
    entry(Anchor::Home, ".local/share/opencode", Kind::Auth),
    entry(
        Anchor::XdgConfig,
        "opencode/opencode.json",
        Kind::SharedConfig,
    ),
    entry(
        Anchor::XdgConfig,
        "opencode/opencode.jsonc",
        Kind::SharedConfig,
    ),
    entry(Anchor::PiDir, "auth.json", Kind::Auth),
    entry(Anchor::PiDir, "mcp-auth.json", Kind::Auth),
    entry(Anchor::PiDir, "mcp.json", Kind::SharedConfig),
    entry(Anchor::XdgConfig, "muse/auth.json", Kind::Auth),
    entry(Anchor::XdgConfig, "muse/trust.json", Kind::Auth),
    entry(Anchor::XdgConfig, "muse/settings.json", Kind::SharedConfig),
    entry(
        Anchor::Home,
        ".gemini/antigravity/mcp_oauth_tokens.json",
        Kind::Auth,
    ),
    entry(Anchor::Home, ".gemini/antigravity-cli/mcp", Kind::Private),
    entry(
        Anchor::Home,
        ".gemini/config/mcp_config.json",
        Kind::SharedConfig,
    ),
    entry(Anchor::Home, ".t3", Kind::Private),
    entry(Anchor::State, "", Kind::LoadoutState),
    entry(Anchor::XdgConfig, "loadout", Kind::LoadoutState),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Use {
    Write,
    WriteInto,
    KeyMerge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub entry: Entry,
    pub reserved: String,
}

impl Conflict {
    pub fn message(&self) -> String {
        format!("overlaps {}, {}", self.reserved, self.entry.kind.reason())
    }
}

fn blocks(kind: Kind, how: Use) -> bool {
    !(how == Use::KeyMerge && kind == Kind::SharedConfig)
}

pub fn check_literal(path: &str, how: Use) -> Option<Conflict> {
    let (base, rest) = split_base(path)?;
    let candidates: Vec<String> = match base {
        Base::Home => vec![rest.to_owned()],
        Base::Config => [".config", "Library/Application Support", "AppData/Roaming"]
            .iter()
            .map(|dir| format!("{dir}/{rest}"))
            .collect(),
    };
    TABLE
        .iter()
        .filter(|entry| blocks(entry.kind, how))
        .find_map(|entry| {
            entry.anchor.defaults().iter().find_map(|anchor| {
                let reserved = [*anchor, entry.path]
                    .iter()
                    .filter(|part| !part.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("/");
                candidates
                    .iter()
                    .any(|candidate| overlaps(&parts(candidate), &parts(&reserved), how))
                    .then(|| Conflict {
                        entry: *entry,
                        reserved: format!("~/{reserved}"),
                    })
            })
        })
}

pub fn check_path(context: &Context, path: &Path, how: Use) -> Option<Conflict> {
    TABLE
        .iter()
        .filter(|entry| blocks(entry.kind, how))
        .find_map(|entry| {
            let reserved = join(&entry.anchor.expand(context), entry.path);
            let reserved_parts = path_parts(&reserved);
            overlaps(&path_parts(path), &reserved_parts, how).then(|| Conflict {
                entry: *entry,
                reserved: reserved.display().to_string(),
            })
        })
}

fn overlaps(target: &[String], reserved: &[String], how: Use) -> bool {
    let inside = target.len() >= reserved.len() && target[..reserved.len()] == *reserved;
    let ancestor = reserved.len() > target.len() && reserved[..target.len()] == *target;
    inside || (ancestor && how != Use::WriteInto)
}

fn parts(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn path_parts(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
        .collect()
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

    fn conflict(path: &str, how: Use) -> Option<Kind> {
        check_literal(path, how).map(|conflict| conflict.entry.kind)
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
    fn shared_configs_are_blocked_except_for_key_merges() {
        for path in [
            "~/.claude.json",
            "~/.codex/config.toml",
            "{config}/opencode/opencode.jsonc",
            "~/.gemini/config/mcp_config.json",
        ] {
            assert_eq!(
                conflict(path, Use::Write),
                Some(Kind::SharedConfig),
                "{path}"
            );
            assert_eq!(conflict(path, Use::KeyMerge), None, "{path}");
        }
        assert_eq!(
            conflict("~/.codex/auth.json", Use::KeyMerge),
            Some(Kind::Auth)
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
            "CODEX_HOME" => Some(OsString::from("/alt/codex")),
            "CLAUDE_CONFIG_DIR" => Some(OsString::from("/alt/claude")),
            _ => None,
        };
        let context = Context::resolve(Os::Linux, vars, Some("/h".into())).unwrap();
        let kind = |path: &str| {
            check_path(&context, Path::new(path), Use::Write).map(|conflict| conflict.entry.kind)
        };
        assert_eq!(kind("/alt/codex/auth.json"), Some(Kind::Auth));
        assert_eq!(kind("/alt/claude/.claude.json"), Some(Kind::SharedConfig));
        assert_eq!(
            kind("/h/.local/state/loadout/state.json"),
            Some(Kind::LoadoutState)
        );
        assert_eq!(kind("/h/.codex/auth.json"), None);
        assert_eq!(kind("/h/.gitconfig"), None);
    }
}
