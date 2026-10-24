use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::table::{self, Capability, Pickup, Place};
use crate::context::{Context, HOME_OVERRIDE};
use crate::diagnostic::{Check, CheckStatus};
use crate::manifest::BuiltinAgent;
use crate::platform::Os;

pub(super) fn checks(context: &Context) -> Vec<Check> {
    let mut checks = vec![overrides(context)];
    checks.extend(
        BuiltinAgent::ALL
            .into_iter()
            .map(|agent| summary(agent, context)),
    );
    checks.extend(codex_override(context));
    checks.push(skill_duplicates(context));
    checks.extend(instruction_duplicates(context));
    checks.extend(mcp_alternatives(context));
    checks
}

fn overrides(context: &Context) -> Check {
    let detail = if context.isolated {
        format!(
            "ignored under {HOME_OVERRIDE}; every agent path derives from {}",
            context.home.display()
        )
    } else {
        let mut set: Vec<String> = context
            .agent_dirs
            .iter()
            .map(|(name, path)| format!("{name}={}", path.display()))
            .collect();
        if context.xdg_config != context.home.join(".config") {
            set.push(format!("XDG_CONFIG_HOME={}", context.xdg_config.display()));
        }
        if set.is_empty() {
            "none set; every agent uses its default folders".to_owned()
        } else {
            format!(
                "{}; loadout writes there, and an agent started without the same variable reads its default folder instead",
                set.join(", ")
            )
        }
    };
    Check::new("agent overrides", CheckStatus::Info, detail)
}

fn summary(agent: BuiltinAgent, context: &Context) -> Check {
    let adapter = table::adapter(agent, context.os);
    let place = |capability: Capability<Place>| match capability {
        Capability::Supported(place) => place.expand(context).display().to_string(),
        Capability::Unsupported(reason) => format!("unsupported ({reason})"),
    };
    let mcp = match adapter.mcp {
        Capability::Supported(mcp) => {
            let existing = mcp.existing.map_or_else(String::new, |(place, _)| {
                format!(", or {} when it exists", place.expand(context).display())
            });
            format!("{} {}{existing}", mcp.file.expand(context).display(), mcp.map)
        }
        Capability::Unsupported(reason) => format!("unsupported ({reason})"),
    };
    let pickup = match adapter.pickup {
        Pickup::Yes(note) => format!("T3 sessions read these paths ({note})"),
        Pickup::Partial(note) => format!("T3 sessions read only part of this: {note}"),
        Pickup::Inferred(note) => format!("T3 pickup is inferred, not proven: {note}"),
    };
    let windows = if context.os == Os::Windows && adapter.windows_inferred {
        "; these Windows paths are inferred from home-directory joins in its source"
    } else {
        ""
    };
    Check::new(
        &format!("agent {}", agent.id()),
        CheckStatus::Info,
        format!(
            "skills {}; instructions {}; mcp {mcp}; {pickup}{windows}; evidence: {}",
            place(adapter.skills),
            place(adapter.instructions),
            adapter.evidence,
        ),
    )
}

fn codex_override(context: &Context) -> Option<Check> {
    let path = table::CODEX_OVERRIDE.expand(context);
    exists(&path).then(|| {
        Check::new(
            "codex override",
            CheckStatus::Warn,
            format!(
                "{} exists, and Codex loads it instead of AGENTS.md, so instructions loadout writes for codex don't take effect",
                path.display()
            ),
        )
    })
}

/// A skill reaches an agent twice when two folders it loads hold the same name. Only folder
/// names are listed; nothing inside a skill is opened.
fn skill_duplicates(context: &Context) -> Check {
    let mut listed: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    let mut problems = Vec::new();
    let mut duplicates = Vec::new();
    for agent in BuiltinAgent::ALL {
        let adapter = table::adapter(agent, context.os);
        let own = match adapter.skills {
            Capability::Supported(place) => Some(place),
            Capability::Unsupported(_) => None,
        };
        let mut roots: Vec<PathBuf> = Vec::new();
        for place in own.iter().chain(adapter.also_reads_skills) {
            let root = place.expand(context);
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        for root in &roots {
            if !listed.contains_key(root) {
                let names = skill_names(root).unwrap_or_else(|error| {
                    problems.push(format!("cannot list {}: {error}", root.display()));
                    Vec::new()
                });
                listed.insert(root.clone(), names);
            }
        }
        let mut by_skill: BTreeMap<&str, Vec<&Path>> = BTreeMap::new();
        for root in &roots {
            for name in &listed[root] {
                by_skill.entry(name.as_str()).or_default().push(root);
            }
        }
        for (name, roots) in by_skill {
            if roots.len() > 1 {
                let roots: Vec<String> = roots
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect();
                duplicates.push(format!(
                    "{} loads {name} from {}",
                    agent.id(),
                    roots.join(" and ")
                ));
            }
        }
    }
    if duplicates.is_empty() && problems.is_empty() {
        return Check::new(
            "skill duplicates",
            CheckStatus::Pass,
            "no skill reaches an agent through two folders",
        );
    }
    duplicates.extend(problems);
    Check::new("skill duplicates", CheckStatus::Warn, duplicates.join("; "))
}

fn skill_names(root: &Path) -> io::Result<Vec<String>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.path().is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(names)
}

fn instruction_duplicates(context: &Context) -> Vec<Check> {
    BuiltinAgent::ALL
        .into_iter()
        .filter_map(|agent| {
            let adapter = table::adapter(agent, context.os);
            let Capability::Supported(own) = adapter.instructions else {
                return None;
            };
            let files: Vec<PathBuf> = [own]
                .iter()
                .chain(adapter.also_reads_instructions)
                .map(|place| place.expand(context))
                .filter(|path| exists(path))
                .collect();
            (files.len() > 1).then(|| {
                let files: Vec<String> = files
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect();
                Check::new(
                    &format!("{} instructions", agent.id()),
                    CheckStatus::Warn,
                    format!(
                        "{} loads all of {}, so the same text can reach it twice",
                        agent.id(),
                        files.join(" and ")
                    ),
                )
            })
        })
        .collect()
}

fn mcp_alternatives(context: &Context) -> Vec<Check> {
    BuiltinAgent::ALL
        .into_iter()
        .filter_map(|agent| {
            let Capability::Supported(mcp) = table::adapter(agent, context.os).mcp else {
                return None;
            };
            let (existing, _) = mcp.existing?;
            let (file, existing) = (mcp.file.expand(context), existing.expand(context));
            (exists(&file) && exists(&existing)).then(|| {
                Check::new(
                    &format!("{} config", agent.id()),
                    CheckStatus::Warn,
                    format!(
                        "both {} and {} exist and {} reads both; loadout merges MCP servers into {}",
                        file.display(),
                        existing.display(),
                        agent.id(),
                        existing.display()
                    ),
                )
            })
        })
        .collect()
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}
