use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::manifest::{Manager, McpFormat};

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
    },
    Program {
        manager: Manager,
        package: String,
    },
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub actions: Vec<Action>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
