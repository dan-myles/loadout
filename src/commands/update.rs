//! `loadout update`, also `loadout upgrade`: updates the installer-managed CLI itself from the
//! project's GitHub releases, on demand. It never fetches or applies the loadout; that is
//! `loadout sync`, and nothing updates the CLI on its own.

use std::path::PathBuf;

use crate::cli::Exit;
use crate::commands::sync;
use crate::context::Context;
use crate::release::{self, Channel};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Also consider prereleases, which are test builds; without it only stable releases are
    /// offered
    #[arg(long)]
    pub prerelease: bool,
    /// Legacy: the scheduled job older loadout versions registered runs `loadout update
    /// --background`, which still runs `loadout sync --background`
    #[arg(long, hide = true, conflicts_with_all = ["prerelease", "loadout"])]
    pub background: bool,
    /// Legacy, with --background: the service.toml the scheduled job was installed with
    #[arg(long, hide = true, value_name = "FILE", requires = "background")]
    pub service: Option<PathBuf>,
    /// Legacy, with --background: the supervised worker of a background sync
    #[arg(long, hide = true, requires = "background")]
    pub worker: bool,
    /// Legacy, with --worker: the supervisor run this worker belongs to
    #[arg(long, hide = true, requires = "worker", value_name = "ID")]
    pub run: Option<String>,
    /// Legacy: a checkout to apply, which is `loadout sync --loadout DIR` now
    #[arg(long, hide = true, value_name = "DIR")]
    pub loadout: Option<PathBuf>,
}

impl Args {
    /// The scheduled form older versions registered, as the `loadout sync --background` it
    /// stands for. A scheduler that still runs it keeps syncing after the CLI is updated, and
    /// is never taken for a CLI update.
    pub fn legacy(&self) -> Option<sync::Args> {
        self.background.then(|| sync::Args {
            loadout: None,
            background: true,
            service: self.service.clone(),
            worker: self.worker,
            run: self.run.clone(),
        })
    }
}

/// Says how to move a job that still runs the legacy form. Printed by every legacy run, so it
/// reaches the scheduler's log as well as a person running it.
pub fn deprecated() {
    eprintln!(
        "note: `loadout update --background` is the scheduled command of older loadout versions; it ran `loadout sync --background`. Run `loadout service install` to re-register the job"
    );
}

/// `loadout update [--prerelease]`: replaces this CLI with the newest release if it is
/// installer-managed and a strictly newer one is published. Exits 0 when updated, already up to
/// date, or when no release is published; 4 when this CLI can't be updated by loadout (not
/// installed by install.sh or install.ps1, under `LOADOUT_HOME` isolation, or another install
/// holds the folder); 1 when the download or verification fails.
pub fn run(args: &Args, context: &Context) -> anyhow::Result<Exit> {
    if let Some(legacy) = args.legacy() {
        deprecated();
        return sync::run(&legacy, context);
    }
    if let Some(dir) = &args.loadout {
        anyhow::bail!(
            "`loadout update` updates only the loadout CLI now; to apply {}, run `loadout sync --loadout {}`",
            dir.display(),
            dir.display()
        );
    }
    let channel = if args.prerelease {
        Channel::Prerelease
    } else {
        Channel::Stable
    };
    let exit = update_with(
        context,
        channel,
        |context| release::latest(context, channel),
        release::install,
        |newer: &release::Release| {
            format!("cli: updated {} to {}", newer.current(), newer.version())
        },
    )?;
    println!(
        "loadout update changes only the CLI; to fetch and apply your loadout, run `loadout sync`"
    );
    Ok(exit)
}

/// The update with its release lookup and install passed in. The live cancellation is checked
/// before the lookup and again before the install, so a cancelled run starts neither; a request
/// already under way is not interrupted. A skip (isolation, an unmanaged binary, another install
/// holding the folder) can't update this CLI and exits 4; no published release is not a failure;
/// any other error fails the update (exit 1). An error is never reported as up to date.
fn update_with<R>(
    context: &Context,
    channel: Channel,
    latest: impl FnOnce(&Context) -> Result<Option<R>, release::Error>,
    install: impl FnOnce(&R) -> Result<(), release::Error>,
    updated: impl FnOnce(&R) -> String,
) -> anyhow::Result<Exit> {
    if context.containment.is_cancelled() {
        return Ok(cancelled());
    }
    let (line, exit) = match latest(context) {
        Ok(None) => (
            format!("cli: up to date at {}", env!("CARGO_PKG_VERSION")),
            Exit::Success,
        ),
        Ok(Some(newer)) => {
            if context.containment.is_cancelled() {
                return Ok(cancelled());
            }
            match install(&newer) {
                Ok(()) => (updated(&newer), Exit::Success),
                Err(error) if error.skipped() => {
                    (format!("cli: not updated: {error}"), Exit::Unhealthy)
                }
                Err(error) => return Err(anyhow::Error::new(error).context("self-update failed")),
            }
        }
        Err(release::Error::NoRelease) => (
            match channel {
                Channel::Stable => "cli: not updated: no stable loadout release is published yet; `loadout update --prerelease` also considers test prereleases".to_owned(),
                Channel::Prerelease => "cli: not updated: no loadout release is published yet".to_owned(),
            },
            Exit::Success,
        ),
        Err(error) if error.skipped() => (format!("cli: not updated: {error}"), Exit::Unhealthy),
        Err(error) => return Err(anyhow::Error::new(error).context("self-update failed")),
    };
    println!("{line}");
    // Cancelled while the lookup or the install ran: what happened is shown, but the run was
    // stopped, so it ends as cancelled. A real error above already returned with exit 1.
    if context.containment.is_cancelled() {
        return Ok(cancelled());
    }
    Ok(exit)
}

fn cancelled() -> Exit {
    println!("cancelled: {}", crate::apply::CANCELLED);
    Exit::Unhealthy
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::path::PathBuf;

    use clap::Parser;

    use super::*;
    use crate::cli::{Cli, Command};
    use crate::platform::Os;
    use crate::process::Containment;

    /// What a test's release lookup answers.
    type Lookup = Result<Option<()>, release::Error>;

    fn context() -> Context {
        Context::resolve(Os::current().unwrap(), |_| None, Some(PathBuf::from("/h"))).unwrap()
    }

    /// A supervised context, the only kind a supervisor can cancel.
    fn supervised() -> Context {
        Context {
            containment: Containment::inherited_for_tests(),
            ..context()
        }
    }

    fn cancel(context: &Context) {
        context
            .containment
            .cancel()
            .expect("only a supervised run can be cancelled")
            .cancel();
    }

    fn parsed(args: &[&str]) -> Args {
        let cli = Cli::try_parse_from(std::iter::once("loadout").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("{args:?}: {error}"));
        match cli.command {
            Command::Update(args) => args,
            other => panic!("{args:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn upgrade_is_the_same_command_as_update() {
        for extra in [&[][..], &["--prerelease"]] {
            let update = parsed(&[&["update"][..], extra].concat());
            let upgrade = parsed(&[&["upgrade"][..], extra].concat());
            assert_eq!(format!("{update:?}"), format!("{upgrade:?}"));
            assert_eq!(update.prerelease, !extra.is_empty());
            assert!(update.legacy().is_none(), "a CLI update never syncs");
        }
    }

    #[test]
    fn only_the_legacy_scheduled_form_stands_for_a_sync() {
        let legacy = parsed(&["update", "--background", "--service", "service.toml"])
            .legacy()
            .unwrap();
        assert!(legacy.background && !legacy.worker && legacy.loadout.is_none());
        assert_eq!(legacy.service, Some(PathBuf::from("service.toml")));
        assert!(legacy.recorded());
        let worker = parsed(&["update", "--background", "--worker", "--run", "r1"])
            .legacy()
            .unwrap();
        assert!(worker.worker && worker.run.as_deref() == Some("r1"));
        for refused in [
            &["update", "--background", "--prerelease"][..],
            &["update", "--background", "--loadout", "x"],
            &["update", "--worker"],
            &["update", "--service", "x"],
            &["update", "--run", "r1"],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("loadout").chain(refused.iter().copied()))
                    .is_err(),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn exits_follow_what_the_update_could_do() {
        let context = context();
        let cases: Vec<(&str, Lookup, Exit)> = vec![
            ("up to date", Ok(None), Exit::Success),
            ("newer installed", Ok(Some(())), Exit::Success),
            ("no release", Err(release::Error::NoRelease), Exit::Success),
            ("isolated", Err(release::Error::Isolated), Exit::Unhealthy),
            (
                "unmanaged",
                Err(release::Error::Unmanaged {
                    reason: "cargo install".to_owned(),
                }),
                Exit::Unhealthy,
            ),
        ];
        for (name, found, exit) in cases {
            let outcome = update_with(
                &context,
                Channel::Stable,
                |_| found,
                |()| Ok(()),
                |()| "cli: updated".to_owned(),
            );
            assert_eq!(outcome.unwrap(), exit, "{name}");
        }
        let busy = update_with(
            &context,
            Channel::Prerelease,
            |_| Ok(Some(())),
            |()| {
                Err(release::Error::Busy {
                    dir: PathBuf::from("/bin"),
                })
            },
            |()| String::new(),
        );
        assert_eq!(busy.unwrap(), Exit::Unhealthy);
    }

    #[test]
    fn a_lookup_or_install_error_fails_and_is_never_up_to_date() {
        let context = context();
        let lookup = update_with(
            &context,
            Channel::Prerelease,
            |_| {
                Err::<Option<()>, _>(release::Error::Download {
                    detail: "rate limited".to_owned(),
                })
            },
            |()| panic!("nothing to install"),
            |()| String::new(),
        );
        assert!(lookup.is_err());
        let install = update_with(
            &context,
            Channel::Stable,
            |_| Ok(Some(())),
            |()| {
                Err(release::Error::Verify {
                    detail: "checksum mismatch".to_owned(),
                })
            },
            |()| String::new(),
        );
        assert!(install.is_err());
    }

    #[test]
    fn a_cancelled_run_makes_no_release_request() {
        let context = supervised();
        cancel(&context);
        let asked = Cell::new(false);
        let exit = update_with(
            &context,
            Channel::Stable,
            |_| {
                asked.set(true);
                Ok(Some(()))
            },
            |()| panic!("a cancelled run never installs"),
            |()| String::new(),
        )
        .unwrap();
        assert_eq!(exit, Exit::Unhealthy);
        assert!(
            !asked.get(),
            "no release request starts after the cancellation"
        );
    }

    #[test]
    fn a_cancellation_between_discovery_and_install_installs_nothing() {
        let context = supervised();
        let installed = Cell::new(false);
        let exit = update_with(
            &context,
            Channel::Prerelease,
            |context| {
                cancel(context);
                Ok(Some(()))
            },
            |()| {
                installed.set(true);
                Ok(())
            },
            |()| String::new(),
        )
        .unwrap();
        assert_eq!(exit, Exit::Unhealthy);
        assert!(!installed.get());
    }

    #[test]
    fn a_real_install_error_keeps_failing_even_when_the_run_was_also_cancelled() {
        let context = supervised();
        let outcome = update_with(
            &context,
            Channel::Stable,
            |_| Ok(Some(())),
            |()| {
                cancel(&context);
                Err(release::Error::Download {
                    detail: "connection reset".to_owned(),
                })
            },
            |()| String::new(),
        );
        assert!(outcome.is_err(), "exit 1 outranks the cancellation");
    }
}
