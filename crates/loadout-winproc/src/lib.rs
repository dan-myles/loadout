//! The one Windows operation loadout needs that neither std nor `process-wrap`'s safe API
//! offers: resume a child it created with `CREATE_SUSPENDED`, after the child was placed in a job
//! object. This crate is loadout's only `unsafe` code. It owns every handle it opens (RAII
//! through `windows::core::Owned`), never takes a caller's word that the child is suspended,
//! and resumes nothing until it has found exactly one thread of that child and opened it.
#![cfg(windows)]

use std::io;
use std::process::Child;

pub use windows::Win32::System::Threading::CREATE_SUSPENDED;
use windows::Win32::Foundation::{ERROR_NO_MORE_FILES, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Threading::{
    GetProcessIdOfThread, OpenThread, ResumeThread, THREAD_QUERY_LIMITED_INFORMATION,
    THREAD_SUSPEND_RESUME,
};
use windows::core::{HRESULT, Owned};

/// Resumes the child's only thread, which must have been suspended exactly once.
///
/// Everything fallible happens before the resume: listing the child's threads (there must be
/// exactly one, as in a process created suspended), opening that thread, and checking that it
/// still belongs to the child. The resume's own answer, the thread's previous suspend count, is
/// checked last: 1 means it was suspended once and now runs. 0 means it wasn't suspended
/// (already running), and more than 1 means it is still suspended. Both are errors.
///
/// Any error after `ResumeThread` was called may leave the child running, so the caller must
/// treat every error from this function as "the child may have run" and settle its whole job.
pub fn resume_suspended(child: &Child) -> io::Result<()> {
    let pid = child.id();
    let thread_id = only_thread(pid)?;
    let thread = open_thread(thread_id, pid)?;
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
    use std::process::{Command, Stdio};

    use super::*;

    fn cmd(script: &str, flags: u32) -> Child {
        Command::new("cmd.exe")
            .args(["/d", "/c", script])
            .creation_flags(flags)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_child_created_suspended_is_resumed_and_runs() {
        let mut child = cmd("exit 7", CREATE_SUSPENDED.0);
        resume_suspended(&child).unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }

    /// Whether the running child has one thread (whose suspend count is 0) or several, it is
    /// refused, and it keeps running.
    #[test]
    fn a_child_that_is_already_running_is_refused() {
        let mut child = cmd("ping -n 30 127.0.0.1 >nul", 0);
        let refused = resume_suspended(&child);
        let still_running = child.try_wait().unwrap().is_none();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(refused.is_err(), "{refused:?}");
        assert!(still_running);
    }

    #[test]
    fn a_child_that_has_exited_is_refused() {
        let mut child = cmd("exit 0", CREATE_SUSPENDED.0);
        resume_suspended(&child).unwrap();
        child.wait().unwrap();
        let error = resume_suspended(&child).unwrap_err();
        assert!(error.to_string().contains("has 0 threads"), "{error}");
    }
}
