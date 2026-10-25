//! Putting verified binaries and their record in place.
//!
//! Every file is first written to a staging name in the install folder that this run created
//! exclusively (`create_new`), so publishing is a same-folder rename and a failure before it
//! leaves the installed files untouched. Only names this run created are ever removed.
//!
//! The binaries are published first and the record last. That is not one atomic transaction:
//! a crash or power loss between two renames can leave new binaries next to the previous record,
//! or no record. The record binds each binary's hash, so that state fails closed: self-update and
//! both install scripts treat the binary as unmanaged and refuse to replace it until the user
//! reinstalls (see `docs/releasing.md`).
//!
//! Writers are serialized by [`lock`]: install.sh, install.ps1 and self-update each hold the
//! install folder's lock from their last check of the installed files until publication and
//! their cleanup are done. A writer that doesn't take it (a hand copy, another tool, or an
//! install script older than the lock) is not serialized, so its change can still race one
//! publication; the hash binding then makes the mismatch fail closed only if the record and the
//! binaries end up disagreeing.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A file this run created under a unique name. Dropping it removes the file unless it was
/// published, so an early return never leaves staging files behind.
#[derive(Debug)]
pub(super) struct Staged {
    path: PathBuf,
    published: bool,
}

impl Staged {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// The install folder's lock file. It stays in place: removing a lock file another writer may
/// already have open would let two writers hold "the" lock at once.
pub(super) const LOCK_FILE: &str = ".loadout.install.lock";

/// The install folder's lock, held until dropped.
#[derive(Debug)]
pub(super) struct Lock {
    _file: File,
}

#[derive(Debug)]
pub(super) enum LockError {
    /// Another install or self-update holds it right now.
    Busy,
    Failed(io::Error),
}

/// Takes the install folder's lock without waiting. On Unix that is `flock(2)` on
/// [`LOCK_FILE`], as install.sh takes it with flock or perl; on Windows it is the file opened
/// unshared, as install.ps1 opens it, so whichever writer is second can't open it at all.
pub(super) fn lock(dir: &Path) -> Result<Lock, LockError> {
    let path = dir.join(LOCK_FILE);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.file_type().is_file()) {
        return Err(LockError::Failed(io::Error::other(format!(
            "{} is a link or not a regular file",
            path.display()
        ))));
    }
    let file = open_lock(&path)?;
    Ok(Lock { _file: file })
}

#[cfg(unix)]
fn open_lock(path: &Path) -> Result<File, LockError> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(LockError::Failed)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(LockError::Busy),
        Err(fs::TryLockError::Error(error)) => Err(LockError::Failed(error)),
    }
}

#[cfg(windows)]
fn open_lock(path: &Path) -> Result<File, LockError> {
    use std::os::windows::fs::OpenOptionsExt;
    /// ERROR_SHARING_VIOLATION: another writer has the file open.
    const SHARING_VIOLATION: i32 = 32;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
        .map_err(|error| {
            if error.raw_os_error() == Some(SHARING_VIOLATION) {
                LockError::Busy
            } else {
                LockError::Failed(error)
            }
        })
}

fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(
        "{:x}{:x}{:x}",
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Writes `bytes` to a new file `.<name>.new.<nonce><suffix>` in `dir`. On Windows `suffix` is
/// `.exe` for a binary, so the staged copy can run for its version check.
pub(super) fn stage(
    dir: &Path,
    name: &str,
    suffix: &str,
    bytes: &[u8],
    executable: bool,
) -> io::Result<Staged> {
    for _ in 0..16 {
        let path = dir.join(format!(".{name}.new.{}{suffix}", nonce()));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let staged = Staged {
            path,
            published: false,
        };
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        set_mode(&staged.path, executable)?;
        return Ok(staged);
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not find a free staging name",
    ))
}

#[cfg(unix)]
fn set_mode(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
    )
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

/// How a publication went wrong, and whether the previous files are back in place.
#[derive(Debug)]
pub(super) struct Failure {
    pub(super) detail: String,
    /// True when every file is as it was before publishing began.
    pub(super) restored: bool,
}

/// Publishes the staged binaries over their destinations, in order, then the record.
///
/// On Unix each binary is one atomic rename over the old file, which a running process keeps
/// using. Windows can't rename over a running executable, but it can rename the running file, so
/// each existing binary is first moved aside to `.<name>.old.<nonce>`, a name this run owns. If a
/// later binary step fails, every completed move is undone in reverse; the record is untouched.
/// Once every binary is in place the record is renamed over the old one. If that last rename
/// fails, the new binaries stay and the old record no longer matches them, which fails closed.
pub(super) fn publish(
    binaries: Vec<(Staged, PathBuf)>,
    record: (Staged, PathBuf),
) -> Result<(), Failure> {
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut published = Vec::new();
    for (mut staged, dest) in binaries {
        if let Err(detail) = place(&staged, &dest, &mut done) {
            let restored = undo(&done);
            return Err(Failure { detail, restored });
        }
        staged.published = true;
        published.push(staged);
    }
    let (mut staged, dest) = record;
    if let Err(error) = fs::rename(&staged.path, &dest) {
        return Err(Failure {
            detail: format!(
                "the new binaries are in place, but the install record {} could not be written ({error}), so self-update will not replace them again until loadout is reinstalled",
                dest.display()
            ),
            restored: false,
        });
    }
    staged.published = true;
    sync_dir(&dest);
    remove_aside(&done);
    Ok(())
}

#[cfg(not(windows))]
fn place(staged: &Staged, dest: &Path, done: &mut Vec<(PathBuf, PathBuf)>) -> Result<(), String> {
    let _ = done;
    fs::rename(&staged.path, dest)
        .map_err(|error| format!("cannot replace {}: {error}", dest.display()))
}

#[cfg(windows)]
fn place(staged: &Staged, dest: &Path, done: &mut Vec<(PathBuf, PathBuf)>) -> Result<(), String> {
    if fs::symlink_metadata(dest).is_ok() {
        let name = dest
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let aside = dest.with_file_name(format!(".{name}.old.{}", nonce()));
        fs::rename(dest, &aside)
            .map_err(|error| format!("cannot move {} aside: {error}", dest.display()))?;
        done.push((dest.to_path_buf(), aside));
    }
    fs::rename(&staged.path, dest)
        .map_err(|error| format!("cannot put the new {} in place: {error}", dest.display()))?;
    done.push((staged.path.clone(), dest.to_path_buf()));
    Ok(())
}

/// Reverses completed moves, newest first. True when all of them were undone.
fn undo(done: &[(PathBuf, PathBuf)]) -> bool {
    done.iter()
        .rev()
        .all(|(from, to)| fs::rename(to, from).is_ok())
}

/// The old executables moved aside on Windows. A running one can't be deleted yet; it is
/// removed by [`sweep`] on a later self-update.
fn remove_aside(done: &[(PathBuf, PathBuf)]) {
    for (_, to) in done {
        if is_aside(to) {
            let _ = fs::remove_file(to);
        }
    }
}

fn is_aside(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            ["loadout.exe", "loadout-background.exe"]
                .iter()
                .any(|binary| name.starts_with(&format!(".{binary}.old.")))
        })
}

/// Removes executables an earlier Windows self-update moved aside, now that they may no longer
/// be running. Only names of exactly that form, and only regular files, are touched; a file
/// that is still in use stays for the next time.
pub(super) fn sweep(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if is_aside(&path) && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = fs::remove_file(&path);
        }
    }
}

#[cfg(unix)]
fn sync_dir(file: &Path) {
    if let Some(dir) = file.parent()
        && let Ok(dir) = fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_file: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_names_are_unique_and_removed_unless_published() {
        let dir = tempfile::tempdir().unwrap();
        let one = stage(dir.path(), "loadout", "", b"one", true).unwrap();
        let two = stage(dir.path(), "loadout", "", b"two", true).unwrap();
        assert_ne!(one.path(), two.path());
        assert_eq!(fs::read(one.path()).unwrap(), b"one");
        let paths = [one.path().to_path_buf(), two.path().to_path_buf()];
        drop(one);
        drop(two);
        assert!(paths.iter().all(|path| !path.exists()));
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn binaries_then_record_replace_the_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("loadout");
        let record = dir.path().join("loadout.install.toml");
        fs::write(&dest, b"old binary").unwrap();
        fs::write(&record, b"old record").unwrap();
        fs::write(dir.path().join(".loadout.new.other-run"), b"someone else's").unwrap();
        let binary = stage(dir.path(), "loadout", "", b"new binary", true).unwrap();
        let staged_record =
            stage(dir.path(), "loadout.install.toml", "", b"new record", false).unwrap();
        publish(
            vec![(binary, dest.clone())],
            (staged_record, record.clone()),
        )
        .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new binary");
        assert_eq!(fs::read(&record).unwrap(), b"new record");
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [".loadout.new.other-run", "loadout", "loadout.install.toml"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_binary_step_leaves_the_record_and_cleans_its_staging() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("loadout.install.toml");
        fs::write(&record, b"old record").unwrap();
        let blocked = dir.path().join("loadout");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("keep"), b"not empty").unwrap();
        let binary = stage(dir.path(), "loadout", "", b"new binary", true).unwrap();
        let staged_record =
            stage(dir.path(), "loadout.install.toml", "", b"new record", false).unwrap();
        let failure = publish(
            vec![(binary, blocked.clone())],
            (staged_record, record.clone()),
        )
        .unwrap_err();
        assert!(failure.restored, "{}", failure.detail);
        assert_eq!(fs::read(&record).unwrap(), b"old record");
        assert!(blocked.join("keep").exists());
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(
            names.len(),
            2,
            "only the record and the folder remain: {names:?}"
        );
    }

    #[test]
    fn sweep_removes_only_moved_aside_executables() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            ".loadout.exe.old.1",
            ".loadout-background.exe.old.2",
            ".loadout.exe.new.3",
            "loadout.exe",
            "notes.old.txt",
        ] {
            fs::write(dir.path().join(name), b"x").unwrap();
        }
        fs::create_dir(dir.path().join(".loadout.exe.old.dir")).unwrap();
        sweep(dir.path());
        let mut names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                ".loadout.exe.new.3",
                ".loadout.exe.old.dir",
                "loadout.exe",
                "notes.old.txt"
            ]
        );
    }
}
