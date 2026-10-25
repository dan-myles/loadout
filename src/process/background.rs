//! The background run: a supervisor process that starts the worker inside a process tree it
//! owns, bounds the whole run, and stops and confirms that tree after every worker exit; and
//! the worker's side, which acts only after a cooperative startup check.
//!
//! The startup check is consistency between cooperating processes, not authentication: the
//! plan's same-user adversary is a non-goal, and a same-user caller that deliberately supplies a
//! matching nonce, pipe, parent and group (or job) passes it without `supervise`. What it does
//! guarantee is that a malformed or mismatched startup refuses before arming anything, and that
//! the only group a Unix worker ever signals is the one it verified and still belongs to.
//!
//! - Unix: the worker joins a fresh process group whose id a loadout-owned anchor holds, the
//!   same anchor and settle as a query (`query::Query`). Queries the worker runs stay in that
//!   group (`Containment::Inherited`).
//! - Windows: the worker is created suspended, placed in a job the supervisor owns (kill on
//!   close, no breakaway), and only then resumed. Queries nest their own jobs inside it.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::process::{ChildStdin, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use std::process::Child;

#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(unix)]
use nix::unistd::{Pid, getpgid, getpgrp, getpid, getppid};

#[cfg(unix)]
use super::QueryError;
use super::query::POLL;
#[cfg(unix)]
use super::query::{Query, Trace};
#[cfg(windows)]
use super::query::{first, until};
use super::{Cancel, Supervised, WORKER_NONCE};

/// The bounds of one background run, measured from the worker's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The worker is asked to stop before its next action.
    pub soft: Duration,
    /// The whole tree is killed.
    pub hard: Duration,
    /// One shared budget for stopping and confirming the tree, and for draining its output.
    pub settle: Duration,
}

impl Limits {
    pub const BACKGROUND: Limits = Limits {
        soft: Duration::from_mins(55),
        hard: Duration::from_mins(60),
        settle: Duration::from_secs(30),
    };

    /// Checks the limits as `supervise` does before it starts anything: every budget is longer
    /// than zero, `soft` is not after `hard`, and the furthest instant a run can compute (its
    /// setup deadline, then its hard deadline, then a fresh settle, plus `HEADROOM`) is
    /// representable from `now`.
    ///
    /// # Errors
    ///
    /// Why the limits are refused.
    pub fn check(self, now: Instant) -> Result<(), String> {
        if self.soft.is_zero() || self.hard.is_zero() || self.settle.is_zero() {
            return Err("every limit must be longer than zero".to_owned());
        }
        if self.soft > self.hard {
            return Err("the soft limit must not come after the hard limit".to_owned());
        }
        let span = self
            .settle
            .checked_add(self.hard)
            .and_then(|span| span.checked_add(self.settle))
            .and_then(|span| span.checked_add(HEADROOM))
            .ok_or_else(|| "the limits are too long to add up".to_owned())?;
        now.checked_add(span).map(|_| ()).ok_or_else(|| {
            "the limits reach past what this system's clock can represent".to_owned()
        })
    }
}

/// How far past a run's furthest planned deadline the clock must still reach, so that the
/// fresh `Instant`s taken later in the run (after scheduling delays) still add up.
const HEADROOM: Duration = Duration::from_secs(24 * 60 * 60);

/// `now + duration`, or `now` when the clock can't represent it: a spent budget, which makes
/// the step that uses it a single attempt that fails honestly rather than a panic. Validated
/// limits keep a day of headroom, so this fallback only covers a clock that jumped further.
fn after(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(duration).unwrap_or(now)
}

/// How much of the worker's combined stdout and stderr is kept: the last bytes.
pub const LOG_CAP: usize = 1024 * 1024;

/// The worker's output, bounded, ready for a private state file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    /// The kept bytes, with a leading note when bytes were dropped and a trailing one when the
    /// log is incomplete. Never more than `LOG_CAP` plus those notes.
    pub bytes: Vec<u8>,
    /// How many earlier bytes the cap dropped.
    pub dropped: u64,
    /// Both streams reached EOF without a read error. `false` means a process still held the
    /// output open when the settle limit ran out (one that left the owned group or job, say),
    /// or a read failed. A drainer blocked that way is not cancelled: it keeps running, holding
    /// its bounded buffer, until the supervisor process exits.
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    /// The worker exited by itself. `code` is `None` when a signal ended it. `cancelled` says the
    /// soft deadline had asked it to stop first.
    Exited { code: Option<i32>, cancelled: bool },
    /// The hard deadline passed, so the tree was killed.
    TimedOut { cancelled: bool },
    /// The worker's exit could not be observed; the tree was killed.
    Lost(String),
}

/// Everything a finished supervision knows, for the caller to record.
#[derive(Debug)]
pub struct Outcome {
    pub ended: Ended,
    /// `Ok` only when the containment the supervisor owns was confirmed empty within the settle
    /// limit and the worker's output reached EOF: on Unix the anchor-held process group (signal
    /// 0 answers `ESRCH`), on Windows the kill-on-close job (no active process). A process that
    /// left that containment is not covered: on Unix one that called `setsid`, on Windows one
    /// created outside the job (a UAC-brokered child, for example). If such a process also
    /// closed its output, nothing here can see it.
    pub cleanup: Result<(), String>,
    pub log: Log,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Refused before anything was started: no process, no nonce, no pipe.
    #[error("refusing to start the background worker: invalid limits: {0}")]
    InvalidLimits(String),
    #[error("cannot start the background worker: {0}")]
    Setup(String),
    #[error(
        "cannot start the background worker ({cause}), and loadout could not confirm that it and everything it started stopped: {detail}"
    )]
    Cleanup { cause: String, detail: String },
}

/// A worker whose startup checks failed. It must exit at once: nothing was locked, written, or
/// signalled, and its control reader was never armed.
#[derive(Debug, thiserror::Error)]
#[error("refusing to run as a background worker: {0}")]
pub struct Refused(String);

/// `CREATE_NO_WINDOW` from the Windows process creation flags.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const MAGIC: &str = "LOADOUT-WORKER";
const VERSION: &str = "1";
const NONCE_BYTES: usize = 32;
const MAX_LINE: usize = 256;
const HANDSHAKE_WAIT: Duration = Duration::from_secs(5);
const CANCEL: &[u8] = b"c";

/// Supervises one background run of `worker`, which must call [`handshake`] first.
///
/// The supervisor sets the worker's stdin (its control pipe), stdout and stderr (drained into a
/// bounded log), and the handshake nonce in its environment. It sends the handshake line, asks
/// the worker to stop at `limits.soft` (one byte on the control pipe), and kills the whole tree
/// at `limits.hard`. However the worker ends, by itself or not, the tree is then stopped and
/// confirmed empty within `limits.settle`. Only the containment the supervisor owns is
/// confirmed (see [`Outcome::cleanup`]); a process that escaped it is reported only through
/// output it still holds.
///
/// `limits` are checked first ([`Limits::check`]); invalid ones are refused before any process,
/// nonce, or pipe exists. Every later deadline is computed with checked arithmetic.
///
/// # Errors
///
/// `InvalidLimits` before anything started. Otherwise the worker could not be started inside
/// its tree: whatever had started was stopped, and `Cleanup` says that could not be confirmed.
pub fn supervise(mut worker: Command, limits: Limits) -> Result<Outcome, Error> {
    let now = Instant::now();
    limits.check(now).map_err(Error::InvalidLimits)?;
    let setup = now
        .checked_add(limits.settle)
        .ok_or_else(|| Error::InvalidLimits("the setup limit is not representable".to_owned()))?;
    let nonce = nonce().map_err(|error| Error::Setup(format!("cannot make a nonce: {error}")))?;
    worker
        .env(WORKER_NONCE, &nonce)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run(worker, &nonce, setup, limits)
}

#[cfg(unix)]
fn run(worker: Command, nonce: &str, setup: Instant, limits: Limits) -> Result<Outcome, Error> {
    let mut trace = Trace::default();
    let mut tree = Query::spawn_in_group(worker, setup, &mut trace).map_err(from_query)?;
    // The worker runs from its spawn, so its soft and hard deadlines count from here, not from
    // the end of setup.
    let started = Instant::now();
    let sink = Sink::shared();
    let mut control = tree.take_stdin();
    let line = handshake_line(nonce, tree.group().as_raw());
    if let Err(cause) = start(&mut tree, &sink, &mut control, &line, setup) {
        disposing(setup, setup);
        let stopped = tree.settle_with(|| finished(&sink), setup, limits.settle, &mut trace);
        return Err(match stopped {
            Ok(()) => Error::Setup(cause),
            Err(detail) => Error::Cleanup { cause, detail },
        });
    }
    let ended = watch(|| tree.try_exit(), &mut control, started, limits);
    let limit = after(limits.settle);
    disposing(setup, limit);
    let cleanup = tree.settle_with(|| finished(&sink), limit, limits.settle, &mut trace);
    drop(control);
    Ok(Outcome {
        ended,
        cleanup,
        log: log(&sink),
    })
}

/// Everything between the worker's spawn and its watch: both drainers and the handshake line.
/// The worker is owned before this runs, so any failure here is settled by the caller with the
/// original setup deadline.
///
/// Setup succeeds only if it finished before that deadline: it is checked on entry and again
/// once the line is sent. A clock check and the steps around it aren't atomic, so a step can
/// finish just after the deadline; that is then a setup timeout, never a success with a fresh
/// budget.
#[cfg(unix)]
fn start(
    tree: &mut Query,
    sink: &Shared,
    control: &mut Option<ChildStdin>,
    line: &str,
    setup: Instant,
) -> Result<(), String> {
    #[cfg(test)]
    seam::hit(&seam::Point::BeforeStart, setup)?;
    within(setup)?;
    #[cfg(test)]
    seam::hit(&seam::Point::AfterSpawn, setup)?;
    drain(tree.take_stdout(), sink)?;
    #[cfg(test)]
    seam::hit(&seam::Point::BetweenDrainers, setup)?;
    drain(tree.take_stderr(), sink)?;
    #[cfg(test)]
    seam::hit(&seam::Point::BeforeSend, setup)?;
    send(control, line.as_bytes())?;
    within(setup)
}

/// `Err` with the setup-timeout cause once `setup` has passed.
#[cfg(unix)]
fn within(setup: Instant) -> Result<(), String> {
    if Instant::now() >= setup {
        Err("its setup did not finish within the setup limit, so loadout stopped it".to_owned())
    } else {
        Ok(())
    }
}

/// Test-only: records the setup deadline and the limit actually given to the disposal, so a
/// control can check a setup failure's cleanup got no renewed budget.
#[cfg(test)]
fn disposing(setup: Instant, limit: Instant) {
    seam::record(setup, limit);
}

#[cfg(not(test))]
fn disposing(_setup: Instant, _limit: Instant) {}

#[cfg(unix)]
fn from_query(error: QueryError) -> Error {
    match error {
        QueryError::Cleanup { cause, detail } => Error::Cleanup {
            cause: cause.map_or_else(|| "it did not start".to_owned(), |cause| cause.to_string()),
            detail,
        },
        other => Error::Setup(other.to_string()),
    }
}

/// Windows: created suspended, placed in a job loadout owns, then resumed within the setup
/// deadline, in that order, so nothing the worker runs is ever outside the job. Until the job
/// holds it, the worker never ran, so stopping that one process is the whole cleanup; after
/// that, every failure stops the whole job within the same, unrenewed deadline.
#[cfg(windows)]
fn run(mut worker: Command, nonce: &str, setup: Instant, limits: Limits) -> Result<Outcome, Error> {
    // No console window: the scheduled launcher has none, and a console program it starts
    // would otherwise open one at every run.
    worker.creation_flags(loadout_winproc::CREATE_SUSPENDED.0 | CREATE_NO_WINDOW);
    let mut child = worker
        .spawn()
        .map_err(|error| Error::Setup(format!("cannot start it: {error}")))?;
    let job = match join(&child, setup) {
        Ok(job) => job,
        Err(error) => {
            let cause = format!("cannot place it in a job loadout owns: {error}");
            disposing(setup, setup);
            return Err(match stop_unstarted(&mut child, setup, limits.settle) {
                Ok(()) => Error::Setup(cause),
                Err(detail) => Error::Cleanup { cause, detail },
            });
        }
    };
    let sink = Sink::shared();
    let mut control = child.stdin.take();
    let line = handshake_line(nonce, 0);
    let started = match start(&mut child, &sink, &mut control, &line, setup) {
        Ok(started) => started,
        Err(cause) => {
            disposing(setup, setup);
            let stopped = settle_job(&job, &mut child, &sink, setup, limits.settle);
            return Err(match stopped {
                Ok(()) => Error::Setup(cause),
                Err(detail) => Error::Cleanup { cause, detail },
            });
        }
    };
    let ended = watch(|| child.try_wait(), &mut control, started, limits);
    let limit = after(limits.settle);
    disposing(setup, limit);
    let cleanup = settle_job(&job, &mut child, &sink, limit, limits.settle);
    drop(control);
    drop(job);
    Ok(Outcome {
        ended,
        cleanup,
        log: log(&sink),
    })
}

/// A new kill-on-close job holding the suspended worker. On an error the worker may or may not
/// be in that job (see `Job::assign`); the job is dropped and the caller stops the worker.
#[cfg(windows)]
fn join(child: &Child, setup: Instant) -> io::Result<loadout_winproc::Job> {
    #[cfg(test)]
    seam::hit(&seam::Point::BeforeMembership, setup).map_err(io::Error::other)?;
    #[cfg(not(test))]
    let _ = setup;
    let job = loadout_winproc::Job::new()?;
    job.assign(child)?;
    Ok(job)
}

/// Everything between job membership and the watch: drainers, the handshake line, and the
/// resume. The job owns the worker before this runs, so any failure here is settled by the
/// caller as a whole job, with the original setup deadline. Returns when the worker started
/// running (its resume), which its soft and hard deadlines count from.
#[cfg(windows)]
fn start(
    child: &mut Child,
    sink: &Shared,
    control: &mut Option<ChildStdin>,
    line: &str,
    setup: Instant,
) -> Result<Instant, String> {
    #[cfg(test)]
    seam::hit(&seam::Point::AfterMembership, setup)?;
    drain(child.stdout.take(), sink)?;
    #[cfg(test)]
    seam::hit(&seam::Point::BetweenDrainers, setup)?;
    drain(child.stderr.take(), sink)?;
    send(control, line.as_bytes())?;
    #[cfg(test)]
    seam::hit(&seam::Point::BeforeResume, setup)?;
    let started = resume(child, setup)?;
    #[cfg(test)]
    seam::hit(&seam::Point::AfterResume, setup)?;
    Ok(started)
}

/// Resumes the suspended worker, never knowingly after `setup`: the deadline is checked on
/// entry, then by the helper after all its preparation as the last step before `ResumeThread`,
/// and again once the resume returned (the clock check and the resume aren't atomic), so a
/// resume that returned late is a setup timeout. Returns when the resume returned.
#[cfg(windows)]
fn resume(child: &Child, setup: Instant) -> Result<Instant, String> {
    if Instant::now() >= setup {
        return Err("its setup limit passed, so loadout did not resume it".to_owned());
    }
    let prepared =
        loadout_winproc::prepare(child).map_err(|error| format!("cannot resume it: {error}"))?;
    #[cfg(test)]
    seam::hit(&seam::Point::AfterPrepare, setup)?;
    prepared
        .resume_before(setup)
        .map_err(|error| format!("cannot resume it: {error}"))?;
    let resumed = Instant::now();
    #[cfg(test)]
    seam::hit(&seam::Point::AfterResumeCall, setup)?;
    if Instant::now() >= setup {
        return Err(
            "it was resumed only as its setup limit passed, so loadout stopped it".to_owned(),
        );
    }
    Ok(resumed)
}

/// Stops a worker that was never resumed, so never ran: kill and reap that one process. The
/// kill is skipped only when the worker is positively known to have exited; an observation
/// that fails still leads to a checked kill. Every problem is kept, in order: the failed
/// observation first, then the kill's and the reap's outcomes.
#[cfg(windows)]
fn stop_unstarted(child: &mut Child, limit: Instant, grace: Duration) -> Result<(), String> {
    let mut problems = Vec::new();
    #[cfg(test)]
    let observed = seam::observation().map_or_else(|| child.try_wait(), Err);
    #[cfg(not(test))]
    let observed = child.try_wait();
    match observed {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => problems.push(format!("cannot check the worker: {error}")),
    }
    if let Err(error) = child.kill() {
        problems.push(format!("cannot stop the worker: {error}"));
    }
    let mut reaped = None;
    let outcome = until(limit, || {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("cannot reap the worker: {error}"))
    });
    first(&mut reaped, outcome, "the worker has not exited");
    problems.extend(reaped);
    settled((!problems.is_empty()).then(|| problems.join("; ")), grace)
}

/// Stops the worker's whole job and confirms it: the job terminated (checked), the worker
/// reaped, no process left in the job, and the log drained, all within `limit`.
#[cfg(windows)]
fn settle_job(
    job: &loadout_winproc::Job,
    child: &mut Child,
    sink: &Shared,
    limit: Instant,
    grace: Duration,
) -> Result<(), String> {
    let mut problem = None;
    if let Err(error) = job.terminate(1) {
        problem = Some(format!("cannot stop the worker's job: {error}"));
    }
    let reaped = until(limit, || {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("cannot reap the worker: {error}"))
    });
    first(&mut problem, reaped, "the worker has not exited");
    let empty = until(limit, || {
        job.active_processes()
            .map(|count| count == 0)
            .map_err(|error| format!("cannot count the processes in the worker's job: {error}"))
    });
    first(
        &mut problem,
        empty,
        "processes the worker started are still running in its job",
    );
    let drained = until(limit, || Ok(finished(sink)));
    first(
        &mut problem,
        drained,
        "the worker's output is still held open",
    );
    settled(problem, grace)
}

#[cfg(windows)]
fn settled(problem: Option<String>, grace: Duration) -> Result<(), String> {
    problem.map_or(Ok(()), |detail| {
        Err(format!(
            "{detail} (within the {} second cleanup limit)",
            grace.as_secs()
        ))
    })
}

/// Waits for the worker to exit. At the soft deadline it writes one byte on the control pipe;
/// a worker that can no longer read it is still bound by the hard deadline, so a failed write
/// changes nothing.
fn watch(
    mut try_exit: impl FnMut() -> io::Result<Option<ExitStatus>>,
    control: &mut Option<ChildStdin>,
    started: Instant,
    limits: Limits,
) -> Ended {
    let (Some(soft), Some(hard)) = (
        started.checked_add(limits.soft),
        started.checked_add(limits.hard),
    ) else {
        return Ended::Lost("the clock can't represent the run's deadlines".to_owned());
    };
    let mut cancelled = false;
    loop {
        match try_exit() {
            Ok(Some(status)) => {
                return Ended::Exited {
                    code: status.code(),
                    cancelled,
                };
            }
            Ok(None) => {}
            Err(error) => return Ended::Lost(format!("cannot wait for the worker: {error}")),
        }
        // The soft deadline is checked first: a poll that wakes late, past both deadlines, still
        // asks the worker to stop before the run is ended.
        let now = Instant::now();
        if !cancelled && now >= soft {
            cancelled = true;
            let _ = send(control, CANCEL);
        }
        if now >= hard {
            return Ended::TimedOut { cancelled };
        }
        thread::sleep(POLL);
    }
}

fn send(control: &mut Option<ChildStdin>, bytes: &[u8]) -> Result<(), String> {
    let Some(stdin) = control.as_mut() else {
        return Err("it has no control pipe".to_owned());
    };
    stdin
        .write_all(bytes)
        .and_then(|()| stdin.flush())
        .map_err(|error| format!("cannot write to its control pipe: {error}"))
}

fn handshake_line(nonce: &str, group: i32) -> String {
    format!("{MAGIC} {VERSION} {nonce} {group} {}\n", std::process::id())
}

/// 32 bytes from the OS's random source, as lowercase hex. It ties the worker's environment to
/// the line on its control pipe: both come from the same supervisor.
fn nonce() -> io::Result<String> {
    #[cfg(unix)]
    let bytes = {
        let mut bytes = [0_u8; NONCE_BYTES];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        bytes
    };
    #[cfg(windows)]
    let bytes: [u8; NONCE_BYTES] = loadout_winproc::random_nonce()?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

fn is_nonce(text: &str) -> bool {
    text.len() == NONCE_BYTES * 2
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Compares in time independent of where the first difference is.
fn same(one: &str, other: &str) -> bool {
    one.len() == other.len()
        && one
            .bytes()
            .zip(other.bytes())
            .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// The bounded log both drainers append to.
struct Sink {
    cap: usize,
    ring: VecDeque<u8>,
    dropped: u64,
    open: u8,
    error: Option<String>,
}

type Shared = Arc<Mutex<Sink>>;

impl Sink {
    fn shared() -> Shared {
        Self::with_cap(LOG_CAP)
    }

    fn with_cap(cap: usize) -> Shared {
        Arc::new(Mutex::new(Sink {
            cap,
            ring: VecDeque::new(),
            dropped: 0,
            open: 0,
            error: None,
        }))
    }

    fn push(&mut self, chunk: &[u8]) {
        self.ring.extend(chunk.iter().copied());
        let excess = self.ring.len().saturating_sub(self.cap);
        if excess > 0 {
            self.ring.drain(..excess);
            self.dropped = self
                .dropped
                .saturating_add(u64::try_from(excess).unwrap_or(u64::MAX));
        }
    }
}

fn lock(sink: &Mutex<Sink>) -> MutexGuard<'_, Sink> {
    sink.lock().unwrap_or_else(PoisonError::into_inner)
}

fn finished(sink: &Mutex<Sink>) -> bool {
    lock(sink).open == 0
}

/// Starts a drainer for one stream. The caller already owns the worker, so a drainer that
/// can't start is a setup error the caller settles; nothing is counted open for it.
fn drain<R: Read + Send + 'static>(source: Option<R>, sink: &Shared) -> Result<(), String> {
    let Some(mut source) = source else {
        return Ok(());
    };
    lock(sink).open += 1;
    let writer = Arc::clone(sink);
    let job = move || {
        let mut chunk = [0_u8; 8192];
        loop {
            match source.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => lock(&writer).push(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    lock(&writer).error.get_or_insert(error.to_string());
                    break;
                }
            }
        }
        lock(&writer).open -= 1;
    };
    #[cfg(test)]
    let spawned = if seam::fail_drainer_spawn() {
        drop(job);
        Err(io::Error::other("injected: no thread could be created"))
    } else {
        thread::Builder::new()
            .name("loadout-worker-log".to_owned())
            .spawn(job)
    };
    #[cfg(not(test))]
    let spawned = thread::Builder::new()
        .name("loadout-worker-log".to_owned())
        .spawn(job);
    if let Err(error) = spawned {
        lock(sink).open -= 1;
        return Err(format!("cannot start a reader for its output: {error}"));
    }
    Ok(())
}

/// The log as it stands. A drainer still blocked on a pipe some escaped process holds keeps
/// running; the bytes so far are taken under the lock, and the log says it is incomplete.
fn log(sink: &Mutex<Sink>) -> Log {
    let sink = lock(sink);
    let mut bytes = Vec::new();
    if sink.dropped > 0 {
        bytes.extend_from_slice(
            format!(
                "[loadout: {} earlier bytes of worker output were dropped]\n",
                sink.dropped
            )
            .as_bytes(),
        );
    }
    bytes.extend(sink.ring.iter().copied());
    if sink.open > 0 {
        bytes.extend_from_slice(
            b"\n[loadout: a process outside loadout's control still holds the worker's output open; this log is incomplete]\n",
        );
    }
    if let Some(error) = &sink.error {
        bytes.extend_from_slice(
            format!("\n[loadout: cannot read the worker's output: {error}]\n").as_bytes(),
        );
    }
    Log {
        bytes,
        dropped: sink.dropped,
        complete: sink.open == 0 && sink.error.is_none(),
    }
}

/// The worker's side of startup. It must run before the worker locks, writes, or signals
/// anything, and a worker must exit at once on `Refused`. It checks that the worker's startup is
/// consistent with having been started by `supervise`:
///
/// - the nonce in `LOADOUT_WORKER_NONCE` equals the one in the first line of stdin, which
///   arrives within 5 seconds and is at most 256 bytes;
/// - stdin is a pipe (Unix: a FIFO; Windows: `FILE_TYPE_PIPE`), never a terminal, console,
///   file, or null device;
/// - Unix: the parent is the supervisor the line names; this process is in the group the line
///   names, does not lead it (a job a shell started leads its own), and the supervisor is not
///   in it;
/// - Windows: the process is in some job. That is not proof it is the supervisor's job, and the
///   parent is not checked.
///
/// These are cooperative-startup checks, not authentication. The supervisor's nonce is
/// unpredictable, but a same-user caller that deliberately sets a matching nonce in the
/// environment and on a pipe, and arranges the same parent and group (or a job), passes them
/// without `supervise`; the plan's same-user adversary is a non-goal. A real background worker
/// comes only from `supervise`, which owns its containment.
///
/// Only after the checks is the control reader armed: a later byte cancels the run, and EOF (the
/// supervisor is gone) cancels it and, on Unix, kills the group this process verified and still
/// belongs to (so the id can't have been reused), never any other. A refused worker never arms
/// it.
///
/// # Errors
///
/// `Refused` for any check that fails. Nothing was locked, written, or signalled.
pub fn handshake() -> Result<Arc<Supervised>, Refused> {
    let expected =
        std::env::var(WORKER_NONCE).map_err(|_| Refused(format!("{WORKER_NONCE} is not set")))?;
    if !is_nonce(&expected) {
        return Err(Refused(format!("{WORKER_NONCE} is not a loadout nonce")));
    }
    stdin_is_pipe()?;
    let (lines, arm) = start_control()?;
    let line = match lines.recv_timeout(HANDSHAKE_WAIT) {
        Ok(Ok(line)) => line,
        Ok(Err(why)) => return Err(Refused(why)),
        Err(_) => {
            return Err(Refused(format!(
                "no handshake line arrived on stdin within {} seconds",
                HANDSHAKE_WAIT.as_secs()
            )));
        }
    };
    let line = parse(&line).map_err(Refused)?;
    if !same(&line.nonce, &expected) {
        return Err(Refused(
            "the handshake line's nonce does not match the environment's".to_owned(),
        ));
    }
    #[cfg(unix)]
    let supervised = Supervised {
        cancel: Cancel::default(),
        group: verified_group(&line)?,
    };
    #[cfg(windows)]
    let supervised = {
        verified_job(&line)?;
        Supervised {
            cancel: Cancel::default(),
        }
    };
    let supervised = Arc::new(supervised);
    arm.send(Arc::clone(&supervised))
        .map_err(|_| Refused("the control pipe closed during the handshake".to_owned()))?;
    Ok(supervised)
}

struct Line {
    nonce: String,
    group: i32,
    supervisor: i32,
}

fn parse(line: &str) -> Result<Line, String> {
    let fields: Vec<&str> = line.split(' ').collect();
    let [magic, version, nonce, group, supervisor] = fields.as_slice() else {
        return Err("the handshake line is malformed".to_owned());
    };
    if *magic != MAGIC || *version != VERSION || !is_nonce(nonce) {
        return Err("the handshake line is malformed".to_owned());
    }
    let number = |text: &str| {
        text.parse::<i32>()
            .map_err(|_| "the handshake line is malformed".to_owned())
    };
    Ok(Line {
        nonce: (*nonce).to_owned(),
        group: number(*group)?,
        supervisor: number(*supervisor)?,
    })
}

#[cfg(unix)]
fn stdin_is_pipe() -> Result<(), Refused> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileTypeExt;
    let stdin = io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .and_then(|fd| std::fs::File::from(fd).metadata())
        .map_err(|error| Refused(format!("cannot inspect stdin: {error}")))?;
    if stdin.file_type().is_fifo() {
        Ok(())
    } else {
        Err(Refused(
            "stdin is not a pipe from a supervisor (a terminal, file, or /dev/null)".to_owned(),
        ))
    }
}

#[cfg(windows)]
fn stdin_is_pipe() -> Result<(), Refused> {
    match loadout_winproc::stdin_is_pipe() {
        Ok(true) => Ok(()),
        Ok(false) => Err(Refused(
            "stdin is not a pipe from a supervisor (a console, file, or NUL)".to_owned(),
        )),
        Err(error) => Err(Refused(format!("cannot inspect stdin: {error}"))),
    }
}

/// The line's group must be this process's group, not led by this process and not the
/// supervisor's own, and the supervisor must be this process's parent right now. With a
/// genuine `supervise` that group is the one its anchor holds; the checks can't tell a
/// deliberately arranged same-user topology apart (see [`handshake`]).
#[cfg(unix)]
fn verified_group(line: &Line) -> Result<Pid, Refused> {
    if line.group <= 1 || line.supervisor <= 1 {
        return Err(Refused("the handshake line names no real group".to_owned()));
    }
    let group = Pid::from_raw(line.group);
    let supervisor = Pid::from_raw(line.supervisor);
    let parent = getppid();
    if parent != supervisor {
        return Err(Refused(format!(
            "its parent is {parent}, not the supervisor {supervisor}"
        )));
    }
    let own = getpgrp();
    if own != group {
        return Err(Refused(format!(
            "it is in process group {own}, not the supervisor's group {group}"
        )));
    }
    if own == getpid() {
        return Err(Refused(
            "it leads its own process group, as a job a shell started does".to_owned(),
        ));
    }
    match getpgid(Some(supervisor)) {
        Ok(theirs) if theirs == group => Err(Refused(
            "it shares its supervisor's process group".to_owned(),
        )),
        Ok(_) => Ok(group),
        Err(error) => Err(Refused(format!(
            "cannot check its supervisor's process group: {error}"
        ))),
    }
}

#[cfg(windows)]
fn verified_job(line: &Line) -> Result<(), Refused> {
    if line.group != 0 || line.supervisor <= 0 {
        return Err(Refused(
            "the handshake line is not a Windows one".to_owned(),
        ));
    }
    match loadout_winproc::current_process_in_job() {
        Ok(true) => Ok(()),
        Ok(false) => Err(Refused("it is not in a job".to_owned())),
        Err(error) => Err(Refused(format!("cannot check its job: {error}"))),
    }
}

type Lines = mpsc::Receiver<Result<String, String>>;
type Arm = mpsc::Sender<Arc<Supervised>>;

/// One thread owns stdin. It reads the handshake line and hands it over, then waits to be
/// armed. Dropping `Arm` (a refused handshake) ends it without reading another byte.
fn start_control() -> Result<(Lines, Arm), Refused> {
    let (line_sender, lines) = mpsc::channel();
    let (arm, armed) = mpsc::channel::<Arc<Supervised>>();
    thread::Builder::new()
        .name("loadout-worker-control".to_owned())
        .spawn(move || {
            let mut stdin = io::stdin().lock();
            let line = read_line(&mut stdin);
            let read = line.is_ok();
            if line_sender.send(line).is_err() || !read {
                return;
            }
            let Ok(supervised) = armed.recv() else {
                return;
            };
            listen(&mut stdin, &supervised);
        })
        .map_err(|error| Refused(format!("cannot start the control reader: {error}")))?;
    Ok((lines, arm))
}

fn read_line(input: &mut impl Read) -> Result<String, String> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Err("the control pipe closed before a handshake line".to_owned()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                line.push(byte[0]);
                if line.len() > MAX_LINE {
                    return Err("the handshake line is too long".to_owned());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(format!("cannot read the handshake line: {error}")),
        }
    }
    String::from_utf8(line).map_err(|_| "the handshake line is not UTF-8".to_owned())
}

/// After the handshake: any byte cancels the run; EOF means the supervisor is gone.
fn listen(input: &mut impl Read, supervised: &Supervised) {
    let mut byte = [0_u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => {
                supervisor_gone(supervised);
                return;
            }
            Ok(_) => supervised.cancel.cancel(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                supervised.cancel.cancel();
                return;
            }
        }
    }
}

/// The control pipe closed: with a genuine `supervise`, the supervisor died, so nothing will
/// stop this tree from outside (macOS has no scheduler limit). On Unix the worker kills its
/// verified group, itself included; it is still a member, so the group exists and its id can't
/// belong to anyone else. Windows only cancels: with a genuine `supervise` the job's last handle
/// was the supervisor's, so the kernel already ended the job.
fn supervisor_gone(supervised: &Supervised) {
    supervised.cancel.cancel();
    #[cfg(unix)]
    if getpgrp() == supervised.group {
        let _ = killpg(supervised.group, Signal::SIGKILL);
    }
}

/// Test-only fault injection for the supervisor's own setup, and a record of the deadlines it
/// used. Each point reaches exactly one branch of `run`. The exhausting faults wait for the real
/// setup deadline to pass; they move no clock.
#[cfg(test)]
mod seam {
    use std::cell::{Cell, RefCell};
    #[cfg(windows)]
    use std::io;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Point {
        /// Before `start`'s entry deadline check.
        #[cfg(unix)]
        BeforeStart,
        /// After that entry check, before the drainers.
        #[cfg(unix)]
        AfterSpawn,
        #[cfg(unix)]
        BeforeSend,
        #[cfg(windows)]
        BeforeMembership,
        #[cfg(windows)]
        AfterMembership,
        #[cfg(windows)]
        BeforeResume,
        /// After the helper found, opened and checked the worker's thread, before its final
        /// deadline check and `ResumeThread`.
        #[cfg(windows)]
        AfterPrepare,
        /// After `ResumeThread` returned, before the post-resume deadline check.
        #[cfg(windows)]
        AfterResumeCall,
        #[cfg(windows)]
        AfterResume,
        BetweenDrainers,
    }

    #[derive(Debug, Clone)]
    pub(super) enum Fault {
        /// Fails at the point at once.
        Fail(Point),
        /// Waits for the setup deadline to pass, then fails at the point.
        ExhaustThenFail(Point),
        /// Waits for the setup deadline to pass, then lets setup go on, so its own deadline
        /// checks see it.
        ExhaustThenContinue(Point),
        /// Waits until this readiness file exists (the worker, and any descendant it reports,
        /// runs), then fails at the point.
        FailWhenReady(Point, PathBuf),
        /// The first observation of a never-run worker fails (Windows pre-job disposal).
        #[cfg(windows)]
        FailObservation,
        /// The next drainer thread can't be created.
        FailDrainerSpawn,
    }

    thread_local! {
        static FAULT: RefCell<Option<Fault>> = const { RefCell::new(None) };
        static LIMITS: Cell<Option<(Instant, Instant)>> = const { Cell::new(None) };
        /// Every point setup reached, in order, since the last `reset`.
        static REACHED: RefCell<Vec<Point>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn set(fault: Option<Fault>) {
        FAULT.with(|cell| *cell.borrow_mut() = fault);
    }

    /// Clears the fault, the recorded deadlines and the reached points, before a control's run.
    pub(super) fn reset() {
        set(None);
        LIMITS.with(|cell| cell.set(None));
        REACHED.with(|cell| cell.borrow_mut().clear());
    }

    /// The points setup reached in the last run, in order.
    #[cfg(unix)]
    pub(super) fn reached() -> Vec<Point> {
        REACHED.with(|cell| cell.borrow().clone())
    }

    /// The setup deadline and the limit the disposal was actually given, from the last run.
    pub(super) fn limits() -> Option<(Instant, Instant)> {
        LIMITS.with(Cell::get)
    }

    pub(super) fn record(setup: Instant, limit: Instant) {
        LIMITS.with(|cell| cell.set(Some((setup, limit))));
    }

    fn current() -> Option<Fault> {
        FAULT.with(|cell| cell.borrow().clone())
    }

    fn take(wanted: impl Fn(&Fault) -> bool) -> bool {
        FAULT.with(|cell| {
            let mut fault = cell.borrow_mut();
            let hit = fault.as_ref().is_some_and(wanted);
            if hit {
                *fault = None;
            }
            hit
        })
    }

    #[cfg(windows)]
    pub(super) fn observation() -> Option<io::Error> {
        take(|fault| matches!(fault, Fault::FailObservation))
            .then(|| io::Error::other("injected: the worker could not be observed"))
    }

    pub(super) fn fail_drainer_spawn() -> bool {
        take(|fault| matches!(fault, Fault::FailDrainerSpawn))
    }

    fn wait_out(limit: Instant) {
        while Instant::now() < limit {
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn hit(point: &Point, setup: Instant) -> Result<(), String> {
        REACHED.with(|cell| cell.borrow_mut().push(*point));
        let injected = || Err(format!("injected: setup failure at {point:?}"));
        match current() {
            Some(Fault::Fail(at)) if at == *point => injected(),
            Some(Fault::ExhaustThenFail(at)) if at == *point => {
                wait_out(setup);
                injected()
            }
            Some(Fault::ExhaustThenContinue(at)) if at == *point => {
                wait_out(setup);
                Ok(())
            }
            Some(Fault::FailWhenReady(at, ready)) if at == *point => {
                let limit = Instant::now() + Duration::from_secs(20);
                while !ready.exists() {
                    if Instant::now() >= limit {
                        return Err("injected: the worker never reported ready".to_owned());
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                injected()
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_the_last_bytes_and_counts_what_it_dropped() {
        let sink = Sink::with_cap(8);
        lock(&sink).push(b"0123456789");
        lock(&sink).push(b"ab");
        let log = log(&sink);
        assert_eq!(log.dropped, 4);
        assert!(log.complete);
        assert_eq!(
            log.bytes,
            b"[loadout: 4 earlier bytes of worker output were dropped]\n456789ab"
        );
    }

    #[test]
    fn an_open_stream_or_a_read_error_makes_the_log_incomplete() {
        let sink = Sink::with_cap(8);
        lock(&sink).open = 1;
        let held = log(&sink);
        assert!(!held.complete);
        assert!(String::from_utf8_lossy(&held.bytes).contains("this log is incomplete"));
        let sink = Sink::with_cap(8);
        lock(&sink).error = Some("broken".to_owned());
        assert!(!log(&sink).complete);
    }

    #[test]
    fn a_log_of_ten_mebibytes_stays_within_the_cap() {
        let sink = Sink::shared();
        let chunk = vec![b'x'; 8192];
        for _ in 0..(10 * 1024 * 1024 / chunk.len()) {
            lock(&sink).push(&chunk);
        }
        let log = log(&sink);
        assert_eq!(log.dropped, 9 * 1024 * 1024);
        let note = "[loadout: 9437184 earlier bytes of worker output were dropped]\n";
        assert_eq!(log.bytes.len(), LOG_CAP + note.len());
        assert!(log.bytes.starts_with(note.as_bytes()));
    }

    #[test]
    fn handshake_lines_parse_only_in_their_exact_form() {
        let nonce = hex(&[0xab; NONCE_BYTES]);
        let line = parse(&format!("{MAGIC} {VERSION} {nonce} 4242 77")).unwrap();
        assert_eq!((line.group, line.supervisor), (4242, 77));
        assert!(same(&line.nonce, &nonce));
        for bad in [
            String::new(),
            format!("{MAGIC} {VERSION} {nonce} 4242"),
            format!("{MAGIC} {VERSION} {nonce} 4242 77 extra"),
            format!("{MAGIC} 2 {nonce} 4242 77"),
            format!("LOADOUT {VERSION} {nonce} 4242 77"),
            format!("{MAGIC} {VERSION} {} 4242 77", nonce.to_uppercase()),
            format!("{MAGIC} {VERSION} {} 4242 77", &nonce[1..]),
            format!("{MAGIC} {VERSION} {nonce} group 77"),
            format!("{MAGIC}  {VERSION} {nonce} 4242 77"),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn reading_the_handshake_line_is_bounded() {
        let ok = read_line(&mut &b"hello\nc"[..]);
        assert_eq!(ok, Ok("hello".to_owned()));
        assert!(read_line(&mut &b"no newline"[..]).is_err());
        let long = vec![b'a'; MAX_LINE + 2];
        assert!(read_line(&mut long.as_slice()).is_err());
    }

    #[test]
    fn nonces_are_hex_of_the_full_length_and_compare_exactly() {
        let one = nonce().unwrap();
        let other = nonce().unwrap();
        assert!(is_nonce(&one) && is_nonce(&other));
        assert_ne!(one, other);
        assert!(same(&one, &one.clone()));
        assert!(!same(&one, &other));
        assert!(!same(&one, &one[..60]));
    }

    #[test]
    fn a_control_byte_cancels_and_the_reader_stops_at_eof() {
        let supervised = Supervised {
            cancel: Cancel::default(),
            #[cfg(unix)]
            group: Pid::from_raw(i32::MAX),
        };
        let mut input = &b"c"[..];
        // A test-only group id that is never a member's, so EOF's own-group check fails and
        // nothing is signalled.
        listen(&mut input, &supervised);
        assert!(supervised.cancel.is_cancelled());
    }

    #[test]
    fn the_soft_deadline_writes_one_cancel_byte_and_the_hard_one_ends_the_watch() {
        let limits = Limits {
            soft: Duration::from_millis(50),
            hard: Duration::from_millis(200),
            settle: Duration::from_secs(1),
        };
        let mut control = None;
        let ended = watch(|| Ok(None), &mut control, Instant::now(), limits);
        assert_eq!(ended, Ended::TimedOut { cancelled: true });
    }

    #[test]
    fn a_first_poll_past_both_deadlines_still_cancels_before_it_ends_the_watch() {
        let limits = Limits {
            soft: Duration::from_millis(50),
            hard: Duration::from_millis(200),
            settle: Duration::from_secs(1),
        };
        let started = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("the clock reaches a second back");
        let mut control = None;
        let ended = watch(|| Ok(None), &mut control, started, limits);
        assert_eq!(ended, Ended::TimedOut { cancelled: true });
    }

    #[test]
    fn limits_are_checked_before_anything_starts() {
        let now = Instant::now();
        assert_eq!(Limits::BACKGROUND.check(now), Ok(()));
        let valid = Limits {
            soft: Duration::from_secs(1),
            hard: Duration::from_secs(2),
            settle: Duration::from_secs(1),
        };
        assert_eq!(valid.check(now), Ok(()));
        for invalid in [
            Limits {
                soft: Duration::MAX,
                ..valid
            },
            Limits {
                hard: Duration::MAX,
                ..valid
            },
            Limits {
                settle: Duration::MAX,
                ..valid
            },
            Limits {
                soft: Duration::from_secs(3),
                ..valid
            },
            Limits {
                soft: Duration::ZERO,
                ..valid
            },
            Limits {
                hard: Duration::ZERO,
                soft: Duration::ZERO,
                ..valid
            },
            Limits {
                settle: Duration::ZERO,
                ..valid
            },
        ] {
            assert!(invalid.check(now).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn a_spent_budget_is_now_rather_than_an_overflow() {
        let before = Instant::now();
        let spent = after(Duration::MAX);
        assert!(spent >= before && spent <= Instant::now());
    }

    #[test]
    fn a_drainer_thread_that_cannot_be_created_leaves_nothing_counted_open() {
        let sink = Sink::shared();
        seam::set(Some(seam::Fault::FailDrainerSpawn));
        let failed = drain(Some(io::Cursor::new(b"unread".to_vec())), &sink);
        seam::set(None);
        assert!(failed.unwrap_err().contains("injected"));
        assert_eq!(
            lock(&sink).open,
            0,
            "a drainer that never started must not stay counted"
        );
        assert!(finished(&sink));

        drain(Some(io::Cursor::new(b"read".to_vec())), &sink).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !finished(&sink) {
            assert!(Instant::now() < until, "the drainer did not reach EOF");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(log(&sink).bytes, b"read");
    }

    /// The fixture processes for the setup controls below: the unit-test binary run again as one
    /// ignored test, by role. None of them runs a handshake, so none has the production EOF
    /// self-kill; each ends by itself within a minute as a fallback, which is a lifetime bound,
    /// not a cleanup.
    ///
    /// - `hold`: takes an exclusive lock on `root.lock` and never closes it (the handle is
    ///   leaked, so no return, unwind or exit path in this process releases it; only the OS
    ///   does, as the process ends), then (with `LOADOUT_TEST_DESCENDANT`) starts a
    ///   `descendant` and waits for it to be ready, then writes `root.ready`.
    /// - `descendant`: the same with `descendant.lock` and `descendant.ready`.
    /// - `release-early`: the counterexample, used only to show what the observer can't tell:
    ///   locks `root.lock`, writes `root.ready`, releases the lock itself, writes
    ///   `root.released`, and stays alive.
    /// - `exit-after`: sleeps `LOADOUT_TEST_MS` milliseconds and exits 0.
    #[test]
    #[ignore = "a fixture process the setup controls start"]
    fn fixture_worker() {
        let (Some(role), Some(dir)) = (
            std::env::var_os("LOADOUT_TEST_UNIT_ROLE"),
            std::env::var_os("LOADOUT_TEST_UNIT_DIR"),
        ) else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let role = role.to_string_lossy().into_owned();
        let until = Instant::now() + Duration::from_mins(1);
        match role.as_str() {
            "hold" | "descendant" => {
                let name = if role == "hold" { "root" } else { "descendant" };
                let lock = std::fs::File::create(dir.join(format!("{name}.lock"))).unwrap();
                lock.lock().unwrap();
                // Never closed by this process: only the OS releases it, as the process ends.
                std::mem::forget(lock);
                if role == "hold" && std::env::var_os("LOADOUT_TEST_DESCENDANT").is_some() {
                    let descendant = setup::fixture("descendant", &dir).spawn().unwrap();
                    // Left to the supervisor's containment on purpose; it also ends by itself.
                    drop(descendant);
                    let ready = dir.join("descendant.ready");
                    while !ready.exists() {
                        assert!(
                            Instant::now() < until,
                            "the descendant never reported ready"
                        );
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                std::fs::write(dir.join(format!("{name}.ready")), "ready").unwrap();
                while Instant::now() < until {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            "release-early" => {
                let lock = std::fs::File::create(dir.join("root.lock")).unwrap();
                lock.lock().unwrap();
                std::fs::write(dir.join("root.ready"), "ready").unwrap();
                lock.unlock().unwrap();
                std::fs::write(dir.join("root.released"), "released").unwrap();
                while Instant::now() < until {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            "exit-after" => {
                let ms = std::env::var("LOADOUT_TEST_MS").unwrap().parse().unwrap();
                thread::sleep(Duration::from_millis(ms));
            }
            other => panic!("unknown fixture role {other}"),
        }
    }

    /// Setup controls for the supervisor's own paths. Each injects one fault, checks the error
    /// and the cause it keeps, and observes fixtures the supervisor owns without any process
    /// identity. A fixture takes an exclusive lock and never releases it itself, and writes its
    /// ready file only after taking it, so the lock's release is the OS releasing a resource of
    /// a process that reached its end. It is not a confirmed exit (only an owned handle gives
    /// that), it can lag the exit on Windows, and a fixture that released early would fool it
    /// (`an_early_release_while_alive_is_not_death`). The claims, kept apart:
    ///
    /// - "never ran": no ready or lock file appeared (only meaningful for a worker that was
    ///   never resumed or never started);
    /// - "released at return": the lock is free on the first check right after `supervise`
    ///   returned. Asserted only after a confirmed stop (Unix `Setup`: the group answered
    ///   `ESRCH`), which is the supervisor's own exit confirmation; the release agrees with it;
    /// - "eventually released": the lock became free within a bound afterwards;
    /// - "confirmed exit": an owned handle reaped the process (direct fixtures only).
    ///
    /// A missing lock file or ready file, or any lock error but "held", fails the control. Every
    /// child a control starts directly is an [`Owned`] from the moment it exists. An outer
    /// watchdog aborts the test process if `supervise` outlives its bound; with these
    /// handshake-less fixtures that only closes the supervisor's handles (the fixtures' own
    /// lifetime bound remains the fallback), so it bounds the control, not the cleanup.
    pub(super) mod setup {
        use std::fs::{OpenOptions, TryLockError};
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, Ordering};

        use super::super::seam::{self, Fault, Point};
        use super::*;
        use crate::process::owned::Owned;

        struct Watchdog(Arc<AtomicBool>);

        impl Watchdog {
            fn new(limit: Duration) -> Watchdog {
                let done = Arc::new(AtomicBool::new(false));
                let watching = Arc::clone(&done);
                thread::Builder::new()
                    .spawn(move || {
                        let until = Instant::now() + limit;
                        while !watching.load(Ordering::SeqCst) {
                            if Instant::now() >= until {
                                eprintln!("watchdog: a setup control outlived {limit:?}");
                                std::process::abort();
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                    })
                    .unwrap();
                Watchdog(done)
            }
        }

        impl Drop for Watchdog {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        pub(in super::super) fn fixture(role: &str, dir: &Path) -> Command {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "process::background::tests::fixture_worker",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env("LOADOUT_TEST_UNIT_ROLE", role)
                .env("LOADOUT_TEST_UNIT_DIR", dir)
                .stdin(Stdio::null());
            command
        }

        fn holder(dir: &Path, descendant: bool) -> Command {
            let mut command = fixture("hold", dir);
            if descendant {
                command.env("LOADOUT_TEST_DESCENDANT", "1");
            }
            command
        }

        fn sleeper(dir: &Path, ms: u64) -> Command {
            let mut command = fixture("exit-after", dir);
            command.env("LOADOUT_TEST_MS", ms.to_string());
            command
        }

        fn limits(settle: Duration) -> Limits {
            Limits {
                soft: Duration::from_secs(60),
                hard: Duration::from_secs(90),
                settle,
            }
        }

        /// Whether the OS released `name.lock` (see the module doc: the end of a fixture that
        /// never releases it itself, not a confirmed exit). It needs that fixture's ready file
        /// and lock file; anything missing or any lock error but "held" is an error.
        pub(in super::super) fn released(dir: &Path, name: &str) -> Result<bool, String> {
            let ready = dir.join(format!("{name}.ready"));
            if !ready.is_file() {
                return Err(format!(
                    "{} is missing, so nothing can be concluded",
                    ready.display()
                ));
            }
            let path = dir.join(format!("{name}.lock"));
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
            match lock.try_lock() {
                Ok(()) => {
                    lock.unlock()
                        .map_err(|error| format!("cannot unlock: {error}"))?;
                    Ok(true)
                }
                Err(TryLockError::WouldBlock) => Ok(false),
                Err(TryLockError::Error(error)) => Err(format!("cannot test the lock: {error}")),
            }
        }

        fn eventually_released(dir: &Path, name: &str) -> Result<bool, String> {
            let until = Instant::now() + Duration::from_secs(15);
            loop {
                if released(dir, name)? {
                    return Ok(true);
                }
                if Instant::now() >= until {
                    return Ok(false);
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        #[cfg(windows)]
        fn never_ran(dir: &Path) -> bool {
            thread::sleep(Duration::from_secs(1));
            !dir.join("root.ready").exists() && !dir.join("root.lock").exists()
        }

        struct Run {
            result: Result<Outcome, Error>,
            elapsed: Duration,
            dir: tempfile::TempDir,
        }

        fn inject(
            fault: Option<Fault>,
            worker: impl FnOnce(&Path) -> Command,
            settle: Duration,
        ) -> Run {
            let dir = tempfile::tempdir().unwrap();
            let command = worker(dir.path());
            let _watchdog = Watchdog::new(Duration::from_secs(120));
            seam::reset();
            seam::set(fault);
            let started = Instant::now();
            let result = supervise(command, limits(settle));
            let elapsed = started.elapsed();
            seam::set(None);
            Run {
                result,
                elapsed,
                dir,
            }
        }

        fn cause(result: &Result<Outcome, Error>) -> String {
            match result {
                Err(Error::Setup(cause) | Error::Cleanup { cause, .. }) => cause.clone(),
                other => panic!("expected a setup error, got {other:?}"),
            }
        }

        /// A setup failure's disposal was given the original setup deadline, not a new one.
        fn disposed_within_setup() {
            let (setup, limit) = seam::limits().expect("the run recorded its deadlines");
            assert_eq!(
                limit, setup,
                "a setup failure's cleanup got a renewed budget"
            );
        }

        /// With a confirmed disposal (`Setup`), a fixture that was ready has released its lock
        /// at return; with an unconfirmed one (`Cleanup`), nothing is concluded at return.
        #[cfg(unix)]
        fn released_at_return_after_a_confirmed_stop(run: &Run, name: &str) {
            if matches!(run.result, Err(Error::Setup(_))) {
                assert_eq!(
                    released(run.dir.path(), name),
                    Ok(true),
                    "{name} outlived a confirmed stop"
                );
            }
        }

        /// Waits, within a minute, for `marker` while the owned fixture still runs: `Ok(false)`
        /// if it exited first or the minute passed.
        fn appeared_while_running(fixture: &mut Owned, marker: &Path) -> Result<bool, String> {
            let until = Instant::now() + Duration::from_secs(60);
            loop {
                if marker.exists() {
                    return Ok(true);
                }
                let exited = fixture
                    .observe()
                    .map_err(|error| format!("cannot observe the fixture: {error}"))?;
                if exited.is_some() || Instant::now() >= until {
                    return Ok(false);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn a_drainer_that_cannot_start_stops_the_owned_worker() {
            let run = inject(
                Some(Fault::Fail(Point::BetweenDrainers)),
                |dir| holder(dir, false),
                Duration::from_secs(10),
            );
            assert!(
                cause(&run.result).contains("BetweenDrainers"),
                "{:?}",
                run.result
            );
            disposed_within_setup();
            // Unix: it may not have become ready before the failure; Windows: it never ran.
            #[cfg(windows)]
            assert!(never_ran(run.dir.path()));
            #[cfg(unix)]
            if run.dir.path().join("root.ready").exists() {
                released_at_return_after_a_confirmed_stop(&run, "root");
            }
        }

        #[cfg(unix)]
        #[test]
        fn a_failure_after_a_ready_descendant_stops_the_whole_group() {
            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("root.ready");
            let _watchdog = Watchdog::new(Duration::from_secs(120));
            seam::reset();
            seam::set(Some(Fault::FailWhenReady(Point::BeforeSend, ready)));
            let result = supervise(holder(dir.path(), true), limits(Duration::from_secs(30)));
            seam::set(None);
            assert!(cause(&result).contains("BeforeSend"), "{result:?}");
            disposed_within_setup();
            if matches!(result, Err(Error::Setup(_))) {
                assert_eq!(released(dir.path(), "root"), Ok(true));
                assert_eq!(released(dir.path(), "descendant"), Ok(true));
            } else {
                assert_eq!(eventually_released(dir.path(), "root"), Ok(true));
                assert_eq!(eventually_released(dir.path(), "descendant"), Ok(true));
            }
        }

        /// N1: setup that finishes after its deadline is a setup timeout, disposed within the
        /// original deadline, never a success with a fresh budget. Here the deadline passes after
        /// the entry check (before the drainers, or before the send) and setup then goes on, so
        /// the check after the send refuses it.
        #[cfg(unix)]
        #[test]
        fn setup_that_ends_after_its_deadline_is_a_timeout_not_a_success() {
            for point in [Point::AfterSpawn, Point::BeforeSend] {
                let settle = Duration::from_secs(2);
                let run = inject(
                    Some(Fault::ExhaustThenContinue(point)),
                    |dir| sleeper(dir, 4000),
                    settle,
                );
                assert!(
                    cause(&run.result).contains("setup limit"),
                    "{point:?}: {:?}",
                    run.result
                );
                disposed_within_setup();
                assert!(
                    run.elapsed < settle + Duration::from_secs(3),
                    "{point:?}: the late setup was given a fresh budget: {:?}",
                    run.elapsed
                );
            }
        }

        /// N1's direct case: setup already spent when `start` begins is refused by the entry check
        /// itself, before either drainer starts or the line is sent.
        #[cfg(unix)]
        #[test]
        fn setup_already_spent_at_start_is_refused_before_anything_starts() {
            let settle = Duration::from_secs(2);
            let run = inject(
                Some(Fault::ExhaustThenContinue(Point::BeforeStart)),
                |dir| sleeper(dir, 4000),
                settle,
            );
            assert!(
                cause(&run.result).contains("setup limit"),
                "{:?}",
                run.result
            );
            assert_eq!(
                seam::reached(),
                vec![Point::BeforeStart],
                "the entry check must refuse before anything after it runs"
            );
            disposed_within_setup();
            assert!(
                run.elapsed < settle + Duration::from_secs(3),
                "the spent setup was given a fresh budget: {:?}",
                run.elapsed
            );
        }

        /// Setup within its deadline succeeds, and the worker may then run far longer than the
        /// setup budget: the run is bounded by its soft and hard limits, not by setup.
        #[test]
        fn an_ordinary_worker_may_outlast_the_setup_budget() {
            let settle = Duration::from_secs(2);
            let run = inject(None, |dir| sleeper(dir, 4000), settle);
            let outcome = run.result.unwrap();
            assert_eq!(
                outcome.ended,
                Ended::Exited {
                    code: Some(0),
                    cancelled: false
                }
            );
            assert_eq!(outcome.cleanup, Ok(()));
            assert!(
                run.elapsed > settle,
                "the worker should outlast the setup budget"
            );
            let (setup, limit) = seam::limits().unwrap();
            assert!(
                limit > setup,
                "a normal run's cleanup gets its own fresh limit"
            );
        }

        #[test]
        fn cleanup_after_a_late_setup_failure_gets_no_renewed_budget() {
            let settle = Duration::from_secs(2);
            #[cfg(unix)]
            let point = Point::BeforeSend;
            #[cfg(windows)]
            let point = Point::AfterMembership;
            let run = inject(
                Some(Fault::ExhaustThenFail(point)),
                |dir| holder(dir, false),
                settle,
            );
            assert!(cause(&run.result).contains("injected"), "{:?}", run.result);
            disposed_within_setup();
            assert!(
                run.elapsed < settle + Duration::from_secs(5),
                "cleanup was given a fresh budget: {:?}",
                run.elapsed
            );
            #[cfg(windows)]
            assert!(never_ran(run.dir.path()));
        }

        #[cfg(windows)]
        #[test]
        fn a_failure_before_membership_disposes_the_never_run_worker() {
            let run = inject(
                Some(Fault::Fail(Point::BeforeMembership)),
                |dir| holder(dir, false),
                Duration::from_secs(10),
            );
            assert!(
                matches!(run.result, Err(Error::Setup(_))),
                "{:?}",
                run.result
            );
            assert!(never_ran(run.dir.path()));
        }

        /// C2: a failed first observation still leads to a checked kill and reap of the
        /// never-run worker. The observation error is kept, so the result is an honest
        /// `Cleanup`, not success. The suspended worker can't end by itself, so it is owned
        /// from the moment it exists; the guard's checked stop covers any failure here.
        #[cfg(windows)]
        #[test]
        fn a_failed_observation_still_kills_the_never_run_worker() {
            let dir = tempfile::tempdir().unwrap();
            let mut worker = Owned::new(
                holder(dir.path(), false)
                    .creation_flags(loadout_winproc::CREATE_SUSPENDED.0)
                    .spawn()
                    .unwrap(),
            );
            seam::reset();
            seam::set(Some(Fault::FailObservation));
            let stopped = stop_unstarted(
                worker.child(),
                Instant::now() + Duration::from_secs(10),
                Duration::from_secs(10),
            );
            seam::set(None);
            let detail = stopped.unwrap_err();
            assert!(detail.contains("injected"), "{detail}");
            assert!(!detail.contains("has not exited"), "{detail}");
            assert!(
                worker.observe().unwrap().is_some(),
                "the worker must be reaped"
            );
            assert_eq!(
                worker.kills(),
                0,
                "nothing was left for the test's own guard to stop"
            );
            assert!(never_ran(dir.path()));
        }

        #[cfg(windows)]
        #[test]
        fn a_failure_after_membership_stops_the_job_before_any_resume() {
            let run = inject(
                Some(Fault::Fail(Point::AfterMembership)),
                |dir| holder(dir, false),
                Duration::from_secs(10),
            );
            assert!(
                matches!(run.result, Err(Error::Setup(_))),
                "{:?}",
                run.result
            );
            assert!(never_ran(run.dir.path()));
        }

        /// The deadline passes on entry to the resume, after the thread was prepared (the
        /// helper's last check refuses), or after `ResumeThread` returned (the post-operation
        /// check refuses). Only in the last case can the worker have run.
        #[cfg(windows)]
        #[test]
        fn a_spent_setup_deadline_around_the_resume_is_a_timeout() {
            for (point, ran) in [
                (Point::BeforeResume, false),
                (Point::AfterPrepare, false),
                (Point::AfterResumeCall, true),
            ] {
                let run = inject(
                    Some(Fault::ExhaustThenContinue(point)),
                    |dir| holder(dir, false),
                    Duration::from_secs(2),
                );
                let cause = cause(&run.result);
                assert!(
                    cause.contains("setup limit") || cause.contains("deadline"),
                    "{point:?}: {cause}"
                );
                disposed_within_setup();
                if ran {
                    if run.dir.path().join("root.ready").exists() {
                        assert_eq!(eventually_released(run.dir.path(), "root"), Ok(true));
                    }
                } else {
                    assert!(never_ran(run.dir.path()), "{point:?}");
                }
            }
        }

        /// The worker and a descendant it started are both running (both ready) before the
        /// injected failure, so the whole job must stop, within a setup deadline long enough for
        /// that readiness wait.
        #[cfg(windows)]
        #[test]
        fn a_failure_after_a_real_resume_stops_root_and_descendant() {
            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("root.ready");
            let _watchdog = Watchdog::new(Duration::from_secs(120));
            seam::reset();
            seam::set(Some(Fault::FailWhenReady(Point::AfterResume, ready)));
            let result = supervise(holder(dir.path(), true), limits(Duration::from_secs(40)));
            seam::set(None);
            assert!(cause(&result).contains("AfterResume"), "{result:?}");
            disposed_within_setup();
            assert_eq!(eventually_released(dir.path(), "root"), Ok(true));
            assert_eq!(eventually_released(dir.path(), "descendant"), Ok(true));
        }

        /// The observer itself, against a fixture this test owns directly: absence, a ready
        /// file without a lock file, and a lock that can't be opened are errors, never
        /// "released"; a live fixture doing nothing (no heartbeat) holds its lock; and only the
        /// owned handle confirms its exit (`stop` reaps it), after which the release follows.
        #[test]
        fn the_lock_observer_never_mistakes_silence_or_absence_for_death() {
            let dir = tempfile::tempdir().unwrap();
            let (ready, lock) = (dir.path().join("root.ready"), dir.path().join("root.lock"));
            assert!(
                released(dir.path(), "root").is_err(),
                "absence must not count"
            );
            std::fs::write(&ready, "ready").unwrap();
            assert!(
                released(dir.path(), "root").is_err(),
                "a missing lock must not count"
            );
            std::fs::create_dir(&lock).unwrap();
            assert!(
                released(dir.path(), "root").is_err(),
                "a lock error must not count"
            );
            std::fs::remove_dir(&lock).unwrap();
            std::fs::remove_file(&ready).unwrap();

            let mut root = Owned::new(holder(dir.path(), false).spawn().unwrap());
            assert_eq!(
                appeared_while_running(&mut root, &ready),
                Ok(true),
                "never ready"
            );
            assert_eq!(
                released(dir.path(), "root"),
                Ok(false),
                "a silent live fixture holds it"
            );
            assert!(root.observe().unwrap().is_none());
            assert_eq!(
                root.stop(Duration::from_secs(10)),
                Ok(()),
                "the owned handle must confirm the exit"
            );
            assert_eq!(eventually_released(dir.path(), "root"), Ok(true));
        }

        /// The counterexample the observer can't see through: a fixture that releases its lock
        /// itself and stays alive. The lock is free while the owned handle still sees the
        /// process running, so a released lock alone never proves death; the fixtures behind
        /// every death claim here never release it themselves.
        #[test]
        fn an_early_release_while_alive_is_not_death() {
            let dir = tempfile::tempdir().unwrap();
            let mut early = Owned::new(fixture("release-early", dir.path()).spawn().unwrap());
            let marker = dir.path().join("root.released");
            assert_eq!(appeared_while_running(&mut early, &marker), Ok(true));
            assert_eq!(released(dir.path(), "root"), Ok(true), "its lock is free");
            assert!(
                early.observe().unwrap().is_none(),
                "yet the process still runs"
            );
            assert_eq!(early.stop(Duration::from_secs(10)), Ok(()));
        }
    }
}
