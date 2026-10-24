use std::fmt;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::{CommandExt, ExitStatusExt};
#[cfg(unix)]
use std::process::{Child, ChildStdin};
use std::process::{ChildStderr, ChildStdout, ExitStatus};

#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(unix)]
use nix::unistd::Pid;
#[cfg(windows)]
use process_wrap::std::{ChildWrapper, CommandWrap, JobObject};

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
    let outcome = watch(&mut query, &stdout, &stderr, deadline, cap);
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
fn until(
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
        first(&mut problem, self.kill_group(trace).map(|()| true), "");
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
        first(&mut problem, self.kill_group(trace).map(|()| true), "");
        let input = self.anchor_input.take();
        if let Err(detail) = release_anchor(&mut self.anchor, input, limit, trace) {
            problem.get_or_insert(detail);
        }
        let group = self.group;
        let empty = until(limit, || group_empty(group));
        note(trace, Event::Probed);
        first(&mut problem, empty, "processes it started are still running");
        match problem {
            None => Ok(()),
            Some(detail) => Err(format!(
                "{detail} (within the {} second cleanup limit)",
                QUERY_CLEANUP_GRACE.as_secs()
            )),
        }
    }

    /// SIGKILL to the whole group. Only called while the anchor is unreaped, so the id is ours.
    fn kill_group(&self, trace: &mut Trace) -> Result<(), String> {
        note(trace, Event::KilledGroup);
        killed(killpg(self.group, Signal::SIGKILL).map_err(io::Error::from))
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

/// The result of killing the group or job. On Unix, `ESRCH` means the group is already gone;
/// every other error is real.
fn killed(result: io::Result<()>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(Errno::ESRCH as i32) => Ok(()),
        Err(error) => Err(format!("cannot stop it: {error}")),
    }
}

/// Whether the group has no members left. Signal 0 delivers nothing, so a reused id can only
/// make this answer "not empty", never harm another group.
#[cfg(unix)]
fn group_empty(group: Pid) -> Result<bool, String> {
    match killpg(group, None) {
        Ok(()) => Ok(false),
        Err(Errno::ESRCH) => Ok(true),
        Err(error) => Err(format!("cannot check process group {group}: {error}")),
    }
}

/// On Windows the query runs in a job object, addressed through loadout's own handle, so there
/// is no id to be reused. No safe API reports that a job has no processes left, so the job's
/// termination, the query's exit, and both readers' EOF are the whole confirmation.
#[cfg(windows)]
struct Query {
    child: Box<dyn ChildWrapper>,
}

#[cfg(windows)]
impl Query {
    fn spawn(
        invocation: &Invocation,
        _grace: Duration,
        _trace: &mut Trace,
    ) -> Result<Self, QueryError> {
        let mut wrapped = CommandWrap::from(command(invocation));
        wrapped.wrap(JobObject);
        Ok(Self {
            child: wrapped.spawn().map_err(QueryError::Spawn)?,
        })
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
        first(&mut problem, killed(self.child.start_kill()).map(|()| true), "");
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

/// The order of signals and reaps, recorded only in tests, to check that no SIGKILL follows the
/// anchor's reap.
#[derive(Default)]
struct Trace {
    #[cfg(all(test, unix))]
    events: Vec<Event>,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    #[cfg(test)]
    Anchor(u32),
    #[cfg(test)]
    Query(u32),
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
        assert!(!alive(&pid), "the backgrounded sleep {pid} must be gone at return");
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
        assert!(!alive(&pid), "the sleep {pid} that closed its pipes must be gone at return");
    }

    #[test]
    fn a_successful_query_has_reaped_its_own_process() {
        let captured = run_query(&sh("echo $$"), Duration::from_secs(20), 1024).unwrap();
        let leader = captured.stdout.trim().to_owned();
        assert!(!alive(&leader), "the query's process {leader} must be reaped, not a zombie");
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
            assert!(!alive(pid), "{pid} must be gone when the timed-out query returns");
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
            matches!(padded, QueryError::Oversized { stream: "stdout", .. }),
            "{padded}"
        );
        let stderr = run_query(&sh("head -c 65 /dev/zero >&2"), Duration::from_secs(20), 64)
            .unwrap_err();
        assert!(
            matches!(stderr, QueryError::Oversized { stream: "stderr", .. }),
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
    fn only_a_missing_group_counts_as_already_stopped() {
        use nix::errno::Errno;

        assert_eq!(killed(Ok(())), Ok(()));
        assert_eq!(
            killed(Err(io::Error::from_raw_os_error(Errno::ESRCH as i32))),
            Ok(())
        );
        assert!(killed(Err(io::Error::from_raw_os_error(Errno::EPERM as i32))).is_err());
        let error = QueryError::Cleanup {
            cause: Some(Box::new(QueryError::TimedOut(QUERY_DEADLINE))),
            detail: "its output is still held open 5 seconds after loadout stopped it".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("could not confirm"), "{text}");
        assert!(text.contains("did not finish within 10 minutes"), "{text}");
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
                .map(|offset| from + offset)
                .unwrap_or_else(|| panic!("no {wanted:?} after {from}: {events:?}"))
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
    }

    fn assert_gone(events: &[Event]) {
        for (role, anchor) in [("anchor", true), ("query", false)] {
            let pid = pid_of(events, anchor).unwrap_or_else(|| panic!("no {role}: {events:?}"));
            assert!(!alive(&pid.to_string()), "the {role} {pid} must be gone at return");
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
        assert!(!alive(&anchor.to_string()), "the anchor {anchor} must be gone");
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
