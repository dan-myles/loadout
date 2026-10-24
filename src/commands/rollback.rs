use crate::cli::Exit;
use crate::context::Context;
use crate::state;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The run to undo, as `loadout status` lists it; defaults to the newest run
    pub run: Option<String>,
}

pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    match state::rollback(context, args.run.as_deref()) {
        Ok(rolled_back) => {
            println!(
                "rolled back run {}: restored {} target(s)",
                rolled_back.run,
                rolled_back.restored.len()
            );
            for target in &rolled_back.restored {
                println!("  {}", state::label(target));
            }
            Ok(Exit::Success)
        }
        Err(error) if error.is_blocked() => {
            eprintln!("error: {error}");
            Ok(Exit::Unhealthy)
        }
        Err(error) => Err(error.into()),
    }
}
