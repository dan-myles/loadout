use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use super::Error;
use super::hash::{self, Blob};
use crate::context::Context;

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

/// Content to put at a target. Tree keys are plain `/`-separated relative paths (`tree_key`);
/// `dirs` holds every folder, `""` for the root, with its exact mode or `None` for the default.
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

fn already_exists(error: &Error) -> bool {
    matches!(error, Error::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists)
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

/// True when `key` is already in the form `tree_key` produces, so joining it under a root can
/// never leave that root: no `..`, `.`, root, drive, or Windows separator.
pub(super) fn plain_key(key: &str) -> bool {
    tree_key(Path::new(key)).is_ok_and(|normal| normal == key)
}

fn join_key(root: &Path, key: &str) -> Result<PathBuf, Error> {
    if key.is_empty() {
        return Ok(root.to_path_buf());
    }
    if !plain_key(key) {
        return Err(Error::InvalidContent {
            message: format!("{key:?} is not a plain relative path"),
        });
    }
    Ok(key
        .split('/')
        .fold(root.to_path_buf(), |path, part| path.join(part)))
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

/// Deletes a scratch path this process just created, or one rollback has verified.
pub(super) fn remove_scratch(path: &Path) -> Result<(), Error> {
    let result = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => Err(error),
    };
    result.map_err(io("remove", path))
}

/// A scratch name that is already taken belongs to something loadout can't vouch for (a crashed
/// run that rollback hasn't verified, or a planted file), so it is never deleted or overwritten.
fn vacant(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io("inspect", path)(error)),
        Ok(_) => Err(Error::Invalid {
            path: path.to_path_buf(),
            message: "already exists; loadout won't overwrite or delete what it can't verify"
                .to_owned(),
        }),
    }
}

/// Writes `content` at `real`, replacing whatever is there, so that each step is atomic: the
/// content is staged privately in a scratch sibling and fsynced, gets its final modes, `verify`
/// runs last, and a rename puts it in place. A file over a file is one rename. Anything
/// involving a folder moves the old one aside first and moves it back if the second rename fails.
pub(super) fn put(
    real: &Path,
    content: &Put,
    tag: &str,
    verify: impl FnOnce() -> Result<(), Error>,
) -> Result<(), Error> {
    let parent = real.parent().ok_or_else(|| Error::InvalidContent {
        message: format!("{} has no parent folder", real.display()),
    })?;
    let missing = missing_dirs(parent);
    fs::create_dir_all(parent).map_err(io("create", parent))?;
    for dir in &missing {
        sync_parent(dir)?;
    }
    let temp = scratch(real, tag, "tmp")?;
    vacant(&temp)?;
    let built = match content {
        Put::File(planned) => write_file(&temp, planned),
        Put::Tree { dirs, files } => write_tree(&temp, dirs, files),
    };
    if let Err(error) = built {
        if !already_exists(&error) {
            remove_scratch(&temp)?;
        }
        return Err(error);
    }
    if let Err(error) = verify().and_then(|()| swap(real, &temp, content, tag)) {
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
    if existing.is_none() {
        return fs::rename(temp, real).map_err(io("replace", real));
    }
    let old = scratch(real, tag, "old")?;
    vacant(&old)?;
    fs::rename(real, &old).map_err(io("move aside", real))?;
    sync_parent(real)?;
    if let Err(error) = fs::rename(temp, real) {
        fs::rename(&old, real).map_err(io("move back", real))?;
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
        vacant(&old)?;
        fs::rename(real, &old).map_err(io("move aside", real))?;
        sync_parent(real)?;
        remove_scratch(&old)?;
    } else {
        fs::remove_file(real).map_err(io("remove", real))?;
    }
    sync_parent(real)
}

/// Creates the file that will hold staged bytes with only owner bits from its first open, so
/// nobody else can open it and keep a descriptor that would still read the bytes after any later
/// chmod. A umask can only narrow `0o600`, and the chmod puts owner read and write back.
fn create_private_file(path: &Path) -> Result<File, Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path).map_err(io("create", path))?;
    #[cfg(unix)]
    chmod_file(&file, path, 0o600)?;
    Ok(file)
}

fn needs_default(mode: Mode) -> bool {
    matches!(mode, Mode::Executable { base: None, .. })
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The mode the kernel gives a new file in `probe`'s folder (the umask, and any default ACL),
/// which `Mode::Executable { base: None }` starts from. It comes from an empty probe that is
/// created and removed here and never holds data. Windows has no modes.
fn probe_file_mode(probe: &Path) -> Result<u32, Error> {
    if !cfg!(unix) {
        return Ok(0);
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o666);
    let file = options.open(probe).map_err(io("create", probe))?;
    let mode = file.metadata().map(|metadata| mode_of(&metadata));
    drop(file);
    fs::remove_file(probe).map_err(io("remove", probe))?;
    mode.map_err(io("inspect", probe))
}

/// The mode the kernel gives a new folder there, from an empty probe folder, as above.
fn probe_dir_mode(probe: &Path) -> Result<u32, Error> {
    if !cfg!(unix) {
        return Ok(0);
    }
    fs::create_dir(probe).map_err(io("create", probe))?;
    let mode = fs::symlink_metadata(probe).map(|metadata| mode_of(&metadata));
    fs::remove_dir(probe).map_err(io("remove", probe))?;
    mode.map_err(io("inspect", probe))
}

/// Writes the bytes into a private new file and returns it with the final mode to apply.
/// `default` is the new-file mode from `probe_file_mode`, used when the mode has no base.
fn stage_file(path: &Path, planned: &Planned, default: u32) -> Result<(File, u32), Error> {
    let mut file = create_private_file(path)?;
    file.write_all(&planned.bytes).map_err(io("write", path))?;
    file.sync_all().map_err(io("sync", path))?;
    let bits = match planned.mode {
        Mode::Exact(bits) => bits & 0o777,
        Mode::Executable { base, executable } => x_rule(base.unwrap_or(default), executable),
    };
    Ok((file, bits))
}

fn write_file(path: &Path, planned: &Planned) -> Result<(), Error> {
    let default = if needs_default(planned.mode) {
        probe_file_mode(&suffixed(path, ".mode"))?
    } else {
        0
    };
    write_staged(path, planned, default)
}

fn write_staged(path: &Path, planned: &Planned, default: u32) -> Result<(), Error> {
    let (file, bits) = stage_file(path, planned, default)?;
    #[cfg(unix)]
    chmod_file(&file, path, bits)?;
    #[cfg(not(unix))]
    let _ = bits;
    file.sync_all().map_err(io("sync", path))
}

#[cfg(unix)]
fn chmod_file(file: &File, path: &Path, bits: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(bits))
        .map_err(io("set the mode of", path))
}

/// Builds a tree under `root` with every folder `0o700` from its creation while files are
/// written, so nothing in it is reachable by others before it is complete. Default modes come
/// from empty probes beside the root. Files get their final modes as they are written. Returns
/// the folders' final modes, deepest first, for `finish_tree`.
fn stage_tree(
    root: &Path,
    dirs: &BTreeMap<String, Option<u32>>,
    files: &BTreeMap<String, Planned>,
) -> Result<Vec<(PathBuf, u32)>, Error> {
    let dir_default = if dirs.values().any(Option::is_none) {
        probe_dir_mode(&suffixed(root, ".dmode"))?
    } else {
        0
    };
    let file_default = if files.values().any(|planned| needs_default(planned.mode)) {
        probe_file_mode(&suffixed(root, ".fmode"))?
    } else {
        0
    };
    let mut modes = Vec::new();
    for (key, mode) in dirs {
        create_private_dir(&join_key(root, key)?)?;
        modes.push((
            join_key(root, key)?,
            mode.map_or(dir_default, |mode| mode & 0o777),
        ));
    }
    for (key, planned) in files {
        write_staged(&join_key(root, key)?, planned, file_default)?;
    }
    modes.reverse();
    Ok(modes)
}

fn write_tree(
    root: &Path,
    dirs: &BTreeMap<String, Option<u32>>,
    files: &BTreeMap<String, Planned>,
) -> Result<(), Error> {
    check_tree(dirs, files)?;
    let modes = stage_tree(root, dirs, files)?;
    finish_tree(&modes)
}

/// Checks a tree's shape before anything is created: plain keys only, a root, every parent
/// folder listed, and no path that is both a file and a folder.
pub(super) fn check_tree<D, F>(
    dirs: &BTreeMap<String, D>,
    files: &BTreeMap<String, F>,
) -> Result<(), Error> {
    let parent = |key: &str| key.rsplit_once('/').map_or("", |(head, _)| head).to_owned();
    let problem = if !dirs.contains_key("") {
        Some("has no root folder".to_owned())
    } else if let Some(key) = dirs
        .keys()
        .filter(|key| !key.is_empty())
        .chain(files.keys())
        .find(|key| !plain_key(key))
    {
        Some(format!("{key:?} is not a plain relative path"))
    } else if let Some(key) = files.keys().find(|key| dirs.contains_key(*key)) {
        Some(format!("{key} is both a file and a folder"))
    } else {
        dirs.keys()
            .filter(|key| !key.is_empty())
            .chain(files.keys())
            .find(|key| !dirs.contains_key(&parent(key)))
            .map(|key| format!("{key} has no listed parent folder"))
    };
    match problem {
        Some(problem) => Err(Error::InvalidContent {
            message: format!("a tree {problem}"),
        }),
        None => Ok(()),
    }
}

/// Applies the folders' final modes, deepest first, then makes their entries durable.
fn finish_tree(modes: &[(PathBuf, u32)]) -> Result<(), Error> {
    for (dir, mode) in modes {
        #[cfg(unix)]
        chmod_dir(dir, *mode)?;
        #[cfg(not(unix))]
        let _ = mode;
        sync_dir(dir)?;
    }
    Ok(())
}

/// Creates a folder with only owner bits from its creation; a umask can only narrow `0o700`,
/// and the chmod puts the owner bits back.
fn create_private_dir(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    let created =
        std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700).create(path);
    #[cfg(not(unix))]
    let created = fs::create_dir(path);
    created.map_err(io("create", path))?;
    #[cfg(unix)]
    chmod_dir(path, 0o700)?;
    Ok(())
}

#[cfg(unix)]
fn chmod_dir(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
        .map_err(io("set the mode of", path))
}

/// Makes the entries of `dir` durable. Windows has no folder fsync; its renames are the
/// durability boundary there.
fn sync_dir(dir: &Path) -> Result<(), Error> {
    if cfg!(unix) {
        File::open(dir)
            .and_then(|dir| dir.sync_all())
            .map_err(io("sync", dir))?;
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<(), Error> {
    match path.parent() {
        Some(parent) => sync_dir(parent),
        None => Ok(()),
    }
}

/// Writes one of loadout's own files (state, journal) atomically, readable only by the user.
/// Its folder must already have passed `internal_dir`.
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

/// The canonical state dir, created `0o700` if missing. The configured path may itself be a
/// link; everything loadout writes below it is checked against this canonical form.
pub(super) fn state_root(context: &Context) -> Result<PathBuf, Error> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&context.state)
        .map_err(io("create", &context.state))?;
    let root = context
        .state
        .canonicalize()
        .map_err(io("resolve", &context.state))?;
    if !fs::metadata(&root).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(Error::Invalid {
            path: context.state.clone(),
            message: "must be a folder".to_owned(),
        });
    }
    Ok(root)
}

/// A folder of loadout's own below the canonical state dir. Each part must be a real folder,
/// never a link, junction, or special file, so no internal write can be redirected outside the
/// state dir. With `create`, missing parts are made `0o700` and their entries fsynced.
pub(super) fn internal_dir(root: &Path, parts: &[&str], create: bool) -> Result<PathBuf, Error> {
    let mut path = root.to_path_buf();
    for part in parts {
        if !plain_key(part) || part.contains('/') {
            return Err(Error::Invalid {
                path: path.join(part),
                message: "is not a plain folder name".to_owned(),
            });
        }
        path.push(part);
        let metadata = match fs::symlink_metadata(&path) {
            Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                create_private_dir(&path)?;
                sync_parent(&path)?;
                fs::symlink_metadata(&path).map_err(io("inspect", &path))?
            }
            Err(error) => return Err(io("inspect", &path)(error)),
            Ok(metadata) => metadata,
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::Invalid {
                path,
                message: "must be a real folder inside the state dir, not a link or a file"
                    .to_owned(),
            });
        }
    }
    let canonical = path.canonicalize().map_err(io("resolve", &path))?;
    if canonical != path {
        return Err(Error::Invalid {
            path,
            message: format!("resolves to {}, outside the state dir", canonical.display()),
        });
    }
    Ok(path)
}

/// Reads one of loadout's own files, only after checking that it is a regular file (not a link)
/// that resolves inside `within`, so nothing planted in the state dir can make loadout read
/// anything else.
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
        for bad in ["../../.codex", "a//b", "a/", "/abs", "a/./b"] {
            assert!(!plain_key(bad), "{bad:?}");
        }
        for drive in ["C:x", "C:/x", "a\\b"] {
            assert_eq!(plain_key(drive), !cfg!(windows), "{drive:?}");
        }
        assert!(plain_key("bin/run.sh"));
    }

    #[test]
    fn a_tree_key_that_climbs_out_is_refused_before_anything_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("stage");
        let dirs = BTreeMap::from([
            (String::new(), None),
            ("../../escape".to_owned(), Some(0o777)),
        ]);
        let error = write_tree(&root, &dirs, &BTreeMap::new()).unwrap_err();
        assert!(matches!(error, Error::InvalidContent { .. }), "{error}");
        assert!(!root.exists(), "nothing was created");
        assert!(!dir.path().join("escape").exists());
        assert!(!dir.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn put_never_reuses_a_scratch_name_that_is_already_taken() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("target");
        let temp = scratch(&real, "t", "tmp").unwrap();
        fs::write(&temp, "someone else's").unwrap();
        let content = Put::File(Planned {
            bytes: b"new".to_vec(),
            mode: Mode::Executable {
                base: None,
                executable: false,
            },
        });
        assert!(put(&real, &content, "t", || Ok(())).is_err());
        assert_eq!(fs::read_to_string(&temp).unwrap(), "someone else's");
        assert!(!real.exists());
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    #[test]
    fn staged_bytes_are_private_until_the_final_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".scratch");
        let planned = Planned {
            bytes: b"secret-ish".to_vec(),
            mode: Mode::Exact(0o644),
        };
        let (file, bits) = stage_file(&path, &planned, 0o644).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"secret-ish",
            "the bytes are on disk"
        );
        assert_eq!(mode(&path), 0o600, "while the bytes are on disk");
        assert_eq!(bits, 0o644);
        chmod_file(&file, &path, bits).unwrap();
        assert_eq!(mode(&path), 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn the_data_file_is_owner_only_from_its_first_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".scratch");
        let file = create_private_file(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0, "before any byte");
        assert_eq!(mode(&path), 0o600, "already private when it first exists");
        drop(file);
    }

    #[cfg(unix)]
    #[test]
    fn default_modes_come_from_empty_probes_that_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("control");
        fs::write(&control, "").unwrap();
        let control_dir = dir.path().join("control-dir");
        fs::create_dir(&control_dir).unwrap();
        let probe = dir.path().join("probe");
        assert_eq!(probe_file_mode(&probe).unwrap(), mode(&control));
        assert!(!probe.exists());
        assert_eq!(probe_dir_mode(&probe).unwrap(), mode(&control_dir));
        assert!(!probe.exists());
        fs::write(&probe, "taken").unwrap();
        assert!(
            probe_file_mode(&probe).is_err(),
            "never reuses a taken name"
        );
        assert_eq!(fs::read_to_string(&probe).unwrap(), "taken");
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_without_a_base_still_gets_the_default_mode_rule() {
        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("control");
        fs::write(&control, "").unwrap();
        let path = dir.path().join("new");
        let planned = Planned {
            bytes: b"#!/bin/sh\n".to_vec(),
            mode: Mode::Executable {
                base: None,
                executable: true,
            },
        };
        write_file(&path, &planned).unwrap();
        assert_eq!(mode(&path), x_rule(mode(&control), true));
        assert!(!suffixed(&path, ".mode").exists());
    }

    #[cfg(unix)]
    #[test]
    fn staged_folders_are_private_until_finish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".stage");
        let dirs = BTreeMap::from([(String::new(), Some(0o755)), ("bin".to_owned(), None)]);
        let files = BTreeMap::from([(
            "bin/run.sh".to_owned(),
            Planned {
                bytes: b"#!/bin/sh\n".to_vec(),
                mode: Mode::Exact(0o755),
            },
        )]);
        let modes = stage_tree(&root, &dirs, &files).unwrap();
        assert_eq!(fs::read(root.join("bin/run.sh")).unwrap(), b"#!/bin/sh\n");
        assert_eq!(mode(&root), 0o700, "root while files are written");
        assert_eq!(
            mode(&root.join("bin")),
            0o700,
            "subfolder while files are written"
        );
        finish_tree(&modes).unwrap();
        let probe = dir.path().join("probe");
        fs::create_dir(&probe).unwrap();
        assert_eq!(mode(&root), 0o755);
        assert_eq!(mode(&root.join("bin/run.sh")), 0o755);
        assert_eq!(
            mode(&root.join("bin")),
            mode(&probe),
            "the process default folder mode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn internal_folders_refuse_links_out_of_the_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let root = root.canonicalize().unwrap();
        std::os::unix::fs::symlink(&outside, root.join("runs")).unwrap();
        assert!(internal_dir(&root, &["runs"], true).is_err());
        assert!(internal_dir(&root, &["..", "x"], true).is_err());
        assert!(internal_dir(&root, &["other", "deep"], true).is_ok());
        assert_eq!(mode(&root.join("other")), 0o700);
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }
}
