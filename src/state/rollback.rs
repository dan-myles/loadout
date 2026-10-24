use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::disk::{self, Disk, Mode, Planned, Put};
use super::journal::{self, BackupKind, Entry, Journal, Status, Step};
use super::run::{tag, unchanged};
use super::{Error, State, check_reserved, check_target, label, lock, observed};
use crate::context::Context;
use crate::plan::{Observed, Target};
use crate::reserved::{self, Use};

/// What `rollback` restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledBack {
    pub run: String,
    /// Targets put back to what they held before the run, in the order they were restored.
    pub restored: Vec<Target>,
    /// Scratch names next to a target that loadout couldn't verify as this run's own (wrong
    /// type, a link, or content that matches neither side), left in place.
    pub leftovers: Vec<PathBuf>,
}

/// What preflight decided for one journaled action. Everything here was validated before the
/// first target is touched.
struct Undo<'a> {
    index: usize,
    entry: &'a Entry,
    real: PathBuf,
    /// What the target held at preflight; it is checked again just before the restore.
    now: Observed,
    fix: Fix,
    /// The run's scratch siblings, and what each must hold to count as the run's own.
    scratch: Vec<(PathBuf, Option<Observed>)>,
    /// Scratch from an earlier rollback attempt that stopped, verified before reuse of its name.
    rescratch: Vec<(PathBuf, Option<Observed>)>,
}

enum Fix {
    Keep,
    Delete,
    Restore(Put),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scratch {
    Absent,
    /// A real file or folder whose content hash is the expected one.
    Matches,
    /// Anything else: kept, never deleted.
    Foreign,
}

/// Undoes one run, by default the newest: restores each backup, deletes what the run created,
/// and puts the ownership records back. An interrupted run is undone the same way, including a
/// folder swap that stopped between its two renames.
///
/// Everything rollback will need is validated before the first change: the journal's shape,
/// the state file, every target and created folder against the target roots and the reserved
/// paths, every backup against its slot, layout, and hash, and every scratch name. Every target
/// the run touched must still hold what the run left there, or already hold what it had before.
/// Otherwise nothing is restored and the changed targets are listed, so a later run's work or
/// the user's edits are never overwritten. A rollback that stops halfway on an I/O error can be
/// run again: targets already restored are skipped, and its own scratch is verified and reused.
pub fn rollback(context: &Context, run: Option<&str>) -> Result<RolledBack, Error> {
    if fs::symlink_metadata(&context.state).is_err() {
        return Err(Error::NoRuns);
    }
    let lock = lock(context)?;
    let root = lock.state.clone();
    let ids = journal::run_ids(&root)?;
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
    let dir = disk::internal_dir(&root, &[journal::RUNS, &id], false)?;
    let mut journal = Journal::load(&dir, &root)?;
    if journal.status == Status::RolledBack {
        return Err(Error::AlreadyRolledBack { run: id });
    }
    let mut state = State::load(context)?;
    let checkouts = [journal.loadout.clone(), context.state.join("loadout")];

    let mut undos = Vec::new();
    let mut blocked = Vec::new();
    for (index, entry) in journal.entries.iter().enumerate().rev() {
        if !matches!(entry.step, Step::Started | Step::Done | Step::Failed) {
            continue;
        }
        match preflight(context, &root, &dir, &id, index, entry, &checkouts)? {
            Ok(undo) => undos.push(undo),
            Err(problem) => blocked.push(problem),
        }
    }
    if !blocked.is_empty() {
        return Err(Error::RollbackBlocked {
            run: id,
            targets: blocked,
        });
    }

    let mut restored = Vec::new();
    let mut leftovers = Vec::new();
    for undo in &undos {
        let retag = format!("{}-rollback", tag(&id, undo.index));
        clear(&undo.rescratch[..1], &mut leftovers)?;
        let verify = || unchanged(&undo.real, &undo.now, &undo.entry.target);
        match &undo.fix {
            Fix::Keep => {}
            Fix::Delete => verify().and_then(|()| disk::remove(&undo.real, &retag))?,
            Fix::Restore(put) => disk::put(&undo.real, put, &retag, verify)?,
        }
        if !matches!(undo.fix, Fix::Keep) {
            restored.push(undo.entry.target.clone());
        }
        clear(&undo.rescratch[1..], &mut leftovers)?;
        clear(&undo.scratch, &mut leftovers)?;
        for created in undo.entry.created.iter().rev() {
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

    drop(undos);

    for entry in &journal.entries {
        if matches!(
            entry.step,
            Step::Started | Step::Done | Step::Released | Step::Failed
        ) {
            state.set(&entry.target, entry.record.clone());
        }
    }
    state.save(context)?;
    journal.status = Status::RolledBack;
    journal.save(&dir)?;
    Ok(RolledBack {
        run: id,
        restored,
        leftovers,
    })
}

/// Decides one action's undo without changing anything. The outer error is a hard failure
/// (invalid input, I/O); the inner one is a reason the whole rollback is blocked.
fn preflight<'a>(
    context: &Context,
    root: &Path,
    dir: &Path,
    run: &str,
    index: usize,
    entry: &'a Entry,
    checkouts: &[PathBuf],
) -> Result<Result<Undo<'a>, String>, Error> {
    let path = check_target(context, &entry.target)?;
    for checkout in checkouts {
        if let Err(error) = check_reserved(context, checkout, path) {
            return Ok(Err(error.to_string()));
        }
    }
    let real = disk::real(path)?;
    if let Some(innermost) = entry.created.last()
        && Some(innermost.as_path()) != real.parent()
    {
        return Err(invalid(
            dir,
            entry,
            "its created folders don't lead to the target",
        ));
    }
    for created in &entry.created {
        for checkout in checkouts {
            if let Some(conflict) = reserved::check_path(context, checkout, created, Use::WriteInto)
            {
                return Ok(Err(format!(
                    "{}: {}",
                    created.display(),
                    conflict.message()
                )));
            }
        }
    }

    let now = observed(disk::read(&real)?.as_ref());
    let prior = Some(entry.prior.clone()).filter(|prior| *prior != Observed::Absent);
    let scratch_tag = tag(run, index);
    let retag = format!("{scratch_tag}-rollback");
    let scratch = vec![
        (
            disk::scratch(&real, &scratch_tag, "tmp")?,
            entry.after.clone(),
        ),
        (disk::scratch(&real, &scratch_tag, "old")?, prior.clone()),
    ];
    let rescratch = vec![
        (disk::scratch(&real, &retag, "tmp")?, prior.clone()),
        (disk::scratch(&real, &retag, "old")?, entry.after.clone()),
    ];
    let old_holds_prior = inspect(&scratch[1].0, scratch[1].1.as_ref())? == Scratch::Matches;
    let reold_holds_after = inspect(&rescratch[1].0, rescratch[1].1.as_ref())? == Scratch::Matches;

    let swap_stopped =
        now == Observed::Absent && prior.is_some() && (old_holds_prior || reold_holds_after);
    let fix = if now == entry.prior {
        Fix::Keep
    } else if Some(&now) == entry.after.as_ref() || swap_stopped {
        match &entry.prior {
            Observed::Absent => Fix::Delete,
            Observed::Present { sha256 } => {
                let backup = load_backup(root, dir, index, entry)?;
                if backup.sha256() != *sha256 {
                    return Err(Error::BackupCorrupt {
                        run: run.to_owned(),
                        target: label(&entry.target),
                    });
                }
                Fix::Restore(exact(backup))
            }
            Observed::Installed => {
                return Err(Error::Program {
                    target: label(&entry.target),
                });
            }
        }
    } else if entry.step == Step::Failed {
        Fix::Keep
    } else {
        return Ok(Err(format!(
            "{} changed after run {run}",
            label(&entry.target)
        )));
    };
    Ok(Ok(Undo {
        index,
        entry,
        real,
        now,
        fix,
        scratch,
        rescratch,
    }))
}

fn invalid(dir: &Path, entry: &Entry, message: &str) -> Error {
    Error::Invalid {
        path: dir.join(journal::FILE),
        message: format!("{}: {message}", label(&entry.target)),
    }
}

/// Classifies a scratch name without following it: only a real file or folder whose content
/// hashes to `expected` is the run's own.
fn inspect(path: &Path, expected: Option<&Observed>) -> Result<Scratch, Error> {
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Scratch::Absent),
        Err(error) => return Err(disk::io("inspect", path)(error)),
        Ok(metadata) => metadata,
    };
    let regular = metadata.is_file() || metadata.is_dir();
    let Some(expected @ Observed::Present { .. }) = expected else {
        return Ok(Scratch::Foreign);
    };
    if metadata.file_type().is_symlink() || !regular {
        return Ok(Scratch::Foreign);
    }
    match disk::read(path) {
        Ok(found) if observed(found.as_ref()) == *expected => Ok(Scratch::Matches),
        Ok(_) | Err(Error::Special { .. }) => Ok(Scratch::Foreign),
        Err(error) => Err(error),
    }
}

/// Deletes each scratch path that still verifies as the run's own; reports the rest.
fn clear(
    scratch: &[(PathBuf, Option<Observed>)],
    leftovers: &mut Vec<PathBuf>,
) -> Result<(), Error> {
    for (path, expected) in scratch {
        match inspect(path, expected.as_ref())? {
            Scratch::Absent => {}
            Scratch::Matches => disk::remove_scratch(path)?,
            Scratch::Foreign => leftovers.push(path.clone()),
        }
    }
    Ok(())
}

/// Loads the backup in this entry's own slot, `<run>/backups/<index>`, which must be a real
/// file or folder (not a link) inside that folder, with exactly the files, folders, and modes the
/// journal records. The copy keeps the original modes inside the private `backups` folder, so a
/// mode changed in only one of the two places is caught.
fn load_backup(root: &Path, dir: &Path, index: usize, entry: &Entry) -> Result<Disk, Error> {
    let corrupt = |message: &str| invalid(dir, entry, &format!("backup: {message}"));
    let backup = entry
        .backup
        .as_ref()
        .ok_or_else(|| corrupt("missing from the journal"))?;
    let slot = index.to_string();
    let run = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| corrupt("the run folder has no name"))?;
    let backups = disk::internal_dir(root, &[journal::RUNS, run, "backups"], false)?;
    let path = backups.join(&slot);
    if backup.path != format!("backups/{slot}") {
        return Err(corrupt("not in its own slot"));
    }
    let metadata = fs::symlink_metadata(&path).map_err(disk::io("inspect", &path))?;
    if metadata.file_type().is_symlink() {
        return Err(corrupt("is a link"));
    }
    match (backup.kind, disk::read(&path)?) {
        (BackupKind::File, Some(Disk::File(node))) if metadata.is_file() => {
            if backup.modes.get("") != Some(&node.mode) {
                return Err(corrupt("its mode doesn't match the journal"));
            }
            Ok(Disk::File(node))
        }
        (BackupKind::Tree, Some(Disk::Tree { dirs, files })) if metadata.is_dir() => {
            let file_modes: BTreeMap<String, u32> = files
                .iter()
                .map(|(key, node)| (key.clone(), node.mode))
                .collect();
            if file_modes != backup.modes || dirs != backup.dirs {
                return Err(corrupt(
                    "its files, folders, or modes don't match the journal",
                ));
            }
            Ok(Disk::Tree { dirs, files })
        }
        _ => Err(corrupt("missing, or not the kind the journal says")),
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
