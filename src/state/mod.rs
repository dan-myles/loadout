//! Ownership records, the one hash definition, and the journaled safe write (architecture
//! sections 4 and 5).

mod disk;
mod hash;
mod journal;
mod rollback;
mod run;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::plan::{Content, ItemRef, Observed, Target};
use crate::reserved::{self, Use};

pub use rollback::{RolledBack, rollback};
pub use run::{Removal, Run};

const STATE_FILE: &str = "state.json";
const LOCK_FILE: &str = "lock";
const VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot {action} {path}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("another loadout run holds {path}; wait for it to finish and try again")]
    Locked { path: PathBuf },
    #[error("{path} {message}")]
    Reserved { path: PathBuf, message: String },
    #[error("{path} is not a regular file or folder; loadout never reads or replaces it")]
    Special { path: PathBuf },
    #[error("{path} is not valid UTF-8, which loadout's records need")]
    NotUtf8 { path: PathBuf },
    #[error("{target} changed since it was planned; nothing was written to it")]
    Changed { target: String },
    #[error(
        "{target} is an MCP entry; reading and writing entries needs the mcp module, which this build doesn't have yet"
    )]
    EntryNeedsMcp { target: String },
    #[error("{target} is a program; programs are installed by the programs module, never written")]
    Program { target: String },
    #[error("invalid content: {message}")]
    InvalidContent { message: String },
    #[error("{target} is not in this run's journal")]
    NotJournaled { target: String },
    #[error("{target} appears twice in the plan")]
    Duplicate { target: String },
    #[error("{target} is planned as {change}, which {operation} doesn't perform")]
    WrongChange {
        target: String,
        change: String,
        operation: &'static str,
    },
    #[error("{target} was already handled in this run")]
    AlreadyRan { target: String },
    #[error("{target} is not owned by loadout, so it is never removed")]
    NotOwned { target: String },
    #[error("there is no run {run}")]
    UnknownRun { run: String },
    #[error("there are no runs to roll back")]
    NoRuns,
    #[error("run {run} was already rolled back")]
    AlreadyRolledBack { run: String },
    #[error("rollback of run {run} would overwrite changes made after it, so nothing was restored:\n{}", .targets.join("\n"))]
    RollbackBlocked { run: String, targets: Vec<String> },
    #[error("the backup of {target} in run {run} doesn't match what the run recorded")]
    BackupCorrupt { run: String, target: String },
}

impl Error {
    /// True for outcomes that block the machine rather than break the tool: a target that
    /// changed under loadout, a rollback that would overwrite later changes, or a reserved path.
    pub fn is_blocked(&self) -> bool {
        matches!(
            self,
            Error::Changed { .. } | Error::RollbackBlocked { .. } | Error::Reserved { .. }
        )
    }
}

/// How a target is shown in messages.
pub fn label(target: &Target) -> String {
    match target {
        Target::Path { path } => path.display().to_string(),
        Target::Entry {
            file, pointer, key, ..
        } => format!("{}#{pointer}/{key}", file.display()),
        Target::Program { manager, package } => format!("{} {package}", manager.id()),
    }
}

/// One target loadout owns, with the hash of what it last wrote there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub target: Target,
    pub item: ItemRef,
    pub sha256: String,
    pub run: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    records: Vec<Record>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    records: BTreeMap<Target, Record>,
}

impl State {
    /// Reads `<state>/state.json`. A machine loadout has never written to has no records.
    pub fn load(context: &Context) -> Result<State, Error> {
        let path = context.state.join(STATE_FILE);
        if fs::symlink_metadata(&context.state).is_err() {
            return Ok(State::default());
        }
        let Some(bytes) = disk::read_internal(&path, &context.state)? else {
            return Ok(State::default());
        };
        let file: StateFile = serde_json::from_slice(&bytes).map_err(|error| Error::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if file.version != VERSION {
            return Err(Error::Invalid {
                path,
                message: format!("has version {}, expected {VERSION}", file.version),
            });
        }
        let mut records = BTreeMap::new();
        for record in file.records {
            if records.insert(record.target.clone(), record).is_some() {
                return Err(Error::Invalid {
                    path,
                    message: "records the same target twice".to_owned(),
                });
            }
        }
        Ok(State { records })
    }

    pub fn record(&self, target: &Target) -> Option<&Record> {
        self.records.get(target)
    }

    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }

    fn set(&mut self, target: &Target, record: Option<Record>) {
        match record {
            Some(record) => self.records.insert(target.clone(), record),
            None => self.records.remove(target),
        };
    }

    fn save(&self, context: &Context) -> Result<(), Error> {
        let path = context.state.join(STATE_FILE);
        let file = StateFile {
            version: VERSION,
            records: self.records.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|error| Error::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        disk::private_dir(&context.state)?;
        disk::save(&path, &bytes)
    }
}

/// Held for one plan and apply, or one rollback. Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

pub fn lock(context: &Context) -> Result<Lock, Error> {
    disk::private_dir(&context.state)?;
    let path = context.state.join(LOCK_FILE);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(Error::Invalid {
            path,
            message: "must be a regular file".to_owned(),
        });
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(disk::io("open", &path))?;
    match file.try_lock() {
        Ok(()) => Ok(Lock { _file: file }),
        Err(TryLockError::WouldBlock) => Err(Error::Locked { path }),
        Err(TryLockError::Error(source)) => Err(Error::Io {
            action: "lock",
            path,
            source,
        }),
    }
}

/// Whether another process holds the lock right now. Never creates anything.
pub fn busy(context: &Context) -> Result<bool, Error> {
    let path = context.state.join(LOCK_FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(Error::Invalid {
                path,
                message: "must be a regular file".to_owned(),
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(disk::io("inspect", &path)(error)),
    }
    let file = File::open(&path).map_err(disk::io("open", &path))?;
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(source)) => Err(Error::Io {
            action: "lock",
            path,
            source,
        }),
    }
}

/// The hash of desired content, by the one definition in architecture section 5.
pub fn digest(content: &Content) -> Result<Observed, Error> {
    let sha256 = match content {
        Content::File(file) => hash::file(&hash::Blob {
            bytes: &file.bytes,
            executable: file.executable,
        }),
        Content::Tree(files) => {
            let keyed = tree_files(files)?;
            hash::tree(keyed.iter().map(|(path, file)| {
                (
                    path.as_str(),
                    hash::Blob {
                        bytes: &file.bytes,
                        executable: file.executable,
                    },
                )
            }))
        }
        Content::Entry(value) => hash::entry(value),
    };
    Ok(Observed::Present { sha256 })
}

fn tree_files(
    files: &BTreeMap<PathBuf, crate::plan::FileContent>,
) -> Result<BTreeMap<String, &crate::plan::FileContent>, Error> {
    let mut keyed = BTreeMap::new();
    for (path, file) in files {
        let key = disk::tree_key(path)?;
        if keyed.insert(key.clone(), file).is_some() {
            return Err(Error::InvalidContent {
                message: format!("{key} appears twice in one folder"),
            });
        }
    }
    if let Some(key) = keyed.keys().find(|key| {
        keyed
            .keys()
            .any(|other| other.starts_with(&format!("{key}/")))
    }) {
        return Err(Error::InvalidContent {
            message: format!("{key} is both a file and a folder"),
        });
    }
    Ok(keyed)
}

/// What a target holds now. The target first goes through `reserved::check_path`, so an auth
/// store, loadout's own state, or the active checkout (`loadout`) is never opened or hashed.
/// A symlinked target is followed to the real file or folder.
pub fn observe(context: &Context, loadout: &Path, target: &Target) -> Result<Observed, Error> {
    let path = path_of(target)?;
    check_reserved(context, loadout, path)?;
    Ok(observed(disk::read(&disk::real(path)?)?.as_ref()))
}

fn observed(disk: Option<&disk::Disk>) -> Observed {
    match disk {
        None => Observed::Absent,
        Some(disk) => Observed::Present {
            sha256: disk.sha256(),
        },
    }
}

fn path_of(target: &Target) -> Result<&Path, Error> {
    match target {
        Target::Path { path } => Ok(path),
        Target::Entry { .. } => Err(Error::EntryNeedsMcp {
            target: label(target),
        }),
        Target::Program { .. } => Err(Error::Program {
            target: label(target),
        }),
    }
}

fn check_reserved(context: &Context, loadout: &Path, path: &Path) -> Result<(), Error> {
    match reserved::check_path(context, loadout, path, Use::Write) {
        Some(conflict) => Err(Error::Reserved {
            path: path.to_path_buf(),
            message: conflict.message(),
        }),
        None => Ok(()),
    }
}

/// How an owned target looks now compared with its record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "health", content = "detail", rename_all = "lowercase")]
pub enum Health {
    Ok,
    Drifted,
    Missing,
    /// Not checked: a reserved path, an unreadable target, or an entry this build can't read.
    Unchecked(String),
}

/// Checks every record against the disk, in record order.
pub fn audit<'a>(context: &Context, loadout: &Path, state: &'a State) -> Vec<(&'a Record, Health)> {
    state
        .records()
        .map(|record| {
            let health = match observe(context, loadout, &record.target) {
                Ok(Observed::Present { sha256 }) if sha256 == record.sha256 => Health::Ok,
                Ok(Observed::Absent) => Health::Missing,
                Ok(_) => Health::Drifted,
                Err(error) => Health::Unchecked(error.to_string()),
            };
            (record, health)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RunStatus {
    /// Another process holds the lock, and this is the newest unfinished run.
    InProgress,
    /// The run stopped before it finished; `rollback` undoes what it did.
    Interrupted,
    Finished,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSummary {
    pub id: String,
    pub started: Option<String>,
    pub loadout: Option<PathBuf>,
    pub status: Option<RunStatus>,
    /// Journaled actions, and how many of them reached the disk.
    pub actions: usize,
    pub done: usize,
    /// Set when the journal can't be read; `status` is then `None`.
    pub error: Option<String>,
}

/// Every run in `<state>/runs`, oldest first. Never writes.
pub fn runs(context: &Context) -> Result<Vec<RunSummary>, Error> {
    let ids = journal::run_ids(&context.state)?;
    let busy = busy(context)?;
    let mut summaries: Vec<RunSummary> = ids
        .into_iter()
        .map(|id| {
            let dir = context.state.join(journal::RUNS).join(&id);
            match journal::Journal::load(&dir, &context.state) {
                Ok(journal) => RunSummary {
                    started: Some(journal.started.clone()),
                    loadout: Some(journal.loadout.clone()),
                    status: Some(match journal.status {
                        journal::Status::Running => RunStatus::Interrupted,
                        journal::Status::Finished => RunStatus::Finished,
                        journal::Status::RolledBack => RunStatus::RolledBack,
                    }),
                    actions: journal.entries.len(),
                    done: journal
                        .entries
                        .iter()
                        .filter(|entry| {
                            matches!(entry.step, journal::Step::Done | journal::Step::Released)
                        })
                        .count(),
                    error: None,
                    id,
                },
                Err(error) => RunSummary {
                    id,
                    started: None,
                    loadout: None,
                    status: None,
                    actions: 0,
                    done: 0,
                    error: Some(error.to_string()),
                },
            }
        })
        .collect();
    if busy
        && let Some(newest) = summaries
            .iter_mut()
            .rev()
            .find(|summary| summary.status == Some(RunStatus::Interrupted))
    {
        newest.status = Some(RunStatus::InProgress);
    }
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::plan::FileContent;

    #[test]
    fn digest_distinguishes_mode_on_unix_only() {
        let file = |executable| {
            Content::File(FileContent {
                bytes: b"#!/bin/sh\n".to_vec(),
                executable,
            })
        };
        let same = digest(&file(true)).unwrap() == digest(&file(false)).unwrap();
        assert_eq!(same, cfg!(windows));
    }

    #[test]
    fn tree_digest_rejects_unsafe_or_conflicting_paths() {
        let file = FileContent {
            bytes: Vec::new(),
            executable: false,
        };
        for paths in [vec!["../escape"], vec!["/abs"], vec!["a", "a/b"]] {
            let tree = paths
                .into_iter()
                .map(|path| (PathBuf::from(path), file.clone()))
                .collect::<BTreeMap<_, _>>();
            assert!(digest(&Content::Tree(tree)).is_err());
        }
    }

    #[test]
    fn entry_digest_ignores_key_order() {
        assert_eq!(
            digest(&Content::Entry(json!({"command": "x", "args": ["a"]}))).unwrap(),
            digest(&Content::Entry(json!({"args": ["a"], "command": "x"}))).unwrap()
        );
    }
}
