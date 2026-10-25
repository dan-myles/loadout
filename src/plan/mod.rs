mod check;
mod compare;
mod files;
mod identity;
mod install;
mod volume;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agents;
use crate::context::Context;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity};
use crate::machine::{self, Facts};
use crate::manifest::{Loadout, Manager, McpFormat, McpShape};
use crate::programs;
use crate::resolve::{self, Program};
use crate::source::Sources;
use crate::state::{self, RunStatus, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    File,
    Skill,
    Instructions,
    Mcp,
    Program,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemRef {
    pub kind: ItemKind,
    pub id: String,
    pub agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Target {
    Path {
        path: PathBuf,
    },
    Entry {
        file: PathBuf,
        format: McpFormat,
        pointer: String,
        key: String,
        shape: McpShape,
    },
    Program {
        manager: Manager,
        package: String,
    },
    /// Only for an unsupported or blocked action whose item has no destination on this machine,
    /// such as Cursor instructions or a program under isolation. Never observed, owned, backed
    /// up, journaled, or written.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContent {
    pub bytes: Vec<u8>,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    File(FileContent),
    Tree(BTreeMap<PathBuf, FileContent>),
    Entry(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "observed", rename_all = "lowercase")]
pub enum Observed {
    Absent,
    Present { sha256: String },
    Installed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub item: ItemRef,
    pub target: Target,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", content = "reason", rename_all = "lowercase")]
pub enum Change {
    Create,
    Update,
    Unchanged,
    Adopt,
    Replace,
    Drifted,
    Remove,
    Install,
    Unsupported(String),
    Blocked(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Action {
    pub item: ItemRef,
    pub target: Target,
    #[serde(flatten)]
    pub change: Change,
    pub prior: Observed,
    #[serde(skip)]
    pub content: Option<Content>,
}

impl Action {
    /// The one shape an unsupported action takes: nothing was observed and nothing is written.
    pub fn unsupported(item: ItemRef, target: Target, reason: String) -> Self {
        Self {
            item,
            target,
            change: Change::Unsupported(reason),
            prior: Observed::Absent,
            content: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub actions: Vec<Action>,
}

impl ItemRef {
    /// The item as the loadout names it, such as `skills.review for codex`.
    pub fn label(&self) -> String {
        let key = compare::item_key(self);
        match &self.agent {
            Some(agent) => format!("{key} for {agent}"),
            None => key,
        }
    }
}

impl Change {
    pub fn name(&self) -> &'static str {
        match self {
            Change::Create => "create",
            Change::Update => "update",
            Change::Unchanged => "unchanged",
            Change::Adopt => "adopt",
            Change::Replace => "replace",
            Change::Drifted => "drifted",
            Change::Remove => "remove",
            Change::Install => "install",
            Change::Unsupported(_) => "unsupported",
            Change::Blocked(_) => "blocked",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Change::Unsupported(reason) | Change::Blocked(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

impl Plan {
    /// Whether applying this plan would leave something undone: a blocked action, or drift
    /// that only `--force` overwrites.
    pub fn blocked(&self) -> bool {
        self.actions
            .iter()
            .any(|action| matches!(action.change, Change::Blocked(_) | Change::Drifted))
    }
}

/// Where `plan`, `apply`, and `update` read the loadout from unless `--loadout` names another
/// checkout: loadout's own clone in the state dir.
pub fn default_checkout(context: &Context) -> PathBuf {
    context.state.join("loadout")
}

/// The checkout a command reads: `chosen`, or loadout's own clone, which must exist.
pub fn checkout(context: &Context, chosen: Option<&Path>) -> anyhow::Result<PathBuf> {
    let checkout = chosen.map_or_else(|| default_checkout(context), Path::to_path_buf);
    if !checkout.is_dir() {
        anyhow::bail!(
            "there is no loadout checkout at {}; pass --loadout DIR",
            checkout.display()
        );
    }
    Ok(checkout)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Whether a system-scope program install is planned as `install` rather than `blocked`.
    pub elevated: bool,
}

/// The program steps behind a plan's program actions. Only `build` makes them, from
/// `programs::steps`, so a caller can pass them back but never forge one.
#[derive(Debug, Clone)]
pub struct Steps {
    steps: BTreeMap<String, programs::Step>,
    programs: BTreeMap<String, Program>,
    facts: Facts,
    context: Context,
}

impl Steps {
    pub fn get(&self, program: &str) -> Option<&programs::Step> {
        self.steps.get(program)
    }

    /// Steps are reused only for the same declared programs, facts, and context; anything else
    /// asks the managers again.
    fn fits(&self, programs: &BTreeMap<String, Program>, facts: &Facts, context: &Context) -> bool {
        self.programs == *programs && self.facts == *facts && self.context == *context
    }
}

#[derive(Debug, Clone)]
pub struct Built {
    pub plan: Plan,
    pub steps: Steps,
    /// The canonical root of the checkout the plan was built from: the active checkout every
    /// reserved check and the journal use.
    pub checkout: PathBuf,
    pub machine: String,
    pub warnings: Diagnostics,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The loadout can't be applied to this machine: exit 3, no destination written.
    #[error(
        "{} problem(s) in the loadout for this machine; no destination was written",
        .0.count(Severity::Error)
    )]
    Invalid(Diagnostics),
    /// The machine is in a state loadout won't build on: exit 4, no destination written.
    #[error("{0}")]
    Blocked(String),
    #[error(transparent)]
    Runtime(#[from] anyhow::Error),
}

/// Builds the plan for this machine from `checkout`. It reads the checkout, the machine's
/// identity, the state, and the targets, and asks the package managers; it never writes, takes
/// the lock, or creates the state dir. `steps` from an earlier build are reused only when they
/// were made for the same programs, facts, and context.
pub fn build(
    context: &Context,
    checkout: &Path,
    state: &State,
    steps: Option<Steps>,
    options: Options,
) -> Result<Built, Error> {
    interrupted(context)?;
    let (loadout, mut warnings) = Loadout::load_guarded(checkout, context).map_err(Error::Invalid)?;
    let root = loadout.root.clone();
    let facts = machine::detect(context).map_err(|error| match error {
        machine::Error::Invalid { .. } => Error::Blocked(format!(
            "this machine's saved identity is invalid: {error}; doctor shows the details"
        )),
        other => Error::Runtime(anyhow::Error::new(other).context("cannot read this machine's facts")),
    })?;
    let (resolved, resolve_warnings) = resolve::resolve(&loadout, &facts).map_err(Error::Invalid)?;
    warnings.extend(resolve_warnings);

    let mut problems = Diagnostics::default();
    let sources = Sources::new(context, &root).map_err(|message| Error::Invalid(one("", message)))?;
    let mut produced = files::produce(&resolved, &facts, context, &root, &sources, &mut problems);
    match agents::materialize(&resolved, context, &root) {
        Ok((desired, unsupported)) => {
            produced.desired.extend(desired);
            produced.unsupported.extend(unsupported);
        }
        Err(agents::Error::Invalid(diagnostics)) => problems.extend(diagnostics),
    }
    if problems.has_errors() {
        return Err(Error::Invalid(problems));
    }

    let steps = match steps {
        Some(steps) if steps.fits(&resolved.programs, &facts, context) => steps,
        _ => Steps {
            steps: programs::steps(&resolved.programs, &facts, context)
                .map_err(program_error)?
                .into_iter()
                .map(|step| (step.program.clone(), step))
                .collect(),
            programs: resolved.programs.clone(),
            facts: facts.clone(),
            context: context.clone(),
        },
    };

    let mut actions = compare::actions(produced, state, context, &root, &mut problems)?;
    actions.extend(install::actions(&steps, options, &mut problems));
    if problems.has_errors() {
        return Err(Error::Invalid(problems));
    }
    let plan = Plan { actions };
    plan.check()
        .map_err(|message| Error::Runtime(anyhow::anyhow!("internal error: {message}")))?;
    Ok(Built {
        plan,
        steps,
        checkout: root,
        machine: facts.name,
        warnings,
    })
}

/// An interrupted run must be rolled back before anything new is planned on top of it, whatever
/// started the next run. A journal that can't be read can't show it isn't interrupted.
///
/// `state::runs` reports an unfinished newest run as in progress while the lock is held, and
/// under `apply`'s own lock it is held by this process. So in progress blocks too: outside the
/// lock another run is changing the machine, and under it the run was interrupted.
fn interrupted(context: &Context) -> Result<(), Error> {
    let runs = state::runs(context).map_err(|error| {
        Error::Runtime(anyhow::Error::new(error).context("cannot list loadout's runs"))
    })?;
    match runs.last() {
        Some(run)
            if matches!(
                run.status,
                Some(RunStatus::Interrupted | RunStatus::InProgress)
            ) =>
        {
            Err(Error::Blocked(format!(
                "run {} didn't finish; if no other loadout run is going, run `loadout rollback` to undo it before planning again",
                run.id
            )))
        }
        Some(run) if run.status.is_none() => Err(Error::Blocked(format!(
            "the journal of run {} can't be read ({}), so loadout can't tell whether it was interrupted",
            run.id,
            run.error.as_deref().unwrap_or("unknown error")
        ))),
        _ => Ok(()),
    }
}

fn program_error(error: programs::Error) -> Error {
    match error {
        programs::Error::Package { .. } => Error::Invalid(one("", error.to_string())),
        other => Error::Runtime(anyhow::Error::new(other)),
    }
}

fn one(key: &str, message: String) -> Diagnostics {
    let mut diagnostics = Diagnostics::default();
    diagnostics.push(error(key, message));
    diagnostics
}

fn error(key: &str, message: String) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        file: None,
        key: (!key.is_empty()).then(|| key.to_owned()),
        message,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::platform::{Arch, Os};

    #[test]
    fn steps_are_reused_only_for_the_same_programs_facts_and_context() {
        let context = Context::resolve(Os::Linux, |_| None, Some(PathBuf::from("/h"))).unwrap();
        let facts = Facts {
            name: "box".to_owned(),
            os: Os::Linux,
            distro: None,
            arch: Arch::X86_64,
            home: PathBuf::from("/h"),
        };
        let declared = BTreeMap::from([(
            "rg".to_owned(),
            Program {
                packages: BTreeMap::from([(Manager::Apt, "ripgrep".to_owned())]),
            },
        )]);
        let steps = Steps {
            steps: BTreeMap::new(),
            programs: declared.clone(),
            facts: facts.clone(),
            context: context.clone(),
        };
        assert!(steps.fits(&declared, &facts, &context));
        assert!(!steps.fits(&BTreeMap::new(), &facts, &context));
        let renamed = Facts {
            name: "other".to_owned(),
            ..facts.clone()
        };
        assert!(!steps.fits(&declared, &renamed, &context));
        let isolated = Context::resolve(
            Os::Linux,
            |name| {
                let isolated = if cfg!(windows) { "C:\\i" } else { "/i" };
                (name == "LOADOUT_HOME").then(|| OsString::from(isolated))
            },
            Some(PathBuf::from("/h")),
        )
        .unwrap();
        assert!(!steps.fits(&declared, &facts, &isolated));
    }

    #[test]
    fn action_json_carries_the_precondition_but_never_content() {
        let action = Action {
            item: ItemRef {
                kind: ItemKind::Skill,
                id: "review".to_owned(),
                agent: Some("codex".to_owned()),
            },
            target: Target::Path {
                path: PathBuf::from("/h/.agents/skills/review"),
            },
            change: Change::Replace,
            prior: Observed::Present {
                sha256: "ab".repeat(32),
            },
            content: Some(Content::Tree(BTreeMap::from([(
                PathBuf::from("scripts/run.sh"),
                FileContent {
                    bytes: b"secret-looking bytes".to_vec(),
                    executable: true,
                },
            )]))),
        };
        let json = serde_json::to_value(&action).unwrap();
        assert_eq!(json["change"], "replace");
        assert_eq!(json["prior"]["observed"], "present");
        assert_eq!(json["prior"]["sha256"], "ab".repeat(32));
        assert!(json.get("content").is_none());
        assert!(!json.to_string().contains("secret-looking"));
    }

    #[test]
    fn an_entry_target_names_its_shape_and_a_missing_destination_is_none() {
        let entry = Target::Entry {
            file: PathBuf::from("/h/.pi/agent/mcp.json"),
            format: McpFormat::Json,
            pointer: "/mcpServers".to_owned(),
            key: "docs".to_owned(),
            shape: McpShape::PiJson,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "entry");
        assert_eq!(json["shape"], "pi-json");
        assert_eq!(json["format"], "json");

        let action = Action::unsupported(
            ItemRef {
                kind: ItemKind::Instructions,
                id: "global".to_owned(),
                agent: Some("cursor".to_owned()),
            },
            Target::None,
            "no file".to_owned(),
        );
        assert_eq!(action.prior, Observed::Absent);
        assert!(action.content.is_none());
        let json = serde_json::to_value(&action).unwrap();
        assert_eq!(json["target"], serde_json::json!({ "type": "none" }));
        assert_eq!(json["change"], "unsupported");
        assert_eq!(json["reason"], "no file");
    }
}
