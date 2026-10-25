//! Loadout's own clone of the loadout, kept with the system `git` (architecture section 8), and
//! the background update that keeps it current: the scheduled job (`service`) and the records a
//! background run leaves for `status` and `doctor` (`background`).
//!
//! The clone lives at `<state>/loadout`. `clone` and `fetch` take the state lock by reference,
//! so the checkout can't move while another run reads it; the caller then hands that same lock
//! to `apply::run_under_lock`. No credential is captured, stored, or passed: a URL with a
//! password is refused, and every URL loadout shows has its userinfo hidden.

pub mod background;
mod git;
pub mod service;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::diagnostic::{Check, CheckStatus, Diagnostics, Severity};
use crate::machine;
use crate::manifest::Loadout;
use crate::resolve::{self, Update};
use crate::state::{self, RunStatus};

use git::Git;
pub use git::redact;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("git is not on PATH; install git and try again")]
    NoGit,
    #[error(
        "the URL carries a password; loadout stores no credential, so give git a credential helper or an SSH key instead"
    )]
    Password,
    #[error("{path} already exists, and loadout only clones into an empty place")]
    Exists { path: PathBuf },
    #[error("there is no loadout clone at {path}; run `loadout init <URL>` first")]
    NoClone { path: PathBuf },
    #[error("{path} {reason}")]
    NotAClone { path: PathBuf, reason: &'static str },
    #[error(
        "the cloned repository has no branch checked out; clone a repository whose default branch is set"
    )]
    Detached,
    #[error("{action} failed: {detail}")]
    Git { action: String, detail: String },
    #[error("cannot {action} {path}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    State(#[from] state::Error),
    #[error("cannot read this machine's facts")]
    Machine(#[source] machine::Error),
    #[error("{} problem(s) in the loadout for this machine", .0.count(Severity::Error))]
    Invalid(Diagnostics),
    #[error(transparent)]
    Service(#[from] service::Error),
    #[error("{path} is missing; the background launcher runs loadout.exe from its own folder")]
    MissingWorker { path: PathBuf },
}

/// What `fetch` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum Fetched {
    /// The clone already had its upstream's commit.
    UpToDate { head: String },
    /// The clone fast-forwarded from `from` to `to`.
    Advanced { from: String, to: String },
    /// The clone's branch and working tree were left as they were. A block found after the
    /// fetch (`Ahead`, `Diverged`) has updated the clone's view of origin, never its branch.
    Blocked { reason: Blocked },
}

/// Why `fetch` left the clone's branch alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Blocked {
    /// Tracked edits, or files git doesn't know.
    Dirty,
    /// A merge, rebase, cherry-pick, revert, or bisect that isn't finished.
    InProgress,
    /// No branch is checked out.
    Detached,
    /// The branch doesn't track a branch of `origin`.
    NoUpstream,
    /// The clone has commits the remote doesn't; loadout never pushes.
    Ahead,
    /// Both have commits the other lacks; loadout only fast-forwards.
    Diverged,
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Blocked::Dirty => {
                "the clone has local changes; commit, stash, or discard them in the clone"
            }
            Blocked::InProgress => {
                "a merge, rebase, cherry-pick, revert, or bisect is unfinished in the clone"
            }
            Blocked::Detached => "the clone has no branch checked out",
            Blocked::NoUpstream => "the clone's branch doesn't track a branch of origin",
            Blocked::Ahead => {
                "the clone has commits the remote doesn't have, and loadout never pushes"
            }
            Blocked::Diverged => {
                "the clone and the remote have both moved on, and loadout only fast-forwards"
            }
        })
    }
}

/// Loadout's own clone: `<state>/loadout`, the checkout `plan`, `apply`, and `update` read by
/// default.
pub fn checkout(context: &Context) -> PathBuf {
    context.state.join("loadout")
}

/// Clones `url` into `<state>/loadout`, through a temporary folder in the state dir that is
/// renamed into place only once the clone has a branch checked out. `lock` must be the state
/// lock for `context`. A failed clone removes only its own temporary folder.
///
/// In the foreground git inherits the terminal, so a credential helper or SSH key can ask the
/// user; nothing it is given is stored by loadout.
pub fn clone(url: &str, context: &Context, lock: &state::Lock) -> Result<(), Error> {
    lock.check(context)?;
    if git::has_password(url) {
        return Err(Error::Password);
    }
    let state = canonical_state(context)?;
    let destination = state.join("loadout");
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(Error::Exists { path: destination });
    }
    let git = Git::find(context, &state)?;
    let temp = state.join(format!(
        ".loadout-clone-{}-{}",
        std::process::id(),
        stamp_nanos()
    ));
    if fs::symlink_metadata(&temp).is_ok() {
        return Err(Error::Exists { path: temp });
    }
    let cloned = clone_into(&git, url, &temp, &destination);
    if cloned.is_err() && fs::symlink_metadata(&temp).is_ok_and(|metadata| metadata.is_dir()) {
        let _ = fs::remove_dir_all(&temp);
    }
    cloned
}

/// The clone itself, into `temp`, then renamed to `destination` once it has a branch.
fn clone_into(git: &Git<'_>, url: &str, temp: &Path, destination: &Path) -> Result<(), Error> {
    git.change(
        None,
        &[
            "clone".into(),
            "--no-recurse-submodules".into(),
            "--origin".into(),
            "origin".into(),
            "--".into(),
            url.into(),
            temp.as_os_str().to_owned(),
        ],
    )?;
    let branch = git.read(temp, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if !branch.ok() {
        return Err(Error::Detached);
    }
    fs::rename(temp, destination).map_err(|source| Error::Io {
        action: "move the clone into",
        path: destination.to_path_buf(),
        source,
    })
}

/// Fetches origin and fast-forwards the clone's branch to its upstream. Anything that would
/// need more than a fast-forward, or that would touch local work, leaves the branch and the
/// working tree as they are and reports why. Checked in order: no branch, no upstream on origin,
/// an unfinished merge or similar, local changes; then the fetch; then ahead or diverged.
///
/// `lock` must be the state lock for `context`.
pub fn fetch(context: &Context, lock: &state::Lock) -> Result<Fetched, Error> {
    lock.check(context)?;
    let state = canonical_state(context)?;
    let repo = own_clone(&state)?;
    let git = Git::find(context, &state)?;
    let blocked = |reason| Ok(Fetched::Blocked { reason });

    if let Some(reason) = local_block(&git, &repo)? {
        return blocked(reason);
    }

    git.change(
        Some(repo.as_path()),
        &[
            "fetch".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "--prune".into(),
            "origin".into(),
        ],
    )?;
    let from = git.value(&repo, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let to = git.value(&repo, &["rev-parse", "--verify", "@{u}^{commit}"])?;
    if from == to {
        return Ok(Fetched::UpToDate { head: from });
    }
    if ancestor(&git, &repo, &from, &to)? {
        git.change(
            Some(repo.as_path()),
            &[
                "merge".into(),
                "--ff-only".into(),
                "--no-edit".into(),
                to.clone().into(),
            ],
        )?;
        let now = git.value(&repo, &["rev-parse", "--verify", "HEAD^{commit}"])?;
        if now != to {
            return Err(Error::Git {
                action: "git merge".to_owned(),
                detail: format!("the clone is at {now} after fast-forwarding to {to}"),
            });
        }
        return Ok(Fetched::Advanced { from, to });
    }
    if ancestor(&git, &repo, &to, &from)? {
        blocked(Blocked::Ahead)
    } else {
        blocked(Blocked::Diverged)
    }
}

/// The `[update]` settings the loadout in `checkout` gives this machine, read the way `plan`
/// reads the loadout: guarded load, this machine's facts, resolve.
pub fn settings(context: &Context, checkout: &Path) -> Result<Update, Error> {
    let (loadout, _) = Loadout::load_guarded(checkout, context).map_err(Error::Invalid)?;
    let facts = machine::detect(context).map_err(Error::Machine)?;
    let (resolved, _) = resolve::resolve(&loadout, &facts).map_err(Error::Invalid)?;
    Ok(resolved.update)
}

/// Why no new run may start on this machine yet, if one may not: the newest run didn't finish
/// (or another run is going), or its journal can't be read. `update` checks this before it
/// fetches, so a blocked machine's clone doesn't move either.
pub fn unfinished(context: &Context) -> Result<Option<String>, Error> {
    let runs = state::runs(context)?;
    Ok(match runs.last() {
        Some(run)
            if matches!(
                run.status,
                Some(RunStatus::Interrupted | RunStatus::InProgress)
            ) =>
        {
            Some(format!(
                "run {} didn't finish; if no other loadout run is going, run `loadout rollback` to undo it first",
                run.id
            ))
        }
        Some(run) if run.status.is_none() => Some(format!(
            "the journal of run {} can't be read ({}), so loadout can't tell whether it finished",
            run.id,
            run.error.as_deref().unwrap_or("unknown error")
        )),
        _ => None,
    })
}

/// Doctor's view of the clone, the service, and the last background run. It reads only: local
/// git queries (no fetch, no index refresh), loadout's own files, and scheduler queries.
pub fn checks(context: &Context) -> Vec<Check> {
    let mut checks = vec![checkout_check(context)];
    checks.extend(service::checks(context));
    checks.push(background::check(context));
    checks
}

fn checkout_check(context: &Context) -> Check {
    let name = "checkout";
    let path = checkout(context);
    if fs::symlink_metadata(&path).is_err() {
        return Check::new(
            name,
            CheckStatus::Info,
            format!(
                "no loadout clone at {}; `loadout init <URL>` makes one",
                path.display()
            ),
        );
    }
    let Ok(state) = context.state.canonicalize() else {
        return Check::new(name, CheckStatus::Warn, "cannot resolve the state dir");
    };
    let repo = match own_clone(&state) {
        Ok(repo) => repo,
        Err(error) => return Check::new(name, CheckStatus::Fail, error.to_string()),
    };
    let git = match Git::find(context, &state) {
        Ok(git) => git,
        Err(error) => return Check::new(name, CheckStatus::Fail, error.to_string()),
    };
    let origin = git
        .read(&repo, &["config", "--get", "remote.origin.url"])
        .ok()
        .filter(git::Answer::ok)
        .map_or_else(|| "no origin".to_owned(), |answer| redact(answer.line()));
    let local = local_block(&git, &repo);
    match local {
        Ok(None) => Check::new(
            name,
            CheckStatus::Pass,
            format!("{} from {origin}, clean", path.display()),
        ),
        Ok(Some(reason)) => Check::new(
            name,
            CheckStatus::Warn,
            format!(
                "{} from {origin}: {reason}; `loadout update` won't move it",
                path.display()
            ),
        ),
        Err(error) => Check::new(name, CheckStatus::Warn, error.to_string()),
    }
}

/// What blocks a fetch before it starts, from local state only (no fetch, no index refresh), in
/// order: no branch, no upstream on origin, an unfinished merge or similar, local changes.
fn local_block(git: &Git<'_>, repo: &Path) -> Result<Option<Blocked>, Error> {
    if !git
        .read(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?
        .ok()
    {
        return Ok(Some(Blocked::Detached));
    }
    let upstream = git.read(
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )?;
    if !upstream.ok() || !upstream.line().starts_with("origin/") {
        return Ok(Some(Blocked::NoUpstream));
    }
    if in_progress(repo) {
        return Ok(Some(Blocked::InProgress));
    }
    let status = [
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--ignored=no",
    ];
    let changes = git.read(repo, &status)?;
    if !changes.ok() {
        return Err(git::failed(&status, &changes));
    }
    Ok((!changes.stdout.is_empty()).then_some(Blocked::Dirty))
}

/// The canonical state dir, which a held lock guarantees exists.
fn canonical_state(context: &Context) -> Result<PathBuf, Error> {
    context.state.canonicalize().map_err(|source| Error::Io {
        action: "resolve",
        path: context.state.clone(),
        source,
    })
}

/// `<state>/loadout`, only when it is a real folder (not a link) that resolves inside the
/// canonical state dir and holds a real `.git` folder.
fn own_clone(state: &Path) -> Result<PathBuf, Error> {
    let path = state.join("loadout");
    let metadata = match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(Error::NoClone { path });
        }
        Err(source) => {
            return Err(Error::Io {
                action: "inspect",
                path,
                source,
            });
        }
        Ok(metadata) => metadata,
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::NotAClone {
            path,
            reason: "must be a real folder inside the state dir, not a link or a file",
        });
    }
    if path.canonicalize().ok().as_deref() != Some(path.as_path()) {
        return Err(Error::NotAClone {
            path,
            reason: "resolves outside the state dir",
        });
    }
    let dot_git = path.join(".git");
    if !fs::symlink_metadata(&dot_git)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        return Err(Error::NotAClone {
            path,
            reason: "has no .git folder of its own, so it isn't loadout's clone",
        });
    }
    Ok(path)
}

/// An operation git started and didn't finish, read from the clone's own `.git` folder.
fn in_progress(repo: &Path) -> bool {
    [
        "MERGE_HEAD",
        "REBASE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
    ]
    .iter()
    .any(|name| fs::symlink_metadata(repo.join(".git").join(name)).is_ok())
}

fn ancestor(git: &Git<'_>, repo: &Path, older: &str, newer: &str) -> Result<bool, Error> {
    let args = ["merge-base", "--is-ancestor", older, newer];
    let answer = git.read(repo, &args)?;
    match answer.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(git::failed(&args, &answer)),
    }
}

fn stamp_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

/// Seconds since the Unix epoch, for loadout's own records.
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
