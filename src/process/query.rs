use std::fmt;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(windows)]
use std::os::windows::io::{AsHandle, BorrowedHandle, OwnedHandle};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, ExitStatus};

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(unix)]
use nix::unistd::Pid;
#[cfg(windows)]
use process_wrap::std::{ChildWrapper, CommandWrap, CreationFlags, JobObject};

use super::{Containment, Invocation, WORKER_NONCE};

/// How long loadout waits for one installed-query's (or doctor's query's) own process. Generous:
/// a query can refresh a source index or fetch an API. Stopping what it leaves behind is bounded
/// separately by `QUERY_CLEANUP_GRACE`.
pub(super) const QUERY_DEADLINE: Duration = Duration::from_mins(10);
/// A query that prints more than this on stdout or stderr is refused, never parsed truncated.
pub(super) const QUERY_OUTPUT_CAP: usize = 8 * 1024 * 1024;
/// How much of each install output stream an error keeps.
pub(super) const INSTALL_TAIL: usize = 64 * 1024;
/// After an install exits, how long to keep reading output that a process it started still
/// holds open. The install itself has no deadline.
const INSTALL_DRAIN: Duration = Duration::from_secs(30);
/// One shared bound for everything that confirms a query stopped: the kill, reaping the direct
/// child, the empty process group (Unix), and both readers reaching EOF. It bounds no query and
/// no install.
const QUERY_CLEANUP_GRACE: Duration = Duration::from_secs(5);
pub(super) const POLL: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

impl Captured {
    pub(crate) fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub(crate) fn hresult(&self) -> Option<u32> {
        #[expect(
            clippy::cast_sign_loss,
            reason = "winget reports HRESULTs as negative i32 codes"
        )]
        self.code.map(|code| code as u32)
    }

    pub(crate) fn status(&self) -> String {
        match self.code {
            #[expect(
                clippy::cast_sign_loss,
                reason = "show HRESULTs the way winget documents them"
            )]
            Some(code) if code < 0 => format!("exit code {:#010X}", code as u32),
            Some(code) => format!("exit code {code}"),
            None => "killed by a signal".to_owned(),
        }
    }

    /// The tail of what the manager printed, as a terminal would show it: a progress bar that
    /// redraws with `\r` keeps only its last state.
    pub(crate) fn tail(&self) -> String {
        let text = format!("{}\n{}", self.stdout.trim(), self.stderr.trim());
        let lines: Vec<&str> = text
            .lines()
            .filter_map(|line| line.trim_end_matches('\r').rsplit('\r').next())
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .collect();
        let tail = lines[lines.len().saturating_sub(20)..].join("\n");
        if tail.is_empty() {
            String::new()
        } else {
            format!(":\n{tail}")
        }
    }

    pub(crate) fn unexpected(&self) -> String {
        format!("unexpected {}{}", self.status(), self.tail())
    }
}

#[derive(Debug)]
pub(crate) enum QueryError {
    Spawn(io::Error),
    Wait(io::Error),
    Read(io::Error),
    TimedOut(Duration),
    Oversized {
        stream: &'static str,
        cap: usize,
    },
    /// Stopping the query could not be confirmed; `cause` is what ended the query, if it failed.
    Cleanup {
        cause: Option<Box<QueryError>>,
        detail: String,
    },
    /// The supervised background run was cancelled, so no query was started.
    Cancelled,
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Spawn(error) => write!(f, "cannot start it: {error}"),
            QueryError::Wait(error) => write!(f, "cannot wait for it: {error}"),
            QueryError::Read(error) => write!(f, "cannot read its output: {error}"),
            QueryError::TimedOut(deadline) => write!(
                f,
                "it did not finish within {} minutes, so loadout stopped it and every process it started",
                deadline.as_secs() / 60
            ),
            QueryError::Oversized { stream, cap } => write!(
                f,
                "it printed more than {} MiB on {stream}, so loadout stopped it rather than read a partial answer",
                cap / (1024 * 1024)
            ),
            QueryError::Cleanup { cause, detail } => {
                write!(
                    f,
                    "loadout could not confirm that it and every process it started stopped: {detail}"
                )?;
                match cause {
                    Some(cause) => write!(f, " (the query itself failed first: {cause})"),
                    None => Ok(()),
                }
            }
            QueryError::Cancelled => write!(
                f,
                "the background run was cancelled, so loadout started no further query"
            ),
        }
    }
}

/// Every manager runs with no stdin and with messages in the C locale, so a manager can't read
/// input and the not-installed messages that detection matches stay stable. (`LC_ALL` doesn't
/// change winget's language, which follows Windows' display language.)
fn command(invocation: &Invocation) -> Command {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .envs(invocation.env.iter().copied())
        .env("LC_ALL", "C")
        .env_remove(WORKER_NONCE)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Runs one query. In the foreground (`OwnGroup`) it is exactly U7's bounded query. In a
/// supervised background worker (`Inherited`) a cancelled run starts no query, every failure
/// cancels the run, and on Unix the query stays in the supervisor's group (see
/// `run_inherited_query`).
pub(crate) fn query(
    invocation: &Invocation,
    containment: &Containment,
) -> Result<Captured, QueryError> {
    let Containment::Inherited(supervised) = containment else {
        return run_query(invocation, QUERY_DEADLINE, QUERY_OUTPUT_CAP);
    };
    if supervised.cancel().is_cancelled() {
        return Err(QueryError::Cancelled);
    }
    #[cfg(unix)]
    let result = run_inherited_query(
        invocation,
        QUERY_DEADLINE,
        QUERY_OUTPUT_CAP,
        QUERY_CLEANUP_GRACE,
    );
    // Windows: the query's own job nests inside the supervisor's kill-on-close job, so ending
    // that job ends this one too.
    #[cfg(windows)]
    let result = run_query(invocation, QUERY_DEADLINE, QUERY_OUTPUT_CAP);
    if result.is_err() {
        supervised.cancel().cancel();
    }
    result
}

/// Runs a query in a process tree loadout can stop as a whole: on Unix a process group whose id
/// a loadout-owned anchor holds, on Windows a job object. Every outcome, success included, ends
/// with `settle`, and only a settled query's buffers are read.
fn run_query(
    invocation: &Invocation,
    deadline: Duration,
    cap: usize,
) -> Result<Captured, QueryError> {
    run_query_with(
        invocation,
        deadline,
        cap,
        QUERY_CLEANUP_GRACE,
        &mut Trace::default(),
    )
}

fn run_query_with(
    invocation: &Invocation,
    deadline: Duration,
    cap: usize,
    grace: Duration,
    trace: &mut Trace,
) -> Result<Captured, QueryError> {
    // One setup deadline, set before anything is spawned, bounds the cleanup of every setup
    // failure, with no renewed budget. Only a query that fully started gets its own deadline and
    // then its own settle.
    let setup = Instant::now() + grace;
    #[cfg(test)]
    {
        trace.setup_limit = Some(setup);
    }
    let mut query = Query::spawn(invocation, setup, trace)?;
    let (stdout, stdout_started) = query_reader(query.take_stdout(), cap, "stdout");
    let (stderr, stderr_started) = query_reader(query.take_stderr(), cap, "stderr");
    let started = stdout_started.and(stderr_started).map_err(QueryError::Read);
    #[cfg(windows)]
    let started = started.and_then(|()| query.start(setup));
    let (outcome, limit) = match started {
        Ok(()) => {
            let outcome = watch(|| query.try_exit(), &stdout, &stderr, deadline, cap);
            (outcome, Instant::now() + grace)
        }
        Err(error) => (Err(error), setup),
    };
    #[cfg(test)]
    {
        trace.disposal_limit = Some(limit);
    }
    if let Err(detail) = query.settle(&stdout, &stderr, limit, trace) {
        return Err(QueryError::Cleanup {
            cause: outcome.err().map(Box::new),
            detail,
        });
    }
    let code = outcome?;
    Ok(Captured {
        code,
        stdout: validated(&stdout, "stdout", cap)?,
        stderr: validated(&stderr, "stderr", cap)?,
    })
}

/// Waits for the query's own process to exit, stopping early on an oversized stream or at the
/// deadline. Whatever it leaves behind belongs to `settle`.
fn watch(
    mut try_exit: impl FnMut() -> io::Result<Option<ExitStatus>>,
    stdout: &Shared,
    stderr: &Shared,
    deadline: Duration,
    cap: usize,
) -> Result<Option<i32>, QueryError> {
    let limit = Instant::now() + deadline;
    loop {
        for (stream, buffer) in [("stdout", stdout), ("stderr", stderr)] {
            if lock(buffer).overflowed {
                return Err(QueryError::Oversized { stream, cap });
            }
        }
        if let Some(status) = try_exit().map_err(QueryError::Wait)? {
            return Ok(status.code());
        }
        if Instant::now() >= limit {
            return Err(QueryError::TimedOut(deadline));
        }
        thread::sleep(POLL);
    }
}

/// A query inside a supervised background worker (Unix). It runs in the group the supervisor
/// made for the worker, not a fresh one: a fresh group would outlive the supervisor's group kill.
/// Only the query's own, still unreaped process is ever signalled here; anything it started is
/// left to the supervisor, which kills and confirms the whole group after the worker exits. A
/// descendant still holding the output, or a query that can't be stopped and reaped, is a
/// `Cleanup` error, so the run is cancelled rather than credited.
#[cfg(unix)]
fn run_inherited_query(
    invocation: &Invocation,
    deadline: Duration,
    cap: usize,
    grace: Duration,
) -> Result<Captured, QueryError> {
    let setup = Instant::now() + grace;
    let mut child = command(invocation).spawn().map_err(QueryError::Spawn)?;
    let (stdout, stdout_started) = query_reader(child.stdout.take(), cap, "stdout");
    let (stderr, stderr_started) = query_reader(child.stderr.take(), cap, "stderr");
    let started = stdout_started.and(stderr_started).map_err(QueryError::Read);
    let (outcome, limit) = match started {
        Ok(()) => {
            let outcome = watch(|| child.try_wait(), &stdout, &stderr, deadline, cap);
            (outcome, Instant::now() + grace)
        }
        Err(error) => (Err(error), setup),
    };
    if let Err(detail) = settle_inherited(&mut child, &stdout, &stderr, limit, grace) {
        return Err(QueryError::Cleanup {
            cause: outcome.err().map(Box::new),
            detail,
        });
    }
    let code = outcome?;
    Ok(Captured {
        code,
        stdout: validated(&stdout, "stdout", cap)?,
        stderr: validated(&stderr, "stderr", cap)?,
    })
}

/// Stops an inherited query: SIGKILL to its own pid only while it is unreaped (so the pid is
/// still the query's), reap it, and require both readers' EOF, all within `limit`.
#[cfg(unix)]
fn settle_inherited(
    child: &mut Child,
    stdout: &Shared,
    stderr: &Shared,
    limit: Instant,
    grace: Duration,
) -> Result<(), String> {
    let mut problem = None;
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => {
            if let Err(error) = child.kill() {
                problem = Some(format!("cannot stop it: {error}"));
            }
        }
        Err(error) => problem = Some(format!("cannot check it: {error}")),
    }
    let reaped = until(limit, || {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("cannot reap it: {error}"))
    });
    first(&mut problem, reaped, "it has not exited");
    let drain = until(limit, || Ok(drained(stdout, stderr)));
    first(
        &mut problem,
        drain,
        "a process it started still holds its output open; the background supervisor stops it when the run ends",
    );
    problem.map_or(Ok(()), |detail| {
        Err(format!(
            "{detail} (within the {} second cleanup limit)",
            grace.as_secs()
        ))
    })
}

/// Polls `done` until it holds or `limit` passes; `Ok(false)` means time ran out.
pub(super) fn until(
    limit: Instant,
    mut done: impl FnMut() -> Result<bool, String>,
) -> Result<bool, String> {
    loop {
        if done()? {
            return Ok(true);
        }
        if Instant::now() >= limit {
            return Ok(false);
        }
        thread::sleep(POLL);
    }
}

fn drained(stdout: &Shared, stderr: &Shared) -> bool {
    lock(stdout).done && lock(stderr).done
}

/// Keeps the first thing that went wrong while cleanup carries on with the rest.
pub(super) fn first(problem: &mut Option<String>, step: Result<bool, String>, pending: &str) {
    let detail = match step {
        Ok(true) => return,
        Ok(false) => pending.to_owned(),
        Err(detail) => detail,
    };
    problem.get_or_insert(detail);
}

/// The query, and the anchor that holds its process group's id.
///
/// The anchor is `/bin/sh -c 'read -r line'`, the leader of a fresh group, blocked on a pipe only
/// loadout writes; if loadout dies, the pipe closes and the anchor exits. While loadout hasn't
/// reaped it, alive or as a zombie, the group id can't be reused, so every SIGKILL to the group is
/// sent before the anchor is reaped, and after that only signal 0. Only each child's own pid is
/// ever waited on, never the group, so nothing reaps the anchor early. A system without `/bin/sh`
/// can't run queries: the spawn fails, and detection is an error.
#[cfg(unix)]
pub(super) struct Query {
    child: Child,
    anchor: Child,
    anchor_input: Option<ChildStdin>,
    group: Pid,
}

#[cfg(unix)]
pub(super) const ANCHOR_SHELL: &str = "/bin/sh";

#[cfg(unix)]
impl Query {
    /// A query that fails to start still has its anchor released, within the setup deadline.
    fn spawn(
        invocation: &Invocation,
        setup: Instant,
        trace: &mut Trace,
    ) -> Result<Self, QueryError> {
        Self::spawn_in_group(command(invocation), setup, trace)
    }

    /// Starts `command` in a fresh group whose id a new anchor holds. The background supervisor
    /// starts its worker this way too, with its own stdio. A command that fails to start still
    /// has its anchor released, within the setup deadline.
    pub(super) fn spawn_in_group(
        mut command: Command,
        setup: Instant,
        trace: &mut Trace,
    ) -> Result<Self, QueryError> {
        let mut anchor = spawn_anchor(ANCHOR_SHELL).map_err(QueryError::Spawn)?;
        let anchor_input = anchor.stdin.take();
        #[cfg(test)]
        note(trace, Event::Anchor(anchor.id()));
        let spawned = i32::try_from(anchor.id())
            .map_err(|_| io::Error::other(format!("pid {} is out of range", anchor.id())))
            .and_then(|group| {
                command.process_group(group);
                command.spawn().map(|child| (child, Pid::from_raw(group)))
            });
        match spawned {
            Ok((child, group)) => {
                #[cfg(test)]
                note(trace, Event::Query(child.id()));
                Ok(Self {
                    child,
                    anchor,
                    anchor_input,
                    group,
                })
            }
            Err(error) => {
                #[cfg(test)]
                {
                    trace.disposal_limit = Some(setup);
                }
                match release_anchor(&mut anchor, anchor_input, setup, trace) {
                    Ok(()) => Err(QueryError::Spawn(error)),
                    Err(detail) => Err(QueryError::Cleanup {
                        cause: Some(Box::new(QueryError::Spawn(error))),
                        detail,
                    }),
                }
            }
        }
    }

    pub(super) fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub(super) fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    pub(super) fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// The process group the anchor holds.
    pub(super) fn group(&self) -> Pid {
        self.group
    }

    /// Waits on the query's own pid only.
    pub(super) fn try_exit(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Stops the query and confirms it within `limit`, which one shared grace sets for every step.
    /// While the anchor is unreaped (phase A) the group is killed, the query reaped, the readers
    /// drained, and the group killed again for processes forked during the first kill. Then the
    /// anchor is reaped, and from there (phase B) only signal 0 checks the group is empty. Every
    /// step runs even after one fails, and the first failure is reported.
    fn settle(
        &mut self,
        stdout: &Shared,
        stderr: &Shared,
        limit: Instant,
        trace: &mut Trace,
    ) -> Result<(), String> {
        self.settle_with(
            || drained(stdout, stderr),
            limit,
            QUERY_CLEANUP_GRACE,
            trace,
        )
    }

    /// `settle` with the caller's own "output fully read" test and the grace its message names.
    /// The background supervisor uses it with its log drains and its own settle limit; the steps
    /// and their order are exactly the query's.
    pub(super) fn settle_with(
        &mut self,
        mut drained: impl FnMut() -> bool,
        limit: Instant,
        grace: Duration,
        trace: &mut Trace,
    ) -> Result<(), String> {
        let mut problem = None;
        let first_kill = self.signal(Some(Signal::SIGKILL), Call::FirstKill, trace);
        if let Some(detail) = first_kill_problem(first_kill) {
            problem.get_or_insert(detail);
        }
        let reaped = until(limit, || {
            self.child
                .try_wait()
                .map(|status| status.is_some())
                .map_err(|error| format!("cannot reap it: {error}"))
        });
        if reaped == Ok(true) {
            note(trace, Event::ReapedQuery);
        }
        first(&mut problem, reaped, "it has not exited");
        let drain = until(limit, || Ok(drained()));
        if drain == Ok(true) {
            note(trace, Event::ReadersDone);
        }
        first(&mut problem, drain, "its output is still held open");
        let second = self.signal(Some(Signal::SIGKILL), Call::SecondKill, trace);
        let pending = match second_kill(second) {
            Ok(pending) => pending,
            Err(detail) => {
                problem.get_or_insert(detail);
                None
            }
        };
        let input = self.anchor_input.take();
        if let Err(detail) = release_anchor(&mut self.anchor, input, limit, trace) {
            problem.get_or_insert(detail);
        }
        let mut last = Probe::Members;
        let freed = until(limit, || {
            last = probe(self.signal(None, Call::Probe, trace))?;
            Ok(last == Probe::Freed)
        });
        note(trace, Event::Probed);
        match freed {
            Ok(true) => {}
            Ok(false) => {
                problem.get_or_insert(still_present(last, pending));
            }
            Err(detail) => {
                problem.get_or_insert(detail);
            }
        }
        match problem {
            None => Ok(()),
            Some(detail) => Err(format!(
                "{detail} (within the {} second cleanup limit)",
                grace.as_secs()
            )),
        }
    }

    /// One signal to the whole group, returning the raw result, which tests record after the
    /// call. `settle` sends SIGKILL only while the anchor is unreaped, so the id is still ours;
    /// after the anchor's reap it sends only signal 0 (`None`).
    fn signal(&self, signal: Option<Signal>, call: Call, trace: &mut Trace) -> Result<(), Errno> {
        if signal.is_some() {
            note(trace, Event::KilledGroup);
        }
        let result = killpg(self.group, signal);
        #[cfg(test)]
        note(trace, Event::Signal(call, result));
        #[cfg(not(test))]
        let _ = call;
        result
    }
}

/// The anchor: the leader of a fresh process group, blocked reading a pipe only loadout writes,
/// with no access to loadout's own stdio.
#[cfg(unix)]
fn spawn_anchor(shell: &str) -> io::Result<Child> {
    Command::new(shell)
        .args(["-c", "read -r line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}

/// Ends the anchor: SIGKILL to its own pid (it is unreaped, so the pid is still the anchor's),
/// close its input, and reap it within `limit`. An anchor that ended by anything but that kill
/// means something outside loadout interfered with the group, which is an error.
#[cfg(unix)]
fn release_anchor(
    anchor: &mut Child,
    input: Option<ChildStdin>,
    limit: Instant,
    trace: &mut Trace,
) -> Result<(), String> {
    let mut problem = None;
    if let Err(error) = anchor.kill() {
        problem = Some(format!("cannot stop loadout's group anchor: {error}"));
    }
    drop(input);
    let mut status = None;
    let reaped = until(limit, || {
        status = anchor
            .try_wait()
            .map_err(|error| format!("cannot reap loadout's group anchor: {error}"))?;
        Ok(status.is_some())
    });
    if reaped == Ok(true) {
        note(trace, Event::ReapedAnchor);
    }
    first(&mut problem, reaped, "loadout's group anchor did not exit");
    if let Some(status) = status
        && status.signal() != Some(Signal::SIGKILL as i32)
    {
        problem.get_or_insert(format!(
            "loadout's group anchor ended before loadout stopped it ({status})"
        ));
    }
    problem.map_or(Ok(()), Err)
}

/// Which group signal a result came from.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    FirstKill,
    SecondKill,
    Probe,
}

/// What a signal-0 probe of the group says once the anchor is reaped.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// Some member took the signal: something is still running.
    Members,
    /// The group exists but has no member loadout may signal. On macOS a group whose members
    /// are all zombies answers this way (XNU skips zombies and then returns EPERM), and so does
    /// a group holding another user's process.
    Unsignalable,
    /// `ESRCH`: the group was freed, so every member has been reaped. The only "gone".
    Freed,
}

/// The first SIGKILL is sent while the anchor is alive and loadout's own, so the group has a
/// member loadout may signal. Any error here, `ESRCH` and `EPERM` included, is real.
#[cfg(unix)]
fn first_kill_problem(result: Result<(), Errno>) -> Option<String> {
    result.err().map(|error| {
        format!("cannot stop it: the first kill of its process group failed ({error})")
    })
}

/// By the second SIGKILL the anchor (still unreaped) may already be a zombie, and a group of
/// only zombies answers `EPERM` on macOS. That is inconclusive, never success: it stays pending
/// until a later probe finds the group freed. Every other error is real.
#[cfg(unix)]
fn second_kill(result: Result<(), Errno>) -> Result<Option<Errno>, String> {
    match result {
        Ok(()) => Ok(None),
        Err(Errno::EPERM) => Ok(Some(Errno::EPERM)),
        Err(error) => Err(format!(
            "cannot stop it: the second kill of its process group failed ({error})"
        )),
    }
}

/// One signal-0 probe of the group. Only `ESRCH` means it is gone; `Ok` and `EPERM` mean wait.
/// Signal 0 delivers nothing, so a reused id can only make cleanup fail, never harm a group.
#[cfg(unix)]
fn probe(result: Result<(), Errno>) -> Result<Probe, String> {
    match result {
        Ok(()) => Ok(Probe::Members),
        Err(Errno::EPERM) => Ok(Probe::Unsignalable),
        Err(Errno::ESRCH) => Ok(Probe::Freed),
        Err(error) => Err(format!("cannot check its process group: {error}")),
    }
}

/// Why the group still exists when the cleanup limit runs out, with any pending second-kill
/// refusal that no `ESRCH` cleared.
#[cfg(unix)]
fn still_present(last: Probe, pending: Option<Errno>) -> String {
    let what = match last {
        Probe::Unsignalable => "its process group still has members loadout can't signal (EPERM)",
        Probe::Members | Probe::Freed => "processes it started are still running",
    };
    match pending {
        Some(error) => format!("{what}, and the second kill of the group was refused ({error})"),
        None => what.to_owned(),
    }
}

/// The result of terminating the job. Every error is real.
#[cfg(windows)]
fn killed(result: io::Result<()>) -> Result<(), String> {
    result.map_err(|error| format!("cannot stop it: {error}"))
}

/// On Windows the query runs in a job object, addressed through loadout's own handle, so there
/// is no id to be reused. No safe API reports that a job has no processes left, so the job's
/// termination, the query's exit, and both readers' EOF are the whole confirmation.
///
/// The query is created suspended. `process-wrap` sees loadout's own `CREATE_SUSPENDED` and so
/// never resumes it; loadout resumes it in `start`, once it owns the job. `root` is loadout's own
/// hold on the process, kept from the moment it exists. `setup` is the setup deadline the
/// innermost wrapper's `wait` is bounded by, cleared only once the query has fully started.
#[cfg(windows)]
struct Query {
    child: Box<dyn ChildWrapper>,
    root: Arc<Mutex<Child>>,
    setup: Arc<Mutex<Option<Instant>>>,
}

#[cfg(windows)]
impl Query {
    /// Until the job exists, loadout owns the child. If anything fails before then, the child has
    /// never run (nothing resumed it), so stopping that one process is complete. That is bounded
    /// by the setup deadline, shared with `process-wrap`'s own wait on the child. The deadline
    /// stays in force after the job is loadout's, until `start` succeeds.
    fn spawn(
        invocation: &Invocation,
        limit: Instant,
        trace: &mut Trace,
    ) -> Result<Self, QueryError> {
        let setup = Arc::new(Mutex::new(Some(limit)));
        let mut root = None;
        let mut wrapped = CommandWrap::from(command(invocation));
        wrapped.wrap(CreationFlags(loadout_winproc::CREATE_SUSPENDED));
        wrapped.wrap(JobObject);
        let spawned = wrapped.spawn_with_child(|command| {
            let child = Arc::new(Mutex::new(command.spawn()?));
            root = Some(Arc::clone(&child));
            #[cfg(test)]
            if let Some(error) = seam::before_duplicate(limit) {
                return Err(error);
            }
            let leaf = Leaf::new(child, Arc::clone(&setup))?;
            Ok(Box::new(leaf) as Box<dyn ChildWrapper>)
        });
        #[cfg(test)]
        {
            trace.root = root.as_ref().map(|root| guard(root).id());
        }
        #[cfg(not(test))]
        let _ = trace;
        match (spawned, root) {
            (Ok(child), Some(root)) => Ok(Self { child, root, setup }),
            (Err(error), None) => Err(QueryError::Spawn(error)),
            (Err(error), Some(root)) => {
                #[cfg(test)]
                {
                    trace.disposal_limit = Some(limit);
                }
                match dispose_never_run(&root, limit) {
                    Ok(()) => Err(QueryError::Spawn(error)),
                    Err(detail) => Err(QueryError::Cleanup {
                        cause: Some(Box::new(QueryError::Spawn(error))),
                        detail,
                    }),
                }
            }
            (Ok(_), None) => unreachable!("process-wrap returned a child loadout did not create"),
        }
    }

    /// Resumes the suspended query within the setup deadline `limit`. Loadout owns the job by
    /// now, so any error, even one after the thread started, leaves the whole job to `settle`,
    /// within the same deadline, with this error as the cause.
    ///
    /// The deadline is checked on entry, then again by the helper after all its preparation
    /// (finding, opening and checking the thread), as the last step before `ResumeThread`: a
    /// query is never knowingly resumed late. The clock check and the resume aren't one atomic
    /// step, so a resume can still land just after the deadline. That is why setup succeeds
    /// only if the deadline still hasn't passed once the resume returns; otherwise it is a
    /// setup timeout, and the query, which may now be running, is stopped with its whole job.
    fn start(&self, limit: Instant) -> Result<(), QueryError> {
        #[cfg(test)]
        seam::before_resume(limit);
        if Instant::now() >= limit {
            return Err(setup_timeout("so loadout did not resume it"));
        }
        let prepared = loadout_winproc::prepare(&guard(&self.root)).map_err(QueryError::Spawn)?;
        #[cfg(test)]
        seam::after_prepare(limit);
        let resumed = prepared.resume_before(limit);
        #[cfg(test)]
        let resumed = resumed.and_then(|()| seam::after_resume(limit));
        resumed.map_err(QueryError::Spawn)?;
        if Instant::now() >= limit {
            return Err(setup_timeout(
                "and it was resumed only as that limit passed, so loadout stopped it",
            ));
        }
        *guard(&self.setup) = None;
        Ok(())
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout().take()
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr().take()
    }

    fn try_exit(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    fn settle(
        &mut self,
        stdout: &Shared,
        stderr: &Shared,
        limit: Instant,
        trace: &mut Trace,
    ) -> Result<(), String> {
        let mut problem = None;
        #[cfg(test)]
        {
            trace.job_kills += 1;
        }
        #[cfg(not(test))]
        let _ = trace;
        first(
            &mut problem,
            killed(self.child.start_kill()).map(|()| true),
            "",
        );
        let reaped = until(limit, || {
            self.child
                .try_wait()
                .map(|status| status.is_some())
                .map_err(|error| format!("cannot reap it: {error}"))
        });
        first(&mut problem, reaped, "it has not exited");
        let drain = until(limit, || Ok(drained(stdout, stderr)));
        first(&mut problem, drain, "its output is still held open");
        match problem {
            None => Ok(()),
            Some(detail) => Err(format!(
                "{detail} (within the {} second cleanup limit)",
                QUERY_CLEANUP_GRACE.as_secs()
            )),
        }
    }
}

#[cfg(windows)]
fn setup_timeout(outcome: &str) -> QueryError {
    QueryError::Spawn(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "its setup did not finish within the {} second setup limit, {outcome}",
            QUERY_CLEANUP_GRACE.as_secs()
        ),
    ))
}

/// The innermost child `process-wrap` wraps: loadout's own process, shared with `Query::root`
/// so loadout keeps it even if `process-wrap` drops this wrapper on a setup error. Its `wait` is
/// bounded by the setup deadline, and refuses to block once the query has started.
#[cfg(windows)]
#[derive(Debug)]
struct Leaf {
    child: Arc<Mutex<Child>>,
    handle: OwnedHandle,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    id: u32,
    setup: Arc<Mutex<Option<Instant>>>,
    #[cfg(test)]
    hide_handle: bool,
}

#[cfg(windows)]
impl Leaf {
    fn new(child: Arc<Mutex<Child>>, setup: Arc<Mutex<Option<Instant>>>) -> io::Result<Self> {
        let mut process = guard(&child);
        let handle = process.as_handle().try_clone_to_owned()?;
        let stdin = process.stdin.take();
        let stdout = process.stdout.take();
        let stderr = process.stderr.take();
        let id = process.id();
        drop(process);
        Ok(Self {
            child,
            handle,
            stdin,
            stdout,
            stderr,
            id,
            setup,
            #[cfg(test)]
            hide_handle: seam::is(&seam::Seam::HideHandle),
        })
    }
}

#[cfg(windows)]
impl ChildWrapper for Leaf {
    fn inner(&self) -> &dyn ChildWrapper {
        self
    }

    fn inner_mut(&mut self) -> &mut dyn ChildWrapper {
        self
    }

    fn into_inner(self: Box<Self>) -> Box<dyn ChildWrapper> {
        self
    }

    fn process_handle(&self) -> Option<BorrowedHandle<'_>> {
        #[cfg(test)]
        if self.hide_handle {
            return None;
        }
        Some(self.handle.as_handle())
    }

    fn stdin(&mut self) -> &mut Option<ChildStdin> {
        &mut self.stdin
    }

    fn stdout(&mut self) -> &mut Option<ChildStdout> {
        &mut self.stdout
    }

    fn stderr(&mut self) -> &mut Option<ChildStderr> {
        &mut self.stderr
    }

    fn id(&self) -> u32 {
        self.id
    }

    fn start_kill(&mut self) -> io::Result<()> {
        guard(&self.child).kill()
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        guard(&self.child).try_wait()
    }

    fn wait(&mut self) -> io::Result<ExitStatus> {
        let Some(limit) = *guard(&self.setup) else {
            return Err(io::Error::other(
                "loadout waits on a query only within a deadline",
            ));
        };
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= limit {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the query did not exit before its setup cleanup deadline",
                ));
            }
            thread::sleep(POLL);
        }
    }
}

/// Stops a query whose job setup failed. Nothing resumed it, so it never ran and started
/// nothing: killing and reaping this one process within `limit` is the whole cleanup. No job is
/// assumed to exist yet, so only loadout's own retained handle on the process is used.
///
/// The kill is skipped only when the process has positively exited. A first observation that
/// fails is no such proof, so the kill and the reap still follow, and every problem is kept, in
/// order: the observation, the kill, the reap.
#[cfg(windows)]
fn dispose_never_run(root: &Mutex<Child>, limit: Instant) -> Result<(), String> {
    let mut child = guard(root);
    let mut problems = Vec::new();
    #[cfg(test)]
    let observed = seam::observation().map_or_else(|| child.try_wait(), Err);
    #[cfg(not(test))]
    let observed = child.try_wait();
    match observed {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(error) => problems.push(format!("cannot check the query loadout created: {error}")),
    }
    #[cfg(test)]
    seam::count_kill();
    if let Err(error) = child.kill() {
        problems.push(format!("cannot stop the query loadout created: {error}"));
    }
    let mut reap = None;
    let reaped = until(limit, || {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("cannot reap the query loadout created: {error}"))
    });
    first(&mut reap, reaped, "the query loadout created has not exited");
    problems.extend(reap);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} (within the {} second cleanup limit)",
            problems.join("; "),
            QUERY_CLEANUP_GRACE.as_secs()
        ))
    }
}

#[cfg(windows)]
fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Test-only fault injection for query setup. Each seam reaches exactly one branch. The
/// deadline seams wait for the real setup deadline to pass, so the branch after them runs with
/// an exhausted budget; they move no clock.
#[cfg(test)]
mod seam {
    use std::cell::RefCell;
    use std::io;
    #[cfg(windows)]
    use std::path::PathBuf;
    #[cfg(windows)]
    use std::thread;
    #[cfg(windows)]
    use std::time::{Duration, Instant};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Seam {
        /// Refuses to start the reader of this stream, as if no thread could be created.
        FailReader(&'static str),
        /// Fails right after loadout took the child, before the handle is duplicated.
        #[cfg(windows)]
        FailDuplicate,
        /// As `FailDuplicate`, but only once the setup deadline has passed.
        #[cfg(windows)]
        FailDuplicateAtDeadline,
        /// Withholds the process handle, so `process-wrap` takes its own no-handle branch.
        #[cfg(windows)]
        HideHandle,
        /// Holds `start` until the setup deadline has passed, so it must refuse to resume.
        #[cfg(windows)]
        ExhaustBeforeResume,
        /// Holds `start` after the thread was found, opened and checked, until the setup deadline
        /// has passed, so the helper's final check must refuse to call `ResumeThread`.
        #[cfg(windows)]
        ExpireAfterPrepare,
        /// Holds `start` for this long after preparation, with time still left: a positive.
        #[cfg(windows)]
        PauseAfterPrepare(Duration),
        /// After a real, successful resume, holds `start` until the setup deadline has passed, so
        /// the recheck must turn the late resume into a setup timeout.
        #[cfg(windows)]
        ExpireAfterResume,
        /// After a real resume, waits until the test's descendant writes this file, then fails.
        #[cfg(windows)]
        FailAfterResume(PathBuf),
        /// The first observation of a never-run query in `dispose_never_run` fails, once.
        #[cfg(windows)]
        FailObservation,
    }

    thread_local! {
        static SEAM: RefCell<Option<Seam>> = const { RefCell::new(None) };
        /// How many kills `dispose_never_run` attempted since the last `set`.
        #[cfg(windows)]
        static KILLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn set(seam: Option<Seam>) {
        SEAM.with(|cell| *cell.borrow_mut() = seam);
        #[cfg(windows)]
        KILLS.with(|kills| kills.set(0));
    }

    /// The injected first-observation failure, consumed so later observations are real.
    #[cfg(windows)]
    pub(super) fn observation() -> Option<io::Error> {
        SEAM.with(|cell| {
            let mut seam = cell.borrow_mut();
            (seam.as_ref() == Some(&Seam::FailObservation)).then(|| {
                *seam = None;
                io::Error::other("injected: the query could not be observed")
            })
        })
    }

    #[cfg(windows)]
    pub(super) fn count_kill() {
        KILLS.with(|kills| kills.set(kills.get() + 1));
    }

    #[cfg(windows)]
    pub(super) fn kills() -> u32 {
        KILLS.with(std::cell::Cell::get)
    }

    #[cfg(windows)]
    pub(super) fn is(wanted: &Seam) -> bool {
        SEAM.with(|cell| cell.borrow().as_ref() == Some(wanted))
    }

    pub(super) fn reader(stream: &str) -> Option<io::Error> {
        SEAM.with(|cell| match cell.borrow().as_ref() {
            Some(Seam::FailReader(failing)) if *failing == stream => Some(io::Error::other(
                format!("injected: cannot start the {stream} reader"),
            )),
            _ => None,
        })
    }

    #[cfg(windows)]
    fn wait_out(limit: Instant) {
        while Instant::now() < limit {
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(windows)]
    pub(super) fn before_duplicate(limit: Instant) -> Option<io::Error> {
        if is(&Seam::FailDuplicateAtDeadline) {
            wait_out(limit);
        } else if !is(&Seam::FailDuplicate) {
            return None;
        }
        Some(io::Error::other("injected: handle duplication failed"))
    }

    #[cfg(windows)]
    pub(super) fn before_resume(limit: Instant) {
        if is(&Seam::ExhaustBeforeResume) {
            wait_out(limit);
        }
    }

    #[cfg(windows)]
    pub(super) fn after_prepare(limit: Instant) {
        let pause = SEAM.with(|cell| match cell.borrow().as_ref() {
            Some(Seam::PauseAfterPrepare(pause)) => Some(*pause),
            _ => None,
        });
        if let Some(pause) = pause {
            thread::sleep(pause);
        } else if is(&Seam::ExpireAfterPrepare) {
            wait_out(limit);
        }
    }

    /// `FailAfterResume`'s readiness wait is the test's own bound. It is longer than the
    /// production setup deadline, so a test using that seam supplies a longer setup deadline of
    /// its own.
    #[cfg(windows)]
    pub(super) fn after_resume(limit: Instant) -> io::Result<()> {
        if is(&Seam::ExpireAfterResume) {
            wait_out(limit);
            return Ok(());
        }
        let ready = SEAM.with(|cell| match cell.borrow().as_ref() {
            Some(Seam::FailAfterResume(ready)) => Some(ready.clone()),
            _ => None,
        });
        let Some(ready) = ready else {
            return Ok(());
        };
        let limit = Instant::now() + Duration::from_secs(20);
        while !ready.exists() {
            if Instant::now() >= limit {
                return Err(io::Error::other(
                    "injected: the descendant never reported ready",
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err(io::Error::other(
            "injected: failure after the query was resumed",
        ))
    }
}

/// What tests check, recorded only in tests: the order of signals and reaps (Unix), to check
/// that no SIGKILL follows the anchor's reap; the id of the process loadout created and each job
/// termination attempt (Windows); and the absolute setup deadline and the limit actually given
/// to cleanup, to check that a setup failure's cleanup gets no renewed budget.
#[derive(Default)]
pub(super) struct Trace {
    #[cfg(all(test, unix))]
    events: Vec<Event>,
    #[cfg(all(test, windows))]
    root: Option<u32>,
    #[cfg(all(test, windows))]
    job_kills: u32,
    #[cfg(test)]
    setup_limit: Option<Instant>,
    #[cfg(test)]
    disposal_limit: Option<Instant>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    #[cfg(test)]
    Anchor(u32),
    #[cfg(test)]
    Query(u32),
    /// A group signal's raw result, recorded after the call.
    #[cfg(test)]
    Signal(Call, Result<(), Errno>),
    KilledGroup,
    ReapedQuery,
    ReadersDone,
    ReapedAnchor,
    Probed,
}

#[cfg(unix)]
fn note(trace: &mut Trace, event: Event) {
    #[cfg(test)]
    trace.events.push(event);
    #[cfg(not(test))]
    let _ = (trace, event);
}

/// A settled stream's final state, read under one lock: an overflow first, then a read error,
/// then the bytes. The reader marks itself done only after its last write, so this is final.
fn validated(
    buffer: &Mutex<Buffer>,
    stream: &'static str,
    cap: usize,
) -> Result<String, QueryError> {
    let mut buffer = lock(buffer);
    if buffer.overflowed {
        return Err(QueryError::Oversized { stream, cap });
    }
    if let Some(error) = buffer.error.take() {
        return Err(QueryError::Read(error));
    }
    Ok(String::from_utf8_lossy(&std::mem::take(&mut buffer.bytes)).into_owned())
}

/// Runs an install in loadout's own process group, so `sudo` can still prompt on the terminal
/// and Ctrl-C reaches it. There is no deadline: installs legitimately take minutes. Only the
/// tail of each output stream is kept. In a supervised background worker the installer is in
/// the supervisor's group or job like the worker itself, which bounds it; a cancelled run starts
/// no installer.
pub(crate) fn install(invocation: &Invocation, containment: &Containment) -> io::Result<Captured> {
    if containment.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "the background run was cancelled, so loadout started no installer",
        ));
    }
    run_install(invocation, INSTALL_TAIL, INSTALL_DRAIN)
}

fn run_install(invocation: &Invocation, keep: usize, drain: Duration) -> io::Result<Captured> {
    let mut child = command(invocation).spawn()?;
    let stdout = reader(child.stdout.take(), Keep::Tail(keep));
    let stderr = reader(child.stderr.take(), Keep::Tail(keep));
    let status = child.wait()?;
    let limit = Instant::now() + drain;
    while !(lock(&stdout).done && lock(&stderr).done) && Instant::now() < limit {
        thread::sleep(POLL);
    }
    let mut stderr_text = text(&stderr);
    if !(lock(&stdout).done && lock(&stderr).done) {
        stderr_text.push_str(
            "\n(loadout stopped reading output that a process started by the installer still holds open)",
        );
    }
    Ok(Captured {
        code: status.code(),
        stdout: text(&stdout),
        stderr: stderr_text,
    })
}

#[derive(Clone, Copy)]
enum Keep {
    All(usize),
    Tail(usize),
}

#[derive(Default)]
struct Buffer {
    bytes: Vec<u8>,
    overflowed: bool,
    done: bool,
    error: Option<io::Error>,
}

impl Buffer {
    fn push(&mut self, chunk: &[u8], keep: Keep) {
        self.bytes.extend_from_slice(chunk);
        match keep {
            Keep::All(cap) => self.overflowed = self.bytes.len() > cap,
            Keep::Tail(keep) => {
                if self.bytes.len() > keep {
                    let excess = self.bytes.len() - keep;
                    self.bytes.drain(..excess);
                }
            }
        }
    }
}

type Shared = Arc<Mutex<Buffer>>;

fn lock(buffer: &Mutex<Buffer>) -> MutexGuard<'_, Buffer> {
    buffer.lock().unwrap_or_else(PoisonError::into_inner)
}

fn text(buffer: &Mutex<Buffer>) -> String {
    String::from_utf8_lossy(&lock(buffer).bytes).into_owned()
}

/// Reads one output stream on its own thread, so a manager can never block on a full pipe
/// while loadout waits for it to exit. A reader that can't start marks its stream with the error.
fn reader<R: Read + Send + 'static>(source: Option<R>, keep: Keep) -> Shared {
    let (shared, started) = start_reader(source, keep);
    if let Err(error) = started {
        lock(&shared).error = Some(error);
    }
    shared
}

/// A query's reader. One that can't start is a setup failure, returned at once, so the query is
/// stopped before it is resumed (Windows) or watched, not discovered at its deadline.
fn query_reader<R: Read + Send + 'static>(
    source: Option<R>,
    cap: usize,
    stream: &str,
) -> (Shared, io::Result<()>) {
    #[cfg(test)]
    if let Some(error) = seam::reader(stream) {
        drop(source);
        let shared = Shared::default();
        lock(&shared).done = true;
        return (shared, Err(error));
    }
    let (shared, started) = start_reader(source, Keep::All(cap));
    let started = started.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("cannot start the {stream} reader: {error}"),
        )
    });
    (shared, started)
}

/// Starts the reader thread. If it can't start, the stream is marked done and the error is
/// returned to the caller, which decides what it means.
fn start_reader<R: Read + Send + 'static>(
    source: Option<R>,
    keep: Keep,
) -> (Shared, io::Result<()>) {
    let shared = Shared::default();
    let Some(mut source) = source else {
        lock(&shared).done = true;
        return (shared, Ok(()));
    };
    let sink = Arc::clone(&shared);
    let spawned = thread::Builder::new().spawn(move || {
        let mut chunk = [0_u8; 8192];
        loop {
            match source.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    let mut buffer = lock(&sink);
                    buffer.push(&chunk[..read], keep);
                    if buffer.overflowed {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    lock(&sink).error = Some(error);
                    break;
                }
            }
        }
        lock(&sink).done = true;
    });
    match spawned {
        Ok(_) => (shared, Ok(())),
        Err(error) => {
            lock(&shared).done = true;
            (shared, Err(error))
        }
    }
}

/// Bounded handling for the processes tests start themselves (helpers such as `ps` or
/// `tasklist`, and fixture children). No test waits on one without a bound: past it, the process
/// is killed and reaped, and the test fails.
#[cfg(test)]
mod fixture {
    use std::cell::Cell;
    use std::io::{self, Read};
    use std::process::{Child, Command, ExitStatus, Output, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A generous bound for a short helper command.
    pub(super) const HELPER: Duration = Duration::from_secs(20);

    thread_local! {
        /// Test-only: the stream whose drainer `drain` refuses to start, as if no thread could
        /// be created.
        static FAIL_DRAINER: Cell<Option<&'static str>> = const { Cell::new(None) };
        /// The id of the last helper `output` started, so a test can check it is gone.
        static LAST_HELPER: Cell<Option<u32>> = const { Cell::new(None) };
    }

    pub(super) fn fail_drainer(stream: Option<&'static str>) {
        FAIL_DRAINER.with(|cell| cell.set(stream));
    }

    pub(super) fn last_helper() -> Option<u32> {
        LAST_HELPER.with(Cell::get)
    }

    /// Runs a helper command to completion within `limit`, reading its output on two drainer
    /// threads. The child is owned from the moment it exists, before either drainer starts, so
    /// every later failure, a panic included, kills and reaps it within a bound.
    ///
    /// An error is either the spawn itself (`NotFound` if there is no such program) or a
    /// drainer that couldn't start, after the child was stopped. Not finishing in time fails the
    /// test. Waiting for the output is bounded only for this caller: a drainer still blocked on
    /// a pipe some other process holds open keeps running, and nothing cancels it. Helper output
    /// is short, read whole, and read errors are not reported.
    pub(super) fn output(command: &mut Command, limit: Duration) -> io::Result<Output> {
        let started = Instant::now();
        let mut owned = Owned::new(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        let child = owned.child.as_mut().expect("just spawned");
        LAST_HELPER.with(|cell| cell.set(Some(child.id())));
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let stdout = drain(stdout, "stdout")?;
        let stderr = match drain(stderr, "stderr") {
            Ok(stderr) => stderr,
            Err(error) => {
                let stopped = owned.stop(Duration::from_secs(10));
                let first = match stdout.recv_timeout(Duration::from_secs(5)) {
                    Ok(_) => "finished",
                    Err(_) => "still running (nothing cancels it)",
                };
                return Err(io::Error::other(format!(
                    "{error}; the helper was stopped: {stopped:?}; the stdout drainer is {first}"
                )));
            }
        };
        let status = owned
            .wait_within(limit.saturating_sub(started.elapsed()))
            .unwrap_or_else(|| panic!("the test helper {command:?} did not exit within {limit:?}"));
        let rest = limit.saturating_sub(started.elapsed()) + Duration::from_secs(1);
        Ok(Output {
            status,
            stdout: collect(&stdout, rest, command),
            stderr: collect(&stderr, rest, command),
        })
    }

    /// Starts a drainer with `thread::Builder`, so a thread that can't be created is an error,
    /// not a panic.
    fn drain(
        source: Option<impl Read + Send + 'static>,
        stream: &'static str,
    ) -> io::Result<mpsc::Receiver<Vec<u8>>> {
        let (sender, receiver) = mpsc::channel();
        if FAIL_DRAINER.with(Cell::get) == Some(stream) {
            return Err(io::Error::other(format!(
                "injected: cannot start the {stream} drainer"
            )));
        }
        let Some(mut source) = source else {
            let _ = sender.send(Vec::new());
            return Ok(receiver);
        };
        thread::Builder::new()
            .spawn(move || {
                let mut bytes = Vec::new();
                let _ = source.read_to_end(&mut bytes);
                let _ = sender.send(bytes);
            })
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot start the {stream} drainer: {error}"),
                )
            })?;
        Ok(receiver)
    }

    fn collect(receiver: &mpsc::Receiver<Vec<u8>>, limit: Duration, command: &Command) -> Vec<u8> {
        receiver
            .recv_timeout(limit)
            .unwrap_or_else(|_| panic!("the test helper {command:?} still holds its output open"))
    }

    /// A child the test started itself. Dropped without `wait_within` succeeding (an assertion
    /// failed, or the test returned early), it is killed and reaped within a bound; if that can't
    /// be confirmed, the test fails, unless it is already failing.
    pub(super) struct Owned {
        child: Option<Child>,
    }

    impl Owned {
        pub(super) fn new(child: Child) -> Self {
            Self { child: Some(child) }
        }

        #[cfg(unix)]
        pub(super) fn child(&mut self) -> &mut Child {
            self.child
                .as_mut()
                .expect("the fixture child was already reaped")
        }

        #[cfg(unix)]
        pub(super) fn id(&self) -> u32 {
            self.child
                .as_ref()
                .expect("the fixture child was already reaped")
                .id()
        }

        /// Waits for the child within `limit`. `None` if it is still running then; it stays owned,
        /// and is killed when dropped.
        pub(super) fn wait_within(&mut self, limit: Duration) -> Option<ExitStatus> {
            self.poll(limit)
                .unwrap_or_else(|error| panic!("cannot wait for the fixture child: {error}"))
        }

        fn poll(&mut self, limit: Duration) -> io::Result<Option<ExitStatus>> {
            let until = Instant::now() + limit;
            loop {
                let Some(child) = self.child.as_mut() else {
                    return Err(io::Error::other("the fixture child was already reaped"));
                };
                if let Some(status) = child.try_wait()? {
                    self.child = None;
                    return Ok(Some(status));
                }
                if Instant::now() >= until {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        /// Kills the child and reaps it within `limit`. An error is the reason it can't be
        /// confirmed gone.
        pub(super) fn stop(&mut self, limit: Duration) -> Result<(), String> {
            let Some(child) = self.child.as_mut() else {
                return Ok(());
            };
            let id = child.id();
            let killed = child.kill();
            match self.poll(limit) {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(format!(
                    "the fixture child {id} did not exit within {limit:?} of being killed ({killed:?})"
                )),
                Err(error) => Err(format!("cannot reap the fixture child {id}: {error}")),
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            if let Err(detail) = self.stop(Duration::from_secs(10)) {
                if thread::panicking() {
                    eprintln!("{detail}");
                } else {
                    panic!("{detail}");
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;

    use nix::sys::signal::kill;

    use super::*;

    fn sh(script: &str) -> Invocation {
        Invocation::new(Path::new("/bin/sh"), ["-c", script])
    }

    #[test]
    fn a_query_that_never_finishes_is_stopped_at_its_deadline() {
        let started = Instant::now();
        let error = run_query(&sh("sleep 30"), Duration::from_millis(300), 1024).unwrap_err();
        assert!(matches!(error, QueryError::TimedOut(_)), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// Signal 0, sent directly, so the check itself starts no process. `EPERM` (it exists but
    /// isn't ours) counts as alive.
    fn alive(pid: &str) -> bool {
        match kill(Pid::from_raw(pid.parse().unwrap()), None) {
            Ok(()) | Err(Errno::EPERM) => true,
            Err(Errno::ESRCH) => false,
            Err(error) => panic!("cannot check process {pid}: {error}"),
        }
    }

    #[test]
    fn a_descendant_holding_the_output_is_gone_when_the_query_returns() {
        let captured = run_query(
            &sh("sleep 30 & echo $!; echo done"),
            Duration::from_secs(20),
            1024,
        )
        .unwrap();
        assert_eq!(captured.code, Some(0));
        let mut lines = captured.stdout.lines();
        let pid = lines.next().unwrap().to_owned();
        assert_eq!(lines.next(), Some("done"));
        assert!(
            !alive(&pid),
            "the backgrounded sleep {pid} must be gone at return"
        );
    }

    #[test]
    fn a_descendant_that_closed_its_output_is_gone_when_the_query_returns() {
        let captured = run_query(
            &sh("sleep 30 </dev/null >/dev/null 2>&1 & echo $!; echo done"),
            Duration::from_secs(20),
            1024,
        )
        .unwrap();
        assert_eq!(captured.code, Some(0));
        let mut lines = captured.stdout.lines();
        let pid = lines.next().unwrap().to_owned();
        assert_eq!(lines.next(), Some("done"));
        assert!(
            !alive(&pid),
            "the sleep {pid} that closed its pipes must be gone at return"
        );
    }

    #[test]
    fn a_successful_query_has_reaped_its_own_process() {
        let captured = run_query(&sh("echo $$"), Duration::from_secs(20), 1024).unwrap();
        let leader = captured.stdout.trim().to_owned();
        assert!(
            !alive(&leader),
            "the query's process {leader} must be reaped, not a zombie"
        );
    }

    #[test]
    fn a_timed_out_query_and_its_descendants_are_gone_when_it_returns() {
        let dir = tempfile::tempdir().unwrap();
        let pids = dir.path().join("pids");
        let script = format!(
            "echo $$ > '{path}'; sleep 30 & echo $! >> '{path}'; exec sleep 31",
            path = pids.display()
        );
        let error = run_query(&sh(&script), Duration::from_millis(500), 1024).unwrap_err();
        assert!(matches!(error, QueryError::TimedOut(_)), "{error}");
        let pids = std::fs::read_to_string(&pids).unwrap();
        assert_eq!(pids.lines().count(), 2, "{pids}");
        for pid in pids.lines() {
            assert!(
                !alive(pid),
                "{pid} must be gone when the timed-out query returns"
            );
        }
    }

    #[test]
    fn final_validation_reads_an_overflow_before_any_bytes() {
        let buffer = |overflowed, error: Option<io::Error>| {
            Mutex::new(Buffer {
                bytes: b"{}".to_vec(),
                overflowed,
                done: true,
                error,
            })
        };
        assert!(matches!(
            validated(&buffer(true, None), "stdout", 2),
            Err(QueryError::Oversized {
                stream: "stdout",
                cap: 2
            })
        ));
        assert!(matches!(
            validated(&buffer(true, Some(io::Error::other("x"))), "stderr", 2),
            Err(QueryError::Oversized {
                stream: "stderr",
                ..
            })
        ));
        assert!(matches!(
            validated(&buffer(false, Some(io::Error::other("x"))), "stdout", 2),
            Err(QueryError::Read(_))
        ));
        assert_eq!(validated(&buffer(false, None), "stdout", 2).unwrap(), "{}");
    }

    #[test]
    fn output_past_the_cap_is_always_an_error_and_exactly_the_cap_is_complete() {
        // A stress loop over the race between the last read and the exit; the forced state is
        // covered by final_validation_reads_an_overflow_before_any_bytes.
        for _ in 0..200 {
            let error =
                run_query(&sh("head -c 65 /dev/zero"), Duration::from_secs(20), 64).unwrap_err();
            assert!(
                matches!(
                    error,
                    QueryError::Oversized {
                        stream: "stdout",
                        cap: 64
                    }
                ),
                "{error}"
            );
        }
        let exact = run_query(&sh("head -c 64 /dev/zero"), Duration::from_secs(20), 64).unwrap();
        assert_eq!(exact.stdout.len(), 64);
        let padded = run_query(
            &sh(r#"printf '{"name":"lib"}%60s!!' ''"#),
            Duration::from_secs(20),
            64,
        )
        .unwrap_err();
        assert!(
            matches!(
                padded,
                QueryError::Oversized {
                    stream: "stdout",
                    ..
                }
            ),
            "{padded}"
        );
        let stderr =
            run_query(&sh("head -c 65 /dev/zero >&2"), Duration::from_secs(20), 64).unwrap_err();
        assert!(
            matches!(
                stderr,
                QueryError::Oversized {
                    stream: "stderr",
                    ..
                }
            ),
            "{stderr}"
        );
    }

    #[test]
    fn a_producer_ignoring_a_closed_pipe_is_stopped_after_an_overflow() {
        let started = Instant::now();
        let error = run_query(
            &sh("trap '' PIPE; while :; do echo 0123456789; done"),
            Duration::from_secs(20),
            1000,
        )
        .unwrap_err();
        assert!(matches!(error, QueryError::Oversized { .. }), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn output_held_by_a_process_that_left_the_group_is_a_bounded_cleanup_error() {
        if !python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (lock, ready) = (dir.path().join("escapee.lock"), dir.path().join("escapee.ready"));
        let script = format!(
            "{} & sleep 0.5; echo done",
            escapee(&lock, &ready, 3)
        );
        let started = Instant::now();
        let result = run_query_with(
            &sh(&script),
            Duration::from_secs(20),
            1024,
            Duration::from_millis(500),
            &mut Trace::default(),
        );
        let elapsed = started.elapsed();
        // The escapee is no child of the test (init reaps it), so the test holds no identity it
        // could safely signal. It ends by itself after 3 seconds and is only observed: its lock's
        // release shows it reached its end, not a confirmed exit.
        let escapee_released = released_after_ready(&lock, &ready, Duration::from_secs(20));
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Cleanup { cause: None, .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("its output is still held open"),
            "{error}"
        );
        assert!(elapsed < Duration::from_secs(10));
        assert_eq!(
            escapee_released,
            Ok(true),
            "the escapee must have been ready and then released its lock by ending"
        );
    }

    /// The negative: an escapee that already exited holds nothing, so the query is a plain
    /// success, and nothing anywhere signals the pid it once had.
    #[test]
    fn an_escapee_that_already_exited_is_observed_never_signalled() {
        if !python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (lock, ready) = (dir.path().join("escapee.lock"), dir.path().join("escapee.ready"));
        let script = format!("{}; echo done", escapee(&lock, &ready, 0));
        let captured = run_query(&sh(&script), Duration::from_secs(20), 1024).unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(
            released_after_ready(&lock, &ready, Duration::from_secs(10)),
            Ok(true)
        );
    }

    /// The observer's positive: a live lock holder this test owns counts as running, and only
    /// its owned handle confirms its exit (`stop` reaps it); the lock's release follows.
    #[test]
    fn the_escapee_observer_sees_an_owned_holder_alive_then_ended() {
        if !python() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (lock, ready) = (dir.path().join("holder.lock"), dir.path().join("holder.ready"));
        let mut holder = fixture::Owned::new(
            Command::new("python3")
                .args(["-c", &escapee_code(false, 30)])
                .arg(&lock)
                .arg(&ready)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let became_ready = until(Instant::now() + Duration::from_secs(20), || Ok(ready.exists()));
        let alive_while_owned = released(&lock);
        let stopped = holder.stop(Duration::from_secs(10));
        assert_eq!(became_ready, Ok(true), "the owned holder never became ready");
        assert_eq!(alive_while_owned, Ok(false), "a live holder must count as running");
        assert_eq!(stopped, Ok(()), "the owned handle must confirm the exit");
        assert_eq!(
            released_after_ready(&lock, &ready, Duration::from_secs(10)),
            Ok(true)
        );
    }

    fn python() -> bool {
        match fixture::output(Command::new("python3").arg("--version"), fixture::HELPER) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            result => {
                assert!(result.unwrap().status.success());
                true
            }
        }
    }

    /// Python code that optionally leaves the process group (`setsid`), takes an exclusive lock
    /// on `argv[1]`, writes and closes `argv[2]`, sleeps `seconds`, and ends with `os._exit`.
    /// Nothing in it closes the lock file: there is no interpreter shutdown to release it early,
    /// so only the kernel releases it, as the process ends. That release is all an observer
    /// sees; it is not a confirmed exit.
    fn escapee_code(leave: bool, seconds: u32) -> String {
        let leave = if leave { "os.setsid(); " } else { "" };
        format!(
            "import fcntl,os,sys,time; {leave}f=open(sys.argv[1], \"w\"); fcntl.flock(f, fcntl.LOCK_EX); r=open(sys.argv[2], \"w\"); r.write(\"ready\"); r.close(); time.sleep({seconds}); os._exit(0)"
        )
    }

    /// A shell command running `escapee_code`, leaving the group when it sleeps at all.
    fn escapee(lock: &Path, ready: &Path, seconds: u32) -> String {
        format!(
            "python3 -c '{}' '{}' '{}'",
            escapee_code(seconds > 0, seconds),
            lock.display(),
            ready.display()
        )
    }

    /// Whether the lock is free (its holder's process released it by ending); an error for a
    /// missing file or any lock error but "held".
    fn released(lock: &Path) -> Result<bool, String> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock)
            .map_err(|error| format!("cannot open {}: {error}", lock.display()))?;
        match file.try_lock() {
            Ok(()) => file
                .unlock()
                .map(|()| true)
                .map_err(|error| format!("cannot unlock: {error}")),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(format!("cannot test the lock: {error}"))
            }
        }
    }

    /// Waits for the holder's ready file (it ran), then for its lock to be released (it reached
    /// its end), within `limit`. A holder that never became ready is an error, never released.
    fn released_after_ready(lock: &Path, ready: &Path, limit: Duration) -> Result<bool, String> {
        let deadline = Instant::now() + limit;
        if until(deadline, || Ok(ready.exists()))? {
            until(deadline, || released(lock))
        } else {
            Err(format!("{} never appeared", ready.display()))
        }
    }

    #[test]
    fn group_signal_results_are_judged_by_when_they_were_sent() {
        assert_eq!(first_kill_problem(Ok(())), None);
        for error in [Errno::EPERM, Errno::ESRCH, Errno::EINVAL] {
            let detail = first_kill_problem(Err(error)).unwrap();
            assert!(detail.contains("first kill"), "{error}: {detail}");
        }
        assert_eq!(second_kill(Ok(())), Ok(None));
        assert_eq!(second_kill(Err(Errno::EPERM)), Ok(Some(Errno::EPERM)));
        for error in [Errno::ESRCH, Errno::EINVAL] {
            assert!(second_kill(Err(error)).is_err(), "{error}");
        }
        assert_eq!(probe(Ok(())), Ok(Probe::Members));
        assert_eq!(probe(Err(Errno::EPERM)), Ok(Probe::Unsignalable));
        assert_eq!(probe(Err(Errno::ESRCH)), Ok(Probe::Freed));
        assert!(probe(Err(Errno::EINVAL)).is_err());
        assert_eq!(
            still_present(Probe::Members, None),
            "processes it started are still running"
        );
        let refused = still_present(Probe::Unsignalable, Some(Errno::EPERM));
        assert!(refused.contains("can't signal"), "{refused}");
        assert!(
            refused.contains("second kill of the group was refused"),
            "{refused}"
        );
        let error = QueryError::Cleanup {
            cause: Some(Box::new(QueryError::TimedOut(QUERY_DEADLINE))),
            detail: "its output is still held open (within the 5 second cleanup limit)".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("could not confirm"), "{text}");
        assert!(text.contains("did not finish within 10 minutes"), "{text}");
    }

    /// The platform behaviour behind R6, reproduced deterministically: a process group whose
    /// only member is an unreaped zombie. Linux signals zombies successfully; macOS's XNU skips
    /// them and answers EPERM. Either way, only reaping frees the group (ESRCH).
    #[test]
    fn a_zombie_only_group_answers_as_the_platform_does_until_it_is_freed() {
        let mut anchor = fixture::Owned::new(spawn_anchor(ANCHOR_SHELL).unwrap());
        let group = Pid::from_raw(i32::try_from(anchor.id()).unwrap());
        let pid = anchor.id().to_string();
        anchor.child().kill().unwrap();
        let zombie = until(Instant::now() + Duration::from_secs(5), || {
            let stat = fixture::output(
                Command::new("ps").args(["-o", "stat=", "-p", &pid]),
                fixture::HELPER,
            )
            .map_err(|error| error.to_string())?;
            Ok(String::from_utf8_lossy(&stat.stdout)
                .trim_start()
                .starts_with('Z'))
        });
        let kill = killpg(group, Signal::SIGKILL);
        let signal_zero = killpg(group, None);
        let reaped = anchor.wait_within(Duration::from_secs(10));
        assert!(
            reaped.is_some(),
            "the anchor {pid} was not reaped within 10 seconds"
        );
        assert_eq!(zombie, Ok(true), "the anchor {pid} never became a zombie");
        if cfg!(target_os = "macos") {
            assert_eq!(kill, Err(Errno::EPERM));
            assert_eq!(signal_zero, Err(Errno::EPERM));
        } else if cfg!(target_os = "linux") {
            assert_eq!(kill, Ok(()));
            assert_eq!(signal_zero, Ok(()));
        }
        assert_eq!(second_kill(kill).map(|_| ()), Ok(()));
        assert!(matches!(
            probe(signal_zero),
            Ok(Probe::Members | Probe::Unsignalable)
        ));
        assert_eq!(probe(killpg(group, None)), Ok(Probe::Freed));
    }

    #[test]
    fn a_slow_query_within_its_deadline_completes() {
        let captured = run_query(
            &sh("sleep 0.3; echo ok; echo note >&2; exit 1"),
            Duration::from_secs(20),
            1024,
        )
        .unwrap();
        assert_eq!(captured.code, Some(1));
        assert_eq!(captured.stdout, "ok\n");
        assert_eq!(captured.stderr, "note\n");
    }

    #[test]
    fn an_oversized_query_answer_is_refused_not_truncated() {
        let error = run_query(
            &sh("i=0; while [ $i -lt 400 ]; do echo 0123456789; i=$((i+1)); done"),
            Duration::from_secs(20),
            1000,
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                QueryError::Oversized {
                    stream: "stdout",
                    cap: 1000
                }
            ),
            "{error}"
        );
        let fits = run_query(&sh("echo 0123456789"), Duration::from_secs(20), 1000).unwrap();
        assert_eq!(fits.stdout, "0123456789\n");
    }

    #[test]
    fn an_install_keeps_only_a_bounded_tail_and_does_not_wait_forever_for_held_output() {
        let captured = run_install(
            &sh("i=0; while [ $i -lt 400 ]; do echo line-$i; i=$((i+1)); done"),
            100,
            Duration::from_secs(20),
        )
        .unwrap();
        assert_eq!(captured.code, Some(0));
        assert!(captured.stdout.len() <= 100, "{}", captured.stdout.len());
        assert!(
            captured.stdout.ends_with("line-399\n"),
            "{}",
            captured.stdout
        );

        let started = Instant::now();
        let held = run_install(
            &sh("sleep 5 & echo installed"),
            100,
            Duration::from_millis(300),
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(held.code, Some(0));
        assert!(held.stderr.contains("still holds open"), "{}", held.stderr);
    }

    fn traced(
        invocation: &Invocation,
        deadline: Duration,
        cap: usize,
    ) -> (Result<Captured, QueryError>, Vec<Event>) {
        let mut trace = Trace::default();
        let result = run_query_with(invocation, deadline, cap, QUERY_CLEANUP_GRACE, &mut trace);
        (result, trace.events)
    }

    fn pid_of(events: &[Event], anchor: bool) -> Option<u32> {
        events.iter().find_map(|event| match (event, anchor) {
            (Event::Anchor(pid), true) | (Event::Query(pid), false) => Some(*pid),
            _ => None,
        })
    }

    /// The identity invariant on a recorded settle: two group kills, the second after the query
    /// is reaped and drained, both before the anchor is reaped, nothing destructive after it, and
    /// a signal-0 probe last.
    fn assert_settled_in_order(events: &[Event]) {
        let at = |wanted: Event, from: usize| {
            events[from..]
                .iter()
                .position(|event| *event == wanted)
                .map_or_else(
                    || panic!("no {wanted:?} after {from}: {events:?}"),
                    |offset| from + offset,
                )
        };
        let first_kill = at(Event::KilledGroup, 0);
        let reaped_query = at(Event::ReapedQuery, first_kill);
        let drained = at(Event::ReadersDone, reaped_query);
        let second_kill = at(Event::KilledGroup, drained);
        let reaped_anchor = at(Event::ReapedAnchor, second_kill);
        assert!(
            !events[reaped_anchor..].contains(&Event::KilledGroup),
            "a SIGKILL after the anchor was reaped: {events:?}"
        );
        assert_eq!(events.last(), Some(&Event::Probed), "{events:?}");
        let results: Vec<(usize, Call, Result<(), Errno>)> = events
            .iter()
            .enumerate()
            .filter_map(|(index, event)| match event {
                Event::Signal(call, result) => Some((index, *call, *result)),
                _ => None,
            })
            .collect();
        match results.as_slice() {
            [
                (first_at, Call::FirstKill, first),
                (second_at, Call::SecondKill, second),
                probes @ ..,
            ] => {
                assert_eq!(
                    *first,
                    Ok(()),
                    "the first kill has a live anchor: {events:?}"
                );
                assert!(matches!(second, Ok(()) | Err(Errno::EPERM)), "{events:?}");
                assert!(*first_at < reaped_query && *second_at < reaped_anchor);
                assert!(
                    probes
                        .iter()
                        .all(|(at, call, _)| *call == Call::Probe && *at > reaped_anchor),
                    "only signal-0 probes after the anchor reap: {events:?}"
                );
                assert_eq!(
                    probes.last().map(|(_, _, result)| *result),
                    Some(Err(Errno::ESRCH)),
                    "a settled query ends with its group freed: {events:?}"
                );
            }
            _ => panic!("expected two kills then probes: {events:?}"),
        }
    }

    fn assert_gone(events: &[Event]) {
        for (role, anchor) in [("anchor", true), ("query", false)] {
            let pid = pid_of(events, anchor).unwrap_or_else(|| panic!("no {role}: {events:?}"));
            assert!(
                !alive(&pid.to_string()),
                "the {role} {pid} must be gone at return"
            );
        }
    }

    #[test]
    fn a_successful_query_is_settled_in_order_and_leaves_no_anchor_or_query() {
        let (result, events) = traced(&sh("echo done"), Duration::from_secs(20), 1024);
        assert_eq!(result.unwrap().stdout, "done\n");
        assert_settled_in_order(&events);
        assert_gone(&events);
    }

    #[test]
    fn a_timed_out_query_is_settled_in_the_same_order() {
        let (result, events) = traced(&sh("sleep 30"), Duration::from_millis(300), 1024);
        assert!(matches!(result, Err(QueryError::TimedOut(_))), "{result:?}");
        assert_settled_in_order(&events);
        assert_gone(&events);
    }

    #[test]
    fn an_oversized_query_is_settled_in_the_same_order() {
        let (result, events) = traced(&sh("head -c 65 /dev/zero"), Duration::from_secs(20), 64);
        assert!(
            matches!(result, Err(QueryError::Oversized { .. })),
            "{result:?}"
        );
        assert_settled_in_order(&events);
        assert_gone(&events);
    }

    #[test]
    fn a_query_that_cannot_start_leaves_no_anchor_behind() {
        let missing = Invocation::new(Path::new("/nonexistent/loadout-manager"), ["x"]);
        let (result, events) = traced(&missing, Duration::from_secs(5), 1024);
        assert!(matches!(result, Err(QueryError::Spawn(_))), "{result:?}");
        assert!(!events.contains(&Event::KilledGroup), "{events:?}");
        assert!(events.contains(&Event::ReapedAnchor), "{events:?}");
        let anchor = pid_of(&events, true).unwrap();
        assert!(
            !alive(&anchor.to_string()),
            "the anchor {anchor} must be gone"
        );
    }

    #[test]
    fn a_missing_anchor_shell_is_a_spawn_failure() {
        assert!(spawn_anchor("/nonexistent/sh").is_err());
    }

    #[test]
    fn a_query_cannot_join_a_group_whose_anchor_is_gone() {
        let mut gone = fixture::Owned::new(spawn_anchor(ANCHOR_SHELL).unwrap());
        let group = i32::try_from(gone.id()).unwrap();
        gone.stop(Duration::from_secs(10)).unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "true"]).process_group(group);
        if let Ok(joined) = command.spawn() {
            let _joined = fixture::Owned::new(joined);
            panic!("joining the dead anchor's group {group} must fail");
        }
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let missing = Invocation::new(Path::new("/nonexistent/loadout-manager"), ["x"]);
        assert!(matches!(
            run_query(&missing, Duration::from_secs(5), 1024),
            Err(QueryError::Spawn(_))
        ));
        assert!(run_install(&missing, 100, Duration::from_secs(1)).is_err());
    }

    /// A reader that can't start is a setup failure: the query is stopped at once, within the
    /// setup deadline, rather than watched until its own deadline. The other stream's reader
    /// started, so the failing one is the primary cause.
    #[test]
    fn a_reader_that_cannot_start_stops_the_query_within_the_setup_deadline() {
        let mut trace = Trace::default();
        seam::set(Some(seam::Seam::FailReader("stderr")));
        let started = Instant::now();
        let result = run_query_with(
            &sh("sleep 30"),
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            &mut trace,
        );
        let elapsed = started.elapsed();
        seam::set(None);
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Read(inner) if inner.to_string().contains("stderr reader")),
            "{error}"
        );
        assert!(trace.setup_limit.is_some());
        assert_eq!(trace.disposal_limit, trace.setup_limit, "no renewed budget");
        assert!(
            elapsed < QUERY_CLEANUP_GRACE + Duration::from_secs(2),
            "{elapsed:?}"
        );
        let query = pid_of(&trace.events, false).unwrap();
        let anchor = pid_of(&trace.events, true).unwrap();
        assert!(!alive(&query.to_string()), "the query {query} must be gone");
        assert!(
            !alive(&anchor.to_string()),
            "the anchor {anchor} must be gone"
        );
    }

    #[test]
    fn a_query_that_fails_to_start_is_cleaned_up_within_the_setup_deadline() {
        let mut trace = Trace::default();
        let invocation = Invocation::new(
            Path::new("/nonexistent/loadout-no-such-program"),
            std::iter::empty::<&str>(),
        );
        let result = run_query_with(
            &invocation,
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            &mut trace,
        );
        assert!(matches!(result, Err(QueryError::Spawn(_))), "{result:?}");
        assert!(trace.setup_limit.is_some());
        assert_eq!(trace.disposal_limit, trace.setup_limit, "no renewed budget");
    }

    /// The setup deadline bounds only setup. A query that runs longer than it still completes,
    /// and its settle gets its own limit, set after it exits.
    #[test]
    fn a_query_longer_than_the_setup_deadline_completes() {
        let mut trace = Trace::default();
        let started = Instant::now();
        let result = run_query_with(
            &sh("sleep 6; echo slow"),
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            &mut trace,
        );
        let elapsed = started.elapsed();
        let captured = result.unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "slow");
        assert!(elapsed > QUERY_CLEANUP_GRACE, "{elapsed:?}");
        assert!(trace.disposal_limit.unwrap() > trace.setup_limit.unwrap());
    }

    /// C4: the helper fixture owns its child before either drainer starts. If the second can't
    /// start, the child is killed and reaped (so it is gone at return), and what became of the
    /// first drainer is reported.
    #[test]
    fn a_helper_whose_second_drainer_cannot_start_is_stopped() {
        fixture::fail_drainer(Some("stderr"));
        let result = fixture::output(Command::new("sleep").arg("30"), fixture::HELPER);
        fixture::fail_drainer(None);
        let text = result.unwrap_err().to_string();
        assert!(
            text.contains("injected: cannot start the stderr drainer"),
            "{text}"
        );
        assert!(text.contains("the helper was stopped: Ok(())"), "{text}");
        assert!(text.contains("the stdout drainer is finished"), "{text}");
        let pid = fixture::last_helper().unwrap();
        assert!(
            !alive(&pid.to_string()),
            "the helper {pid} must be gone at return"
        );
    }

    #[test]
    fn a_helper_with_both_drainers_returns_its_output() {
        let output = fixture::output(
            Command::new("/bin/sh").args(["-c", "echo out; echo err >&2"]),
            fixture::HELPER,
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use super::*;

    /// A `.cmd` query: the script is written to a file, so no argument is re-parsed by cmd.exe.
    fn script(dir: &Path, body: &str) -> Invocation {
        let path = dir.join("query.cmd");
        fs::write(&path, format!("@echo off\r\n{body}\r\n")).unwrap();
        Invocation::new(&path, std::iter::empty::<&str>())
    }

    /// A query that would leave a marker if it ever ran, then exits.
    fn marking(dir: &Path) -> (Invocation, std::path::PathBuf) {
        let marker = dir.join("ran");
        let invocation = script(dir, &format!("echo ran> \"{}\"", marker.display()));
        (invocation, marker)
    }

    /// Starts a PowerShell descendant that writes its own id to `ready` (written whole, then
    /// renamed, so the file never holds a partial id) and then stays alive. It ends on its own
    /// after 60 seconds, so even a failed test leaves nothing running for long.
    fn descendant(dir: &Path) -> String {
        let partial = dir.join("ready.partial");
        let ready = dir.join("ready");
        format!(
            "start /b \"\" powershell -NoProfile -NonInteractive -Command \"Set-Content -Encoding ascii -NoNewline -Path '{}' -Value $PID; Move-Item '{}' '{}'; Start-Sleep 60\"",
            partial.display(),
            partial.display(),
            ready.display()
        )
    }

    fn descendant_id(dir: &Path) -> u32 {
        fs::read_to_string(dir.join("ready"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    /// `tasklist`, run within a bound.
    fn alive(pid: u32) -> bool {
        let output = fixture::output(
            Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]),
            fixture::HELPER,
        )
        .unwrap();
        assert!(output.status.success(), "tasklist failed");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }

    /// For the exhausted-budget controls: the job was terminated, but its exit may not have been
    /// observed before the spent limit. The test waits for it within its own bound. This shows
    /// it eventually disappeared, not that it was already gone when the query returned.
    fn gone_within(pid: u32, limit: Duration) -> bool {
        until(Instant::now() + limit, || Ok(!alive(pid))) == Ok(true)
    }

    /// One query run with `injected` set on this thread, and `grace` as its setup deadline and
    /// settle limit. Production uses `QUERY_CLEANUP_GRACE`; only the readiness control supplies
    /// a longer one, and says so.
    fn run(
        invocation: &Invocation,
        deadline: Duration,
        cap: usize,
        grace: Duration,
        injected: Option<seam::Seam>,
    ) -> (Result<Captured, QueryError>, Trace, Duration) {
        let mut trace = Trace::default();
        seam::set(injected);
        let started = Instant::now();
        let result = run_query_with(invocation, deadline, cap, grace, &mut trace);
        let elapsed = started.elapsed();
        seam::set(None);
        (result, trace, elapsed)
    }

    /// The whole setup-failure path, the dependency's leaf wait plus loadout's disposal, shares
    /// one five-second deadline. The margin covers only creating the process and polling.
    const SETUP_BOUND: Duration = Duration::from_secs(7);

    fn assert_setup_disposal(trace: &Trace) {
        assert!(trace.setup_limit.is_some());
        assert_eq!(
            trace.disposal_limit, trace.setup_limit,
            "a setup failure's cleanup must use the original setup deadline, not a renewed one"
        );
    }

    /// The error is `cause`, or a `Cleanup` that keeps `cause` as what ended the query.
    fn primary(error: &QueryError) -> &QueryError {
        match error {
            QueryError::Cleanup {
                cause: Some(cause), ..
            } => cause,
            other => other,
        }
    }

    #[test]
    fn a_failed_handle_duplication_stops_the_query_that_never_ran() {
        let scratch = tempdir().unwrap();
        let (invocation, marker) = marking(scratch.path());
        let (result, trace, elapsed) = run(
            &invocation,
            Duration::from_secs(20),
            1024,
            QUERY_CLEANUP_GRACE,
            Some(seam::Seam::FailDuplicate),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Spawn(inner) if inner.to_string().contains("handle duplication")),
            "{error}"
        );
        let root = trace
            .root
            .expect("loadout created the query before the injected failure");
        assert!(!alive(root), "the query {root} must be gone at return");
        assert!(!marker.exists(), "the query must never have run");
        assert!(elapsed < SETUP_BOUND, "{elapsed:?}");
        assert_setup_disposal(&trace);
    }

    /// The same branch, reached only once the setup deadline has passed: the disposal still
    /// tries to stop the query at once, and gets no fresh budget for it.
    #[test]
    fn a_handle_duplication_failure_at_the_deadline_is_still_disposed_of() {
        let scratch = tempdir().unwrap();
        let (invocation, marker) = marking(scratch.path());
        let (result, trace, elapsed) = run(
            &invocation,
            Duration::from_secs(20),
            1024,
            QUERY_CLEANUP_GRACE,
            Some(seam::Seam::FailDuplicateAtDeadline),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(primary(&error), QueryError::Spawn(inner) if inner.to_string().contains("handle duplication")),
            "{error}"
        );
        assert_setup_disposal(&trace);
        assert!(
            elapsed >= QUERY_CLEANUP_GRACE,
            "the seam waits out the deadline: {elapsed:?}"
        );
        assert!(elapsed < SETUP_BOUND, "no renewed budget: {elapsed:?}");
        let root = trace.root.unwrap();
        assert!(
            gone_within(root, Duration::from_secs(10)),
            "the query {root} must be stopped"
        );
        assert!(!marker.exists(), "the query must never have run");
    }

    /// A suspended `cmd.exe` that would exit 7 if it ever ran, owned from the moment it exists,
    /// as `Query::spawn` keeps its root before any job exists. It is never resumed, so it has no
    /// self-exit; only a kill ends it, and the guard's own checked stop is the fallback.
    fn never_run_root() -> crate::process::owned::Owned {
        use std::os::windows::process::CommandExt;
        crate::process::owned::Owned::new(
            Command::new("cmd.exe")
                .args(["/c", "exit 7"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(loadout_winproc::CREATE_SUSPENDED.0)
                .spawn()
                .unwrap(),
        )
    }

    /// F3: a first observation that fails is no proof the query exited, so `dispose_never_run`
    /// still kills it and reaps it, and keeps the observation error first.
    #[test]
    fn a_failed_first_observation_of_a_never_run_query_still_kills_and_reaps_it() {
        let mut root = never_run_root();
        seam::set(Some(seam::Seam::FailObservation));
        let disposed = dispose_never_run(root.shared(), Instant::now() + QUERY_CLEANUP_GRACE);
        let kills = seam::kills();
        seam::set(None);
        let detail = disposed.unwrap_err();
        assert!(
            detail.starts_with("cannot check the query loadout created: injected"),
            "{detail}"
        );
        assert!(!detail.contains("has not exited"), "{detail}");
        assert_eq!(kills, 1, "the kill must follow a failed observation");
        let status = root.observe().unwrap().expect("the query must have been reaped");
        assert_ne!(status.code(), Some(7), "the never-resumed query must not have run");
        assert_eq!(root.kills(), 0, "nothing was left for the test's own guard to stop");
    }

    #[test]
    fn a_live_never_run_query_is_killed_and_reaped() {
        let mut root = never_run_root();
        seam::set(None);
        let disposed = dispose_never_run(root.shared(), Instant::now() + QUERY_CLEANUP_GRACE);
        let kills = seam::kills();
        assert_eq!(disposed, Ok(()));
        assert_eq!(kills, 1);
        let status = root.observe().unwrap().expect("the query must have been reaped");
        assert_ne!(status.code(), Some(7), "the never-resumed query must not have run");
    }

    /// The negative: a process that has positively exited is not killed.
    #[test]
    fn a_query_that_already_exited_is_not_killed() {
        let mut root = crate::process::owned::Owned::new(
            Command::new("cmd.exe")
                .args(["/c", "exit 7"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let status = root.wait_within(Duration::from_secs(20)).unwrap();
        assert_eq!(status.and_then(|status| status.code()), Some(7));
        seam::set(None);
        let disposed = dispose_never_run(root.shared(), Instant::now() + QUERY_CLEANUP_GRACE);
        assert_eq!(disposed, Ok(()));
        assert_eq!(seam::kills(), 0, "an exited process must not be killed");
    }

    #[test]
    fn a_withheld_process_handle_takes_the_dependency_no_handle_branch() {
        let scratch = tempdir().unwrap();
        let (invocation, marker) = marking(scratch.path());
        let (result, trace, elapsed) = run(
            &invocation,
            Duration::from_secs(20),
            1024,
            QUERY_CLEANUP_GRACE,
            Some(seam::Seam::HideHandle),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Spawn(inner) if inner.kind() == io::ErrorKind::Unsupported),
            "{error}"
        );
        let root = trace
            .root
            .expect("loadout created the query before process-wrap refused it");
        assert!(!alive(root), "the query {root} must be gone at return");
        assert!(!marker.exists(), "the query must never have run");
        assert!(elapsed < SETUP_BOUND, "{elapsed:?}");
        assert_setup_disposal(&trace);
    }

    /// An immediate setup failure after the job is loadout's: the reader can't start, so the
    /// query is never resumed, and the whole job is stopped within the setup deadline.
    #[test]
    fn a_reader_that_cannot_start_stops_the_job_before_the_query_is_resumed() {
        let scratch = tempdir().unwrap();
        let (invocation, marker) = marking(scratch.path());
        let (result, trace, elapsed) = run(
            &invocation,
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            Some(seam::Seam::FailReader("stderr")),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Read(inner) if inner.to_string().contains("stderr reader")),
            "{error}"
        );
        assert_eq!(trace.job_kills, 1);
        assert_setup_disposal(&trace);
        assert!(elapsed < SETUP_BOUND, "{elapsed:?}");
        assert!(!alive(trace.root.unwrap()));
        assert!(!marker.exists(), "the query must never have been resumed");
    }

    /// The setup deadline passes before `start`: the query is not newly resumed, and the job
    /// is still terminated at once, with the spent budget and no fresh one.
    #[test]
    fn a_query_whose_setup_deadline_passed_is_not_resumed_and_its_job_is_stopped() {
        let scratch = tempdir().unwrap();
        let (invocation, marker) = marking(scratch.path());
        let (result, trace, elapsed) = run(
            &invocation,
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            Some(seam::Seam::ExhaustBeforeResume),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(primary(&error), QueryError::Spawn(inner) if inner.kind() == io::ErrorKind::TimedOut),
            "{error}"
        );
        assert_eq!(
            trace.job_kills, 1,
            "the job must be terminated even with no budget left"
        );
        assert_setup_disposal(&trace);
        assert!(
            elapsed >= QUERY_CLEANUP_GRACE,
            "the seam waits out the deadline: {elapsed:?}"
        );
        assert!(elapsed < SETUP_BOUND, "no renewed budget: {elapsed:?}");
        let root = trace.root.unwrap();
        assert!(
            gone_within(root, Duration::from_secs(10)),
            "the query {root} must be stopped"
        );
        assert!(!marker.exists(), "the query must never have been resumed");
    }

    /// The deadline passes after the helper found, opened and checked the thread, before its
    /// final check: `ResumeThread` must not be called. The query never runs (no marker), and its
    /// job is stopped with the original deadline. Its disappearance is then observed within the
    /// test's own 10 second poll, which is not proof that it was gone when `run` returned.
    #[test]
    fn a_query_whose_deadline_passes_after_preparation_is_not_resumed() {
        let scratch = tempdir().unwrap();
        let (invocation, marker) = marking(scratch.path());
        let (result, trace, elapsed) = run(
            &invocation,
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            Some(seam::Seam::ExpireAfterPrepare),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(primary(&error), QueryError::Spawn(inner)
                if inner.kind() == io::ErrorKind::TimedOut && inner.to_string().contains("so it was not resumed")),
            "{error}"
        );
        assert_eq!(
            trace.job_kills, 1,
            "the job must be terminated even with no budget left"
        );
        assert_setup_disposal(&trace);
        assert!(
            elapsed >= QUERY_CLEANUP_GRACE,
            "the seam waits out the deadline: {elapsed:?}"
        );
        assert!(elapsed < SETUP_BOUND, "no renewed budget: {elapsed:?}");
        let root = trace.root.unwrap();
        assert!(
            gone_within(root, Duration::from_secs(10)),
            "the query {root} must be stopped"
        );
        assert!(!marker.exists(), "the query must never have been resumed");
    }

    /// The positive for the same boundary: a pause after preparation that leaves time on the
    /// setup deadline still resumes, and the query answers.
    #[test]
    fn a_query_paused_after_preparation_with_time_left_is_resumed() {
        let scratch = tempdir().unwrap();
        let (result, trace, _) = run(
            &script(scratch.path(), "echo hello"),
            Duration::from_secs(20),
            1024,
            QUERY_CLEANUP_GRACE,
            Some(seam::Seam::PauseAfterPrepare(Duration::from_secs(1))),
        );
        let captured = result.unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "hello");
        assert!(trace.disposal_limit.unwrap() > trace.setup_limit.unwrap());
        assert!(!alive(trace.root.unwrap()));
    }

    /// A real resume that returns only once the deadline has passed: setup must not succeed.
    /// The query may be running, so its whole job is stopped with the original deadline.
    /// Disappearance is observed within the test's own poll, not claimed at return.
    #[test]
    fn a_resume_that_returns_after_the_deadline_is_a_setup_timeout() {
        let scratch = tempdir().unwrap();
        let (result, trace, elapsed) = run(
            &script(scratch.path(), "ping -n 60 127.0.0.1 >nul"),
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            Some(seam::Seam::ExpireAfterResume),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(primary(&error), QueryError::Spawn(inner)
                if inner.kind() == io::ErrorKind::TimedOut && inner.to_string().contains("resumed only as that limit passed")),
            "{error}"
        );
        assert_eq!(trace.job_kills, 1);
        assert_setup_disposal(&trace);
        assert!(
            elapsed >= QUERY_CLEANUP_GRACE,
            "the seam waits out the deadline: {elapsed:?}"
        );
        assert!(elapsed < SETUP_BOUND, "no renewed budget: {elapsed:?}");
        let root = trace.root.unwrap();
        assert!(
            gone_within(root, Duration::from_secs(10)),
            "the query {root} must be stopped"
        );
    }

    /// The query really runs and really starts a descendant, which reports itself ready, before
    /// the injected error. The whole job must then be stopped: the query and the descendant are
    /// each checked to be gone.
    ///
    /// Test-only setup deadline: the readiness wait can take up to 20 seconds, so this control
    /// supplies 60. It proves the cleanup uses that same original deadline (no fresh budget),
    /// not that production setup fits in 5 seconds; the 5-second controls above prove that.
    #[test]
    fn a_failure_after_the_query_resumed_stops_the_whole_job_by_the_setup_deadline() {
        const READINESS_GRACE: Duration = Duration::from_secs(60);
        let scratch = tempdir().unwrap();
        let dir = scratch.path();
        let invocation = script(
            dir,
            &format!("{}\r\nping -n 60 127.0.0.1 >nul", descendant(dir)),
        );
        let (result, trace, elapsed) = run(
            &invocation,
            Duration::from_secs(60),
            1024,
            READINESS_GRACE,
            Some(seam::Seam::FailAfterResume(dir.join("ready"))),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Spawn(inner) if inner.to_string().contains("after the query was resumed")),
            "{error}"
        );
        assert_setup_disposal(&trace);
        assert_eq!(trace.job_kills, 1);
        let root = trace.root.unwrap();
        let descendant = descendant_id(dir);
        assert!(!alive(root), "the query {root} must be gone at return");
        assert!(
            !alive(descendant),
            "its descendant {descendant} must be gone at return"
        );
        assert!(elapsed < READINESS_GRACE, "{elapsed:?}");
    }

    /// A real `TerminateJobObject` failure can't be constructed. This proves only that its error
    /// is kept as a cleanup failure, not that one is ever reported natively.
    #[test]
    fn a_job_termination_error_is_kept() {
        let detail = killed(Err(io::Error::other("access is denied"))).unwrap_err();
        assert_eq!(detail, "cannot stop it: access is denied");
    }

    #[test]
    fn an_ordinary_query_is_resumed_and_answers() {
        let scratch = tempdir().unwrap();
        let (result, trace, _) = run(
            &script(scratch.path(), "echo hello"),
            Duration::from_secs(20),
            1024,
            QUERY_CLEANUP_GRACE,
            None,
        );
        let captured = result.unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "hello");
        assert!(!alive(trace.root.unwrap()));
    }

    /// The setup deadline bounds only setup. A query that runs longer than it still completes,
    /// and its settle gets its own limit, set after it exits.
    #[test]
    fn a_query_longer_than_the_setup_deadline_completes() {
        let scratch = tempdir().unwrap();
        let (result, trace, elapsed) = run(
            &script(scratch.path(), "ping -n 8 127.0.0.1 >nul\r\necho slow"),
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            None,
        );
        let captured = result.unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "slow");
        assert!(elapsed > QUERY_CLEANUP_GRACE, "{elapsed:?}");
        assert!(trace.disposal_limit.unwrap() > trace.setup_limit.unwrap());
        assert!(!alive(trace.root.unwrap()));
    }

    #[test]
    fn a_missing_program_is_a_spawn_error_with_no_process() {
        let invocation = Invocation::new(
            Path::new(r"C:\loadout-no-such-dir\no-such-program.exe"),
            std::iter::empty::<&str>(),
        );
        let (result, trace, _) = run(
            &invocation,
            Duration::from_secs(20),
            1024,
            QUERY_CLEANUP_GRACE,
            None,
        );
        assert!(matches!(result, Err(QueryError::Spawn(_))), "{result:?}");
        assert_eq!(trace.root, None);
    }

    #[test]
    fn a_descendant_holding_the_output_is_gone_when_the_query_returns() {
        let scratch = tempdir().unwrap();
        let dir = scratch.path();
        let ready = dir.join("ready");
        let body = format!(
            "{}\r\n:wait\r\nif not exist \"{}\" (ping -n 2 127.0.0.1 >nul & goto wait)\r\necho done",
            descendant(dir),
            ready.display()
        );
        let (result, trace, _) = run(
            &script(dir, &body),
            Duration::from_secs(60),
            1024,
            QUERY_CLEANUP_GRACE,
            None,
        );
        let captured = result.unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "done");
        let descendant = descendant_id(dir);
        assert!(!alive(trace.root.unwrap()));
        assert!(
            !alive(descendant),
            "the descendant {descendant} must be gone at return"
        );
    }

    #[test]
    fn a_query_that_never_finishes_is_stopped_at_its_deadline() {
        let scratch = tempdir().unwrap();
        let started = Instant::now();
        let (result, trace, _) = run(
            &script(scratch.path(), "ping -n 60 127.0.0.1 >nul"),
            Duration::from_millis(300),
            1024,
            QUERY_CLEANUP_GRACE,
            None,
        );
        assert!(matches!(result, Err(QueryError::TimedOut(_))), "{result:?}");
        assert!(!alive(trace.root.unwrap()));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn an_oversized_answer_is_refused_and_the_query_stopped() {
        let scratch = tempdir().unwrap();
        let (result, trace, _) = run(
            &script(
                scratch.path(),
                "echo 0123456789012345678901234567890123456789\r\nping -n 60 127.0.0.1 >nul",
            ),
            Duration::from_secs(60),
            16,
            QUERY_CLEANUP_GRACE,
            None,
        );
        assert!(
            matches!(
                result,
                Err(QueryError::Oversized {
                    stream: "stdout",
                    ..
                })
            ),
            "{result:?}"
        );
        assert!(!alive(trace.root.unwrap()));
    }

    /// C4, on Windows: as the Unix control, with `ping.exe` started directly.
    #[test]
    fn a_helper_whose_second_drainer_cannot_start_is_stopped() {
        fixture::fail_drainer(Some("stderr"));
        let result = fixture::output(
            Command::new("ping.exe").args(["-n", "30", "127.0.0.1"]),
            fixture::HELPER,
        );
        fixture::fail_drainer(None);
        let text = result.unwrap_err().to_string();
        assert!(
            text.contains("injected: cannot start the stderr drainer"),
            "{text}"
        );
        assert!(text.contains("the helper was stopped: Ok(())"), "{text}");
        assert!(text.contains("the stdout drainer is finished"), "{text}");
        let pid = fixture::last_helper().unwrap();
        assert!(!alive(pid), "the helper {pid} must be gone at return");
    }
}

/// The supervised background mode (`Containment::Inherited`) on Unix. These run in the test
/// process's own group, standing in for the supervisor's: an inherited query must stay in it
/// and signal nothing but its own process.
///
/// Descendants a fixture leaves are not this test's children, so no test holds an identity it
/// could safely signal. They are therefore self-bounded (each sleeps a few seconds and exits by
/// itself), and the tests only observe them with signal 0, which delivers nothing, never killing
/// a recorded numeric pid that might by then belong to another process.
#[cfg(all(test, unix))]
mod inherited_tests {
    use std::path::{Path, PathBuf};

    use nix::sys::signal::kill;
    use nix::unistd::getpgrp;

    use super::*;

    fn sh(script: &str) -> Invocation {
        Invocation::new(Path::new("/bin/sh"), ["-c", script])
    }

    fn supervised() -> Containment {
        Containment::inherited_for_tests()
    }

    /// Signal 0 only: an observation, never a destructive signal.
    fn alive(pid: i32) -> bool {
        match kill(Pid::from_raw(pid), None) {
            Ok(()) | Err(Errno::EPERM) => true,
            Err(Errno::ESRCH) => false,
            Err(error) => panic!("cannot check process {pid}: {error}"),
        }
    }

    fn written_pid(file: &Path) -> Option<i32> {
        std::fs::read_to_string(file).ok()?.trim().parse().ok()
    }

    /// Waits, without signalling, for a self-bounded fixture descendant to exit by itself.
    fn exits_by_itself(pid: i32, within: Duration) -> bool {
        until(Instant::now() + within, || Ok(!alive(pid))) == Ok(true)
    }

    fn marker(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[test]
    fn an_inherited_query_runs_in_the_callers_group() {
        let captured = query(&sh("ps -o pgid= -p $$"), &supervised()).unwrap();
        let group: i32 = captured.stdout.trim().parse().unwrap();
        assert_eq!(group, getpgrp().as_raw());
    }

    #[test]
    fn a_hung_inherited_query_is_stopped_alone_and_its_descendant_left_to_the_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = marker(&dir, "descendant");
        let script = format!(
            "sleep 8 >/dev/null 2>&1 & echo $! > '{}'; sleep 30",
            pid_file.display()
        );
        let error = run_inherited_query(
            &sh(&script),
            Duration::from_millis(500),
            1024,
            Duration::from_secs(5),
        )
        .unwrap_err();
        let descendant = written_pid(&pid_file);
        let left_alive = descendant.is_some_and(alive);
        let gone = descendant.is_none_or(|pid| exits_by_itself(pid, Duration::from_secs(20)));
        assert!(matches!(error, QueryError::TimedOut(_)), "{error}");
        assert!(
            left_alive,
            "only the query's own process may be signalled; its descendant is the supervisor's"
        );
        assert!(gone, "the self-bounded descendant did not exit by itself");
    }

    #[test]
    fn a_descendant_holding_the_output_is_a_cleanup_error_not_success() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = marker(&dir, "holder");
        let script = format!("sleep 3 & echo $! > '{}'", pid_file.display());
        let started = Instant::now();
        let result = run_inherited_query(
            &sh(&script),
            Duration::from_secs(20),
            1024,
            Duration::from_millis(500),
        );
        let elapsed = started.elapsed();
        let gone = written_pid(&pid_file)
            .is_none_or(|pid| exits_by_itself(pid, Duration::from_secs(20)));
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Cleanup { cause: None, .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("still holds its output open"),
            "{error}"
        );
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        assert!(gone, "the self-bounded holder did not exit by itself");
    }

    #[test]
    fn oversized_output_stops_an_inherited_query_too() {
        let error = run_inherited_query(
            &sh("head -c 65 /dev/zero"),
            Duration::from_secs(20),
            64,
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(error, QueryError::Oversized { .. }), "{error}");
        let exact = run_inherited_query(
            &sh("head -c 64 /dev/zero"),
            Duration::from_secs(20),
            64,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(exact.stdout.len(), 64);
    }

    #[test]
    fn a_failed_query_cancels_the_run_and_nothing_starts_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let containment = supervised();
        let missing = Invocation::new(Path::new("/nonexistent/loadout-manager"), ["x"]);
        assert!(matches!(
            query(&missing, &containment),
            Err(QueryError::Spawn(_))
        ));
        assert!(containment.is_cancelled());

        let ran = marker(&dir, "ran");
        let touch = sh(&format!("touch '{}'", ran.display()));
        assert!(matches!(
            query(&touch, &containment),
            Err(QueryError::Cancelled)
        ));
        let installed = install(&touch, &containment).unwrap_err();
        assert_eq!(installed.kind(), io::ErrorKind::Interrupted);
        assert!(!ran.exists(), "a cancelled run must start no query or installer");
    }

    #[test]
    fn the_foreground_is_unchanged_and_never_cancelled() {
        let foreground = Containment::OwnGroup;
        let missing = Invocation::new(Path::new("/nonexistent/loadout-manager"), ["x"]);
        assert!(query(&missing, &foreground).is_err());
        assert!(!foreground.is_cancelled());
        let captured = query(&sh("echo ok"), &foreground).unwrap();
        assert_eq!(captured.stdout, "ok\n");
    }

    #[test]
    fn queries_and_installs_never_pass_on_the_worker_nonce() {
        let script = format!("printf '%s' \"${{{WORKER_NONCE}-unset}}\"");
        let invocation = Invocation {
            env: &[(WORKER_NONCE, "a-nonce")],
            ..sh(&script)
        };
        let captured = query(&invocation, &Containment::OwnGroup).unwrap();
        assert_eq!(captured.stdout, "unset");
        let installed = install(&invocation, &Containment::OwnGroup).unwrap();
        assert_eq!(installed.stdout, "unset");
    }
}
