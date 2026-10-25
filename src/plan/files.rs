use std::path::Path;

use super::{Action, Content, Desired, ItemKind, ItemRef, Target, error};
use crate::context::Context;
use crate::diagnostic::Diagnostics;
use crate::machine::Facts;
use crate::reserved::{self, Use};
use crate::resolve::Resolved;
use crate::source::Sources;
use crate::template;

/// What the producers hand to the comparison: content that should be on disk, and items that
/// can't be placed on this machine.
#[derive(Debug, Default)]
pub(super) struct Produced {
    pub(super) desired: Vec<Desired>,
    pub(super) unsupported: Vec<Action>,
}

/// User files: each target is checked against the reserved table before its source is read, and
/// every template renders before any action exists, so a missing variable or a refused source
/// stops the run before any destination is written.
pub(super) fn produce(
    resolved: &Resolved,
    facts: &Facts,
    context: &Context,
    loadout: &Path,
    sources: &Sources<'_>,
    problems: &mut Diagnostics,
) -> Produced {
    let mut produced = Produced::default();
    for (id, file) in &resolved.files {
        let item = ItemRef {
            kind: ItemKind::File,
            id: id.clone(),
            agent: None,
        };
        let Some(path) = context.expand(&file.target) else {
            problems.push(error(
                &format!("files.{id}.target"),
                format!("{} is not a valid path on {}", file.target, context.os),
            ));
            continue;
        };
        if let Some(conflict) = reserved::check_path(context, loadout, &path, Use::Write) {
            let reason = format!("{} {}", path.display(), conflict.message());
            produced
                .unsupported
                .push(Action::unsupported(item, Target::Path { path }, reason));
            continue;
        }
        let mut copy = match sources.file(&file.source) {
            Ok(copy) => copy,
            Err(message) => {
                problems.push(error(&format!("files.{id}.source"), message));
                continue;
            }
        };
        if file.template {
            let name = file.source.strip_prefix(loadout).unwrap_or(&file.source);
            let Ok(text) = std::str::from_utf8(&copy.bytes) else {
                problems.push(error(
                    &format!("files.{id}.source"),
                    format!(
                        "{} is not UTF-8 text, which a template must be",
                        name.display()
                    ),
                ));
                continue;
            };
            match template::render(name, text, &resolved.variables, facts) {
                Ok(rendered) => copy.bytes = rendered.into_bytes(),
                Err(failure) => {
                    problems.push(error(&format!("files.{id}"), failure.to_string()));
                    continue;
                }
            }
        }
        produced.desired.push(Desired {
            item,
            target: Target::Path { path },
            content: Content::File(copy),
        });
    }
    produced
}
