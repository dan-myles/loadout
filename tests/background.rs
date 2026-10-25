//! The background supervisor and the worker handshake, through the public API and real
//! processes. This test binary is its own fixture: the ignored `worker` and `supervisor` tests
//! do nothing unless a test re-runs this executable with their role in the environment.
//!
//! Ownership rules for every control here:
//! - each child a test starts is wrapped in [`Owned`] the moment it exists, so any failure,
//!   panic included, kills and reaps it within a bound while the test still owns it. On Unix a
//!   real observation or kill error ends that ownership: the child is then left alone and the
//!   cleanup reported unconfirmed (see [`Owned`]);
//! - a process the test did not start is never signalled. It is observed without any process
//!   identity: each fixture takes an exclusive lock on `<name>.lock` and never releases it
//!   itself (the handle is leaked, so no return, unwind or exit path in the fixture closes it;
//!   only the OS does, as the process ends), and writes `<name>.ready` only after taking it. A
//!   released lock therefore shows the fixture reached its end. It is not a confirmed exit (only
//!   an owned handle gives that), it can lag the exit on Windows, and it would be fooled by a
//!   fixture that released early, which none here does (the unit tests show that case). A
//!   missing ready or lock file, or any lock error but "held", fails the observation; silence
//!   never counts as an end. Every fixture also ends by itself within a minute, as a lifetime
//!   bound, not a cleanup;
//! - each in-process `supervise` call runs under a [`Watchdog`] that aborts this test process if
//!   the call outlives its bound. That closes the supervisor's pipes and job handle: a worker
//!   past its handshake then stops its group (Unix EOF) or dies with the job (Windows), and the
//!   fixtures' own lifetime bound is the fallback.

use std::env;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use loadout::process::{self, Ended, Error, LOG_CAP, Limits};

const ROLE: &str = "LOADOUT_TEST_ROLE";
const DIR: &str = "LOADOUT_TEST_DIR";
const STARTED: &str = "LOADOUT_TEST_STARTED";
const SUPERVISE: &str = "LOADOUT_TEST_SUPERVISE";
const NONCE: &str = "LOADOUT_WORKER_NONCE";
/// The exit code of a worker whose handshake was refused.
const REFUSED: i32 = 42;
/// How long any fixture process lives at most by itself.
const FIXTURE_LIFETIME: Duration = Duration::from_secs(60);

/// The fixture worker. It does nothing in a normal test run.
#[test]
#[ignore = "a background worker that the other tests start"]
fn worker() {
    let Ok(role) = env::var(ROLE) else {
        return;
    };
    if let Some(started) = env::var_os(STARTED) {
        std::fs::write(started, "started").unwrap();
    }
    let supervised = match process::handshake() {
        Ok(supervised) => supervised,
        Err(refused) => {
            eprintln!("{refused}");
            std::process::exit(REFUSED);
        }
    };
    let dir = env::var_os(DIR).map(PathBuf::from);
    let until = Instant::now() + FIXTURE_LIFETIME;
    match role.as_str() {
        "exit" => std::process::exit(7),
        "obey" => {
            while !supervised.cancel().is_cancelled() && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            std::process::exit(3);
        }
        "ignore" => {
            while Instant::now() < until {
                thread::sleep(Duration::from_millis(200));
            }
            std::process::exit(0);
        }
        "holder" => {
            let dir = dir.expect("a holder needs its directory");
            start_descendant(&dir, Stdio::inherit());
            std::process::exit(0);
        }
        "flood" => {
            let chunk = vec![b'x'; 64 * 1024];
            let mut stdout = std::io::stdout().lock();
            for _ in 0..48 {
                stdout.write_all(&chunk).unwrap();
            }
            stdout.flush().unwrap();
            std::process::exit(0);
        }
        "linger" => {
            let dir = dir.expect("a lingering worker needs its directory");
            hold(&dir, "root");
            start_descendant(&dir, Stdio::null());
            ready(&dir, "root");
            while Instant::now() < until {
                thread::sleep(Duration::from_millis(100));
            }
            std::process::exit(0);
        }
        _ => std::process::exit(99),
    }
}

/// The fixture descendant: holds `descendant.lock` until the OS releases it, writes
/// `descendant.ready`, and ends by itself within a minute. It does nothing in a normal test run.
#[test]
#[ignore = "a descendant that the worker fixture starts"]
fn descendant() {
    let (Ok(_), Some(dir)) = (env::var(ROLE), env::var_os(DIR)) else {
        return;
    };
    let dir = PathBuf::from(dir);
    hold(&dir, "descendant");
    ready(&dir, "descendant");
    let until = Instant::now() + FIXTURE_LIFETIME;
    while Instant::now() < until {
        thread::sleep(Duration::from_millis(100));
    }
}

/// Takes the exclusive lock `name.lock` and leaks its handle, so this process never releases
/// it: not on return to the test harness, not on unwind, not on exit. Only the OS does, as the
/// process ends.
fn hold(dir: &Path, name: &str) {
    let lock = File::create(dir.join(format!("{name}.lock"))).unwrap();
    lock.lock().unwrap();
    std::mem::forget(lock);
}

/// Says `name` holds its lock and runs.
fn ready(dir: &Path, name: &str) {
    std::fs::write(dir.join(format!("{name}.ready")), "ready").unwrap();
}

/// Starts the descendant fixture with the given stdout, waits for it to be ready, and leaves
/// it running on purpose: the supervisor's containment must stop it.
fn start_descendant(dir: &Path, stdout: Stdio) {
    let descendant = fixture("descendant", "descendant", dir)
        .stdin(Stdio::null())
        .stdout(stdout)
        .spawn()
        .unwrap();
    drop(descendant);
    let until = Instant::now() + Duration::from_secs(30);
    while !dir.join("descendant.ready").exists() {
        assert!(Instant::now() < until, "the descendant never reported ready");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Whether the OS released `name.lock`: the fixture reached its end, not a confirmed exit (see
/// the module doc).
fn released(dir: &Path, name: &str) -> Result<bool, String> {
    let ready = dir.join(format!("{name}.ready"));
    if !ready.is_file() {
        return Err(format!("{} is missing, so nothing can be concluded", ready.display()));
    }
    let path = dir.join(format!("{name}.lock"));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    match lock.try_lock() {
        Ok(()) => lock
            .unlock()
            .map(|()| true)
            .map_err(|error| format!("cannot unlock: {error}")),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(format!("cannot test the lock: {error}")),
    }
}

/// `released`, polled within a bound: "eventually released", weaker than released at an API's
/// return.
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

/// The fixture supervisor, for the test that kills a supervisor. It does nothing in a normal
/// test run.
#[test]
#[ignore = "a background supervisor that the supervisor-death test starts and kills"]
fn supervisor() {
    let Ok(role) = env::var(SUPERVISE) else {
        return;
    };
    let dir = PathBuf::from(env::var_os(DIR).unwrap());
    let limits = Limits {
        soft: FIXTURE_LIFETIME,
        hard: FIXTURE_LIFETIME,
        settle: Duration::from_secs(30),
    };
    let _ = process::supervise(fixture("worker", &role, &dir), limits);
}

/// This executable, running only the named ignored fixture test.
fn fixture(test: &str, role: &str, dir: &Path) -> Command {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(["--exact", test, "--ignored", "--nocapture", "--test-threads=1"])
        .env(ROLE, role)
        .env(DIR, dir);
    command
}

#[cfg(unix)]
fn sleeper(seconds: u32) -> Command {
    let mut command = Command::new("sleep");
    command.arg(seconds.to_string());
    command
}

#[cfg(windows)]
fn sleeper(seconds: u32) -> Command {
    let mut command = Command::new("ping.exe");
    command.args(["-n", &seconds.to_string(), "127.0.0.1"]);
    command
}

fn short(soft: u64, hard: u64) -> Limits {
    Limits {
        soft: Duration::from_secs(soft),
        hard: Duration::from_secs(hard),
        settle: Duration::from_secs(20),
    }
}

/// A child this test started. However the test ends, it is stopped within a bound if this
/// test still owns it; one that can't be confirmed gone fails the test (unless the test is
/// already failing). An optional record receives its exit status and every call made on it.
///
/// Ownership follows the same platform rule as the unit tests' guard (`src/process/owned.rs`).
/// On Windows the retained handle pins the process, so an observation error still leads to a
/// kill and a reap. On Unix a `Child` pins its process only while it is unreaped: a real
/// `try_wait` or kill error may mean it was reaped elsewhere, and a later kill could then
/// address a reused PID. So such an error latches the identity as lost: the error is kept,
/// cleanup is reported unconfirmed, and no further poll or kill is made, in an explicit stop,
/// a retry, or `Drop`.
struct Owned {
    child: Child,
    exited: Option<ExitStatus>,
    /// Unix only: why the identity is lost, and whether an explicit call already reported it.
    lost: Option<(String, bool)>,
    record: Option<Arc<Mutex<Record>>>,
    /// Test-only: how many next observations get an error in place of the OS's answer.
    os_errors: u32,
}

/// How a stop reports a child whose identity is lost.
const LEFT_ALONE: &str = "its identity is lost, so it is left alone; cleanup unconfirmed";

/// Every call an [`Owned`] made on its child, kept outside it so a control can check it after
/// the guard is gone.
#[derive(Debug, Default, Clone, Copy)]
struct Record {
    exit: Option<ExitStatus>,
    /// `try_wait` calls that reached the OS.
    polls: u32,
    kills: u32,
}

impl Owned {
    fn spawn(command: &mut Command) -> Owned {
        Owned {
            child: command.spawn().unwrap(),
            exited: None,
            lost: None,
            record: None,
            os_errors: 0,
        }
    }

    fn recording(mut self, record: &Arc<Mutex<Record>>) -> Owned {
        self.record = Some(Arc::clone(record));
        self
    }

    /// Lends the child to Windows code the test drives (job assignment, resume), which keeps
    /// the retained handle.
    #[cfg(windows)]
    fn child(&mut self) -> &mut Child {
        &mut self.child
    }

    #[cfg(unix)]
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// Test-only: the next observation gets an error in place of the OS's answer, judged as a
    /// real one would be. A safe stand-in for `ECHILD`: no reaper, no PID reuse.
    fn stand_in_os_error(&mut self) {
        self.os_errors += 1;
    }

    fn note(&self, change: impl FnOnce(&mut Record)) {
        if let Some(record) = &self.record {
            change(&mut record.lock().unwrap_or_else(PoisonError::into_inner));
        }
    }

    /// A real error (or its stand-in) about the child. On Unix it latches the identity as lost.
    fn failed(&mut self, error: &io::Error) -> String {
        let id = self.child.id();
        #[cfg(unix)]
        {
            let why = format!(
                "fixture {id}: {error}; it may have been reaped elsewhere, so its identity is lost"
            );
            self.lost.get_or_insert((why.clone(), false));
            why
        }
        #[cfg(windows)]
        format!("fixture {id}: {error}")
    }

    /// One observation: `Some` once the child has positively exited. After a loss, every call
    /// returns that loss without touching the child.
    fn observe(&mut self) -> Result<Option<ExitStatus>, String> {
        if let Some(status) = self.exited {
            return Ok(Some(status));
        }
        if let Some((why, _)) = &self.lost {
            return Err(why.clone());
        }
        let answer = if self.os_errors > 0 {
            self.os_errors -= 1;
            Err(io::Error::other("injected: the OS could not report on the child"))
        } else {
            self.note(|record| record.polls += 1);
            self.child.try_wait()
        };
        match answer {
            Ok(Some(status)) => {
                self.exited = Some(status);
                self.note(|record| record.exit = Some(status));
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(self.failed(&error)),
        }
    }

    fn alive(&mut self) -> bool {
        self.observe().unwrap().is_none()
    }

    /// Waits for the child within `limit`. Past it the test fails, and `Drop` stops it.
    fn exit_within(&mut self, limit: Duration) -> ExitStatus {
        let until = Instant::now() + limit;
        loop {
            if let Some(status) = self.observe().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < until,
                "process {} did not exit within {limit:?}",
                self.child.id()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A deliberate kill of a child this test still owns: nothing once it has exited, and a
    /// refusal once its identity is lost.
    fn kill(&mut self) -> Result<(), String> {
        if self.observe()?.is_some() {
            return Ok(());
        }
        self.note(|record| record.kills += 1);
        match self.child.kill() {
            Ok(()) => Ok(()),
            Err(error) => Err(self.failed(&error)),
        }
    }

    /// Stops the child within 10 seconds: nothing to do once it has positively exited; nothing
    /// at all once its identity is lost; otherwise a kill, even after an observation error that
    /// left it pinned (Windows), then a poll for its exit. Every problem is kept, in order: the
    /// observation, the kill, the reap; a lost identity always reports the cleanup unconfirmed.
    fn stop(&mut self) -> Result<(), String> {
        let result = self.stopping();
        if let Some((_, reported)) = &mut self.lost {
            *reported = true;
        }
        result
    }

    fn stopping(&mut self) -> Result<(), String> {
        let mut problems = Vec::new();
        match self.observe() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(why) => problems.push(format!("cannot check {why}")),
        }
        if self.lost.is_none() {
            self.note(|record| record.kills += 1);
            if let Err(error) = self.child.kill() {
                let why = self.failed(&error);
                problems.push(format!("cannot kill {why}"));
            }
        }
        if self.lost.is_none() {
            let until = Instant::now() + Duration::from_secs(10);
            let mut reap = None;
            loop {
                match self.observe() {
                    Ok(Some(_)) => break,
                    Ok(None) => {}
                    Err(why) => {
                        reap.get_or_insert_with(|| format!("cannot reap {why}"));
                        if self.lost.is_some() {
                            break;
                        }
                    }
                }
                if Instant::now() >= until {
                    if self.exited.is_none() {
                        let id = self.child.id();
                        reap.get_or_insert_with(|| {
                            format!("fixture {id} did not exit within 10 seconds")
                        });
                    }
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            problems.extend(reap);
        }
        if self.lost.is_some() {
            problems.push(LEFT_ALONE.to_owned());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        self.os_errors = 0;
        let already = matches!(self.lost, Some((_, true)));
        if let Err(detail) = self.stopping() {
            if already || thread::panicking() {
                eprintln!("{detail}");
            } else {
                panic!("{detail}");
            }
        }
    }
}

/// Aborts this test process if dropped too late. Aborting closes every handle the process
/// holds: a supervisor's control pipe (so a genuine worker sees EOF and stops its group) and its
/// kill-on-close job. It is the outer bound for in-process `supervise` calls.
struct Watchdog(Arc<AtomicBool>);

impl Watchdog {
    fn new(limit: Duration, what: &'static str) -> Watchdog {
        let done = Arc::new(AtomicBool::new(false));
        let watching = Arc::clone(&done);
        thread::Builder::new()
            .name("test-watchdog".to_owned())
            .spawn(move || {
                let until = Instant::now() + limit;
                while !watching.load(Ordering::SeqCst) {
                    if Instant::now() >= until {
                        eprintln!("watchdog: {what} outlived {limit:?}; aborting");
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

fn supervised(role: &str, dir: &Path, limits: Limits) -> process::Outcome {
    let _watchdog = Watchdog::new(Duration::from_secs(120), "a supervise call");
    process::supervise(fixture("worker", role, dir), limits).unwrap()
}

/// A test-owned process with no relation to any supervised containment.
struct Canary(Owned);

impl Canary {
    #[cfg(unix)]
    fn new() -> Canary {
        use std::os::unix::process::CommandExt;
        Canary(Owned::spawn(sleeper(60).process_group(0)))
    }

    #[cfg(windows)]
    fn new() -> Canary {
        Canary(Owned::spawn(&mut sleeper(60)))
    }

    #[cfg(unix)]
    fn group(&self) -> i32 {
        i32::try_from(self.0.id()).unwrap()
    }

    fn alive(&mut self) -> bool {
        self.0.alive()
    }
}

#[test]
fn an_owned_fixture_is_reaped_when_a_test_fails_right_after_spawn() {
    let record = Arc::new(Mutex::new(Record::default()));
    let failed = catch_unwind(AssertUnwindSafe(|| {
        let mut fixture = Owned::spawn(sleeper(60).stdin(Stdio::piped())).recording(&record);
        let _input = fixture.take_stdin();
        panic!("injected: failure after spawn, before any write or wait");
    }));
    assert!(failed.is_err());
    assert!(
        record.lock().unwrap().exit.is_some(),
        "the fixture must have been killed and reaped"
    );
}

/// The guard's ordinary positive, recorded: a live owned fixture is killed once and reaped, and
/// `Drop` adds nothing.
#[test]
fn a_live_owned_fixture_is_killed_once_and_reaped() {
    let record = Arc::new(Mutex::new(Record::default()));
    let mut fixture = Owned::spawn(&mut sleeper(60)).recording(&record);
    assert!(fixture.alive());
    assert_eq!(fixture.stop(), Ok(()));
    drop(fixture);
    let record = *record.lock().unwrap();
    assert!(record.exit.is_some());
    assert_eq!(record.kills, 1);
}

/// A fixture that has positively exited is not killed, by `stop` or by `Drop`.
#[test]
fn a_fixture_that_already_exited_is_not_killed() {
    let record = Arc::new(Mutex::new(Record::default()));
    let mut fixture = Owned::spawn(&mut sleeper(0)).recording(&record);
    fixture.exit_within(Duration::from_secs(20));
    assert_eq!(fixture.stop(), Ok(()));
    drop(fixture);
    assert_eq!(record.lock().unwrap().kills, 0);
}

/// Unix: an OS observation error (here a stand-in, with no reaper and no PID reuse) latches the
/// fixture's identity as lost. The error is kept, cleanup is reported unconfirmed, and neither
/// a retry nor `Drop` polls or signals it. The fixture ends by itself within a second, and its
/// status is deliberately never collected.
#[cfg(unix)]
#[test]
fn a_fixture_whose_identity_is_lost_is_never_signalled_or_polled_again() {
    let record = Arc::new(Mutex::new(Record::default()));
    let mut fixture = Owned::spawn(&mut sleeper(1)).recording(&record);
    fixture.stand_in_os_error();
    let detail = fixture.stop().unwrap_err();
    assert!(detail.starts_with("cannot check fixture"), "{detail}");
    assert!(detail.contains("injected: the OS"), "{detail}");
    assert!(detail.ends_with("cleanup unconfirmed"), "{detail}");
    assert!(fixture.stop().is_err(), "a retry stays refused");
    assert!(fixture.kill().is_err(), "so does a deliberate kill");
    let before = *record.lock().unwrap();
    assert_eq!((before.polls, before.kills), (0, 0));
    drop(fixture);
    let after = *record.lock().unwrap();
    assert_eq!((after.polls, after.kills), (0, 0), "Drop must not touch it");
    assert!(after.exit.is_none());
}

/// Windows: the retained handle still pins the fixture after an OS observation error, so it is
/// killed and reaped, with that error kept first.
#[cfg(windows)]
#[test]
fn a_fixture_with_a_retained_handle_is_still_stopped_after_an_os_error() {
    let record = Arc::new(Mutex::new(Record::default()));
    let mut fixture = Owned::spawn(&mut sleeper(60)).recording(&record);
    fixture.stand_in_os_error();
    let detail = fixture.stop().unwrap_err();
    assert!(detail.starts_with("cannot check fixture"), "{detail}");
    assert!(!detail.contains("did not exit"), "{detail}");
    let record = *record.lock().unwrap();
    assert!(record.exit.is_some(), "the fixture must have been reaped");
    assert_eq!(record.kills, 1);
}

/// The observer: absence, a missing lock and a lock that can't be opened are errors; a live
/// fixture doing nothing holds its lock; only the owned handle confirms its exit (`stop` reaps
/// it), and the release follows.
#[test]
fn the_lock_observer_never_mistakes_silence_or_absence_for_death() {
    let dir = tempfile::tempdir().unwrap();
    let (ready, lock) = (
        dir.path().join("descendant.ready"),
        dir.path().join("descendant.lock"),
    );
    assert!(released(dir.path(), "descendant").is_err(), "absence must not count");
    std::fs::write(&ready, "ready").unwrap();
    assert!(released(dir.path(), "descendant").is_err(), "a missing lock must not count");
    std::fs::create_dir(&lock).unwrap();
    assert!(released(dir.path(), "descendant").is_err(), "a lock error must not count");
    std::fs::remove_dir(&lock).unwrap();
    std::fs::remove_file(&ready).unwrap();

    let mut fixture_process = Owned::spawn(
        fixture("descendant", "descendant", dir.path()).stdin(Stdio::null()),
    );
    let until = Instant::now() + Duration::from_secs(60);
    while !ready.exists() {
        assert!(fixture_process.alive(), "the fixture exited before it became ready");
        assert!(Instant::now() < until, "the fixture never became ready");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(released(dir.path(), "descendant"), Ok(false), "a silent live fixture holds it");
    assert!(fixture_process.alive());
    assert_eq!(fixture_process.stop(), Ok(()), "the owned handle must confirm the exit");
    assert_eq!(eventually_released(dir.path(), "descendant"), Ok(true));
}

#[test]
fn a_worker_that_exits_by_itself_is_reported_and_its_tree_confirmed_gone() {
    let dir = tempfile::tempdir().unwrap();
    let mut outside = Canary::new();
    let outcome = supervised("exit", dir.path(), short(60, 120));
    assert_eq!(
        outcome.ended,
        Ended::Exited {
            code: Some(7),
            cancelled: false
        }
    );
    assert_eq!(outcome.cleanup, Ok(()));
    assert!(outcome.log.complete);
    assert!(outcome.log.bytes.len() <= LOG_CAP);
    assert!(outside.alive(), "a process outside the supervised tree was affected");
}

#[test]
fn the_soft_deadline_asks_the_worker_to_stop_over_its_control_pipe() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let outcome = supervised("obey", dir.path(), short(2, 60));
    assert_eq!(
        outcome.ended,
        Ended::Exited {
            code: Some(3),
            cancelled: true
        }
    );
    assert_eq!(outcome.cleanup, Ok(()));
    assert!(started.elapsed() < Duration::from_secs(30));
}

#[test]
fn a_worker_that_ignores_the_soft_deadline_is_killed_at_the_hard_one() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let outcome = supervised("ignore", dir.path(), short(1, 4));
    assert_eq!(outcome.ended, Ended::TimedOut { cancelled: true });
    assert_eq!(outcome.cleanup, Ok(()));
    assert!(started.elapsed() < Duration::from_secs(40));
}

/// The worker exits at once, but a process it started still holds its output. The tree is
/// stopped after every exit, so that holder is killed with it, and only then does the log
/// count as complete.
#[test]
fn a_descendant_holding_the_output_is_stopped_after_the_worker_exits() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = supervised("holder", dir.path(), short(60, 120));
    // Unix: the holder was in the supervisor's group, which `cleanup` confirmed empty
    // (signal 0 answered ESRCH, so every member was reaped): that is the supervisor's own exit
    // confirmation, and the lock, released on the first check after return, agrees with it.
    // Windows: the job's process count reached zero; a lock released by a terminated process
    // may lag its exit, so only "eventually released" is asserted there.
    let at_return = released(dir.path(), "descendant");
    assert_eq!(
        outcome.ended,
        Ended::Exited {
            code: Some(0),
            cancelled: false
        }
    );
    assert_eq!(outcome.cleanup, Ok(()));
    assert!(outcome.log.complete);
    #[cfg(unix)]
    assert_eq!(at_return, Ok(true), "the holder must have ended when supervise returns");
    #[cfg(windows)]
    {
        assert!(at_return.is_ok(), "{at_return:?}");
        assert_eq!(eventually_released(dir.path(), "descendant"), Ok(true));
    }
}

#[test]
fn worker_output_is_kept_within_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = supervised("flood", dir.path(), short(60, 120));
    assert_eq!(outcome.cleanup, Ok(()));
    assert!(outcome.log.dropped >= 2 * 1024 * 1024, "{}", outcome.log.dropped);
    assert!(outcome.log.bytes.len() <= LOG_CAP + 100);
    assert!(
        String::from_utf8_lossy(&outcome.log.bytes[..100])
            .contains("earlier bytes of worker output were dropped")
    );
}

/// Invalid limits are refused before anything starts: the worker never writes its start
/// marker. A valid run of the same worker does.
#[test]
fn invalid_limits_start_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("started");
    let valid = short(60, 120);
    let invalid = [
        Limits {
            soft: Duration::MAX,
            ..valid
        },
        Limits {
            hard: Duration::MAX,
            soft: Duration::from_secs(1),
            ..valid
        },
        Limits {
            settle: Duration::MAX,
            ..valid
        },
        Limits {
            soft: Duration::from_secs(10),
            hard: Duration::from_secs(5),
            ..valid
        },
        Limits {
            settle: Duration::ZERO,
            ..valid
        },
    ];
    for limits in invalid {
        let mut worker = fixture("worker", "exit", dir.path());
        worker.env(STARTED, &marker);
        let refused = process::supervise(worker, limits);
        assert!(matches!(refused, Err(Error::InvalidLimits(_))), "{limits:?}: {refused:?}");
        assert!(!marker.exists(), "{limits:?} started the worker");
    }
    let _watchdog = Watchdog::new(Duration::from_secs(120), "the valid positive");
    let mut worker = fixture("worker", "exit", dir.path());
    worker.env(STARTED, &marker);
    let outcome = process::supervise(worker, valid).unwrap();
    assert!(marker.exists());
    assert_eq!(outcome.cleanup, Ok(()));
}

/// A supervisor killed from outside leaves nothing of its tree running: on Unix the worker
/// sees its control pipe close and kills its own verified group; on Windows the kill-on-close
/// job ends. A process outside that tree is untouched. The test owns only the supervisor; the
/// worker and its descendant are observed by their locks alone, so the claim for them is
/// "eventually released" (each reached its end), not a confirmed exit.
#[test]
fn a_worker_whose_supervisor_dies_does_not_outlive_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut outside = Canary::new();
    let mut command = fixture("supervisor", "linger", dir.path());
    command.env(SUPERVISE, "linger").stdin(Stdio::null());
    let mut supervisor = Owned::spawn(&mut command);
    let until = Instant::now() + Duration::from_secs(60);
    while !dir.path().join("root.ready").exists() {
        assert!(Instant::now() < until, "the worker never reported ready");
        thread::sleep(Duration::from_millis(50));
    }
    // The worker writes its ready file only once its descendant is ready too.
    assert_eq!(released(dir.path(), "root"), Ok(false), "the worker must run before the kill");
    assert_eq!(released(dir.path(), "descendant"), Ok(false), "so must its descendant");
    supervisor.kill().unwrap();
    supervisor.exit_within(Duration::from_secs(10));
    assert_eq!(
        eventually_released(dir.path(), "root"),
        Ok(true),
        "the worker kept running after its supervisor died"
    );
    assert_eq!(
        eventually_released(dir.path(), "descendant"),
        Ok(true),
        "the worker's descendant kept running after the supervisor died"
    );
    assert!(outside.alive(), "a process outside the supervised tree was affected");
}

/// Workers started any other way than by `supervise` refuse before they lock, write, or signal
/// anything, when their startup is inconsistent. On Unix a canary leads the worker's group: it is
/// a test-owned, unreaped child, so its group id stays its own, and it would die if the worker
/// signalled that group.
#[cfg(unix)]
mod refusals {
    use std::os::unix::process::CommandExt;

    use nix::unistd::getpgrp;

    use super::*;

    const A_NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Starts the worker fixture directly, as a shell would, with `line` (if any) on its stdin.
    fn direct(group: Option<i32>, line: Option<&str>, nonce: Option<&str>) -> ExitStatus {
        let dir = tempfile::tempdir().unwrap();
        let mut command = fixture("worker", "exit", dir.path());
        command.env_remove(NONCE);
        if let Some(nonce) = nonce {
            command.env(NONCE, nonce);
        }
        if let Some(group) = group {
            command.process_group(group);
        }
        command.stdin(if line.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut worker = Owned::spawn(&mut command);
        let input = worker.take_stdin();
        if let (Some(mut input), Some(line)) = (input.as_ref(), line) {
            let _ = input.write_all(line.as_bytes());
        }
        let status = worker.exit_within(Duration::from_secs(30));
        drop(input);
        status
    }

    fn line(nonce: &str, group: i32, supervisor: u32) -> String {
        format!("LOADOUT-WORKER 1 {nonce} {group} {supervisor}\n")
    }

    #[test]
    fn without_a_nonce_or_a_pipe_the_worker_refuses() {
        let mut canary = Canary::new();
        assert_eq!(direct(Some(canary.group()), None, None).code(), Some(REFUSED));
        assert_eq!(direct(Some(canary.group()), None, Some(A_NONCE)).code(), Some(REFUSED));
        assert!(canary.alive());
    }

    #[test]
    fn a_pipe_with_no_line_is_refused_after_the_handshake_wait() {
        let mut canary = Canary::new();
        let started = Instant::now();
        assert_eq!(
            direct(Some(canary.group()), Some(""), Some(A_NONCE)).code(),
            Some(REFUSED)
        );
        assert!(started.elapsed() >= Duration::from_secs(5));
        assert!(canary.alive());
    }

    #[test]
    fn a_line_whose_nonce_differs_is_refused() {
        let mut canary = Canary::new();
        let other = A_NONCE.replace('0', "1");
        let status = direct(
            Some(canary.group()),
            Some(&line(&other, canary.group(), std::process::id())),
            Some(A_NONCE),
        );
        assert_eq!(status.code(), Some(REFUSED));
        assert!(canary.alive());
    }

    #[test]
    fn a_line_naming_another_group_is_refused() {
        let mut canary = Canary::new();
        let status = direct(
            Some(canary.group()),
            Some(&line(A_NONCE, getpgrp().as_raw(), std::process::id())),
            Some(A_NONCE),
        );
        assert_eq!(status.code(), Some(REFUSED));
        assert!(canary.alive());
    }

    #[test]
    fn a_line_naming_another_parent_is_refused() {
        let mut canary = Canary::new();
        let status = direct(
            Some(canary.group()),
            Some(&line(A_NONCE, canary.group(), 1)),
            Some(A_NONCE),
        );
        assert_eq!(status.code(), Some(REFUSED));
        assert!(canary.alive());
    }

    /// A worker that leads its own group, as a job started from a shell does.
    #[test]
    fn a_worker_leading_its_own_group_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = fixture("worker", "exit", dir.path());
        command
            .env(NONCE, A_NONCE)
            .process_group(0)
            .stdin(Stdio::piped());
        let mut worker = Owned::spawn(&mut command);
        let own = i32::try_from(worker.id()).unwrap();
        let input = worker.take_stdin();
        if let Some(mut input) = input.as_ref() {
            let _ = input.write_all(line(A_NONCE, own, std::process::id()).as_bytes());
        }
        let status = worker.exit_within(Duration::from_secs(30));
        drop(input);
        assert_eq!(status.code(), Some(REFUSED));
    }

    /// The worker shares this test's own group, which is also its supervisor's. Signalling the
    /// group would kill this test.
    #[test]
    fn a_worker_in_its_supervisors_group_is_refused() {
        let status = direct(
            None,
            Some(&line(A_NONCE, getpgrp().as_raw(), std::process::id())),
            Some(A_NONCE),
        );
        assert_eq!(status.code(), Some(REFUSED));
    }

    /// The accepted cooperative-protocol limit (contracts/u8-handshake-disposition-r1.md): a
    /// same-user caller that deliberately arranges a matching nonce, pipe, parent and group
    /// passes the handshake without `supervise`. Here the group is led by a test-owned,
    /// unreaped canary and the fixture role exits before its control pipe closes, so nothing is
    /// ever signalled; a canary outside that group is checked untouched too.
    #[test]
    fn a_matching_caller_arranged_topology_is_accepted_as_a_cooperative_limit() {
        let mut outside = Canary::new();
        let mut leader = Canary::new();
        let status = direct(
            Some(leader.group()),
            Some(&line(A_NONCE, leader.group(), std::process::id())),
            Some(A_NONCE),
        );
        assert_eq!(
            status.code(),
            Some(7),
            "the handshake is not authentication; a matching topology passes it"
        );
        assert!(leader.alive() && outside.alive());
    }

    /// The positive control: the same fixture, started by the supervisor, is accepted.
    #[test]
    fn the_supervisor_started_worker_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = supervised("exit", dir.path(), short(60, 120));
        assert_eq!(
            outcome.ended,
            Ended::Exited {
                code: Some(7),
                cancelled: false
            }
        );
    }
}

/// Windows startup checks: stdin must be a pipe (`FILE_TYPE_PIPE`) and the worker must be in
/// some job, which is not proof of the supervisor's job.
#[cfg(windows)]
mod refusals {
    use super::*;

    const A_NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn line() -> String {
        format!("LOADOUT-WORKER 1 {A_NONCE} 0 {}\n", std::process::id())
    }

    fn direct(stdin: Stdio, line: Option<&str>) -> ExitStatus {
        let dir = tempfile::tempdir().unwrap();
        let mut command = fixture("worker", "exit", dir.path());
        command.env(NONCE, A_NONCE).stdin(stdin);
        let mut worker = Owned::spawn(&mut command);
        let input = worker.take_stdin();
        if let (Some(mut input), Some(line)) = (input.as_ref(), line) {
            let _ = input.write_all(line.as_bytes());
        }
        let status = worker.exit_within(Duration::from_secs(30));
        drop(input);
        status
    }

    #[test]
    fn a_regular_file_with_a_matching_line_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("line");
        std::fs::write(&path, line()).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(direct(Stdio::from(file), None).code(), Some(REFUSED));
    }

    #[test]
    fn the_null_device_is_refused() {
        assert_eq!(direct(Stdio::null(), None).code(), Some(REFUSED));
    }

    /// A job this test owns, unrelated to any supervisor, satisfies the some-job check: the
    /// documented limit, in a controlled job rather than the ambient one.
    #[test]
    fn an_unrelated_job_the_test_owns_is_accepted_as_some_job() {
        use std::os::windows::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let mut command = fixture("worker", "exit", dir.path());
        command
            .env(NONCE, A_NONCE)
            .stdin(Stdio::piped())
            .creation_flags(loadout_winproc::CREATE_SUSPENDED.0);
        let mut worker = Owned::spawn(&mut command);
        let job = loadout_winproc::Job::new().unwrap();
        job.assign(worker.child()).unwrap();
        let input = worker.take_stdin();
        if let Some(mut input) = input.as_ref() {
            let _ = input.write_all(line().as_bytes());
        }
        loadout_winproc::prepare(worker.child())
            .and_then(|prepared| prepared.resume_before(Instant::now() + Duration::from_secs(30)))
            .unwrap();
        let status = worker.exit_within(Duration::from_secs(30));
        drop(input);
        drop(job);
        assert_eq!(
            status.code(),
            Some(7),
            "a job that isn't the supervisor's passes the some-job check"
        );
    }

    /// The ambient case: without a job the worker refuses. If this test process is itself in a
    /// job (as under many CI runners and IDEs), its child inherits it and the same start is
    /// accepted. One run covers only the branch its environment selects; a truly jobless start
    /// can't be forced from inside a job, so that branch needs a jobless runner.
    #[test]
    fn a_pipe_start_depends_only_on_being_in_some_job() {
        let in_job = loadout_winproc::current_process_in_job().unwrap();
        let status = direct(Stdio::piped(), Some(&line()));
        if in_job {
            assert_eq!(status.code(), Some(7), "some job, not the supervisor's, passes");
        } else {
            assert_eq!(status.code(), Some(REFUSED), "a jobless worker must refuse");
        }
    }
}
