use serde::Serialize;

use crate::cli::Exit;
use crate::context::Context;
use crate::sync::background::{self, Summary};
use crate::sync::service::{self, Status};
use crate::sync::{self, Error as SyncError};

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub action: Action,
}

#[derive(Debug, clap::Subcommand)]
pub enum Action {
    /// Install or replace the scheduled background sync for this user, at the interval the
    /// loadout sets
    Install,
    /// Remove the scheduled background sync
    Uninstall,
    /// Show whether the background sync is installed and how its last run went
    Status {
        /// Print the status as JSON
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    match args.action {
        Action::Install => install(context),
        Action::Uninstall => uninstall(context),
        Action::Status { json } => status(context, json),
    }
}

fn install(context: &Context) -> anyhow::Result<Exit> {
    if context.isolated {
        return refused(service::Error::Isolated);
    }
    let checkout = sync::checkout(context);
    let update = match sync::settings(context, &checkout) {
        Ok(update) => update,
        Err(SyncError::Invalid(diagnostics)) => {
            for diagnostic in diagnostics.iter() {
                println!("{diagnostic}");
            }
            println!("invalid: no service was installed");
            return Ok(Exit::Invalid);
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context(format!(
                "cannot read the loadout in {}; `loadout init <URL>` makes loadout's clone",
                checkout.display()
            )));
        }
    };
    if !update.auto {
        println!(
            "blocked: the loadout sets [update] auto = false for this machine, so no service was installed"
        );
        return Ok(Exit::Unhealthy);
    }
    match service::install(context, update.interval_minutes) {
        Ok(installed) => {
            for warning in &installed.warnings {
                println!("warning: {warning}");
            }
            println!(
                "installed: `{}` runs `loadout sync --background` every {} minutes",
                installed.exe.display(),
                installed.interval_minutes
            );
            Ok(Exit::Success)
        }
        Err(error) => refused(error),
    }
}

fn uninstall(context: &Context) -> anyhow::Result<Exit> {
    match service::uninstall(context) {
        Ok(()) => {
            println!("removed: no background sync is scheduled");
            Ok(Exit::Success)
        }
        Err(error) => refused(error),
    }
}

fn refused(error: service::Error) -> anyhow::Result<Exit> {
    if error.is_unhealthy() {
        println!("blocked: {error}");
        Ok(Exit::Unhealthy)
    } else {
        Err(error.into())
    }
}

#[derive(Serialize)]
struct Report<'a> {
    healthy: bool,
    installed: bool,
    service: &'a Status,
    background: &'a Summary,
}

fn status(context: &Context, json: bool) -> anyhow::Result<Exit> {
    let status = service::status(context)?;
    let summary = background::summary(context);
    let healthy = status.healthy() && summary.healthy();
    if json {
        let report = Report {
            healthy,
            installed: status.installed(),
            service: &status,
            background: &summary,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let line = if !status.installed() {
            "not installed".to_owned()
        } else if status.healthy() {
            let every = status
                .recorded
                .as_ref()
                .map(|recorded| format!(", every {} minutes", recorded.interval_minutes))
                .unwrap_or_default();
            format!("installed{every}")
        } else {
            service::describe(&status)
        };
        println!("{:<10} {line}", "service");
        if let Some(scheduler) = &status.scheduler {
            println!("{:<10} {}", "scheduler", scheduler.detail);
        }
        for advice in &status.advice {
            println!("{:<10} {advice}", "advice");
        }
        println!("{:<10} {}", "last run", summary.describe());
        if let Some(log) = &summary.log {
            println!("{:<10} {}", "log", log.display());
        }
    }
    Ok(if healthy {
        Exit::Success
    } else {
        Exit::Unhealthy
    })
}
