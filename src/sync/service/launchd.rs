//! macOS: a launchd agent, `~/Library/LaunchAgents/dev.loadout.update.plist`, in the user's
//! `gui/<uid>` domain. launchd has no run-time limit of its own, so the supervisor's hard
//! deadline is the run's only bound. The arguments are a plist array, never a joined command
//! line; each is XML-escaped.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::context::Context;

use super::{Error, Scheduler, failed, query, remove, required, write, xml};

const LABEL: &str = "dev.loadout.update";

pub(super) fn file(context: &Context) -> PathBuf {
    context
        .home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

pub(super) fn plist(exe: &str, service: &str, interval_minutes: u32) -> String {
    let arguments = [exe, "update", "--background", "--service", service]
        .iter()
        .fold(String::new(), |mut out, arg| {
            let _ = writeln!(out, "\t\t<string>{}</string>", xml(arg));
            out
        });
    let seconds = u64::from(interval_minutes) * 60;
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{LABEL}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         {arguments}\
         \t</array>\n\
         \t<key>StartInterval</key>\n\
         \t<integer>{seconds}</integer>\n\
         \t<key>RunAtLoad</key>\n\
         \t<false/>\n\
         \t<key>ProcessType</key>\n\
         \t<string>Background</string>\n\
         \t<key>LowPriorityIO</key>\n\
         \t<true/>\n\
         \t<key>StandardInPath</key>\n\
         \t<string>/dev/null</string>\n\
         \t<key>StandardOutPath</key>\n\
         \t<string>/dev/null</string>\n\
         \t<key>StandardErrorPath</key>\n\
         \t<string>/dev/null</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// The user's launchd domain, `gui/<uid>`. The uid is the owner of `owned`, a file this process
/// just created, so no subprocess or extra system call is needed to learn it.
fn domain(owned: &Path) -> Result<String, Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(owned).map_err(super::io("inspect", owned))?;
        Ok(format!("gui/{}", metadata.uid()))
    }
    #[cfg(not(unix))]
    {
        Err(Error::Invalid {
            path: owned.to_path_buf(),
            message: "launchd exists only on macOS".to_owned(),
        })
    }
}

pub(super) fn install(
    context: &Context,
    exe: &str,
    service: &str,
    owned: &Path,
    interval_minutes: u32,
) -> Result<(), Error> {
    let path = file(context);
    let path_text = super::safe(&path)?.to_owned();
    write(&path, plist(exe, service, interval_minutes).as_bytes())?;
    let domain = domain(owned)?;
    let job = format!("{domain}/{LABEL}");
    query(context, "launchctl", &["bootout", &job])?;
    required(context, "launchctl", &["bootstrap", &domain, &path_text])?;
    required(context, "launchctl", &["enable", &job])?;
    Ok(())
}

/// Unloads the agent and removes its plist. `bootout` must succeed, or say the agent isn't
/// loaded (`No such process`, 3; `Could not find specified service`, 113); any other answer is
/// an error, and the plist and `service.toml` stay.
///
/// With neither the plist nor `service.toml` there, launchd is still asked (`print`) whether it
/// has the agent loaded, since its files may have been removed by hand; not loaded is nothing to
/// remove.
pub(super) fn uninstall(context: &Context) -> Result<(), Error> {
    let path = file(context);
    let owned = super::Private::Service.path(context);
    let exists = |path: &Path| std::fs::symlink_metadata(path).is_ok();
    let domain = if exists(&owned) {
        domain(&owned)?
    } else if exists(&path) {
        domain(&path)?
    } else {
        domain(&context.home)?
    };
    let job = format!("{domain}/{LABEL}");
    if !exists(&path) && !exists(&owned) {
        let args = ["print", job.as_str()];
        let printed = query(context, "launchctl", &args)?;
        if !printed.success() {
            if not_loaded(&printed) {
                return Ok(());
            }
            return Err(failed("launchctl", &args, &printed));
        }
    }
    let args = ["bootout", job.as_str()];
    let answer = query(context, "launchctl", &args)?;
    if !answer.success() && !not_loaded(&answer) {
        return Err(failed("launchctl", &args, &answer));
    }
    remove(&path)?;
    Ok(())
}

/// `launchctl bootout`'s answer for an agent that isn't loaded.
fn not_loaded(answer: &crate::process::Captured) -> bool {
    matches!(answer.code, Some(3 | 113))
        || answer.stderr.contains("No such process")
        || answer.stderr.contains("Could not find specified service")
}

/// How the agent differs from the one `install` would load for this record, if it does: first
/// the plist on disk, then (outside isolation) the program arguments launchd has loaded, since a
/// matching plist proves nothing about an agent loaded before it was written. An agent launchd
/// doesn't have loaded when its arguments are read is a mismatch, even if an earlier query found
/// it; a plist that can't be read, or a launchd that can't be asked, is an error. Neither is ever
/// a match.
pub(super) fn differs(
    context: &Context,
    exe: &str,
    service: &str,
    interval_minutes: u32,
) -> Result<Option<String>, String> {
    let Some(actual) = super::definition(&file(context))? else {
        return Ok(None);
    };
    if actual != plist(exe, service, interval_minutes) {
        return Ok(Some(super::differs_from_record(&format!("{LABEL}.plist"))));
    }
    if context.isolated {
        return Ok(None);
    }
    let owned = super::Private::Service.path(context);
    let job = format!(
        "{}/{LABEL}",
        domain(&owned).map_err(|error| error.to_string())?
    );
    let args = ["print", job.as_str()];
    let answer = query(context, "launchctl", &args).map_err(|error| error.to_string())?;
    if !answer.success() {
        if not_loaded(&answer) {
            return Ok(Some(format!("launchd doesn't have {job} loaded")));
        }
        return Err(failed("launchctl", &args, &answer).to_string());
    }
    let expected = [exe, "update", "--background", "--service", service];
    match arguments(&answer.stdout) {
        Some(loaded) if loaded == expected => Ok(None),
        Some(loaded) => Ok(Some(format!(
            "launchd has {LABEL} loaded with other arguments ({}); the job it runs is not this record's",
            loaded.join(" ")
        ))),
        None => Err(format!(
            "`launchctl {}` didn't list the agent's arguments",
            args.join(" ")
        )),
    }
}

/// The program arguments in `launchctl print`'s answer: one per line between `arguments = {` and
/// its closing `}`, indented with tabs.
fn arguments(printed: &str) -> Option<Vec<&str>> {
    let mut lines = printed.lines();
    lines.find(|line| line.trim() == "arguments = {")?;
    let mut found = Vec::new();
    for line in lines {
        if line.trim() == "}" {
            return Some(found);
        }
        found.push(line.trim_start_matches('\t'));
    }
    None
}

pub(super) fn registered(context: &Context, owned: &Path) -> Scheduler {
    let owned = if std::fs::symlink_metadata(owned).is_ok() {
        owned.to_path_buf()
    } else {
        file(context)
    };
    let domain = match domain(&owned) {
        Ok(domain) => domain,
        Err(error) => {
            return Scheduler {
                registered: false,
                detail: error.to_string(),
            };
        }
    };
    match query(
        context,
        "launchctl",
        &["print", &format!("{domain}/{LABEL}")],
    ) {
        Ok(answer) => Scheduler {
            registered: answer.success(),
            detail: if answer.success() {
                format!("{LABEL} is loaded in {domain}")
            } else {
                format!("{LABEL} is not loaded in {domain}")
            },
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
    fn the_agent_runs_the_supervisor_with_each_argument_escaped_and_never_joined() {
        let plist = plist(
            "/Users/a & b/<bin>/loadout",
            "/Users/a & b/state/service.toml",
            30,
        );
        assert!(
            plist.contains(
                "\t<array>\n\
                 \t\t<string>/Users/a &amp; b/&lt;bin&gt;/loadout</string>\n\
                 \t\t<string>update</string>\n\
                 \t\t<string>--background</string>\n\
                 \t\t<string>--service</string>\n\
                 \t\t<string>/Users/a &amp; b/state/service.toml</string>\n\
                 \t</array>\n"
            ),
            "{plist}"
        );
        assert!(
            plist.contains("<key>StartInterval</key>\n\t<integer>1800</integer>"),
            "{plist}"
        );
        assert!(
            plist.contains("<key>RunAtLoad</key>\n\t<false/>"),
            "{plist}"
        );
        assert!(
            plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"),
            "{plist}"
        );
        assert!(plist.ends_with("</dict>\n</plist>\n"), "{plist}");
    }
}
