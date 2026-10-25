//! Linux: a systemd user service and timer in `{config}/systemd/user`.
//!
//! The service is `Type=exec`, because systemd ignores `RuntimeMaxSec=` for `Type=oneshot`; that
//! limit and the control-group kill are only a backstop behind loadout's own supervisor. The
//! timer counts the interval from the end of each run, so runs never overlap.

use std::path::PathBuf;

use crate::context::Context;

use super::{Error, Scheduler, failed, query, remove, required, write};

const SERVICE: &str = "loadout-update.service";
const TIMER: &str = "loadout-update.timer";
/// The supervisor's own hard limit is 60 minutes plus a 30 second settle; this backstop only
/// fires if the supervisor itself hangs.
const BACKSTOP_SECONDS: u32 = 3720;

fn dir(context: &Context) -> PathBuf {
    context.config.join("systemd").join("user")
}

pub(super) fn files(context: &Context) -> Vec<PathBuf> {
    vec![dir(context).join(SERVICE), dir(context).join(TIMER)]
}

/// One `ExecStart=` argument: double-quoted, with backslash and quote escaped, `%` doubled so
/// systemd doesn't read a specifier, and `$` doubled so it doesn't expand a variable.
fn quote(arg: &str) -> String {
    let mut out = String::from("\"");
    for character in arg.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

pub(super) fn service_unit(exe: &str, service: &str) -> String {
    let command = [exe, "update", "--background", "--service", service]
        .iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description=loadout background update\n\
         Documentation=https://github.com/dan-myles/loadout\n\
         \n\
         [Service]\n\
         Type=exec\n\
         ExecStart={command}\n\
         RuntimeMaxSec={BACKSTOP_SECONDS}\n\
         KillMode=control-group\n\
         TimeoutStopSec=30\n\
         NoNewPrivileges=yes\n"
    )
}

pub(super) fn timer_unit(interval_minutes: u32) -> String {
    format!(
        "[Unit]\n\
         Description=Run the loadout background update every {interval_minutes} minutes\n\
         \n\
         [Timer]\n\
         OnBootSec=5min\n\
         OnUnitInactiveSec={interval_minutes}min\n\
         RandomizedDelaySec=60\n\
         Unit={SERVICE}\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

/// A systemd user manager this session can talk to. Any state but `offline` (or no answer)
/// means one is running.
pub(super) fn session(context: &Context) -> Result<(), Error> {
    let answer = query(context, "systemctl", &["--user", "is-system-running"])
        .map_err(|error| Error::NoUserSession(error.to_string()))?;
    let state = answer.stdout.trim();
    match state {
        "running" | "degraded" | "starting" | "initializing" | "maintenance" => Ok(()),
        "" => Err(Error::NoUserSession(answer.stderr.trim().to_owned())),
        other => Err(Error::NoUserSession(format!(
            "systemctl --user reports {other}"
        ))),
    }
}

pub(super) fn install(
    context: &Context,
    exe: &str,
    service: &str,
    interval_minutes: u32,
) -> Result<(), Error> {
    let dir = dir(context);
    write(&dir.join(SERVICE), service_unit(exe, service).as_bytes())?;
    write(&dir.join(TIMER), timer_unit(interval_minutes).as_bytes())?;
    required(context, "systemctl", &["--user", "daemon-reload"])?;
    required(context, "systemctl", &["--user", "enable", "--now", TIMER])?;
    required(context, "systemctl", &["--user", "restart", TIMER])?;
    Ok(())
}

/// Stops the timer and removes both units. systemd is asked first whether it has the timer
/// loaded, whichever unit files are still there (one may have been removed by hand): a timer
/// file there is disabled and stopped, a timer loaded without its file is stopped, and only
/// `not-found` is nothing to stop. A user manager that can't be reached is a failure, never
/// taken as absence, so `service.toml` stays. A failure stops the removal there: units already
/// removed stay removed, the rest stay in place.
pub(super) fn uninstall(context: &Context) -> Result<(), Error> {
    let exists = |path: &PathBuf| std::fs::symlink_metadata(path).is_ok();
    let [service, timer] = [dir(context).join(SERVICE), dir(context).join(TIMER)];
    let args = ["--user", "show", "--property=LoadState", "--value", TIMER];
    let shown = manager(context, &args)?;
    let loaded = shown.stdout.trim() != "not-found";
    if exists(&timer) {
        manager(context, &["--user", "disable", "--now", TIMER])?;
    } else if loaded {
        manager(context, &["--user", "stop", TIMER])?;
    }
    let files = exists(&service) || exists(&timer);
    for path in [&service, &timer] {
        remove(path)?;
    }
    if files || loaded {
        manager(context, &["--user", "daemon-reload"])?;
    }
    Ok(())
}

/// A `systemctl` call that must succeed; no user manager to talk to is `NoUserSession`.
fn manager(context: &Context, args: &[&str]) -> Result<crate::process::Captured, Error> {
    let answer = query(context, "systemctl", args)?;
    if answer.success() {
        Ok(answer)
    } else if answer.stderr.contains("Failed to connect") {
        Err(Error::NoUserSession(answer.stderr.trim().to_owned()))
    } else {
        Err(failed("systemctl", args, &answer))
    }
}

/// Whether systemd keeps this user's manager running after logout, from `loginctl`; `None` when
/// it can't tell. Read-only.
pub(super) fn linger(context: &Context) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata(&context.home).ok()?.uid().to_string();
        let answer = query(
            context,
            "loginctl",
            &["show-user", uid.as_str(), "--property=Linger", "--value"],
        )
        .ok()?;
        match (answer.success(), answer.stdout.trim()) {
            (true, "yes") => Some(true),
            (true, "no") => Some(false),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = context;
        None
    }
}

/// How the unit files differ from the ones `install` would write for this record, if they do.
pub(super) fn differs(
    context: &Context,
    exe: &str,
    service: &str,
    interval_minutes: u32,
) -> Option<String> {
    let expected = [
        (SERVICE, service_unit(exe, service)),
        (TIMER, timer_unit(interval_minutes)),
    ];
    expected.into_iter().find_map(|(name, text)| {
        let actual = super::definition(&dir(context).join(name))?;
        (actual != text).then(|| super::differs_from_record(name))
    })
}

pub(super) fn registered(context: &Context) -> Scheduler {
    match query(context, "systemctl", &["--user", "is-enabled", TIMER]) {
        Ok(answer) => Scheduler {
            registered: answer.success() && answer.stdout.trim() == "enabled",
            detail: format!("{TIMER} is {}", answer.stdout.trim()),
        },
        Err(error) => Scheduler {
            registered: false,
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_runs_the_supervisor_with_every_argument_quoted() {
        let unit = service_unit(
            "/opt/my tools/loadout",
            "/home/u/.local/state/loadout/service.toml",
        );
        assert_eq!(
            unit,
            "[Unit]\n\
             Description=loadout background update\n\
             Documentation=https://github.com/dan-myles/loadout\n\
             \n\
             [Service]\n\
             Type=exec\n\
             ExecStart=\"/opt/my tools/loadout\" \"update\" \"--background\" \"--service\" \"/home/u/.local/state/loadout/service.toml\"\n\
             RuntimeMaxSec=3720\n\
             KillMode=control-group\n\
             TimeoutStopSec=30\n\
             NoNewPrivileges=yes\n"
        );
    }

    #[test]
    fn specifiers_variables_quotes_and_backslashes_are_escaped() {
        assert_eq!(
            quote(r#"/a b/%h/$HOME/"q"/\x"#),
            r#""/a b/%%h/$$HOME/\"q\"/\\x""#
        );
    }

    #[test]
    fn the_timer_counts_the_interval_from_the_end_of_each_run() {
        let timer = timer_unit(15);
        assert!(timer.contains("\nOnUnitInactiveSec=15min\n"), "{timer}");
        assert!(timer.contains("\nOnBootSec=5min\n"), "{timer}");
        assert!(timer.contains("\nUnit=loadout-update.service\n"), "{timer}");
        assert!(
            timer.ends_with("[Install]\nWantedBy=timers.target\n"),
            "{timer}"
        );
    }
}
