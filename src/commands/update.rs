use std::path::PathBuf;

use anyhow::Context as _;

use crate::apply::{self, Mode, Options, Outcome, Report};
use crate::cli::Exit;
use crate::commands::apply as shown;
use crate::context::Context;
use crate::plan;
use crate::process;
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
        Ok(report) => shown::print(&report),
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
            shown::print(&report)
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
