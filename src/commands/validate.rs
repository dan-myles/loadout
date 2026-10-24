use std::path::PathBuf;

use serde::Serialize;

use crate::cli::Exit;
use crate::diagnostic::{Diagnostics, Severity};
use crate::manifest::{Loadout, Scope};
use crate::resolve;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The loadout checkout to check
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    /// Print the result as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Serialize)]
struct Report<'a> {
    valid: bool,
    root: &'a PathBuf,
    layers: Vec<Layer<'a>>,
    items: usize,
    diagnostics: &'a Diagnostics,
}

#[derive(Serialize)]
struct Layer<'a> {
    #[serde(flatten)]
    scope: &'a Scope,
    file: &'a PathBuf,
}

pub fn run(args: &Args) -> anyhow::Result<Exit> {
    let (loadout, diagnostics) = match Loadout::load(&args.dir) {
        Ok((loadout, mut diagnostics)) => {
            diagnostics.extend(resolve::check_all(&loadout));
            (Some(loadout), diagnostics)
        }
        Err(diagnostics) => (None, diagnostics),
    };
    let loadout = loadout.as_ref();
    let diagnostics = &diagnostics;
    let valid = !diagnostics.has_errors();

    if args.json {
        let report = Report {
            valid,
            root: &args.dir,
            layers: loadout
                .iter()
                .flat_map(|loadout| &loadout.layers)
                .map(|file| Layer {
                    scope: &file.scope,
                    file: &file.file,
                })
                .collect(),
            items: loadout.map_or(0, Loadout::item_count),
            diagnostics,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for diagnostic in diagnostics.iter() {
            println!("{diagnostic}");
        }
        let warnings = diagnostics.count(Severity::Warning);
        match loadout.filter(|_| valid) {
            Some(loadout) => println!(
                "valid: {} ({} layers, {} items, {warnings} warnings)",
                args.dir.display(),
                loadout.layers.len(),
                loadout.item_count(),
            ),
            None => println!(
                "invalid: {} ({} errors, {warnings} warnings)",
                args.dir.display(),
                diagnostics.count(Severity::Error),
            ),
        }
    }

    Ok(if valid { Exit::Success } else { Exit::Invalid })
}
