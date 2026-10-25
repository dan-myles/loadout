//! MCP entries: one server key inside an agent's shared config, edited through `mcp` and owned
//! per key (architecture section 5). State owns the transaction; `mcp` never calls back.

use std::path::Path;

use serde_json::Value;

use super::disk::{self, Mode, Planned, Put};
use super::{Error, clean_absolute, hash, label, within_roots};
use crate::context::Context;
use crate::manifest::{McpFormat, McpShape};
use crate::mcp;
use crate::plan::{Observed, Target};
use crate::reserved::{self, Merge, Use};

/// A newly created shared config may later receive agent-written state (Claude's
/// `oauthAccount`, Codex `projects`) that loadout itself never writes, so it starts private.
const NEW_DOCUMENT_MODE: u32 = 0o600;

/// The fields of a `Target::Entry`.
pub(super) struct Place<'a> {
    pub(super) target: &'a Target,
    pub(super) file: &'a Path,
    pub(super) format: McpFormat,
    pub(super) pointer: &'a str,
    pub(super) key: &'a str,
    pub(super) shape: McpShape,
}

pub(super) fn place(target: &Target) -> Result<Place<'_>, Error> {
    match target {
        Target::Entry {
            file,
            format,
            pointer,
            key,
            shape,
        } => Ok(Place {
            target,
            file,
            format: *format,
            pointer,
            key,
            shape: *shape,
        }),
        Target::Path { .. } | Target::Program { .. } | Target::None => Err(Error::InvalidContent {
            message: format!("{} is not an MCP entry", label(target)),
        }),
    }
}

pub(super) fn mcp_error(target: &Target, source: mcp::Error) -> Error {
    Error::Mcp {
        target: label(target),
        source,
    }
}

/// Checks the document against the target roots and the reserved MCP map rule
/// (`Use::KeyMerge` with this exact map, format, and shape), before anything reads it.
pub(super) fn check(context: &Context, loadout: &Path, place: &Place<'_>) -> Result<(), Error> {
    if !clean_absolute(place.file) || !within_roots(context, place.file) {
        return Err(Error::OutsideRoots {
            path: place.file.to_path_buf(),
        });
    }
    let how = Use::KeyMerge(Merge {
        map: place.pointer,
        format: place.format.id(),
        shape: place.shape.id(),
    });
    match reserved::check_path(context, loadout, place.file, how) {
        Some(conflict) => Err(Error::Reserved {
            path: place.file.to_path_buf(),
            message: conflict.message(),
        }),
        None => Ok(()),
    }
}

/// One read of a document: its exact bytes, its mode when it exists, and the owned key's value.
pub(super) struct Document {
    pub(super) bytes: Vec<u8>,
    pub(super) mode: Option<u32>,
    pub(super) value: Option<Value>,
}

pub(super) fn read(place: &Place<'_>, real: &Path) -> Result<Document, Error> {
    let (bytes, mode) = match disk::read_document(real)? {
        Some(node) => (node.bytes, Some(node.mode)),
        None => (Vec::new(), None),
    };
    let value = value_in(place, &bytes)?;
    Ok(Document { bytes, mode, value })
}

/// The owned key's value in a document's bytes (empty bytes are an absent document).
pub(super) fn value_in(place: &Place<'_>, bytes: &[u8]) -> Result<Option<Value>, Error> {
    let servers = mcp::parse(bytes, place.format, place.pointer)
        .map_err(|source| mcp_error(place.target, source))?;
    Ok(servers.get(place.key).cloned())
}

pub(super) fn observed(value: Option<&Value>) -> Observed {
    match value {
        None => Observed::Absent,
        Some(value) => Observed::Present {
            sha256: hash::entry(value),
        },
    }
}

/// What the key holds now. Read-only: no lock is taken, so plan and status never create Grok's
/// lock file.
pub(super) fn observe(
    context: &Context,
    loadout: &Path,
    target: &Target,
) -> Result<Observed, Error> {
    let place = place(target)?;
    check(context, loadout, &place)?;
    let real = disk::real(place.file)?;
    Ok(observed(read(&place, &real)?.value.as_ref()))
}

/// Whether the key now holds `expected`, for journaling a step that failed after it landed.
pub(super) fn holds(place: &Place<'_>, real: &Path, expected: Option<&Observed>) -> bool {
    read(place, real).is_ok_and(|document| Some(&observed(document.value.as_ref())) == expected)
}

pub(super) enum Edit<'a> {
    Set(&'a Value),
    Remove,
}

/// The document's new bytes with only the owned key set or removed; everything else, including
/// comments and formatting, is kept by `mcp::edit`.
pub(super) fn edit(place: &Place<'_>, bytes: &[u8], edit: &Edit<'_>) -> Result<Vec<u8>, Error> {
    let mut set = mcp::Servers::new();
    let mut remove = Vec::new();
    match edit {
        Edit::Set(value) => {
            set.insert(place.key.to_owned(), (*value).clone());
        }
        Edit::Remove => remove.push(place.key.to_owned()),
    }
    mcp::edit(
        bytes,
        place.format,
        place.pointer,
        place.shape,
        &set,
        &remove,
    )
    .map_err(|source| mcp_error(place.target, source))
}

/// Replaces the document with `bytes` through the staged, fsynced, atomic `disk::put`. Just
/// before the rename the document is read again and must still be exactly `base`'s bytes (or
/// still absent), so any write by an agent or the user since the read aborts with `Changed`.
/// An existing document keeps its exact mode; a new one is created private.
pub(super) fn put(
    real: &Path,
    base: &Document,
    bytes: Vec<u8>,
    tag: &str,
    target: &Target,
) -> Result<(), Error> {
    let planned = Planned {
        bytes,
        mode: Mode::Exact(base.mode.unwrap_or(NEW_DOCUMENT_MODE)),
    };
    disk::put(real, &Put::File(planned), tag, || {
        let same = match (disk::read_document(real)?, base.mode) {
            (Some(now), Some(_)) => now.bytes == base.bytes,
            (None, None) => true,
            _ => false,
        };
        if same {
            Ok(())
        } else {
            Err(Error::Changed {
                target: label(target),
            })
        }
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    fn target(file: PathBuf) -> Target {
        Target::Entry {
            file,
            format: McpFormat::Json,
            pointer: "/mcpServers".to_owned(),
            key: "github".to_owned(),
            shape: McpShape::CursorJson,
        }
    }

    #[test]
    fn a_document_changed_between_read_and_rename_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        fs::write(&file, "{\"mcpServers\":{}}\n").unwrap();
        let target = target(file.clone());
        let place = place(&target).unwrap();
        let base = read(&place, &file).unwrap();
        let bytes = edit(
            &place,
            &base.bytes,
            &Edit::Set(&serde_json::json!({"command": "x"})),
        )
        .unwrap();
        fs::write(&file, "{\"mcpServers\":{},\"user\":true}\n").unwrap();
        let error = put(&file, &base, bytes, "t", &target).unwrap_err();
        assert!(matches!(error, Error::Changed { .. }), "{error}");
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "{\"mcpServers\":{},\"user\":true}\n"
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "the staged copy is removed");
    }

    #[test]
    fn a_document_created_meanwhile_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mcp.json");
        let target = target(file.clone());
        let place = place(&target).unwrap();
        let base = read(&place, &file).unwrap();
        assert!(base.mode.is_none());
        let bytes = edit(
            &place,
            &base.bytes,
            &Edit::Set(&serde_json::json!({"command": "x"})),
        )
        .unwrap();
        fs::write(&file, "{}").unwrap();
        assert!(matches!(
            put(&file, &base, bytes, "t", &target),
            Err(Error::Changed { .. })
        ));
        assert_eq!(fs::read_to_string(&file).unwrap(), "{}");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_document_is_private_and_an_existing_one_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("new.json");
        let target_new = target(fresh.clone());
        let place_new = place(&target_new).unwrap();
        let base = read(&place_new, &fresh).unwrap();
        let value = serde_json::json!({"command": "x"});
        let bytes = edit(&place_new, &base.bytes, &Edit::Set(&value)).unwrap();
        put(&fresh, &base, bytes, "t", &target_new).unwrap();
        assert_eq!(mode(&fresh), 0o600);

        let shared = dir.path().join("shared.json");
        fs::write(&shared, "{}").unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o644)).unwrap();
        let target_old = target(shared.clone());
        let place_old = place(&target_old).unwrap();
        let base = read(&place_old, &shared).unwrap();
        let bytes = edit(&place_old, &base.bytes, &Edit::Set(&value)).unwrap();
        put(&shared, &base, bytes, "t", &target_old).unwrap();
        assert_eq!(mode(&shared), 0o644);
    }
}
