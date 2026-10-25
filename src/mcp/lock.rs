use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use super::Error;
use crate::context::Context;
use crate::platform::Os;
use crate::reserved::{self, Merge, Part, Use};

const GROK_CONFIG: &str = "config.toml";
const GROK_LOCK: &str = ".config-init.lock";
const ATTEMPTS: u32 = 50;
const PAUSE: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    _file: File,
}

impl Lock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn same(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .eq_ignore_ascii_case(&b.to_string_lossy())
}

// A config is named by its own path and by its parent's path; either may be a link, and
// reserved::check_path accepts every alias of Grok's config, so every alias must find its lock.
fn forms(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut forms = vec![reserved::normalize(path)?];
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        forms.push(reserved::normalize(parent)?.join(name));
    }
    Ok(forms)
}

fn grok_home(context: &Context, file: &Path) -> Result<Option<PathBuf>, Error> {
    let unverifiable = |path: &Path, error: io::Error| Error::Reserved {
        path: path.to_path_buf(),
        message: format!("cannot resolve it to check for Grok's config ({error}); refusing it"),
    };
    let targets = forms(file).map_err(|error| unverifiable(file, error))?;
    let roots = context
        .agent_dir("GROK_HOME")
        .map(Path::to_path_buf)
        .into_iter()
        .chain([context.home.join(".grok")]);
    for root in roots {
        let config = root.join(GROK_CONFIG);
        let known = forms(&config).map_err(|error| unverifiable(&config, error))?;
        if targets
            .iter()
            .any(|target| known.iter().any(|known| same(target, known)))
        {
            return reserved::normalize(&root)
                .map(Some)
                .map_err(|error| unverifiable(&root, error));
        }
    }
    Ok(None)
}

/// The lock that guards writes to one Grok config: Grok's home, resolved. Two ids are equal when
/// they name the same home under the reserved checks' lossless path identity (case-insensitive
/// on Windows only), so a caller that needs several configs can drop aliases of one lock. They
/// are ordered by that same identity, never by the spelling `path` happens to keep, so every
/// caller that takes several locks in this order takes them in one order.
#[derive(Debug, Clone)]
pub struct LockId {
    home: PathBuf,
    identity: Vec<Part>,
}

impl PartialEq for LockId {
    fn eq(&self, other: &Self) -> bool {
        self.identity == other.identity
    }
}

impl Eq for LockId {}

impl Ord for LockId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.identity
            .iter()
            .map(rank)
            .cmp(other.identity.iter().map(rank))
    }
}

impl PartialOrd for LockId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One identity part as a sort key: its kind, then its exact names. Distinct parts have distinct
/// keys, so the order agrees with equality.
fn rank(part: &Part) -> (u8, u8, &OsStr, &OsStr) {
    let none = OsStr::new("");
    match part {
        Part::Disk(drive) => (0, *drive, none, none),
        Part::Share(server, share) => (1, 0, server.as_os_str(), share.as_os_str()),
        Part::Device(name) => (2, 0, name.as_os_str(), none),
        Part::Root => (3, 0, none, none),
        Part::Name(name) => (4, 0, name.as_os_str(), none),
    }
}

impl LockId {
    /// The lock file this id names.
    pub fn path(&self) -> PathBuf {
        self.home.join(GROK_LOCK)
    }
}

/// Which lock guards writes to `file`, found exactly as `lock` finds it, without creating or
/// taking anything: `Some` only for Grok's config, under any alias. A file outside Grok's home is
/// `None`.
pub fn lock_id(context: &Context, loadout: &Path, file: &Path) -> Result<Option<LockId>, Error> {
    let Some(home) = grok_home(context, file)? else {
        return Ok(None);
    };
    let how = Use::KeyMerge(Merge {
        map: "/mcp_servers",
        format: "toml",
        shape: "grok-toml",
    });
    if let Some(conflict) = reserved::check_path(context, loadout, file, how) {
        return Err(Error::Reserved {
            path: file.to_path_buf(),
            message: conflict.message(),
        });
    }
    let identity = reserved::identity(&home, context.os == Os::Windows);
    Ok(Some(LockId { home, identity }))
}

/// Finds and takes the lock for `file`: `lock_id`, then `acquire`.
pub fn lock(context: &Context, loadout: &Path, file: &Path) -> Result<Option<Lock>, Error> {
    match lock_id(context, loadout, file)? {
        Some(id) => acquire(&id).map(Some),
        None => Ok(None),
    }
}

/// Takes the lock `id` names. This may create Grok's home and its `.config-init.lock`, the only
/// files it writes; a lock file that is a link is refused, and contention gives up after about a
/// second.
pub fn acquire(id: &LockId) -> Result<Lock, Error> {
    let home = &id.home;
    let path = id.path();
    let failed = |error: io::Error| Error::Lock {
        path: path.clone(),
        message: error.to_string(),
    };
    fs::create_dir_all(home).map_err(failed)?;
    let file = open(&path).map_err(failed)?;
    for _ in 0..ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(Lock { path, _file: file }),
            Err(TryLockError::WouldBlock) => thread::sleep(PAUSE),
            Err(TryLockError::Error(error)) => return Err(failed(error)),
        }
    }
    Err(Error::Lock {
        message: "Grok is writing its config; timed out after 1s".to_owned(),
        path,
    })
}

fn open(path: &Path) -> io::Result<File> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => existing(path),
                other => other,
            }
        }
        Err(error) => Err(error),
        Ok(_) => existing(path),
    }
}

fn existing(path: &Path) -> io::Result<File> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() {
        return Err(io::Error::other(
            "is not a regular file; refusing to follow it",
        ));
    }
    let file = OpenOptions::new().write(true).open(path)?;
    let after = fs::symlink_metadata(path)?;
    if !after.is_file() || !same_file(&before, &file.metadata()?) {
        return Err(io::Error::other("changed while it was opened"));
    }
    Ok(file)
}

#[cfg(unix)]
fn same_file(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == after.dev() && before.ino() == after.ino()
}

#[cfg(not(unix))]
fn same_file(_before: &fs::Metadata, opened: &fs::Metadata) -> bool {
    opened.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(home: &str, fold_case: bool) -> LockId {
        let home = PathBuf::from(home);
        let identity = reserved::identity(&home, fold_case);
        LockId { home, identity }
    }

    // A home whose tail doesn't exist yet keeps the caller's spelling, so two callers can hold two
    // spellings of one Windows identity. Both must sort the same way against every other lock.
    #[test]
    fn case_spellings_of_one_folded_identity_sort_alike_against_others() {
        let upper = id("/h/.GROK2", true);
        let lower = id("/h/.grok2", true);
        let default = id("/h/.grok", true);
        assert_eq!(upper, lower);
        assert_eq!(upper.cmp(&lower), Ordering::Equal);
        assert_eq!(upper.cmp(&default), lower.cmp(&default));
        assert_eq!(upper.cmp(&default), Ordering::Greater, "a prefix sorts first");
        assert!(
            upper.path() < default.path(),
            "sorting by the kept spelling would have put it first"
        );
    }

    #[test]
    fn without_case_folding_spellings_are_distinct_and_ordered_exactly() {
        let upper = id("/h/.GROK2", false);
        let lower = id("/h/.grok2", false);
        assert_ne!(upper, lower);
        assert_eq!(upper.cmp(&lower), Ordering::Less);
        assert_eq!(lower.cmp(&upper), Ordering::Greater);
    }

    #[test]
    fn the_order_agrees_with_equality_over_every_pair() {
        let ids: Vec<LockId> = [
            "/h/.grok",
            "/h/.GROK",
            "/h/.grok2",
            "/h/.Grok2",
            "/h",
            "/h/.grok/x",
            "/i/.grok",
        ]
        .into_iter()
        .map(|home| id(home, true))
        .collect();
        for a in &ids {
            for b in &ids {
                assert_eq!(a.cmp(b) == Ordering::Equal, a == b, "{a:?} {b:?}");
                assert_eq!(a.cmp(b), b.cmp(a).reverse(), "{a:?} {b:?}");
            }
        }
        let mut forward = ids.clone();
        forward.sort();
        let mut backward: Vec<LockId> = ids.iter().rev().cloned().collect();
        backward.sort();
        assert!(forward.iter().zip(&backward).all(|(a, b)| a == b));
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_and_plain_drive_spellings_are_one_identity_in_one_place() {
        let plain = id(r"C:\Users\h\.GROK2", true);
        let verbatim = id(r"\\?\c:\users\h\.grok2", true);
        let default = id(r"C:\Users\h\.grok", true);
        let other_drive = id(r"D:\h\.grok", true);
        assert_eq!(plain, verbatim);
        assert_eq!(plain.cmp(&default), verbatim.cmp(&default));
        assert_eq!(plain.cmp(&other_drive), verbatim.cmp(&other_drive));
    }
}
