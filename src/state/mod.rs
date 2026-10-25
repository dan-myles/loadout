//! Ownership records, the one hash definition, and the journaled safe write (architecture
//! sections 4 and 5).

mod disk;
mod entry;
mod hash;
mod journal;
mod rollback;
mod run;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::manifest::McpFormat;
use crate::mcp;
use crate::plan::{Content, ItemRef, Observed, Target};
use crate::platform::Os;
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
    #[error("{target}: {source}")]
    Mcp {
        target: String,
        #[source]
        source: mcp::Error,
    },
    #[error(
        "{path} is a folder, not the single file an MCP config must be; loadout never reads into it"
    )]
    NotADocument { path: PathBuf },
    #[error(
        "{target} already holds an MCP server loadout doesn't own, with a different value; loadout never overwrites it"
    )]
    UnownedEntry { target: String },
    #[error("{target} is a program; programs are installed by the programs module, never written")]
    Program { target: String },
    #[error(
        "an action with no target reached {operation}; an item with no destination on this machine is an unsupported plan action, never observed, journaled, or written"
    )]
    NoTarget { operation: &'static str },
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
    #[error(
        "the lock is for the state dir {locked}, not {state}; take the lock of the state dir the run writes"
    )]
    WrongLock { locked: PathBuf, state: PathBuf },
    #[error(
        "{path} is not under the home dir, the config dirs, or an agent dir override, where loadout's targets live"
    )]
    OutsideRoots { path: PathBuf },
}

impl Error {
    /// True for outcomes that block the machine rather than break the tool: a target that
    /// changed under loadout, a rollback that would overwrite later changes, a reserved path, or
    /// an MCP config whose content or surroundings loadout won't edit (unparseable, duplicate or
    /// unrepresentable keys, mixed newlines, a Muse schema or Pi name loadout can't write, a
    /// folder in its place, or Grok holding its lock), or a server name that is taken by a value
    /// loadout doesn't own. Contract and I/O faults are not.
    pub fn is_blocked(&self) -> bool {
        match self {
            Error::Changed { .. }
            | Error::RollbackBlocked { .. }
            | Error::Reserved { .. }
            | Error::NotADocument { .. }
            | Error::UnownedEntry { .. } => true,
            Error::Mcp { source, .. } => matches!(
                source,
                mcp::Error::NotUtf8
                    | mcp::Error::Parse { .. }
                    | mcp::Error::NotAnObject { .. }
                    | mcp::Error::Duplicate { .. }
                    | mcp::Error::Value { .. }
                    | mcp::Error::MixedNewlines
                    | mcp::Error::Schema { .. }
                    | mcp::Error::Collision { .. }
                    | mcp::Error::Reserved { .. }
                    | mcp::Error::NotAFile { .. }
                    | mcp::Error::Lock { .. }
            ),
            _ => false,
        }
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
        Target::None => "no target".to_owned(),
    }
}

/// `Target::None` is valid only on an unsupported plan action, which plan filters out before
/// state. State refuses one it is handed anyway, before touching the file system.
pub(super) fn refuse_none(target: &Target, operation: &'static str) -> Result<(), Error> {
    match target {
        Target::None => Err(Error::NoTarget { operation }),
        Target::Path { .. } | Target::Entry { .. } | Target::Program { .. } => Ok(()),
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
            let problem = if !target_shape(&record.target) {
                Some("records a target that is not an absolute clean path")
            } else if let Some(problem) = entry_problem(&record.target) {
                Some(problem)
            } else if !valid_sha(&record.sha256) {
                Some("records an invalid hash")
            } else if !journal::is_run_id(&record.run) {
                Some("records an invalid run id")
            } else {
                None
            };
            if let Some(problem) = problem {
                return Err(Error::Invalid {
                    path,
                    message: format!("{problem} ({})", label(&record.target)),
                });
            }
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
        let path = disk::state_root(context)?.join(STATE_FILE);
        let file = StateFile {
            version: VERSION,
            records: self.records.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|error| Error::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        disk::save(&path, &bytes)
    }
}

/// Held for one plan and apply, or one rollback. Dropping it releases the lock. It remembers
/// the canonical state dir it locks, so it only admits runs on that state dir.
#[derive(Debug)]
pub struct Lock {
    _file: File,
    state: PathBuf,
}

impl Lock {
    /// Whether this lock guards `context`'s state dir, compared the way `Run::begin` compares
    /// them. Read-only: it creates no folder, opens no file, takes no lock, and the lock stays
    /// held, so a caller can check the lock it was handed before work that may never begin a
    /// `Run`. The configured state dir must already exist and resolve to a folder. An alias of
    /// the locked dir passes; another dir is `Error::WrongLock`; a dir that is missing or can't
    /// be resolved is an I/O error, and one that isn't a folder is `Error::Invalid`.
    pub fn check(&self, context: &Context) -> Result<(), Error> {
        let path = &context.state;
        let state = path.canonicalize().map_err(disk::io("resolve", path))?;
        if !fs::metadata(&state).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(Error::Invalid {
                path: path.clone(),
                message: "must be a folder".to_owned(),
            });
        }
        if state != self.state {
            return Err(Error::WrongLock {
                locked: self.state.clone(),
                state,
            });
        }
        Ok(())
    }
}

pub fn lock(context: &Context) -> Result<Lock, Error> {
    let state = disk::state_root(context)?;
    let path = state.join(LOCK_FILE);
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
        Ok(()) => Ok(Lock { _file: file, state }),
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

/// One of the files the background update keeps in the state dir, next to the state and the
/// journals. Only these names can be written or read through [`write_private`] and
/// [`read_private`], so no caller can reach any other path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Private {
    /// `service.toml`: what `loadout service install` recorded for the scheduled job.
    Service,
    /// `background/supervisor.json`: how the last background run's supervisor ended.
    Supervisor,
    /// `background/last-run.json`: what the last background worker did.
    LastRun,
    /// `background/worker.log`: the last background worker's bounded output.
    WorkerLog,
    /// `background/task.xml`: the Windows task definition, staged for `schtasks`.
    TaskXml,
}

impl Private {
    fn parts(self) -> (&'static [&'static str], &'static str) {
        match self {
            Private::Service => (&[], "service.toml"),
            Private::Supervisor => (&["background"], "supervisor.json"),
            Private::LastRun => (&["background"], "last-run.json"),
            Private::WorkerLog => (&["background"], "worker.log"),
            Private::TaskXml => (&["background"], "task.xml"),
        }
    }

    /// Where the file lives for `context`, whether or not it exists.
    pub fn path(self, context: &Context) -> PathBuf {
        let (dirs, name) = self.parts();
        dirs.iter()
            .fold(context.state.clone(), |path, dir| path.join(dir))
            .join(name)
    }
}

/// Writes one of the background update's files atomically, readable only by the user, creating
/// the state dir and its `background` folder (`0o700`) if needed. The same rules as the state
/// file apply: every folder must be a real folder inside the canonical state dir.
pub fn write_private(context: &Context, file: Private, bytes: &[u8]) -> Result<PathBuf, Error> {
    let root = disk::state_root(context)?;
    let (dirs, name) = file.parts();
    let path = disk::internal_dir(&root, dirs, true)?.join(name);
    disk::save(&path, bytes)?;
    Ok(path)
}

/// The folder `file` lives in, checked like every internal folder: each part a real folder, not
/// a link, resolving inside the canonical state dir. `None` when it, or the state dir, doesn't
/// exist.
fn private_dir(context: &Context, file: Private) -> Result<Option<PathBuf>, Error> {
    let root = match context.state.canonicalize() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(disk::io("resolve", &context.state)(error)),
        Ok(root) => root,
    };
    let (dirs, _) = file.parts();
    match disk::internal_dir(&root, dirs, false) {
        Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        other => other.map(Some),
    }
}

/// Reads one of the background update's files: `None` when it, or the state dir, doesn't exist
/// (only a missing path counts as absent; any other error is returned). It must be a regular
/// file itself, never a link, even one to another file in the state dir, in folders that are
/// real folders there.
pub fn read_private(context: &Context, file: Private) -> Result<Option<Vec<u8>>, Error> {
    let Some(dir) = private_dir(context, file)? else {
        return Ok(None);
    };
    let (_, name) = file.parts();
    let path = dir.join(name);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(disk::io("inspect", &path)(error)),
        Ok(metadata) if !metadata.is_file() => {
            return Err(Error::Invalid {
                path,
                message: "must be a regular file, not a link".to_owned(),
            });
        }
        Ok(_) => {}
    }
    disk::read_internal(&path, &context.state)
}

/// Removes one of the background update's files, if it is there. Its folders must pass the same
/// checks as for a read, so a linked folder can't point the removal outside the state dir, and
/// only a regular file is removed; a link or anything else in its place is refused and left
/// alone.
pub fn remove_private(context: &Context, file: Private) -> Result<(), Error> {
    let Some(dir) = private_dir(context, file)? else {
        return Ok(());
    };
    let (_, name) = file.parts();
    let path = dir.join(name);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(disk::io("inspect", &path)(error)),
        Ok(metadata) if metadata.is_file() => {
            fs::remove_file(&path).map_err(disk::io("remove", &path))
        }
        Ok(_) => Err(Error::Invalid {
            path,
            message: "must be a regular file".to_owned(),
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
    refuse_none(target, "observe")?;
    if matches!(target, Target::Entry { .. }) {
        return entry::observe(context, loadout, target);
    }
    let path = check_target(context, target)?;
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
        Target::Entry { .. } => Err(Error::InvalidContent {
            message: format!(
                "{} is an MCP entry, which state edits by key, never as a whole file",
                label(target)
            ),
        }),
        Target::Program { .. } => Err(Error::Program {
            target: label(target),
        }),
        Target::None => Err(Error::NoTarget {
            operation: "a path lookup",
        }),
    }
}

fn clean_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| !matches!(part, Component::CurDir | Component::ParentDir))
}

fn valid_sha(sha256: &str) -> bool {
    sha256.len() == 64
        && sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_observed(observed: &Observed) -> bool {
    match observed {
        Observed::Absent => true,
        Observed::Present { sha256 } => valid_sha(sha256),
        Observed::Installed => false,
    }
}

/// What is wrong with an MCP entry target beyond its path: a pointer `mcp` can't use, an empty
/// key, or a shape written in another format. `None` for a valid entry and for other targets.
/// The shape itself is required by the record format, so an entry without one never loads.
fn entry_problem(target: &Target) -> Option<&'static str> {
    let Target::Entry {
        format,
        pointer,
        key,
        shape,
        ..
    } = target
    else {
        return None;
    };
    if mcp::parse(&[], *format, pointer).is_err() {
        Some("records an MCP entry whose map pointer is invalid")
    } else if key.is_empty() {
        Some("records an MCP entry with an empty key")
    } else if shape.is_toml() != (*format == McpFormat::Toml) {
        Some("records an MCP entry whose shape and format disagree")
    } else {
        None
    }
}

/// The file or folder a path or entry target names must be absolute and clean.
fn target_shape(target: &Target) -> bool {
    match target {
        Target::Path { path } | Target::Entry { file: path, .. } => clean_absolute(path),
        Target::Program { .. } | Target::None => false,
    }
}

/// Where loadout's targets may live: the bases `Context::expand` uses for `~/` and `{config}/`,
/// the XDG config dir some agents use on every OS, and each agent dir override. These are the
/// same roots the resolver and the agent adapters build targets from, so a target outside all
/// of them never came from a loadout.
///
/// Membership compares path identities (`reserved::identity`, the same prefix rules the reserved
/// checks use) by whole components. A target belongs when its lexical form or its physical form
/// (`reserved::normalize`, existing ancestors canonicalized) equals or lies inside the lexical or
/// physical form of a root. So `\\?\C:\Users\a` and `C:\Users\a` are one root, and so are a
/// symlinked spelling of a root and its real path. Names compare case-insensitively on Windows
/// only. A form that can't be normalized adds nothing. A symlink below a root may still lead
/// elsewhere, and the write follows it (section 5).
fn within_roots(context: &Context, path: &Path) -> bool {
    let fold_case = context.os == Os::Windows;
    let forms = |path: &Path| {
        let mut forms = vec![reserved::identity(path, fold_case)];
        if let Ok(real) = reserved::normalize(path) {
            forms.push(reserved::identity(&real, fold_case));
        }
        forms
    };
    let targets = forms(path);
    [&context.home, &context.config, &context.xdg_config]
        .into_iter()
        .chain(context.agent_dirs.values())
        .any(|root| {
            let roots = forms(root);
            targets
                .iter()
                .any(|target| roots.iter().any(|root| target.starts_with(root)))
        })
}

/// The path of a file or tree target, after checking it is absolute, clean, and under a target
/// root. Entries and programs are refused here, before any read.
fn check_target<'a>(context: &Context, target: &'a Target) -> Result<&'a Path, Error> {
    let path = path_of(target)?;
    if !clean_absolute(path) || !within_roots(context, path) {
        return Err(Error::OutsideRoots {
            path: path.to_path_buf(),
        });
    }
    Ok(path)
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
    /// Not checked: a reserved path, an unreadable target, or an MCP config loadout can't parse.
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

    fn scratch_context(state: &Path) -> Context {
        Context::resolve(
            Os::current().unwrap(),
            |name| match name {
                "LOADOUT_HOME" => Some(state.with_file_name("home").into_os_string()),
                "LOADOUT_STATE_DIR" => Some(state.as_os_str().to_owned()),
                _ => None,
            },
            None,
        )
        .unwrap()
    }

    #[test]
    fn private_files_round_trip_and_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        // The state root is resolved before anything is written, so the scratch root is given in
        // its resolved form for the exact path assertion (macOS's /var is /private/var; a Windows
        // temp dir can be a short 8.3 name, resolved to its long verbatim form).
        let root = fs::canonicalize(dir.path()).unwrap();
        let context = scratch_context(&root.join("state"));
        assert_eq!(read_private(&context, Private::LastRun).unwrap(), None);
        let path = write_private(&context, Private::LastRun, b"{}").unwrap();
        assert_eq!(path, Private::LastRun.path(&context));
        assert_eq!(
            read_private(&context, Private::LastRun).unwrap().as_deref(),
            Some(b"{}".as_slice())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        remove_private(&context, Private::LastRun).unwrap();
        remove_private(&context, Private::LastRun).unwrap();
        assert_eq!(read_private(&context, Private::LastRun).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_place_of_a_private_file_or_folder_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let context = scratch_context(&dir.path().join("state"));
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("service.toml"), "planted").unwrap();
        fs::create_dir_all(&context.state).unwrap();
        std::os::unix::fs::symlink(
            outside.join("service.toml"),
            Private::Service.path(&context),
        )
        .unwrap();
        assert!(read_private(&context, Private::Service).is_err());
        assert!(remove_private(&context, Private::Service).is_err());

        std::os::unix::fs::symlink(&outside, context.state.join("background")).unwrap();
        assert!(read_private(&context, Private::Supervisor).is_err());
        assert!(write_private(&context, Private::Supervisor, b"{}").is_err());
        assert!(!outside.join("supervisor.json").exists());
    }

    /// A link at the file itself is refused on read even when it leads to another regular file
    /// inside the state dir; a missing file or folder reads as absent and creates nothing.
    #[cfg(unix)]
    #[test]
    fn a_private_file_that_is_a_link_is_refused_even_inside_the_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let context = scratch_context(&dir.path().join("state"));
        assert_eq!(read_private(&context, Private::LastRun).unwrap(), None);
        assert!(!context.state.exists(), "a read creates nothing");
        fs::create_dir_all(&context.state).unwrap();
        assert_eq!(read_private(&context, Private::LastRun).unwrap(), None);
        assert!(!context.state.join("background").exists(), "nor its folder");
        remove_private(&context, Private::LastRun).unwrap();
        assert!(!context.state.join("background").exists());

        let supervisor = write_private(&context, Private::Supervisor, b"{}").unwrap();
        std::os::unix::fs::symlink(&supervisor, Private::LastRun.path(&context)).unwrap();
        assert!(read_private(&context, Private::LastRun).is_err());
    }

    /// A linked `background` folder that leads outside the state dir is refused before anything
    /// is removed, and so is a dangling link, as the folder or as the file; the outside file
    /// keeps every byte and every link stays where it was.
    #[cfg(unix)]
    #[test]
    fn removal_never_follows_a_linked_or_dangling_folder_or_file() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let context = scratch_context(&dir.path().join("state"));
        fs::create_dir_all(&context.state).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("last-run.json"), "SENTINEL").unwrap();
        let background = context.state.join("background");

        symlink(&outside, &background).unwrap();
        assert!(remove_private(&context, Private::LastRun).is_err());
        assert_eq!(
            fs::read(outside.join("last-run.json")).unwrap(),
            b"SENTINEL"
        );
        fs::remove_file(&background).unwrap();

        symlink(dir.path().join("missing"), &background).unwrap();
        assert!(remove_private(&context, Private::LastRun).is_err());
        assert!(fs::symlink_metadata(&background).is_ok(), "the link stays");
        fs::remove_file(&background).unwrap();

        fs::create_dir(&background).unwrap();
        let leaf = Private::LastRun.path(&context);
        symlink(dir.path().join("missing"), &leaf).unwrap();
        assert!(remove_private(&context, Private::LastRun).is_err());
        assert!(
            fs::symlink_metadata(&leaf).is_ok(),
            "the dangling link stays"
        );
        fs::remove_file(&leaf).unwrap();

        write_private(&context, Private::LastRun, b"{}").unwrap();
        remove_private(&context, Private::LastRun).unwrap();
        assert!(
            fs::symlink_metadata(&leaf).is_err(),
            "a real file is removed"
        );
        assert_eq!(
            fs::read(outside.join("last-run.json")).unwrap(),
            b"SENTINEL"
        );
    }
}
