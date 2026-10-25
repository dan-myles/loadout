//! The Windows operations loadout needs that neither std nor `process-wrap`'s safe API offers:
//!
//! - resume a child it created with `CREATE_SUSPENDED`, after the child was placed in a job
//!   object (U7 queries, and the background worker);
//! - a job object of its own whose processes all die when its last handle closes, with
//!   breakaway not allowed, and a count of the processes still in it (the background
//!   supervisor, U8);
//! - whether the current process is in any job, whether its standard input is a pipe, and a
//!   32-byte nonce from the system RNG (the background worker's handshake, U8). These are
//!   cooperative-startup checks only: none of them proves which job or which supervisor.
//!
//! This crate is loadout's only `unsafe` code. It owns every handle it opens (RAII through
//! `windows::core::Owned`). It can't prove a child is suspended: its one-thread check only
//! rules out a child that already runs several threads, and the caller's own
//! `CREATE_SUSPENDED` is what keeps the child from running. `Prepared::resume_before` then
//! resumes nothing unless it found exactly one thread of that child and opened it, never once
//! the caller's deadline has passed, and reports an error unless the thread had been suspended
//! exactly once.
#![cfg(windows)]

use std::io;
use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::time::Instant;

use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, FILE_TYPE_UNKNOWN, GetFileType};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
pub use windows::Win32::System::Threading::CREATE_SUSPENDED;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessIdOfThread, OpenThread, ResumeThread,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};
use windows::core::{BOOL, HRESULT, Owned, PCWSTR};

/// The child's only thread, found and opened for resuming, not yet resumed.
///
/// Only [`prepare`] makes one, so holding one means: one snapshot listed exactly one thread of
/// the child (as in a process created suspended), that thread is open with the rights to resume
/// and query it, and it was confirmed to still belong to the child. The handle is owned and is
/// closed when this is dropped or used.
#[derive(Debug)]
pub struct Prepared {
    thread: Owned<HANDLE>,
    thread_id: u32,
    pid: u32,
}

/// Finds and opens the child's only thread, ready to resume. Nothing is resumed.
///
/// # Errors
///
/// The thread snapshot fails, or lists no thread of the child (it exited) or more than one (it
/// already ran), or the thread can't be opened, or it belongs to another process.
pub fn prepare(child: &Child) -> io::Result<Prepared> {
    let pid = child.id();
    let thread_id = only_thread(pid)?;
    let thread = open_thread(thread_id, pid)?;
    Ok(Prepared {
        thread,
        thread_id,
        pid,
    })
}

impl Prepared {
    /// Resumes the thread, which must have been suspended exactly once, unless `deadline` has
    /// passed: the clock is read after all preparation, as the last step before `ResumeThread`,
    /// and a passed deadline means `ResumeThread` is not called at all.
    ///
    /// Reading the clock and calling `ResumeThread` are not one atomic step: the calling thread
    /// can be preempted between them, so the resume itself can happen just after `deadline`.
    /// This guarantees only that no resume is issued once the deadline was seen to have passed.
    /// A caller that needs to know whether setup finished in time reads the clock again after
    /// this returns.
    ///
    /// The previous suspend count is checked last: 1 means it was suspended once and now runs.
    /// 0 means it wasn't suspended (already running), and more than 1 means it is still
    /// suspended. Both are errors.
    ///
    /// # Errors
    ///
    /// `ErrorKind::TimedOut` if `deadline` had passed, in which case nothing was resumed. Any
    /// other error comes after `ResumeThread` was called and may leave the child running, so the
    /// caller must treat every error from this function as "the child may have run" and settle
    /// its whole job.
    pub fn resume_before(self, deadline: Instant) -> io::Result<()> {
        let Self {
            thread,
            thread_id,
            pid,
        } = self;
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "the deadline passed before thread {thread_id} of process {pid} was resumed, so it was not resumed"
                ),
            ));
        }
        // SAFETY: `thread` is a live handle owned by `Owned`, opened with THREAD_SUSPEND_RESUME,
        // and it stays open for the duration of the call.
        let previous = unsafe { ResumeThread(*thread) };
        match previous {
            u32::MAX => Err(io::Error::last_os_error()),
            1 => Ok(()),
            0 => Err(io::Error::other(format!(
                "thread {thread_id} of process {pid} was not suspended"
            ))),
            count => Err(io::Error::other(format!(
                "thread {thread_id} of process {pid} was suspended {count} times, so it is still suspended"
            ))),
        }
    }
}

/// A job object loadout created and owns. Its only limit is kill-on-close: when the last
/// handle closes (this is dropped, or the owning process dies), every process in it is
/// terminated. Breakaway is not allowed, so a child of a member can't leave it. Processes a
/// member places in jobs of their own (U7 queries) are in nested jobs inside this one, and
/// ending this job ends them.
#[derive(Debug)]
pub struct Job {
    handle: Owned<HANDLE>,
}

impl Job {
    /// Creates an unnamed job with kill-on-close set.
    ///
    /// # Errors
    ///
    /// The job can't be created, or its limit can't be set (then the new job is closed).
    pub fn new() -> io::Result<Job> {
        // SAFETY: CreateJobObjectW reads no pointer here (default security, no name) and only
        // creates a new handle.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }?;
        // SAFETY: `handle` came from a successful call and nothing else holds it, so `Owned` may
        // close it exactly once, when it drops.
        let handle = unsafe { Owned::new(handle) };
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..JOBOBJECT_BASIC_LIMIT_INFORMATION::default()
            },
            ..JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default()
        };
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(io::Error::other)?;
        // SAFETY: `handle` is a live job handle; `info` is a correctly sized, initialized
        // JOBOBJECT_EXTENDED_LIMIT_INFORMATION that outlives the call, and `size` is its size.
        unsafe {
            SetInformationJobObject(
                *handle,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size,
            )
        }?;
        Ok(Job { handle })
    }

    /// Places a child loadout created suspended in this job, and confirms it is a member.
    ///
    /// The child must still have exactly one thread, as a process created suspended does. That
    /// check alone doesn't prove suspension: a running process with one thread can't be told
    /// apart here. The caller's own `CREATE_SUSPENDED` is what establishes that nothing the child
    /// could have started exists outside the job, and the resume that follows ([`prepare`])
    /// checks that the thread was suspended exactly once.
    ///
    /// # Errors
    ///
    /// The child has exited or already runs several threads, or the assignment fails (for
    /// example when the caller's own job forbids nesting): the child is then not in this job.
    /// Or the assignment succeeded but the membership check after it failed or said no: the
    /// child **may** be in this job. In every error case the caller must stop and reap the child
    /// itself; dropping this job also ends it if it was assigned.
    pub fn assign(&self, child: &Child) -> io::Result<()> {
        only_thread(child.id())?;
        let process = HANDLE(child.as_raw_handle());
        // SAFETY: `self.handle` is a live job handle and `process` is the child's process
        // handle, which `child` owns and keeps open for the duration of the call.
        unsafe { AssignProcessToJobObject(*self.handle, process) }?;
        if self.contains(child)? {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "process {} is not in loadout's job after it was assigned",
                child.id()
            )))
        }
    }

    /// Whether `child` is in this job (directly or in a job nested inside it).
    ///
    /// # Errors
    ///
    /// The membership query fails.
    pub fn contains(&self, child: &Child) -> io::Result<bool> {
        let mut member = BOOL(0);
        let process = HANDLE(child.as_raw_handle());
        // SAFETY: `process` is kept open by `child`, `self.handle` is a live job handle, and
        // `member` is a writable BOOL that outlives the call.
        unsafe { IsProcessInJob(process, Some(*self.handle), &raw mut member) }?;
        Ok(member.as_bool())
    }

    /// Terminates every process in the job, nested jobs included.
    ///
    /// # Errors
    ///
    /// `TerminateJobObject` fails. Its result is never discarded.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: `self.handle` is a live job handle, opened with all access by CreateJobObjectW.
        unsafe { TerminateJobObject(*self.handle, exit_code) }?;
        Ok(())
    }

    /// How many processes are in the job now, nested jobs included. Zero is the confirmation
    /// that nothing loadout started through this job is still running.
    ///
    /// # Errors
    ///
    /// The accounting query fails.
    pub fn active_processes(&self) -> io::Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        let size = u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
            .map_err(io::Error::other)?;
        // SAFETY: `self.handle` is a live job handle; `info` is a writable, correctly sized
        // JOBOBJECT_BASIC_ACCOUNTING_INFORMATION that outlives the call, and `size` is its size.
        unsafe {
            QueryInformationJobObject(
                Some(*self.handle),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size,
                None,
            )
        }?;
        Ok(info.ActiveProcesses)
    }
}

/// Whether the current process is in any job.
///
/// # Errors
///
/// The membership query fails.
pub fn current_process_in_job() -> io::Result<bool> {
    let mut member = BOOL(0);
    // SAFETY: GetCurrentProcess takes nothing and returns a pseudo-handle that needs no closing.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: `process` is the current process's pseudo-handle, valid for the whole call; no
    // job handle is given (any job); `member` is a writable BOOL that outlives the call.
    unsafe { IsProcessInJob(process, None, &raw mut member) }?;
    Ok(member.as_bool())
}

/// 32 bytes from the system's preferred random number generator: the worker handshake's nonce.
/// The size is fixed, so the generated wrapper's length conversion can't fail.
///
/// # Errors
///
/// `BCryptGenRandom` returns a failure status.
pub fn random_nonce() -> io::Result<[u8; 32]> {
    let mut bytes = [0_u8; 32];
    // SAFETY: `bytes` is a writable 32-byte array for the duration of the call, and no algorithm
    // handle with BCRYPT_USE_SYSTEM_PREFERRED_RNG is the documented use of the system RNG.
    let status = unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status.0 >= 0 {
        Ok(bytes)
    } else {
        Err(io::Error::other(format!(
            "BCryptGenRandom failed with status {:#010x}",
            status.0
        )))
    }
}

/// Whether this process's standard input is a pipe (`FILE_TYPE_PIPE`), as a supervisor's
/// control pipe is. A console, a regular file, or the `NUL` device is not.
///
/// The handle is only borrowed from std's standard input for one `GetFileType` call; it is
/// never closed or duplicated.
///
/// # Errors
///
/// The process has no standard input handle (null or `INVALID_HANDLE_VALUE`), or `GetFileType`
/// fails (`FILE_TYPE_UNKNOWN` with an error set).
pub fn stdin_is_pipe() -> io::Result<bool> {
    let raw = io::stdin().as_raw_handle();
    if raw.is_null() || raw == INVALID_HANDLE_VALUE.0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "this process has no standard input handle",
        ));
    }
    // SAFETY: `raw` is this process's standard input handle, checked non-null and not
    // INVALID_HANDLE_VALUE. It is borrowed for this call only and not closed; GetFileType reads
    // no pointer.
    let kind = unsafe { GetFileType(HANDLE(raw)) };
    if kind == FILE_TYPE_UNKNOWN {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(0) {
            return Err(error);
        }
    }
    Ok(kind == FILE_TYPE_PIPE)
}

/// The id of the single thread the process owns, from one thread snapshot. Zero threads (the
/// process exited) or several (it already ran) are errors, as is any snapshot error other than
/// the end of the list.
fn only_thread(pid: u32) -> io::Result<u32> {
    // SAFETY: CreateToolhelp32Snapshot takes no pointers and only creates a new handle.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }?;
    // SAFETY: `snapshot` came from a successful call and nothing else holds it, so `Owned` may
    // close it exactly once, when it drops.
    let snapshot = unsafe { Owned::new(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(size_of::<THREADENTRY32>()).map_err(io::Error::other)?,
        ..THREADENTRY32::default()
    };
    let mut threads = Vec::new();
    // SAFETY: `snapshot` is a live thread snapshot, and `entry` is a correctly sized, writable
    // THREADENTRY32 that outlives the call.
    let mut step = unsafe { Thread32First(*snapshot, &raw mut entry) };
    loop {
        match step {
            Ok(()) => {
                if entry.th32OwnerProcessID == pid {
                    threads.push(entry.th32ThreadID);
                }
            }
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => break,
            Err(error) => return Err(error.into()),
        }
        // SAFETY: as for Thread32First: the same live snapshot and the same writable entry.
        step = unsafe { Thread32Next(*snapshot, &raw mut entry) };
    }
    match threads.as_slice() {
        [only] => Ok(*only),
        threads => Err(io::Error::other(format!(
            "process {pid} has {} threads, not the one suspended thread of a new process",
            threads.len()
        ))),
    }
}

/// Opens the thread for resuming and checks it belongs to `pid`. A thread id can be reused
/// after its thread exits, so the owner check rules out resuming another process's thread.
fn open_thread(thread_id: u32, pid: u32) -> io::Result<Owned<HANDLE>> {
    // SAFETY: OpenThread takes no pointers and only creates a new handle.
    let thread = unsafe {
        OpenThread(
            THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
            false,
            thread_id,
        )
    }?;
    // SAFETY: `thread` came from a successful call and nothing else holds it, so `Owned` may
    // close it exactly once, when it drops.
    let thread = unsafe { Owned::new(thread) };
    // SAFETY: `thread` is a live handle opened with THREAD_QUERY_LIMITED_INFORMATION, which
    // GetProcessIdOfThread requires, and it stays open for the call.
    let owner = unsafe { GetProcessIdOfThread(*thread) };
    match owner {
        0 => Err(io::Error::last_os_error()),
        owner if owner == pid => Ok(thread),
        owner => Err(io::Error::other(format!(
            "thread {thread_id} belongs to process {owner}, not {pid}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, ExitStatus, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    /// A child the test started. Every fixture is one process with no descendants: `cmd /c exit`
    /// runs a built-in, and `ping` is started directly. However the test ends, the child is
    /// killed and reaped within a bound, and a child that can't be confirmed gone fails the test.
    /// The `Child`, and so its process handle, is kept after the reap, so its id can't be reused.
    struct Fixture {
        child: Child,
        reaped: bool,
    }

    impl Fixture {
        fn spawn(program: &str, args: &[&str], flags: u32) -> Self {
            let child = Command::new(program)
                .args(args)
                .creation_flags(flags)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            Self {
                child,
                reaped: false,
            }
        }

        /// Waits for the child to exit, within `limit`.
        fn exit_within(&mut self, limit: Duration) -> io::Result<Option<ExitStatus>> {
            let until = Instant::now() + limit;
            loop {
                if let Some(status) = self.child.try_wait()? {
                    self.reaped = true;
                    return Ok(Some(status));
                }
                if Instant::now() >= until {
                    return Ok(None);
                }
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn stop(&mut self) -> Result<(), String> {
            if self.reaped {
                return Ok(());
            }
            let id = self.child.id();
            let killed = self.child.kill();
            match self.exit_within(Duration::from_secs(10)) {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err(format!(
                    "fixture {id} did not exit within 10 seconds of being killed ({killed:?})"
                )),
                Err(error) => Err(format!("cannot reap fixture {id}: {error}")),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(detail) = self.stop() {
                if thread::panicking() {
                    eprintln!("{detail}");
                } else {
                    panic!("{detail}");
                }
            }
        }
    }

    const EXIT: Duration = Duration::from_secs(20);

    fn resume(child: &Child) -> io::Result<()> {
        prepare(child)?.resume_before(Instant::now() + Duration::from_secs(60))
    }

    #[test]
    fn a_job_holds_a_suspended_child_and_ends_it_when_terminated() {
        let mut fixture =
            Fixture::spawn("ping.exe", &["-n", "30", "127.0.0.1"], CREATE_SUSPENDED.0);
        let job = Job::new().unwrap();
        job.assign(&fixture.child).unwrap();
        assert!(job.contains(&fixture.child).unwrap());
        resume(&fixture.child).unwrap();
        // At least the child: a console host the system starts for it may be counted too.
        assert!(job.active_processes().unwrap() >= 1);
        job.terminate(1).unwrap();
        assert!(fixture.exit_within(EXIT).unwrap().is_some());
        let until = Instant::now() + EXIT;
        while job.active_processes().unwrap() != 0 {
            assert!(Instant::now() < until, "the job still has processes");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kill-on-close: dropping the last handle ends a running member.
    #[test]
    fn dropping_the_job_ends_its_processes() {
        let mut fixture =
            Fixture::spawn("ping.exe", &["-n", "30", "127.0.0.1"], CREATE_SUSPENDED.0);
        let job = Job::new().unwrap();
        job.assign(&fixture.child).unwrap();
        resume(&fixture.child).unwrap();
        assert!(fixture.child.try_wait().unwrap().is_none());
        drop(job);
        assert!(fixture.exit_within(EXIT).unwrap().is_some());
    }

    #[test]
    fn a_child_that_has_exited_is_not_assigned() {
        let mut fixture = Fixture::spawn("cmd.exe", &["/d", "/c", "exit 0"], CREATE_SUSPENDED.0);
        resume(&fixture.child).unwrap();
        assert!(fixture.exit_within(EXIT).unwrap().is_some());
        let job = Job::new().unwrap();
        assert!(job.assign(&fixture.child).is_err());
        assert_eq!(job.active_processes().unwrap(), 0);
    }

    #[test]
    fn job_membership_of_this_process_can_be_asked() {
        current_process_in_job().unwrap();
    }

    #[test]
    fn nonces_differ_between_calls() {
        assert_ne!(random_nonce().unwrap(), random_nonce().unwrap());
    }

    /// The test harness's own standard input can be a console, a file, a pipe, or absent, so
    /// only that the call answers without closing anything is checked here; the refusals and
    /// the pipe positive run through real worker processes in `tests/background.rs`.
    #[test]
    fn the_stdin_type_can_be_asked_twice() {
        let first = stdin_is_pipe().map_err(|error| error.kind());
        assert_eq!(first, stdin_is_pipe().map_err(|error| error.kind()));
    }

    #[test]
    fn a_child_created_suspended_is_resumed_and_runs() {
        let mut fixture = Fixture::spawn("cmd.exe", &["/d", "/c", "exit 7"], CREATE_SUSPENDED.0);
        resume(&fixture.child).unwrap();
        let status = fixture.exit_within(EXIT).unwrap();
        assert_eq!(status.and_then(|status| status.code()), Some(7));
    }

    /// Whether the running child has one thread (whose suspend count is 0) or several, it is
    /// refused, and it keeps running.
    #[test]
    fn a_child_that_is_already_running_is_refused() {
        let mut fixture = Fixture::spawn("ping.exe", &["-n", "30", "127.0.0.1"], 0);
        let refused = resume(&fixture.child);
        let still_running = fixture.child.try_wait().unwrap().is_none();
        fixture.stop().unwrap();
        assert!(refused.is_err(), "{refused:?}");
        assert!(still_running);
    }

    #[test]
    fn a_child_that_has_exited_is_refused() {
        let mut fixture = Fixture::spawn("cmd.exe", &["/d", "/c", "exit 0"], CREATE_SUSPENDED.0);
        resume(&fixture.child).unwrap();
        assert!(fixture.exit_within(EXIT).unwrap().is_some());
        let error = resume(&fixture.child).unwrap_err();
        assert!(error.to_string().contains("has 0 threads"), "{error}");
    }

    /// A deadline that passed after preparation stops the resume: `ResumeThread` isn't called.
    /// The proof is that the thread is still suspended exactly once afterwards, so a second,
    /// in-time resume finds a previous count of 1 and the child then runs to its own exit.
    #[test]
    fn a_resume_whose_deadline_has_passed_is_not_issued() {
        let mut fixture = Fixture::spawn("cmd.exe", &["/d", "/c", "exit 7"], CREATE_SUSPENDED.0);
        let prepared = prepare(&fixture.child).unwrap();
        let error = prepared.resume_before(Instant::now()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(fixture.child.try_wait().unwrap().is_none());
        resume(&fixture.child).unwrap();
        let status = fixture.exit_within(EXIT).unwrap();
        assert_eq!(status.and_then(|status| status.code()), Some(7));
    }
}
