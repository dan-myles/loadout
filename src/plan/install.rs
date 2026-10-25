use std::collections::BTreeMap;

use super::{Action, Change, ItemKind, ItemRef, Observed, Options, Steps, Target, error};
use crate::diagnostic::Diagnostics;
use crate::manifest::Manager;
use crate::programs::State;

/// One action per program, in program id order. A step with a package keeps its real target; a
/// step with none (isolation, no available manager) has `Target::None`, so no manager is ever
/// made up. Two programs that pick the same package on this machine are a loadout problem.
pub(super) fn actions(steps: &Steps, options: Options, problems: &mut Diagnostics) -> Vec<Action> {
    let mut seen: BTreeMap<(Manager, String), &str> = BTreeMap::new();
    let mut actions = Vec::new();
    for (id, step) in &steps.by_program {
        let item = ItemRef {
            kind: ItemKind::Program,
            id: id.clone(),
            agent: None,
        };
        let package = match &step.state {
            State::Installed(package) => Some(package),
            State::Missing(missing) => Some(missing.package()),
            State::Unsupported(_) | State::Blocked(_) => None,
        };
        if let Some(package) = package {
            let key = (package.manager(), package.id().to_owned());
            if let Some(first) = seen.get(&key) {
                problems.push(error(
                    &format!("programs.{id}"),
                    format!(
                        "programs.{first} and programs.{id} both install {} package {:?} on this machine",
                        package.manager().id(),
                        package.id()
                    ),
                ));
                continue;
            }
            seen.insert(key, id);
        }
        let (target, change, prior) = match &step.state {
            State::Installed(package) => (
                program(package.manager(), package.id()),
                Change::Unchanged,
                Observed::Installed,
            ),
            State::Missing(missing) => {
                let package = missing.package();
                let change = if step.needs_elevation() && !options.elevated {
                    Change::Blocked(format!(
                        "{} installs {:?} system-wide, which needs `loadout apply --elevated`",
                        package.manager().id(),
                        package.id()
                    ))
                } else {
                    Change::Install
                };
                (
                    program(package.manager(), package.id()),
                    change,
                    Observed::Absent,
                )
            }
            State::Unsupported(reason) => (
                Target::None,
                Change::Unsupported(reason.clone()),
                Observed::Absent,
            ),
            State::Blocked(reason) => (
                Target::None,
                Change::Blocked(reason.clone()),
                Observed::Absent,
            ),
        };
        actions.push(Action {
            item,
            target,
            change,
            prior,
            content: None,
        });
    }
    actions
}

fn program(manager: Manager, package: &str) -> Target {
    Target::Program {
        manager,
        package: package.to_owned(),
    }
}
