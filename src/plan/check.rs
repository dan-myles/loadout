use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{Action, Change, Content, Observed, Plan, Target};
use crate::manifest::Manager;

/// What makes two concrete actions the same destination. `Target::None` has no identity, so any
/// number of valid `None` actions can share a plan.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Identity {
    Path(PathBuf),
    Entry(PathBuf, String, String),
    Program(Manager, String),
}

impl Plan {
    /// The shapes a built plan may hold. `build` checks its own result, and `apply` checks again
    /// before anything reaches state, so a forged or mistaken action never does.
    pub fn check(&self) -> Result<(), String> {
        let mut seen = BTreeSet::new();
        for action in &self.actions {
            shape(action).map_err(|problem| format!("{}: {problem}", describe(action)))?;
            let identity = match &action.target {
                Target::None => continue,
                Target::Path { path } => Identity::Path(path.clone()),
                Target::Entry {
                    file, pointer, key, ..
                } => Identity::Entry(file.clone(), pointer.clone(), key.clone()),
                Target::Program { manager, package } => {
                    Identity::Program(*manager, package.clone())
                }
            };
            if !seen.insert(identity) {
                return Err(format!("{} appears twice in the plan", describe(action)));
            }
        }
        Ok(())
    }
}

fn shape(action: &Action) -> Result<(), &'static str> {
    let reported = matches!(action.change, Change::Unsupported(_) | Change::Blocked(_));
    match &action.target {
        Target::None => {
            if !reported {
                return Err("an action with no target can only be unsupported or blocked");
            }
            if action.prior != Observed::Absent || action.content.is_some() {
                return Err("an action with no target observes nothing and carries no content");
            }
        }
        Target::Program { .. } => {
            if action.content.is_some() {
                return Err("a program action carries no content");
            }
            let prior = match action.change {
                Change::Install | Change::Unsupported(_) | Change::Blocked(_) => Observed::Absent,
                Change::Unchanged => Observed::Installed,
                _ => {
                    return Err(
                        "a program can only be installed, unchanged, unsupported, or blocked",
                    );
                }
            };
            if action.prior != prior {
                return Err("a program action's prior doesn't fit its change");
            }
        }
        Target::Path { .. } | Target::Entry { .. } => {
            let entry = matches!(action.target, Target::Entry { .. });
            if action.prior == Observed::Installed {
                return Err("only a program is observed as installed");
            }
            match action.change {
                Change::Install => return Err("only a program is installed"),
                Change::Replace if entry => {
                    return Err("an MCP entry that loadout doesn't own is never replaced");
                }
                Change::Create
                | Change::Update
                | Change::Adopt
                | Change::Replace
                | Change::Drifted => {
                    let fits = match &action.content {
                        Some(Content::Entry(_)) => entry,
                        Some(Content::File(_) | Content::Tree(_)) => !entry,
                        None => false,
                    };
                    if !fits {
                        return Err("a write needs content of its target's kind");
                    }
                }
                Change::Remove
                | Change::Unchanged
                | Change::Unsupported(_)
                | Change::Blocked(_) => {
                    if action.content.is_some() {
                        return Err("only a write carries content");
                    }
                }
            }
            // Unsupported means loadout never looked at the target. A blocked action may keep
            // what it saw, such as an MCP key loadout doesn't own; it still carries no content.
            if matches!(action.change, Change::Unsupported(_)) && action.prior != Observed::Absent {
                return Err("an unsupported action observes nothing");
            }
        }
    }
    Ok(())
}

fn describe(action: &Action) -> String {
    let agent = action
        .item
        .agent
        .as_deref()
        .map(|agent| format!(" for {agent}"))
        .unwrap_or_default();
    format!("{:?} {}{agent}", action.item.kind, action.item.id).to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::manifest::{McpFormat, McpShape};
    use crate::plan::{FileContent, ItemKind, ItemRef};

    fn item(kind: ItemKind, id: &str) -> ItemRef {
        ItemRef {
            kind,
            id: id.to_owned(),
            agent: None,
        }
    }

    fn file() -> Content {
        Content::File(FileContent {
            bytes: b"x".to_vec(),
            executable: false,
        })
    }

    fn path(path: &str) -> Target {
        Target::Path {
            path: PathBuf::from(path),
        }
    }

    fn entry() -> Target {
        Target::Entry {
            file: PathBuf::from("/h/.claude.json"),
            format: McpFormat::Json,
            pointer: "/mcpServers".to_owned(),
            key: "docs".to_owned(),
            shape: McpShape::ClaudeJson,
        }
    }

    fn action(target: Target, change: Change, prior: Observed, content: Option<Content>) -> Action {
        Action {
            item: item(ItemKind::File, "x"),
            target,
            change,
            prior,
            content,
        }
    }

    fn check(actions: Vec<Action>) -> Result<(), String> {
        Plan { actions }.check()
    }

    #[test]
    fn several_valid_none_actions_share_a_plan() {
        let none = |change| action(Target::None, change, Observed::Absent, None);
        check(vec![
            Action::unsupported(
                item(ItemKind::Instructions, "a"),
                Target::None,
                "no file".into(),
            ),
            Action::unsupported(
                item(ItemKind::Instructions, "b"),
                Target::None,
                "no file".into(),
            ),
            none(Change::Blocked("isolated".into())),
            none(Change::Blocked("isolated".into())),
        ])
        .unwrap();
    }

    #[test]
    fn a_none_action_that_would_write_or_observe_is_refused() {
        for bad in [
            action(Target::None, Change::Create, Observed::Absent, Some(file())),
            action(Target::None, Change::Remove, Observed::Absent, None),
            action(
                Target::None,
                Change::Unsupported("u".into()),
                Observed::Present {
                    sha256: "a".repeat(64),
                },
                None,
            ),
            action(
                Target::None,
                Change::Blocked("b".into()),
                Observed::Absent,
                Some(file()),
            ),
        ] {
            assert!(check(vec![bad.clone()]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_concrete_destination_twice_is_refused() {
        let create = || {
            action(
                path("/h/.a"),
                Change::Create,
                Observed::Absent,
                Some(file()),
            )
        };
        assert!(check(vec![create(), create()]).is_err());
        let entry_write = |key: &str| {
            let mut target = entry();
            if let Target::Entry { key: slot, .. } = &mut target {
                *slot = key.to_owned();
            }
            action(
                target,
                Change::Create,
                Observed::Absent,
                Some(Content::Entry(serde_json::json!({}))),
            )
        };
        assert!(check(vec![entry_write("a"), entry_write("a")]).is_err());
        check(vec![entry_write("a"), entry_write("b")]).unwrap();
        let program = || Action {
            item: item(ItemKind::Program, "rg"),
            target: Target::Program {
                manager: Manager::Apt,
                package: "ripgrep".into(),
            },
            change: Change::Install,
            prior: Observed::Absent,
            content: None,
        };
        assert!(check(vec![program(), program()]).is_err());
    }

    #[test]
    fn writes_need_content_of_their_kind_and_entries_are_never_replaced() {
        let tree = Content::Tree(BTreeMap::new());
        check(vec![action(
            path("/h/.s"),
            Change::Create,
            Observed::Absent,
            Some(tree),
        )])
        .unwrap();
        assert!(
            check(vec![action(
                path("/h/.a"),
                Change::Update,
                Observed::Absent,
                None
            )])
            .is_err()
        );
        assert!(
            check(vec![action(
                path("/h/.a"),
                Change::Create,
                Observed::Absent,
                Some(Content::Entry(serde_json::json!({})))
            )])
            .is_err()
        );
        assert!(
            check(vec![action(
                entry(),
                Change::Replace,
                Observed::Present {
                    sha256: "a".repeat(64)
                },
                Some(Content::Entry(serde_json::json!({})))
            )])
            .is_err()
        );
        assert!(
            check(vec![action(
                path("/h/.a"),
                Change::Install,
                Observed::Absent,
                None
            )])
            .is_err()
        );
    }

    #[test]
    fn a_blocked_entry_may_keep_its_observation_but_never_content_or_a_replace() {
        let seen = || Observed::Present {
            sha256: "a".repeat(64),
        };
        check(vec![action(
            entry(),
            Change::Blocked("not owned".into()),
            seen(),
            None,
        )])
        .unwrap();
        assert!(
            check(vec![action(
                entry(),
                Change::Blocked("not owned".into()),
                seen(),
                Some(Content::Entry(serde_json::json!({})))
            )])
            .is_err()
        );
        assert!(
            check(vec![action(
                entry(),
                Change::Unsupported("u".into()),
                seen(),
                None
            )])
            .is_err()
        );
        assert!(
            check(vec![action(
                entry(),
                Change::Blocked("b".into()),
                Observed::Installed,
                None
            )])
            .is_err()
        );
    }

    #[test]
    fn a_program_action_fits_its_change() {
        let program = |change, prior| Action {
            item: item(ItemKind::Program, "rg"),
            target: Target::Program {
                manager: Manager::Brew,
                package: "ripgrep".into(),
            },
            change,
            prior,
            content: None,
        };
        check(vec![program(Change::Install, Observed::Absent)]).unwrap();
        check(vec![program(Change::Unchanged, Observed::Installed)]).unwrap();
        assert!(check(vec![program(Change::Unchanged, Observed::Absent)]).is_err());
        assert!(check(vec![program(Change::Create, Observed::Absent)]).is_err());
    }
}
