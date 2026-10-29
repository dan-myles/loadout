mod all;
mod finish;
mod merge;
mod paths;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::context::Context;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity};
use crate::machine::Facts;
use crate::manifest::{
    AptRepo, BuiltinAgent, CustomAgent, GithubRelease, Loadout, Manager, Value, Variable,
};

/// `Ok` carries warnings too, such as an item naming an agent this machine doesn't enable.
pub fn resolve(loadout: &Loadout, facts: &Facts) -> Result<(Resolved, Diagnostics), Diagnostics> {
    let mut diagnostics = Diagnostics::default();
    let merged = merge::merge(loadout, facts, &mut diagnostics);
    let resolved = finish::finish(loadout, facts, merged, &mut diagnostics);
    if diagnostics.has_errors() {
        Err(diagnostics)
    } else {
        Ok((resolved, diagnostics))
    }
}

/// Uses synthetic facts only, so what validate reports never depends on the machine running it.
pub fn check_all(loadout: &Loadout) -> Diagnostics {
    all::check_all(loadout)
}

/// `loadout` is the active checkout, which is reserved like loadout's own state. Only stats
/// paths, so a reserved file is never opened.
pub fn check_targets(resolved: &Resolved, context: &Context, loadout: &Path) -> Diagnostics {
    paths::check_targets(resolved, context, loadout)
}

fn report(
    diagnostics: &mut Diagnostics,
    severity: Severity,
    file: Option<&Path>,
    key: impl Into<String>,
    message: impl Into<String>,
) {
    let key = key.into();
    diagnostics.push(Diagnostic {
        severity,
        file: file.map(Path::to_path_buf),
        key: (!key.is_empty()).then_some(key),
        message: message.into(),
    });
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolved {
    pub tags: Vec<String>,
    pub variables: BTreeMap<String, Variable>,
    pub update: Update,
    pub agents: Vec<Agent>,
    pub files: BTreeMap<String, File>,
    pub skills: BTreeMap<String, Skill>,
    pub instructions: BTreeMap<String, Instructions>,
    pub mcp: BTreeMap<String, McpServer>,
    pub programs: BTreeMap<String, Program>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Update {
    pub auto: bool,
    pub interval_minutes: u32,
}

impl Default for Update {
    fn default() -> Self {
        Self {
            auto: true,
            interval_minutes: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Agent {
    Builtin {
        agent: BuiltinAgent,
    },
    Custom {
        id: String,
        declaration: Box<CustomAgent>,
    },
}

impl Agent {
    pub fn id(&self) -> &str {
        match self {
            Agent::Builtin { agent } => agent.id(),
            Agent::Custom { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct File {
    pub source: PathBuf,
    pub target: String,
    pub template: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    pub source: PathBuf,
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Instructions {
    pub source: PathBuf,
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServer {
    pub transport: Transport,
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, Value>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Program {
    pub packages: BTreeMap<Manager, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apt_repo: Option<AptRepo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github: Option<GithubRelease>,
}
