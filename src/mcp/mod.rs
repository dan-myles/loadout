mod json;
mod lock;
mod pointer;
mod shape;
mod toml_file;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::manifest::{EnvSyntax, McpFormat, McpShape};
use crate::reserved::{self, Merge, Use};
use crate::resolve::McpServer;

pub use lock::{Lock, LockId, acquire, lock, lock_id};

pub type Servers = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Unsupported {
    #[error(
        "{field} references an environment variable, but this agent has no documented env syntax"
    )]
    NoEnvSyntax { field: String },
    #[error(
        "Codex passes env.{name} only as the variable of the same name (env_vars); it cannot take {var}{}",
        prefix.as_ref().map(|prefix| format!(" with prefix {prefix:?}")).unwrap_or_default()
    )]
    CodexEnv {
        name: String,
        var: String,
        prefix: Option<String>,
    },
    #[error(
        "Codex reads a header from the environment only as Authorization: Bearer (bearer_token_env_var); it cannot send headers.{name} from {var}"
    )]
    CodexHeader { name: String, var: String },
    #[error("codex-env-vars works only with shape codex-toml, not {}", shape.id())]
    CodexSyntax { shape: McpShape },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{pointer:?} is not a pointer such as \"/mcpServers\" (RFC 6901, non-empty parts)")]
    Pointer { pointer: String },
    #[error("shape {} is written in {}, not {}", shape.id(), if shape.is_toml() { "toml" } else { "json or jsonc" }, format.id())]
    ShapeFormat { shape: McpShape, format: McpFormat },
    #[error("server {name:?} is both set and removed")]
    SetAndRemove { name: String },
    #[error("the file is not UTF-8")]
    NotUtf8,
    #[error("cannot parse the {} file: {message}", format.id())]
    Parse { format: McpFormat, message: String },
    #[error("{} holds {found}, not a table of servers", if pointer.is_empty() { "the document" } else { pointer })]
    NotAnObject {
        pointer: String,
        found: &'static str,
    },
    #[error("{} has the key {key:?} more than once", if pointer.is_empty() { "the document" } else { pointer })]
    Duplicate { pointer: String, key: String },
    #[error("{pointer}: {message}")]
    Value { pointer: String, message: String },
    #[error("the TOML file mixes CRLF and LF line endings; loadout won't rewrite it")]
    MixedNewlines,
    #[error("schema_version is {found}; loadout edits Muse settings only at schema_version 1")]
    Schema { found: String },
    #[error("Pi treats - and _ as the same character, so server {name:?} collides with {other:?}")]
    Collision { name: String, other: String },
    #[error("{}: {message}", path.display())]
    Reserved { path: PathBuf, message: String },
    #[error("{} is not a regular file", path.display())]
    NotAFile { path: PathBuf },
    #[error("cannot read {}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("cannot take the lock {}: {message}", path.display())]
    Lock { path: PathBuf, message: String },
}

pub fn entry(
    server: &McpServer,
    shape: McpShape,
    env: EnvSyntax,
) -> Result<serde_json::Value, Unsupported> {
    shape::entry(server, shape, env)
}

const BOM: &str = "\u{feff}";

fn text(bytes: &[u8]) -> Result<(&str, &str), Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::NotUtf8)?;
    Ok(match text.strip_prefix(BOM) {
        Some(rest) => (BOM, rest),
        None => ("", text),
    })
}

pub fn parse(bytes: &[u8], format: McpFormat, pointer: &str) -> Result<Servers, Error> {
    let parts = pointer::parse(pointer)?;
    let (_, text) = text(bytes)?;
    match format {
        McpFormat::Json | McpFormat::Jsonc => json::parse(text, format, &parts),
        McpFormat::Toml => toml_file::parse(text, &parts),
    }
}

pub fn edit(
    bytes: &[u8],
    format: McpFormat,
    pointer: &str,
    shape: McpShape,
    set: &Servers,
    remove: &[String],
) -> Result<Vec<u8>, Error> {
    let parts = pointer::parse(pointer)?;
    if shape.is_toml() != (format == McpFormat::Toml) {
        return Err(Error::ShapeFormat { shape, format });
    }
    if let Some(name) = remove.iter().find(|name| set.contains_key(*name)) {
        return Err(Error::SetAndRemove { name: name.clone() });
    }
    let existing = parse(bytes, format, pointer)?;
    let (bom, text) = text(bytes)?;
    if shape == McpShape::MuseJson {
        json::check_schema_version(text, format)?;
    }
    if shape == McpShape::PiJson {
        pi_collision(&existing, set, remove)?;
    }
    if set.is_empty() && !remove.iter().any(|name| existing.contains_key(name)) {
        return Ok(bytes.to_vec());
    }
    let edited = match format {
        McpFormat::Json | McpFormat::Jsonc => json::edit(
            text,
            format,
            &parts,
            &json::Change {
                set,
                remove,
                schema_version: shape == McpShape::MuseJson,
            },
        )?,
        McpFormat::Toml => toml_file::edit(text, &parts, set, remove)?,
    };
    Ok([bom, &edited].concat().into_bytes())
}

fn pi_collision(existing: &Servers, set: &Servers, remove: &[String]) -> Result<(), Error> {
    let canonical = |name: &str| name.replace('_', "-");
    let kept: BTreeSet<&String> = existing
        .keys()
        .filter(|name| !remove.contains(name))
        .chain(set.keys())
        .collect();
    for name in set.keys() {
        if let Some(other) = kept
            .iter()
            .find(|other| **other != name && canonical(other) == canonical(name))
        {
            return Err(Error::Collision {
                name: name.clone(),
                other: (*other).clone(),
            });
        }
    }
    Ok(())
}

pub fn read(
    context: &Context,
    loadout: &Path,
    file: &Path,
    format: McpFormat,
    pointer: &str,
    shape: McpShape,
) -> Result<Servers, Error> {
    pointer::parse(pointer)?;
    let how = Use::KeyMerge(Merge {
        map: pointer,
        format: format.id(),
        shape: shape.id(),
    });
    if let Some(conflict) = reserved::check_path(context, loadout, file, how) {
        return Err(Error::Reserved {
            path: file.to_path_buf(),
            message: conflict.message(),
        });
    }
    let io = |source| Error::Io {
        path: file.to_path_buf(),
        source,
    };
    match fs::metadata(file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Servers::new()),
        Err(error) => return Err(io(error)),
        Ok(metadata) if !metadata.is_file() => {
            return Err(Error::NotAFile {
                path: file.to_path_buf(),
            });
        }
        Ok(_) => {}
    }
    let bytes = fs::read(file).map_err(io)?;
    parse(&bytes, format, pointer)
}
