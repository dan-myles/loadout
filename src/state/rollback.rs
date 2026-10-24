use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use super::disk::{self, Disk, Mode, Node, Planned, Put};
use super::journal::{self, BackupKind, Entry, Journal, Status, Step};
use super::run::{tag, unchanged};
use super::{Error, State, check_reserved, label, lock, observed, path_of};
use crate::context::Context;
use crate::plan::{Observed, Target};

/// What `rollback` restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledBack {
    pub run: String,
    /// Targets put back to what they held before the run, in the order they were restored.
    pub restored: Vec<Target>,
}

/// Undoes one run, by default the newest: restores each backup, deletes what the run created,
/// and puts the ownership records back. An interrupted run is undone the same way.
///
/// Every target the run touched must still hold what the run left there (or already hold what
/// it had before). Otherwise nothing is restored and the changed targets are listed, so a later
/// run's work or the user's edits are never overwritten. A rollback that stops halfway can be
/// run again: targets already restored are skipped.
pub fn rollback(context: &Context, run: Option<&str>) -> Result<RolledBack, Error> {
    if fs::symlink_metadata(&context.state).is_err() {
        return Err(Error::NoRuns);
    }
    let _lock = lock(context)?;
    let ids = journal::run_ids(&context.state)?;
    let id = match run {
        Some(run) => {
            ids.iter()
                .find(|id| *id == run)
                .cloned()
                .ok_or_else(|| Error::UnknownRun {
                    run: run.to_owned(),
                })?
        }
        None => ids.last().cloned().ok_or(Error::NoRuns)?,
    };
    let dir = context.state.join(journal::RUNS).join(&id);
    let mut journal = Journal::load(&dir, &context.state)?;
    if journal.status == Status::RolledBack {
        return Err(Error::AlreadyRolledBack { run: id });
    }

    let mut work = Vec::new();
    let mut blocked = Vec::new();
    for (index, entry) in journal.entries.iter().enumerate().rev() {
        if !matches!(entry.step, Step::Started | Step::Done) {
            continue;
        }
        let path = path_of(&entry.target)?;
        if let Err(error) = check_reserved(context, &journal.loadout, path) {
            blocked.push(error.to_string());
            continue;
        }
        let now = observed(disk::read(&disk::real(path)?)?.as_ref());
        if now == entry.prior {
            work.push((index, false));
        } else if Some(&now) == entry.after.as_ref() {
            work.push((index, true));
        } else {
            blocked.push(format!("{} changed after run {id}", label(&entry.target)));
        }
    }
    if !blocked.is_empty() {
        return Err(Error::RollbackBlocked {
            run: id,
            targets: blocked,
        });
    }

    let mut restored = Vec::new();
    for (index, needed) in work {
        let entry = &journal.entries[index];
        let real = disk::real(path_of(&entry.target)?)?;
        let tag = tag(&id, index);
        if needed {
            restore(context, &dir, &id, entry, &real, &tag)?;
            restored.push(entry.target.clone());
        }
        disk::remove_scratch(&disk::scratch(&real, &tag, "tmp")?)?;
        disk::remove_scratch(&disk::scratch(&real, &tag, "old")?)?;
        for created in entry.created.iter().rev() {
            match fs::remove_dir(created) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(disk::io("remove", created)(error)),
            }
        }
    }

    let mut state = State::load(context)?;
    for entry in &journal.entries {
        if matches!(entry.step, Step::Started | Step::Done | Step::Released) {
            state.set(&entry.target, entry.record.clone());
        }
    }
    state.save(context)?;
    journal.status = Status::RolledBack;
    journal.save(&dir)?;
    Ok(RolledBack { run: id, restored })
}

fn restore(
    context: &Context,
    dir: &Path,
    run: &str,
    entry: &Entry,
    real: &Path,
    tag: &str,
) -> Result<(), Error> {
    let after = entry.after.clone().unwrap_or(Observed::Absent);
    let verify = || unchanged(real, &after, &entry.target);
    match &entry.prior {
        Observed::Absent => verify().and_then(|()| disk::remove(real, tag)),
        Observed::Present { sha256 } => {
            let backup = load_backup(context, dir, entry)?;
            if backup.sha256() != *sha256 {
                return Err(Error::BackupCorrupt {
                    run: run.to_owned(),
                    target: label(&entry.target),
                });
            }
            disk::put(real, &exact(backup), tag, verify)
        }
        Observed::Installed => Err(Error::Program {
            target: label(&entry.target),
        }),
    }
}

fn load_backup(context: &Context, dir: &Path, entry: &Entry) -> Result<Disk, Error> {
    let corrupt = |message: &str| Error::Invalid {
        path: dir.to_path_buf(),
        message: format!("backup of {}: {message}", label(&entry.target)),
    };
    let backup = entry
        .backup
        .as_ref()
        .ok_or_else(|| corrupt("missing from the journal"))?;
    let path = dir.join(&backup.path);
    match backup.kind {
        BackupKind::File => {
            let bytes = disk::read_internal(&path, &context.state)?
                .ok_or_else(|| corrupt("the file is missing"))?;
            let mode = *backup.modes.get("").ok_or_else(|| corrupt("no mode"))?;
            Ok(Disk::File(Node { bytes, mode }))
        }
        BackupKind::Tree => {
            let real = path.canonicalize().map_err(disk::io("resolve", &path))?;
            let state = context
                .state
                .canonicalize()
                .map_err(disk::io("resolve", &context.state))?;
            if !real.starts_with(&state) {
                return Err(corrupt("resolves outside the state dir"));
            }
            let Some(Disk::Tree { files, .. }) = disk::read(&real)? else {
                return Err(corrupt("the folder is missing"));
            };
            if files.keys().ne(backup.modes.keys()) {
                return Err(corrupt("its files don't match the journal"));
            }
            let files = files
                .into_iter()
                .map(|(key, node)| {
                    let mode = backup.modes[&key];
                    (
                        key,
                        Node {
                            bytes: node.bytes,
                            mode,
                        },
                    )
                })
                .collect();
            Ok(Disk::Tree {
                dirs: backup.dirs.clone(),
                files,
            })
        }
        BackupKind::Pending => Err(corrupt("it was never taken")),
    }
}

fn exact(disk: Disk) -> Put {
    match disk {
        Disk::File(node) => Put::File(Planned {
            bytes: node.bytes,
            mode: Mode::Exact(node.mode),
        }),
        Disk::Tree { dirs, files } => Put::Tree {
            dirs: dirs
                .into_iter()
                .map(|(key, mode)| (key, Some(mode)))
                .collect::<BTreeMap<_, _>>(),
            files: files
                .into_iter()
                .map(|(key, node)| {
                    (
                        key,
                        Planned {
                            bytes: node.bytes,
                            mode: Mode::Exact(node.mode),
                        },
                    )
                })
                .collect(),
        },
    }
}
