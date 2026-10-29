use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::merge::{Draft, Edits, Item, McpDraft, Merged, SourceDraft};
use super::paths::{self, Write};
use super::{Agent, File, Instructions, McpServer, Program, Resolved, Skill, Transport, report};
use crate::diagnostic::{Diagnostics, Severity};
use crate::machine::Facts;
use crate::manifest::{BuiltinAgent, Loadout};

const TEMPLATE_SUFFIX: &str = ".tmpl";

pub(super) fn finish(
    loadout: &Loadout,
    facts: &Facts,
    merged: Merged,
    diagnostics: &mut Diagnostics,
) -> Resolved {
    let agents = agents(&merged, diagnostics);
    let enabled: Vec<String> = agents.iter().map(|agent| agent.id().to_owned()).collect();
    for (id, item) in &merged.mcp {
        transport_problems(id, item, diagnostics);
    }

    let root = loadout.root.as_path();
    let mut reach = Reach {
        enabled: &enabled,
        diagnostics,
    };

    let files: BTreeMap<String, (File, Option<&Path>)> = present(&merged.files, facts)
        .filter_map(|(id, item)| {
            let (Some(source), Some(target)) = (&item.draft.source, &item.draft.target) else {
                return None;
            };
            let file = File {
                source: source_path(root, source),
                target: target.clone(),
                template: source.ends_with(TEMPLATE_SUFFIX),
            };
            Some((id.clone(), (file, item.file())))
        })
        .collect();

    let skills: BTreeMap<String, Skill> = present(&merged.skills, facts)
        .filter_map(|(id, item)| {
            let (source, agents) = reach.sourced("skills", id, item, &item.draft.0)?;
            let source = source_path(root, source);
            Some((id.clone(), Skill { source, agents }))
        })
        .collect();
    let instructions: BTreeMap<String, Instructions> = present(&merged.instructions, facts)
        .filter_map(|(id, item)| {
            let (source, agents) = reach.sourced("instructions", id, item, &item.draft.0)?;
            let source = source_path(root, source);
            Some((id.clone(), Instructions { source, agents }))
        })
        .collect();
    let mcp: BTreeMap<String, McpServer> = present(&merged.mcp, facts)
        .filter_map(|(id, item)| {
            let agents = reach.agents(&format!("mcp.{id}"), item.file(), &item.draft.agents);
            Some((
                id.clone(),
                McpServer {
                    transport: transport(&item.draft)?,
                    agents,
                },
            ))
        })
        .collect();
    let programs = present(&merged.programs, facts)
        .map(|(id, item)| {
            let program = Program {
                packages: item.draft.packages.clone(),
                apt_repo: item.draft.apt_repo.clone(),
            };
            (id.clone(), program)
        })
        .collect();

    one_instructions_per_agent(&merged, &instructions, diagnostics);

    let writes = writes(
        &merged,
        &files,
        &skills,
        &instructions,
        &mcp,
        &agents,
        facts,
    );
    paths::check(facts.os, &writes, diagnostics);

    Resolved {
        tags: merged.tags,
        variables: merged.variables,
        update: merged.update,
        agents,
        files: files
            .into_iter()
            .map(|(id, (file, _))| (id, file))
            .collect(),
        skills,
        instructions,
        mcp,
        programs,
    }
}

fn present<'a, D: Draft>(
    items: &'a BTreeMap<String, Item<D>>,
    facts: &'a Facts,
) -> impl Iterator<Item = (&'a String, &'a Item<D>)> {
    items.iter().filter(|(_, item)| {
        item.draft
            .when()
            .is_none_or(|condition| condition.matches(facts))
    })
}

fn source_path(root: &Path, source: &str) -> PathBuf {
    source
        .split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part))
}

fn agents(merged: &Merged, diagnostics: &mut Diagnostics) -> Vec<Agent> {
    let mut agents: Vec<Agent> = Vec::new();
    for id in merged.enabled.finish(&[]) {
        if agents.iter().any(|agent| agent.id() == id) {
            continue;
        }
        if let Some(agent) = BuiltinAgent::from_id(&id) {
            agents.push(Agent::Builtin { agent });
        } else if let Some((declaration, _)) = merged.custom.get(&id) {
            agents.push(Agent::Custom {
                id,
                declaration: Box::new(declaration.clone()),
            });
        } else {
            report(
                diagnostics,
                Severity::Error,
                merged.enabled_file.as_deref(),
                "agents.enabled",
                format!(
                    "enables {id:?}, but no layer that applies to this machine declares [agents.custom.{id}]"
                ),
            );
        }
    }
    agents
}

fn transport_problems(id: &str, item: &Item<McpDraft>, diagnostics: &mut Diagnostics) {
    let draft = &item.draft;
    let stdio_fields = draft.args.is_set() || draft.env.is_some();
    let problem = if draft.command.is_some() && draft.url.is_some() {
        Some("has both command (stdio) and url (remote)")
    } else if draft.url.is_some() && stdio_fields {
        Some("is a url server but carries stdio args or env")
    } else if draft.command.is_some() && draft.headers.is_some() {
        Some("is a stdio server but carries url headers")
    } else {
        None
    };
    if let Some(problem) = problem {
        report(
            diagnostics,
            Severity::Error,
            item.file(),
            format!("mcp.{id}"),
            format!(
                "{problem} after merging {}; to switch transports, write replace = true with the complete server in the narrower layer",
                item.history()
            ),
        );
    }
}

fn transport(draft: &McpDraft) -> Option<Transport> {
    match (&draft.command, &draft.url) {
        (Some(command), None) => Some(Transport::Stdio {
            command: command.clone(),
            args: draft.args.finish(&[]),
            env: draft.env.clone().unwrap_or_default(),
        }),
        (None, Some(url)) => Some(Transport::Http {
            url: url.clone(),
            headers: draft.headers.clone().unwrap_or_default(),
        }),
        _ => None,
    }
}

struct Reach<'a> {
    enabled: &'a [String],
    diagnostics: &'a mut Diagnostics,
}

impl Reach<'_> {
    fn sourced<'d, D>(
        &mut self,
        kind: &str,
        id: &str,
        item: &Item<D>,
        draft: &'d SourceDraft,
    ) -> Option<(&'d str, Vec<String>)> {
        let agents = self.agents(&format!("{kind}.{id}"), item.file(), &draft.agents);
        Some((draft.source.as_deref()?, agents))
    }

    fn agents(&mut self, key: &str, file: Option<&Path>, list: &Edits<String>) -> Vec<String> {
        if !list.is_set() {
            if self.enabled.is_empty() {
                report(
                    self.diagnostics,
                    Severity::Warning,
                    file,
                    key,
                    "reaches no agent, because no agent is enabled for this machine ([agents] enabled)",
                );
            }
            return self.enabled.to_vec();
        }
        let mut agents: Vec<String> = Vec::new();
        for agent in list.finish(self.enabled) {
            if agents.contains(&agent) {
                continue;
            }
            if self.enabled.contains(&agent) {
                agents.push(agent);
            } else {
                report(
                    self.diagnostics,
                    Severity::Warning,
                    file,
                    format!("{key}.agents"),
                    format!(
                        "names {agent:?}, which is not enabled for this machine, so it gets nothing"
                    ),
                );
            }
        }
        agents
    }
}

fn one_instructions_per_agent(
    merged: &Merged,
    instructions: &BTreeMap<String, Instructions>,
    diagnostics: &mut Diagnostics,
) {
    let mut by_agent: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, item) in instructions {
        for agent in &item.agents {
            by_agent.entry(agent).or_default().push(id);
        }
    }
    let mut by_items: BTreeMap<Vec<&str>, Vec<&str>> = BTreeMap::new();
    for (agent, ids) in by_agent {
        if ids.len() > 1 {
            by_items.entry(ids).or_default().push(agent);
        }
    }
    for (ids, agents) in by_items {
        let last = ids.last().expect("two or more ids");
        let named = ids
            .iter()
            .map(|id| format!("instructions.{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        report(
            diagnostics,
            Severity::Error,
            merged.instructions[*last].file(),
            format!("instructions.{last}"),
            format!(
                "{named} all reach {}; an agent takes one instructions file, so narrow them with agents = [...]",
                agents.join(", ")
            ),
        );
    }
}

fn writes(
    merged: &Merged,
    files: &BTreeMap<String, (File, Option<&Path>)>,
    skills: &BTreeMap<String, Skill>,
    instructions: &BTreeMap<String, Instructions>,
    mcp: &BTreeMap<String, McpServer>,
    agents: &[Agent],
    facts: &Facts,
) -> Vec<Write> {
    let rank = |file: Option<&Path>| {
        file.and_then(|file| merged.applied.iter().position(|applied| applied == file))
    };
    let mut writes: Vec<Write> = files
        .iter()
        .map(|(id, (file, layer))| Write {
            owner: format!("files.{id}"),
            key: format!("files.{id}.target"),
            path: file.target.clone(),
            file: layer.map(Path::to_path_buf),
            rank: rank(*layer),
            merge: None,
        })
        .collect();
    for agent in agents {
        let Agent::Custom { id, declaration } = agent else {
            continue;
        };
        let file = merged.custom.get(id).map(|(_, file)| file.clone());
        let rank = rank(file.as_deref());
        let key = format!("agents.custom.{id}");
        let reaches = |agents: &Vec<String>| agents.contains(id);
        let mut push = |owner: String, key: String, path: String, merge| {
            writes.push(Write {
                owner,
                key,
                path,
                file: file.clone(),
                rank,
                merge,
            });
        };
        if let Some(dir) = declaration
            .skills
            .as_ref()
            .and_then(|paths| paths.get(facts.os))
        {
            for (skill, _) in skills.iter().filter(|(_, skill)| reaches(&skill.agents)) {
                push(
                    format!("skills.{skill} for {id}"),
                    format!("{key}.skills.{}", facts.os),
                    format!("{dir}/{skill}"),
                    None,
                );
            }
        }
        if let Some(path) = declaration
            .instructions
            .as_ref()
            .and_then(|paths| paths.get(facts.os))
            && let Some((item, _)) = instructions
                .iter()
                .find(|(_, instructions)| reaches(&instructions.agents))
        {
            push(
                format!("instructions.{item} for {id}"),
                format!("{key}.instructions.{}", facts.os),
                path.to_owned(),
                None,
            );
        }
        if let Some(declared) = &declaration.mcp
            && let Some(path) = declared.paths().get(facts.os).map(str::to_owned)
            && mcp.values().any(|server| reaches(&server.agents))
        {
            push(
                format!("the MCP servers for {id}"),
                format!("{key}.mcp.{}", facts.os),
                path,
                Some(declared.format),
            );
        }
    }
    writes
}
