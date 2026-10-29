use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::commands::{
    apply, doctor, init, plan, profile, rollback, service, status, sync, update, validate,
};
use crate::context::Context;

#[derive(Debug, Parser)]
#[command(
    name = "loadout",
    version,
    about = "Brings this machine to the setup declared in a loadout Git repository",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Check that this machine can run loadout
    Doctor(doctor::Args),
    /// Check a loadout for mistakes without touching this machine
    Validate(validate::Args),
    /// Show what loadout owns on this machine and the state of its runs
    Status(status::Args),
    /// Undo a run: restore its backups and delete what it created
    Rollback(rollback::Args),
    /// Show what apply would change on this machine, without changing anything
    Plan(plan::Args),
    /// Bring this machine in line with the loadout
    Apply(apply::Args),
    /// Fetch loadout's clone of the loadout, fast-forward it, and apply it
    Sync(sync::Args),
    /// Update the loadout CLI itself to the newest release
    #[command(visible_alias = "upgrade")]
    Update(update::Args),
    /// Install, remove, or check the scheduled background sync
    Service(service::Args),
    /// Set this machine up from a loadout: clone it, choose its profiles, apply, and keep it synced
    Init(init::Args),
    /// Show or change the profiles this machine uses
    Profile(profile::Args),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Success,
    Invalid,
    Unhealthy,
}

impl Exit {
    pub fn code(self) -> u8 {
        match self {
            Exit::Success => 0,
            Exit::Invalid => 3,
            Exit::Unhealthy => 4,
        }
    }
}

const FAILURE: u8 = 1;

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(exit) => ExitCode::from(exit.code()),
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(FAILURE)
        }
    }
}

pub fn run(cli: Cli) -> anyhow::Result<Exit> {
    if let Command::Sync(args) = &cli.command
        && args.recorded()
    {
        return sync::run_recorded(args);
    }
    // The scheduled job of older versions, `update --background --service FILE`, is a sync.
    if let Command::Update(args) = &cli.command
        && let Some(legacy) = args.legacy()
        && legacy.recorded()
    {
        update::deprecated();
        return sync::run_recorded(&legacy);
    }
    let context = Context::from_env()?;
    match cli.command {
        Command::Doctor(args) => doctor::run(&args, &context),
        Command::Validate(args) => validate::run(&args, &context),
        Command::Status(args) => status::run(&args, &context),
        Command::Rollback(args) => rollback::run(&args, &context),
        Command::Plan(args) => plan::run(&args, &context),
        Command::Apply(args) => apply::run(&args, &context),
        Command::Sync(args) => sync::run(&args, &context),
        Command::Update(args) => update::run(&args, &context),
        Command::Service(args) => service::run(&args, &context),
        Command::Init(args) => init::run(&args, &context),
        Command::Profile(args) => profile::run(&args, &context),
    }
}
