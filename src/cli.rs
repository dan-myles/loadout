use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::commands::{doctor, validate};
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
    let context = Context::from_env()?;
    match cli.command {
        Command::Doctor(args) => doctor::run(&args, &context),
        Command::Validate(args) => validate::run(&args, &context),
    }
}
