use std::collections::BTreeSet;
use std::path::PathBuf;

use super::resolve;
use crate::diagnostic::{Diagnostic, Diagnostics, Severity};
use crate::machine::Facts;
use crate::manifest::{Condition, Layer, Loadout, OneOrMany, Scope};
use crate::platform::{Arch, Os};

const ARCHES: [Arch; 2] = [Arch::X86_64, Arch::Aarch64];
const UNLISTED: &str = "unlisted";

struct Machine {
    name: String,
    label: String,
    profiles: Vec<String>,
    /// A profile alone whose tag a declared machine lists: it may be written only to go with
    /// that machine's other tags, so a problem seen nowhere else stays a warning.
    composed: bool,
}

struct Variant {
    machine: usize,
    os: Os,
    arch: Arch,
    distro: Option<String>,
}

pub(super) fn check_all(loadout: &Loadout) -> Diagnostics {
    let machines = machines(loadout);
    let distros: BTreeSet<String> = conditions(loadout)
        .flat_map(|condition| condition.distro.iter().flat_map(OneOrMany::values))
        .cloned()
        .collect();

    let mut variants = Vec::new();
    for machine in 0..machines.len() {
        for os in Os::ALL {
            let distros: Vec<Option<String>> = match os {
                Os::Linux => std::iter::once(None)
                    .chain(distros.iter().cloned().map(Some))
                    .collect(),
                Os::Macos | Os::Windows => vec![None],
            };
            for arch in ARCHES {
                for distro in &distros {
                    variants.push(Variant {
                        machine,
                        os,
                        arch,
                        distro: distro.clone(),
                    });
                }
            }
        }
    }

    let mut groups: Vec<(Diagnostic, Vec<usize>)> = Vec::new();
    for (index, variant) in variants.iter().enumerate() {
        let facts = Facts {
            name: machines[variant.machine].name.clone(),
            os: variant.os,
            distro: variant.distro.clone(),
            arch: variant.arch,
            home: PathBuf::from("~"),
            profiles: machines[variant.machine].profiles.clone(),
        };
        let diagnostics = match resolve(loadout, &facts) {
            Ok((_, warnings)) => warnings,
            Err(diagnostics) => diagnostics,
        };
        for diagnostic in diagnostics.iter() {
            match groups.iter_mut().find(|(seen, _)| seen == diagnostic) {
                Some((_, hits)) => hits.push(index),
                None => groups.push((diagnostic.clone(), vec![index])),
            }
        }
    }

    let mut out = Diagnostics::default();
    for (mut diagnostic, hits) in groups {
        if diagnostic.severity == Severity::Error
            && hits
                .iter()
                .all(|index| machines[variants[*index].machine].composed)
        {
            diagnostic.severity = Severity::Warning;
            diagnostic.message = format!(
                "{}; [machines] lists this tag with others, so only a machine that chooses it alone as a profile would fail",
                diagnostic.message
            );
        }
        diagnostic.message = format!(
            "{} (on {})",
            diagnostic.message,
            describe(&machines, &variants, &hits)
        );
        out.push(diagnostic);
    }
    out
}

fn machines(loadout: &Loadout) -> Vec<Machine> {
    let mut machines: Vec<Machine> = loadout
        .machines
        .keys()
        .map(|name| Machine {
            name: name.clone(),
            label: name.clone(),
            profiles: Vec::new(),
            composed: false,
        })
        .collect();
    let named: BTreeSet<&String> = conditions(loadout)
        .flat_map(|condition| condition.machine.iter().flat_map(OneOrMany::values))
        .collect();
    for name in &named {
        if !loadout.machines.contains_key(*name) {
            machines.push(Machine {
                name: (*name).clone(),
                label: format!("unlisted machine {name}"),
                profiles: Vec::new(),
                composed: false,
            });
        }
    }
    let taken = |name: &String| loadout.machines.contains_key(name) || named.contains(name);
    let candidates = loadout.machines.len() + named.len() + 1;
    let name = (1..=candidates)
        .map(|number| match number {
            1 => UNLISTED.to_owned(),
            _ => format!("{UNLISTED}-{number}"),
        })
        .find(|name| !taken(name))
        .expect("more candidates than taken names");
    for file in &loadout.layers {
        if let Scope::Tag(tag) = &file.scope {
            machines.push(Machine {
                name: name.clone(),
                label: format!("a machine with only profile {tag}"),
                profiles: vec![tag.clone()],
                composed: loadout
                    .machines
                    .values()
                    .any(|entry| entry.tags.contains(tag)),
            });
        }
    }
    machines.push(Machine {
        name,
        label: "any other unlisted machine".to_owned(),
        profiles: Vec::new(),
        composed: false,
    });
    machines
}

fn conditions(loadout: &Loadout) -> impl Iterator<Item = &Condition> {
    loadout
        .layers
        .iter()
        .flat_map(|file| layer_conditions(&file.layer))
}

fn layer_conditions(layer: &Layer) -> impl Iterator<Item = &Condition> {
    let files = layer.files.values().map(|item| item.when.as_ref());
    let skills = layer.skills.values().map(|item| item.when.as_ref());
    let instructions = layer.instructions.values().map(|item| item.when.as_ref());
    let mcp = layer.mcp.values().map(|item| item.when.as_ref());
    let programs = layer.programs.values().map(|item| item.when.as_ref());
    files
        .chain(skills)
        .chain(instructions)
        .chain(mcp)
        .chain(programs)
        .flatten()
}

fn describe(machines: &[Machine], variants: &[Variant], hits: &[usize]) -> String {
    if hits.len() == variants.len() {
        return "every machine and OS".to_owned();
    }
    let distros = variants.iter().any(|variant| variant.distro.is_some());
    let mut parts = Vec::new();
    for (machine_index, machine) in machines.iter().enumerate() {
        let mut oses = Vec::new();
        for os in Os::ALL {
            let all: Vec<usize> = (0..variants.len())
                .filter(|index| {
                    variants[*index].machine == machine_index && variants[*index].os == os
                })
                .collect();
            let hit: Vec<usize> = all
                .iter()
                .copied()
                .filter(|index| hits.contains(index))
                .collect();
            if hit.is_empty() {
                continue;
            }
            if hit.len() == all.len() {
                oses.push(os.to_string());
                continue;
            }
            for index in hit {
                let variant = &variants[index];
                let distro = match (&variant.distro, os) {
                    (Some(distro), _) => format!(" {distro}"),
                    (None, Os::Linux) if distros => " with another distro".to_owned(),
                    (None, _) => String::new(),
                };
                oses.push(format!("{os} {}{distro}", variant.arch));
            }
        }
        if !oses.is_empty() {
            parts.push(format!("{}: {}", machine.label, oses.join(", ")));
        }
    }
    parts.join("; ")
}
