use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::diagnostic::Diagnostics;
use crate::plan::{self, Action, Change, Content, Plan, Target};
use crate::programs::{self, Installed};
use crate::state::{self, Lock, Removal, Run, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `loadout apply`: may replace an unowned target after a backup, and honours `--force` and
    /// `--elevated`.
    Interactive,
    /// The background update: never replaces, forces, elevates, or prompts.
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    pub force: bool,
    pub elevated: bool,
    pub mode: Mode,
}

impl Options {
    fn force(self) -> bool {
        self.force && self.mode == Mode::Interactive
    }

    fn elevated(self) -> bool {
        self.elevated && self.mode == Mode::Interactive
    }

    fn plan(self) -> plan::Options {
        plan::Options {
            elevated: self.elevated(),
        }
    }
}

/// What happened to one planned action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Adopted,
    Removed,
    Released,
    Unchanged,
    Installed,
    AlreadyInstalled,
    Unsupported(String),
    /// Planned, but this run doesn't do it: drift without `--force`, or a replace in the
    /// background.
    Skipped(String),
    Blocked(String),
    /// Written, but the target doesn't hold what was written.
    Mismatch(String),
    Failed(String),
}

#[derive(Debug)]
pub struct Report {
    pub checkout: PathBuf,
    pub machine: String,
    pub run: Option<String>,
    pub outcomes: Vec<(Action, Outcome)>,
    pub warnings: Diagnostics,
    /// Saving the ownership records failed after the run's writes. No program was installed.
    pub finish_error: Option<String>,
}

impl Report {
    /// A failed action or a failed finish: the command exits 1.
    pub fn failed(&self) -> bool {
        self.finish_error.is_some()
            || self
                .outcomes
                .iter()
                .any(|(_, outcome)| matches!(outcome, Outcome::Failed(_)))
    }

    /// Something was left undone or doesn't match what was written: the command exits 4.
    pub fn unhealthy(&self) -> bool {
        self.outcomes.iter().any(|(_, outcome)| {
            matches!(
                outcome,
                Outcome::Blocked(_) | Outcome::Skipped(_) | Outcome::Mismatch(_)
            )
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Plan(#[from] plan::Error),
    #[error(transparent)]
    State(#[from] state::Error),
}

/// `loadout apply`. A read-only plan comes first, so an invalid loadout, a missing template
/// variable, or an interrupted run ends with nothing written, not even the lock. Only then is
/// the lock taken and the plan rebuilt under it.
pub fn run(context: &Context, checkout: &Path, options: Options) -> Result<Report, Error> {
    let state = State::load(context)?;
    let preflight = plan::build(context, checkout, &state, None, options.plan())?;
    let lock = state::lock(context)?;
    under_lock(context, checkout, lock, options, Some(preflight.steps))
}

/// For a caller that already holds the lock, such as `update` across its fetch. The lock must be
/// the one for `context`'s state dir; that is checked before anything else, on every route,
/// including a run with nothing to write. Everything is then planned again under it. An invalid
/// loadout found here ends with no destination written; taking the lock may already have written
/// the state dir if it was missing, and its lock file.
pub fn run_under_lock(
    context: &Context,
    checkout: &Path,
    lock: Lock,
    options: Options,
) -> Result<Report, Error> {
    under_lock(context, checkout, lock, options, None)
}

fn under_lock(
    context: &Context,
    checkout: &Path,
    lock: Lock,
    options: Options,
    steps: Option<plan::Steps>,
) -> Result<Report, Error> {
    lock.check(context)?;
    let mut state = State::load(context)?;
    let built = plan::build(context, checkout, &state, steps, options.plan())?;
    built
        .plan
        .check()
        .map_err(|message| plan::Error::Runtime(anyhow::anyhow!("internal error: {message}")))?;
    let actions = &built.plan.actions;

    let mut outcomes: Vec<Option<Outcome>> = vec![None; actions.len()];
    let mut journaled = Vec::new();
    let mut installs = Vec::new();
    for (index, action) in actions.iter().enumerate() {
        match disposition(action, options) {
            Disposition::Journal => journaled.push(index),
            Disposition::Install => installs.push(index),
            Disposition::Report(outcome) => outcomes[index] = Some(outcome),
        }
    }
    journaled.sort_by_key(|index| actions[*index].change != Change::Remove);

    // Programs are never journaled. Installing them only after the run is finished means a
    // hung installer that has to be killed leaves a finished journal, not an interrupted one.
    let (lock, run, finish_error) = if journaled.is_empty() {
        (lock, None, None)
    } else {
        let filtered = Plan {
            actions: journaled.iter().map(|index| actions[*index].clone()).collect(),
        };
        let mut run = Run::begin(context, &built.checkout, lock, &state, &filtered)?;
        let id = run.id().to_owned();
        for index in &journaled {
            outcomes[*index] = Some(execute(&mut run, context, &built.checkout, &actions[*index]));
        }
        let (lock, finished) = run.finish_with_lock(&mut state);
        (lock, Some(id), finished.err().map(|error| error.to_string()))
    };

    for index in &installs {
        outcomes[*index] = Some(match &finish_error {
            Some(error) => Outcome::Skipped(format!(
                "loadout couldn't save its ownership records ({error}), so it installed no program"
            )),
            None => install(&built.steps, &actions[*index], options),
        });
    }

    let report = Report {
        checkout: built.checkout.clone(),
        machine: built.machine.clone(),
        run,
        outcomes: actions
            .iter()
            .cloned()
            .zip(outcomes)
            .map(|(action, outcome)| {
                let outcome = outcome.unwrap_or_else(|| {
                    Outcome::Failed("internal error: the action was never handled".to_owned())
                });
                (action, outcome)
            })
            .collect(),
        warnings: built.warnings.clone(),
        finish_error,
    };
    drop(lock);
    Ok(report)
}

enum Disposition {
    Journal,
    Install,
    Report(Outcome),
}

/// Only file, tree, and entry actions that may write reach the journal. Programs install after
/// it; unchanged, unsupported, and blocked actions, and every `Target::None`, are only reported.
fn disposition(action: &Action, options: Options) -> Disposition {
    let concrete = matches!(action.target, Target::Path { .. } | Target::Entry { .. });
    match &action.change {
        Change::Unsupported(reason) => Disposition::Report(Outcome::Unsupported(reason.clone())),
        Change::Blocked(reason) => Disposition::Report(Outcome::Blocked(reason.clone())),
        Change::Unchanged => Disposition::Report(Outcome::Unchanged),
        Change::Install if matches!(action.target, Target::Program { .. }) => Disposition::Install,
        _ if !concrete => Disposition::Report(Outcome::Failed(
            "internal error: only a file, folder, or MCP entry can be written".to_owned(),
        )),
        Change::Create | Change::Update | Change::Adopt | Change::Remove => Disposition::Journal,
        Change::Replace => {
            if options.mode == Mode::Background {
                Disposition::Report(Outcome::Skipped(
                    "background runs never replace a target loadout doesn't own; run `loadout apply`"
                        .to_owned(),
                ))
            } else {
                Disposition::Journal
            }
        }
        Change::Drifted => {
            if options.force() {
                Disposition::Journal
            } else {
                Disposition::Report(Outcome::Skipped(
                    "it changed since loadout wrote it; `loadout apply --force` overwrites it after a backup"
                        .to_owned(),
                ))
            }
        }
        Change::Install => Disposition::Report(Outcome::Failed(
            "internal error: only a program is installed".to_owned(),
        )),
    }
}

fn execute(run: &mut Run, context: &Context, checkout: &Path, action: &Action) -> Outcome {
    if action.change == Change::Remove {
        return match run.remove(action) {
            Ok(Removal::Removed) => Outcome::Removed,
            Ok(Removal::Released) => Outcome::Released,
            Err(error) => failure(&error),
        };
    }
    let Some(content) = &action.content else {
        return Outcome::Failed("internal error: a write without content".to_owned());
    };
    if let Err(error) = run.write(action, content) {
        return failure(&error);
    }
    if action.change == Change::Adopt {
        return Outcome::Adopted;
    }
    verify(context, checkout, action, content)
}

/// A file system that ignores Unix modes, such as FAT, can't keep the execute bit loadout set,
/// and nothing read-only could have told the plan. Reading the target back is where it shows.
fn verify(context: &Context, checkout: &Path, action: &Action, content: &Content) -> Outcome {
    if !matches!(action.target, Target::Path { .. }) {
        return Outcome::Done;
    }
    let label = state::label(&action.target);
    match (
        state::digest(content),
        state::observe(context, checkout, &action.target),
    ) {
        (Ok(wanted), Ok(now)) if wanted == now => Outcome::Done,
        (Ok(_), Ok(_)) => Outcome::Mismatch(format!(
            "{label} doesn't hold what loadout wrote; its file system may not keep execute bits"
        )),
        (Err(error), _) | (_, Err(error)) => {
            Outcome::Mismatch(format!("cannot check what loadout wrote to {label}: {error}"))
        }
    }
}

fn failure(error: &state::Error) -> Outcome {
    if error.is_blocked() {
        Outcome::Blocked(error.to_string())
    } else {
        Outcome::Failed(error.to_string())
    }
}

fn install(steps: &plan::Steps, action: &Action, options: Options) -> Outcome {
    let Some(step) = steps.get(&action.item.id) else {
        return Outcome::Failed(format!(
            "internal error: no program step for programs.{}",
            action.item.id
        ));
    };
    match programs::install(step, options.elevated()) {
        Ok(Installed::Now) => Outcome::Installed,
        Ok(Installed::Already) => Outcome::AlreadyInstalled,
        Err(error @ (programs::Error::Changed { .. } | programs::Error::NeedsElevation { .. })) => {
            Outcome::Blocked(error.to_string())
        }
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::manifest::Manager;
    use crate::plan::{FileContent, ItemKind, ItemRef, Observed};

    fn action(target: Target, change: Change) -> Action {
        Action {
            item: ItemRef {
                kind: ItemKind::File,
                id: "x".to_owned(),
                agent: None,
            },
            target,
            change,
            prior: Observed::Absent,
            content: Some(Content::File(FileContent {
                bytes: b"x".to_vec(),
                executable: false,
            })),
        }
    }

    fn path() -> Target {
        Target::Path {
            path: PathBuf::from("/h/.a"),
        }
    }

    const INTERACTIVE: Options = Options {
        force: false,
        elevated: false,
        mode: Mode::Interactive,
    };
    const BACKGROUND_FORCED: Options = Options {
        force: true,
        elevated: true,
        mode: Mode::Background,
    };

    fn journaled(action: &Action, options: Options) -> bool {
        matches!(disposition(action, options), Disposition::Journal)
    }

    #[test]
    fn only_writable_file_and_entry_actions_reach_the_journal() {
        for change in [Change::Create, Change::Update, Change::Adopt, Change::Remove] {
            assert!(journaled(&action(path(), change.clone()), INTERACTIVE), "{change:?}");
        }
        for change in [
            Change::Unchanged,
            Change::Blocked("b".to_owned()),
            Change::Unsupported("u".to_owned()),
        ] {
            assert!(!journaled(&action(path(), change.clone()), INTERACTIVE), "{change:?}");
            assert!(!journaled(&action(Target::None, change.clone()), INTERACTIVE), "{change:?}");
        }
        let program = action(
            Target::Program {
                manager: Manager::Brew,
                package: "ripgrep".to_owned(),
            },
            Change::Install,
        );
        assert!(matches!(
            disposition(&program, INTERACTIVE),
            Disposition::Install
        ));
        let forged = action(Target::None, Change::Create);
        assert!(matches!(
            disposition(&forged, INTERACTIVE),
            Disposition::Report(Outcome::Failed(_))
        ));
    }

    #[test]
    fn the_background_never_replaces_or_forces_and_drift_needs_force() {
        let replace = action(path(), Change::Replace);
        let drifted = action(path(), Change::Drifted);
        assert!(journaled(&replace, INTERACTIVE));
        assert!(!journaled(&drifted, INTERACTIVE));
        let forced = Options {
            force: true,
            ..INTERACTIVE
        };
        assert!(journaled(&drifted, forced));
        assert!(!journaled(&replace, BACKGROUND_FORCED));
        assert!(!journaled(&drifted, BACKGROUND_FORCED));
        assert!(!BACKGROUND_FORCED.elevated());
        assert!(!BACKGROUND_FORCED.plan().elevated);
    }

    #[test]
    fn failures_exit_one_and_anything_left_undone_exits_four() {
        let report = |outcome: Outcome, finish_error: Option<&str>| Report {
            checkout: PathBuf::from("/c"),
            machine: "m".to_owned(),
            run: None,
            outcomes: vec![(action(path(), Change::Create), outcome)],
            warnings: Diagnostics::default(),
            finish_error: finish_error.map(str::to_owned),
        };
        assert!(!report(Outcome::Done, None).failed());
        assert!(!report(Outcome::Done, None).unhealthy());
        assert!(!report(Outcome::Unsupported("u".to_owned()), None).unhealthy());
        for undone in [
            Outcome::Blocked("b".to_owned()),
            Outcome::Skipped("s".to_owned()),
            Outcome::Mismatch("m".to_owned()),
        ] {
            assert!(report(undone, None).unhealthy());
        }
        assert!(report(Outcome::Failed("f".to_owned()), None).failed());
        assert!(report(Outcome::Done, Some("disk full")).failed());
    }
}
