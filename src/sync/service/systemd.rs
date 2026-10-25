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

fn service_unit(exe: &str, service: &str) -> String {
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

fn timer_unit(interval_minutes: u32) -> String {
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

/// Disables the timer and removes both units. Every step must succeed before the files go: a
/// failure (no user session included) leaves the files, and `service.toml`, in place.
///
/// With neither unit file there, systemd is still asked whether it has the timer loaded (from
/// an install whose files were removed by hand): `not-found`, or no user manager to ask, is
/// nothing to remove; a loaded timer is stopped and systemd reloaded.
pub(super) fn uninstall(context: &Context) -> Result<(), Error> {
    let exists = |path: &PathBuf| std::fs::symlink_metadata(path).is_ok();
    let [service, timer] = [dir(context).join(SERVICE), dir(context).join(TIMER)];
    if !exists(&service) && !exists(&timer) {
        let args = ["--user", "show", "--property=LoadState", "--value", TIMER];
        let shown = query(context, "systemctl", &args)?;
        if !shown.success() {
            if shown.stderr.contains("Failed to connect") {
                return Ok(());
            }
            return Err(failed("systemctl", &args, &shown));
        }
        if shown.stdout.trim() == "not-found" {
            return Ok(());
        }
        required(context, "systemctl", &["--user", "stop", TIMER])?;
        required(context, "systemctl", &["--user", "daemon-reload"])?;
        return Ok(());
    }
    if exists(&timer) {
        let args = ["--user", "disable", "--now", TIMER];
        let disabled = query(context, "systemctl", &args)?;
        if !disabled.success() {
            if disabled.stderr.contains("Failed to connect") {
                return Err(Error::NoUserSession(disabled.stderr.trim().to_owned()));
            }
            return Err(failed("systemctl", &args, &disabled));
        }
    }
    for path in [&service, &timer] {
        remove(path)?;
    }
    required(context, "systemctl", &["--user", "daemon-reload"])?;
    Ok(())
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
