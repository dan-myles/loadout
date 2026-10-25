use std::path::PathBuf;

use anyhow::Context as _;
use serde::Serialize;

use crate::cli::Exit;
use crate::context::Context;
use crate::plan::{ItemRef, Target};
use crate::state::{self, Health, Record, RunStatus, RunSummary, State};
use crate::sync::background::{self, Summary};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Print the status as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    state_dir: &'a PathBuf,
    healthy: bool,
    records: Vec<Owned<'a>>,
    runs: &'a [RunSummary],
    background: &'a Summary,
}

#[derive(Serialize)]
struct Owned<'a> {
    target: &'a Target,
    item: &'a ItemRef,
    sha256: &'a str,
    run: &'a str,
    #[serde(flatten)]
    health: &'a Health,
}

pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    let state = State::load(context).context("cannot read loadout's ownership records")?;
    let checkout = context.state.join("loadout");
    let audit = state::audit(context, &checkout, &state);
    let runs = state::runs(context).context("cannot list loadout's runs")?;
    let background = background::summary(context);

    let healthy = audit.iter().all(|(_, health)| *health == Health::Ok)
        && runs.iter().all(|run| {
            matches!(
                run.status,
                Some(RunStatus::Finished | RunStatus::RolledBack | RunStatus::InProgress)
            )
        })
        && background.healthy();

    if args.json {
        let report = Report {
            state_dir: &context.state,
            healthy,
            records: audit
                .iter()
                .map(|(record, health)| Owned {
                    target: &record.target,
                    item: &record.item,
                    sha256: &record.sha256,
                    run: &record.run,
                    health,
                })
                .collect(),
            runs: &runs,
            background: &background,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_text(context, &audit, &runs);
        println!("{:<9} {}", "background", background.describe());
    }

    Ok(if healthy {
        Exit::Success
    } else {
        Exit::Unhealthy
    })
}

fn print_text(context: &Context, audit: &[(&Record, Health)], runs: &[RunSummary]) {
    println!("{:<9} {}", "state dir", context.state.display());
    let off = audit
        .iter()
        .filter(|(_, health)| *health != Health::Ok)
        .count();
    let summary = if off == 0 {
        String::new()
    } else {
        format!(", {off} not as loadout left them")
    };
    println!("{:<9} {} owned{summary}", "records", audit.len());
    for (record, health) in audit {
        let (word, detail) = match health {
            Health::Ok => ("ok", String::new()),
            Health::Drifted => ("drifted", " (changed outside loadout)".to_owned()),
            Health::Missing => ("missing", " (deleted outside loadout)".to_owned()),
            Health::Unchecked(reason) => ("unchecked", format!(" ({reason})")),
        };
        println!(
            "  {word:<11} {:<28} {}{detail}",
            item(&record.item),
            state::label(&record.target)
        );
    }
    println!("{:<9} {}", "runs", runs.len());
    for run in runs {
        let word = match run.status {
            Some(RunStatus::InProgress) => "running",
            Some(RunStatus::Interrupted) => "interrupted",
            Some(RunStatus::Finished) => "finished",
            Some(RunStatus::RolledBack) => "rolled back",
            None => "unreadable",
        };
        let detail = match (&run.status, &run.error) {
            (_, Some(error)) => error.clone(),
            (Some(RunStatus::Interrupted), None) => format!(
                "{} of {} actions done; undo it with `loadout rollback {}`",
                run.done, run.actions, run.id
            ),
            _ => format!("{} of {} actions done", run.done, run.actions),
        };
        println!("  {word:<11} {}  {detail}", run.id);
    }
}

fn item(item: &ItemRef) -> String {
    let kind = serde_json::to_value(item.kind)
        .ok()
        .and_then(|kind| kind.as_str().map(str::to_owned))
        .unwrap_or_default();
    match &item.agent {
        Some(agent) => format!("{kind} {} for {agent}", item.id),
        None => format!("{kind} {}", item.id),
    }
}
