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

/// How long one installed-query (or doctor's query) may run, including any process it starts
/// that keeps its output open. Generous: a query can refresh a source index or fetch an API.
pub(super) const QUERY_DEADLINE: Duration = Duration::from_secs(10 * 60);
/// A query that prints more than this on stdout or stderr is refused, never parsed truncated.
pub(super) const QUERY_OUTPUT_CAP: usize = 8 * 1024 * 1024;
/// How much of each install output stream an error keeps.
pub(super) const INSTALL_TAIL: usize = 64 * 1024;
/// After an install exits, how long to keep reading output that a process it started still
/// holds open. The install itself has no deadline.
const INSTALL_DRAIN: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(20);

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
    Oversized { stream: &'static str, cap: usize },
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
/// oversized answer, or a process it left holding the output can stop the whole tree.
fn run_query(
    invocation: &Invocation,
    deadline: Duration,
    cap: usize,
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
    let limit = Instant::now() + deadline;
    let mut exited = None;
    let mut swept = false;
    let code = loop {
        for (stream, buffer) in [("stdout", &stdout), ("stderr", &stderr)] {
            if lock(buffer).overflowed {
                stop(child.as_mut());
                return Err(QueryError::Oversized { stream, cap });
            }
        }
        if exited.is_none() {
            exited = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    stop(child.as_mut());
                    return Err(QueryError::Wait(error));
                }
            };
        }
        if let Some(status) = exited {
            if lock(&stdout).done && lock(&stderr).done {
                break status.code();
            }
            if !swept {
                // The query exited, but a process it started still holds its output; that
                // process belongs to the query's group, so it goes too.
                stop(child.as_mut());
                swept = true;
            }
        }
        if Instant::now() >= limit {
            stop(child.as_mut());
            return Err(QueryError::TimedOut(deadline));
        }
        thread::sleep(POLL);
    };
    for buffer in [&stdout, &stderr] {
        if let Some(error) = lock(buffer).error.take() {
            return Err(QueryError::Read(error));
        }
    }
    Ok(Captured {
        code,
        stdout: text(&stdout),
        stderr: text(&stderr),
    })
}

fn stop(child: &mut dyn ChildWrapper) {
    let _ = child.start_kill();
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

    #[test]
    fn a_descendant_holding_the_output_is_stopped_once_the_query_exits() {
        let started = Instant::now();
        let captured = run_query(
            &sh("sleep 30 & echo $!; echo done"),
            Duration::from_secs(20),
            1024,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(captured.code, Some(0));
        let mut lines = captured.stdout.lines();
        let pid = lines.next().unwrap().to_owned();
        assert_eq!(lines.next(), Some("done"));
        let alive = || {
            Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("kill -0 {pid} 2>/dev/null"))
                .status()
                .unwrap()
                .success()
        };
        let gone_by = Instant::now() + Duration::from_secs(5);
        while alive() && Instant::now() < gone_by {
            thread::sleep(POLL);
        }
        assert!(
            !alive(),
            "the backgrounded sleep {pid} must be killed with the query's group"
        );
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
