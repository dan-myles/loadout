use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::files::Produced;
use super::identity::{self, Place, Probe, Unresolved, Verdict};
use super::volume::Case;
use super::{Action, Change, Desired, Error, ItemKind, ItemRef, Observed, Target, error};
use crate::context::Context;
use crate::diagnostic::Diagnostics;
use crate::state::{self, State};

/// Turns what should be on disk into actions by the one hash definition: `state::digest` of the
/// desired content against `state::observe` of the target and the target's ownership record.
/// Owned targets the loadout no longer declares become removals. Destinations are compared by
/// where they lead, not only by spelling, before any of them is changed.
pub(super) fn actions(
    produced: Produced,
    state: &State,
    context: &Context,
    loadout: &Path,
    problems: &mut Diagnostics,
) -> Result<Vec<Action>, Error> {
    let mut actions = Vec::new();
    for desired in produced.desired {
        let wanted = match state::digest(&desired.content) {
            Ok(Observed::Present { sha256 }) => sha256,
            Ok(other) => {
                return Err(internal(&format!(
                    "the digest of {} is {other:?}",
                    state::label(&desired.target)
                )));
            }
            Err(failure) => {
                problems.push(error(&item_key(&desired.item), failure.to_string()));
                continue;
            }
        };
        let record = state
            .record(&desired.target)
            .map(|record| record.sha256.as_str());
        match state::observe(context, loadout, &desired.target) {
            Ok(observed) => actions.push(assemble(desired, observed, record, &wanted)?),
            Err(failure) => actions.push(refused(
                desired.item,
                desired.target,
                &failure,
                Refusal::Desired,
            )?),
        }
    }
    // An item still declared but unsupported here keeps whatever loadout owns at its target;
    // only targets the loadout no longer declares at all are removed.
    actions.extend(produced.unsupported);

    let declared = actions.len();
    let (mut places, unknown) = places(context, &mut actions)?;
    let probe = identity::case_probe(context);
    let probe = probe
        .as_ref()
        .map(|probe| probe as &dyn Fn(&Path) -> (PathBuf, Case));
    let mut owned = Owned::default();
    let (removals, removal_places) = removals(
        state,
        context,
        loadout,
        &mut actions,
        &places,
        declared,
        unknown,
        probe,
        &mut owned,
    )?;
    actions.extend(removals);
    places.extend(removal_places);
    owned.migrating.resize(actions.len(), false);
    relate_all(&mut actions, &places, declared, probe, &owned, problems);
    Ok(actions)
}

/// What a migration leaves behind. `migrating` marks declared actions blocked as migrations:
/// they still count against other writers, but their new contract never stands in for the
/// ownership. `retained` is that ownership: each old record kept under its OLD format and
/// shape, which protects its place from conflicting writers and removals although no action
/// carries it.
#[derive(Default)]
struct Owned {
    migrating: Vec<bool>,
    retained: Vec<(Target, Place)>,
}

/// One declared target's action: its change, the observation it was planned from, and the
/// content only for a change that may write. A blocked entry keeps what was observed there.
fn assemble(
    desired: Desired,
    observed: Observed,
    record: Option<&str>,
    wanted: &str,
) -> Result<Action, Error> {
    let entry = matches!(desired.target, Target::Entry { .. });
    let change = classify(entry, &observed, record, wanted, &desired.target)?;
    let content = keeps_content(&change).then_some(desired.content);
    Ok(Action {
        item: desired.item,
        target: desired.target,
        change,
        prior: observed,
        content,
    })
}

/// The place of every declared action, by spelling and with links resolved. A writer whose
/// place can't be established writes nothing. The flag is set when any declared place is
/// unknown: then no removal can be shown to stay clear of it.
fn places(context: &Context, actions: &mut [Action]) -> Result<(Vec<Option<Place>>, bool), Error> {
    let mut places = Vec::with_capacity(actions.len());
    let mut unknown = false;
    for action in actions.iter_mut() {
        let Some(path) = identity::file(&action.target) else {
            places.push(None);
            continue;
        };
        match identity::place(context, path) {
            Ok(place) => places.push(Some(place)),
            Err(Unresolved::Internal(message)) => return Err(internal(&message)),
            Err(Unresolved::Unknown(reason)) => {
                unknown = true;
                places.push(None);
                if !matches!(action.change, Change::Unsupported(_) | Change::Blocked(_)) {
                    action.change =
                        Change::Blocked(format!("{reason}, so it writes nothing there"));
                    action.content = None;
                }
            }
        }
    }
    Ok((places, unknown))
}

/// Every ownership record, in two jobs. First, each record owns its stored MCP key, whether or
/// not its exact target is still declared: a declared key with the same stored key in one
/// document, or possibly one, under another format or shape, is a migration. The declaration is
/// blocked and the record is kept as retained ownership under its old contract. Second, a
/// record the loadout no longer declares becomes a removal, blocked too when it migrates, and
/// left out when its file, map, and key are declared as written under another contract, so the
/// plan holds one action for that identity.
#[expect(
    clippy::too_many_arguments,
    reason = "one pass over the records needs every input"
)]
fn removals(
    state: &State,
    context: &Context,
    loadout: &Path,
    actions: &mut [Action],
    declared_places: &[Option<Place>],
    declared: usize,
    unknown: bool,
    probe: Option<Probe<'_>>,
    owned: &mut Owned,
) -> Result<(Vec<Action>, Vec<Option<Place>>), Error> {
    owned.migrating = vec![false; declared];
    let targets: BTreeSet<Target> = actions[..declared]
        .iter()
        .filter(|action| identity::file(&action.target).is_some())
        .map(|action| action.target.clone())
        .collect();
    let mut removals = Vec::new();
    let mut places = Vec::new();
    for record in state.records() {
        let still_declared = targets.contains(&record.target);
        let blocked = |reason: String| Action {
            item: record.item.clone(),
            target: record.target.clone(),
            change: Change::Blocked(reason),
            prior: Observed::Absent,
            content: None,
        };
        let Some(path) = identity::file(&record.target) else {
            return Err(internal(&format!(
                "a record has no file: {}",
                state::label(&record.target)
            )));
        };
        let place = match identity::place(context, path) {
            Ok(place) => place,
            Err(Unresolved::Internal(message)) => return Err(internal(&message)),
            Err(Unresolved::Unknown(reason)) => {
                // A declaration at this exact path has the same unknown place, which already
                // blocks it and every removal.
                if !still_declared {
                    removals.push(blocked(format!("{reason}, so loadout doesn't remove it")));
                    places.push(None);
                }
                continue;
            }
        };

        let migrating = migrations(
            actions,
            declared_places,
            declared,
            &record.target,
            &place,
            probe,
        );
        for index in &migrating {
            let reason = migration_reason(&record.target, &actions[*index].target);
            block(&mut actions[*index], reason);
            owned.migrating[*index] = true;
        }
        // The record keeps owning its key under its old contract unless a writer still carries
        // that contract. That is so after a migration, and also when its exact declaration is
        // unsupported here: nothing then writes the key, but the record stays, and so does what
        // it owns. An ordinary still-declared writer carries the contract itself.
        let unsupported_owner = actions[..declared].iter().any(|action| {
            action.target == record.target && matches!(action.change, Change::Unsupported(_))
        });
        if !migrating.is_empty() || unsupported_owner {
            owned.retained.push((record.target.clone(), place.clone()));
        }

        if still_declared
            || actions[..declared]
                .iter()
                .any(|action| same_key(&action.target, &record.target))
        {
            continue;
        }
        let mut action = if unknown {
            blocked(format!(
                "loadout can't tell whether removing {} would touch a declared destination whose place is unknown",
                state::label(&record.target)
            ))
        } else {
            match state::observe(context, loadout, &record.target) {
                Ok(observed) => Action {
                    item: record.item.clone(),
                    target: record.target.clone(),
                    change: Change::Remove,
                    prior: observed,
                    content: None,
                },
                Err(failure) => refused(
                    record.item.clone(),
                    record.target.clone(),
                    &failure,
                    Refusal::Removal,
                )?,
            }
        };
        for index in &migrating {
            block(
                &mut action,
                migration_reason(&record.target, &actions[*index].target),
            );
        }
        removals.push(action);
        places.push(Some(place));
    }
    Ok((removals, places))
}

/// The declared actions that would take over an owned record's stored key under another format
/// or shape: the same stored key (raw pointer and key, nothing folded), in one document or
/// possibly one. This is not the logical address collision check: a Cursor `my_docs` and a Pi
/// `my_docs` are one stored key, though their addresses differ.
fn migrations(
    actions: &[Action],
    places: &[Option<Place>],
    declared: usize,
    owned: &Target,
    owned_place: &Place,
    probe: Option<Probe<'_>>,
) -> Vec<usize> {
    let Some(stored) = identity::storage(owned) else {
        return Vec::new();
    };
    (0..declared)
        .filter(|index| {
            let target = &actions[*index].target;
            identity::storage(target).as_ref() == Some(&stored)
                && contract(target) != contract(owned)
                && places[*index].as_ref().is_some_and(|place| {
                    identity::document(owned_place, place, probe) != Verdict::Disjoint
                })
        })
        .collect()
}

/// Two entry targets that name one key, file, and map as written but differ in format or
/// shape: the same plan identity under another contract.
fn same_key(declared: &Target, owned: &Target) -> bool {
    match (declared, owned) {
        (
            Target::Entry {
                file, pointer, key, ..
            },
            Target::Entry {
                file: owned_file,
                pointer: owned_pointer,
                key: owned_key,
                ..
            },
        ) => {
            file == owned_file && pointer == owned_pointer && key == owned_key && declared != owned
        }
        _ => false,
    }
}

fn contract(target: &Target) -> String {
    match target {
        Target::Entry { format, shape, .. } => format!("{} {}", format.id(), shape.id()),
        _ => String::new(),
    }
}

fn migration_reason(owned: &Target, declared: &Target) -> String {
    format!(
        "loadout owns {} as {}, and the loadout now declares that stored key as {} at {}; loadout doesn't convert an owned MCP key to another format or shape, so remove the key by hand or keep its old declaration",
        state::label(owned),
        contract(owned),
        contract(declared),
        state::label(declared)
    )
}

/// Blocks an action for one more reason. A reason it already has stays first and the new one
/// is appended once, so no refusal hides another. An unsupported action writes nothing and
/// stays as it is. Blocked actions carry no content; the prior stays what was observed.
fn block(action: &mut Action, reason: String) {
    match &action.change {
        Change::Unsupported(_) => return,
        Change::Blocked(existing) if existing.contains(&reason) => {}
        Change::Blocked(existing) => {
            action.change = Change::Blocked(format!("{existing}; {reason}"));
        }
        _ => action.change = Change::Blocked(reason),
    }
    action.content = None;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// A declared item that would write: any change but unsupported.
    Writer,
    /// A declared item that can't be placed here. It writes nothing but still protects its
    /// place from removals.
    Silent,
    /// A declaration blocked as an ownership migration. It still counts against other writers
    /// when it would write, but the retained old ownership, not its new contract, guards the
    /// place against everything else.
    Migrating {
        writes: bool,
    },
    Removal,
}

fn role(index: usize, action: &Action, declared: usize, owned: &Owned) -> Role {
    let writes = !matches!(action.change, Change::Unsupported(_));
    if index >= declared {
        Role::Removal
    } else if owned.migrating[index] {
        Role::Migrating { writes }
    } else if writes {
        Role::Writer
    } else {
        Role::Silent
    }
}

fn relation_text(verdict: &Verdict) -> Option<String> {
    match verdict {
        Verdict::Disjoint => None,
        Verdict::Conflict { case: None } => Some(
            "are one destination: the same file, one inside the other, one MCP key inside the other, or one file in two formats".to_owned(),
        ),
        Verdict::Conflict { case: Some(dir) } => Some(format!(
            "are one destination, because {} ignores case",
            dir.display()
        )),
        Verdict::Unknown { dir } => Some(format!(
            "might be one destination: loadout can't tell how {} treats case",
            dir.display()
        )),
    }
}

/// Every pair of concrete destinations, by `identity::relate`, with the roles' rules: two
/// writers that are one destination are invalid before anything is written, and possibly one
/// are both blocked; a removal that reaches, or might reach, any declared destination or
/// another removal is blocked. Compatible distinct MCP keys in one document never meet. Then
/// each retained old ownership, under its old contract, blocks a writer or removal that would
/// conflict with it.
fn relate_all(
    actions: &mut [Action],
    places: &[Option<Place>],
    declared: usize,
    probe: Option<Probe<'_>>,
    owned: &Owned,
    problems: &mut Diagnostics,
) {
    let mut blocks: Vec<(usize, String)> = Vec::new();
    for (first, first_place) in places.iter().enumerate() {
        let Some(first_place) = first_place else {
            continue;
        };
        for (offset, second_place) in places[first + 1..].iter().enumerate() {
            let Some(second_place) = second_place else {
                continue;
            };
            let second = first + 1 + offset;
            let (one, other) = (&actions[first], &actions[second]);
            let roles = (
                role(first, one, declared, owned),
                role(second, other, declared, owned),
            );
            let writer =
                |role: Role| matches!(role, Role::Writer | Role::Migrating { writes: true });
            let relevant = match roles {
                (Role::Removal, Role::Removal | Role::Writer | Role::Silent)
                | (Role::Writer | Role::Silent, Role::Removal) => true,
                (a, b) => writer(a) && writer(b),
            };
            if !relevant {
                continue;
            }
            let verdict = identity::relate(
                (&one.target, first_place),
                (&other.target, second_place),
                probe,
            );
            let Some(how) = relation_text(&verdict) else {
                continue;
            };
            let both = format!(
                "{} ({}) and {} ({}) {how}",
                other.item.label(),
                state::label(&other.target),
                one.item.label(),
                state::label(&one.target)
            );
            match roles {
                (Role::Removal, Role::Removal) => {
                    let reason = format!("{both}, so loadout removes neither");
                    blocks.push((first, reason.clone()));
                    blocks.push((second, reason));
                }
                (Role::Removal, _) | (_, Role::Removal) => {
                    let (removal, kept) = if roles.0 == Role::Removal {
                        (first, second)
                    } else {
                        (second, first)
                    };
                    blocks.push((
                        removal,
                        format!(
                            "loadout owns {} but now declares {} there: they {how}; remove one of them by hand",
                            state::label(&actions[removal].target),
                            state::label(&actions[kept].target)
                        ),
                    ));
                }
                _ if matches!(verdict, Verdict::Conflict { .. }) => {
                    problems.push(error(&item_key(&other.item), both));
                }
                _ => {
                    let reason = format!("{both}, so loadout writes neither");
                    blocks.push((first, reason.clone()));
                    blocks.push((second, reason));
                }
            }
        }
    }

    for (old, old_place) in &owned.retained {
        for (index, place) in places.iter().enumerate() {
            let Some(place) = place else {
                continue;
            };
            let action = &actions[index];
            if !matches!(
                role(index, action, declared, owned),
                Role::Writer | Role::Removal
            ) || action.target == *old
            {
                continue;
            }
            let verdict = identity::relate((old, old_place), (&action.target, place), probe);
            let Some(how) = relation_text(&verdict) else {
                continue;
            };
            blocks.push((
                index,
                format!(
                    "loadout still owns {} as {}, which no declaration writes here; {} and that key {how}",
                    state::label(old),
                    contract(old),
                    state::label(&action.target)
                ),
            ));
        }
    }

    for (index, reason) in blocks {
        block(&mut actions[index], reason);
    }
}

/// The change for one declared target. `wanted` is the digest of the desired content.
fn classify(
    entry: bool,
    observed: &Observed,
    record: Option<&str>,
    wanted: &str,
    target: &Target,
) -> Result<Change, Error> {
    let disk = match observed {
        Observed::Absent => {
            return Ok(match record {
                None => Change::Create,
                Some(_) => Change::Drifted,
            });
        }
        Observed::Present { sha256 } => sha256.as_str(),
        Observed::Installed => {
            return Err(internal(&format!(
                "{} was observed as an installed program",
                state::label(target)
            )));
        }
    };
    Ok(match record {
        None if disk == wanted => Change::Adopt,
        None if entry => Change::Blocked(format!(
            "{} already holds an MCP server by this name that loadout doesn't own, and it differs; loadout never replaces it",
            state::label(target)
        )),
        None => Change::Replace,
        Some(owned) if disk == wanted && owned == wanted => Change::Unchanged,
        Some(owned) if disk == wanted || owned == disk => Change::Update,
        Some(_) => Change::Drifted,
    })
}

/// Whether an action needs its content: every change that may write, so that `--force` can
/// still write a drifted target.
fn keeps_content(change: &Change) -> bool {
    matches!(
        change,
        Change::Create | Change::Update | Change::Adopt | Change::Replace | Change::Drifted
    )
}

#[derive(Clone, Copy)]
enum Refusal {
    Desired,
    Removal,
}

/// A target loadout must not read or replace. A reserved destination is unsupported for a
/// declared item (architecture section 6) and blocks a removal, and a special file blocks
/// either. Then state's own classifier decides: a fact about the machine, such as an MCP config
/// loadout won't edit or a server name taken by a value loadout doesn't own, is blocked.
/// Anything else, such as an I/O fault or a broken entry contract, is a runtime error.
fn refused(
    item: ItemRef,
    target: Target,
    failure: &state::Error,
    refusal: Refusal,
) -> Result<Action, Error> {
    let reason = failure.to_string();
    let change = match (failure, refusal) {
        (state::Error::Reserved { .. }, Refusal::Desired) => Change::Unsupported(reason),
        (state::Error::Reserved { .. } | state::Error::Special { .. }, _) => {
            Change::Blocked(reason)
        }
        _ if failure.is_blocked() => Change::Blocked(reason),
        _ => {
            return Err(Error::Runtime(anyhow::anyhow!(
                "cannot observe {}: {reason}",
                state::label(&target)
            )));
        }
    };
    Ok(Action {
        item,
        target,
        change,
        prior: Observed::Absent,
        content: None,
    })
}

pub(super) fn item_key(item: &ItemRef) -> String {
    let section = match item.kind {
        ItemKind::File => "files",
        ItemKind::Skill => "skills",
        ItemKind::Instructions => "instructions",
        ItemKind::Mcp => "mcp",
        ItemKind::Program => "programs",
    };
    format!("{section}.{}", item.id)
}

fn internal(message: &str) -> Error {
    Error::Runtime(anyhow::anyhow!("internal error: {message}"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::manifest::{McpFormat, McpShape};

    const D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const H: &str = "hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh";
    const R: &str = "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr";

    fn path() -> Target {
        Target::Path {
            path: PathBuf::from("/h/.gitconfig"),
        }
    }

    fn entry(key: &str) -> Target {
        Target::Entry {
            file: PathBuf::from("/h/.claude.json"),
            format: McpFormat::Json,
            pointer: "/mcpServers".to_owned(),
            key: key.to_owned(),
            shape: McpShape::ClaudeJson,
        }
    }

    fn present(sha256: &str) -> Observed {
        Observed::Present {
            sha256: sha256.to_owned(),
        }
    }

    fn change(is_entry: bool, observed: &Observed, record: Option<&str>) -> Change {
        let target = if is_entry { entry("docs") } else { path() };
        classify(is_entry, observed, record, D, &target).unwrap()
    }

    #[test]
    fn every_disk_and_record_combination_has_one_change() {
        for is_entry in [false, true] {
            assert_eq!(change(is_entry, &Observed::Absent, None), Change::Create);
            assert_eq!(
                change(is_entry, &Observed::Absent, Some(R)),
                Change::Drifted
            );
            assert_eq!(change(is_entry, &present(D), None), Change::Adopt);
            assert_eq!(change(is_entry, &present(D), Some(D)), Change::Unchanged);
            assert_eq!(change(is_entry, &present(D), Some(R)), Change::Update);
            assert_eq!(change(is_entry, &present(H), Some(H)), Change::Update);
            assert_eq!(change(is_entry, &present(H), Some(R)), Change::Drifted);
        }
        assert_eq!(change(false, &present(H), None), Change::Replace);
    }

    #[test]
    fn an_unowned_entry_that_differs_is_blocked_never_replaced() {
        let blocked = change(true, &present(H), None);
        assert!(
            matches!(&blocked, Change::Blocked(reason) if reason.contains("never replaces")),
            "{blocked:?}"
        );
    }

    #[test]
    fn an_installed_observation_of_a_file_is_an_internal_error() {
        assert!(classify(false, &Observed::Installed, None, D, &path()).is_err());
    }

    #[test]
    fn only_changes_that_may_write_keep_their_content() {
        for keep in [
            Change::Create,
            Change::Update,
            Change::Adopt,
            Change::Replace,
            Change::Drifted,
        ] {
            assert!(keeps_content(&keep), "{keep:?}");
        }
        for drop in [
            Change::Unchanged,
            Change::Remove,
            Change::Blocked("b".to_owned()),
            Change::Unsupported("u".to_owned()),
        ] {
            assert!(!keeps_content(&drop), "{drop:?}");
        }
    }

    fn desired(target: Target) -> Desired {
        let content = match &target {
            Target::Entry { .. } => {
                super::super::Content::Entry(serde_json::json!({"command": "docs"}))
            }
            _ => super::super::Content::File(super::super::FileContent {
                bytes: b"x".to_vec(),
                executable: false,
            }),
        };
        Desired {
            item: ItemRef {
                kind: ItemKind::Mcp,
                id: "docs".to_owned(),
                agent: Some("claude".to_owned()),
            },
            target,
            content,
        }
    }

    #[test]
    fn an_assembled_unowned_entry_that_differs_is_blocked_and_passes_the_plan_check() {
        let action = assemble(desired(entry("docs")), present(H), None, D).unwrap();
        assert!(matches!(action.change, Change::Blocked(_)), "{action:?}");
        assert_eq!(action.prior, present(H), "the observation stays as seen");
        assert!(
            action.content.is_none(),
            "a blocked entry carries nothing to write"
        );
        super::super::Plan {
            actions: vec![action],
        }
        .check()
        .unwrap();
    }

    #[test]
    fn assembled_entries_that_match_or_are_owned_keep_their_writes_and_pass_the_plan_check() {
        let adopt = assemble(desired(entry("docs")), present(D), None, D).unwrap();
        assert_eq!(adopt.change, Change::Adopt);
        assert!(adopt.content.is_some());
        let update = assemble(desired(entry("other")), present(H), Some(H), D).unwrap();
        assert_eq!(update.change, Change::Update);
        assert!(update.content.is_some());
        let unchanged = assemble(desired(entry("third")), present(D), Some(D), D).unwrap();
        assert_eq!(unchanged.change, Change::Unchanged);
        assert!(unchanged.content.is_none());
        super::super::Plan {
            actions: vec![adopt, update, unchanged],
        }
        .check()
        .unwrap();
    }

    #[test]
    fn a_reserved_declared_target_is_unsupported_and_a_reserved_removal_is_blocked() {
        let reserved = state::Error::Reserved {
            path: PathBuf::from("/h/.codex/auth.json"),
            message: "overlaps it".to_owned(),
        };
        let item = ItemRef {
            kind: ItemKind::File,
            id: "x".to_owned(),
            agent: None,
        };
        let declared = refused(item.clone(), path(), &reserved, Refusal::Desired).unwrap();
        assert!(matches!(declared.change, Change::Unsupported(_)));
        let removal = refused(item.clone(), path(), &reserved, Refusal::Removal).unwrap();
        assert!(matches!(removal.change, Change::Blocked(_)));
        for action in [&declared, &removal] {
            assert_eq!(action.prior, Observed::Absent);
            assert!(action.content.is_none());
        }
        let io = state::Error::Locked {
            path: PathBuf::from("/s/lock"),
        };
        assert!(refused(item, path(), &io, Refusal::Desired).is_err());
    }

    /// What a real entry observation can report is the machine's problem (blocked, exit 4) or
    /// loadout's (runtime, exit 1), as state's own `is_blocked` says, after the reserved and
    /// special arms. A special file is blocked by R1 although state's classifier doesn't list it.
    #[test]
    fn a_real_entry_refusal_is_blocked_when_it_is_the_machine_and_a_runtime_error_otherwise() {
        let item = ItemRef {
            kind: ItemKind::Mcp,
            id: "docs".to_owned(),
            agent: Some("claude".to_owned()),
        };
        let target = entry("docs");
        let document = PathBuf::from("/h/.claude.json");
        let mcp = |source: crate::mcp::Error| state::Error::Mcp {
            target: state::label(&target),
            source,
        };
        let machine_faults = [
            state::Error::NotADocument {
                path: document.clone(),
            },
            state::Error::UnownedEntry {
                target: state::label(&target),
            },
            mcp(crate::mcp::Error::Parse {
                format: McpFormat::Json,
                message: "expected a value".to_owned(),
            }),
            mcp(crate::mcp::Error::Duplicate {
                pointer: "/mcpServers".to_owned(),
                key: "docs".to_owned(),
            }),
            mcp(crate::mcp::Error::NotAFile {
                path: document.clone(),
            }),
            mcp(crate::mcp::Error::Lock {
                path: PathBuf::from("/h/.grok/.config-init.lock"),
                message: "Grok holds it".to_owned(),
            }),
        ];
        for failure in &machine_faults {
            assert!(failure.is_blocked(), "{failure}");
            for refusal in [Refusal::Desired, Refusal::Removal] {
                let action = refused(item.clone(), target.clone(), failure, refusal).unwrap();
                assert_eq!(
                    action.change,
                    Change::Blocked(failure.to_string()),
                    "{failure}"
                );
                assert_eq!(action.prior, Observed::Absent);
                assert!(action.content.is_none());
            }
        }

        let special = state::Error::Special {
            path: document.clone(),
        };
        assert!(
            !special.is_blocked(),
            "only R1's own arm blocks a special file"
        );
        for refusal in [Refusal::Desired, Refusal::Removal] {
            let action = refused(item.clone(), target.clone(), &special, refusal).unwrap();
            assert_eq!(action.change, Change::Blocked(special.to_string()));
        }

        let loadout_faults = [
            mcp(crate::mcp::Error::Pointer {
                pointer: "mcpServers".to_owned(),
            }),
            mcp(crate::mcp::Error::ShapeFormat {
                shape: McpShape::CodexToml,
                format: McpFormat::Json,
            }),
            mcp(crate::mcp::Error::Io {
                path: document.clone(),
                source: std::io::Error::other("disk"),
            }),
            state::Error::Io {
                action: "read",
                path: document.clone(),
                source: std::io::Error::other("disk"),
            },
            state::Error::NotUtf8 { path: document },
        ];
        for failure in &loadout_faults {
            assert!(!failure.is_blocked(), "{failure}");
            for refusal in [Refusal::Desired, Refusal::Removal] {
                assert!(
                    refused(item.clone(), target.clone(), failure, refusal).is_err(),
                    "a programming, contract, or I/O fault is never a blocked action: {failure}"
                );
            }
        }
    }

    struct Machine {
        _dir: tempfile::TempDir,
        home: PathBuf,
        loadout: PathBuf,
        context: Context,
    }

    fn machine() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let loadout = root.join("loadout");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&loadout).unwrap();
        let vars = (
            home.as_os_str().to_owned(),
            root.join("state").as_os_str().to_owned(),
        );
        let context = Context::resolve(
            crate::platform::Os::current().unwrap(),
            move |name| match name {
                "LOADOUT_HOME" => Some(vars.0.clone()),
                "LOADOUT_STATE_DIR" => Some(vars.1.clone()),
                _ => None,
            },
            None,
        )
        .unwrap();
        Machine {
            _dir: dir,
            home,
            loadout,
            context,
        }
    }

    /// A state with these ownership records, written as state.json the way state saves it.
    fn owning(machine: &Machine, targets: &[Target]) -> State {
        let records: Vec<serde_json::Value> = targets
            .iter()
            .enumerate()
            .map(|(index, target)| {
                serde_json::json!({
                    "target": target,
                    "item": { "kind": "mcp", "id": format!("old{index}"), "agent": "claude" },
                    "sha256": "a".repeat(64),
                    "run": "20261025T000000.000000000Z",
                })
            })
            .collect();
        std::fs::create_dir_all(&machine.context.state).unwrap();
        std::fs::write(
            machine.context.state.join("state.json"),
            serde_json::to_vec(&serde_json::json!({ "version": 1, "records": records })).unwrap(),
        )
        .unwrap();
        State::load(&machine.context).unwrap()
    }

    fn mcp_at(
        file: PathBuf,
        format: McpFormat,
        pointer: &str,
        key: &str,
        shape: McpShape,
    ) -> Target {
        Target::Entry {
            file,
            format,
            pointer: pointer.to_owned(),
            key: key.to_owned(),
            shape,
        }
    }

    fn compared(
        machine: &Machine,
        desired: Vec<Desired>,
        state: &State,
    ) -> (Vec<Action>, Diagnostics) {
        let mut problems = Diagnostics::default();
        let actions = actions(
            Produced {
                desired,
                unsupported: Vec::new(),
            },
            state,
            &machine.context,
            &machine.loadout,
            &mut problems,
        )
        .unwrap();
        (actions, problems)
    }

    #[test]
    fn an_owned_key_declared_under_another_shape_or_format_is_one_blocked_action() {
        let machine = machine();
        let file = machine.home.join("custom.json");
        for (owned, declared) in [
            (
                mcp_at(
                    file.clone(),
                    McpFormat::Json,
                    "/mcpServers",
                    "docs",
                    McpShape::CursorJson,
                ),
                mcp_at(
                    file.clone(),
                    McpFormat::Json,
                    "/mcpServers",
                    "docs",
                    McpShape::PiJson,
                ),
            ),
            (
                mcp_at(
                    file.clone(),
                    McpFormat::Json,
                    "/mcpServers",
                    "docs",
                    McpShape::CursorJson,
                ),
                mcp_at(
                    file.clone(),
                    McpFormat::Jsonc,
                    "/mcpServers",
                    "docs",
                    McpShape::CursorJson,
                ),
            ),
        ] {
            let state = owning(&machine, std::slice::from_ref(&owned));
            let (actions, problems) = compared(&machine, vec![desired(declared.clone())], &state);
            assert!(problems.is_empty(), "{problems:?}");
            assert_eq!(actions.len(), 1, "no separate removal: {actions:#?}");
            assert_eq!(actions[0].target, declared);
            assert!(
                only_migrated(&actions[0], &owned, &declared),
                "{actions:#?}"
            );
            assert!(actions[0].content.is_none());
            super::super::Plan { actions }.check().unwrap();
            assert_eq!(
                State::load(&machine.context).unwrap().records().count(),
                1,
                "the old record stays as it is"
            );
        }
    }

    #[test]
    fn an_owned_key_declared_unchanged_is_not_a_migration() {
        let machine = machine();
        let target = mcp_at(
            machine.home.join("custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "docs",
            McpShape::CursorJson,
        );
        let state = owning(&machine, std::slice::from_ref(&target));
        let (actions, problems) = compared(&machine, vec![desired(target)], &state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(actions.len(), 1);
        assert!(
            !matches!(&actions[0].change, Change::Blocked(reason) if reason.contains("doesn't convert")),
            "{actions:#?}"
        );
        super::super::Plan { actions }.check().unwrap();
    }

    #[test]
    fn writers_in_one_document_conflict_by_address_pi_name_and_format_but_not_as_disjoint_keys() {
        let machine = machine();
        let file = machine.home.join("custom.json");
        let writers = |pairs: [(McpFormat, &str, &str, McpShape); 2]| {
            pairs
                .into_iter()
                .map(|(format, pointer, key, shape)| {
                    desired(mcp_at(file.clone(), format, pointer, key, shape))
                })
                .collect::<Vec<_>>()
        };
        let state = State::default();
        for conflicting in [
            writers([
                (McpFormat::Json, "/servers", "group", McpShape::CursorJson),
                (
                    McpFormat::Json,
                    "/servers/group",
                    "env",
                    McpShape::CursorJson,
                ),
            ]),
            writers([
                (McpFormat::Json, "/mcpServers", "my_docs", McpShape::PiJson),
                (McpFormat::Json, "/mcpServers", "my-docs", McpShape::PiJson),
            ]),
            writers([
                (McpFormat::Json, "/mcpServers", "docs", McpShape::CursorJson),
                (
                    McpFormat::Jsonc,
                    "/elsewhere",
                    "search",
                    McpShape::CursorJson,
                ),
            ]),
        ] {
            let (_, problems) = compared(&machine, conflicting, &state);
            assert!(
                problems.has_errors(),
                "a conflicting pair is invalid before any write"
            );
        }
        for apart in [
            writers([
                (McpFormat::Json, "/mcpServers", "docs", McpShape::CursorJson),
                (
                    McpFormat::Json,
                    "/mcpServers",
                    "search",
                    McpShape::CursorJson,
                ),
            ]),
            writers([
                (
                    McpFormat::Json,
                    "/mcp~1servers",
                    "docs",
                    McpShape::CursorJson,
                ),
                (McpFormat::Json, "/mcp", "servers", McpShape::CursorJson),
            ]),
            writers([
                (
                    McpFormat::Json,
                    "/mcpServers",
                    "my_docs",
                    McpShape::CursorJson,
                ),
                (
                    McpFormat::Json,
                    "/mcpServers",
                    "my-docs",
                    McpShape::CursorJson,
                ),
            ]),
        ] {
            let (actions, problems) = compared(&machine, apart, &state);
            assert!(problems.is_empty(), "{problems:?}");
            super::super::Plan { actions }.check().unwrap();
        }
    }

    fn planned(target: Target, change: Change) -> Action {
        Action {
            item: ItemRef {
                kind: ItemKind::Mcp,
                id: "x".to_owned(),
                agent: None,
            },
            target,
            change,
            prior: present(H),
            content: None,
        }
    }

    fn roles(machine: &Machine, actions: &mut [Action], declared: usize) -> Diagnostics {
        let places: Vec<Option<Place>> = actions
            .iter()
            .map(|action| {
                Some(
                    identity::place(&machine.context, identity::file(&action.target).unwrap())
                        .unwrap(),
                )
            })
            .collect();
        let mut problems = Diagnostics::default();
        let owned = Owned {
            migrating: vec![false; actions.len()],
            retained: Vec::new(),
        };
        let probe = identity::case_probe(&machine.context);
        let probe = probe
            .as_ref()
            .map(|probe| probe as &dyn Fn(&Path) -> (PathBuf, Case));
        relate_all(actions, &places, declared, probe, &owned, &mut problems);
        problems
    }

    #[test]
    fn an_owned_parent_key_is_never_removed_under_a_declared_child_and_neither_is_the_child_under_a_parent()
     {
        let machine = machine();
        let file = machine.home.join("custom.json");
        let parent = mcp_at(
            file.clone(),
            McpFormat::Json,
            "/servers",
            "group",
            McpShape::CursorJson,
        );
        let child = mcp_at(
            file.clone(),
            McpFormat::Json,
            "/servers/group",
            "env",
            McpShape::CursorJson,
        );
        for (declared, owned) in [(child.clone(), parent.clone()), (parent, child)] {
            let mut actions = vec![
                planned(
                    declared,
                    Change::Blocked("an unowned key differs".to_owned()),
                ),
                planned(owned, Change::Remove),
            ];
            let problems = roles(&machine, &mut actions, 1);
            assert!(problems.is_empty(), "{problems:?}");
            assert!(
                matches!(&actions[0].change, Change::Blocked(reason) if reason == "an unowned key differs"),
                "the declared child keeps its own block"
            );
            assert!(
                matches!(&actions[1].change, Change::Blocked(reason) if reason.contains("loadout owns")),
                "{actions:#?}"
            );
            assert!(actions[1].content.is_none());
        }
    }

    #[test]
    fn two_owned_nested_keys_dropped_together_are_both_kept() {
        let machine = machine();
        let file = machine.home.join("custom.json");
        let mut actions = vec![
            planned(
                mcp_at(
                    file.clone(),
                    McpFormat::Json,
                    "/servers",
                    "group",
                    McpShape::CursorJson,
                ),
                Change::Remove,
            ),
            planned(
                mcp_at(
                    file,
                    McpFormat::Json,
                    "/servers/group",
                    "env",
                    McpShape::CursorJson,
                ),
                Change::Remove,
            ),
        ];
        roles(&machine, &mut actions, 0);
        for action in &actions {
            assert!(
                matches!(&action.change, Change::Blocked(reason) if reason.contains("removes neither")),
                "{action:?}"
            );
        }
    }

    #[test]
    fn an_unsupported_declaration_protects_its_place_from_a_removal_but_conflicts_with_no_writer() {
        let machine = machine();
        let at = |path: &str| Target::Path {
            path: machine.home.join(path),
        };
        let mut actions = vec![
            planned(at(".tool"), Change::Unsupported("reserved".to_owned())),
            planned(at(".tool"), Change::Update),
            planned(at(".tool/settings"), Change::Remove),
        ];
        actions[1].content = Some(super::super::Content::File(super::super::FileContent {
            bytes: b"x".to_vec(),
            executable: false,
        }));
        let problems = roles(&machine, &mut actions, 2);
        assert!(
            problems.is_empty(),
            "an unsupported item writes nothing: {problems:?}"
        );
        assert_eq!(actions[1].change, Change::Update);
        assert!(
            matches!(&actions[2].change, Change::Blocked(_)),
            "{actions:#?}"
        );
    }

    #[cfg(unix)]
    fn linked(machine: &Machine) {
        std::fs::create_dir(machine.home.join("real")).unwrap();
        std::os::unix::fs::symlink("real", machine.home.join("alias")).unwrap();
    }

    fn reasons(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .map(|action| match &action.change {
                Change::Blocked(reason) | Change::Unsupported(reason) => reason.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn of<'a>(actions: &'a [Action], target: &Target) -> &'a Action {
        actions
            .iter()
            .find(|action| action.target == *target)
            .unwrap_or_else(|| panic!("no action for {target:?}: {actions:#?}"))
    }

    #[cfg(unix)]
    fn migrated(action: &Action) -> bool {
        matches!(&action.change, Change::Blocked(reason) if reason.contains("doesn't convert"))
    }

    /// A real entry observation refuses nothing here (an absent document plans a create), so
    /// the migration of `owned` to `declared` is the action's whole refusal: no other reason
    /// comes before it or after it.
    fn only_migrated(action: &Action, owned: &Target, declared: &Target) -> bool {
        action.change == Change::Blocked(migration_reason(owned, declared))
    }

    /// The migration is the action's first refusal; a relation found later may follow it.
    #[cfg(unix)]
    fn migrated_first(action: &Action, owned: &Target, declared: &Target) -> bool {
        matches!(&action.change, Change::Blocked(reason) if reason.starts_with(&migration_reason(owned, declared)))
    }

    /// The retained OLD contract of `old` is the action's whole refusal: `old` and the action's
    /// target are one document, which `old`'s format forbids writing another way.
    fn only_guarded_by(action: &Action, old: &Target) -> bool {
        let how = relation_text(&Verdict::Conflict { case: None }).unwrap();
        action.change
            == Change::Blocked(format!(
                "loadout still owns {} as {}, which no declaration writes here; {} and that key {how}",
                state::label(old),
                contract(old),
                state::label(&action.target)
            ))
    }

    /// Cursor and Pi read `my_docs` as different names, but the document stores one key
    /// `my_docs`, so an owned key reached through a link under the other shape is a migration in
    /// both directions, both link orientations, and either record or declaration order.
    #[cfg(unix)]
    #[test]
    fn an_owned_raw_key_reached_through_a_link_under_another_shape_blocks_both_sides() {
        for (owned_shape, declared_shape) in [
            (McpShape::CursorJson, McpShape::PiJson),
            (McpShape::PiJson, McpShape::CursorJson),
        ] {
            for (owned_dir, declared_dir) in [("alias", "real"), ("real", "alias")] {
                for key in ["my_docs", "docs"] {
                    let machine = machine();
                    linked(&machine);
                    let owned = mcp_at(
                        machine.home.join(owned_dir).join("custom.json"),
                        McpFormat::Json,
                        "/mcpServers",
                        key,
                        owned_shape,
                    );
                    let declared = mcp_at(
                        machine.home.join(declared_dir).join("custom.json"),
                        McpFormat::Json,
                        "/mcpServers",
                        key,
                        declared_shape,
                    );
                    let other = mcp_at(
                        machine.home.join(declared_dir).join("custom.json"),
                        McpFormat::Json,
                        "/mcpServers",
                        "zzz_other",
                        declared_shape,
                    );
                    for order in [
                        vec![desired(declared.clone()), desired(other.clone())],
                        vec![desired(other.clone()), desired(declared.clone())],
                    ] {
                        let state = owning(&machine, std::slice::from_ref(&owned));
                        let (actions, problems) = compared(&machine, order, &state);
                        assert!(problems.is_empty(), "{problems:?}");
                        assert!(
                            only_migrated(of(&actions, &declared), &owned, &declared),
                            "{:?}",
                            reasons(&actions)
                        );
                        assert!(
                            only_migrated(of(&actions, &owned), &owned, &declared),
                            "the old record's removal is refused by the migration alone: {:?}",
                            reasons(&actions)
                        );
                        assert!(of(&actions, &owned).content.is_none());
                        assert!(
                            !migrated(of(&actions, &other)),
                            "a distinct raw key is no migration: {:?}",
                            reasons(&actions)
                        );
                        super::super::Plan { actions }.check().unwrap();
                        assert_eq!(State::load(&machine.context).unwrap().records().count(), 1);
                    }
                }
            }
        }
    }

    /// The old record still owns its stored key while its exact target stays declared, ordinary
    /// or unsupported. A second declaration that reaches that stored key through a link under
    /// another shape is a migration, though the two shapes' logical addresses differ.
    #[cfg(unix)]
    #[test]
    fn a_still_declared_owned_key_guards_its_stored_key_against_an_aliased_declaration_under_another_shape()
     {
        for (owned_shape, other_shape) in [
            (McpShape::CursorJson, McpShape::PiJson),
            (McpShape::PiJson, McpShape::CursorJson),
        ] {
            for (owned_dir, other_dir) in [("alias", "real"), ("real", "alias")] {
                for exact_unsupported in [false, true] {
                    let machine = machine();
                    linked(&machine);
                    let owned = mcp_at(
                        machine.home.join(owned_dir).join("custom.json"),
                        McpFormat::Json,
                        "/mcpServers",
                        "my_docs",
                        owned_shape,
                    );
                    let other = mcp_at(
                        machine.home.join(other_dir).join("custom.json"),
                        McpFormat::Json,
                        "/mcpServers",
                        "my_docs",
                        other_shape,
                    );
                    for other_first in [false, true] {
                        // An unsupported A comes from `Produced.unsupported`, after every
                        // desired action whatever its position, so it has one order only.
                        if exact_unsupported && other_first {
                            continue;
                        }
                        let state = owning(&machine, std::slice::from_ref(&owned));
                        let (mut desired_list, mut unsupported) =
                            (vec![desired(other.clone())], Vec::new());
                        if exact_unsupported {
                            unsupported.push(Action::unsupported(
                                ItemRef {
                                    kind: ItemKind::Mcp,
                                    id: "exact".to_owned(),
                                    agent: Some("custom".to_owned()),
                                },
                                owned.clone(),
                                "this agent can't take it".to_owned(),
                            ));
                        } else if other_first {
                            desired_list.push(desired(owned.clone()));
                        } else {
                            desired_list.insert(0, desired(owned.clone()));
                        }
                        let mut problems = Diagnostics::default();
                        let actions = actions(
                            Produced {
                                desired: desired_list,
                                unsupported,
                            },
                            &state,
                            &machine.context,
                            &machine.loadout,
                            &mut problems,
                        )
                        .unwrap();
                        assert!(problems.is_empty(), "{problems:?}");
                        assert_eq!(
                            actions.len(),
                            2,
                            "no removal for the still-declared record: {actions:#?}"
                        );
                        assert!(
                            only_migrated(of(&actions, &other), &owned, &other),
                            "{:?}",
                            reasons(&actions)
                        );
                        let exact = of(&actions, &owned);
                        if exact_unsupported {
                            assert!(
                                matches!(exact.change, Change::Unsupported(_)),
                                "unsupported stays as it is"
                            );
                        } else {
                            assert!(
                                !migrated(exact),
                                "the exact old declaration is no migration: {:?}",
                                reasons(&actions)
                            );
                        }
                        super::super::Plan { actions }.check().unwrap();
                        assert_eq!(State::load(&machine.context).unwrap().records().count(), 1);
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    fn unsupported_at(target: Target) -> Action {
        Action::unsupported(
            ItemRef {
                kind: ItemKind::Mcp,
                id: "exact".to_owned(),
                agent: Some("custom".to_owned()),
            },
            target,
            "this agent can't take it".to_owned(),
        )
    }

    #[cfg(unix)]
    fn with_unsupported(
        machine: &Machine,
        desired: Vec<Desired>,
        unsupported: Vec<Action>,
        state: &State,
    ) -> (Vec<Action>, Diagnostics) {
        let mut problems = Diagnostics::default();
        let actions = actions(
            Produced {
                desired,
                unsupported,
            },
            state,
            &machine.context,
            &machine.loadout,
            &mut problems,
        )
        .unwrap();
        (actions, problems)
    }

    #[cfg(unix)]
    fn guarded_by_old(action: &Action) -> bool {
        matches!(&action.change, Change::Blocked(reason) if reason.contains("still owns") && reason.contains("as json cursor-json"))
    }

    /// An owned key whose exact declaration is unsupported here: nothing writes it, but the record
    /// stays and keeps owning it under its old contract. Through a link, a declaration of another
    /// key in another format, or a whole-file writer of that document, is blocked; a compatible
    /// key, an old compatible removal, and the same picture without the record are not. The
    /// unsupported action comes from `Produced.unsupported`, so the declaration order is fixed;
    /// both link orientations are covered.
    #[cfg(unix)]
    #[test]
    fn an_exact_unsupported_owner_keeps_guarding_its_document_under_the_old_contract() {
        for (owned_dir, other_dir) in [("alias", "real"), ("real", "alias")] {
            let machine = machine();
            linked(&machine);
            let in_dir = |dir: &str| machine.home.join(dir).join("custom.json");
            let owned = mcp_at(
                in_dir(owned_dir),
                McpFormat::Json,
                "/mcpServers",
                "docs",
                McpShape::CursorJson,
            );
            let old_compatible = mcp_at(
                in_dir(owned_dir),
                McpFormat::Json,
                "/mcpServers",
                "old",
                McpShape::CursorJson,
            );
            let incompatible = mcp_at(
                in_dir(other_dir),
                McpFormat::Jsonc,
                "/mcpServers",
                "search",
                McpShape::CursorJson,
            );
            let compatible = mcp_at(
                in_dir(other_dir),
                McpFormat::Json,
                "/mcpServers",
                "search",
                McpShape::CursorJson,
            );
            let whole_file = Desired {
                item: ItemRef {
                    kind: ItemKind::File,
                    id: "whole".to_owned(),
                    agent: None,
                },
                target: Target::Path {
                    path: in_dir(other_dir),
                },
                content: super::super::Content::File(super::super::FileContent {
                    bytes: b"{}".to_vec(),
                    executable: false,
                }),
            };
            let owners = owning(&machine, std::slice::from_ref(&owned));

            let (actions, problems) = with_unsupported(
                &machine,
                vec![desired(incompatible.clone())],
                vec![unsupported_at(owned.clone())],
                &owners,
            );
            assert!(problems.is_empty(), "{problems:?}");
            assert_eq!(
                actions.len(),
                2,
                "no removal and no second action for the old record: {actions:#?}"
            );
            assert!(
                matches!(of(&actions, &owned).change, Change::Unsupported(_)),
                "unsupported stays as it is"
            );
            let blocked = of(&actions, &incompatible);
            assert!(guarded_by_old(blocked), "{:?}", reasons(&actions));
            assert!(
                only_guarded_by(blocked, &owned),
                "the old owner's guard is the whole refusal, not a mask: {:?}",
                reasons(&actions)
            );
            assert!(blocked.content.is_none());
            super::super::Plan { actions }.check().unwrap();

            let (actions, problems) = with_unsupported(
                &machine,
                vec![whole_file.clone()],
                vec![unsupported_at(owned.clone())],
                &owners,
            );
            assert!(problems.is_empty(), "{problems:?}");
            let path = of(&actions, &whole_file.target);
            assert!(
                guarded_by_old(path),
                "a whole-file writer of the owned document: {:?}",
                reasons(&actions)
            );
            assert!(path.content.is_none());
            assert_eq!(
                path.prior,
                Observed::Absent,
                "the prior stays what was observed"
            );
            super::super::Plan { actions }.check().unwrap();

            let (actions, problems) = with_unsupported(
                &machine,
                vec![desired(compatible.clone())],
                vec![unsupported_at(owned.clone())],
                &owners,
            );
            assert!(problems.is_empty(), "{problems:?}");
            assert!(
                !guarded_by_old(of(&actions, &compatible)),
                "same format, another key: {:?}",
                reasons(&actions)
            );
            super::super::Plan { actions }.check().unwrap();

            let both = owning(&machine, &[owned.clone(), old_compatible.clone()]);
            let (actions, problems) = with_unsupported(
                &machine,
                Vec::new(),
                vec![unsupported_at(owned.clone())],
                &both,
            );
            assert!(problems.is_empty(), "{problems:?}");
            let removal = of(&actions, &old_compatible);
            assert!(
                !matches!(&removal.change, Change::Blocked(reason) if reason.contains("still owns") || reason.contains("loadout owns")),
                "an old compatible key is judged against the old contract, which it doesn't touch: {:?}",
                reasons(&actions)
            );
            super::super::Plan { actions }.check().unwrap();

            let unowned = State::default();
            let (actions, problems) = with_unsupported(
                &machine,
                vec![desired(incompatible.clone())],
                vec![unsupported_at(owned.clone())],
                &unowned,
            );
            assert!(problems.is_empty(), "{problems:?}");
            assert!(
                !guarded_by_old(of(&actions, &incompatible)),
                "nothing is owned: {:?}",
                reasons(&actions)
            );
            super::super::Plan { actions }.check().unwrap();
            assert_eq!(
                State::load(&machine.context).unwrap().records().count(),
                2,
                "the records are never changed"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_still_declared_owned_key_beside_a_distinct_key_or_the_same_contract_keeps_the_writer_rules()
     {
        let machine = machine();
        linked(&machine);
        let owned = mcp_at(
            machine.home.join("alias/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "my_docs",
            McpShape::CursorJson,
        );
        let state = owning(&machine, std::slice::from_ref(&owned));

        let distinct = mcp_at(
            machine.home.join("real/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "other",
            McpShape::PiJson,
        );
        let (actions, problems) = compared(
            &machine,
            vec![desired(owned.clone()), desired(distinct.clone())],
            &state,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert!(
            !migrated(of(&actions, &distinct)),
            "a distinct stored key: {:?}",
            reasons(&actions)
        );
        super::super::Plan { actions }.check().unwrap();

        let same = mcp_at(
            machine.home.join("real/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "my_docs",
            McpShape::CursorJson,
        );
        let (_, problems) = compared(&machine, vec![desired(owned), desired(same)], &state);
        assert!(
            problems.has_errors(),
            "two current writers of one key stay invalid"
        );
    }

    /// Two old contracts on one stored key (two spellings of one document) both guard it, and
    /// the declaration keeps every reason once, after its first refusal.
    #[cfg(unix)]
    #[test]
    fn every_old_contract_on_one_stored_key_adds_its_own_reason_once() {
        let machine = machine();
        linked(&machine);
        let cursor = mcp_at(
            machine.home.join("alias/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "docs",
            McpShape::CursorJson,
        );
        let claude = mcp_at(
            machine.home.join("real/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "docs",
            McpShape::ClaudeJson,
        );
        let declared = mcp_at(
            machine.home.join("real/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "docs",
            McpShape::PiJson,
        );
        let state = owning(&machine, &[cursor.clone(), claude.clone()]);
        let (actions, problems) = compared(&machine, vec![desired(declared.clone())], &state);
        assert!(problems.is_empty(), "{problems:?}");
        let Change::Blocked(reason) = &of(&actions, &declared).change else {
            panic!("{actions:#?}");
        };
        assert!(
            reason.starts_with("loadout owns "),
            "nothing refuses the real entry before its migrations: {reason}"
        );
        assert_eq!(reason.matches("as json cursor-json").count(), 1, "{reason}");
        assert_eq!(reason.matches("as json claude-json").count(), 1, "{reason}");
        let (by_cursor, by_claude) = (
            migration_reason(&cursor, &declared),
            migration_reason(&claude, &declared),
        );
        assert!(
            *reason == format!("{by_cursor}; {by_claude}")
                || *reason == format!("{by_claude}; {by_cursor}"),
            "exactly the two migrations, each once, and nothing else: {reason}"
        );
        super::super::Plan { actions }.check().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_owned_key_reached_through_a_link_under_the_same_contract_is_no_migration() {
        let machine = machine();
        linked(&machine);
        let owned = mcp_at(
            machine.home.join("alias/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "my_docs",
            McpShape::CursorJson,
        );
        let declared = mcp_at(
            machine.home.join("real/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "my_docs",
            McpShape::CursorJson,
        );
        let state = owning(&machine, std::slice::from_ref(&owned));
        let (actions, problems) = compared(&machine, vec![desired(declared.clone())], &state);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(
            !migrated(of(&actions, &declared)),
            "{:?}",
            reasons(&actions)
        );
        assert!(!migrated(of(&actions, &owned)), "{:?}", reasons(&actions));
        super::super::Plan { actions }.check().unwrap();
    }

    /// The old record keeps owning its key under the OLD format. A sibling written under the
    /// new format would put two formats in one document, so it is blocked; a sibling under the
    /// old format, an unsupported one, and an old sibling's removal stay as they are.
    #[test]
    fn a_retained_old_contract_guards_the_document_against_siblings_under_another_format() {
        let machine = machine();
        let file = machine.home.join("custom.json");
        let at = |format, key: &str| {
            mcp_at(
                file.clone(),
                format,
                "/mcpServers",
                key,
                McpShape::CursorJson,
            )
        };
        let owned_docs = at(McpFormat::Json, "docs");
        let owned_search = at(McpFormat::Json, "search");
        let declared_docs = at(McpFormat::Jsonc, "docs");
        let jsonc_sibling = at(McpFormat::Jsonc, "other");
        let plain_sibling = at(McpFormat::Json, "third");

        for order in [
            vec![
                desired(declared_docs.clone()),
                desired(jsonc_sibling.clone()),
            ],
            vec![
                desired(jsonc_sibling.clone()),
                desired(declared_docs.clone()),
            ],
        ] {
            let state = owning(&machine, std::slice::from_ref(&owned_docs));
            let (actions, problems) = compared(&machine, order, &state);
            assert!(problems.is_empty(), "{problems:?}");
            assert_eq!(
                actions.len(),
                2,
                "one migration action and the sibling, no removal: {actions:#?}"
            );
            assert!(
                only_migrated(of(&actions, &declared_docs), &owned_docs, &declared_docs),
                "{:?}",
                reasons(&actions)
            );
            assert!(
                only_guarded_by(of(&actions, &jsonc_sibling), &owned_docs),
                "the old json contract alone blocks the jsonc sibling: {:?}",
                reasons(&actions)
            );
            super::super::Plan { actions }.check().unwrap();
        }

        let state = owning(&machine, &[owned_docs.clone(), owned_search.clone()]);
        let (actions, problems) = compared(&machine, vec![desired(declared_docs.clone())], &state);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            actions.len(),
            2,
            "the migration and the old search's removal: {actions:#?}"
        );
        assert!(
            !matches!(&of(&actions, &owned_search).change, Change::Blocked(reason) if reason.contains("still owns") || reason.contains("loadout owns")),
            "the old json search is judged against the old json docs, which it doesn't touch, not against the new jsonc declaration: {:?}",
            reasons(&actions)
        );
        super::super::Plan { actions }.check().unwrap();

        let state = owning(&machine, std::slice::from_ref(&owned_docs));
        let (actions, problems) = compared(
            &machine,
            vec![
                desired(declared_docs.clone()),
                desired(plain_sibling.clone()),
            ],
            &state,
        );
        assert!(
            problems.has_errors(),
            "two current formats in one document stay invalid"
        );
        drop(actions);

        let state = owning(&machine, std::slice::from_ref(&owned_docs));
        let unsupported = Action::unsupported(
            ItemRef {
                kind: ItemKind::Mcp,
                id: "other".to_owned(),
                agent: Some("cursor".to_owned()),
            },
            jsonc_sibling.clone(),
            "this agent can't take it".to_owned(),
        );
        let mut problems = Diagnostics::default();
        let actions = super::actions(
            Produced {
                desired: vec![desired(declared_docs.clone())],
                unsupported: vec![unsupported],
            },
            &state,
            &machine.context,
            &machine.loadout,
            &mut problems,
        )
        .unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert!(matches!(
            of(&actions, &jsonc_sibling).change,
            Change::Unsupported(_)
        ));
        super::super::Plan { actions }.check().unwrap();
    }

    #[test]
    fn an_unchanged_contract_retains_nothing_and_guards_no_compatible_sibling() {
        let machine = machine();
        let file = machine.home.join("custom.json");
        let docs = mcp_at(
            file.clone(),
            McpFormat::Json,
            "/mcpServers",
            "docs",
            McpShape::CursorJson,
        );
        let search = mcp_at(
            file,
            McpFormat::Json,
            "/mcpServers",
            "search",
            McpShape::CursorJson,
        );
        let state = owning(&machine, std::slice::from_ref(&docs));
        let (actions, problems) = compared(
            &machine,
            vec![desired(docs.clone()), desired(search.clone())],
            &state,
        );
        assert!(problems.is_empty(), "{problems:?}");
        for target in [&docs, &search] {
            assert!(
                !matches!(&of(&actions, target).change, Change::Blocked(reason) if reason.contains("still owns") || reason.contains("doesn't convert")),
                "{:?}",
                reasons(&actions)
            );
        }
        super::super::Plan { actions }.check().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_retained_old_contract_reached_through_a_link_guards_siblings_too() {
        let machine = machine();
        linked(&machine);
        let owned = mcp_at(
            machine.home.join("alias/custom.json"),
            McpFormat::Json,
            "/mcpServers",
            "docs",
            McpShape::CursorJson,
        );
        let declared = mcp_at(
            machine.home.join("real/custom.json"),
            McpFormat::Jsonc,
            "/mcpServers",
            "docs",
            McpShape::CursorJson,
        );
        let sibling = mcp_at(
            machine.home.join("real/custom.json"),
            McpFormat::Jsonc,
            "/mcpServers",
            "search",
            McpShape::CursorJson,
        );
        let state = owning(&machine, std::slice::from_ref(&owned));
        let (actions, problems) = compared(
            &machine,
            vec![desired(sibling.clone()), desired(declared.clone())],
            &state,
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert!(
            only_migrated(of(&actions, &declared), &owned, &declared),
            "{:?}",
            reasons(&actions)
        );
        assert!(
            migrated_first(of(&actions, &owned), &owned, &declared),
            "the old removal's migration comes first; its conflict with the jsonc sibling may follow: {:?}",
            reasons(&actions)
        );
        assert!(
            only_guarded_by(of(&actions, &sibling), &owned),
            "the retained old contract alone blocks the sibling: {:?}",
            reasons(&actions)
        );
        super::super::Plan { actions }.check().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn two_declared_files_through_a_link_are_invalid_in_the_full_comparison() {
        let machine = machine();
        std::fs::create_dir(machine.home.join("real")).unwrap();
        std::os::unix::fs::symlink("real", machine.home.join("alias")).unwrap();
        let file = |path: &str| Desired {
            item: ItemRef {
                kind: ItemKind::File,
                id: path.replace('/', "-"),
                agent: None,
            },
            target: Target::Path {
                path: machine.home.join(path),
            },
            content: super::super::Content::File(super::super::FileContent {
                bytes: b"same".to_vec(),
                executable: false,
            }),
        };
        let (_, problems) = compared(
            &machine,
            vec![file("real/tool"), file("alias/tool")],
            &State::default(),
        );
        assert!(
            problems.has_errors(),
            "equal bytes don't make two writers one"
        );
        let (actions, problems) = compared(
            &machine,
            vec![file("real/a"), file("alias/b")],
            &State::default(),
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert!(actions.iter().all(|action| action.change == Change::Create));
    }
}
