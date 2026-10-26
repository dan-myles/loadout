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
    /// The background sync: never replaces, forces, elevates, or prompts.
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
    /// The run was cancelled before its report: a supervised background run whose supervisor
    /// stopped it, or one a failed manager query cancelled. It started no work after it saw
    /// that, and reports what it didn't do as skipped. Only this report carries it; it is never
    /// saved.
    pub cancelled: bool,
}

/// Why an action or install a cancelled run didn't start is skipped.
pub(crate) const CANCELLED: &str =
    "the background run was cancelled, so loadout started no further work";

impl Report {
    /// A failed action or a failed finish: the command exits 1, whether or not the run was
    /// also cancelled.
    pub fn failed(&self) -> bool {
        self.finish_error.is_some()
            || self
                .outcomes
                .iter()
                .any(|(_, outcome)| matches!(outcome, Outcome::Failed(_)))
    }

    /// Something was left undone or doesn't match what was written, or the run was cancelled:
    /// the command exits 4. A cancelled run with nothing to do is never a clean success.
    pub fn unhealthy(&self) -> bool {
        self.cancelled
            || self.outcomes.iter().any(|(_, outcome)| {
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
    //
    // A cancelled run (only a supervised background run can be) starts no new work: no run
    // when nothing has begun, no further journaled action, and no install. Each is checked just
    // before it would start, so a cancellation that arrives while one runs doesn't stop that
    // one, and U8's own refusal still guards a query or installer that starts after the check.
    // A run that has begun is always finished, so its journal is complete, and its lock is
    // handed back and held through the installs and the report.
    let cancelled = || context.containment.is_cancelled();
    let (lock, run, finish_error) = if journaled.is_empty() || cancelled() {
        for index in &journaled {
            outcomes[*index] = Some(Outcome::Skipped(CANCELLED.to_owned()));
        }
        (lock, None, None)
    } else {
        let filtered = Plan {
            actions: journaled
                .iter()
                .map(|index| actions[*index].clone())
                .collect(),
        };
        let mut run = Run::begin(context, &built.checkout, lock, &state, &filtered)?;
        let id = run.id().to_owned();
        #[cfg(test)]
        observe::at(observe::Boundary::Begun, context);
        for index in &journaled {
            if cancelled() {
                outcomes[*index] = Some(Outcome::Skipped(CANCELLED.to_owned()));
                continue;
            }
            outcomes[*index] = Some(execute(
                &mut run,
                context,
                &built.checkout,
                &actions[*index],
            ));
            #[cfg(test)]
            observe::at(observe::Boundary::Action, context);
        }
        let (lock, finished) = run.finish_with_lock(&mut state);
        #[cfg(test)]
        observe::at(observe::Boundary::Finished, context);
        (
            lock,
            Some(id),
            finished.err().map(|error| error.to_string()),
        )
    };

    for index in &installs {
        outcomes[*index] = Some(match &finish_error {
            Some(error) => Outcome::Skipped(format!(
                "loadout couldn't save its ownership records ({error}), so it installed no program"
            )),
            None if cancelled() => Outcome::Skipped(CANCELLED.to_owned()),
            None => {
                #[cfg(test)]
                observe::at(observe::Boundary::Install, context);
                install(&built.steps, &actions[*index], options)
            }
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
        // The cancellation flag is never cleared. Set by now, the report says the run was
        // cancelled, even when it was set after the last step and nothing was left to skip.
        cancelled: cancelled(),
    };
    #[cfg(test)]
    observe::at(observe::Boundary::Report, context);
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
    let Some(wanted) = &action.content else {
        return Outcome::Failed("internal error: a write without content".to_owned());
    };
    if let Err(error) = run.write(action, wanted) {
        return failure(&error);
    }
    if action.change == Change::Adopt {
        return Outcome::Adopted;
    }
    verify(context, checkout, action, wanted)
}

/// A file system that ignores Unix modes, such as FAT, can't keep the execute bit loadout set,
/// and nothing read-only could have told the plan. Reading the target back is where it shows.
fn verify(context: &Context, checkout: &Path, action: &Action, wanted: &Content) -> Outcome {
    if !matches!(action.target, Target::Path { .. }) {
        return Outcome::Done;
    }
    let label = state::label(&action.target);
    match (
        state::digest(wanted),
        state::observe(context, checkout, &action.target),
    ) {
        (Ok(wanted), Ok(now)) if wanted == now => Outcome::Done,
        (Ok(_), Ok(_)) => Outcome::Mismatch(format!(
            "{label} doesn't hold what loadout wrote; its file system may not keep execute bits"
        )),
        (Err(error), _) | (_, Err(error)) => Outcome::Mismatch(format!(
            "cannot check what loadout wrote to {label}: {error}"
        )),
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
        Err(error @ programs::Error::Unconfirmed { .. }) => Outcome::Skipped(error.to_string()),
        Err(programs::Error::NotStarted { .. }) => Outcome::Skipped(CANCELLED.to_owned()),
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

/// The points `under_lock` reaches, for this module's own unit tests: a test sets a hook that
/// runs at each one on the current thread, to observe the lock or change the fixture there.
/// It doesn't exist outside a test build, so production never branches on it.
#[cfg(test)]
mod observe {
    use std::cell::RefCell;

    use crate::context::Context;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Boundary {
        /// The run has begun and no journaled action has started.
        Begun,
        /// A journaled action has just been handled.
        Action,
        /// The begun run is finished and its lock is back.
        Finished,
        /// An install is about to start.
        Install,
        /// The report is built and the lock is about to be released.
        Report,
    }

    type Hook = Box<dyn FnMut(Boundary, &Context)>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Runs `hook` at every boundary this thread reaches until the guard is dropped.
    pub(super) fn with(hook: impl FnMut(Boundary, &Context) + 'static) -> Guard {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        Guard
    }

    pub(super) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }

    pub(super) fn at(boundary: Boundary, context: &Context) {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(boundary, context);
            }
        });
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
        for change in [
            Change::Create,
            Change::Update,
            Change::Adopt,
            Change::Remove,
        ] {
            assert!(
                journaled(&action(path(), change.clone()), INTERACTIVE),
                "{change:?}"
            );
        }
        for change in [
            Change::Unchanged,
            Change::Blocked("b".to_owned()),
            Change::Unsupported("u".to_owned()),
        ] {
            assert!(
                !journaled(&action(path(), change.clone()), INTERACTIVE),
                "{change:?}"
            );
            assert!(
                !journaled(&action(Target::None, change.clone()), INTERACTIVE),
                "{change:?}"
            );
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
            cancelled: false,
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

        let cancelled = |outcomes: Vec<(Action, Outcome)>, finish_error: Option<&str>| Report {
            checkout: PathBuf::from("/c"),
            machine: "m".to_owned(),
            run: None,
            outcomes,
            warnings: Diagnostics::default(),
            finish_error: finish_error.map(str::to_owned),
            cancelled: true,
        };
        let empty = cancelled(Vec::new(), None);
        assert!(
            empty.unhealthy() && !empty.failed(),
            "a cancelled run with nothing to do exits 4, never 0"
        );
        let failed = cancelled(
            vec![(
                action(path(), Change::Create),
                Outcome::Failed("f".to_owned()),
            )],
            None,
        );
        assert!(failed.failed(), "a real failure still exits 1");
        assert!(cancelled(Vec::new(), Some("disk full")).failed());
    }

    /// A synthetic machine for the tests below: a home, a state dir that doesn't exist yet, and
    /// a loadout checkout with this manifest and these source files.
    struct Sandbox {
        _dir: tempfile::TempDir,
        home: PathBuf,
        loadout: PathBuf,
        context: Context,
    }

    fn sandbox(manifest: &str, files: &[(&str, &str)]) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let loadout = root.join("loadout");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&loadout).unwrap();
        std::fs::write(
            loadout.join("loadout.toml"),
            format!("[loadout]\nversion = 1\n{manifest}"),
        )
        .unwrap();
        for (name, text) in files {
            let path = loadout.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let context = isolated(&home, &root.join("state"));
        Sandbox {
            _dir: dir,
            home,
            loadout,
            context,
        }
    }

    fn isolated(home: &Path, state: &Path) -> Context {
        let vars = (home.as_os_str().to_owned(), state.as_os_str().to_owned());
        Context::resolve(
            crate::platform::Os::current().unwrap(),
            move |name| match name {
                "LOADOUT_HOME" => Some(vars.0.clone()),
                "LOADOUT_STATE_DIR" => Some(vars.1.clone()),
                _ => None,
            },
            None,
        )
        .unwrap()
    }

    /// The same context as a supervised background worker's, through U8's own crate-private
    /// test containment: a cancellation flag no supervisor sets, and no group anything signals.
    fn supervised(context: &Context) -> Context {
        Context {
            containment: crate::process::Containment::inherited_for_tests(),
            ..context.clone()
        }
    }

    fn cancel(context: &Context) {
        context
            .containment
            .cancel()
            .expect("only a supervised run can be cancelled")
            .cancel();
    }

    /// Whether the state lock is free within a few seconds. Other tests in this process spawn
    /// children in parallel, and each child holds a copy of this process's descriptors until it
    /// execs, so a lock dropped a moment ago can still look held. One that is really kept stays
    /// held past the deadline.
    fn released(context: &Context) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !state::busy(context).unwrap() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    const BACKGROUND: Options = Options {
        force: false,
        elevated: false,
        mode: Mode::Background,
    };

    const TWO_FILES: &str = "[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n[files.b]\nsource = \"b\"\ntarget = \"~/.b\"\n";

    fn skipped_by_cancel(outcome: &Outcome) -> bool {
        *outcome == Outcome::Skipped(CANCELLED.to_owned())
    }

    /// Cancelled before anything began: no run starts, so there is no journal to finish or
    /// roll back, nothing is written, and every planned write is skipped. The report says it
    /// was cancelled, so the command exits 4.
    #[test]
    fn a_run_cancelled_before_it_begins_starts_no_run_and_skips_every_write() {
        let sandbox = sandbox(TWO_FILES, &[("a", "first"), ("b", "second")]);
        let context = supervised(&sandbox.context);
        cancel(&context);
        let lock = state::lock(&context).unwrap();
        let report = run_under_lock(&context, &sandbox.loadout, lock, BACKGROUND).unwrap();
        assert!(report.cancelled, "{report:#?}");
        assert!(report.unhealthy() && !report.failed());
        assert_eq!(report.run, None, "no run began, so no journal exists");
        assert_eq!(report.outcomes.len(), 2);
        assert!(
            report
                .outcomes
                .iter()
                .all(|(_, outcome)| skipped_by_cancel(outcome)),
            "{report:#?}"
        );
        assert!(!sandbox.home.join(".a").exists() && !sandbox.home.join(".b").exists());
        assert!(state::runs(&context).unwrap().is_empty());
        assert_eq!(State::load(&context).unwrap().records().count(), 0);
        assert!(released(&context), "the lock is released");
    }

    /// Cancelled after the first journaled action: that action's outcome and record stay, the
    /// next one never starts, and the run is still finished, not left interrupted.
    #[test]
    fn a_run_cancelled_after_its_first_action_finishes_its_journal_and_starts_nothing_more() {
        let sandbox = sandbox(TWO_FILES, &[("a", "first"), ("b", "second")]);
        let context = supervised(&sandbox.context);
        let _hook = observe::with(|boundary, context| {
            if boundary == observe::Boundary::Action {
                cancel(context);
            }
        });
        let lock = state::lock(&context).unwrap();
        let report = run_under_lock(&context, &sandbox.loadout, lock, BACKGROUND).unwrap();
        assert!(report.cancelled && report.unhealthy() && !report.failed());
        let done: Vec<&Action> = report
            .outcomes
            .iter()
            .filter(|(_, outcome)| *outcome == Outcome::Done)
            .map(|(action, _)| action)
            .collect();
        let skipped = report
            .outcomes
            .iter()
            .filter(|(_, outcome)| skipped_by_cancel(outcome))
            .count();
        assert_eq!((done.len(), skipped), (1, 1), "{report:#?}");
        let written = [".a", ".b"]
            .into_iter()
            .filter(|name| sandbox.home.join(name).exists())
            .count();
        assert_eq!(written, 1, "the skipped write never started");
        let runs = state::runs(&context).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].status,
            Some(state::RunStatus::Finished),
            "the begun run is finished"
        );
        let records = State::load(&context).unwrap();
        assert_eq!(records.records().count(), 1);
        assert!(records.record(&done[0].target).is_some());
        assert!(released(&context));
    }

    /// A cancelled run with nothing to write is not a clean success, whether the loadout
    /// declares nothing or everything is already in place.
    #[test]
    fn a_cancelled_run_with_nothing_to_do_still_reports_the_cancellation() {
        let empty = sandbox("", &[]);
        let context = supervised(&empty.context);
        cancel(&context);
        let report = run_under_lock(
            &context,
            &empty.loadout,
            state::lock(&context).unwrap(),
            BACKGROUND,
        )
        .unwrap();
        assert!(report.outcomes.is_empty() && report.cancelled && report.unhealthy());
        assert!(!report.failed() && report.run.is_none());

        let settled = sandbox(TWO_FILES, &[("a", "first"), ("b", "second")]);
        let first = run_under_lock(
            &settled.context,
            &settled.loadout,
            state::lock(&settled.context).unwrap(),
            BACKGROUND,
        )
        .unwrap();
        assert!(!first.unhealthy(), "{first:#?}");
        assert!(released(&settled.context), "the first run let the lock go");
        let context = supervised(&settled.context);
        cancel(&context);
        let report = run_under_lock(
            &context,
            &settled.loadout,
            state::lock(&context).unwrap(),
            BACKGROUND,
        )
        .unwrap();
        assert!(
            report
                .outcomes
                .iter()
                .all(|(_, outcome)| *outcome == Outcome::Unchanged)
        );
        assert!(report.cancelled && report.unhealthy() && report.run.is_none());
        assert_eq!(state::runs(&context).unwrap().len(), 1, "no empty run");
    }

    /// The foreground has no cancellation: the same run with the hook in place does every
    /// write, and its report is clean.
    #[test]
    fn the_foreground_is_never_cancelled() {
        let sandbox = sandbox(TWO_FILES, &[("a", "first"), ("b", "second")]);
        let _hook = observe::with(|boundary, context| {
            if boundary == observe::Boundary::Action {
                assert!(
                    context.containment.cancel().is_none(),
                    "a foreground run has no cancellation flag to set"
                );
            }
        });
        let report = run_under_lock(
            &sandbox.context,
            &sandbox.loadout,
            state::lock(&sandbox.context).unwrap(),
            BACKGROUND,
        )
        .unwrap();
        assert!(!report.cancelled && !report.unhealthy() && !report.failed());
        assert!(
            report
                .outcomes
                .iter()
                .all(|(_, outcome)| *outcome == Outcome::Done)
        );
    }

    /// R3 with independent witnesses. The plan observer marks the interrupted-run gate, both
    /// producers, and the manager query as `build` reaches them. Every earlier gate is valid
    /// here (no interrupted run), so a refusal under a lock for another state dir can only be
    /// the lock's, and on every route it comes before any of those stages and any write. The
    /// same route under the right lock reaches the stages the markers watch, so they can see
    /// them. Under isolation the query stage asks `programs::steps` but no manager runs; the
    /// fake-manager control below covers a manager that would.
    #[test]
    fn a_wrong_lock_stops_every_route_before_the_gate_the_producers_and_the_managers() {
        use crate::plan::observe::{self as plan_observe, Stage};

        let all = vec![Stage::Gate, Stage::Files, Stage::Agents, Stage::Query];
        let poisoned = vec![Stage::Gate, Stage::Files, Stage::Agents];
        for (manifest, files, reached) in [
            ("", vec![], all.clone()),
            (
                "[programs.ripgrep]\nbrew = \"ripgrep\"\napt = \"ripgrep\"\nwinget = \"BurntSushi.ripgrep.MSVC\"\n",
                vec![],
                all.clone(),
            ),
            (
                "[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n",
                vec![("a", "x")],
                all.clone(),
            ),
            (
                "[agents]\nenabled = [\"claude\"]\n[mcp.docs]\ncommand = \"docs-server\"\n",
                vec![],
                all.clone(),
            ),
            (
                "[files.poison]\nsource = \"poison.tmpl\"\ntarget = \"~/.poison\"\n",
                vec![("poison.tmpl", "{{ never_defined }}")],
                poisoned,
            ),
        ] {
            let sandbox = sandbox(manifest, &files);
            std::fs::create_dir(&sandbox.context.state).unwrap();
            let other = tempfile::tempdir().unwrap();
            let wrong = state::lock(&isolated(&sandbox.home, &other.path().join("state"))).unwrap();
            plan_observe::take();
            let result = run_under_lock(&sandbox.context, &sandbox.loadout, wrong, INTERACTIVE);
            assert!(
                matches!(result, Err(Error::State(state::Error::WrongLock { .. }))),
                "{manifest:?}: {result:?}"
            );
            assert_eq!(
                plan_observe::take(),
                Vec::<Stage>::new(),
                "{manifest:?}: no gate, producer, or query under a wrong lock"
            );
            assert_eq!(
                std::fs::read_dir(&sandbox.home).unwrap().count(),
                0,
                "{manifest:?}"
            );
            assert_eq!(
                std::fs::read_dir(&sandbox.context.state).unwrap().count(),
                0,
                "{manifest:?}: the check created nothing"
            );

            let right = state::lock(&sandbox.context).unwrap();
            let result = run_under_lock(&sandbox.context, &sandbox.loadout, right, INTERACTIVE);
            assert_eq!(plan_observe::take(), reached, "{manifest:?}: {result:?}");
            if manifest.contains("poison") {
                assert!(
                    matches!(result, Err(Error::Plan(plan::Error::Invalid(_)))),
                    "{result:?}"
                );
            } else {
                assert!(result.is_ok(), "{manifest:?}: {result:?}");
            }
        }
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Whether this process can write into a folder it made read-only (root can), in which case
    /// a permission fault can't be staged and the test says so and returns.
    #[cfg(unix)]
    fn permissions_bind(probe: &Path) -> bool {
        std::fs::create_dir_all(probe).unwrap();
        set_mode(probe, 0o555);
        let bound = std::fs::write(probe.join("probe"), "x").is_err();
        set_mode(probe, 0o700);
        if !bound {
            eprintln!("running with permission to write a 0555 folder (root?); skipped");
        }
        bound
    }

    /// A real write failure, then a cancellation, then a finish that can't save the records:
    /// the failed action and the finish error both keep the command at exit 1, though the run
    /// was also cancelled and skipped its next write.
    #[cfg(unix)]
    #[test]
    fn a_cancellation_never_hides_a_failed_action_or_a_failed_finish() {
        let sandbox = sandbox(
            "[files.a]\nsource = \"a\"\ntarget = \"~/sub/a\"\n[files.b]\nsource = \"b\"\ntarget = \"~/sub/b\"\n",
            &[("a", "first"), ("b", "second")],
        );
        if !permissions_bind(&sandbox.home.join("probe")) {
            return;
        }
        let sub = sandbox.home.join("sub");
        std::fs::create_dir(&sub).unwrap();
        let context = supervised(&sandbox.context);
        let state_dir = context.state.clone();
        let staged_sub = sub.clone();
        let _hook = observe::with(move |boundary, context| match boundary {
            observe::Boundary::Begun => set_mode(&staged_sub, 0o555),
            observe::Boundary::Action => {
                cancel(context);
                set_mode(&state_dir, 0o555);
            }
            _ => {}
        });
        let lock = state::lock(&context).unwrap();
        let report = run_under_lock(&context, &sandbox.loadout, lock, BACKGROUND);
        set_mode(&sub, 0o755);
        set_mode(&context.state, 0o700);
        let report = report.unwrap();
        let failed = report
            .outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::Failed(_)))
            .count();
        let skipped = report
            .outcomes
            .iter()
            .filter(|(_, outcome)| skipped_by_cancel(outcome))
            .count();
        assert_eq!((failed, skipped), (1, 1), "{report:#?}");
        assert!(report.finish_error.is_some(), "{report:#?}");
        assert!(report.cancelled);
        assert!(report.failed(), "a real failure and a failed finish exit 1");
        assert!(released(&context));
    }

    /// The finish saves the records, then the journal. With the run's own folder read-only
    /// after the last action, the records are saved and the journal isn't: the finish error is
    /// reported, the write that landed keeps its outcome, and the lock stays held from the
    /// first action through the finish and the report, then is released.
    #[cfg(unix)]
    #[test]
    fn a_finish_that_cannot_save_its_journal_keeps_the_outcome_and_holds_the_lock_to_the_report() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let sandbox = sandbox(
            "[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n",
            &[("a", "x")],
        );
        if !permissions_bind(&sandbox.home.join("probe")) {
            return;
        }
        let held = Rc::new(RefCell::new(Vec::new()));
        let seen = Rc::clone(&held);
        let _hook = observe::with(move |boundary, context| {
            seen.borrow_mut()
                .push((boundary, state::busy(context).unwrap()));
            if boundary == observe::Boundary::Action {
                let runs = context.state.join("runs");
                let only = std::fs::read_dir(&runs)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .path();
                set_mode(&only, 0o555);
            }
        });
        let lock = state::lock(&sandbox.context).unwrap();
        let report = run_under_lock(&sandbox.context, &sandbox.loadout, lock, INTERACTIVE);
        for run in std::fs::read_dir(sandbox.context.state.join("runs")).unwrap() {
            set_mode(&run.unwrap().path(), 0o700);
        }
        let report = report.unwrap();
        assert!(report.finish_error.is_some(), "{report:#?}");
        assert!(report.failed());
        assert_eq!(report.outcomes[0].1, Outcome::Done);
        assert_eq!(
            State::load(&sandbox.context).unwrap().records().count(),
            1,
            "the records were saved before the journal failed"
        );
        assert_eq!(
            *held.borrow(),
            vec![
                (observe::Boundary::Begun, true),
                (observe::Boundary::Action, true),
                (observe::Boundary::Finished, true),
                (observe::Boundary::Report, true),
            ],
            "the lock is held at every boundary"
        );
        assert!(released(&sandbox.context), "and released after");
    }

    /// Controls that need a real package manager: `programs::steps` finds a genuinely missing
    /// program only outside `LOADOUT_HOME` isolation, through a manager on `PATH`. Each scenario
    /// runs `flow` in a child process of this test binary, the way U8's producer controls do,
    /// with a cleared environment whose PATH holds only a fake `cargo`, and owned by U8's
    /// child guard from the moment it exists. The fake records every call it gets, answers the
    /// installed-list query, and an install writes `installed-<name>`.
    #[cfg(unix)]
    mod child {
        use std::time::Duration;

        use super::super::*;
        use super::{BACKGROUND, INTERACTIVE, cancel, permissions_bind, set_mode, supervised};
        use crate::process::owned::Owned;

        const MANIFEST: &str = "[loadout]\nversion = 1\n[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n[programs.a]\ncargo = \"a\"\n";

        fn fake_cargo(dir: &Path) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;

            let bin = dir.join("bin");
            std::fs::create_dir(&bin).unwrap();
            let root = dir.display().to_string();
            // The child's PATH holds only `bin`, so the hold's wait names the test's own `sleep`.
            let sleep = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|dir| dir.join("sleep"))
                .find(|path| path.is_file())
                .expect("a sleep command on the test's PATH");
            let sleep = sleep.display();
            let cargo = bin.join("cargo");
            std::fs::write(
                &cargo,
                format!(
                    "#!/bin/sh\n\
                     echo \"$*\" >> '{root}/calls'\n\
                     if [ \"$1\" = install ] && [ \"$2\" = --list ]; then\n\
                     if [ -e '{root}/fail' ]; then exit 2; fi\n\
                     if [ -e '{root}/installed-a' ]; then printf 'a v1.0.0:\\n    a\\n'; fi\n\
                     exit 0\nfi\n\
                     if [ \"$1\" = install ]; then\n\
                     if [ -e '{root}/hold' ]; then : > '{root}/install-started'\n\
                     n=0; while [ ! -e '{root}/release' ] && [ $n -lt 300 ]; do '{sleep}' 0.1; n=$((n + 1)); done\n\
                     if [ ! -e '{root}/release' ]; then echo 'never released' >&2; exit 101; fi; fi\n\
                     : > '{root}/installed-'\"$2\"; exit 0; fi\n\
                     exit 64\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
            bin
        }

        /// Runs `scenario` in a child process and requires it to pass.
        fn scenario(name: &str) {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let bin = fake_cargo(&root);
            std::fs::create_dir(root.join("home")).unwrap();
            std::fs::create_dir(root.join("loadout")).unwrap();
            std::fs::write(root.join("loadout/loadout.toml"), MANIFEST).unwrap();
            std::fs::write(root.join("loadout/a"), "x").unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "apply::tests::child::flow",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env_clear()
                .env("PATH", &bin)
                .env("LOADOUT_STATE_DIR", root.join("state"))
                .env("LOADOUT_TEST_APPLY_FLOW", &root)
                .env("LOADOUT_TEST_APPLY_SCENARIO", name)
                .stdin(std::process::Stdio::null());
            let mut flow = Owned::new(command.spawn().unwrap());
            let status = flow
                .finish_within(Duration::from_secs(60), Duration::from_secs(10))
                .unwrap_or_else(|detail| panic!("{name}: the child did not finish: {detail}"));
            assert!(status.success(), "{name}: the child flow failed: {status}");
        }

        /// The child side. Without the variables the parent sets, it does nothing.
        #[test]
        #[ignore = "run in a child process by the tests below"]
        fn flow() {
            let (Some(root), Ok(scenario)) = (
                std::env::var_os("LOADOUT_TEST_APPLY_FLOW"),
                std::env::var("LOADOUT_TEST_APPLY_SCENARIO"),
            ) else {
                return;
            };
            let root = PathBuf::from(root);
            let context = Context::resolve(
                crate::platform::Os::current().unwrap(),
                |name| std::env::var_os(name),
                Some(root.join("home")),
            )
            .unwrap();
            assert!(
                !context.isolated,
                "a real manager is asked only outside isolation"
            );
            let checkout = root.join("loadout");
            let calls = || std::fs::read_to_string(root.join("calls")).unwrap_or_default();
            let installs = |calls: &str| calls.lines().filter(|line| *line == "install a").count();
            match scenario.as_str() {
                "wrong-lock" => {
                    std::fs::create_dir(&context.state).unwrap();
                    let other = root.join("other");
                    let elsewhere = Context::resolve(
                        crate::platform::Os::current().unwrap(),
                        |name| match name {
                            "LOADOUT_STATE_DIR" => Some(other.clone().into_os_string()),
                            _ => std::env::var_os(name),
                        },
                        Some(root.join("home")),
                    )
                    .unwrap();
                    let wrong = state::lock(&elsewhere).unwrap();
                    let result = run_under_lock(&context, &checkout, wrong, INTERACTIVE);
                    assert!(
                        matches!(result, Err(Error::State(state::Error::WrongLock { .. }))),
                        "{result:?}"
                    );
                    assert_eq!(calls(), "", "no manager is asked under a wrong lock");
                    let report = run_under_lock(
                        &context,
                        &checkout,
                        state::lock(&context).unwrap(),
                        INTERACTIVE,
                    )
                    .unwrap();
                    assert!(
                        calls().lines().any(|line| line == "install --list"),
                        "under the right lock the same route asks the manager: {:?}",
                        calls()
                    );
                    assert!(
                        report
                            .outcomes
                            .iter()
                            .any(|(_, outcome)| *outcome == Outcome::Installed),
                        "{report:#?}"
                    );
                }
                "finish-error" => {
                    if !permissions_bind(&root.join("probe")) {
                        return;
                    }
                    std::fs::create_dir_all(context.state.join("runs")).unwrap();
                    let lock = state::lock(&context).unwrap();
                    set_mode(&context.state, 0o555);
                    let report = run_under_lock(&context, &checkout, lock, INTERACTIVE);
                    set_mode(&context.state, 0o700);
                    let report = report.unwrap();
                    assert!(
                        report.finish_error.is_some() && report.failed(),
                        "{report:#?}"
                    );
                    let program = report
                        .outcomes
                        .iter()
                        .find(|(action, _)| matches!(action.target, Target::Program { .. }))
                        .expect("the genuinely missing program is planned");
                    assert_eq!(program.0.change, Change::Install, "{report:#?}");
                    assert!(
                        matches!(&program.1, Outcome::Skipped(reason) if reason.contains("installed no program")),
                        "{report:#?}"
                    );
                    assert_eq!(installs(&calls()), 0, "no installer started: {:?}", calls());
                    assert!(!root.join("installed-a").exists());
                    assert!(!state::busy(&context).unwrap());
                }
                "install-held-lock" => {
                    use std::cell::RefCell;
                    use std::rc::Rc;

                    let held = Rc::new(RefCell::new(Vec::new()));
                    let seen = Rc::clone(&held);
                    let _hook = observe::with(move |boundary, context| {
                        if boundary == observe::Boundary::Install {
                            seen.borrow_mut().push(state::busy(context).unwrap());
                        }
                    });
                    let report = run_under_lock(
                        &context,
                        &checkout,
                        state::lock(&context).unwrap(),
                        INTERACTIVE,
                    )
                    .unwrap();
                    assert_eq!(
                        *held.borrow(),
                        vec![true],
                        "the lock is held as the installer starts"
                    );
                    assert_eq!(installs(&calls()), 1, "{:?}", calls());
                    assert!(root.join("installed-a").exists());
                    assert!(!report.failed(), "{report:#?}");
                    assert!(!state::busy(&context).unwrap());
                }
                "cancel-before-install" => {
                    let context = supervised(&context);
                    let _hook = observe::with(|boundary, context| {
                        if boundary == observe::Boundary::Finished {
                            cancel(context);
                        }
                    });
                    let report = run_under_lock(
                        &context,
                        &checkout,
                        state::lock(&context).unwrap(),
                        BACKGROUND,
                    )
                    .unwrap();
                    let program = report
                        .outcomes
                        .iter()
                        .find(|(action, _)| matches!(action.target, Target::Program { .. }))
                        .unwrap();
                    assert_eq!(
                        program.1,
                        Outcome::Skipped(CANCELLED.to_owned()),
                        "{report:#?}"
                    );
                    assert!(report.cancelled && report.unhealthy() && !report.failed());
                    assert!(
                        calls().lines().all(|line| line == "install --list"),
                        "only the plan's query reached the manager: {:?}",
                        calls()
                    );
                    assert!(!root.join("installed-a").exists());
                }
                "cancel-during-install" => {
                    let context = supervised(&context);
                    std::fs::write(root.join("hold"), "").unwrap();
                    let flag = context.containment.cancel().unwrap().clone();
                    let marks = root.clone();
                    let canceller = std::thread::spawn(move || {
                        let deadline = std::time::Instant::now() + Duration::from_secs(30);
                        while !marks.join("install-started").exists() {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "the installer never started"
                            );
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        flag.cancel();
                        std::fs::write(marks.join("release"), "").unwrap();
                    });
                    let report = run_under_lock(
                        &context,
                        &checkout,
                        state::lock(&context).unwrap(),
                        BACKGROUND,
                    )
                    .unwrap();
                    canceller.join().unwrap();
                    let program = report
                        .outcomes
                        .iter()
                        .find(|(action, _)| matches!(action.target, Target::Program { .. }))
                        .unwrap();
                    assert!(
                        matches!(&program.1, Outcome::Skipped(reason) if reason.contains("didn't confirm")),
                        "{report:#?}"
                    );
                    assert!(
                        report.cancelled && report.unhealthy() && !report.failed(),
                        "{report:#?}"
                    );
                    assert!(root.join("installed-a").exists());
                    let calls = calls();
                    let after = calls
                        .split_once("install a\n")
                        .map_or("", |(_, after)| after);
                    assert_eq!(after, "", "no query starts after the cancel: {calls:?}");
                }
                "cancel-at-installer-start" => {
                    let context = supervised(&context);
                    let _hook = observe::with(|boundary, context| {
                        if boundary == observe::Boundary::Install {
                            cancel(context);
                        }
                    });
                    let report = run_under_lock(
                        &context,
                        &checkout,
                        state::lock(&context).unwrap(),
                        BACKGROUND,
                    )
                    .unwrap();
                    let program = report
                        .outcomes
                        .iter()
                        .find(|(action, _)| matches!(action.target, Target::Program { .. }))
                        .unwrap();
                    assert_eq!(
                        program.1,
                        Outcome::Skipped(CANCELLED.to_owned()),
                        "{report:#?}"
                    );
                    assert!(
                        report.cancelled && report.unhealthy() && !report.failed(),
                        "{report:#?}"
                    );
                    assert_eq!(installs(&calls()), 0, "no installer started: {:?}", calls());
                    assert!(!root.join("installed-a").exists());
                }
                "query-error" => {
                    let context = supervised(&context);
                    std::fs::write(root.join("fail"), "").unwrap();
                    let result = run_under_lock(
                        &context,
                        &checkout,
                        state::lock(&context).unwrap(),
                        BACKGROUND,
                    );
                    assert!(
                        matches!(result, Err(Error::Plan(plan::Error::Runtime(_)))),
                        "a failed query is a runtime error: {result:?}"
                    );
                    assert!(
                        context.containment.is_cancelled(),
                        "the failed query also cancelled the run, which doesn't make it a skip"
                    );
                    assert!(calls().lines().any(|line| line == "install --list"));
                    assert!(state::runs(&context).unwrap().is_empty());
                    assert!(!root.join("installed-a").exists());
                }
                other => panic!("unknown scenario {other}"),
            }
        }

        #[test]
        fn a_wrong_lock_reaches_no_package_manager_and_the_right_one_does() {
            scenario("wrong-lock");
        }

        #[test]
        fn a_failed_finish_starts_no_installer_for_a_genuinely_missing_program() {
            scenario("finish-error");
        }

        #[test]
        fn the_lock_is_held_while_an_installer_starts() {
            scenario("install-held-lock");
        }

        #[test]
        fn a_run_cancelled_before_its_installs_starts_no_installer() {
            scenario("cancel-before-install");
        }

        /// The production soft deadline lands while an installer runs: that installer finishes,
        /// and the run, cancelled by then, starts no confirming query. The program is left
        /// unconfirmed, so the run exits 4 as cancelled, not 1 as failed.
        #[test]
        fn a_run_cancelled_during_an_install_reports_it_unconfirmed_not_failed() {
            scenario("cancel-during-install");
        }

        /// The cancellation lands after apply's own check and before the installer starts: U8
        /// refuses to start it, and that refusal is a skip, not a failed spawn.
        #[test]
        fn a_cancel_between_the_check_and_the_installer_skips_it() {
            scenario("cancel-at-installer-start");
        }

        #[test]
        fn a_failed_query_stays_a_runtime_error_though_it_sets_the_cancel_flag() {
            scenario("query-error");
        }
    }
}
