//! A background update is two processes of the same binary (architecture section 8): a
//! supervisor, which starts the worker inside a process tree it owns and bounds the whole run
//! (`process::supervise`), and the worker, which acts only after `process::handshake`. This
//! module starts the worker and keeps the records both leave in `<state>/background`:
//!
//! - `supervisor.json`: how the supervisor ended the run;
//! - `worker.log`: the worker's output, at most `process::LOG_CAP` bytes plus its notes;
//! - `last-run.json`: what the worker did (fetched, applied, blocked, disabled), never content.
//!
//! `status` and `doctor` read them. A background run never changes the service itself.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::context::{Context, HOME_OVERRIDE};
use crate::diagnostic::{Check, CheckStatus};
use crate::process::{self, Ended, Limits};
use crate::state::{self, Private};

use super::{Error, Fetched, now};

/// The worker a supervisor starts.
#[derive(Debug)]
pub struct Worker<'a> {
    /// `loadout` itself: this executable, or on Windows `loadout.exe` beside the launcher.
    pub exe: &'a Path,
    /// The `service.toml` the scheduler named. The worker rebuilds its `Context` from it and
    /// is started with its recorded variables. `None` for a background run started by hand,
    /// which keeps the caller's environment.
    pub service: Option<(&'a Path, &'a super::service::Recorded)>,
}

/// How the supervisor ended the run, as `supervisor.json` records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorRecord {
    pub started: u64,
    pub ended: u64,
    /// `exited`, `timed-out`, `lost`, or `not-started`.
    pub outcome: String,
    /// The worker's exit code, when it exited by itself.
    pub code: Option<i32>,
    /// The soft deadline asked the worker to stop.
    pub cancelled: bool,
    /// Why the tree could not be confirmed stopped, or the worker could not be started.
    pub problem: Option<String>,
    pub log_complete: bool,
    pub log_dropped: u64,
}

impl SupervisorRecord {
    /// The worker exited by itself and everything it started was confirmed stopped.
    pub fn clean(&self) -> bool {
        self.outcome == "exited" && self.problem.is_none() && self.log_complete
    }
}

/// Runs one background update under supervision with the production limits, records how it
/// ended, and returns the exit code for the supervisor process: the worker's own code when it
/// exited by itself and its tree was confirmed stopped, otherwise 1.
pub fn supervise(context: &Context, worker: &Worker<'_>) -> Result<u8, Error> {
    supervise_with(context, worker, Limits::BACKGROUND)
}

/// `supervise` with other limits. Only tests pass shorter ones; nothing in the shipped commands
/// does, and nothing the loadout or the environment says can change them.
pub fn supervise_with(context: &Context, worker: &Worker<'_>, limits: Limits) -> Result<u8, Error> {
    let started = now();
    let mut command = Command::new(worker.exe);
    command.args(["update", "--background", "--worker"]);
    if let Some((path, recorded)) = worker.service {
        command
            .arg("--service")
            .arg(path)
            .env_remove(HOME_OVERRIDE)
            .envs(recorded.env());
        #[cfg(unix)]
        command.env("HOME", &recorded.home);
    }
    let supervised = process::supervise(command, limits);
    let (record, log, exit) = match supervised {
        Ok(outcome) => {
            let problem = outcome.cleanup.err();
            let (name, code, cancelled) = match &outcome.ended {
                Ended::Exited { code, cancelled } => ("exited", *code, *cancelled),
                Ended::TimedOut { cancelled } => ("timed-out", None, *cancelled),
                Ended::Lost(_) => ("lost", None, false),
            };
            let problem = match (&outcome.ended, problem) {
                (Ended::Lost(why), None) => {
                    Some(format!("the worker's exit could not be observed: {why}"))
                }
                (Ended::Lost(why), Some(detail)) => Some(format!(
                    "the worker's exit could not be observed ({why}), and its tree could not be confirmed stopped: {detail}"
                )),
                (_, problem) => problem,
            };
            let exit = match (name, code, &problem) {
                ("exited", Some(code), None) if outcome.log.complete => {
                    u8::try_from(code).unwrap_or(1)
                }
                _ => 1,
            };
            let record = SupervisorRecord {
                started,
                ended: now(),
                outcome: name.to_owned(),
                code,
                cancelled,
                problem,
                log_complete: outcome.log.complete,
                log_dropped: outcome.log.dropped,
            };
            (record, Some(outcome.log.bytes), exit)
        }
        Err(error) => {
            let record = SupervisorRecord {
                started,
                ended: now(),
                outcome: "not-started".to_owned(),
                code: None,
                cancelled: false,
                problem: Some(error.to_string()),
                log_complete: true,
                log_dropped: 0,
            };
            (record, None, 1)
        }
    };
    let json = serde_json::to_vec_pretty(&record).map_err(|error| state::Error::Invalid {
        path: Private::Supervisor.path(context),
        message: error.to_string(),
    })?;
    state::write_private(context, Private::Supervisor, &json)?;
    if let Some(log) = log {
        state::write_private(context, Private::WorkerLog, &log)?;
    }
    Ok(exit)
}

/// The Windows launcher, `loadout-background.exe`, given its arguments: `--version`, or
/// `--service <FILE>`, which supervises one background update with `loadout.exe` from the
/// launcher's own folder as the worker (never one found on PATH). Elsewhere it refuses with
/// exit 2. It has no console, so a failure is visible only through its exit code and the
/// background records.
pub fn launch(args: &[OsString]) -> u8 {
    match args {
        [flag] if flag == "--version" => {
            println!("{} {}", super::service::LAUNCHER, env!("CARGO_PKG_VERSION"));
            0
        }
        _ if !cfg!(windows) => {
            eprintln!("{} is only used on Windows", super::service::LAUNCHER);
            2
        }
        [flag, file] if flag == "--service" => match launch_service(Path::new(file)) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        _ => {
            eprintln!("usage: {} --service <FILE>", super::service::LAUNCHER);
            2
        }
    }
}

fn launch_service(path: &Path) -> Result<u8, Error> {
    let (recorded, context) = super::service::load(path)?;
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|source| Error::Io {
            action: "find",
            path: PathBuf::from(super::service::LAUNCHER),
            source,
        })?
        .with_file_name("loadout.exe");
    if !std::fs::metadata(&exe).is_ok_and(|metadata| metadata.is_file()) {
        return Err(Error::MissingWorker { path: exe });
    }
    supervise(
        &context,
        &Worker {
            exe: &exe,
            service: Some((path, &recorded)),
        },
    )
}

/// What the worker's run came to, as `last-run.json` records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunResult {
    /// The plan was applied; `outcomes` says how each action went.
    Applied,
    /// The fetch, an unfinished run, or the plan stopped the run before anything was applied.
    Blocked,
    /// The loadout is invalid for this machine.
    Invalid,
    /// The loadout sets `[update] auto = false`, so nothing was planned or applied.
    Disabled,
    /// Another loadout run held the lock, so this one did nothing.
    Locked,
    /// Something failed: a fetch, a write, an install, or loadout itself.
    Failed,
}

/// What the worker did, without any content: `last-run.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastRun {
    pub started: u64,
    pub ended: u64,
    pub result: RunResult,
    pub fetched: Option<Fetched>,
    pub run: Option<String>,
    /// How many actions ended each way (`done`, `blocked`, `skipped`, ...).
    pub outcomes: BTreeMap<String, usize>,
    /// Every action that didn't end cleanly, as `<outcome> <target> (<item>): <reason>`.
    pub details: Vec<String>,
    pub cancelled: bool,
    pub exit: u8,
    pub message: Option<String>,
}

impl LastRun {
    pub fn new(started: u64, result: RunResult, exit: u8) -> Self {
        Self {
            started,
            ended: started,
            result,
            fetched: None,
            run: None,
            outcomes: BTreeMap::new(),
            details: Vec::new(),
            cancelled: false,
            exit,
            message: None,
        }
    }

    /// The worker's run left the machine where it should be, or deliberately did nothing.
    pub fn healthy(&self) -> bool {
        match self.result {
            RunResult::Disabled | RunResult::Locked => true,
            RunResult::Applied => self.exit == 0,
            RunResult::Blocked | RunResult::Invalid | RunResult::Failed => false,
        }
    }
}

/// Saves `last-run.json`.
pub fn record(context: &Context, run: &LastRun) -> Result<(), Error> {
    let json = serde_json::to_vec_pretty(run).map_err(|error| state::Error::Invalid {
        path: Private::LastRun.path(context),
        message: error.to_string(),
    })?;
    state::write_private(context, Private::LastRun, &json)?;
    Ok(())
}

/// The last background run as its records show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub supervisor: Option<SupervisorRecord>,
    pub last_run: Option<LastRun>,
    /// A record that exists but can't be read.
    pub problem: Option<String>,
    pub log: Option<PathBuf>,
}

impl Summary {
    /// No background run yet, or the last one ended cleanly and left the machine healthy.
    pub fn healthy(&self) -> bool {
        self.problem.is_none()
            && self.supervisor.as_ref().is_none_or(SupervisorRecord::clean)
            && self.last_run.as_ref().is_none_or(LastRun::healthy)
    }

    /// One line on why it isn't healthy, or what happened.
    pub fn describe(&self) -> String {
        if let Some(problem) = &self.problem {
            return problem.clone();
        }
        let supervisor = self.supervisor.as_ref().map(|record| {
            if record.clean() {
                format!("exited {}", record.code.unwrap_or(1))
            } else {
                let mut text = record.outcome.clone();
                if let Some(problem) = &record.problem {
                    text.push_str(": ");
                    text.push_str(problem);
                } else if !record.log_complete {
                    text.push_str(
                        ": a process outside loadout's containment still held its output",
                    );
                }
                text
            }
        });
        let worker = self.last_run.as_ref().map(|run| {
            let result = serde_json::to_value(run.result)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            match &run.message {
                Some(message) => format!("{result}: {message}"),
                None => result,
            }
        });
        match (supervisor, worker) {
            (None, None) => "no background run yet".to_owned(),
            (Some(supervisor), None) => {
                format!("supervisor {supervisor}; the worker left no record")
            }
            (None, Some(worker)) => format!("worker {worker}"),
            (Some(supervisor), Some(worker)) => format!("worker {worker}; supervisor {supervisor}"),
        }
    }
}

/// Reads the background records. Never writes.
pub fn summary(context: &Context) -> Summary {
    let mut problem = None;
    let mut read = |file: Private| match state::read_private(context, file) {
        Ok(bytes) => bytes,
        Err(error) => {
            problem.get_or_insert_with(|| error.to_string());
            None
        }
    };
    let supervisor_bytes = read(Private::Supervisor);
    let run_bytes = read(Private::LastRun);
    let log = read(Private::WorkerLog).map(|_| Private::WorkerLog.path(context));
    let supervisor = parse(context, supervisor_bytes, Private::Supervisor, &mut problem);
    let last_run = parse(context, run_bytes, Private::LastRun, &mut problem);
    Summary {
        supervisor,
        last_run,
        problem,
        log,
    }
}

fn parse<T: DeserializeOwned>(
    context: &Context,
    bytes: Option<Vec<u8>>,
    file: Private,
    problem: &mut Option<String>,
) -> Option<T> {
    let bytes = bytes?;
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(error) => {
            problem.get_or_insert_with(|| {
                format!("{} is invalid: {error}", file.path(context).display())
            });
            None
        }
    }
}

pub(super) fn check(context: &Context) -> Check {
    let summary = summary(context);
    let status = if summary.supervisor.is_none()
        && summary.last_run.is_none()
        && summary.problem.is_none()
    {
        CheckStatus::Info
    } else if summary.healthy() {
        CheckStatus::Pass
    } else {
        CheckStatus::Warn
    };
    Check::new("background", status, summary.describe())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supervisor(outcome: &str, code: Option<i32>, problem: Option<&str>) -> SupervisorRecord {
        SupervisorRecord {
            started: 1,
            ended: 2,
            outcome: outcome.to_owned(),
            code,
            cancelled: false,
            problem: problem.map(str::to_owned),
            log_complete: true,
            log_dropped: 0,
        }
    }

    #[test]
    fn a_blocked_timed_out_or_uncontained_run_is_not_healthy() {
        let applied = LastRun::new(1, RunResult::Applied, 0);
        let clean = Summary {
            supervisor: Some(supervisor("exited", Some(0), None)),
            last_run: Some(applied.clone()),
            problem: None,
            log: None,
        };
        assert!(clean.healthy(), "{}", clean.describe());

        let blocked = Summary {
            last_run: Some(LastRun::new(1, RunResult::Blocked, 4)),
            ..clean.clone()
        };
        assert!(!blocked.healthy());

        let timed_out = Summary {
            supervisor: Some(supervisor("timed-out", None, None)),
            ..clean.clone()
        };
        assert!(!timed_out.healthy());

        let uncontained = Summary {
            supervisor: Some(supervisor(
                "exited",
                Some(0),
                Some("processes it started are still running"),
            )),
            ..clean.clone()
        };
        assert!(!uncontained.healthy());
        assert!(
            uncontained.describe().contains("still running"),
            "{}",
            uncontained.describe()
        );

        let disabled = Summary {
            last_run: Some(LastRun::new(1, RunResult::Disabled, 0)),
            ..clean
        };
        assert!(disabled.healthy());
    }

    #[test]
    fn records_round_trip_without_content() {
        let mut run = LastRun::new(5, RunResult::Blocked, 4);
        run.fetched = Some(Fetched::Blocked {
            reason: super::super::Blocked::Diverged,
        });
        run.message = Some("the clone and the remote have both moved on".to_owned());
        let json = serde_json::to_string(&run).unwrap();
        assert!(json.contains("\"result\":\"blocked\""), "{json}");
        assert!(json.contains("\"reason\":\"diverged\""), "{json}");
        let back: LastRun = serde_json::from_str(&json).unwrap();
        assert_eq!(back, run);
    }
}
