use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use super::Error;
use crate::context::Context;
use crate::reserved::{self, Merge, Use};

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

fn grok_home(context: &Context, file: &Path) -> Option<PathBuf> {
    if file.file_name()? != GROK_CONFIG {
        return None;
    }
    let parent = reserved::normalize(file.parent()?).ok()?;
    context
        .agent_dir("GROK_HOME")
        .map(Path::to_path_buf)
        .into_iter()
        .chain([context.home.join(".grok")])
        .filter_map(|root| reserved::normalize(&root).ok())
        .find(|root| *root == parent)
}

pub fn lock(context: &Context, loadout: &Path, file: &Path) -> Result<Option<Lock>, Error> {
    let Some(home) = grok_home(context, file) else {
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
    let path = home.join(GROK_LOCK);
    let failed = |error: io::Error| Error::Lock {
        path: path.clone(),
        message: error.to_string(),
    };
    fs::create_dir_all(&home).map_err(failed)?;
    let file = open(&path).map_err(failed)?;
    for _ in 0..ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(Some(Lock { path, _file: file })),
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
