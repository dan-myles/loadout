//! Subprocesses loadout can stop as a whole: bounded manager queries and installs (built by U7
//! inside `programs`, moved here unchanged in behaviour once the background supervisor became a
//! second user), and the background run's supervisor and worker.
//!
//! This is a floor module: it imports only `platform`-level crates (`nix` on Unix,
//! `process-wrap` and `loadout-winproc` on Windows), never `context`, `programs`, `plan`, or
//! `state`.

mod background;
#[cfg(test)]
pub(crate) mod owned;
mod query;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use background::{Ended, Error, LOG_CAP, Limits, Log, Outcome, Refused, handshake, supervise};
pub(crate) use query::{Captured, QueryError, install, query};

/// The environment variable that carries a worker's half of the handshake nonce. It is removed
/// from every query and install the worker starts.
pub(crate) const WORKER_NONCE: &str = "LOADOUT_WORKER_NONCE";

/// Git variables that would point a git process, or one a manager runs, at another repository,
/// index, or object store than the one on its command line. None is inherited by a subprocess
/// loadout starts: loadout names its own clone explicitly, and a variable left in the caller's
/// environment (by a git hook or a script, say) must not redirect a read or a write.
///
/// The transient configuration variables (`GIT_CONFIG_COUNT` with its `GIT_CONFIG_KEY_<n>` and
/// `GIT_CONFIG_VALUE_<n>`, and `GIT_CONFIG_PARAMETERS`) are deliberately not here: they carry
/// the user's credential helper, extra headers, proxy, and URL rewrites, which managers and
/// loadout's own git need. Git applies an explicit `-c` after them, so the settings loadout's
/// own git calls pass with `-c` still win.
pub(crate) const GIT_REDIRECTS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_QUARANTINE_PATH",
    "GIT_TEMPLATE_DIR",
    "GIT_EXEC_PATH",
    "GIT_PREFIX",
    "GIT_CONFIG",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// One manager command: an exact program path, its argv, and fixed extra environment. Never a
/// shell line.
pub(crate) struct Invocation {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: &'static [(&'static str, &'static str)],
}

impl Invocation {
    pub(crate) fn new<I, S>(program: &Path, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: program.to_path_buf(),
            args: args.into_iter().map(Into::into).collect(),
            env: &[],
        }
    }

    pub(crate) fn display(&self) -> String {
        std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A run's cancellation flag. Callers outside this crate can only ask: the setter is
/// crate-private. Inside loadout it is set by the worker's control reader (the supervisor's
/// soft deadline, or its pipe closing), by a failed query in a supervised run, and by
/// `programs` when a manager's answer can't be classified.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The result of a successful [`handshake`]: this process's startup was consistent with a
/// background worker that `supervise` started, and its control reader is armed.
///
/// It is not proof of provenance. Its fields are private, so Rust code can only obtain one
/// through `handshake`; but a same-user caller that deliberately supplies a matching nonce,
/// pipe, parent and group (or a job) passes the handshake without `supervise` (the plan's
/// same-user adversary is a non-goal). The Unix group it records is the one the process
/// verified and belongs to, and the only one it ever signals.
///
/// ```compile_fail
/// let built = loadout::process::Supervised { cancel: Default::default() };
/// ```
#[derive(Debug)]
pub struct Supervised {
    cancel: Cancel,
    #[cfg(unix)]
    group: nix::unistd::Pid,
}

impl Supervised {
    pub fn cancel(&self) -> &Cancel {
        &self.cancel
    }
}

/// How the queries and installs of this run are contained.
///
/// - `OwnGroup`: the foreground. Each query gets its own group (Unix) or job (Windows) and is
///   settled on its own, exactly as U7 built it.
/// - `Inherited`: a background worker after its handshake. On Unix a query stays in the group
///   the worker was started in (with `supervise`, the anchor-held group its final group kill
///   covers); on Windows its own job nests inside the job the worker runs in (with `supervise`,
///   the kill-on-close job). A failed query or manager answer loadout can't classify cancels
///   the run, and a cancelled run starts no query or installer.
#[derive(Debug, Clone, Default)]
pub enum Containment {
    #[default]
    OwnGroup,
    Inherited(Arc<Supervised>),
}

impl Containment {
    pub fn cancel(&self) -> Option<&Cancel> {
        match self {
            Containment::OwnGroup => None,
            Containment::Inherited(supervised) => Some(supervised.cancel()),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel().is_some_and(Cancel::is_cancelled)
    }
}

#[cfg(test)]
impl Containment {
    /// A background containment without a handshake, for unit tests of code that only reads it
    /// (cancellation, inherited queries). Its group is the test process's own and no control
    /// reader is armed, so nothing ever signals that group.
    pub(crate) fn inherited_for_tests() -> Containment {
        Containment::Inherited(Arc::new(Supervised {
            cancel: Cancel::default(),
            #[cfg(unix)]
            group: nix::unistd::getpgrp(),
        }))
    }
}

impl PartialEq for Containment {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Containment::OwnGroup, Containment::OwnGroup) => true,
            (Containment::Inherited(one), Containment::Inherited(other)) => Arc::ptr_eq(one, other),
            _ => false,
        }
    }
}

impl Eq for Containment {}
