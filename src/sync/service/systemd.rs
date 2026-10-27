//! Linux: a systemd user service and timer in `{config}/systemd/user`.
//!
//! The service is `Type=exec`, because systemd ignores `RuntimeMaxSec=` for `Type=oneshot`; that
//! limit and the control-group kill are only a backstop behind loadout's own supervisor. The
//! timer counts the interval from the end of each run, so runs never overlap.

use std::path::{Path, PathBuf};

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
    unit(exe, service, "sync", "loadout background sync")
}

/// The service unit older versions wrote: it runs the legacy `loadout update --background`,
/// which still syncs, until `service install` writes [`service_unit`] in its place.
pub(super) fn legacy_service_unit(exe: &str, service: &str) -> String {
    unit(exe, service, "update", "loadout background update")
}

fn unit(exe: &str, service: &str, verb: &str, description: &str) -> String {
    let command = [exe, verb, "--background", "--service", service]
        .iter()
        .map(|arg| quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description={description}\n\
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
    timer(interval_minutes, "sync")
}

/// The timer older versions wrote, which differs only in its description.
pub(super) fn legacy_timer_unit(interval_minutes: u32) -> String {
    timer(interval_minutes, "update")
}

fn timer(interval_minutes: u32, what: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Run the loadout background {what} every {interval_minutes} minutes\n\
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

/// How the units differ from the ones `install` would write for this record, if they do: first
/// the files on disk, then (outside isolation) the units systemd has loaded, since matching files
/// prove nothing about a job loaded before they were written. A unit file that can't be read,
/// or a systemd that can't be asked, is an error, never a match.
pub(super) fn differs(
    context: &Context,
    exe: &str,
    service: &str,
    interval_minutes: u32,
) -> Result<Option<String>, String> {
    let expected = [
        (
            SERVICE,
            service_unit(exe, service),
            legacy_service_unit(exe, service),
        ),
        (
            TIMER,
            timer_unit(interval_minutes),
            legacy_timer_unit(interval_minutes),
        ),
    ];
    let mut present = true;
    let mut legacy = false;
    for (name, text, older) in expected {
        match super::definition(&dir(context).join(name))? {
            Some(actual) if actual == text => {}
            Some(actual) if actual == older => legacy = true,
            Some(_) => return Ok(Some(super::differs_from_record(name))),
            None => present = false,
        }
    }
    if legacy {
        return Ok(Some(super::LEGACY.to_owned()));
    }
    if !present || context.isolated {
        return Ok(None);
    }
    loaded(context)
}

/// Whether systemd runs the units on disk: each loaded from its file in loadout's unit folder,
/// with no drop-in that could change it, and not changed on disk since systemd loaded it.
fn loaded(context: &Context) -> Result<Option<String>, String> {
    let args = [
        "--user",
        "show",
        "--property=LoadState,FragmentPath,DropInPaths,NeedDaemonReload",
        SERVICE,
        TIMER,
    ];
    let answer = query(context, "systemctl", &args).map_err(|error| error.to_string())?;
    if !answer.success() {
        return Err(failed("systemctl", &args, &answer).to_string());
    }
    let units: Vec<&str> = answer.stdout.trim().split("\n\n").collect();
    let [service, timer] = units.as_slice() else {
        return Err(format!(
            "`systemctl {}` didn't describe both units: {}",
            args.join(" "),
            answer.stdout.trim()
        ));
    };
    for (name, shown) in [(SERVICE, service), (TIMER, timer)] {
        let property = |key: &str| {
            shown
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        };
        let (Some(state), Some(fragment), Some(drop_ins), Some(reload)) = (
            property("LoadState"),
            property("FragmentPath"),
            property("DropInPaths"),
            property("NeedDaemonReload"),
        ) else {
            return Err(format!(
                "`systemctl {}` didn't describe {name}",
                args.join(" ")
            ));
        };
        let file = dir(context).join(name);
        let from_file = Path::new(fragment) == file
            || std::fs::canonicalize(fragment).is_ok_and(|fragment| {
                std::fs::canonicalize(&file).is_ok_and(|file| file == fragment)
            });
        let why = if state != "loaded" {
            format!("systemd has {name} {state}")
        } else if !from_file {
            format!(
                "systemd loaded {name} from {fragment}, not {}",
                file.display()
            )
        } else if !drop_ins.is_empty() {
            format!("systemd runs {name} with drop-ins that can change it: {drop_ins}")
        } else if reload != "no" {
            format!("{name} changed on disk since systemd loaded it, so it runs the old one")
        } else {
            continue;
        };
        return Ok(Some(format!(
            "{why}; the job systemd runs may not be this record's"
        )));
    }
    Ok(None)
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
             Description=loadout background sync\n\
             Documentation=https://github.com/dan-myles/loadout\n\
             \n\
             [Service]\n\
             Type=exec\n\
             ExecStart=\"/opt/my tools/loadout\" \"sync\" \"--background\" \"--service\" \"/home/u/.local/state/loadout/service.toml\"\n\
             RuntimeMaxSec=3720\n\
             KillMode=control-group\n\
             TimeoutStopSec=30\n\
             NoNewPrivileges=yes\n"
        );
    }

    /// The unit older versions wrote, byte for byte, which `differs` recognizes as the legacy
    /// job rather than a foreign one.
    #[test]
    fn the_legacy_unit_is_the_one_older_versions_wrote() {
        assert_eq!(
            legacy_service_unit(
                "/opt/my tools/loadout",
                "/home/u/.local/state/loadout/service.toml"
            ),
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
        assert_eq!(
            legacy_timer_unit(45),
            "[Unit]\n\
             Description=Run the loadout background update every 45 minutes\n\
             \n\
             [Timer]\n\
             OnBootSec=5min\n\
             OnUnitInactiveSec=45min\n\
             RandomizedDelaySec=60\n\
             Unit=loadout-update.service\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n"
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
