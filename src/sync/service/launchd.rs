//! macOS: a launchd agent, `~/Library/LaunchAgents/dev.loadout.update.plist`, in the user's
//! `gui/<uid>` domain. launchd has no run-time limit of its own, so the supervisor's hard
//! deadline is the run's only bound. The arguments are a plist array, never a joined command
//! line; each is XML-escaped.

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

fn plist(exe: &str, service: &str, interval_minutes: u32) -> String {
    let arguments: String = [exe, "update", "--background", "--service", service]
        .iter()
        .map(|arg| format!("\t\t<string>{}</string>\n", xml(arg)))
        .collect();
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

/// How the plist differs from the one `install` would write for this record, if it does.
pub(super) fn differs(
    context: &Context,
    exe: &str,
    service: &str,
    interval_minutes: u32,
) -> Option<String> {
    let actual = super::definition(&file(context))?;
    (actual != plist(exe, service, interval_minutes))
        .then(|| super::differs_from_record(&format!("{LABEL}.plist")))
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
