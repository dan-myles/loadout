use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use crate::apply::{self, Mode, Options};
use crate::cli::Exit;
use crate::commands::apply::show;
use crate::context::Context;
use crate::machine::{self, NameSource};
use crate::manifest::Loadout;
use crate::state;
use crate::sync;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// The loadout Git repository, such as https://github.com/you/loadout.git or git@github.com:you/loadout.git
    #[arg(value_name = "URL")]
    pub url: String,
    /// This machine's name in the loadout [default: the saved name, else one made from the hostname]
    #[arg(long, value_name = "NAME", value_parser = machine_name)]
    pub name: Option<String>,
    /// Don't install the background update service, even when the loadout's [update] auto is true
    #[arg(long)]
    pub no_service: bool,
}

fn machine_name(value: &str) -> Result<String, String> {
    if crate::id::is_id(value) {
        Ok(value.to_owned())
    } else {
        Err("must be lowercase letters, digits, - or _, starting with a letter or digit".to_owned())
    }
}

/// Sets this machine up from a loadout: clones it into loadout's own checkout, saves the
/// machine's name and the loadout's URL, applies it, and installs the background update service
/// unless the loadout's `[update] auto` is false or `--no-service` is given.
///
/// Every check that can refuse runs before anything is written. The state lock is held from
/// the clone through the apply. A private loadout clones with the user's own Git credentials or
/// SSH keys; loadout stores no token.
pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    let checkout = sync::checkout(context);
    if checkout.exists() {
        anyhow::bail!(
            "loadout is already set up on this machine ({} exists); run `loadout update` to apply the latest loadout",
            checkout.display()
        );
    }
    let saved = match machine::name(context)? {
        machine::Name {
            name,
            source: NameSource::Saved,
        } => Some(name),
        machine::Name { .. } => None,
    };
    if let (Some(saved), Some(wanted)) = (&saved, &args.name)
        && saved != wanted
    {
        anyhow::bail!(
            "this machine is already named {saved} in {}; machine names don't change, so remove that file first to rename it",
            context.machine_file().display()
        );
    }

    let lock = state::lock(context)?;
    println!("cloning {} into {}", args.url, checkout.display());
    sync::clone(&args.url, context, &lock)?;

    let name = match (&args.name, saved) {
        (Some(name), _) => name.clone(),
        (None, Some(saved)) => saved,
        (None, None) => choose(context, &checkout)?,
    };
    machine::save(context, &name, &args.url)?;
    println!("this machine is {name}");

    let options = Options {
        force: false,
        elevated: false,
        mode: Mode::Interactive,
    };
    let applied = show(apply::run_under_lock(context, &checkout, lock, options))?;
    if applied == Exit::Invalid {
        println!(
            "background service not installed: fix the loadout, then run `loadout update` and `loadout service install`"
        );
        return Ok(applied);
    }

    let service = service(args, context, &checkout)?;
    Ok(
        if applied == Exit::Unhealthy || service == Exit::Unhealthy {
            Exit::Unhealthy
        } else {
            Exit::Success
        },
    )
}

/// The name for a machine with no saved name: the hostname's, or in a terminal the one the user
/// picks, preferably among the machines the loadout lists.
fn choose(context: &Context, checkout: &Path) -> anyhow::Result<String> {
    let default = machine::name(context)?.name;
    let listed: Vec<String> = Loadout::load_guarded(checkout, context)
        .map(|(loadout, _)| loadout.machines.into_keys().collect())
        .unwrap_or_default();
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let name = if interactive {
        prompt(
            &default,
            &listed,
            &mut io::stdin().lock(),
            &mut io::stdout(),
        )?
    } else {
        default
    };
    if !listed.is_empty() && !listed.contains(&name) {
        println!(
            "note: the loadout's [machines] doesn't list {name}, so it gets the global and OS layers only"
        );
    }
    Ok(name)
}

fn prompt(
    default: &str,
    listed: &[String],
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> anyhow::Result<String> {
    if !listed.is_empty() {
        writeln!(output, "machines in this loadout: {}", listed.join(", "))?;
    }
    loop {
        write!(output, "name for this machine [{default}]: ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(default.to_owned());
        }
        let answer = line.trim();
        if answer.is_empty() {
            return Ok(default.to_owned());
        }
        match machine_name(answer) {
            Ok(name) => return Ok(name),
            Err(problem) => writeln!(output, "{answer:?} {problem}")?,
        }
    }
}

fn service(args: &Args, context: &Context, checkout: &Path) -> anyhow::Result<Exit> {
    if args.no_service {
        println!(
            "background service not installed (--no-service); `loadout service install` adds it"
        );
        return Ok(Exit::Success);
    }
    let settings = sync::settings(context, checkout)?;
    if !settings.auto {
        println!("background service not installed: the loadout sets [update] auto = false");
        return Ok(Exit::Success);
    }
    match sync::service::install(context, settings.interval_minutes) {
        Ok(installed) => {
            for warning in &installed.warnings {
                println!("warning: {warning}");
            }
            println!(
                "background service installed: {} runs `loadout update --background` every {} minutes",
                installed.exe.display(),
                installed.interval_minutes
            );
            Ok(Exit::Success)
        }
        Err(error) if error.is_unhealthy() => {
            println!("background service not installed: {error}");
            Ok(Exit::Unhealthy)
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(default: &str, listed: &[&str], typed: &str) -> (String, String) {
        let listed: Vec<String> = listed.iter().map(|name| (*name).to_owned()).collect();
        let mut output = Vec::new();
        let name = prompt(default, &listed, &mut typed.as_bytes(), &mut output).unwrap();
        (name, String::from_utf8(output).unwrap())
    }

    #[test]
    fn an_empty_answer_or_end_of_input_keeps_the_default() {
        assert_eq!(ask("desk", &["work-laptop"], "\n").0, "desk");
        assert_eq!(ask("desk", &[], "").0, "desk");
    }

    #[test]
    fn a_valid_name_is_taken_and_an_invalid_one_asked_again() {
        let (name, output) = ask(
            "desk",
            &["work-laptop", "desk"],
            "Work Laptop\nwork-laptop\n",
        );
        assert_eq!(name, "work-laptop");
        assert!(
            output.contains("machines in this loadout: work-laptop, desk"),
            "{output}"
        );
        assert!(
            output.contains("\"Work Laptop\" must be lowercase"),
            "{output}"
        );
        assert_eq!(output.matches("name for this machine [desk]: ").count(), 2);
    }

    #[test]
    fn name_flag_uses_the_id_rule() {
        assert!(machine_name("work-laptop").is_ok());
        for bad in ["", "Work", "-x", "a b", "../x"] {
            assert!(machine_name(bad).is_err(), "{bad}");
        }
    }
}
