use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::disk::{self, Disk, Mode, Node, Planned, Put};
use super::entry::{self, Edit};
use super::journal::{self, Backup, BackupKind, Entry, Journal, Staged, Status, Step};
use super::{
    Error, Lock, Record, State, check_reserved, clean_absolute, digest, entry_problem, hash, label,
    observed, path_of, refuse_none, tree_files, valid_observed, within_roots,
};
use crate::context::Context;
use crate::mcp;
use crate::plan::{Action, Change, Content, Observed, Plan, Target};

/// One apply: the journal in `<state>/runs/<id>/`, its backups, and the lock it holds.
#[derive(Debug)]
pub struct Run {
    context: Context,
    loadout: PathBuf,
    dir: PathBuf,
    journal: Journal,
    index: BTreeMap<Target, usize>,
    lock: Lock,
}

/// What `Run::remove` did with an owned target the loadout no longer declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// It still held what loadout wrote, so it was backed up and deleted.
    Removed,
    /// It had changed since loadout wrote it, or was already gone, so it was left as it is and
    /// loadout stopped owning it.
    Released,
}

fn journaled(change: &Change) -> bool {
    matches!(
        change,
        Change::Create
            | Change::Update
            | Change::Adopt
            | Change::Replace
            | Change::Drifted
            | Change::Remove
    )
}

pub(super) fn tag(run: &str, index: usize) -> String {
    format!("{run}-{index}")
}

impl Run {
    /// Writes the journal before anything else is touched. It lists every action that may write
    /// (create, update, adopt, replace, drifted for `--force`, remove), with its `prior`, the
    /// ownership record it had, and where its backup will go. Programs are never journaled.
    /// `loadout` is the active checkout, kept reserved for every write and for rollback.
    pub fn begin(
        context: &Context,
        loadout: &Path,
        lock: Lock,
        state: &State,
        plan: &Plan,
    ) -> Result<Run, Error> {
        for action in &plan.actions {
            refuse_none(&action.target, "begin")?;
        }
        let root = disk::state_root(context)?;
        if root != lock.state {
            return Err(Error::WrongLock {
                locked: lock.state.clone(),
                state: root,
            });
        }
        let mut index = BTreeMap::new();
        let mut entries = Vec::new();
        for action in &plan.actions {
            if !journaled(&action.change) || matches!(action.target, Target::Program { .. }) {
                continue;
            }
            let (Target::Path { path } | Target::Entry { file: path, .. }) = &action.target else {
                continue;
            };
            if !clean_absolute(path) || !within_roots(context, path) {
                return Err(Error::OutsideRoots { path: path.clone() });
            }
            if !valid_observed(&action.prior) {
                return Err(Error::InvalidContent {
                    message: format!("{} has an invalid prior", label(&action.target)),
                });
            }
            if let Some(problem) = entry_problem(&action.target) {
                return Err(Error::InvalidContent {
                    message: format!("{}: {problem}", label(&action.target)),
                });
            }
            if matches!(action.target, Target::Entry { .. }) && action.change == Change::Replace {
                return Err(Error::WrongChange {
                    target: label(&action.target),
                    change: "replace".to_owned(),
                    operation: "an MCP entry write, which never overwrites a server loadout doesn't own",
                });
            }
            let position = entries.len();
            if index.insert(action.target.clone(), position).is_some() {
                return Err(Error::Duplicate {
                    target: label(&action.target),
                });
            }
            entries.push(Entry {
                item: action.item.clone(),
                target: action.target.clone(),
                change: action.change.clone(),
                prior: action.prior.clone(),
                record: state.record(&action.target).cloned(),
                backup: matches!(action.prior, Observed::Present { .. }).then(|| Backup {
                    path: format!("backups/{position}"),
                    kind: BackupKind::Pending,
                    modes: BTreeMap::new(),
                    dirs: BTreeMap::new(),
                }),
                step: Step::Pending,
                after: None,
                created: Vec::new(),
                error: None,
                document: None,
                rollback: None,
            });
        }
        let runs = disk::internal_dir(&root, &[journal::RUNS], true)?;
        let (id, started, dir) = loop {
            let (id, started) = journal::now();
            match fs::symlink_metadata(runs.join(&id)) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(disk::io("inspect", &runs.join(&id))(error)),
                Ok(_) => continue,
            }
            let dir = disk::internal_dir(&root, &[journal::RUNS, &id], true)?;
            break (id, started, dir);
        };
        disk::internal_dir(&root, &[journal::RUNS, &id, "backups"], true)?;
        let journal = Journal::new(id, started, loadout.to_path_buf(), entries);
        journal.save(&dir)?;
        Ok(Run {
            context: context.clone(),
            loadout: loadout.to_path_buf(),
            dir,
            journal,
            index,
            lock,
        })
    }

    pub fn id(&self) -> &str {
        &self.journal.run
    }

    fn entry(&self, action: &Action, removing: bool) -> Result<usize, Error> {
        let index = *self
            .index
            .get(&action.target)
            .ok_or_else(|| Error::NotJournaled {
                target: label(&action.target),
            })?;
        let entry = &self.journal.entries[index];
        if entry.step != Step::Pending {
            return Err(Error::AlreadyRan {
                target: label(&action.target),
            });
        }
        if (entry.change == Change::Remove) != removing {
            return Err(Error::WrongChange {
                target: label(&action.target),
                change: format!("{:?}", entry.change).to_lowercase(),
                operation: if removing { "remove" } else { "write" },
            });
        }
        Ok(index)
    }

    /// The safe write in architecture section 5. The target is re-observed first; if it no
    /// longer matches the plan's `prior`, this returns `Error::Changed` and writes nothing.
    pub fn write(&mut self, action: &Action, content: &Content) -> Result<(), Error> {
        refuse_none(&action.target, "write")?;
        let index = self.entry(action, false)?;
        if matches!(action.target, Target::Entry { .. }) {
            return self.write_entry(index, action, content);
        }
        let path = path_of(&action.target)?.to_path_buf();
        if matches!(content, Content::Entry(_)) {
            return Err(Error::InvalidContent {
                message: format!(
                    "{} is a path; an MCP entry can't be written to it",
                    path.display()
                ),
            });
        }
        let after = digest(content)?;
        check_reserved(&self.context, &self.loadout, &path)?;
        let real = disk::real(&path)?;
        let current = disk::read(&real)?;
        let prior = self.journal.entries[index].prior.clone();
        if observed(current.as_ref()) != prior {
            return Err(Error::Changed {
                target: label(&action.target),
            });
        }
        if after == prior {
            let entry = &mut self.journal.entries[index];
            entry.step = Step::Done;
            entry.after = Some(after);
            return self.journal.save(&self.dir);
        }
        let put = planned(content, current.as_ref())?;
        self.start(index, current.as_ref(), after, &real)?;
        let tag = tag(&self.journal.run, index);
        let result = disk::put(&real, &put, &tag, || {
            unchanged(&real, &prior, &action.target)
        });
        let expected = self.journal.entries[index].after.clone();
        self.settle(index, result, || landed(&real, expected.as_ref()))
    }

    /// Deletes an owned target the loadout no longer declares, after backing it up. A target
    /// that drifted from its record, or is already gone, is released instead and left in place.
    pub fn remove(&mut self, action: &Action) -> Result<Removal, Error> {
        refuse_none(&action.target, "remove")?;
        let index = self.entry(action, true)?;
        if matches!(action.target, Target::Entry { .. }) {
            return self.remove_entry(index, action);
        }
        let path = path_of(&action.target)?.to_path_buf();
        let Some(record) = self.journal.entries[index].record.clone() else {
            return Err(Error::NotOwned {
                target: label(&action.target),
            });
        };
        check_reserved(&self.context, &self.loadout, &path)?;
        let real = disk::real(&path)?;
        let current = disk::read(&real)?;
        let prior = self.journal.entries[index].prior.clone();
        if observed(current.as_ref()) != prior {
            return Err(Error::Changed {
                target: label(&action.target),
            });
        }
        let owned = matches!(&prior, Observed::Present { sha256 } if *sha256 == record.sha256);
        if !owned {
            self.journal.entries[index].step = Step::Released;
            self.journal.save(&self.dir)?;
            return Ok(Removal::Released);
        }
        self.start(index, current.as_ref(), Observed::Absent, &real)?;
        let tag = tag(&self.journal.run, index);
        let result =
            unchanged(&real, &prior, &action.target).and_then(|()| disk::remove(&real, &tag));
        let expected = self.journal.entries[index].after.clone();
        self.settle(index, result, || landed(&real, expected.as_ref()))
            .map(|()| Removal::Removed)
    }

    /// One MCP server key, as one transaction (architecture section 5): the KeyMerge check, Grok's
    /// lock held to the end, one read, the key's prior, a backup of that key's value only, the
    /// edit, the staged document's hash journaled, then the atomic write that re-reads the whole
    /// document just before its rename.
    fn write_entry(
        &mut self,
        index: usize,
        action: &Action,
        content: &Content,
    ) -> Result<(), Error> {
        let Content::Entry(value) = content else {
            return Err(Error::InvalidContent {
                message: format!(
                    "{} is an MCP entry; only an entry value can be written to it",
                    label(&action.target)
                ),
            });
        };
        let place = entry::place(&action.target)?;
        entry::check(&self.context, &self.loadout, &place)?;
        let real = disk::real(place.file)?;
        let after = digest(content)?;
        let _grok = mcp::lock(&self.context, &self.loadout, place.file)
            .map_err(|source| entry::mcp_error(&action.target, source))?;
        let document = entry::read(&place, &real)?;
        let prior = self.journal.entries[index].prior.clone();
        if entry::observed(document.value.as_ref()) != prior {
            return Err(Error::Changed {
                target: label(&action.target),
            });
        }
        if after == prior {
            return self.done_without_write(index, after);
        }
        // Ownership is the supplied record, whatever the action is labeled: a key that exists,
        // has no record, and differs from what is wanted belongs to someone else.
        if prior != Observed::Absent && self.journal.entries[index].record.is_none() {
            return Err(Error::UnownedEntry {
                target: label(&action.target),
            });
        }
        let bytes = entry::edit(&place, &document.bytes, &Edit::Set(value))?;
        self.start_entry(index, &document, after, &real, &bytes)?;
        let tag = tag(&self.journal.run, index);
        let result = entry::put(&real, &document, bytes, &tag, &action.target);
        let expected = self.journal.entries[index].after.clone();
        self.settle(index, result, || {
            entry::holds(&place, &real, expected.as_ref())
        })
    }

    /// Removes an owned MCP server key the loadout no longer declares, in the same transaction
    /// as `write_entry`. A key that drifted from its record, or is already gone, is released.
    fn remove_entry(&mut self, index: usize, action: &Action) -> Result<Removal, Error> {
        let Some(record) = self.journal.entries[index].record.clone() else {
            return Err(Error::NotOwned {
                target: label(&action.target),
            });
        };
        let place = entry::place(&action.target)?;
        entry::check(&self.context, &self.loadout, &place)?;
        let real = disk::real(place.file)?;
        let _grok = mcp::lock(&self.context, &self.loadout, place.file)
            .map_err(|source| entry::mcp_error(&action.target, source))?;
        let document = entry::read(&place, &real)?;
        let prior = self.journal.entries[index].prior.clone();
        if entry::observed(document.value.as_ref()) != prior {
            return Err(Error::Changed {
                target: label(&action.target),
            });
        }
        let owned = matches!(&prior, Observed::Present { sha256 } if *sha256 == record.sha256);
        if !owned {
            self.journal.entries[index].step = Step::Released;
            self.journal.save(&self.dir)?;
            return Ok(Removal::Released);
        }
        let bytes = entry::edit(&place, &document.bytes, &Edit::Remove)?;
        self.start_entry(index, &document, Observed::Absent, &real, &bytes)?;
        let tag = tag(&self.journal.run, index);
        let result = entry::put(&real, &document, bytes, &tag, &action.target);
        let expected = self.journal.entries[index].after.clone();
        self.settle(index, result, || {
            entry::holds(&place, &real, expected.as_ref())
        })
        .map(|()| Removal::Removed)
    }

    fn done_without_write(&mut self, index: usize, after: Observed) -> Result<(), Error> {
        let entry = &mut self.journal.entries[index];
        entry.step = Step::Done;
        entry.after = Some(after);
        self.journal.save(&self.dir)
    }

    /// Backs up the owned key's value, if it had one, and journals the staged document's
    /// identity before anything is staged.
    fn start_entry(
        &mut self,
        index: usize,
        document: &entry::Document,
        after: Observed,
        real: &Path,
        staged: &[u8],
    ) -> Result<(), Error> {
        if let Some(value) = &document.value {
            let backup = self.journal.entries[index].backup.clone().ok_or_else(|| {
                Error::InvalidContent {
                    message: format!("{} has no backup slot", real.display()),
                }
            })?;
            let taken = take_value_backup(&self.dir, &backup.path, value)?;
            self.journal.entries[index].backup = Some(taken);
        }
        let entry = &mut self.journal.entries[index];
        entry.step = Step::Started;
        entry.after = Some(after);
        entry.created = real.parent().map(disk::missing_dirs).unwrap_or_default();
        entry.document = Some(Staged {
            base: hash::document(&document.bytes),
            staged: hash::document(staged),
        });
        self.journal.save(&self.dir)
    }

    /// Takes the backup and records the planned result before the target is touched.
    fn start(
        &mut self,
        index: usize,
        current: Option<&Disk>,
        after: Observed,
        real: &Path,
    ) -> Result<(), Error> {
        if let Some(current) = current {
            let backup = self.journal.entries[index].backup.clone().ok_or_else(|| {
                Error::InvalidContent {
                    message: format!("{} has no backup slot", real.display()),
                }
            })?;
            let taken = take_backup(&self.dir, &backup.path, current)?;
            self.journal.entries[index].backup = Some(taken);
        }
        let entry = &mut self.journal.entries[index];
        entry.step = Step::Started;
        entry.after = Some(after);
        entry.created = real.parent().map(disk::missing_dirs).unwrap_or_default();
        self.journal.save(&self.dir)
    }

    /// Marks the action from what the target actually holds, so a step that failed after the
    /// target changed (such as removing the moved-aside old folder) is still journaled as done,
    /// recorded, and undone by rollback.
    fn settle(
        &mut self,
        index: usize,
        result: Result<(), Error>,
        landed: impl FnOnce() -> bool,
    ) -> Result<(), Error> {
        let reached = result.is_ok() || landed();
        let entry = &mut self.journal.entries[index];
        match result {
            Ok(()) => {
                entry.step = Step::Done;
                self.journal.save(&self.dir)
            }
            Err(error) if reached => {
                entry.step = Step::Done;
                entry.error = Some(error.to_string());
                self.journal.save(&self.dir)?;
                Err(error)
            }
            Err(error) => {
                entry.step = Step::Failed;
                entry.error = Some(error.to_string());
                for dir in entry.created.iter().rev() {
                    if fs::remove_dir(dir).is_err() {
                        break;
                    }
                }
                self.journal.save(&self.dir)?;
                Err(error)
            }
        }
    }

    /// Records ownership for every action that reached the disk, drops the records of removed
    /// and released targets, and marks the run finished. Actions that never ran keep their
    /// old records. Releases the state lock.
    pub fn finish(self, state: &mut State) -> Result<(), Error> {
        self.finish_with_lock(state).1
    }

    /// `finish`, but hands back the state lock, whether or not finishing succeeded, so the caller
    /// can keep everything after the file actions (program installs, the report) under the same
    /// lock without taking it again.
    pub fn finish_with_lock(mut self, state: &mut State) -> (Lock, Result<(), Error>) {
        let result = self.close(state);
        (self.lock, result)
    }

    fn close(&mut self, state: &mut State) -> Result<(), Error> {
        for entry in &self.journal.entries {
            match (entry.step, &entry.change, &entry.after) {
                (Step::Done, Change::Remove, _) | (Step::Released, _, _) => {
                    state.set(&entry.target, None);
                }
                (Step::Done, _, Some(Observed::Present { sha256 })) => state.set(
                    &entry.target,
                    Some(Record {
                        target: entry.target.clone(),
                        item: entry.item.clone(),
                        sha256: sha256.clone(),
                        run: self.journal.run.clone(),
                    }),
                ),
                _ => {}
            }
        }
        state.save(&self.context)?;
        self.journal.status = Status::Finished;
        self.journal.save(&self.dir)
    }
}

/// Whether a path target now holds the planned result.
fn landed(real: &Path, expected: Option<&Observed>) -> bool {
    disk::read(real).is_ok_and(|now| Some(&observed(now.as_ref())) == expected)
}

/// Re-reads the target just before it is replaced, so a change in the last moment still aborts.
pub(super) fn unchanged(real: &Path, expected: &Observed, target: &Target) -> Result<(), Error> {
    if observed(disk::read(real)?.as_ref()) == *expected {
        Ok(())
    } else {
        Err(Error::Changed {
            target: label(target),
        })
    }
}

fn planned(content: &Content, current: Option<&Disk>) -> Result<Put, Error> {
    match content {
        Content::File(file) => Ok(Put::File(Planned {
            bytes: file.bytes.clone(),
            mode: Mode::Executable {
                base: match current {
                    Some(Disk::File(node)) => Some(node.mode),
                    _ => None,
                },
                executable: file.executable,
            },
        })),
        Content::Tree(files) => {
            let (old_dirs, old_files) = match current {
                Some(Disk::Tree { dirs, files }) => (Some(dirs), Some(files)),
                _ => (None, None),
            };
            let mut dirs = BTreeMap::from([(String::new(), None)]);
            let mut planned = BTreeMap::new();
            for (key, file) in tree_files(files)? {
                let mut parent = key.as_str();
                while let Some((head, _)) = parent.rsplit_once('/') {
                    dirs.insert(head.to_owned(), None);
                    parent = head;
                }
                let base = old_files
                    .and_then(|old| old.get(&key))
                    .map(|node| node.mode);
                planned.insert(
                    key,
                    Planned {
                        bytes: file.bytes.clone(),
                        mode: Mode::Executable {
                            base,
                            executable: file.executable,
                        },
                    },
                );
            }
            for (key, mode) in &mut dirs {
                *mode = old_dirs.and_then(|old| old.get(key)).copied();
            }
            Ok(Put::Tree {
                dirs,
                files: planned,
            })
        }
        Content::Entry(_) => Err(Error::InvalidContent {
            message: "an MCP entry is not file content".to_owned(),
        }),
    }
}

/// Backs up one MCP server's value as canonical JSON, private, in this action's own slot. The
/// shared document holding it is never copied, so other servers' and the agent's own keys (such
/// as Claude's `oauthAccount`) never reach the state dir.
fn take_value_backup(run: &Path, relative: &str, value: &Value) -> Result<Backup, Error> {
    let path = run.join(relative);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(Error::Invalid {
            path,
            message: "a backup is already in this slot; loadout never overwrites one".to_owned(),
        });
    }
    let planned = Planned {
        bytes: hash::canonical_json(value),
        mode: Mode::Exact(0o600),
    };
    disk::put(&path, &Put::File(planned), "backup", || Ok(()))?;
    Ok(Backup {
        path: relative.to_owned(),
        kind: BackupKind::Entry,
        modes: BTreeMap::new(),
        dirs: BTreeMap::new(),
    })
}

fn take_backup(run: &Path, relative: &str, current: &Disk) -> Result<Backup, Error> {
    let path = run.join(relative);
    if fs::symlink_metadata(&path).is_ok() {
        return Err(Error::Invalid {
            path,
            message: "a backup is already in this slot; loadout never overwrites one".to_owned(),
        });
    }
    let exact = |node: &Node| Planned {
        bytes: node.bytes.clone(),
        mode: Mode::Exact(node.mode),
    };
    let backup = match current {
        Disk::File(node) => {
            disk::put(&path, &Put::File(exact(node)), "backup", || Ok(()))?;
            Backup {
                path: relative.to_owned(),
                kind: BackupKind::File,
                modes: BTreeMap::from([(String::new(), node.mode)]),
                dirs: BTreeMap::new(),
            }
        }
        Disk::Tree { dirs, files } => {
            let put = Put::Tree {
                dirs: dirs
                    .iter()
                    .map(|(key, mode)| (key.clone(), Some(*mode)))
                    .collect(),
                files: files
                    .iter()
                    .map(|(key, node)| (key.clone(), exact(node)))
                    .collect(),
            };
            disk::put(&path, &put, "backup", || Ok(()))?;
            Backup {
                path: relative.to_owned(),
                kind: BackupKind::Tree,
                modes: files
                    .iter()
                    .map(|(key, node)| (key.clone(), node.mode))
                    .collect(),
                dirs: dirs.clone(),
            }
        }
    };
    Ok(backup)
}
