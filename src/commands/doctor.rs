use std::process::Command;

use serde::Serialize;

use crate::agents;
use crate::cli::Exit;
use crate::context::{self, Context};
use crate::diagnostic::{Check, CheckStatus};
use crate::machine::{self, NameSource};
use crate::manifest::Manager;
use crate::platform::{Arch, Os};
use crate::programs;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Print the checks as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    healthy: bool,
    checks: &'a [Check],
}

pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    let checks: Vec<Check> = [
        foundation(context),
        agents::checks(context),
        programs::checks(context),
    ]
    .concat();

    let healthy = checks.iter().all(|check| check.status != CheckStatus::Fail);
    if args.json {
        let report = Report {
            healthy,
            checks: &checks,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for check in &checks {
            let status = match check.status {
                CheckStatus::Pass => "ok",
                CheckStatus::Info => "info",
                CheckStatus::Warn => "warn",
                CheckStatus::Fail => "FAIL",
            };
            println!("{status:>4}  {:<16} {}", check.name, check.detail);
        }
    }

    Ok(if healthy {
        Exit::Success
    } else {
        Exit::Unhealthy
    })
}

fn foundation(context: &Context) -> Vec<Check> {
    vec![
        Check::new(
            "version",
            CheckStatus::Info,
            format!("loadout {}", env!("CARGO_PKG_VERSION")),
        ),
        platform(),
        home(context),
        Check::new(
            "config dir",
            CheckStatus::Info,
            context.config.display().to_string(),
        ),
        state(context),
        machine_name(context),
        git(),
        managers(context.os),
    ]
}

fn platform() -> Check {
    match (Os::current(), Arch::current()) {
        (Ok(os), Ok(arch)) => {
            let distro = match os {
                Os::Linux => std::fs::read_to_string("/etc/os-release")
                    .ok()
                    .and_then(|text| {
                        text.lines()
                            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                            .map(|name| format!(" ({})", name.trim_matches('"')))
                    })
                    .unwrap_or_default(),
                Os::Macos | Os::Windows => String::new(),
            };
            Check::new(
                "platform",
                CheckStatus::Pass,
                format!("{os} {arch}{distro}"),
            )
        }
        (Err(error), _) | (_, Err(error)) => {
            Check::new("platform", CheckStatus::Fail, error.to_string())
        }
    }
}

fn home(context: &Context) -> Check {
    let source = if context.isolated {
        format!(
            " (from {}; ambient XDG and agent dir overrides ignored)",
            context::HOME_OVERRIDE
        )
    } else {
        String::new()
    };
    if context.home.is_dir() {
        Check::new(
            "home",
            CheckStatus::Pass,
            format!("{}{source}", context.home.display()),
        )
    } else {
        Check::new(
            "home",
            CheckStatus::Fail,
            format!("{}{source} is not a directory", context.home.display()),
        )
    }
}

fn state(context: &Context) -> Check {
    let detail = if context.state.is_dir() {
        context.state.display().to_string()
    } else {
        format!("{} (not created yet)", context.state.display())
    };
    Check::new("state dir", CheckStatus::Info, detail)
}

fn machine_name(context: &Context) -> Check {
    match machine::name(context) {
        Ok(name) => {
            let detail = match name.source {
                NameSource::Saved => format!(
                    "{} (saved in {})",
                    name.name,
                    context.machine_file().display()
                ),
                NameSource::Hostname => format!("{} (from the hostname; not saved yet)", name.name),
            };
            Check::new("machine name", CheckStatus::Info, detail)
        }
        Err(error) => Check::new("machine name", CheckStatus::Fail, error.to_string()),
    }
}

fn git() -> Check {
    let output = Command::new("git").arg("--version").output();
    match output {
        Ok(output) if output.status.success() => Check::new(
            "git",
            CheckStatus::Pass,
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ),
        Ok(output) => Check::new(
            "git",
            CheckStatus::Fail,
            format!("git --version exited with {}", output.status),
        ),
        Err(_) => Check::new(
            "git",
            CheckStatus::Fail,
            format!(
                "git is not on PATH; loadout needs it to fetch the loadout. Install it with {}",
                match Os::current() {
                    Ok(Os::Macos) => "xcode-select --install or brew install git",
                    Ok(Os::Windows) => "winget install --id Git.Git",
                    Ok(Os::Linux) | Err(_) =>
                        "your distribution's package manager, such as sudo apt install git",
                }
            ),
        ),
    }
}

fn managers(os: Os) -> Check {
    let found: Vec<&str> = Manager::ALL
        .into_iter()
        .filter(|manager| *manager != Manager::Cask)
        .filter(|manager| context::which(manager.executable()).is_some())
        .map(Manager::id)
        .collect();
    let detail = if found.is_empty() {
        format!("none of loadout's package managers are on PATH ({os})")
    } else {
        found.join(", ")
    };
    Check::new("managers", CheckStatus::Info, detail)
}
