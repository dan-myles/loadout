use std::path::{Path, PathBuf};

use super::{Agent, Resolved, report};
use crate::context::{Base, Context, split_base, windows_problem};
use crate::diagnostic::{Diagnostics, Severity};
use crate::platform::Os;
use crate::reserved::{self, Merge, Use};

#[derive(Debug)]
pub(super) struct Write {
    pub(super) owner: String,
    pub(super) key: String,
    pub(super) path: String,
    pub(super) file: Option<PathBuf>,
    pub(super) rank: Option<usize>,
}

struct Expanded<'a> {
    write: &'a Write,
    shown: String,
    folded: Vec<String>,
}

pub(super) fn check(os: Os, writes: &[Write], diagnostics: &mut Diagnostics) {
    let config = config_dir(os);
    let mut expanded = Vec::new();
    for write in writes {
        let Some((base, rest)) = split_base(&write.path) else {
            report(
                diagnostics,
                Severity::Error,
                write.file.as_deref(),
                &write.key,
                format!("{} is not a clean ~/ or {{config}}/ path", write.path),
            );
            continue;
        };
        if os == Os::Windows
            && let Some(problem) = windows_problem(rest)
        {
            report(
                diagnostics,
                Severity::Error,
                write.file.as_deref(),
                &write.key,
                format!(
                    "{problem}, so {} has no valid Windows path for {}; remove or override it in os/windows.toml, or add a when condition",
                    write.path, write.owner
                ),
            );
            continue;
        }
        let mut parts: Vec<&str> = match base {
            Base::Home => Vec::new(),
            Base::Config => config.iter().map(String::as_str).collect(),
        };
        parts.extend(rest.split('/'));
        let folded = parts.iter().map(|part| fold(os, part)).collect::<Vec<_>>();
        expanded.push(Expanded {
            write,
            shown: format!("~/{}", parts.join("/")),
            folded,
        });
    }

    for (index, one) in expanded.iter().enumerate() {
        for other in &expanded[..index] {
            let (later, earlier) = if one.write.rank >= other.write.rank {
                (one, other)
            } else {
                (other, one)
            };
            let message = if later.folded == earlier.folded {
                format!(
                    "{} ({}) and {} ({}) both write {}",
                    later.write.owner,
                    later.write.path,
                    earlier.write.owner,
                    earlier.write.path,
                    later.shown
                )
            } else if let Some((inner, outer)) = nested(later, earlier) {
                format!(
                    "{} writes {} inside {} ({}); one path cannot be both a file and a folder",
                    inner.write.owner, inner.shown, outer.write.owner, outer.shown
                )
            } else {
                continue;
            };
            report(
                diagnostics,
                Severity::Error,
                later.write.file.as_deref(),
                &later.write.key,
                message,
            );
        }
    }
}

fn nested<'b, 'a>(
    one: &'b Expanded<'a>,
    other: &'b Expanded<'a>,
) -> Option<(&'b Expanded<'a>, &'b Expanded<'a>)> {
    let inside = |inner: &Expanded<'_>, outer: &Expanded<'_>| {
        inner.folded.len() > outer.folded.len() && inner.folded.starts_with(&outer.folded)
    };
    if inside(one, other) {
        Some((one, other))
    } else if inside(other, one) {
        Some((other, one))
    } else {
        None
    }
}

/// Linux volumes are case-sensitive by default and macOS and Windows volumes are not. The
/// actual volume is probed at plan time (U4); validate never writes a probe file.
fn fold(os: Os, part: &str) -> String {
    match os {
        Os::Linux => part.to_owned(),
        Os::Macos | Os::Windows => part.to_lowercase(),
    }
}

fn config_dir(os: Os) -> Vec<String> {
    let home = PathBuf::from("home");
    let context = Context::resolve(os, |_| None, Some(home.clone()))
        .expect("a context with a home and no environment always resolves");
    context
        .config
        .strip_prefix(&home)
        .expect("without overrides, {config} lies under home")
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect()
}

pub(super) fn check_targets(resolved: &Resolved, context: &Context, loadout: &Path) -> Diagnostics {
    let mut diagnostics = Diagnostics::default();
    let mut check = |key: String, path: &str, how: Use<'_>| {
        let message = match context.expand(path) {
            None => format!("{path} is not a valid path on {}", context.os),
            Some(expanded) => match reserved::check_path(context, loadout, &expanded, how) {
                Some(conflict) => format!("{} {}", expanded.display(), conflict.message()),
                None => return,
            },
        };
        report(&mut diagnostics, Severity::Error, None, key, message);
    };
    for (id, file) in &resolved.files {
        check(format!("files.{id}.target"), &file.target, Use::Write);
    }
    for agent in &resolved.agents {
        let Agent::Custom { id, declaration } = agent else {
            continue;
        };
        let os = context.os;
        let key = format!("agents.custom.{id}");
        if let Some(path) = declaration.skills.as_ref().and_then(|paths| paths.get(os)) {
            check(format!("{key}.skills.{os}"), path, Use::WriteInto);
        }
        if let Some(path) = declaration
            .instructions
            .as_ref()
            .and_then(|paths| paths.get(os))
        {
            check(format!("{key}.instructions.{os}"), path, Use::Write);
        }
        if let Some(mcp) = &declaration.mcp
            && let paths = mcp.paths()
            && let Some(path) = paths.get(os)
        {
            let merge = Use::KeyMerge(Merge {
                map: &mcp.map,
                format: mcp.format.id(),
                shape: mcp.shape.id(),
            });
            check(format!("{key}.mcp.{os}"), path, merge);
        }
    }
    diagnostics
}
