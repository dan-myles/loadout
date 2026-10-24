use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::manifest::{BuiltinAgent, EnvSyntax, McpFormat, McpShape};
use crate::platform::Os;

/// The folder a place hangs off. An override root is read only through `Context::agent_dir`, so
/// `LOADOUT_HOME` isolation ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Root {
    Home,
    Override {
        var: &'static str,
        default: &'static str,
    },
    XdgConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Place {
    pub(super) root: Root,
    pub(super) path: &'static str,
}

impl Place {
    pub(super) fn expand(self, context: &Context) -> PathBuf {
        let base = match self.root {
            Root::Home => context.home.clone(),
            Root::Override { var, default } => context
                .agent_dir(var)
                .map_or_else(|| join(&context.home, default), Path::to_path_buf),
            Root::XdgConfig => context.xdg_config.clone(),
        };
        join(&base, self.path)
    }
}

fn join(base: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct McpPlace {
    pub(super) file: Place,
    pub(super) format: McpFormat,
    /// Written instead of `file` when it already exists.
    pub(super) existing: Option<(Place, McpFormat)>,
    pub(super) map: &'static str,
    pub(super) shape: McpShape,
    pub(super) env: EnvSyntax,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Capability<T> {
    Supported(T),
    Unsupported(&'static str),
}

/// Whether a T3-launched process for this driver reads the native user paths loadout syncs.
/// Syncing a path and T3 picking it up are separate claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pickup {
    Yes(&'static str),
    Partial(&'static str),
    Inferred(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Adapter {
    pub(super) skills: Capability<Place>,
    pub(super) instructions: Capability<Place>,
    pub(super) mcp: Capability<McpPlace>,
    /// Other skill folders the agent also loads, so one skill can reach it twice.
    pub(super) also_reads_skills: &'static [Place],
    /// Other instruction files the agent also loads on top of its own.
    pub(super) also_reads_instructions: &'static [Place],
    pub(super) pickup: Pickup,
    /// The Windows paths come from home-directory joins in the agent's source, not its docs.
    pub(super) windows_inferred: bool,
    pub(super) evidence: &'static str,
}

const CLAUDE_DIR: Root = Root::Override {
    var: "CLAUDE_CONFIG_DIR",
    default: ".claude",
};
const CLAUDE_ROOT: Root = Root::Override {
    var: "CLAUDE_CONFIG_DIR",
    default: "",
};
const CODEX_HOME: Root = Root::Override {
    var: "CODEX_HOME",
    default: ".codex",
};
const GROK_HOME: Root = Root::Override {
    var: "GROK_HOME",
    default: ".grok",
};
const PI_DIR: Root = Root::Override {
    var: "PI_CODING_AGENT_DIR",
    default: ".pi/agent",
};
const GEMINI_HOME: Root = Root::Override {
    var: "GEMINI_HOME",
    default: ".gemini",
};

const fn at(root: Root, path: &'static str) -> Place {
    Place { root, path }
}

const AGENTS_SKILLS: Place = at(Root::Home, ".agents/skills");
const CLAUDE_HOME_SKILLS: Place = at(Root::Home, ".claude/skills");
const CLAUDE_HOME_INSTRUCTIONS: Place = at(Root::Home, ".claude/CLAUDE.md");
const CURSOR_SKILLS: Place = at(Root::Home, ".cursor/skills");
const CODEX_HOME_SKILLS: Place = at(CODEX_HOME, "skills");
const DOT_CODEX_SKILLS: Place = at(Root::Home, ".codex/skills");
const GEMINI_CONFIG_SKILLS: Place = at(GEMINI_HOME, "config/skills");

/// Codex loads this instead of `AGENTS.md` when it exists.
pub(super) const CODEX_OVERRIDE: Place = at(CODEX_HOME, "AGENTS.override.md");

const MUSE_WINDOWS: &str = "Muse documents no Windows paths";

pub(super) fn adapter(agent: BuiltinAgent, os: Os) -> Adapter {
    match agent {
        BuiltinAgent::Claude => Adapter {
            skills: Capability::Supported(at(CLAUDE_DIR, "skills")),
            instructions: Capability::Supported(at(CLAUDE_DIR, "CLAUDE.md")),
            mcp: Capability::Supported(McpPlace {
                file: at(CLAUDE_ROOT, ".claude.json"),
                format: McpFormat::Json,
                existing: None,
                map: "/mcpServers",
                shape: McpShape::ClaudeJson,
                env: EnvSyntax::DollarBrace,
            }),
            also_reads_skills: &[],
            also_reads_instructions: &[],
            pickup: Pickup::Yes(
                "T3 Claude without an instance home runs the CLI with its default setting sources; an instance home sets CLAUDE_CONFIG_DIR to that folder instead",
            ),
            windows_inferred: false,
            evidence: "code.claude.com/docs/en/skills and /memory; T3 ClaudeHome.ts",
        },
        BuiltinAgent::Codex => Adapter {
            skills: Capability::Supported(AGENTS_SKILLS),
            instructions: Capability::Supported(at(CODEX_HOME, "AGENTS.md")),
            mcp: Capability::Supported(McpPlace {
                file: at(CODEX_HOME, "config.toml"),
                format: McpFormat::Toml,
                existing: None,
                map: "/mcp_servers",
                shape: McpShape::CodexToml,
                env: EnvSyntax::CodexEnvVars,
            }),
            also_reads_skills: &[CODEX_HOME_SKILLS],
            also_reads_instructions: &[],
            pickup: Pickup::Yes(
                "T3's default Codex shadow home links every shared entry of the Codex home; an instance with a custom home reads that home instead",
            ),
            windows_inferred: true,
            evidence: "codex ext/skills host_roots.rs, core agents_md.rs; T3 CodexHomeLayout.ts",
        },
        BuiltinAgent::Cursor => Adapter {
            skills: Capability::Supported(CURSOR_SKILLS),
            instructions: Capability::Unsupported(
                "Cursor keeps User Rules in its app settings, not in a file",
            ),
            mcp: Capability::Supported(McpPlace {
                file: at(Root::Home, ".cursor/mcp.json"),
                format: McpFormat::Json,
                existing: None,
                map: "/mcpServers",
                shape: McpShape::CursorJson,
                env: EnvSyntax::DollarBraceEnv,
            }),
            also_reads_skills: &[AGENTS_SKILLS, CLAUDE_HOME_SKILLS, DOT_CODEX_SKILLS],
            also_reads_instructions: &[],
            pickup: Pickup::Inferred(
                "T3 passes the user setting source to the Cursor SDK, which does not document what that source reads",
            ),
            windows_inferred: true,
            evidence: "cursor.com/docs/skills, /context/rules, /mcp; T3 CURSOR_AGENT_SETTING_SOURCES",
        },
        BuiltinAgent::Grok => Adapter {
            skills: Capability::Supported(at(GROK_HOME, "skills")),
            instructions: Capability::Supported(at(GROK_HOME, "AGENTS.md")),
            mcp: Capability::Supported(McpPlace {
                file: at(GROK_HOME, "config.toml"),
                format: McpFormat::Toml,
                existing: None,
                map: "/mcp_servers",
                shape: McpShape::GrokToml,
                env: EnvSyntax::DollarBrace,
            }),
            also_reads_skills: &[CLAUDE_HOME_SKILLS, CURSOR_SKILLS],
            also_reads_instructions: &[CLAUDE_HOME_INSTRUCTIONS],
            pickup: Pickup::Yes("T3 Grok inherits GROK_HOME or uses ~/.grok"),
            windows_inferred: false,
            evidence: "grok-build 2bdd1d6a docs/user-guide/08-skills.md, prompt/agents_md.rs, xai-dirs",
        },
        BuiltinAgent::Muse => {
            let (skills, mcp, also): (_, _, &'static [Place]) = if os == Os::Windows {
                (
                    Capability::Unsupported(MUSE_WINDOWS),
                    Capability::Unsupported(MUSE_WINDOWS),
                    &[],
                )
            } else {
                (
                    Capability::Supported(at(Root::XdgConfig, "muse/skills")),
                    Capability::Supported(McpPlace {
                        file: at(Root::XdgConfig, "muse/settings.json"),
                        format: McpFormat::Json,
                        existing: None,
                        map: "/mcp_servers",
                        shape: McpShape::MuseJson,
                        env: EnvSyntax::DollarBrace,
                    }),
                    &[AGENTS_SKILLS, CLAUDE_HOME_SKILLS, CODEX_HOME_SKILLS],
                )
            };
            Adapter {
                skills,
                instructions: Capability::Unsupported(
                    "Muse loads machine-wide user rules but documents no path for them",
                ),
                mcp,
                also_reads_skills: also,
                also_reads_instructions: &[],
                pickup: Pickup::Yes(
                    "T3 only strips META_API_KEY and turns off Muse auto-update",
                ),
                windows_inferred: false,
                evidence: "dev.meta.ai/docs/muse-code/configuration and /extending; T3 provider-muse sdk.ts",
            }
        }
        BuiltinAgent::Pi => Adapter {
            skills: Capability::Supported(at(PI_DIR, "skills")),
            instructions: Capability::Supported(at(PI_DIR, "AGENTS.md")),
            mcp: Capability::Supported(McpPlace {
                file: at(PI_DIR, "mcp.json"),
                format: McpFormat::Json,
                existing: None,
                map: "/mcpServers",
                shape: McpShape::PiJson,
                env: EnvSyntax::DollarBrace,
            }),
            also_reads_skills: &[AGENTS_SKILLS],
            also_reads_instructions: &[],
            pickup: Pickup::Yes("T3 Pi honours PI_CODING_AGENT_DIR"),
            windows_inferred: true,
            evidence: "pi-mono docs/skills.md, docs/configuration.md, src/core/skills.ts",
        },
        BuiltinAgent::Opencode => Adapter {
            skills: Capability::Supported(at(Root::XdgConfig, "opencode/skills")),
            instructions: Capability::Supported(at(Root::XdgConfig, "opencode/AGENTS.md")),
            mcp: Capability::Supported(McpPlace {
                file: at(Root::XdgConfig, "opencode/opencode.json"),
                format: McpFormat::Json,
                existing: Some((
                    at(Root::XdgConfig, "opencode/opencode.jsonc"),
                    McpFormat::Jsonc,
                )),
                map: "/mcp",
                shape: McpShape::OpencodeJson,
                env: EnvSyntax::BraceEnv,
            }),
            also_reads_skills: &[AGENTS_SKILLS, CLAUDE_HOME_SKILLS],
            also_reads_instructions: &[],
            pickup: Pickup::Yes(
                "T3 adds OPENCODE_CONFIG_CONTENT as an extra layer, so the global config still loads",
            ),
            windows_inferred: false,
            evidence: "opencode c272a63 skills.mdx, rules.mdx, config; T3 OpenCodeRuntime.ts",
        },
        BuiltinAgent::Antigravity => Adapter {
            skills: Capability::Supported(at(GEMINI_HOME, "antigravity-cli/skills")),
            instructions: Capability::Supported(at(GEMINI_HOME, "AGENTS.md")),
            mcp: Capability::Supported(McpPlace {
                file: at(GEMINI_HOME, "config/mcp_config.json"),
                format: McpFormat::Json,
                existing: None,
                map: "/mcpServers",
                shape: McpShape::AntigravityJson,
                env: EnvSyntax::None,
            }),
            also_reads_skills: &[GEMINI_CONFIG_SKILLS],
            also_reads_instructions: &[],
            pickup: Pickup::Partial(
                "T3 gives Antigravity a private GEMINI_HOME and links only the user skill folders into it, so T3 sessions see synced skills but not AGENTS.md or config/mcp_config.json; agy run outside T3 sees all three",
            ),
            windows_inferred: true,
            evidence: "antigravity.google/docs/cli/gcli-migration, /rules, /mcp; T3 AntigravitySkills.ts, prepareAntigravityProfile",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reserved::{self, Merge, Use};

    fn contexts(home: &Path) -> Vec<Context> {
        Os::ALL
            .into_iter()
            .map(|os| Context::resolve(os, |_| None, Some(home.to_path_buf())).unwrap())
            .collect()
    }

    fn free(context: &Context, checkout: &Path, path: &Path, how: Use<'_>, agent: BuiltinAgent) {
        let conflict = reserved::check_path(context, checkout, path, how);
        assert_eq!(conflict, None, "{agent:?} on {}: {}", context.os, path.display());
    }

    #[test]
    fn every_default_destination_passes_the_reserved_table_with_its_own_merge() {
        let home = tempfile::tempdir().unwrap();
        let checkout = home.path().join("checkout");
        for context in contexts(home.path()) {
            for agent in BuiltinAgent::ALL {
                let found = adapter(agent, context.os);
                let check = |path: &Path, how| free(&context, &checkout, path, how, agent);
                if let Capability::Supported(place) = found.skills {
                    check(&place.expand(&context).join("review"), Use::Write);
                }
                if let Capability::Supported(place) = found.instructions {
                    check(&place.expand(&context), Use::Write);
                }
                if let Capability::Supported(mcp) = found.mcp {
                    let files = [(mcp.file, mcp.format)].into_iter().chain(mcp.existing);
                    for (place, format) in files {
                        let merge = Merge {
                            map: mcp.map,
                            format: format.id(),
                            shape: mcp.shape.id(),
                        };
                        check(&place.expand(&context), Use::KeyMerge(merge));
                    }
                }
            }
        }
    }

    #[test]
    fn doctor_probes_only_unreserved_places() {
        let home = tempfile::tempdir().unwrap();
        let checkout = home.path().join("checkout");
        for context in contexts(home.path()) {
            for agent in BuiltinAgent::ALL {
                let found = adapter(agent, context.os);
                let own = [found.skills, found.instructions]
                    .into_iter()
                    .filter_map(|capability| match capability {
                        Capability::Supported(place) => Some(place),
                        Capability::Unsupported(_) => None,
                    });
                let probed = own
                    .chain(found.also_reads_skills.iter().copied())
                    .chain(found.also_reads_instructions.iter().copied())
                    .chain([CODEX_OVERRIDE]);
                for place in probed {
                    free(&context, &checkout, &place.expand(&context), Use::WriteInto, agent);
                }
            }
        }
    }

    #[test]
    fn muse_alone_lacks_windows_paths_and_cursor_and_muse_lack_instructions() {
        for agent in BuiltinAgent::ALL {
            for os in Os::ALL {
                let found = adapter(agent, os);
                let muse_on_windows = agent == BuiltinAgent::Muse && os == Os::Windows;
                assert_eq!(
                    matches!(found.skills, Capability::Unsupported(_)),
                    muse_on_windows,
                    "{agent:?} skills on {os}"
                );
                assert_eq!(
                    matches!(found.mcp, Capability::Unsupported(_)),
                    muse_on_windows,
                    "{agent:?} mcp on {os}"
                );
                assert_eq!(
                    matches!(found.instructions, Capability::Unsupported(_)),
                    matches!(agent, BuiltinAgent::Cursor | BuiltinAgent::Muse),
                    "{agent:?} instructions on {os}"
                );
            }
        }
    }
}
