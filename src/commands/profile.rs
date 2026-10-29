use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::Context as _;
use serde::Serialize;

use crate::cli::Exit;
use crate::context::Context;
use crate::machine;
use crate::manifest::Loadout;
use crate::plan;
use crate::state;

#[derive(Debug, clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub action: Option<Action>,
    /// The loadout checkout to read profiles from [default: loadout's own clone in the state dir]
    #[arg(long, value_name = "DIR", global = true)]
    pub loadout: Option<PathBuf>,
}

#[derive(Debug, clap::Subcommand)]
pub enum Action {
    /// List the loadout's profiles and mark the ones this machine uses (the default)
    List {
        /// Print the list as JSON
        #[arg(long)]
        json: bool,
    },
    /// Pick this machine's profiles from the loadout's list, in a terminal
    Choose,
    /// Replace this machine's profiles; with none, it uses no profiles
    Set {
        /// Profiles from the loadout's tags/ folder, in order; a later one wins a clash
        #[arg(value_name = "PROFILE", value_parser = profile_name, value_delimiter = ',')]
        profiles: Vec<String>,
    },
}

pub fn profile_name(value: &str) -> Result<String, String> {
    if crate::id::is_id(value) {
        Ok(value.to_owned())
    } else {
        Err("must be lowercase letters, digits, - or _, starting with a letter or digit".to_owned())
    }
}

#[derive(Serialize)]
struct Listing<'a> {
    machine: &'a str,
    profiles: Vec<Entry<'a>>,
}

#[derive(Serialize)]
struct Entry<'a> {
    name: &'a str,
    chosen: bool,
    declared: bool,
    available: bool,
}

/// Shows or changes the profiles chosen on this machine. They are saved in the state dir's
/// `machine.toml`, never in the loadout, and every later plan, apply, and sync uses them.
pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    let checkout = plan::checkout(context, args.loadout.as_deref())?;
    let loadout = match Loadout::load_guarded(&checkout, context) {
        Ok((loadout, _)) => loadout,
        Err(diagnostics) => {
            for diagnostic in diagnostics.iter() {
                println!("{diagnostic}");
            }
            println!("invalid: fix the loadout first; `loadout validate` shows the same problems");
            return Ok(Exit::Invalid);
        }
    };
    let available = loadout.profiles();
    let name = machine::name(context)?.name;
    let declared: Vec<String> = loadout
        .machines
        .get(&name)
        .map(|entry| entry.tags.clone())
        .unwrap_or_default();
    let chosen = machine::profiles(context)?;
    match &args.action {
        None | Some(Action::List { json: false }) => {
            list(&name, &available, &declared, &chosen);
            Ok(Exit::Success)
        }
        Some(Action::List { json: true }) => {
            let mut names: Vec<&String> = available.iter().chain(&chosen).collect();
            let mut seen = std::collections::BTreeSet::new();
            names.retain(|name| seen.insert(*name));
            let listing = Listing {
                machine: &name,
                profiles: names
                    .into_iter()
                    .map(|profile| Entry {
                        name: profile,
                        chosen: chosen.contains(profile),
                        declared: declared.contains(profile),
                        available: available.contains(profile),
                    })
                    .collect(),
            };
            println!("{}", serde_json::to_string_pretty(&listing)?);
            Ok(Exit::Success)
        }
        Some(Action::Set { profiles }) => set(context, &available, profiles),
        Some(Action::Choose) => {
            if !(io::stdin().is_terminal() && io::stdout().is_terminal()) {
                anyhow::bail!(
                    "`loadout profile choose` asks in a terminal; use `loadout profile set PROFILE...` instead"
                );
            }
            let picked = ask(
                &available,
                &chosen,
                &declared,
                &mut io::stdin().lock(),
                &mut io::stdout(),
            )?;
            set(context, &available, &picked)
        }
    }
}

fn list(name: &str, available: &[String], declared: &[String], chosen: &[String]) {
    if available.is_empty() {
        println!("this loadout has no profiles (no tags/<name>.toml files)");
    } else {
        println!("profiles in this loadout:");
    }
    for profile in available {
        let mut notes = Vec::new();
        if chosen.contains(profile) {
            notes.push("chosen on this machine".to_owned());
        }
        if declared.contains(profile) {
            notes.push(format!("from [machines.{name}] in loadout.toml"));
        }
        if notes.is_empty() {
            println!("  {profile}");
        } else {
            println!("  {profile} ({})", notes.join("; "));
        }
    }
    for profile in chosen.iter().filter(|profile| !available.contains(profile)) {
        println!(
            "  {profile} (chosen on this machine, but the loadout has no tags/{profile}.toml)"
        );
    }
    println!("{}", summary(chosen));
}

fn summary(chosen: &[String]) -> String {
    if chosen.is_empty() {
        "this machine has no profiles chosen".to_owned()
    } else {
        format!("this machine's profiles: {}", chosen.join(", "))
    }
}

fn set(context: &Context, available: &[String], profiles: &[String]) -> anyhow::Result<Exit> {
    let unknown: Vec<&String> = profiles
        .iter()
        .filter(|profile| !available.contains(profile))
        .collect();
    if !unknown.is_empty() {
        for profile in &unknown {
            println!("no profile {profile} in this loadout (no tags/{profile}.toml)");
        }
        println!("{}", offered(available));
        println!("nothing changed");
        return Ok(Exit::Invalid);
    }
    let _lock = state::lock(context)?;
    machine::save_profiles(context, profiles).context("cannot save this machine's profiles")?;
    println!("{}", summary(profiles));
    println!(
        "saved in {}; run `loadout apply` to bring this machine in line now, or let the next sync do it",
        context.machine_file().display()
    );
    Ok(Exit::Success)
}

pub fn offered(available: &[String]) -> String {
    if available.is_empty() {
        "this loadout has no profiles".to_owned()
    } else {
        format!("profiles in this loadout: {}", available.join(", "))
    }
}

/// Asks which of `available` this machine uses, by number or name, separated by spaces or commas.
/// An empty answer or the end of input keeps `current`; `-` chooses none.
pub fn ask(
    available: &[String],
    current: &[String],
    declared: &[String],
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> anyhow::Result<Vec<String>> {
    writeln!(output, "profiles in this loadout:")?;
    for (number, profile) in available.iter().enumerate() {
        let note = if declared.contains(profile) {
            " (already given by [machines] in loadout.toml)"
        } else {
            ""
        };
        writeln!(output, "  {}. {profile}{note}", number + 1)?;
    }
    let shown = if current.is_empty() {
        "none".to_owned()
    } else {
        current.join(", ")
    };
    loop {
        write!(
            output,
            "profiles for this machine, by number or name; - for none [{shown}]: "
        )?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            return Ok(current.to_vec());
        }
        match parse(line.trim(), available) {
            Ok(None) => return Ok(current.to_vec()),
            Ok(Some(picked)) => return Ok(picked),
            Err(problem) => writeln!(output, "{problem}")?,
        }
    }
}

fn parse(answer: &str, available: &[String]) -> Result<Option<Vec<String>>, String> {
    if answer.is_empty() {
        return Ok(None);
    }
    if answer == "-" {
        return Ok(Some(Vec::new()));
    }
    let mut picked: Vec<String> = Vec::new();
    for word in answer.split([',', ' ']).filter(|word| !word.is_empty()) {
        let profile = match word.parse::<usize>() {
            Ok(number) => number
                .checked_sub(1)
                .and_then(|index| available.get(index))
                .ok_or_else(|| format!("{word} is not a number in the list"))?,
            Err(_) => available
                .iter()
                .find(|profile| *profile == word)
                .ok_or_else(|| format!("{word:?} is not a profile in this loadout"))?,
        };
        if !picked.contains(profile) {
            picked.push(profile.clone());
        }
    }
    Ok(Some(picked))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(current: &[&str], typed: &str) -> (Vec<String>, String) {
        let available: Vec<String> = ["development", "work"].map(str::to_owned).to_vec();
        let current: Vec<String> = current.iter().map(|name| (*name).to_owned()).collect();
        let mut output = Vec::new();
        let picked = ask(
            &available,
            &current,
            &[],
            &mut typed.as_bytes(),
            &mut output,
        )
        .unwrap();
        (picked, String::from_utf8(output).unwrap())
    }

    #[test]
    fn numbers_and_names_both_choose() {
        assert_eq!(pick(&[], "1\n").0, ["development"]);
        assert_eq!(pick(&[], "work, 1 work\n").0, ["work", "development"]);
    }

    #[test]
    fn an_empty_answer_or_end_of_input_keeps_the_current_choice() {
        assert!(pick(&[], "\n").0.is_empty());
        assert_eq!(pick(&["work"], "").0, ["work"]);
        assert_eq!(pick(&["work"], "\n").0, ["work"]);
    }

    #[test]
    fn a_dash_chooses_none() {
        assert!(pick(&["work"], "-\n").0.is_empty());
    }

    #[test]
    fn an_unknown_answer_is_asked_again() {
        let (picked, output) = pick(&[], "gaming\n3\n2\n");
        assert_eq!(picked, ["work"]);
        assert!(
            output.contains("\"gaming\" is not a profile in this loadout"),
            "{output}"
        );
        assert!(output.contains("3 is not a number in the list"), "{output}");
        assert_eq!(output.matches("[none]: ").count(), 3, "{output}");
    }
}
