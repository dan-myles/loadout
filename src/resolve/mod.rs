use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::manifest::{BuiltinAgent, CustomAgent, Manager, Value, Variable};

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
    pub cli: bool,
}

impl Default for Update {
    fn default() -> Self {
        Self {
            auto: true,
            interval_minutes: 60,
            cli: true,
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
}
