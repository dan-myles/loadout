use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Serialize;

use crate::cli::Exit;
use crate::context::Context;
use crate::diagnostic::Diagnostics;
use crate::plan::{self, Action};
use crate::state::{self, State};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The loadout checkout to plan from [default: loadout's own clone in the state dir]
    #[arg(long, value_name = "DIR")]
    pub loadout: Option<PathBuf>,
    /// Print the plan as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    machine: &'a str,
    checkout: &'a Path,
    actions: &'a [Action],
    warnings: &'a Diagnostics,
}

#[derive(Serialize)]
struct Refusal<'a> {
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<&'a Diagnostics>,
}

/// Prints what `apply` would do on this machine. Reads the checkout, the machine, the state, and
/// the targets; never writes, never takes the lock, and never creates the state dir.
pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    let checkout = plan::checkout(context, args.loadout.as_deref())?;
    let state = State::load(context).context("cannot read loadout's ownership records")?;
    let built = match plan::build(context, &checkout, &state, None, plan::Options::default()) {
        Ok(built) => built,
        Err(plan::Error::Invalid(diagnostics)) => {
            if args.json {
                print_json(&Refusal {
                    error: "invalid",
                    message: None,
                    diagnostics: Some(&diagnostics),
                })?;
            } else {
                for diagnostic in diagnostics.iter() {
                    println!("{diagnostic}");
                }
                println!("invalid: nothing can be planned for this machine");
            }
            return Ok(Exit::Invalid);
        }
        Err(plan::Error::Blocked(message)) => {
            if args.json {
                print_json(&Refusal {
                    error: "blocked",
                    message: Some(&message),
                    diagnostics: None,
                })?;
            } else {
                println!("blocked: {message}");
            }
            return Ok(Exit::Unhealthy);
        }
        Err(plan::Error::Runtime(error)) => return Err(error),
    };

    if args.json {
        print_json(&Report {
            machine: &built.machine,
            checkout: &built.checkout,
            actions: &built.plan.actions,
            warnings: &built.warnings,
        })?;
    } else {
        for diagnostic in built.warnings.iter() {
            println!("{diagnostic}");
        }
        println!(
            "plan for {} from {}",
            built.machine,
            built.checkout.display()
        );
        for action in &built.plan.actions {
            let reason = action
                .change
                .reason()
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default();
            println!(
                "  {:<11} {}  ({}){reason}",
                action.change.name(),
                state::label(&action.target),
                action.item.label()
            );
        }
        let changes = built
            .plan
            .actions
            .iter()
            .filter(|action| {
                !matches!(
                    action.change,
                    plan::Change::Unchanged
                        | plan::Change::Unsupported(_)
                        | plan::Change::Blocked(_)
                )
            })
            .count();
        println!(
            "{changes} of {} actions would change this machine",
            built.plan.actions.len()
        );
    }

    Ok(if built.plan.blocked() {
        Exit::Unhealthy
    } else {
        Exit::Success
    })
}

fn print_json(value: &impl Serialize) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
