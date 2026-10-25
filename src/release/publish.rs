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

/// What a binary's destination held when the installed files were last checked, under the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Prior {
    /// The installed binary its record names, which is replaced.
    Installed,
    /// Nothing: a binary the record doesn't name yet, the launcher a schema-1 Windows install gains.
    /// It goes in only where nothing is, never over a file that has appeared since.
    Nothing,
}

/// Publishes the staged binaries over their destinations, in order, then the record.
///
/// On Unix each binary is one atomic rename over the old file, which a running process keeps
/// using. Windows can't rename over a running executable, but it can rename the running file, so
/// each existing binary is first moved aside to `.<name>.old.<nonce>`, a name this run owns. If a
/// later binary step fails, every completed move is undone in reverse, each attempted even when
/// another can't be, and the record is untouched. Staging files moved back are this run's to
/// remove again; files whose move back failed stay where they are, for recovery, and are named in
/// the failure. On Unix a rename over a binary can't be undone, so a later failure there is never
/// reported as restored.
/// Once every binary is in place the record is renamed over the old one. If that last rename
/// fails, the new binaries stay and the old record no longer matches them, which fails closed.
/// Only after the record is in place are moved-aside executables deleted (see `settle_asides`).
pub(super) fn publish(
    binaries: Vec<(Staged, PathBuf, Prior)>,
    record: (Staged, PathBuf),
) -> Result<(), Failure> {
    let mut done: Vec<Move> = Vec::new();
    let mut published: Vec<Staged> = Vec::new();
    // A Unix rename over an installed binary, which no undo can reverse, has happened.
    let mut irreversible = false;
    for (mut staged, dest, prior) in binaries {
        let placed = match prior {
            Prior::Installed => place(&staged, &dest, &mut done),
            Prior::Nothing => place_new(&staged, &dest, &mut done),
        };
        if let Err(detail) = placed {
            let failed = undo(&done);
            for staged in &mut published {
                staged.published = failed.iter().any(|(step, _)| step.from == staged.path);
            }
            let restored = failed.is_empty() && !irreversible;
            let detail = if failed.is_empty() {
                detail
            } else {
                let unrestored: Vec<String> = failed
                    .iter()
                    .map(|(step, error)| {
                        format!(
                            "{} could not be moved back to {} ({error})",
                            step.to.display(),
                            step.from.display()
                        )
                    })
                    .collect();
                format!("{detail}; then {}", unrestored.join("; "))
            };
            return Err(Failure { detail, restored });
        }
        irreversible |= !cfg!(windows) && prior == Prior::Installed;
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
    if let Some(dir) = dest.parent() {
        settle_asides(dir, &done, delete_file);
    }
    Ok(())
}

/// One completed rename, which [`undo`] reverses. `aside` marks an existing destination this run
/// moved to a fresh `.<name>.old.<nonce>` name.
#[derive(Debug)]
struct Move {
    from: PathBuf,
    to: PathBuf,
    aside: bool,
}

#[cfg(not(windows))]
fn place(staged: &Staged, dest: &Path, done: &mut Vec<Move>) -> Result<(), String> {
    let _ = done;
    fs::rename(&staged.path, dest)
        .map_err(|error| format!("cannot replace {}: {error}", dest.display()))
}

#[cfg(windows)]
fn place(staged: &Staged, dest: &Path, done: &mut Vec<Move>) -> Result<(), String> {
    if fs::symlink_metadata(dest).is_ok() {
        let name = dest
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let aside = dest.with_file_name(format!(".{name}.old.{}", nonce()));
        fs::rename(dest, &aside)
            .map_err(|error| format!("cannot move {} aside: {error}", dest.display()))?;
        done.push(Move {
            from: dest.to_path_buf(),
            to: aside,
            aside: true,
        });
    }
    fs::rename(&staged.path, dest)
        .map_err(|error| format!("cannot put the new {} in place: {error}", dest.display()))?;
    done.push(Move {
        from: staged.path.clone(),
        to: dest.to_path_buf(),
        aside: false,
    });
    Ok(())
}

/// Puts a staged binary where nothing is: a hard link, which fails if anything is there, then the
/// staging name is dropped. Undone like a rename, by moving it back to the staging name.
fn place_new(staged: &Staged, dest: &Path, done: &mut Vec<Move>) -> Result<(), String> {
    fs::hard_link(&staged.path, dest).map_err(|error| {
        format!(
            "cannot put the new {} where nothing was: {error}",
            dest.display()
        )
    })?;
    if let Err(error) = fs::remove_file(&staged.path) {
        // The staging name still holds the file, so the link just made is the one to drop.
        let _ = fs::remove_file(dest);
        return Err(format!(
            "cannot drop the staging name {}: {error}",
            staged.path.display()
        ));
    }
    done.push(Move {
        from: staged.path.clone(),
        to: dest.to_path_buf(),
        aside: false,
    });
    Ok(())
}

/// Reverses completed moves, newest first, attempting every one even after another fails. Returns
/// the moves that couldn't be reversed, each with why.
fn undo(done: &[Move]) -> Vec<(&Move, io::Error)> {
    done.iter()
        .rev()
        .filter_map(|step| {
            fs::rename(&step.to, &step.from)
                .err()
                .map(|error| (step, error))
        })
        .collect()
}

/// The ledger of executables completed self-updates moved aside on Windows but couldn't delete,
/// because they were running: one `<file name> <sha256>` line each.
pub(super) const ASIDES_FILE: &str = ".loadout.install.asides";
const ASIDES_LIMIT: u64 = 64 * 1024;

/// After a completed publication, under the install lock: deletes the executables this run moved
/// aside, and the ones the ledger lists, then rewrites the ledger to list those still running.
///
/// Only files this run moved, or a regular file at exactly a listed name with exactly its listed
/// sha256, are ever deleted, so a file of any other name or content is left alone however it is
/// named, and so is any other writer's aside. A missing, unreadable or malformed ledger lists
/// nothing; a malformed or linked one is also left as it is, and nothing new is recorded.
fn settle_asides(dir: &Path, done: &[Move], delete: fn(&Path) -> io::Result<()>) {
    let Some(listed) = read_asides(dir) else {
        for step in done.iter().filter(|step| step.aside) {
            let _ = delete(&step.to);
        }
        return;
    };
    let mut running = Vec::new();
    for (name, sha256) in listed {
        let path = dir.join(&name);
        let ours = fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file())
            && super::sha256_file(&path).is_ok_and(|actual| actual == sha256);
        if ours && delete(&path).is_err() {
            running.push((name, sha256));
        }
    }
    for step in done.iter().filter(|step| step.aside) {
        if delete(&step.to).is_ok() {
            continue;
        }
        let name = step.to.file_name().and_then(|name| name.to_str());
        if let (Some(name), Ok(sha256)) = (name, super::sha256_file(&step.to)) {
            running.push((name.to_owned(), sha256));
        }
    }
    let path = dir.join(ASIDES_FILE);
    if running.is_empty() {
        let _ = fs::remove_file(&path);
        return;
    }
    let mut text = String::new();
    for (name, sha256) in &running {
        text.push_str(&format!("{name} {sha256}\n"));
    }
    if let Ok(mut staged) = stage(dir, ASIDES_FILE, "", text.as_bytes(), false)
        && fs::rename(&staged.path, &path).is_ok()
    {
        staged.published = true;
    }
}

fn delete_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

/// The ledger's entries: `Some(empty)` when there is none, `None` when it can't be trusted.
fn read_asides(dir: &Path) -> Option<Vec<(String, String)>> {
    let path = dir.join(ASIDES_FILE);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(Vec::new()),
        Ok(metadata) if metadata.file_type().is_file() && metadata.len() <= ASIDES_LIMIT => {}
        _ => return None,
    }
    let text = String::from_utf8(fs::read(&path).ok()?).ok()?;
    let body = text.strip_suffix('\n')?;
    body.split('\n')
        .map(|line| {
            let (name, sha256) = line.split_once(' ')?;
            let nonce = ["loadout.exe", "loadout-background.exe"]
                .iter()
                .find_map(|binary| name.strip_prefix(&format!(".{binary}.old.")))?;
            let hex = |text: &str, len: Option<usize>| {
                !text.is_empty()
                    && len.is_none_or(|len| text.len() == len)
                    && text
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            };
            (hex(nonce, None) && hex(sha256, Some(64)))
                .then(|| (name.to_owned(), sha256.to_owned()))
        })
        .collect()
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
        fs::write(dir.path().join(".loadout.exe.old.keep"), b"someone else's").unwrap();
        let binary = stage(dir.path(), "loadout", "", b"new binary", true).unwrap();
        let staged_record =
            stage(dir.path(), "loadout.install.toml", "", b"new record", false).unwrap();
        publish(
            vec![(binary, dest.clone(), Prior::Installed)],
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
            [
                ".loadout.exe.old.keep",
                ".loadout.new.other-run",
                "loadout",
                "loadout.install.toml"
            ]
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
            vec![(binary, blocked.clone(), Prior::Installed)],
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
    fn a_binary_new_to_the_record_goes_only_where_nothing_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("loadout.install.toml"), b"old record").unwrap();
        let launcher = stage(path, "loadout-background", ".exe", b"launcher", true).unwrap();
        let record = stage(path, "loadout.install.toml", "", b"new record", false).unwrap();
        publish(
            vec![(
                launcher,
                path.join("loadout-background.exe"),
                Prior::Nothing,
            )],
            (record, path.join("loadout.install.toml")),
        )
        .unwrap();
        assert_eq!(
            fs::read(path.join("loadout-background.exe")).unwrap(),
            b"launcher"
        );
        assert_eq!(
            names(path),
            ["loadout-background.exe", "loadout.install.toml"]
        );

        // Something appeared there since the last check: it is left alone, and nothing changes.
        fs::write(path.join("loadout-background.exe"), b"someone else's").unwrap();
        let launcher = stage(path, "loadout-background", ".exe", b"launcher", true).unwrap();
        let record = stage(path, "loadout.install.toml", "", b"newer record", false).unwrap();
        let failure = publish(
            vec![(
                launcher,
                path.join("loadout-background.exe"),
                Prior::Nothing,
            )],
            (record, path.join("loadout.install.toml")),
        )
        .unwrap_err();
        assert!(failure.restored, "{}", failure.detail);
        assert_eq!(
            fs::read(path.join("loadout-background.exe")).unwrap(),
            b"someone else's"
        );
        assert_eq!(
            fs::read(path.join("loadout.install.toml")).unwrap(),
            b"new record"
        );
        assert_eq!(
            names(path),
            ["loadout-background.exe", "loadout.install.toml"]
        );
    }

    #[test]
    fn undo_attempts_every_move_and_names_each_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        // What Windows publication of the launcher, then the aside of the main, leaves.
        let steps = [
            Move {
                from: path.join("loadout-background.exe"),
                to: path.join(".loadout-background.exe.old.1"),
                aside: true,
            },
            Move {
                from: path.join(".loadout-background.new.2.exe"),
                to: path.join("loadout-background.exe"),
                aside: false,
            },
            Move {
                from: path.join("loadout.exe"),
                to: path.join(".loadout.exe.old.3"),
                aside: true,
            },
        ];
        fs::write(path.join(".loadout-background.exe.old.1"), b"old launcher").unwrap();
        fs::write(path.join("loadout-background.exe"), b"new launcher").unwrap();
        fs::write(path.join(".loadout.exe.old.3"), b"old main").unwrap();
        // The newest move can't be reversed: something now sits where the main was.
        fs::create_dir(path.join("loadout.exe")).unwrap();
        fs::write(path.join("loadout.exe").join("keep"), b"blocker").unwrap();

        let failed = undo(&steps);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0.to, path.join(".loadout.exe.old.3"));
        assert_eq!(
            fs::read(path.join("loadout-background.exe")).unwrap(),
            b"old launcher"
        );
        assert_eq!(
            fs::read(path.join(".loadout-background.new.2.exe")).unwrap(),
            b"new launcher"
        );
        assert_eq!(
            fs::read(path.join(".loadout.exe.old.3")).unwrap(),
            b"old main",
            "the move that failed leaves its file for recovery"
        );
        assert!(!path.join(".loadout-background.exe.old.1").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_later_failure_after_a_unix_rename_is_not_called_restored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("first"), b"old first").unwrap();
        fs::create_dir(path.join("second")).unwrap();
        fs::write(path.join("second").join("keep"), b"blocker").unwrap();
        fs::write(path.join("loadout.install.toml"), b"old record").unwrap();
        let first = stage(path, "first", "", b"new first", true).unwrap();
        let second = stage(path, "second", "", b"new second", true).unwrap();
        let record = stage(path, "loadout.install.toml", "", b"new record", false).unwrap();
        let failure = publish(
            vec![
                (first, path.join("first"), Prior::Installed),
                (second, path.join("second"), Prior::Installed),
            ],
            (record, path.join("loadout.install.toml")),
        )
        .unwrap_err();
        assert!(!failure.restored, "{}", failure.detail);
        assert_eq!(
            fs::read(path.join("loadout.install.toml")).unwrap(),
            b"old record"
        );
        assert_eq!(
            names(path),
            ["first", "loadout.install.toml", "second"],
            "no staging file is left"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_failed_windows_publication_puts_everything_back_and_cleans_its_staging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("loadout-background.exe"), b"old launcher").unwrap();
        fs::write(path.join("loadout.exe"), b"old main").unwrap();
        fs::write(path.join("loadout.install.toml"), b"old record").unwrap();
        let launcher = stage(path, "loadout-background", ".exe", b"new launcher", true).unwrap();
        let main = stage(path, "loadout", ".exe", b"new main", true).unwrap();
        let record = stage(path, "loadout.install.toml", "", b"new record", false).unwrap();
        // The main's staging file disappears, so placing it fails after the launcher is in.
        fs::remove_file(main.path()).unwrap();
        let failure = publish(
            vec![
                (
                    launcher,
                    path.join("loadout-background.exe"),
                    Prior::Installed,
                ),
                (main, path.join("loadout.exe"), Prior::Installed),
            ],
            (record, path.join("loadout.install.toml")),
        )
        .unwrap_err();
        assert!(failure.restored, "{}", failure.detail);
        assert_eq!(fs::read(path.join("loadout.exe")).unwrap(), b"old main");
        assert_eq!(
            fs::read(path.join("loadout-background.exe")).unwrap(),
            b"old launcher"
        );
        assert_eq!(
            names(path),
            [
                "loadout-background.exe",
                "loadout.exe",
                "loadout.install.toml"
            ],
            "the launcher's staging file, moved back, is removed again"
        );
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn sha(bytes: &[u8]) -> String {
        super::super::sha256(bytes)
    }

    /// One aside this run made, already moved.
    fn moved(dir: &Path, name: &str) -> Move {
        Move {
            from: dir.join("loadout.exe"),
            to: dir.join(name),
            aside: true,
        }
    }

    #[test]
    fn only_listed_asides_with_their_exact_bytes_are_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        for (name, bytes) in [
            (".loadout.exe.old.1a", b"old one".as_slice()),
            (".loadout-background.exe.old.2b", b"old launcher"),
            (".loadout.exe.old.3c", b"replaced since"),
            (".loadout.exe.old.keep", b"someone else's"),
            (".loadout.exe.old.4d", b"another writer's, mid-transaction"),
            (".loadout.exe.new.5e", b"staging"),
        ] {
            fs::write(path.join(name), bytes).unwrap();
        }
        fs::create_dir(path.join(".loadout.exe.old.6f")).unwrap();
        let ledger = format!(
            ".loadout.exe.old.1a {}\n.loadout-background.exe.old.2b {}\n.loadout.exe.old.3c {}\n.loadout.exe.old.6f {}\n",
            sha(b"old one"),
            sha(b"old launcher"),
            sha(b"what was moved aside"),
            sha(b"")
        );
        fs::write(path.join(ASIDES_FILE), ledger).unwrap();

        settle_asides(path, &[], delete_file);
        assert_eq!(
            names(path),
            [
                ".loadout.exe.new.5e",
                ".loadout.exe.old.3c",
                ".loadout.exe.old.4d",
                ".loadout.exe.old.6f",
                ".loadout.exe.old.keep",
            ]
        );
        assert_eq!(
            fs::read(path.join(".loadout.exe.old.3c")).unwrap(),
            b"replaced since"
        );
    }

    #[test]
    fn running_asides_are_listed_with_their_hash_until_they_can_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join(".loadout.exe.old.1a"), b"running main").unwrap();
        fs::write(
            path.join(".loadout-background.exe.old.2b"),
            b"stopped launcher",
        )
        .unwrap();
        let done = [
            moved(path, ".loadout.exe.old.1a"),
            moved(path, ".loadout-background.exe.old.2b"),
        ];
        let running = |file: &Path| {
            if file.ends_with(".loadout.exe.old.1a") {
                Err(io::Error::other("in use"))
            } else {
                fs::remove_file(file)
            }
        };
        settle_asides(path, &done, running);
        assert_eq!(
            fs::read_to_string(path.join(ASIDES_FILE)).unwrap(),
            format!(".loadout.exe.old.1a {}\n", sha(b"running main"))
        );
        assert_eq!(
            names(path),
            [".loadout.exe.old.1a", ".loadout.install.asides"]
        );

        // A later completed publication, once it has stopped running.
        settle_asides(path, &[], delete_file);
        assert!(names(path).is_empty());
    }

    #[test]
    fn an_untrusted_ledger_deletes_nothing_it_lists() {
        let listed = format!(".loadout.exe.old.1a {}\n", sha(b"old"));
        for (name, ledger) in [
            ("no final newline", listed.trim_end().to_owned()),
            ("bad name", format!("loadout.exe {}\n", sha(b"old"))),
            (
                "nested name",
                format!(".loadout.exe.old.1a/x {}\n", sha(b"old")),
            ),
            (
                "uppercase hash",
                format!(".loadout.exe.old.1a {}\n", sha(b"old").to_uppercase()),
            ),
            ("short hash", ".loadout.exe.old.1a abc\n".to_owned()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path();
            fs::write(path.join(".loadout.exe.old.1a"), b"old").unwrap();
            fs::write(path.join(ASIDES_FILE), &ledger).unwrap();
            settle_asides(path, &[], delete_file);
            assert_eq!(
                fs::read(path.join(".loadout.exe.old.1a")).unwrap(),
                b"old",
                "{name}"
            );
            assert_eq!(
                fs::read_to_string(path.join(ASIDES_FILE)).unwrap(),
                ledger,
                "{name}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_publication_deletes_only_its_own_asides() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        fs::write(path.join("loadout-background.exe"), b"old launcher").unwrap();
        fs::write(path.join("loadout.exe"), b"old main").unwrap();
        fs::write(path.join("loadout.install.toml"), b"old record").unwrap();
        fs::write(path.join(".loadout.exe.old.keep"), b"someone else's").unwrap();
        fs::write(path.join(".loadout.exe.old.0123"), b"another run's").unwrap();
        let launcher = stage(path, "loadout-background", ".exe", b"new launcher", true).unwrap();
        let main = stage(path, "loadout", ".exe", b"new main", true).unwrap();
        let record = stage(path, "loadout.install.toml", "", b"new record", false).unwrap();
        publish(
            vec![
                (
                    launcher,
                    path.join("loadout-background.exe"),
                    Prior::Installed,
                ),
                (main, path.join("loadout.exe"), Prior::Installed),
            ],
            (record, path.join("loadout.install.toml")),
        )
        .unwrap();
        assert_eq!(fs::read(path.join("loadout.exe")).unwrap(), b"new main");
        assert_eq!(
            fs::read(path.join("loadout-background.exe")).unwrap(),
            b"new launcher"
        );
        assert_eq!(
            names(path),
            [
                ".loadout.exe.old.0123",
                ".loadout.exe.old.keep",
                "loadout-background.exe",
                "loadout.exe",
                "loadout.install.toml"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_ledger_or_listed_link_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("old");
        fs::write(&target, b"old").unwrap();
        std::os::unix::fs::symlink(&target, path.join(".loadout.exe.old.1a")).unwrap();
        fs::write(
            outside.path().join("ledger"),
            format!(".loadout.exe.old.1a {}\n", sha(b"old")),
        )
        .unwrap();
        fs::write(
            path.join(ASIDES_FILE),
            fs::read(outside.path().join("ledger")).unwrap(),
        )
        .unwrap();
        settle_asides(path, &[], delete_file);
        assert!(fs::symlink_metadata(path.join(".loadout.exe.old.1a")).is_ok());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(
            !path.join(ASIDES_FILE).exists(),
            "a listed link isn't ours, so it is dropped"
        );

        std::os::unix::fs::symlink(outside.path().join("ledger"), path.join(ASIDES_FILE)).unwrap();
        settle_asides(path, &[], delete_file);
        assert!(fs::symlink_metadata(path.join(".loadout.exe.old.1a")).is_ok());
        assert!(
            fs::symlink_metadata(path.join(ASIDES_FILE))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
