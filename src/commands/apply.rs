use std::path::PathBuf;

use crate::apply::{self, Mode, Options, Outcome, Report};
use crate::cli::Exit;
use crate::context::Context;
use crate::plan;
use crate::state;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The loadout checkout to apply [default: loadout's own clone in the state dir]
    #[arg(long, value_name = "DIR")]
    pub loadout: Option<PathBuf>,
    /// Overwrite targets that changed since loadout wrote them, after backing them up
    #[arg(long)]
    pub force: bool,
    /// Install system-wide programs, through sudo or the installer's UAC prompt
    #[arg(long)]
    pub elevated: bool,
}

/// Brings this machine in line with the loadout: plans read-only first, then takes the lock and
/// applies a plan rebuilt under it.
pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    let checkout = plan::checkout(context, args.loadout.as_deref())?;
    let options = Options {
        force: args.force,
        elevated: args.elevated,
        mode: Mode::Interactive,
    };
    show(apply::run(context, &checkout, options))
}

/// Prints an apply's result and maps it to the exit code, the same for `apply`, `init`, and
/// `update`: `refused` for a run that couldn't start, `print` for one that did.
pub(crate) fn show(result: Result<apply::Report, apply::Error>) -> anyhow::Result<Exit> {
    match result {
        Ok(report) => print(&report),
        Err(error) => refused(error),
    }
}

/// How `apply` and `update` show a run that couldn't start: an invalid loadout (exit 3), a
/// blocked machine (exit 4), or a runtime error (exit 1). Nothing was written to a destination.
pub(crate) fn refused(error: apply::Error) -> anyhow::Result<Exit> {
    match error {
        apply::Error::Plan(plan::Error::Invalid(diagnostics)) => {
            for diagnostic in diagnostics.iter() {
                println!("{diagnostic}");
            }
            println!("invalid: no destination was written");
            Ok(Exit::Invalid)
        }
        apply::Error::Plan(plan::Error::Blocked(message)) => {
            println!("blocked: {message}; no destination was written");
            Ok(Exit::Unhealthy)
        }
        apply::Error::Plan(plan::Error::Runtime(error)) => Err(error),
        apply::Error::State(error) => Err(error.into()),
    }
}

/// How `apply` and `update` show a run: each action and how it went, then the exit: 1 when an
/// action or the finish failed, 4 when anything was left undone, else 0.
pub(crate) fn print(report: &Report) -> anyhow::Result<Exit> {
    for diagnostic in report.warnings.iter() {
        println!("{diagnostic}");
    }
    println!(
        "apply for {} from {}",
        report.machine,
        report.checkout.display()
    );
    for (action, outcome) in &report.outcomes {
        let (name, detail) = describe(outcome);
        let detail = detail
            .map(|detail| format!(": {detail}"))
            .unwrap_or_default();
        println!(
            "  {name:<17} {}  ({}){detail}",
            state::label(&action.target),
            action.item.label()
        );
    }
    if let Some(run) = &report.run {
        println!("run {run}; `loadout rollback {run}` undoes it");
    }
    if report.cancelled {
        println!("cancelled: {}", apply::CANCELLED);
    }
    if let Some(error) = &report.finish_error {
        eprintln!("error: loadout couldn't save its ownership records: {error}");
    }
    if report.failed() {
        anyhow::bail!("some actions failed; the lines above say which");
    }
    Ok(if report.unhealthy() {
        Exit::Unhealthy
    } else {
        Exit::Success
    })
}

pub(crate) fn describe(outcome: &Outcome) -> (&'static str, Option<&str>) {
    match outcome {
        Outcome::Done => ("done", None),
        Outcome::Adopted => ("adopted", None),
        Outcome::Removed => ("removed", None),
        Outcome::Released => ("released", None),
        Outcome::Unchanged => ("unchanged", None),
        Outcome::Installed => ("installed", None),
        Outcome::AlreadyInstalled => ("already installed", None),
        Outcome::Unsupported(reason) => ("unsupported", Some(reason.as_str())),
        Outcome::Skipped(reason) => ("skipped", Some(reason.as_str())),
        Outcome::Blocked(reason) => ("blocked", Some(reason.as_str())),
        Outcome::Mismatch(reason) => ("mismatch", Some(reason.as_str())),
        Outcome::Failed(reason) => ("failed", Some(reason.as_str())),
    }
}
