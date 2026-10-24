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

use super::manager::Invocation;

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
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug)]
pub(super) struct Captured {
    pub(super) code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

impl Captured {
    pub(super) fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub(super) fn hresult(&self) -> Option<u32> {
        #[expect(
            clippy::cast_sign_loss,
            reason = "winget reports HRESULTs as negative i32 codes"
        )]
        self.code.map(|code| code as u32)
    }

    pub(super) fn status(&self) -> String {
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
    pub(super) fn tail(&self) -> String {
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

    pub(super) fn unexpected(&self) -> String {
        format!("unexpected {}{}", self.status(), self.tail())
    }
}

#[derive(Debug)]
pub(super) enum QueryError {
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
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub(super) fn query(invocation: &Invocation) -> Result<Captured, QueryError> {
    run_query(invocation, QUERY_DEADLINE, QUERY_OUTPUT_CAP)
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
    let mut query = Query::spawn(invocation, grace, trace)?;
    let stdout = reader(query.take_stdout(), Keep::All(cap));
    let stderr = reader(query.take_stderr(), Keep::All(cap));
    #[cfg(windows)]
    let started = query.start();
    #[cfg(unix)]
    let started = Ok(());
    let outcome = started.and_then(|()| watch(&mut query, &stdout, &stderr, deadline, cap));
    if let Err(detail) = query.settle(&stdout, &stderr, Instant::now() + grace, trace) {
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
    query: &mut Query,
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
        if let Some(status) = query.try_exit().map_err(QueryError::Wait)? {
            return Ok(status.code());
        }
        if Instant::now() >= limit {
            return Err(QueryError::TimedOut(deadline));
        }
        thread::sleep(POLL);
    }
}

/// Polls `done` until it holds or `limit` passes; `Ok(false)` means time ran out.
fn until(limit: Instant, mut done: impl FnMut() -> Result<bool, String>) -> Result<bool, String> {
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
fn first(problem: &mut Option<String>, step: Result<bool, String>, pending: &str) {
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
struct Query {
    child: Child,
    anchor: Child,
    anchor_input: Option<ChildStdin>,
    group: Pid,
}

#[cfg(unix)]
const ANCHOR_SHELL: &str = "/bin/sh";

#[cfg(unix)]
impl Query {
    fn spawn(
        invocation: &Invocation,
        grace: Duration,
        trace: &mut Trace,
    ) -> Result<Self, QueryError> {
        let mut anchor = spawn_anchor(ANCHOR_SHELL).map_err(QueryError::Spawn)?;
        let anchor_input = anchor.stdin.take();
        #[cfg(test)]
        note(trace, Event::Anchor(anchor.id()));
        let spawned = i32::try_from(anchor.id())
            .map_err(|_| io::Error::other(format!("pid {} is out of range", anchor.id())))
            .and_then(|group| {
                let mut command = command(invocation);
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
                let limit = Instant::now() + grace;
                match release_anchor(&mut anchor, anchor_input, limit, trace) {
                    Ok(()) => Err(QueryError::Spawn(error)),
                    Err(detail) => Err(QueryError::Cleanup {
                        cause: Some(Box::new(QueryError::Spawn(error))),
                        detail,
                    }),
                }
            }
        }
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Waits on the query's own pid only.
    fn try_exit(&mut self) -> io::Result<Option<ExitStatus>> {
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
        let drain = until(limit, || Ok(drained(stdout, stderr)));
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
                QUERY_CLEANUP_GRACE.as_secs()
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
/// hold on the process, kept from the moment it exists.
#[cfg(windows)]
struct Query {
    child: Box<dyn ChildWrapper>,
    root: Arc<Mutex<Child>>,
}

#[cfg(windows)]
impl Query {
    /// Until the job exists, loadout owns the child. If anything fails before then, the child has
    /// never run (nothing resumed it), so stopping that one process is complete. That is bounded
    /// by `grace`, one deadline shared with `process-wrap`'s own wait on the child. Once the job
    /// is loadout's, the deadline is cleared, and the query gets its ordinary limits.
    fn spawn(
        invocation: &Invocation,
        grace: Duration,
        trace: &mut Trace,
    ) -> Result<Self, QueryError> {
        let setup = Arc::new(Mutex::new(Some(Instant::now() + grace)));
        let mut root = None;
        let mut wrapped = CommandWrap::from(command(invocation));
        wrapped.wrap(CreationFlags(loadout_winproc::CREATE_SUSPENDED));
        wrapped.wrap(JobObject);
        let spawned = wrapped.spawn_with_child(|command| {
            let child = Arc::new(Mutex::new(command.spawn()?));
            root = Some(Arc::clone(&child));
            #[cfg(test)]
            if seam::is(&seam::Seam::FailDuplicate) {
                return Err(io::Error::other("injected: handle duplication failed"));
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
            (Ok(child), Some(root)) => {
                *guard(&setup) = None;
                Ok(Self { child, root })
            }
            (Err(error), None) => Err(QueryError::Spawn(error)),
            (Err(error), Some(root)) => {
                let limit = guard(&setup).unwrap_or_else(Instant::now);
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

    /// Resumes the suspended query. Loadout owns the job by now, so any error, even one after the
    /// thread started, leaves the whole job to `settle`, with this error as the cause.
    fn start(&self) -> Result<(), QueryError> {
        let resumed = loadout_winproc::resume_suspended(&guard(&self.root));
        #[cfg(test)]
        let resumed = resumed.and_then(|()| seam::after_resume());
        resumed.map_err(QueryError::Spawn)
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
        _trace: &mut Trace,
    ) -> Result<(), String> {
        let mut problem = None;
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

/// The innermost child `process-wrap` wraps: loadout's own process, shared with `Query::root`
/// so loadout keeps it even if `process-wrap` drops this wrapper on a setup error. Its `wait` is
/// bounded by the setup deadline, and refuses to block once that is cleared.
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
/// nothing: killing and reaping this one process within `limit` is the whole cleanup.
#[cfg(windows)]
fn dispose_never_run(root: &Mutex<Child>, limit: Instant) -> Result<(), String> {
    let mut child = guard(root);
    let mut problem = None;
    match child.try_wait() {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {
            if let Err(error) = child.kill() {
                problem = Some(format!("cannot stop the query loadout created: {error}"));
            }
        }
        Err(error) => problem = Some(format!("cannot check the query loadout created: {error}")),
    }
    let reaped = until(limit, || {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|error| format!("cannot reap the query loadout created: {error}"))
    });
    first(&mut problem, reaped, "the query loadout created has not exited");
    problem.map_or(Ok(()), |detail| {
        Err(format!(
            "{detail} (within the {} second cleanup limit)",
            QUERY_CLEANUP_GRACE.as_secs()
        ))
    })
}

#[cfg(windows)]
fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Test-only fault injection for the Windows setup path. Each seam reaches exactly one branch.
#[cfg(all(test, windows))]
mod seam {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use std::{io, thread};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Seam {
        /// Fails right after loadout took the child, before the handle is duplicated.
        FailDuplicate,
        /// Withholds the process handle, so `process-wrap` takes its own no-handle branch.
        HideHandle,
        /// After a real resume, waits until the test's descendant writes this file, then fails.
        FailAfterResume(PathBuf),
    }

    thread_local! {
        static SEAM: RefCell<Option<Seam>> = const { RefCell::new(None) };
    }

    pub(super) fn set(seam: Option<Seam>) {
        SEAM.with(|cell| *cell.borrow_mut() = seam);
    }

    pub(super) fn is(wanted: &Seam) -> bool {
        SEAM.with(|cell| cell.borrow().as_ref() == Some(wanted))
    }

    pub(super) fn after_resume() -> io::Result<()> {
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
                return Err(io::Error::other("injected: the descendant never reported ready"));
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err(io::Error::other("injected: failure after the query was resumed"))
    }
}

/// The order of signals and reaps, recorded only in tests, to check that no SIGKILL follows the
/// anchor's reap. On Windows, the id of the process loadout created, to check it is gone.
#[derive(Default)]
struct Trace {
    #[cfg(all(test, unix))]
    events: Vec<Event>,
    #[cfg(all(test, windows))]
    root: Option<u32>,
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
/// tail of each output stream is kept.
pub(super) fn install(invocation: &Invocation) -> io::Result<Captured> {
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
/// while loadout waits for it to exit.
fn reader<R: Read + Send + 'static>(source: Option<R>, keep: Keep) -> Shared {
    let shared = Shared::default();
    let Some(mut source) = source else {
        lock(&shared).done = true;
        return shared;
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
    if let Err(error) = spawned {
        let mut buffer = lock(&shared);
        buffer.error = Some(error);
        buffer.done = true;
    }
    shared
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::Path;

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

    fn alive(pid: &str) -> bool {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("kill -0 {pid} 2>/dev/null"))
            .status()
            .unwrap()
            .success()
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
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let escapee = dir.path().join("escapee");
        let script = format!(
            "python3 -c 'import os,sys,time; os.setsid(); open(sys.argv[1], \"w\").write(str(os.getpid())); time.sleep(30)' '{}' & sleep 0.5; echo done",
            escapee.display()
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
        if let Ok(pid) = std::fs::read_to_string(&escapee) {
            let _ = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("kill -9 {}", pid.trim()))
                .status();
        }
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
        assert!(refused.contains("second kill of the group was refused"), "{refused}");
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
        let mut anchor = spawn_anchor(ANCHOR_SHELL).unwrap();
        let group = Pid::from_raw(i32::try_from(anchor.id()).unwrap());
        anchor.kill().unwrap();
        let pid = anchor.id().to_string();
        let zombie = until(Instant::now() + Duration::from_secs(5), || {
            let stat = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .map_err(|error| error.to_string())?;
            Ok(String::from_utf8_lossy(&stat.stdout).trim_start().starts_with('Z'))
        });
        let kill = killpg(group, Signal::SIGKILL);
        let signal_zero = killpg(group, None);
        anchor.wait().unwrap();
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
                assert_eq!(*first, Ok(()), "the first kill has a live anchor: {events:?}");
                assert!(
                    matches!(second, Ok(()) | Err(Errno::EPERM)),
                    "{events:?}"
                );
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
        let mut gone = spawn_anchor(ANCHOR_SHELL).unwrap();
        gone.kill().unwrap();
        gone.wait().unwrap();
        let group = i32::try_from(gone.id()).unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "true"]).process_group(group);
        assert!(
            command.spawn().is_err(),
            "joining the dead anchor's group {group} must fail"
        );
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

    /// Starts a PowerShell descendant that writes its own id to `ready` (written whole, then
    /// renamed, so the file never holds a partial id) and then stays alive.
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

    fn alive(pid: u32) -> bool {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .unwrap();
        assert!(output.status.success(), "tasklist failed");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }

    /// Runs one query with `injected` set on this thread, returning the result, the id of the process
    /// loadout created, and how long the whole call took.
    fn run(
        invocation: &Invocation,
        deadline: Duration,
        cap: usize,
        injected: Option<seam::Seam>,
    ) -> (Result<Captured, QueryError>, Option<u32>, Duration) {
        let mut trace = Trace::default();
        seam::set(injected);
        let started = Instant::now();
        let result = run_query_with(invocation, deadline, cap, QUERY_CLEANUP_GRACE, &mut trace);
        let elapsed = started.elapsed();
        seam::set(None);
        (result, trace.root, elapsed)
    }

    /// The whole setup-failure path, the dependency's leaf wait plus loadout's disposal, shares
    /// one five-second deadline. The margin covers only creating the process and polling.
    const SETUP_BOUND: Duration = Duration::from_secs(7);

    #[test]
    fn a_failed_handle_duplication_stops_the_query_that_never_ran() {
        let scratch = tempdir().unwrap();
        let dir = scratch.path();
        let marker = dir.join("ran");
        let invocation = script(dir, &format!("echo ran> \"{}\"", marker.display()));
        let (result, root, elapsed) = run(
            &invocation,
            Duration::from_secs(20),
            1024,
            Some(seam::Seam::FailDuplicate),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Spawn(inner) if inner.to_string().contains("handle duplication")),
            "{error}"
        );
        let root = root.expect("loadout created the query before the injected failure");
        assert!(!alive(root), "the query {root} must be gone at return");
        assert!(!marker.exists(), "the query must never have run");
        assert!(elapsed < SETUP_BOUND, "{elapsed:?}");
    }

    #[test]
    fn a_withheld_process_handle_takes_the_dependency_no_handle_branch() {
        let scratch = tempdir().unwrap();
        let dir = scratch.path();
        let marker = dir.join("ran");
        let invocation = script(dir, &format!("echo ran> \"{}\"", marker.display()));
        let (result, root, elapsed) = run(
            &invocation,
            Duration::from_secs(20),
            1024,
            Some(seam::Seam::HideHandle),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Spawn(inner) if inner.kind() == io::ErrorKind::Unsupported),
            "{error}"
        );
        let root = root.expect("loadout created the query before process-wrap refused it");
        assert!(!alive(root), "the query {root} must be gone at return");
        assert!(!marker.exists(), "the query must never have run");
        assert!(elapsed < SETUP_BOUND, "{elapsed:?}");
    }

    /// The query really runs and really starts a descendant, which reports itself ready, before
    /// the injected error. The whole job must then be stopped: the query and the descendant are
    /// each checked to be gone.
    #[test]
    fn a_failure_after_the_query_resumed_stops_the_whole_job() {
        let scratch = tempdir().unwrap();
        let dir = scratch.path();
        let invocation = script(
            dir,
            &format!("{}\r\nping -n 60 127.0.0.1 >nul", descendant(dir)),
        );
        let (result, root, elapsed) = run(
            &invocation,
            Duration::from_secs(60),
            1024,
            Some(seam::Seam::FailAfterResume(dir.join("ready"))),
        );
        let error = result.unwrap_err();
        assert!(
            matches!(&error, QueryError::Spawn(inner) if inner.to_string().contains("after the query was resumed")),
            "{error}"
        );
        let root = root.unwrap();
        let descendant = descendant_id(dir);
        assert!(!alive(root), "the query {root} must be gone at return");
        assert!(!alive(descendant), "its descendant {descendant} must be gone at return");
        // At most 20 seconds waiting for the descendant, then one five-second settle.
        assert!(elapsed < Duration::from_secs(30), "{elapsed:?}");
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
        let dir = scratch.path();
        let (result, root, _) = run(&script(dir, "echo hello"), Duration::from_secs(20), 1024, None);
        let captured = result.unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "hello");
        assert!(!alive(root.unwrap()));
    }

    #[test]
    fn a_missing_program_is_a_spawn_error_with_no_process() {
        let invocation = Invocation::new(
            Path::new(r"C:\loadout-no-such-dir\no-such-program.exe"),
            std::iter::empty::<&str>(),
        );
        let (result, root, _) = run(&invocation, Duration::from_secs(20), 1024, None);
        assert!(matches!(result, Err(QueryError::Spawn(_))), "{result:?}");
        assert_eq!(root, None);
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
        let (result, root, _) = run(&script(dir, &body), Duration::from_secs(60), 1024, None);
        let captured = result.unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.trim(), "done");
        let descendant = descendant_id(dir);
        assert!(!alive(root.unwrap()));
        assert!(!alive(descendant), "the descendant {descendant} must be gone at return");
    }

    #[test]
    fn a_query_that_never_finishes_is_stopped_at_its_deadline() {
        let scratch = tempdir().unwrap();
        let dir = scratch.path();
        let started = Instant::now();
        let (result, root, _) = run(
            &script(dir, "ping -n 60 127.0.0.1 >nul"),
            Duration::from_millis(300),
            1024,
            None,
        );
        assert!(matches!(result, Err(QueryError::TimedOut(_))), "{result:?}");
        assert!(!alive(root.unwrap()));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn an_oversized_answer_is_refused_and_the_query_stopped() {
        let scratch = tempdir().unwrap();
        let dir = scratch.path();
        let (result, root, _) = run(
            &script(dir, "echo 0123456789012345678901234567890123456789\r\nping -n 60 127.0.0.1 >nul"),
            Duration::from_secs(60),
            16,
            None,
        );
        assert!(
            matches!(result, Err(QueryError::Oversized { stream: "stdout", .. })),
            "{result:?}"
        );
        assert!(!alive(root.unwrap()));
    }
}
