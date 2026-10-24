use std::fmt;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use process_wrap::std::{ChildWrapper, CommandWrap};

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

/// Runs a query in its own process group (Unix) or job object (Windows), so the deadline, an
/// oversized answer, or a process it left behind can stop the whole tree. Every outcome, success
/// included, ends with `settle`, and only a settled query's buffers are read.
fn run_query(
    invocation: &Invocation,
    deadline: Duration,
    cap: usize,
) -> Result<Captured, QueryError> {
    run_query_with(invocation, deadline, cap, QUERY_CLEANUP_GRACE)
}

fn run_query_with(
    invocation: &Invocation,
    deadline: Duration,
    cap: usize,
    grace: Duration,
) -> Result<Captured, QueryError> {
    let mut wrapped = CommandWrap::from(command(invocation));
    #[cfg(unix)]
    {
        wrapped.wrap(ProcessGroup::leader());
    }
    #[cfg(windows)]
    {
        wrapped.wrap(JobObject);
    }
    let mut child = wrapped.spawn().map_err(QueryError::Spawn)?;
    let stdout = reader(child.stdout().take(), Keep::All(cap));
    let stderr = reader(child.stderr().take(), Keep::All(cap));
    let outcome = watch(child.as_mut(), &stdout, &stderr, deadline, cap);
    if let Err(detail) = settle(child.as_mut(), &stdout, &stderr, grace) {
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

/// Waits for the query's direct child to exit, stopping early on an oversized stream or at the
/// deadline. Output still held open after the exit belongs to `settle`.
fn watch(
    child: &mut dyn ChildWrapper,
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
        if let Some(status) = child.try_wait().map_err(QueryError::Wait)? {
            return Ok(status.code());
        }
        if Instant::now() >= limit {
            return Err(QueryError::TimedOut(deadline));
        }
        thread::sleep(POLL);
    }
}

/// Stops whatever is left of the query and confirms it within one shared `grace`: the whole
/// group or job is killed, the direct child is reaped, the Unix process group is empty, and
/// both readers reached EOF. A reader blocked on output held by a process that left the group
/// can't be confirmed, so that is an error (its thread stays blocked until that process exits).
///
/// Windows exposes no safe check that a job has no processes left, so there the job's
/// termination, the direct child's exit, and both readers' EOF are the whole confirmation.
///
/// On Unix the group is signalled right after `watch` reaped its leader. Once the group is empty
/// its id is free, so in principle a new group could take it in that instant; the empty-group
/// probe sends no signal, so a reused id can only make cleanup fail, never kill.
fn settle(
    child: &mut dyn ChildWrapper,
    stdout: &Shared,
    stderr: &Shared,
    grace: Duration,
) -> Result<(), String> {
    let limit = Instant::now() + grace;
    killed(child.start_kill())?;
    let mut reaped = false;
    loop {
        if !reaped {
            reaped = child
                .try_wait()
                .map_err(|error| format!("cannot reap it: {error}"))?
                .is_some();
        }
        #[cfg(unix)]
        let empty = group_empty(child.id())?;
        #[cfg(windows)]
        let empty = true;
        let drained = lock(stdout).done && lock(stderr).done;
        if reaped && empty && drained {
            return Ok(());
        }
        if Instant::now() >= limit {
            let pending = [
                (!reaped, "it has not exited"),
                (!empty, "processes it started are still running"),
                (!drained, "its output is still held open"),
            ];
            let pending: Vec<&str> = pending
                .iter()
                .filter(|(still, _)| *still)
                .map(|(_, what)| *what)
                .collect();
            return Err(format!(
                "{} {} seconds after loadout stopped it",
                pending.join(", and "),
                grace.as_secs()
            ));
        }
        thread::sleep(POLL);
    }
}

/// The result of killing the group or job. On Unix, `ESRCH` from `killpg` means the group is
/// already gone, which is the goal; every other error is real.
fn killed(result: io::Result<()>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) => Ok(()),
        Err(error) => Err(format!("cannot stop it: {error}")),
    }
}

/// Whether the query's process group has no members left. `ProcessGroup::leader()` makes the
/// direct child the group leader, so the group id is its pid. Signal 0 delivers nothing.
#[cfg(unix)]
fn group_empty(leader: u32) -> Result<bool, String> {
    use nix::errno::Errno;
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;

    let group =
        i32::try_from(leader).map_err(|_| format!("group id {leader} is out of range"))?;
    match killpg(Pid::from_raw(group), None) {
        Ok(()) => Ok(false),
        Err(Errno::ESRCH) => Ok(true),
        Err(error) => Err(format!("cannot check process group {group}: {error}")),
    }
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
    drop(thread::spawn(move || {
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
    }));
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
