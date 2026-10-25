use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::disk::{self, Disk, Mode, Planned, Put};
use super::entry::{self, Edit};
use super::journal::{self, BackupKind, Entry, Journal, Staged, Status, Step};
use super::run::{tag, unchanged};
use super::{
    Error, State, check_reserved, check_target, clean_absolute, hash, label, lock, observed,
    within_roots,
};
use crate::context::Context;
use crate::mcp;
use crate::plan::{Observed, Target};
use crate::platform::Os;
use crate::reserved::{self, Part, Use};

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
/// first target is touched. It owns a copy of its journal entry, so rollback can journal an MCP
/// restore's intent while undoing.
struct Undo {
    index: usize,
    entry: Entry,
    real: PathBuf,
    /// What the target (for an MCP entry, its key) held at preflight; checked again just before
    /// the restore.
    now: Observed,
    fix: Fix,
    /// The run's scratch siblings, and what each must hold to count as the run's own.
    scratch: Vec<(PathBuf, Expect)>,
    /// Scratch from an earlier rollback attempt that stopped, verified before reuse of its name.
    rescratch: Vec<(PathBuf, Expect)>,
    /// For an MCP entry, the document bytes preflight read. They only seed `combined`, which
    /// checks this rollback's restores to one document together; the restore never writes them.
    document: Option<Vec<u8>>,
}

enum Fix {
    Keep,
    Delete,
    Restore(Put),
    /// Put an MCP server key back to its backed-up value.
    SetKey(Value),
    /// Remove an MCP server key the run created.
    RemoveKey,
}

/// What a scratch name must hold to be the run's own.
enum Expect {
    /// A file or folder whose content hash is this one (path and tree targets).
    Content(Option<Observed>),
    /// A regular file whose exact bytes have this `hash::document` (an MCP config staged by this
    /// run or this rollback, as journaled before it was staged).
    Bytes(Option<String>),
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
    // Held to the end of the rollback, so every Grok config it may edit stays locked from before
    // the first change until its journal is final.
    let _grok = grok_locks(context, &journal, &checkouts)?;

    let mut undos = Vec::new();
    let mut blocked = Vec::new();
    for (index, entry) in journal.entries.iter().enumerate().rev() {
        if !matches!(entry.step, Step::Started | Step::Done | Step::Failed) {
            continue;
        }
        let decided = if matches!(entry.target, Target::Entry { .. }) {
            preflight_entry(context, &root, &dir, &id, index, entry, &checkouts)?
        } else {
            preflight(context, &root, &dir, &id, index, entry, &checkouts)?
        };
        match decided {
            Ok(undo) => undos.push(undo),
            Err(problem) => blocked.push(problem),
        }
    }
    if blocked.is_empty() {
        blocked = combined(context, &undos)?;
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
            Fix::SetKey(value) => {
                restore_key(&mut journal, &dir, undo, &Edit::Set(value), &retag)?;
            }
            Fix::RemoveKey => {
                restore_key(&mut journal, &dir, undo, &Edit::Remove, &retag)?;
            }
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
fn preflight(
    context: &Context,
    root: &Path,
    dir: &Path,
    run: &str,
    index: usize,
    entry: &Entry,
    checkouts: &[PathBuf],
) -> Result<Result<Undo, String>, Error> {
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
            Expect::Content(entry.after.clone()),
        ),
        (
            disk::scratch(&real, &scratch_tag, "old")?,
            Expect::Content(prior.clone()),
        ),
    ];
    let rescratch = vec![
        (
            disk::scratch(&real, &retag, "tmp")?,
            Expect::Content(prior.clone()),
        ),
        (
            disk::scratch(&real, &retag, "old")?,
            Expect::Content(entry.after.clone()),
        ),
    ];
    let old_holds_prior = inspect(&scratch[1].0, &scratch[1].1)? == Scratch::Matches;
    let retmp = inspect(&rescratch[0].0, &rescratch[0].1)?;
    let reold = inspect(&rescratch[1].0, &rescratch[1].1)?;

    // The run's own `.old` proves a swap that stopped between its renames only while the action
    // never finished. A `done` action already put its result in place, so a `.old` left beside
    // it (its removal failed) says nothing about a later deletion of the target. Rollback's own
    // `.old` holding `after` proves an earlier rollback of this run stopped mid-swap.
    let interrupted = matches!(entry.step, Step::Started | Step::Failed);
    let swap_stopped = now == Observed::Absent
        && prior.is_some()
        && ((interrupted && old_holds_prior) || reold == Scratch::Matches);
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
    // A restore writes through rollback's own scratch names, which must be free or verifiably
    // its own; anything else there would make the write fail after other targets changed.
    if !matches!(fix, Fix::Keep) {
        let in_way = [
            (retmp == Scratch::Foreign, &rescratch[0].0),
            (
                reold == Scratch::Foreign || (reold == Scratch::Matches && now != Observed::Absent),
                &rescratch[1].0,
            ),
        ];
        if let Some((_, path)) = in_way.iter().find(|(blocked, _)| *blocked) {
            return Ok(Err(format!(
                "{} is in the way of restoring {}; it isn't verifiably this rollback's own, so it is left for you to move",
                path.display(),
                label(&entry.target)
            )));
        }
    }
    Ok(Ok(Undo {
        index,
        entry: entry.clone(),
        real,
        now,
        fix,
        scratch,
        rescratch,
        document: None,
    }))
}

/// Decides an MCP entry's undo without changing anything, per key: only the owned key's value
/// decides, so other keys edited since the run never block it and are kept by the restore.
fn preflight_entry(
    context: &Context,
    root: &Path,
    dir: &Path,
    run: &str,
    index: usize,
    entry: &Entry,
    checkouts: &[PathBuf],
) -> Result<Result<Undo, String>, Error> {
    let place = entry::place(&entry.target)?;
    if !clean_absolute(place.file) || !within_roots(context, place.file) {
        return Err(Error::OutsideRoots {
            path: place.file.to_path_buf(),
        });
    }
    for checkout in checkouts {
        if let Err(error) = entry::check(context, checkout, &place) {
            return Ok(Err(error.to_string()));
        }
    }
    let real = disk::real(place.file)?;
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
    let document = match entry::read(&place, &real) {
        Ok(document) => document,
        Err(error) if error.is_blocked() || matches!(error, Error::Special { .. }) => {
            return Ok(Err(error.to_string()));
        }
        Err(error) => return Err(error),
    };
    let now = entry::observed(document.value.as_ref());
    let staged =
        |staged: Option<&Staged>| Expect::Bytes(staged.map(|staged| staged.staged.clone()));
    let scratch_tag = tag(run, index);
    let retag = format!("{scratch_tag}-rollback");
    let scratch = vec![
        (
            disk::scratch(&real, &scratch_tag, "tmp")?,
            staged(entry.document.as_ref()),
        ),
        (
            disk::scratch(&real, &scratch_tag, "old")?,
            Expect::Bytes(None),
        ),
    ];
    let rescratch = vec![
        (
            disk::scratch(&real, &retag, "tmp")?,
            staged(entry.rollback.as_ref()),
        ),
        (disk::scratch(&real, &retag, "old")?, Expect::Bytes(None)),
    ];
    let retmp = inspect(&rescratch[0].0, &rescratch[0].1)?;
    let fix = if now == entry.prior {
        Fix::Keep
    } else if Some(&now) == entry.after.as_ref() {
        match &entry.prior {
            Observed::Absent => Fix::RemoveKey,
            Observed::Present { sha256 } => {
                let value = load_value_backup(root, dir, index, entry)?;
                if hash::entry(&value) != *sha256 {
                    return Err(Error::BackupCorrupt {
                        run: run.to_owned(),
                        target: label(&entry.target),
                    });
                }
                Fix::SetKey(value)
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
    // Whether `mcp` will make the restore's edit is checked by `combined`, with this rollback's
    // other restores to the same document, before anything changes.
    // A restore stages through rollback's `.tmp` name, which must be free or exactly the bytes
    // an earlier attempt journaled. A document write is one rename, so `.old` names are never used.
    if !matches!(fix, Fix::Keep) && retmp == Scratch::Foreign {
        return Ok(Err(format!(
            "{} is in the way of restoring {}; it isn't verifiably this rollback's own, so it is left for you to move",
            rescratch[0].0.display(),
            label(&entry.target)
        )));
    }
    Ok(Ok(Undo {
        index,
        entry: entry.clone(),
        real,
        now,
        fix,
        scratch,
        rescratch,
        document: Some(document.bytes),
    }))
}

/// The edit an MCP entry's fix makes, if any.
fn edit_of(fix: &Fix) -> Option<Edit<'_>> {
    match fix {
        Fix::SetKey(value) => Some(Edit::Set(value)),
        Fix::RemoveKey => Some(Edit::Remove),
        Fix::Keep | Fix::Delete | Fix::Restore(_) => None,
    }
}

/// Checks every MCP restore this rollback will make, per physical document and in the order the
/// restores run, before anything changes. Each document starts from the bytes preflight read;
/// each restore must find its key as preflight saw it and be an edit `mcp` will make on the
/// document as the earlier restores leave it. So a restore that only an earlier one makes
/// impossible (two Pi names that collide once both are back) blocks the whole rollback, not
/// half of it. Documents are told apart by resolved path identity, so every alias of one
/// document is checked as one. The simulated bytes are only checked: each restore reads,
/// re-checks, and edits the document again. The outer error is a hard failure; the list names
/// what blocks.
fn combined(context: &Context, undos: &[Undo]) -> Result<Vec<String>, Error> {
    let fold_case = context.os == Os::Windows;
    // Each physical document's identity, and its simulated bytes; `None` once a restore to it
    // is blocked, since later ones can't be judged.
    let mut documents: Vec<(Vec<Part>, Option<Vec<u8>>)> = Vec::new();
    let mut blocked = Vec::new();
    for undo in undos {
        let (Some(read), Some(edit)) = (&undo.document, edit_of(&undo.fix)) else {
            continue;
        };
        let place = entry::place(&undo.entry.target)?;
        let physical = reserved::normalize(&undo.real).map_err(disk::io("resolve", &undo.real))?;
        let identity = reserved::identity(&physical, fold_case);
        let position = match documents.iter().position(|(known, _)| *known == identity) {
            Some(position) => position,
            None => {
                documents.push((identity, Some(read.clone())));
                documents.len() - 1
            }
        };
        let Some(bytes) = &documents[position].1 else {
            continue;
        };
        let step = entry::value_in(&place, bytes).and_then(|value| {
            if entry::observed(value.as_ref()) == undo.now {
                entry::edit(&place, bytes, &edit)
            } else {
                Err(Error::Changed {
                    target: label(&undo.entry.target),
                })
            }
        });
        match step {
            Ok(next) => documents[position].1 = Some(next),
            Err(error) if error.is_blocked() => {
                blocked.push(error.to_string());
                documents[position].1 = None;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(blocked)
}

/// Restores one MCP server key on the document as it is now, so keys others changed since the
/// run are kept: read, re-check the key against preflight, edit those current bytes, journal the
/// staged bytes' identity, then the atomic write that re-reads the whole document before its
/// rename. Grok's lock is already held by `rollback`.
fn restore_key(
    journal: &mut Journal,
    dir: &Path,
    undo: &Undo,
    edit: &Edit<'_>,
    retag: &str,
) -> Result<(), Error> {
    let place = entry::place(&undo.entry.target)?;
    let current = entry::read(&place, &undo.real)?;
    if entry::observed(current.value.as_ref()) != undo.now {
        return Err(Error::Changed {
            target: label(&undo.entry.target),
        });
    }
    let bytes = entry::edit(&place, &current.bytes, edit)?;
    if bytes == current.bytes {
        return Ok(());
    }
    journal.entries[undo.index].rollback = Some(Staged {
        base: hash::document(&current.bytes),
        staged: hash::document(&bytes),
    });
    journal.save(dir)?;
    entry::put(&undo.real, &current, bytes, retag, &undo.entry.target)
}

/// Takes every Grok lock this rollback may need, before anything is changed: one per distinct
/// Grok config among the MCP entries it could restore, aliases of one config counted once, in
/// the order of their identity (the same identity that says two are one), so two rollbacks
/// spelling the same homes differently still take them in one order. An entry whose policy
/// check fails is left to preflight, which blocks on it. A lock that can't be found or taken
/// ends the rollback with nothing changed, releasing any already taken.
/// Taking a lock may create Grok's home and its `.config-init.lock`.
fn grok_locks(
    context: &Context,
    journal: &Journal,
    checkouts: &[PathBuf],
) -> Result<Vec<mcp::Lock>, Error> {
    let mut ids: Vec<(mcp::LockId, &Target)> = Vec::new();
    for entry in &journal.entries {
        if !matches!(entry.step, Step::Started | Step::Done | Step::Failed)
            || !matches!(entry.target, Target::Entry { .. })
        {
            continue;
        }
        let place = entry::place(&entry.target)?;
        if checkouts
            .iter()
            .any(|checkout| entry::check(context, checkout, &place).is_err())
        {
            continue;
        }
        let id = match mcp::lock_id(context, &journal.loadout, place.file) {
            Ok(Some(id)) => id,
            Ok(None) => continue,
            Err(source) => return Err(entry::mcp_error(&entry.target, source)),
        };
        if !ids.iter().any(|(known, _)| *known == id) {
            ids.push((id, &entry.target));
        }
    }
    ids.sort_by(|(a, _), (b, _)| a.cmp(b));
    ids.iter()
        .map(|(id, target)| mcp::acquire(id).map_err(|source| entry::mcp_error(target, source)))
        .collect()
}

fn invalid(dir: &Path, entry: &Entry, message: &str) -> Error {
    Error::Invalid {
        path: dir.join(journal::FILE),
        message: format!("{}: {message}", label(&entry.target)),
    }
}

/// Classifies a scratch name without following it. A link is never the run's own, and is
/// refused before anything reads through it. Otherwise only a real file or folder matching
/// `expected` is: by content hash for paths and trees, by exact bytes for an MCP config.
fn inspect(path: &Path, expected: &Expect) -> Result<Scratch, Error> {
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Scratch::Absent),
        Err(error) => return Err(disk::io("inspect", path)(error)),
        Ok(metadata) => metadata,
    };
    if metadata.file_type().is_symlink() {
        return Ok(Scratch::Foreign);
    }
    match expected {
        Expect::Content(expected) => {
            let Some(expected @ Observed::Present { .. }) = expected else {
                return Ok(Scratch::Foreign);
            };
            if !(metadata.is_file() || metadata.is_dir()) {
                return Ok(Scratch::Foreign);
            }
            match disk::read(path) {
                Ok(found) if observed(found.as_ref()) == *expected => Ok(Scratch::Matches),
                Ok(_) | Err(Error::Special { .. }) => Ok(Scratch::Foreign),
                Err(error) => Err(error),
            }
        }
        Expect::Bytes(staged) => {
            let Some(staged) = staged else {
                return Ok(Scratch::Foreign);
            };
            if !metadata.is_file() {
                return Ok(Scratch::Foreign);
            }
            match disk::read_document(path) {
                Ok(Some(node)) if hash::document(&node.bytes) == *staged => Ok(Scratch::Matches),
                Ok(_)
                | Err(Error::Special { .. } | Error::NotADocument { .. } | Error::Changed { .. }) => {
                    Ok(Scratch::Foreign)
                }
                Err(error) => Err(error),
            }
        }
    }
}

/// Deletes each scratch path that still verifies as the run's own; reports the rest.
fn clear(scratch: &[(PathBuf, Expect)], leftovers: &mut Vec<PathBuf>) -> Result<(), Error> {
    for (path, expected) in scratch {
        match inspect(path, expected)? {
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

/// Loads an MCP key's backed-up value from this entry's own slot: a non-link regular file in this
/// run's `backups` folder holding one JSON value.
fn load_value_backup(root: &Path, dir: &Path, index: usize, entry: &Entry) -> Result<Value, Error> {
    let corrupt = |message: &str| invalid(dir, entry, &format!("backup: {message}"));
    let backup = entry
        .backup
        .as_ref()
        .ok_or_else(|| corrupt("missing from the journal"))?;
    if backup.kind != BackupKind::Entry {
        return Err(corrupt("not an MCP key backup"));
    }
    let slot = index.to_string();
    if backup.path != format!("backups/{slot}") {
        return Err(corrupt("not in its own slot"));
    }
    let run = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| corrupt("the run folder has no name"))?;
    let backups = disk::internal_dir(root, &[journal::RUNS, run, "backups"], false)?;
    let path = backups.join(&slot);
    let metadata = fs::symlink_metadata(&path).map_err(disk::io("inspect", &path))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(corrupt("is not a regular file"));
    }
    let bytes = disk::read_internal(&path, &backups)?.ok_or_else(|| corrupt("is missing"))?;
    serde_json::from_slice(&bytes).map_err(|_| corrupt("is not one JSON value"))
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::manifest::{McpFormat, McpShape};
    use crate::plan::{Change, Content, ItemKind, ItemRef};
    use crate::state::digest;

    /// A run that created the Cursor key `one` as `{"command": "ours"}` in a new document, with
    /// the forward intent a real run journals, saved and read back through `Journal::load` so the
    /// public validator accepts it; and the undo preflight decides for it: remove the key, which
    /// preflight saw holding that value. Returns the journal, its folder, and the undo.
    fn created(root: &Path) -> (Journal, PathBuf, Undo) {
        let file = root.join("home").join("mcp.json");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let target = Target::Entry {
            file: file.clone(),
            format: McpFormat::Json,
            pointer: "/mcpServers".to_owned(),
            key: "one".to_owned(),
            shape: McpShape::CursorJson,
        };
        let value = json!({"command": "ours"});
        let written =
            entry::edit(&entry::place(&target).unwrap(), b"", &Edit::Set(&value)).unwrap();
        let planned = digest(&Content::Entry(value)).unwrap();
        let (run, started) = journal::now();
        let dir = root.join(journal::RUNS).join(&run);
        fs::create_dir_all(&dir).unwrap();
        let entry = Entry {
            item: ItemRef {
                kind: ItemKind::Mcp,
                id: "one".to_owned(),
                agent: None,
            },
            target,
            change: Change::Create,
            prior: Observed::Absent,
            record: None,
            backup: None,
            step: Step::Done,
            after: Some(planned.clone()),
            created: Vec::new(),
            error: None,
            document: Some(Staged {
                base: hash::document(b""),
                staged: hash::document(&written),
            }),
            rollback: None,
        };
        Journal::new(run, started, root.join("checkout"), vec![entry])
            .save(&dir)
            .unwrap();
        let journal = Journal::load(&dir, root).unwrap();
        let undo = Undo {
            index: 0,
            entry: journal.entries[0].clone(),
            real: file,
            now: planned,
            fix: Fix::RemoveKey,
            scratch: Vec::new(),
            rescratch: Vec::new(),
            document: None,
        };
        (journal, dir, undo)
    }

    // The race preflight can't see: the owned key changes after preflight, before its restore.
    #[test]
    fn a_key_changed_after_preflight_is_never_restored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut journal, run, undo) = created(&root);
        let theirs = "{\"mcpServers\":{\"one\":{\"command\":\"theirs\"}}}\n";
        fs::write(&undo.real, theirs).unwrap();
        let saved = fs::read(run.join(journal::FILE)).unwrap();
        let retag = format!("{}-rollback", tag(&journal.run, 0));
        let error = restore_key(&mut journal, &run, &undo, &Edit::Remove, &retag).unwrap_err();
        assert!(matches!(error, Error::Changed { .. }), "{error}");
        assert_eq!(fs::read_to_string(&undo.real).unwrap(), theirs);
        assert!(journal.entries[0].rollback.is_none());
        assert_eq!(
            fs::read(run.join(journal::FILE)).unwrap(),
            saved,
            "nothing journaled"
        );
    }

    // An unrelated key added after preflight is kept: the restore edits the bytes it reads now.
    #[test]
    fn an_unrelated_edit_after_preflight_is_kept_by_the_restore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut journal, run, undo) = created(&root);
        let before =
            "{\"mcpServers\":{\"one\":{\"command\":\"ours\"},\"later\":{\"command\":\"x\"}}}\n";
        fs::write(&undo.real, before).unwrap();
        let retag = format!("{}-rollback", tag(&journal.run, 0));
        restore_key(&mut journal, &run, &undo, &Edit::Remove, &retag).unwrap();
        let after = fs::read(&undo.real).unwrap();
        let document: Value = serde_json::from_slice(&after).unwrap();
        assert_eq!(document, json!({"mcpServers": {"later": {"command": "x"}}}));
        let saved = Journal::load(&run, &root).unwrap();
        let intent = saved.entries[0].rollback.as_ref().unwrap();
        assert_eq!(intent.base, hash::document(before.as_bytes()));
        assert_eq!(intent.staged, hash::document(&after));
    }
}
