use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::diagnostic::{Diagnostic, Diagnostics, Severity};
use crate::manifest::McpShape;
use crate::platform::Os;
use crate::reserved;

pub(super) struct Write {
    pub(super) owner: String,
    pub(super) key: String,
    pub(super) path: PathBuf,
    pub(super) entry: Option<Entry>,
    /// A user file item. Two file items are U1's and U4's to compare, not this module's.
    pub(super) file_item: bool,
}

pub(super) struct Entry {
    pub(super) pointer: String,
    pub(super) key: String,
    pub(super) shape: McpShape,
}

struct Forms {
    lexical: Vec<String>,
    real: Option<Vec<String>>,
    address: Option<Vec<String>>,
}

/// Two writes collide when one path is the other or sits inside it, compared both as written
/// and with links resolved, so an alias of a folder can't hide a second writer. Case folds by
/// the OS default, as resolve does; U4 probes the real volume. Two MCP entries in one file
/// collide only when one key's address is the other's or contains it.
pub(super) fn check(os: Os, writes: &[Write]) -> Diagnostics {
    let forms: Vec<Forms> = writes
        .iter()
        .map(|write| Forms {
            lexical: parts(os, &write.path),
            real: reserved::normalize(&write.path)
                .ok()
                .map(|real| parts(os, &real)),
            address: write.entry.as_ref().map(address),
        })
        .collect();
    let mut diagnostics = Diagnostics::default();
    for (index, (later, later_forms)) in writes.iter().zip(&forms).enumerate() {
        for (earlier, earlier_forms) in writes[..index].iter().zip(&forms[..index]) {
            if later.file_item && earlier.file_item {
                continue;
            }
            let Some(how) = collision(later_forms, earlier_forms) else {
                continue;
            };
            diagnostics.push(Diagnostic {
                severity: Severity::Error,
                file: None,
                key: Some(later.key.clone()),
                message: format!(
                    "{} ({}) {how} {} ({}); narrow the item's agents, or point one of them elsewhere",
                    later.owner,
                    later.path.display(),
                    earlier.owner,
                    earlier.path.display(),
                ),
            });
        }
    }
    diagnostics
}

fn collision(one: &Forms, other: &Forms) -> Option<&'static str> {
    let pairs = [
        Some((&one.lexical, &other.lexical)),
        one.real.as_ref().zip(other.real.as_ref()),
    ];
    let mut found = None;
    for (a, b) in pairs.into_iter().flatten() {
        let how = match (&one.address, &other.address) {
            (Some(x), Some(y)) if a == b => nested(x, y).then_some("sets the same MCP key as"),
            _ if a == b => Some("writes the same path as"),
            _ if inside(a, b) || inside(b, a) => {
                Some("nests a file and a folder at one path with")
            }
            _ => None,
        };
        found = found.or(how);
    }
    found
}

fn inside(inner: &[String], outer: &[String]) -> bool {
    inner.len() > outer.len() && inner.starts_with(outer)
}

fn nested(one: &[String], other: &[String]) -> bool {
    one.starts_with(other) || other.starts_with(one)
}

/// Pi treats `-` and `_` in server names as the same character.
fn address(entry: &Entry) -> Vec<String> {
    let key = if entry.shape == McpShape::PiJson {
        entry.key.replace('_', "-")
    } else {
        entry.key.clone()
    };
    entry
        .pointer
        .split('/')
        .skip(1)
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .chain([key])
        .collect()
}

fn parts(os: Os, path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|part| match part {
            Component::Prefix(prefix) => Some(prefix_form(prefix.as_os_str())),
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            Component::RootDir | Component::CurDir | Component::ParentDir => None,
        })
        .map(|part| match os {
            Os::Linux => part,
            Os::Macos | Os::Windows => part.to_lowercase(),
        })
        .collect()
}

/// `canonicalize` on Windows returns `\\?\C:` where a written path has `C:`.
fn prefix_form(prefix: &OsStr) -> String {
    let text = prefix.to_string_lossy();
    match text.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned(),
    }
}
