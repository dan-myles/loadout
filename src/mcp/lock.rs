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

pub fn lock(context: &Context, loadout: &Path, file: &Path) -> Result<Option<Lock>, Error> {
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
