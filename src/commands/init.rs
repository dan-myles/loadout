use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

use crate::apply::{self, Mode, Options};
use crate::cli::Exit;
use crate::commands::apply::show;
use crate::commands::profile;
use crate::context::Context;
use crate::machine::{self, NameSource};
use crate::manifest::Loadout;
use crate::progress::Spinner;
use crate::state;
use crate::sync;

#[derive(Debug, clap::Args)]
pub struct Args {
    // The help text is given directly, not as a doc comment: it is the exact text `--help` prints,
    // and its example URLs are plain text there, not Markdown.
    #[arg(
        value_name = "URL",
        help = "The loadout Git repository, such as https://github.com/you/loadout.git or git@github.com:you/loadout.git"
    )]
    pub url: String,
    /// A profile for this machine, such as `work`; repeat it or separate names with commas.
    /// Without it, init asks in a terminal and chooses none elsewhere
    #[arg(long = "profile", value_name = "PROFILE", value_parser = profile::profile_name, value_delimiter = ',')]
    pub profiles: Vec<String>,
    /// This machine's name, needed only for a loadout that declares machines in [machines]
    /// [default: the saved name, else one made from the hostname]
    #[arg(long, value_name = "NAME", value_parser = machine_name)]
    pub name: Option<String>,
    /// Don't install the background sync service, even when the loadout's [update] auto is true
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
/// machine's name, its chosen profiles, and the loadout's URL, applies it, and installs the background sync service
/// unless the loadout's `[update] auto` is false or `--no-service` is given.
///
/// The existing setup, a conflicting `--name`, and a URL carrying a password are refused before
/// anything is written; a clone, unknown `--profile`, machine-file, apply or service failure can
/// still come after the clone. The state lock is held from the clone through the apply. A private loadout clones with
/// the user's own Git credentials or SSH keys; loadout stores no token, and shows the URL only
/// with its userinfo hidden.
pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    if machine::has_password(&args.url) {
        anyhow::bail!(
            "the URL carries a password; loadout stores no credential, so give git a credential helper or an SSH key instead"
        );
    }
    let checkout = sync::checkout(context);
    if checkout.exists() {
        anyhow::bail!(
            "loadout is already set up on this machine ({} exists); run `loadout sync` to apply the latest loadout, or `loadout profile set` to change this machine's profiles",
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
    println!(
        "cloning {} into {}",
        sync::redact(&args.url),
        checkout.display()
    );
    sync::clone(&args.url, context, &lock)?;

    let loadout = Loadout::load_guarded(&checkout, context)
        .ok()
        .map(|(loadout, _)| loadout);
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let name = match (&args.name, saved) {
        (Some(name), _) => name.clone(),
        (None, Some(saved)) => saved,
        (None, None) => choose(context, loadout.as_ref(), interactive)?,
    };
    let available = loadout.as_ref().map(Loadout::profiles).unwrap_or_default();
    let declared = loadout
        .as_ref()
        .and_then(|loadout| loadout.machines.get(&name))
        .map(|entry| entry.tags.clone())
        .unwrap_or_default();
    let profiles = if args.profiles.is_empty() && interactive && !available.is_empty() {
        profile::ask(
            &available,
            &[],
            &declared,
            &mut io::stdin().lock(),
            &mut io::stdout(),
        )?
    } else {
        args.profiles.clone()
    };
    // An invalid loadout lists no profiles; its apply below reports why, so nothing is refused here.
    let unknown: Vec<&String> = profiles
        .iter()
        .filter(|profile| loadout.is_some() && !available.contains(profile))
        .collect();
    if !unknown.is_empty() {
        machine::save(context, &name, &args.url, &[])?;
        for profile in unknown {
            println!("no profile {profile} in this loadout (no tags/{profile}.toml)");
        }
        println!("{}", profile::offered(&available));
        println!(
            "nothing applied: run `loadout profile set PROFILE...`, then `loadout sync` and `loadout service install`"
        );
        return Ok(Exit::Invalid);
    }
    machine::save(context, &name, &args.url, &profiles)?;
    println!("this machine is {name}");
    if !profiles.is_empty() {
        println!("profiles: {}", profiles.join(", "));
    } else if !available.is_empty() {
        let gets = if declared.is_empty() {
            "the global and OS layers only".to_owned()
        } else {
            format!("its [machines] tags ({})", declared.join(", "))
        };
        println!(
            "no profiles chosen, so this machine gets {gets}; `loadout profile set PROFILE...` or `loadout profile choose` adds some later ({})",
            profile::offered(&available)
        );
    }

    let options = Options {
        force: false,
        elevated: false,
        mode: Mode::Interactive,
    };
    let applied = show(apply::run_under_lock(context, &checkout, lock, options))?;
    if applied == Exit::Invalid {
        println!(
            "background service not installed: fix the loadout, then run `loadout sync` and `loadout service install`"
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
/// picks when the loadout declares machines. A loadout without [machines] never asks, since only
/// a declared machine's name changes what it gets.
fn choose(
    context: &Context,
    loadout: Option<&Loadout>,
    interactive: bool,
) -> anyhow::Result<String> {
    let default = machine::name(context)?.name;
    let listed: Vec<String> = loadout
        .map(|loadout| loadout.machines.keys().cloned().collect())
        .unwrap_or_default();
    if listed.is_empty() {
        return Ok(default);
    }
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
    if !listed.contains(&name) {
        println!(
            "note: the loadout's [machines] doesn't list {name}, so it gets the global and OS layers and its chosen profiles"
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
    let installed = {
        let _spinner = Spinner::start(true, "installing the background service");
        sync::service::install(context, settings.interval_minutes)
    };
    match installed {
        Ok(installed) => {
            for warning in &installed.warnings {
                println!("warning: {warning}");
            }
            println!(
                "background service installed: {} runs `loadout sync --background` every {} minutes",
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
