use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use crate::manifest::{Manager, McpFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    File,
    Skill,
    Instructions,
    Mcp,
    Program,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ItemRef {
    pub kind: ItemKind,
    pub id: String,
    pub agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
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
    },
    Program {
        manager: Manager,
        package: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Bytes(Vec<u8>),
    Tree(BTreeMap<PathBuf, Vec<u8>>),
    Entry(serde_json::Value),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub item: ItemRef,
    pub target: Target,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    #[serde(skip)]
    pub content: Option<Content>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub actions: Vec<Action>,
}
