//! Test-only ownership of a child process a test started itself.
//!
//! [`Owned`] takes the child in the same expression that spawns it, before any fallible
//! readiness check, thread, observation or assertion. However the test then ends (a return, an
//! error, a timeout or a panic), the guard stops the child if it still owns it: a checked kill
//! unless it has positively exited, then a bounded poll for its exit. A stop that can't be
//! confirmed is reported, never assumed, and the first error is never replaced by a cleanup
//! error.
//!
//! What "still owns it" means differs by platform:
//!
//! - Windows: the `Child` keeps its process handle open, which pins the process. An
//!   observation that fails leaves that identity intact, so the kill and the reap still follow.
//! - Unix: a `Child` pins its process only while it is unreaped. A `try_wait` that fails (for
//!   example with `ECHILD`) may mean it was reaped without this `Child` recording its status,
//!   and a later kill may then address a reused PID (as on macOS, or wherever the standard
//!   library signals by number; some Linux builds use a pidfd instead, which this guard does not
//!   rely on). So a real observation or kill error on Unix latches the identity as lost: the
//!   error is kept, cleanup is reported unconfirmed, and the guard makes no further call on
//!   the child at all, no poll and no kill, in an explicit stop, a later retry, or `Drop`.
//!
//! The latch is state of its own. Synthetic faults (`Faults`) that stand in for the guard's own
//! bookkeeping failing, before any OS call, leave the child pinned and are cleared by `Drop`;
//! the latch never is.
//!
//! This is direct, single-process cleanup: it says nothing of processes the child started.
//! Whole-group and whole-job cleanup is the supervisor's, and is checked separately. A watchdog
//! or a fixture's own lifetime is never counted as this cleanup.

use std::io;
use std::process::{Child, ExitStatus};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// How long `Drop` gives a stop that the test didn't confirm itself.
pub(crate) const DROP_LIMIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(20);

/// Faults a control injects into this guard's operations, to check its failure paths. None of
/// them makes an OS call.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Faults {
    /// How many of the next observations fail before the OS is asked: the guard's own
    /// bookkeeping failing. The child is not polled, so it stays pinned on every platform.
    /// `Drop` clears this.
    pub(crate) before_poll: u32,
    /// How many of the next observations get an error in place of the OS's answer, judged as a
    /// real one would be: on Unix it latches the identity as lost (a safe stand-in for `ECHILD`
    /// that needs no reaper and no PID reuse); on Windows the retained handle still pins the
    /// process. `Drop` clears what is left of this, never a latch it caused.
    pub(crate) os_error: u32,
    /// Whether kills fail before the OS is asked, leaving the child running and pinned. `Drop`
    /// clears this.
    pub(crate) kill: bool,
}

/// Every call this guard made on its child, kept outside the guard so a control can check it
/// after the guard is gone.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Record {
    /// The exit status, once confirmed.
    pub(crate) exit: Option<ExitStatus>,
    /// `try_wait` calls that reached the OS.
    pub(crate) polls: u32,
    /// Kill attempts, whether the OS call ran or a synthetic fault stood in for it.
    pub(crate) kills: u32,
}

/// Whether the guard can still act on the child.
#[derive(Debug)]
enum Identity {
    /// Pinned: polls and kills address this child.
    Pinned,
    /// Unix only: a real error means the child may have been reaped elsewhere. Never cleared.
    /// `reported` is set once an explicit call returned the loss, so `Drop` doesn't fail a
    /// test twice for it.
    #[cfg_attr(not(unix), allow(dead_code))]
    Lost { why: String, reported: bool },
}

/// A child this test started, owned from the moment it exists.
pub(crate) struct Owned {
    child: Mutex<Child>,
    identity: Identity,
    exited: Option<ExitStatus>,
    record: Option<Arc<Mutex<Record>>>,
    faults: Faults,
    polls: u32,
    kills: u32,
}

impl Owned {
    pub(crate) fn new(child: Child) -> Owned {
        Owned {
            child: Mutex::new(child),
            identity: Identity::Pinned,
            exited: None,
            record: None,
            faults: Faults::default(),
            polls: 0,
            kills: 0,
        }
    }

    /// Also keeps `record` up to date, so a control can check every call after the guard
    /// itself is gone.
    pub(crate) fn recording(mut self, record: &Arc<Mutex<Record>>) -> Owned {
        self.record = Some(Arc::clone(record));
        self.publish();
        self
    }

    pub(crate) fn inject(&mut self, faults: Faults) {
        self.faults = faults;
    }

    fn os(&mut self) -> &mut Child {
        self.child.get_mut().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lends the child to Windows code under test, which keeps its own retained-handle policy.
    #[cfg(windows)]
    pub(crate) fn child(&mut self) -> &mut Child {
        self.os()
    }

    /// The child, for Windows code under test that takes it behind a mutex. The guard keeps
    /// owning it.
    #[cfg(windows)]
    pub(crate) fn shared(&self) -> &Mutex<Child> {
        &self.child
    }

    /// How many kills this guard attempted.
    pub(crate) fn kills(&self) -> u32 {
        self.kills
    }

    /// Whether a real error (or its stand-in) latched the child's identity as lost.
    pub(crate) fn lost(&self) -> bool {
        matches!(self.identity, Identity::Lost { .. })
    }

    fn publish(&self) {
        if let Some(record) = &self.record {
            let mut record = record.lock().unwrap_or_else(PoisonError::into_inner);
            record.exit = self.exited;
            record.polls = self.polls;
            record.kills = self.kills;
        }
    }

    /// Latches the loss (Unix only) and returns the error to report.
    #[cfg(unix)]
    fn lose(&mut self, why: String) -> io::Error {
        if !self.lost() {
            self.identity = Identity::Lost {
                why: why.clone(),
                reported: false,
            };
        }
        io::Error::other(why)
    }

    /// One observation: `Some` once the child has positively exited. After a loss, every call
    /// returns that loss without touching the child.
    pub(crate) fn observe(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.exited {
            return Ok(Some(status));
        }
        if let Identity::Lost { why, .. } = &self.identity {
            return Err(io::Error::other(why.clone()));
        }
        if self.faults.before_poll > 0 {
            self.faults.before_poll -= 1;
            return Err(io::Error::other(
                "injected: the guard failed before polling the child",
            ));
        }
        let answer = if self.faults.os_error > 0 {
            self.faults.os_error -= 1;
            Err(io::Error::other(
                "injected: the OS could not report on the child",
            ))
        } else {
            self.polls += 1;
            let answer = self.os().try_wait();
            self.publish();
            answer
        };
        match answer {
            Ok(Some(status)) => {
                self.exited = Some(status);
                self.publish();
                Ok(Some(status))
            }
            Ok(None) => Ok(None),
            #[cfg(unix)]
            Err(error) => Err(self.lose(format!(
                "{error}; the child may have been reaped elsewhere, so its identity is lost"
            ))),
            #[cfg(windows)]
            Err(error) => Err(error),
        }
    }

    /// Waits for the child to exit within `limit`: `Ok(None)` if it still runs then. It stays
    /// owned either way.
    pub(crate) fn wait_within(&mut self, limit: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.observe()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            thread::sleep(POLL);
        }
    }

    /// Stops the child within `limit`: nothing to do once it has positively exited; nothing at
    /// all once its identity is lost; otherwise a kill, even after an observation error that
    /// left it pinned, then a poll for its exit until `limit`. Every problem is kept, in order:
    /// the observation, the kill, the reap, and a lost identity always reports the cleanup
    /// unconfirmed.
    pub(crate) fn stop(&mut self, limit: Duration) -> Result<(), String> {
        let result = self.stopping(limit);
        if let Identity::Lost { reported, .. } = &mut self.identity {
            *reported = true;
        }
        result
    }

    fn stopping(&mut self, limit: Duration) -> Result<(), String> {
        let deadline = Instant::now() + limit;
        let mut problems = Vec::new();
        match self.observe() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => problems.push(format!("cannot check the child: {error}")),
        }
        if self.lost() {
            return Err(unconfirmed(problems));
        }
        self.kills += 1;
        self.publish();
        let killed = if self.faults.kill {
            Err(io::Error::other("injected: the child could not be killed"))
        } else {
            match self.os().kill() {
                Ok(()) => Ok(()),
                #[cfg(unix)]
                Err(error) => Err(self.lose(format!(
                    "{error}; the child may have been reaped elsewhere, so its identity is lost"
                ))),
                #[cfg(windows)]
                Err(error) => Err(error),
            }
        };
        if let Err(error) = killed {
            problems.push(format!("cannot kill the child: {error}"));
        }
        if self.lost() {
            return Err(unconfirmed(problems));
        }
        let mut reap = None;
        loop {
            match self.observe() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => {
                    reap.get_or_insert_with(|| format!("cannot reap the child: {error}"));
                    if self.lost() {
                        break;
                    }
                }
            }
            if Instant::now() >= deadline {
                if self.exited.is_none() {
                    reap.get_or_insert_with(|| {
                        format!("the child has not exited within {limit:?}")
                    });
                }
                break;
            }
            thread::sleep(POLL);
        }
        problems.extend(reap);
        if self.lost() {
            Err(unconfirmed(problems))
        } else if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Waits for the child within `limit`. If it doesn't exit in time, or can't be observed, it
    /// is stopped within `cleanup` (or, with its identity lost, left alone), and the error keeps
    /// that first problem and any cleanup error.
    pub(crate) fn finish_within(
        &mut self,
        limit: Duration,
        cleanup: Duration,
    ) -> Result<ExitStatus, String> {
        let primary = match self.wait_within(limit) {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => format!("the child did not exit within {limit:?}"),
            Err(error) => format!("cannot observe the child: {error}"),
        };
        match self.stop(cleanup) {
            Ok(()) => Err(format!("{primary}; it was stopped")),
            Err(detail) => Err(format!(
                "{primary}; it could not be confirmed stopped: {detail}"
            )),
        }
    }
}

fn unconfirmed(mut problems: Vec<String>) -> String {
    problems.push("its identity is lost, so it is left alone; cleanup unconfirmed".to_owned());
    problems.join("; ")
}

impl Drop for Owned {
    fn drop(&mut self) {
        self.faults.before_poll = 0;
        self.faults.os_error = 0;
        self.faults.kill = false;
        let already = matches!(self.identity, Identity::Lost { reported: true, .. });
        if let Err(detail) = self.stopping(DROP_LIMIT) {
            if already || thread::panicking() {
                eprintln!("{detail}");
            } else {
                panic!("{detail}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::process::Command;

    use super::*;

    #[cfg(unix)]
    fn sleeper() -> Command {
        let mut command = Command::new("sleep");
        command.arg("30");
        command
    }

    #[cfg(windows)]
    fn sleeper() -> Command {
        let mut command = Command::new("ping.exe");
        command.args(["-n", "30", "127.0.0.1"]);
        command
    }

    /// A child that ends by itself within about a second, for a control that leaves it alone.
    #[cfg(unix)]
    fn brief() -> Command {
        let mut command = Command::new("sleep");
        command.arg("1");
        command
    }

    #[cfg(unix)]
    fn quick() -> Command {
        Command::new("true")
    }

    #[cfg(windows)]
    fn quick() -> Command {
        let mut command = Command::new("cmd.exe");
        command.args(["/c", "exit 0"]);
        command
    }

    fn recorded(record: &Arc<Mutex<Record>>) -> Record {
        *record.lock().unwrap()
    }

    #[test]
    fn an_error_right_after_spawn_still_stops_the_child() {
        let record = Arc::new(Mutex::new(Record::default()));
        let failed = catch_unwind(AssertUnwindSafe(|| {
            let _owned = Owned::new(sleeper().spawn().unwrap()).recording(&record);
            panic!("injected: failure right after spawn");
        }));
        assert!(failed.is_err());
        let record = recorded(&record);
        assert!(
            record.exit.is_some(),
            "the child must have been killed and its exit confirmed"
        );
        assert_eq!(record.kills, 1);
    }

    /// The ordinary positive: a child that is definitely owned and live is killed and reaped.
    #[test]
    fn a_live_owned_child_is_killed_and_reaped() {
        let record = Arc::new(Mutex::new(Record::default()));
        let mut owned = Owned::new(sleeper().spawn().unwrap()).recording(&record);
        assert!(owned.observe().unwrap().is_none());
        assert_eq!(owned.stop(Duration::from_secs(10)), Ok(()));
        drop(owned);
        let record = recorded(&record);
        assert!(record.exit.is_some());
        assert_eq!(record.kills, 1, "one kill, and none again from Drop");
    }

    /// The guard's own bookkeeping failing before any OS poll leaves the child pinned, on every
    /// platform, so the kill still follows. This is not an OS error; see the controls below for
    /// those.
    #[test]
    fn a_failure_before_the_poll_still_kills_the_pinned_child() {
        let mut owned = Owned::new(sleeper().spawn().unwrap());
        owned.inject(Faults {
            before_poll: 1,
            ..Faults::default()
        });
        let detail = owned.stop(Duration::from_secs(10)).unwrap_err();
        assert!(detail.contains("before polling"), "{detail}");
        assert!(!detail.contains("has not exited"), "{detail}");
        assert!(!owned.lost());
        assert_eq!(owned.kills(), 1);
        assert!(
            owned.observe().unwrap().is_some(),
            "the child must have exited"
        );
    }

    /// Unix: an OS observation error may mean the child was reaped elsewhere, so the guard
    /// keeps the error, reports cleanup unconfirmed, and makes no call on the child again: not
    /// in this stop, a retry, a later wait, or `Drop`. The stand-in error needs no reaper and
    /// no PID reuse. The child ends by itself within a second; nothing ever signals it, and its
    /// status is deliberately never collected.
    #[cfg(unix)]
    #[test]
    fn a_lost_identity_is_never_signalled_or_polled_again_even_by_drop() {
        let record = Arc::new(Mutex::new(Record::default()));
        let mut owned = Owned::new(brief().spawn().unwrap()).recording(&record);
        owned.inject(Faults {
            os_error: 1,
            ..Faults::default()
        });
        let started = Instant::now();
        let detail = owned.stop(Duration::from_secs(10)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "no wait follows a lost identity"
        );
        assert!(
            detail.starts_with("cannot check the child: injected: the OS"),
            "{detail}"
        );
        assert!(detail.ends_with("cleanup unconfirmed"), "{detail}");
        assert!(owned.lost());
        assert!(
            owned.stop(Duration::from_secs(10)).is_err(),
            "a retry stays refused"
        );
        assert!(owned.wait_within(Duration::from_secs(1)).is_err());
        let before_drop = recorded(&record);
        assert_eq!((before_drop.polls, before_drop.kills), (0, 0));
        drop(owned);
        let after_drop = recorded(&record);
        assert_eq!(
            (after_drop.polls, after_drop.kills),
            (0, 0),
            "Drop must not touch it"
        );
        assert!(after_drop.exit.is_none());
    }

    /// Unix: errors keep their order, and a loss found while reaping ends the stop at once
    /// with nothing further sent. Here the guard's bookkeeping fails before the first poll (the
    /// child stays pinned), the one kill fails before the OS is asked, and the next observation
    /// reports the loss. Drop then adds no call.
    #[cfg(unix)]
    #[test]
    fn errors_stay_in_order_and_a_loss_while_reaping_stops_everything() {
        let record = Arc::new(Mutex::new(Record::default()));
        let mut owned = Owned::new(brief().spawn().unwrap()).recording(&record);
        owned.inject(Faults {
            before_poll: 1,
            os_error: 1,
            kill: true,
        });
        let started = Instant::now();
        let detail = owned.stop(Duration::from_secs(10)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        let parts: Vec<&str> = detail.split("; ").collect();
        assert!(parts[0].contains("before polling"), "{detail}");
        assert!(
            parts[1].contains("cannot kill the child: injected"),
            "{detail}"
        );
        assert!(
            parts[2].starts_with("cannot reap the child: injected: the OS"),
            "{detail}"
        );
        assert!(detail.ends_with("cleanup unconfirmed"), "{detail}");
        drop(owned);
        let record = recorded(&record);
        assert_eq!(
            (record.polls, record.kills),
            (0, 1),
            "only the one synthetic kill"
        );
    }

    /// Windows: the retained handle still pins the process after an OS observation error, so
    /// the guard kills and reaps it, keeping that error first.
    #[cfg(windows)]
    #[test]
    fn a_retained_handle_is_still_killed_and_reaped_after_an_os_error() {
        let mut owned = Owned::new(sleeper().spawn().unwrap());
        owned.inject(Faults {
            os_error: 1,
            ..Faults::default()
        });
        let detail = owned.stop(Duration::from_secs(10)).unwrap_err();
        assert!(
            detail.starts_with("cannot check the child: injected: the OS"),
            "{detail}"
        );
        assert!(!detail.contains("has not exited"), "{detail}");
        assert!(!owned.lost());
        assert_eq!(owned.kills(), 1);
        assert!(
            owned.observe().unwrap().is_some(),
            "the child must have exited"
        );
    }

    #[test]
    fn a_wait_whose_observation_fails_stops_the_child_and_keeps_that_error() {
        let mut owned = Owned::new(sleeper().spawn().unwrap());
        owned.inject(Faults {
            before_poll: 1,
            ..Faults::default()
        });
        let detail = owned
            .finish_within(Duration::from_secs(5), Duration::from_secs(10))
            .unwrap_err();
        assert!(
            detail.contains("cannot observe the child: injected"),
            "{detail}"
        );
        assert!(detail.ends_with("it was stopped"), "{detail}");
        assert!(owned.observe().unwrap().is_some());
    }

    /// A kill that fails leaves the child running: the stop gives up at its limit with every
    /// problem kept, and the same guard can still stop the child for real afterwards.
    #[test]
    fn a_failed_kill_is_reported_within_the_limit_and_the_child_stays_recoverable() {
        let mut owned = Owned::new(sleeper().spawn().unwrap());
        owned.inject(Faults {
            kill: true,
            ..Faults::default()
        });
        let started = Instant::now();
        let detail = owned
            .finish_within(Duration::from_millis(200), Duration::from_secs(1))
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(detail.contains("did not exit within"), "{detail}");
        assert!(
            detail.contains("injected: the child could not be killed"),
            "{detail}"
        );
        assert!(detail.contains("has not exited"), "{detail}");
        assert!(owned.observe().unwrap().is_none(), "the child still runs");
        owned.inject(Faults::default());
        assert_eq!(owned.stop(Duration::from_secs(10)), Ok(()));
        assert!(owned.observe().unwrap().is_some());
    }

    #[test]
    fn a_child_that_already_exited_is_not_killed() {
        let record = Arc::new(Mutex::new(Record::default()));
        let mut owned = Owned::new(quick().spawn().unwrap()).recording(&record);
        assert!(
            owned
                .wait_within(Duration::from_secs(20))
                .unwrap()
                .is_some()
        );
        assert_eq!(owned.stop(Duration::from_secs(10)), Ok(()));
        drop(owned);
        assert_eq!(recorded(&record).kills, 0);
    }
}
