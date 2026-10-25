use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::apply::{self, Mode, Options, Outcome, Report};
use crate::cli::Exit;
use crate::commands::apply as shown;
use crate::context::Context;
use crate::plan;
use crate::process;
use crate::release;
use crate::state;
use crate::sync::background::{self, LastRun, RunResult, Worker};
use crate::sync::{self, Fetched, service};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Apply this checkout instead of loadout's own clone. It is applied as it is: loadout never
    /// fetches, merges, or otherwise changes a checkout it doesn't manage
    #[arg(long, value_name = "DIR", conflicts_with = "background")]
    pub loadout: Option<PathBuf>,
    /// Run as the scheduled job does: supervised and time-bounded, with no prompts, no --force,
    /// and no elevation; what happened is recorded for `loadout status`
    #[arg(long)]
    pub background: bool,
    /// The service.toml the scheduled job was installed with
    #[arg(long, value_name = "FILE", requires = "background")]
    pub service: Option<PathBuf>,
    /// The supervised worker of a background run; only `loadout update --background` starts it
    #[arg(long, hide = true, requires = "background")]
    pub worker: bool,
    /// The supervisor run this worker belongs to, recorded with what the worker did
    #[arg(long, hide = true, requires = "worker", value_name = "ID")]
    pub run: Option<String>,
}

/// `loadout update`: takes the lock, refuses on an unfinished run, fetches and fast-forwards
/// loadout's clone, then applies it under the same lock.
pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    match (args.background, args.worker) {
        (false, _) => foreground(args, context),
        (true, false) => supervisor(args, Some(context)),
        (true, true) => worker(args, Some(context)),
    }
}

impl Args {
    /// The scheduled job's route: `--background --service FILE`, whose `Context` comes from
    /// that file alone.
    pub fn recorded(&self) -> bool {
        self.background && self.service.is_some()
    }
}

/// `loadout update --background --service FILE` (supervisor or worker), run without resolving
/// the scheduler's own environment: the record is authoritative, so a variable there that
/// `Context` would reject (a relative `LOADOUT_STATE_DIR`, say) can't stop the job. The worker
/// still checks its handshake before anything else.
pub fn run_recorded(args: &Args) -> anyhow::Result<Exit> {
    if args.worker {
        worker(args, None)
    } else {
        supervisor(args, None)
    }
}

/// The run's `Context`: the recorded one with `--service`, else the caller's.
fn chosen(loaded: Option<&Context>, ambient: Option<&Context>) -> anyhow::Result<Context> {
    loaded
        .or(ambient)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("a background run needs --service or a usable environment"))
}

fn foreground(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    let lock = state::lock(context).context("cannot take loadout's lock")?;
    if let Some(reason) = sync::unfinished(context)? {
        println!("blocked: {reason}; nothing was fetched or applied");
        return Ok(Exit::Unhealthy);
    }
    let checkout = match &args.loadout {
        Some(dir) => {
            let checkout = plan::checkout(context, Some(dir))?;
            println!(
                "not fetched: {} is not loadout's own clone, so it is applied as it is",
                checkout.display()
            );
            checkout
        }
        None => match sync::fetch(context, &lock)? {
            Fetched::Blocked { reason } => {
                println!(
                    "blocked: {reason}; the clone's branch and working tree were left as they are, and nothing was applied"
                );
                return Ok(Exit::Unhealthy);
            }
            fetched => {
                println!("{}", describe(&fetched));
                sync::checkout(context)
            }
        },
    };
    let options = Options {
        force: false,
        elevated: false,
        mode: Mode::Interactive,
    };
    match apply::run_under_lock(context, &checkout, lock, options) {
        Ok(report) => {
            let exit = shown::print(&report)?;
            let outcome = self_update(context, &checkout, &report)?;
            Ok(foreground_exit(&outcome, exit))
        }
        Err(error) => shown::refused(error),
    }
}

/// The scheduled job's process: starts the worker supervised and records how it ended. With
/// `--service`, everything comes from that file, not from the scheduler's environment.
fn supervisor(args: &Args, ambient: Option<&Context>) -> anyhow::Result<Exit> {
    let loaded = match &args.service {
        Some(path) => Some(service::load(path)?),
        None => None,
    };
    let context = chosen(loaded.as_ref().map(|(_, context)| context), ambient)?;
    let exe = std::env::current_exe().context("cannot find loadout's own executable")?;
    let worker = Worker {
        exe: &exe,
        service: args
            .service
            .as_deref()
            .zip(loaded.as_ref().map(|(recorded, _)| recorded)),
    };
    let code = background::supervise(&context, &worker)?;
    exit_code(code)
}

/// The supervised worker: checks its startup first, then runs the update with no prompt,
/// force, or elevation, and records what it did in `last-run.json`.
fn worker(args: &Args, ambient: Option<&Context>) -> anyhow::Result<Exit> {
    let supervised = process::handshake()?;
    let loaded = match &args.service {
        Some(path) => Some(service::load(path)?.1),
        None => None,
    };
    let context = chosen(loaded.as_ref(), ambient)?.supervised(supervised);
    let mut record = LastRun::new(sync::now(), RunResult::Failed, 1);
    record.supervisor.clone_from(&args.run);
    let result = background_update(&context, &mut record);
    record.ended = sync::now();
    match &result {
        Ok(exit) => record.exit = exit.code(),
        Err(error) => {
            record.exit = 1;
            record.message.get_or_insert_with(|| format!("{error:#}"));
        }
    }
    background::record(&context, &record)?;
    result
}

fn background_update(context: &Context, record: &mut LastRun) -> anyhow::Result<Exit> {
    let lock = match state::lock(context) {
        Ok(lock) => lock,
        Err(error @ state::Error::Locked { .. }) => {
            record.result = RunResult::Locked;
            record.message = Some(error.to_string());
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    if let Some(reason) = sync::unfinished(context)? {
        println!("blocked: {reason}; nothing was fetched or applied");
        record.result = RunResult::Blocked;
        record.message = Some(reason);
        return Ok(Exit::Unhealthy);
    }
    let fetched = sync::fetch(context, &lock)?;
    record.fetched = Some(fetched.clone());
    if let Fetched::Blocked { reason } = fetched {
        println!("blocked: {reason}; nothing was applied");
        record.result = RunResult::Blocked;
        record.message = Some(reason.to_string());
        return Ok(Exit::Unhealthy);
    }
    println!("{}", describe(&fetched));
    let checkout = sync::checkout(context);
    match sync::settings(context, &checkout) {
        Ok(update) if !update.auto => {
            let message =
                "the loadout sets [update] auto = false, so nothing was planned or applied";
            println!("{message}");
            record.result = RunResult::Disabled;
            record.message = Some(message.to_owned());
            return Ok(Exit::Success);
        }
        Ok(_) => {}
        Err(sync::Error::Invalid(diagnostics)) => {
            for diagnostic in diagnostics.iter() {
                println!("{diagnostic}");
            }
            record.result = RunResult::Invalid;
            record.message = Some("the loadout is invalid for this machine".to_owned());
            return Ok(Exit::Invalid);
        }
        Err(error) => return Err(error.into()),
    }
    let options = Options {
        force: false,
        elevated: false,
        mode: Mode::Background,
    };
    match apply::run_under_lock(context, &checkout, lock, options) {
        Ok(report) => {
            summarize(&report, record);
            let exit = shown::print(&report)?;
            recorded(self_update(context, &checkout, &report), record, exit)
        }
        Err(error) => {
            record.result = match &error {
                apply::Error::Plan(plan::Error::Invalid(_)) => RunResult::Invalid,
                apply::Error::Plan(plan::Error::Blocked(message)) => {
                    record.message = Some(message.clone());
                    RunResult::Blocked
                }
                _ => RunResult::Failed,
            };
            shown::refused(error)
        }
    }
}

/// What the self-update step did after an apply.
#[derive(Debug, PartialEq, Eq)]
enum SelfUpdate {
    /// Not attempted: the apply failed or was cancelled, or the loadout sets `cli = false`.
    Off,
    /// Asked for a newer release; the line it printed.
    Done(String),
    /// The run was cancelled after its report and before a release request started, so none was
    /// made.
    Cancelled,
}

/// Self-update after an apply that ran to its report, when the loadout's resolved `[update] cli`
/// is true (section 8). It never runs after a failed apply (already exit 1) or a cancelled one,
/// so it starts no work in a run that is stopping and never hides the apply's exit. The live
/// cancellation is checked again before the release request and before the install, so a run
/// cancelled after its report starts neither; a request already under way is not interrupted. A
/// skip (isolation, an unmanaged binary, no release, another install holding the folder) is
/// shown and keeps the apply's exit; any other error fails the update.
fn self_update(context: &Context, checkout: &Path, report: &Report) -> anyhow::Result<SelfUpdate> {
    if report.cancelled || report.failed() || !sync::settings(context, checkout)?.cli {
        return Ok(SelfUpdate::Off);
    }
    self_update_with(
        context,
        release::latest,
        release::install,
        |newer: &release::Release| {
            format!("cli: updated {} to {}", newer.current(), newer.version())
        },
    )
}

fn self_update_with<R>(
    context: &Context,
    latest: impl FnOnce(&Context) -> Result<Option<R>, release::Error>,
    install: impl FnOnce(&R) -> Result<(), release::Error>,
    updated: impl FnOnce(&R) -> String,
) -> anyhow::Result<SelfUpdate> {
    if context.containment.is_cancelled() {
        return Ok(SelfUpdate::Cancelled);
    }
    let line = match latest(context) {
        Ok(None) => "cli: up to date".to_owned(),
        Ok(Some(newer)) => {
            if context.containment.is_cancelled() {
                return Ok(SelfUpdate::Cancelled);
            }
            match install(&newer) {
                Ok(()) => updated(&newer),
                Err(error) if error.skipped() => format!("cli: not updated: {error}"),
                Err(error) => return Err(anyhow::Error::new(error).context("self-update failed")),
            }
        }
        Err(error) if error.skipped() => format!("cli: not updated: {error}"),
        Err(error) => return Err(anyhow::Error::new(error).context("self-update failed")),
    };
    println!("{line}");
    // Cancelled while the lookup or the install ran: what happened is shown, but the run was
    // stopped, so it ends as cancelled, not as a clean or skipped update. A real error above
    // already returned and keeps its exit 1.
    if context.containment.is_cancelled() {
        return Ok(SelfUpdate::Cancelled);
    }
    Ok(SelfUpdate::Done(line))
}

/// The foreground's exit after the self-update step: a cancellation ends the run unhealthy (4).
fn foreground_exit(outcome: &SelfUpdate, exit: Exit) -> Exit {
    if *outcome == SelfUpdate::Cancelled {
        println!("cancelled: {}", apply::CANCELLED);
        return Exit::Unhealthy;
    }
    exit
}

/// The worker's record and exit after the self-update step. A cancellation records the run as
/// cancelled with the usual message and ends it unhealthy (4); a real error records the run as
/// failed and keeps the error (exit 1); otherwise the self-update's line is the message unless
/// the run already has one.
fn recorded(
    outcome: anyhow::Result<SelfUpdate>,
    record: &mut LastRun,
    exit: Exit,
) -> anyhow::Result<Exit> {
    match outcome {
        Ok(SelfUpdate::Off) => Ok(exit),
        Ok(SelfUpdate::Done(line)) => {
            if record.message.is_none() {
                record.message = Some(line);
            }
            Ok(exit)
        }
        Ok(SelfUpdate::Cancelled) => {
            println!("cancelled: {}", apply::CANCELLED);
            record.cancelled = true;
            record.message = Some(apply::CANCELLED.to_owned());
            Ok(Exit::Unhealthy)
        }
        Err(error) => {
            record.result = RunResult::Failed;
            record.message = Some(format!("{error:#}"));
            Err(error)
        }
    }
}

/// Counts and labels for `last-run.json`: how each action ended, and every action that didn't
/// end cleanly, by target and item only.
fn summarize(report: &Report, record: &mut LastRun) {
    record.result = RunResult::Applied;
    record.run.clone_from(&report.run);
    record.cancelled = report.cancelled;
    for (action, outcome) in &report.outcomes {
        let (name, detail) = shown::describe(outcome);
        *record.outcomes.entry(name.to_owned()).or_default() += 1;
        if matches!(
            outcome,
            Outcome::Unsupported(_)
                | Outcome::Skipped(_)
                | Outcome::Blocked(_)
                | Outcome::Mismatch(_)
                | Outcome::Failed(_)
        ) {
            record.details.push(format!(
                "{name} {} ({}){}",
                state::label(&action.target),
                action.item.label(),
                detail
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default()
            ));
        }
    }
    if report.cancelled {
        record.message = Some(apply::CANCELLED.to_owned());
    }
}

fn describe(fetched: &Fetched) -> String {
    match fetched {
        Fetched::UpToDate { head } => format!("fetched: up to date at {}", short(head)),
        Fetched::Advanced { from, to } => {
            format!("fetched: fast-forwarded {} to {}", short(from), short(to))
        }
        Fetched::Blocked { reason } => format!("blocked: {reason}"),
    }
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The supervisor exits with the worker's code: 0, 3, or 4 as the worker chose, and 1 for
/// anything else, including a run the supervisor had to stop.
fn exit_code(code: u8) -> anyhow::Result<Exit> {
    match code {
        0 => Ok(Exit::Success),
        3 => Ok(Exit::Invalid),
        4 => Ok(Exit::Unhealthy),
        _ => anyhow::bail!(
            "the background run failed or had to be stopped; `loadout status` and the background records say why"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;

    use super::*;
    use crate::platform::Os;
    use crate::process::Containment;

    /// A supervised background context, the only kind a supervisor can cancel.
    fn supervised() -> Context {
        let base =
            Context::resolve(Os::current().unwrap(), |_| None, Some(PathBuf::from("/h"))).unwrap();
        Context {
            containment: Containment::inherited_for_tests(),
            ..base
        }
    }

    fn cancel(context: &Context) {
        context
            .containment
            .cancel()
            .expect("only a supervised run can be cancelled")
            .cancel();
    }

    #[test]
    fn a_run_cancelled_after_its_report_makes_no_release_request() {
        let context = supervised();
        cancel(&context);
        let asked = Cell::new(false);
        let outcome = self_update_with(
            &context,
            |_| {
                asked.set(true);
                Ok(Some(()))
            },
            |()| panic!("a cancelled run never installs"),
            |()| String::new(),
        )
        .unwrap();
        assert_eq!(outcome, SelfUpdate::Cancelled);
        assert!(
            !asked.get(),
            "no release request starts after the cancellation"
        );
    }

    #[test]
    fn a_cancellation_between_discovery_and_install_installs_nothing() {
        let context = supervised();
        let installed = Cell::new(false);
        let outcome = self_update_with(
            &context,
            |context| {
                cancel(context);
                Ok(Some(()))
            },
            |()| {
                installed.set(true);
                Ok(())
            },
            |()| String::new(),
        )
        .unwrap();
        assert_eq!(outcome, SelfUpdate::Cancelled);
        assert!(
            !installed.get(),
            "a newer release found before the cancellation isn't installed"
        );
    }

    #[test]
    fn without_a_cancellation_a_newer_release_is_installed() {
        let context = supervised();
        let installed = Cell::new(false);
        let outcome = self_update_with(
            &context,
            |_| Ok(Some(())),
            |()| {
                installed.set(true);
                Ok(())
            },
            |()| "cli: updated".to_owned(),
        )
        .unwrap();
        assert_eq!(outcome, SelfUpdate::Done("cli: updated".to_owned()));
        assert!(installed.get());
    }

    #[test]
    fn a_cancellation_during_a_lookup_that_finds_nothing_ends_cancelled() {
        let context = supervised();
        let outcome = self_update_with(
            &context,
            |context| {
                cancel(context);
                Ok(None::<()>)
            },
            |()| panic!("nothing to install"),
            |()| String::new(),
        )
        .unwrap();
        assert_eq!(outcome, SelfUpdate::Cancelled);
    }

    #[test]
    fn a_cancellation_during_a_lookup_that_is_skipped_ends_cancelled_without_installing() {
        let context = supervised();
        let installed = Cell::new(false);
        let outcome = self_update_with(
            &context,
            |context| {
                cancel(context);
                Err::<Option<()>, _>(release::Error::Isolated)
            },
            |()| {
                installed.set(true);
                Ok(())
            },
            |()| String::new(),
        )
        .unwrap();
        assert_eq!(outcome, SelfUpdate::Cancelled);
        assert!(!installed.get());
    }

    #[test]
    fn a_cancellation_during_a_successful_install_ends_cancelled() {
        let context = supervised();
        let outcome = self_update_with(
            &context,
            |_| Ok(Some(())),
            |()| {
                cancel(&context);
                Ok(())
            },
            |()| "cli: updated".to_owned(),
        )
        .unwrap();
        assert_eq!(outcome, SelfUpdate::Cancelled);
    }

    #[test]
    fn a_cancellation_during_an_install_another_one_holds_ends_cancelled() {
        let context = supervised();
        let outcome = self_update_with(
            &context,
            |_| Ok(Some(())),
            |()| {
                cancel(&context);
                Err(release::Error::Busy {
                    dir: PathBuf::from("/bin"),
                })
            },
            |()| String::new(),
        )
        .unwrap();
        assert_eq!(outcome, SelfUpdate::Cancelled);
    }

    #[test]
    fn a_real_install_error_keeps_failing_even_when_the_run_was_also_cancelled() {
        let context = supervised();
        let outcome = self_update_with(
            &context,
            |_| Ok(Some(())),
            |()| {
                cancel(&context);
                Err(release::Error::Download {
                    detail: "connection reset".to_owned(),
                })
            },
            |()| String::new(),
        );
        assert!(outcome.is_err(), "exit 1 outranks the cancellation");
    }

    #[test]
    fn the_worker_records_a_cancelled_self_update_as_cancelled_and_exits_four() {
        let mut record = LastRun::new(0, RunResult::Applied, 0);
        let exit = recorded(Ok(SelfUpdate::Cancelled), &mut record, Exit::Success).unwrap();
        assert_eq!(exit.code(), 4);
        assert!(record.cancelled);
        assert_eq!(record.message.as_deref(), Some(apply::CANCELLED));
        assert_eq!(
            record.result,
            RunResult::Applied,
            "the apply itself ran to its report"
        );
    }

    #[test]
    fn the_worker_records_a_real_self_update_error_as_failed() {
        let mut record = LastRun::new(0, RunResult::Applied, 0);
        let outcome = recorded(
            Err(anyhow::anyhow!("self-update failed")),
            &mut record,
            Exit::Success,
        );
        assert!(outcome.is_err());
        assert_eq!(record.result, RunResult::Failed);
        assert!(!record.cancelled);
    }

    #[test]
    fn the_worker_keeps_the_apply_exit_and_records_a_done_line_only_without_a_message() {
        let mut record = LastRun::new(0, RunResult::Applied, 0);
        let exit = recorded(
            Ok(SelfUpdate::Done("cli: up to date".to_owned())),
            &mut record,
            Exit::Unhealthy,
        )
        .unwrap();
        assert_eq!(exit, Exit::Unhealthy);
        assert_eq!(record.message.as_deref(), Some("cli: up to date"));
        assert!(!record.cancelled);
    }

    #[test]
    fn the_foreground_ends_unhealthy_only_when_cancelled() {
        assert_eq!(
            foreground_exit(&SelfUpdate::Cancelled, Exit::Success),
            Exit::Unhealthy
        );
        assert_eq!(
            foreground_exit(&SelfUpdate::Done(String::new()), Exit::Success),
            Exit::Success
        );
        assert_eq!(
            foreground_exit(&SelfUpdate::Off, Exit::Unhealthy),
            Exit::Unhealthy
        );
    }

    #[test]
    fn a_skip_is_shown_and_any_other_release_error_fails_the_update() {
        let context = supervised();
        let skipped = self_update_with(
            &context,
            |_| Err::<Option<()>, _>(release::Error::Isolated),
            |()| Ok(()),
            |()| String::new(),
        )
        .unwrap();
        assert!(
            matches!(&skipped, SelfUpdate::Done(line) if line.starts_with("cli: not updated:")),
            "{skipped:?}"
        );
        let failed = self_update_with(
            &context,
            |_| Ok(Some(())),
            |()| {
                Err(release::Error::Download {
                    detail: "connection reset".to_owned(),
                })
            },
            |()| String::new(),
        );
        assert!(failed.is_err(), "a real install error is not a skip");
    }
}
