mod collide;
mod doctor;
mod table;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::context::{Context, windows_problem};
use crate::diagnostic::{Check, Diagnostic, Diagnostics, Severity};
use crate::manifest::{CustomAgent, EnvSyntax, McpFormat, McpShape, PerOs};
use crate::mcp;
use crate::plan::{Action, Content, Desired, ItemKind, ItemRef, Target};
use crate::platform::Os;
use crate::reserved::{self, Merge, Use};
use crate::resolve::{Agent, Resolved};
use crate::source::Sources;

use table::{Capability, McpPlace};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "{} problem(s) stop the agent items for this machine; nothing was written",
        .0.count(Severity::Error)
    )]
    Invalid(Diagnostics),
}

/// Turns the skills, instructions, and MCP servers that reach each enabled agent into what
/// should be on disk, plus an unsupported action for each one an agent can't take. `loadout` is
/// the active checkout: every destination is checked against the reserved table (which reserves
/// the checkout) before any source is read, and sources must stay inside it. Reads sources and
/// stats; never writes.
pub fn materialize(
    resolved: &Resolved,
    context: &Context,
    loadout: &Path,
) -> Result<(Vec<Desired>, Vec<Action>), Error> {
    let sources = Sources::new(context, loadout)
        .map_err(|message| Error::Invalid(one(String::new(), message)))?;
    let mut out = Out {
        context,
        loadout,
        desired: Vec::new(),
        unsupported: Vec::new(),
        problems: Diagnostics::default(),
    };
    let agents: BTreeMap<&str, Destinations> = resolved
        .agents
        .iter()
        .map(|agent| {
            let needs_mcp = resolved
                .mcp
                .values()
                .any(|server| server.agents.iter().any(|id| id == agent.id()));
            (agent.id(), destinations(agent, context, loadout, needs_mcp))
        })
        .collect();

    for (id, skill) in &resolved.skills {
        let mut tree = None;
        for agent in &skill.agents {
            let item = item_ref(ItemKind::Skill, id, agent);
            let dir = match &agents[agent.as_str()].skills {
                Ok(dir) => dir,
                Err(reason) => {
                    out.unsupported(item, Target::None, reason.clone());
                    continue;
                }
            };
            let path = dir.join(id);
            if context.os == Os::Windows
                && let Some(problem) = windows_problem(id)
            {
                out.unsupported(item, Target::Path { path }, problem);
                continue;
            }
            if let Some(reason) = out.reserved(&path, Use::Write) {
                out.unsupported(item, Target::Path { path }, reason);
                continue;
            }
            let read = tree.get_or_insert_with(|| {
                out.source(format!("skills.{id}"), sources.tree(&skill.source))
            });
            if let Some(tree) = read {
                out.desired(item, Target::Path { path }, Content::Tree(tree.clone()));
            }
        }
    }

    for (id, instructions) in &resolved.instructions {
        let mut file = None;
        for agent in &instructions.agents {
            let item = item_ref(ItemKind::Instructions, id, agent);
            let path = match &agents[agent.as_str()].instructions {
                Ok(path) => path.clone(),
                Err(reason) => {
                    out.unsupported(item, Target::None, reason.clone());
                    continue;
                }
            };
            if let Some(reason) = out.reserved(&path, Use::Write) {
                out.unsupported(item, Target::Path { path }, reason);
                continue;
            }
            let read = file.get_or_insert_with(|| {
                let copy = sources.file(&instructions.source).map(|mut copy| {
                    copy.executable = false;
                    copy
                });
                out.source(format!("instructions.{id}"), copy)
            });
            if let Some(copy) = read {
                let copy = Content::File(copy.clone());
                out.desired(item, Target::Path { path }, copy);
            }
        }
    }

    for (id, server) in &resolved.mcp {
        for agent in &server.agents {
            let item = item_ref(ItemKind::Mcp, id, agent);
            let place = match &agents[agent.as_str()].mcp {
                Ok(place) => place,
                Err(reason) => {
                    out.unsupported(item, Target::None, reason.clone());
                    continue;
                }
            };
            let target = Target::Entry {
                file: place.file.clone(),
                format: place.format,
                pointer: place.map.clone(),
                key: id.clone(),
                shape: place.shape,
            };
            let merge = Use::KeyMerge(Merge {
                map: &place.map,
                format: place.format.id(),
                shape: place.shape.id(),
            });
            if let Some(reason) = out.reserved(&place.file, merge) {
                out.unsupported(item, target, reason);
                continue;
            }
            match mcp::entry(server, place.shape, place.env) {
                Ok(value) => out.desired(item, target, Content::Entry(value)),
                Err(unsupported) => out.unsupported(item, target, unsupported.to_string()),
            }
        }
    }

    let writes = planned_writes(resolved, context, &out.desired);
    out.problems.extend(collide::check(context.os, &writes));
    if out.problems.has_errors() {
        return Err(Error::Invalid(out.problems));
    }
    Ok((out.desired, out.unsupported))
}

/// Doctor's agent checks. They stat and list folder names only: no file is opened, nothing is
/// written, and T3's own settings are never read.
pub fn checks(context: &Context) -> Vec<Check> {
    doctor::checks(context)
}

struct Out<'a> {
    context: &'a Context,
    loadout: &'a Path,
    desired: Vec<Desired>,
    unsupported: Vec<Action>,
    problems: Diagnostics,
}

impl Out<'_> {
    fn reserved(&self, path: &Path, how: Use<'_>) -> Option<String> {
        reserved::check_path(self.context, self.loadout, path, how)
            .map(|conflict| format!("{} {}", path.display(), conflict.message()))
    }

    fn source<T>(&mut self, key: String, read: Result<T, String>) -> Option<T> {
        match read {
            Ok(value) => Some(value),
            Err(message) => {
                self.problems.extend(one(key, message));
                None
            }
        }
    }

    fn desired(&mut self, item: ItemRef, target: Target, content: Content) {
        self.desired.push(Desired {
            item,
            target,
            content,
        });
    }

    fn unsupported(&mut self, item: ItemRef, target: Target, reason: String) {
        self.unsupported
            .push(Action::unsupported(item, target, reason));
    }
}

fn one(key: String, message: String) -> Diagnostics {
    let mut diagnostics = Diagnostics::default();
    diagnostics.push(Diagnostic {
        severity: Severity::Error,
        file: None,
        key: (!key.is_empty()).then_some(key),
        message,
    });
    diagnostics
}

fn item_ref(kind: ItemKind, id: &str, agent: &str) -> ItemRef {
    ItemRef {
        kind,
        id: id.to_owned(),
        agent: Some(agent.to_owned()),
    }
}

/// Where one enabled agent takes each kind of item on this machine, or why it can't.
struct Destinations {
    skills: Result<PathBuf, String>,
    instructions: Result<PathBuf, String>,
    mcp: Result<Mcp, String>,
}

struct Mcp {
    file: PathBuf,
    format: McpFormat,
    map: String,
    shape: McpShape,
    env: EnvSyntax,
}

fn destinations(agent: &Agent, context: &Context, loadout: &Path, needs_mcp: bool) -> Destinations {
    match agent {
        Agent::Builtin { agent } => {
            let adapter = table::adapter(*agent, context.os);
            let place = |capability: Capability<table::Place>| match capability {
                Capability::Supported(place) => Ok(place.expand(context)),
                Capability::Unsupported(reason) => Err(reason.to_owned()),
            };
            Destinations {
                skills: place(adapter.skills),
                instructions: place(adapter.instructions),
                mcp: match adapter.mcp {
                    Capability::Supported(place) => {
                        Ok(builtin_mcp(place, context, loadout, needs_mcp))
                    }
                    Capability::Unsupported(reason) => Err(reason.to_owned()),
                },
            }
        }
        Agent::Custom { declaration, .. } => custom(declaration, context),
    }
}

/// The opencode adapter reads `opencode.jsonc` as well as `opencode.json`; loadout writes the one
/// the user already keeps. The other name is statted only after its own reserved check passes.
fn builtin_mcp(place: McpPlace, context: &Context, loadout: &Path, needs_mcp: bool) -> Mcp {
    let mut file = place.file.expand(context);
    let mut format = place.format;
    if needs_mcp && let Some((existing, existing_format)) = place.existing {
        let path = existing.expand(context);
        let merge = Use::KeyMerge(Merge {
            map: place.map,
            format: existing_format.id(),
            shape: place.shape.id(),
        });
        if reserved::check_path(context, loadout, &path, merge).is_none()
            && fs::symlink_metadata(&path).is_ok()
        {
            file = path;
            format = existing_format;
        }
    }
    Mcp {
        file,
        format,
        map: place.map.to_owned(),
        shape: place.shape,
        env: place.env,
    }
}

/// A custom agent gets exactly what it declares; a kind or OS it leaves out is unsupported.
fn custom(declaration: &CustomAgent, context: &Context) -> Destinations {
    let mcp = match &declaration.mcp {
        None => Err("its declaration has no mcp table".to_owned()),
        Some(mcp) => declared(Some(&mcp.paths()), "mcp", context).map(|file| Mcp {
            file,
            format: mcp.format,
            map: mcp.map.clone(),
            shape: mcp.shape,
            env: mcp.env_syntax,
        }),
    };
    Destinations {
        skills: declared(declaration.skills.as_ref(), "skills", context),
        instructions: declared(declaration.instructions.as_ref(), "instructions", context),
        mcp,
    }
}

fn declared(paths: Option<&PerOs>, kind: &str, context: &Context) -> Result<PathBuf, String> {
    let os = context.os;
    let paths = paths.ok_or_else(|| format!("its declaration has no {kind} table"))?;
    let path = paths
        .get(os)
        .ok_or_else(|| format!("its declaration names no {kind} path for {os}"))?;
    context
        .expand(path)
        .ok_or_else(|| format!("{path} is not a valid path on {os}"))
}

fn planned_writes(
    resolved: &Resolved,
    context: &Context,
    desired: &[Desired],
) -> Vec<collide::Write> {
    let files = resolved.files.iter().filter_map(|(id, file)| {
        Some(collide::Write {
            owner: format!("files.{id}"),
            key: format!("files.{id}.target"),
            path: context.expand(&file.target)?,
            entry: None,
            file_item: true,
        })
    });
    let items = desired.iter().filter_map(|desired| {
        let section = match desired.item.kind {
            ItemKind::Skill => "skills",
            ItemKind::Instructions => "instructions",
            ItemKind::Mcp => "mcp",
            ItemKind::File | ItemKind::Program => return None,
        };
        let id = &desired.item.id;
        let agent = desired.item.agent.as_deref().unwrap_or_default();
        let (path, entry) = match &desired.target {
            Target::Path { path } => (path.clone(), None),
            Target::Entry {
                file,
                format,
                pointer,
                key,
                shape,
            } => (
                file.clone(),
                Some(collide::Entry {
                    format: *format,
                    pointer: pointer.clone(),
                    key: key.clone(),
                    shape: *shape,
                }),
            ),
            Target::Program { .. } | Target::None => return None,
        };
        Some(collide::Write {
            owner: format!("{section}.{id} for {agent}"),
            key: format!("{section}.{id}"),
            path,
            entry,
            file_item: false,
        })
    });
    files.chain(items).collect()
}
