use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use super::Error;
use super::hash::{self, Blob};

/// What a path target holds on disk. Modes are the Unix permission bits (`0o777`), and always
/// 0 on Windows, where loadout ignores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Disk {
    File(Node),
    Tree {
        dirs: BTreeMap<String, u32>,
        files: BTreeMap<String, Node>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Node {
    pub(super) bytes: Vec<u8>,
    pub(super) mode: u32,
}

impl Node {
    fn blob(&self) -> Blob<'_> {
        Blob {
            bytes: &self.bytes,
            executable: self.mode & 0o111 != 0,
        }
    }
}

impl Disk {
    pub(super) fn sha256(&self) -> String {
        match self {
            Disk::File(node) => hash::file(&node.blob()),
            Disk::Tree { files, .. } => hash::tree(
                files
                    .iter()
                    .map(|(path, node)| (path.as_str(), node.blob())),
            ),
        }
    }
}

/// The mode a file gets when loadout writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Restore exactly these bits.
    Exact(u32),
    /// Start from `base`, or the process default for a new file, keep its read and write bits,
    /// and give `x` only to classes that can read.
    Executable { base: Option<u32>, executable: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Planned {
    pub(super) bytes: Vec<u8>,
    pub(super) mode: Mode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Put {
    File(Planned),
    Tree {
        dirs: BTreeMap<String, Option<u32>>,
        files: BTreeMap<String, Planned>,
    },
}

pub(super) fn io(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io {
        action,
        path,
        source,
    }
}

/// Applies the mode rule in architecture section 5. Setuid, setgid, and sticky bits are dropped.
pub(super) fn x_rule(mode: u32, executable: bool) -> u32 {
    let read_write = mode & 0o666;
    if executable {
        read_write | ((read_write & 0o444) >> 2)
    } else {
        read_write
    }
}

/// The real file behind a symlinked target, so a write keeps the user's link.
pub(super) fn real(path: &Path) -> Result<PathBuf, Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            crate::reserved::normalize(path).map_err(io("resolve the link", path))
        }
        _ => Ok(path.to_path_buf()),
    }
}

/// Reads a target without ever opening a FIFO, socket, or device. A missing target, or a link
/// to a missing one, is `None`. Links inside a folder are refused rather than followed, because
/// writing the folder back would turn them into copies.
pub(super) fn read(path: &Path) -> Result<Option<Disk>, Error> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io("inspect", path)(error)),
    };
    if metadata.is_file() {
        return Ok(Some(Disk::File(read_file(path)?)));
    }
    if !metadata.is_dir() {
        return Err(Error::Special {
            path: path.to_path_buf(),
        });
    }
    let mut dirs = BTreeMap::from([(String::new(), mode_of(&metadata))]);
    let mut files = BTreeMap::new();
    walk(path, "", &mut dirs, &mut files)?;
    Ok(Some(Disk::Tree { dirs, files }))
}

fn walk(
    dir: &Path,
    prefix: &str,
    dirs: &mut BTreeMap<String, u32>,
    files: &mut BTreeMap<String, Node>,
) -> Result<(), Error> {
    for entry in fs::read_dir(dir).map_err(io("list", dir))? {
        let entry = entry.map_err(io("list", dir))?;
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::NotUtf8 { path: path.clone() })?;
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = fs::symlink_metadata(&path).map_err(io("inspect", &path))?;
        if kind.is_file() {
            files.insert(relative, read_file(&path)?);
        } else if kind.is_dir() {
            dirs.insert(relative.clone(), mode_of(&kind));
            walk(&path, &relative, dirs, files)?;
        } else {
            return Err(Error::Special { path });
        }
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<Node, Error> {
    let mut file = File::open(path).map_err(io("open", path))?;
    let metadata = file.metadata().map_err(io("inspect", path))?;
    if !metadata.is_file() {
        return Err(Error::Special {
            path: path.to_path_buf(),
        });
    }
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut file, &mut bytes).map_err(io("read", path))?;
    Ok(Node {
        bytes,
        mode: mode_of(&metadata),
    })
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_: &fs::Metadata) -> u32 {
    0
}

/// Checks that a relative path from `Content::Tree` is plain, and returns it `/`-separated.
pub(super) fn tree_key(path: &Path) -> Result<String, Error> {
    let parts: Option<Vec<&str>> = path
        .components()
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    match parts {
        Some(parts) if !parts.is_empty() => Ok(parts.join("/")),
        _ => Err(Error::InvalidContent {
            message: format!("{} is not a plain relative path", path.display()),
        }),
    }
}

/// Names of loadout's scratch siblings: `.<name>.loadout-<tag>.tmp` and `.old`.
pub(super) fn scratch(real: &Path, tag: &str, suffix: &str) -> Result<PathBuf, Error> {
    let name = real
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::NotUtf8 {
            path: real.to_path_buf(),
        })?;
    Ok(real.with_file_name(format!(".{name}.loadout-{tag}.{suffix}")))
}

/// Folders between the nearest existing ancestor and `dir`, outermost first.
pub(super) fn missing_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut missing: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|ancestor| fs::symlink_metadata(ancestor).is_err())
        .map(Path::to_path_buf)
        .collect();
    missing.reverse();
    missing
}

pub(super) fn remove_scratch(path: &Path) -> Result<(), Error> {
    let result = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
    };
    result.map_err(io("remove", path))
}

/// Writes `content` at `real`, replacing whatever is there, so that each step is atomic:
/// the content is built in a scratch sibling and fsynced, `verify` runs last, and a rename puts it
/// in place. A file over a file is one rename. Anything involving a folder moves the old one
/// aside first and moves it back if the second rename fails.
pub(super) fn put(
    real: &Path,
    content: &Put,
    tag: &str,
    verify: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    let parent = real.parent().ok_or_else(|| Error::InvalidContent {
        message: format!("{} has no parent folder", real.display()),
    })?;
    fs::create_dir_all(parent).map_err(io("create", parent))?;
    let temp = scratch(real, tag, "tmp")?;
    remove_scratch(&temp)?;
    let built = match content {
        Put::File(planned) => write_file(&temp, planned),
        Put::Tree { dirs, files } => write_tree(&temp, dirs, files),
    };
    if let Err(error) = built.and_then(|()| verify()) {
        remove_scratch(&temp)?;
        return Err(error);
    }
    if let Err(error) = swap(real, &temp, content, tag) {
        remove_scratch(&temp)?;
        return Err(error);
    }
    sync_parent(real)
}

fn swap(real: &Path, temp: &Path, content: &Put, tag: &str) -> Result<(), Error> {
    let existing = fs::symlink_metadata(real).ok();
    let simple =
        matches!(content, Put::File(_)) && existing.as_ref().is_none_or(fs::Metadata::is_file);
    if simple {
        return fs::rename(temp, real).map_err(io("replace", real));
    }
    let old = scratch(real, tag, "old")?;
    remove_scratch(&old)?;
    if existing.is_some() {
        fs::rename(real, &old).map_err(io("move aside", real))?;
    }
    if let Err(error) = fs::rename(temp, real) {
        if existing.is_some() {
            fs::rename(&old, real).map_err(io("move back", real))?;
        }
        return Err(io("replace", real)(error));
    }
    remove_scratch(&old)
}

/// Deletes the target with one rename out of the way, so it never stays half removed.
pub(super) fn remove(real: &Path, tag: &str) -> Result<(), Error> {
    let metadata = match fs::symlink_metadata(real) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io("inspect", real)(error)),
    };
    if metadata.is_dir() {
        let old = scratch(real, tag, "old")?;
        remove_scratch(&old)?;
        fs::rename(real, &old).map_err(io("move aside", real))?;
        remove_scratch(&old)?;
    } else {
        fs::remove_file(real).map_err(io("remove", real))?;
    }
    sync_parent(real)
}

fn write_file(path: &Path, planned: &Planned) -> Result<(), Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o666);
    let mut file = options.open(path).map_err(io("create", path))?;
    file.write_all(&planned.bytes).map_err(io("write", path))?;
    set_mode(&file, path, planned.mode)?;
    file.sync_all().map_err(io("sync", path))
}

fn set_mode(file: &File, path: &Path, mode: Mode) -> Result<(), Error> {
    let bits = match mode {
        Mode::Exact(bits) => bits & 0o777,
        Mode::Executable { base, executable } => {
            let base = match base {
                Some(base) => base,
                None => mode_of(&file.metadata().map_err(io("inspect", path))?),
            };
            x_rule(base, executable)
        }
    };
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(bits))
        .map_err(io("set the mode of", path))?;
    #[cfg(not(unix))]
    let _ = bits;
    Ok(())
}

fn write_tree(
    root: &Path,
    dirs: &BTreeMap<String, Option<u32>>,
    files: &BTreeMap<String, Planned>,
) -> Result<(), Error> {
    let join = |relative: &str| {
        relative
            .split('/')
            .filter(|part| !part.is_empty())
            .fold(root.to_path_buf(), |path, part| path.join(part))
    };
    fs::create_dir(root).map_err(io("create", root))?;
    for relative in dirs.keys().filter(|relative| !relative.is_empty()) {
        let dir = join(relative);
        fs::create_dir_all(&dir).map_err(io("create", &dir))?;
    }
    for (relative, planned) in files {
        let path = join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io("create", parent))?;
        }
        write_file(&path, planned)?;
    }
    #[cfg(unix)]
    for (relative, mode) in dirs.iter().rev() {
        if let Some(mode) = mode {
            set_dir_mode(&join(relative), *mode)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .map_err(io("set the mode of", path))
}

/// Makes a rename in `path`'s folder durable. Windows has no folder fsync; its rename is the
/// boundary there.
fn sync_parent(path: &Path) -> Result<(), Error> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if cfg!(unix) {
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(io("sync", parent))?;
    }
    Ok(())
}

/// Writes one of loadout's own files (state, journal) atomically, readable only by the user.
pub(super) fn save(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let temp = path.with_extension("tmp");
    remove_scratch(&temp)?;
    write_file(
        &temp,
        &Planned {
            bytes: bytes.to_vec(),
            mode: Mode::Exact(0o600),
        },
    )?;
    fs::rename(&temp, path).map_err(io("replace", path))?;
    sync_parent(path)
}

/// Creates one of loadout's own folders, private to the user on Unix.
pub(super) fn private_dir(path: &Path) -> Result<(), Error> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path).map_err(io("create", path))
}

/// Reads one of loadout's own files, only after checking that it resolves to a regular file
/// inside `within`, so a link planted in the state dir can't make loadout read anything else.
pub(super) fn read_internal(path: &Path, within: &Path) -> Result<Option<Vec<u8>>, Error> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io("inspect", path)(error)),
        Ok(_) => {}
    }
    let real = path.canonicalize().map_err(io("resolve", path))?;
    let root = within.canonicalize().map_err(io("resolve", within))?;
    if !real.starts_with(&root) {
        return Err(Error::Invalid {
            path: path.to_path_buf(),
            message: format!(
                "resolves outside {}, which loadout never reads",
                root.display()
            ),
        });
    }
    if !fs::metadata(&real).is_ok_and(|metadata| metadata.is_file()) {
        return Err(Error::Invalid {
            path: path.to_path_buf(),
            message: "must be a regular file".to_owned(),
        });
    }
    Ok(Some(read_file(&real)?.bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_rule_keeps_read_write_bits_and_gives_x_only_where_readable() {
        assert_eq!(x_rule(0o644, true), 0o755);
        assert_eq!(x_rule(0o600, true), 0o700);
        assert_eq!(x_rule(0o755, false), 0o644);
        assert_eq!(x_rule(0o640, true), 0o750);
        assert_eq!(x_rule(0o200, true), 0o200);
        assert_eq!(x_rule(0o4755, true), 0o755);
        assert_eq!(x_rule(0o604, true), 0o705);
    }

    #[test]
    fn tree_keys_must_be_plain_relative_paths() {
        assert_eq!(tree_key(Path::new("a/b.sh")).unwrap(), "a/b.sh");
        for bad in ["", "/etc/passwd", "../x", "a/../b", "./a"] {
            assert!(tree_key(Path::new(bad)).is_err(), "{bad:?}");
        }
    }
}
