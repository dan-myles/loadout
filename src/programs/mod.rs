use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::context::{self, Context};
use crate::diagnostic::{Check, CheckStatus};
use crate::machine::Facts;
use crate::manifest::Manager;
use crate::platform::Os;
use crate::process::{self, Containment};
use crate::resolve::Program;

mod detect;
mod manager;

use detect::{Eligibility, Unanswered};
use manager::Invocation;

pub const ISOLATED: &str = "programs are not installed under LOADOUT_HOME isolation";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("programs.{program}: {manager} package {package:?} {reason}")]
    Package {
        program: String,
        manager: &'static str,
        package: String,
        reason: String,
    },
    #[error(
        "programs.{program}: cannot tell whether {manager} package {package:?} is installed: {detail}"
    )]
    Detect {
        program: String,
        manager: &'static str,
        package: String,
        detail: String,
    },
    #[error("programs.{program}: only a missing program can be installed")]
    NotMissing { program: String },
    #[error(
        "programs.{program}: {manager} finished installing {package:?}, but the background run was cancelled, so loadout didn't confirm it"
    )]
    Unconfirmed {
        program: String,
        manager: &'static str,
        package: String,
    },
    /// U8 refused to start this install's query or installer because the run was cancelled.
    #[error(
        "programs.{program}: the background run was cancelled, so loadout started no further work"
    )]
    NotStarted { program: String },
    #[error(
        "programs.{program}: the machine changed after the plan ({detail}), so loadout installed nothing; plan again"
    )]
    Changed { program: String, detail: String },
    #[error(
        "programs.{program}: {manager} installs {package:?} system-wide, which needs --elevated"
    )]
    NeedsElevation {
        program: String,
        manager: &'static str,
        package: String,
    },
    #[error(
        "programs.{program}: installing {package:?} with {manager} needs sudo, which is not on PATH"
    )]
    NoSudo {
        program: String,
        manager: &'static str,
        package: String,
    },
    #[error("programs.{program}: cannot run {command}")]
    Spawn {
        program: String,
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("programs.{program}: {command} failed with {status}{output}")]
    Install {
        program: String,
        command: String,
        status: String,
        output: String,
    },
    #[error(
        "programs.{program}: {manager} reported success, but {package:?} is still not installed"
    )]
    NotInstalled {
        program: String,
        manager: &'static str,
        package: String,
    },
}

/// One program's outcome on this machine. `plan::build` turns `Installed` into `unchanged`,
/// `Missing` into `install` (or `blocked` when it needs elevation the run doesn't have), and
/// carries `Unsupported` and `Blocked` through. Only `steps` can produce an `Installed` or
/// `Missing` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub program: String,
    pub state: State,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Installed(Package),
    Missing(Missing),
    Unsupported(String),
    Blocked(String),
}

/// The selected manager, the exact package id the loadout declared for it (already validated),
/// and the manager executable found on PATH.
///
/// ```compile_fail
/// let package = loadout::programs::Package {
///     manager: loadout::manifest::Manager::Npm,
///     id: "--prefix=/etc".to_owned(),
///     executable: "/usr/bin/npm".into(),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    manager: Manager,
    id: String,
    executable: PathBuf,
}

impl Package {
    pub fn manager(&self) -> Manager {
        self.manager
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }
}

/// A package `steps` found missing, the scope its install needs, and the containment of the
/// run that found it, which its install and re-checks use too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    package: Package,
    scope: Scope,
    containment: Containment,
}

impl Missing {
    pub fn package(&self) -> &Package {
        &self.package
    }

    pub fn scope(&self) -> Scope {
        self.scope
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::System => "system",
        }
    }
}

/// What `install` did: installed the program, or found it installed when it looked again just
/// before installing, and ran nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    Now,
    Already,
}

impl Step {
    pub fn needs_elevation(&self) -> bool {
        matches!(&self.state, State::Missing(missing) if missing.scope == Scope::System)
    }
}

pub fn steps(
    programs: &BTreeMap<String, Program>,
    facts: &Facts,
    context: &Context,
) -> Result<Vec<Step>, Error> {
    programs
        .iter()
        .map(|(id, program)| {
            let state = if context.isolated {
                State::Blocked(ISOLATED.to_owned())
            } else {
                state(id, program, facts.os, &context.containment)?
            };
            Ok(Step {
                program: id.clone(),
                state,
            })
        })
        .collect()
}

fn state(id: &str, program: &Program, os: Os, containment: &Containment) -> Result<State, Error> {
    let (manager, package, executable) = match select(program, os, manager::available) {
        Ok(selected) => selected,
        Err(reason) => return Ok(State::Unsupported(reason)),
    };
    manager::check_package(manager, package).map_err(|reason| Error::Package {
        program: id.to_owned(),
        manager: manager.id(),
        package: package.to_owned(),
        reason,
    })?;
    let package = Package {
        manager,
        id: package.to_owned(),
        executable,
    };
    Ok(
        match observe(&package, containment)
            .map_err(|unanswered| detect_error(id, &package, unanswered.to_string()))?
        {
            Observation::Installed => State::Installed(package),
            Observation::Missing(scope) => State::Missing(Missing {
                package,
                scope,
                containment: containment.clone(),
            }),
            Observation::Unsupported(reason) => State::Unsupported(reason),
        },
    )
}

enum Observation {
    Installed,
    Missing(Scope),
    Unsupported(String),
}

/// The one answer `steps` and `install` both act on: the manager's installed-query, then, for a
/// missing package, whether and in which scope it can be installed. An answer loadout can't
/// classify cancels a supervised run (see `unclassified`).
fn observe(package: &Package, containment: &Containment) -> Result<Observation, Unanswered> {
    unclassified(containment, classify(package, containment))
}

fn classify(package: &Package, containment: &Containment) -> Result<Observation, Unanswered> {
    if detect::installed(
        containment,
        package.manager,
        &package.executable,
        &package.id,
    )? {
        return Ok(Observation::Installed);
    }
    Ok(
        match detect::eligibility(
            containment,
            package.manager,
            &package.executable,
            &package.id,
        )? {
            Eligibility::Scope(scope) => Observation::Missing(scope),
            Eligibility::Unsupported(reason) => Observation::Unsupported(reason),
        },
    )
}

/// A manager answer `detect` can't classify (an unexpected exit, malformed or unrecognized
/// output, or a query that failed) is an error, as in the foreground; in a supervised
/// background run it also cancels the run, so no later query or installer starts. Installed,
/// missing, and unsupported answers, including the nonzero exits some managers use for
/// "missing", are classified and never cancel.
fn unclassified<T>(
    containment: &Containment,
    answer: Result<T, Unanswered>,
) -> Result<T, Unanswered> {
    if answer.is_err()
        && let Some(cancel) = containment.cancel()
    {
        cancel.cancel();
    }
    answer
}

fn detect_error(program: &str, package: &Package, detail: String) -> Error {
    Error::Detect {
        program: program.to_owned(),
        manager: package.manager.id(),
        package: package.id.clone(),
        detail,
    }
}

/// The first manager in this OS's order that the program declares and that is on PATH. The
/// error is the reason the program is unsupported here.
fn select(
    program: &Program,
    os: Os,
    available: impl Fn(Manager) -> Option<PathBuf>,
) -> Result<(Manager, &str, PathBuf), String> {
    let declared: Vec<(Manager, &str)> = manager::order(os)
        .filter_map(|manager| {
            program
                .packages
                .get(&manager)
                .map(|package| (manager, package.as_str()))
        })
        .collect();
    if declared.is_empty() {
        let all: Vec<&str> = program
            .packages
            .keys()
            .map(|manager| manager.id())
            .collect();
        return Err(format!(
            "declares no manager loadout uses on {os} (declared: {})",
            if all.is_empty() {
                "none".to_owned()
            } else {
                all.join(", ")
            }
        ));
    }
    if let Some((manager, package, executable)) = declared
        .iter()
        .find_map(|(manager, package)| available(*manager).map(|path| (*manager, *package, path)))
    {
        return Ok((manager, package, executable));
    }
    let missing: Vec<String> = declared
        .iter()
        .map(|(manager, _)| format!("{} ({})", manager.id(), manager::tools(*manager).join(", ")))
        .collect();
    Err(format!(
        "none of its managers for {os} is on PATH: {}",
        missing.join("; ")
    ))
}

/// Installs one missing program.
///
/// First it checks the step against the machine as it is now: the package id, the manager's
/// scope, the executable PATH resolves the manager to, and the manager's own answer. A package
/// that is installed by then is `Installed::Already`, and nothing runs, so a stale step never
/// reinstalls or upgrades. Only then does elevation matter: a system-scope install runs only with
/// `elevated`, through `sudo` on Unix (unless already root) and directly on Windows, where the
/// installer raises UAC. After the manager reports success, it asks again, so a success that left
/// nothing installed is an error.
///
/// Two limits remain. Another process can install the package between that last look and the
/// installer starting; winget (`--no-upgrade`), Homebrew, apt (`--no-upgrade`), flatpak, Scoop,
/// Chocolatey, uv, pipx, and snap then skip it themselves, while npm, pnpm, bun, go, cargo, dnf,
/// pacman, and zypper may upgrade it. And an equal PATH answer means the same path, not the same
/// file: a manager binary replaced in place isn't detected.
pub fn install(step: &Step, elevated: bool) -> Result<Installed, Error> {
    let program = step.program.as_str();
    let State::Missing(missing) = &step.state else {
        return Err(Error::NotMissing {
            program: program.to_owned(),
        });
    };
    let package = &missing.package;
    let manager = package.manager;
    let changed = |detail: String| Error::Changed {
        program: program.to_owned(),
        detail,
    };
    manager::check_package(manager, &package.id).map_err(|reason| Error::Package {
        program: program.to_owned(),
        manager: manager.id(),
        package: package.id.clone(),
        reason,
    })?;
    if manager::scope(manager).is_some_and(|scope| scope != missing.scope) {
        return Err(changed(format!(
            "{} never installs in {} scope",
            manager.id(),
            missing.scope.name()
        )));
    }
    match manager::available(manager) {
        Some(path) if path == package.executable => {}
        Some(path) => {
            return Err(changed(format!(
                "{} now resolves to {}, not {}",
                manager.id(),
                path.display(),
                package.executable.display()
            )));
        }
        None => {
            return Err(changed(format!(
                "{} is no longer on PATH ({})",
                manager.id(),
                manager::tools(manager).join(", ")
            )));
        }
    }
    let not_started = || Error::NotStarted {
        program: program.to_owned(),
    };
    match observe(package, &missing.containment).map_err(|unanswered| match unanswered {
        Unanswered::Refused(_) => not_started(),
        Unanswered::Failed(detail) => detect_error(program, package, detail),
    })? {
        Observation::Installed => return Ok(Installed::Already),
        Observation::Missing(scope) if scope == missing.scope => {}
        Observation::Missing(scope) => {
            return Err(changed(format!(
                "{} now needs {} scope, not {}",
                package.id,
                scope.name(),
                missing.scope.name()
            )));
        }
        Observation::Unsupported(reason) => return Err(changed(reason)),
    }
    if missing.scope == Scope::System && !elevated {
        return Err(Error::NeedsElevation {
            program: program.to_owned(),
            manager: manager.id(),
            package: package.id.clone(),
        });
    }
    trust_tap(program, package, &missing.containment)?;
    let invocation = manager::install(manager, &package.executable, &package.id, missing.scope);
    let invocation = if missing.scope == Scope::System {
        elevate(invocation).ok_or_else(|| Error::NoSudo {
            program: program.to_owned(),
            manager: manager.id(),
            package: package.id.clone(),
        })?
    } else {
        invocation
    };
    let command = invocation.display();
    let output = process::install(&invocation, &missing.containment).map_err(|source| {
        if process::install_refused(&source) {
            not_started()
        } else {
            Error::Spawn {
                program: program.to_owned(),
                command: command.clone(),
                source,
            }
        }
    })?;
    if !output
        .code
        .is_some_and(|code| manager::install_succeeded(manager, code))
    {
        return Err(Error::Install {
            program: program.to_owned(),
            command,
            status: output.status(),
            output: output.tail(),
        });
    }
    verified(program, missing)
}

/// Taps and trusts the third-party tap a `user/repo/name` brew package comes from: naming it in
/// the loadout is the trust. Homebrew from before `brew trust` loads any tapped formula, so its
/// unknown-command answer counts as trusted.
fn trust_tap(program: &str, package: &Package, containment: &Containment) -> Result<(), Error> {
    if !matches!(package.manager, Manager::Brew | Manager::Cask) {
        return Ok(());
    }
    let Some((tap, _)) = detect::third_party_tap(&package.id) else {
        return Ok(());
    };
    for args in [&["tap", tap][..], &["trust", "--tap", tap][..]] {
        let invocation = Invocation::new(&package.executable, args);
        let command = invocation.display();
        let output = process::install(&invocation, containment).map_err(|source| {
            if process::install_refused(&source) {
                Error::NotStarted {
                    program: program.to_owned(),
                }
            } else {
                Error::Spawn {
                    program: program.to_owned(),
                    command: command.clone(),
                    source,
                }
            }
        })?;
        let older_brew = args[0] == "trust" && output.stderr.contains("Unknown command");
        if !output.success() && !older_brew {
            return Err(Error::Install {
                program: program.to_owned(),
                command,
                status: output.status(),
                output: output.tail(),
            });
        }
    }
    Ok(())
}

/// The installed-query after a successful install. An answer that can't be classified cancels a
/// supervised run, as in `observe`.
fn verified(program: &str, missing: &Missing) -> Result<Installed, Error> {
    let package = &missing.package;
    let answer = detect::installed(
        &missing.containment,
        package.manager,
        &package.executable,
        &package.id,
    );
    match unclassified(&missing.containment, answer) {
        Ok(true) => Ok(Installed::Now),
        Ok(false) => Err(Error::NotInstalled {
            program: program.to_owned(),
            manager: package.manager.id(),
            package: package.id.clone(),
        }),
        // A cancellation that landed while the installer ran let it finish; U8 then refused the
        // confirming query. That is work the run didn't do, not a failed install.
        Err(Unanswered::Refused(_)) => Err(Error::Unconfirmed {
            program: program.to_owned(),
            manager: package.manager.id(),
            package: package.id.clone(),
        }),
        Err(Unanswered::Failed(detail)) => Err(detect_error(program, package, detail)),
    }
}

fn elevate(invocation: Invocation) -> Option<Invocation> {
    if cfg!(windows) || is_root() {
        return Some(invocation);
    }
    let sudo = context::which("sudo")?;
    let mut args = vec!["--".into()];
    if !invocation.env.is_empty() {
        args.push("env".into());
        args.extend(
            invocation
                .env
                .iter()
                .map(|(name, value)| format!("{name}={value}").into()),
        );
    }
    args.push(invocation.program.into_os_string());
    args.extend(invocation.args);
    Some(Invocation {
        program: sudo,
        args,
        env: &[],
    })
}

#[cfg(target_os = "linux")]
fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").is_ok_and(|metadata| metadata.uid() == 0)
}

#[cfg(not(target_os = "linux"))]
fn is_root() -> bool {
    false
}

/// Which managers this OS would use, which are missing, and what elevation needs. Runs no
/// manager under isolation.
pub fn checks(context: &Context) -> Vec<Check> {
    let mut user = Vec::new();
    let mut system = Vec::new();
    let mut per_package = Vec::new();
    let mut missing = Vec::new();
    for manager in manager::order(context.os) {
        let list = match (manager::available(manager), manager::scope(manager)) {
            (None, _) => &mut missing,
            (Some(_), Some(Scope::User)) => &mut user,
            (Some(_), Some(Scope::System)) => &mut system,
            (Some(_), None) => &mut per_package,
        };
        list.push(manager.id());
    }
    let groups = [
        ("per user", &user),
        ("with --elevated", &system),
        (
            "per package (user scope when it has one, else --elevated)",
            &per_package,
        ),
        ("not on PATH", &missing),
    ];
    let mut detail: Vec<String> = groups
        .iter()
        .filter(|(_, managers)| !managers.is_empty())
        .map(|(label, managers)| format!("{label}: {}", managers.join(", ")))
        .collect();
    if context.isolated {
        detail.push(ISOLATED.to_owned());
    }
    let none = user.is_empty() && system.is_empty() && per_package.is_empty();
    let mut checks = vec![Check::new(
        "programs",
        if none {
            CheckStatus::Warn
        } else {
            CheckStatus::Info
        },
        detail.join("; "),
    )];
    if context.os != Os::Windows && !system.is_empty() {
        checks.push(elevation(&system));
    }
    if !context.isolated && user.contains(&Manager::Flatpak.id()) {
        checks.push(flatpak_remotes(&context.containment));
    }
    checks
}

fn elevation(system: &[&str]) -> Check {
    if is_root() {
        return Check::new("elevation", CheckStatus::Pass, "running as root");
    }
    match context::which("sudo") {
        Some(sudo) => Check::new(
            "elevation",
            CheckStatus::Pass,
            format!(
                "--elevated runs {} through {}",
                system.join(", "),
                sudo.display()
            ),
        ),
        None => Check::new(
            "elevation",
            CheckStatus::Warn,
            format!(
                "sudo is not on PATH, so --elevated cannot install with {}",
                system.join(", ")
            ),
        ),
    }
}

fn flatpak_remotes(containment: &Containment) -> Check {
    let Some(flatpak) = manager::available(Manager::Flatpak) else {
        return Check::new("flatpak", CheckStatus::Warn, "flatpak is no longer on PATH");
    };
    let invocation = Invocation::new(&flatpak, ["remotes", "--user", "--columns=name"]);
    match process::query(&invocation, containment) {
        Ok(output) if output.code == Some(0) => {
            let remotes: Vec<&str> = output
                .stdout
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            if remotes.is_empty() {
                Check::new(
                    "flatpak",
                    CheckStatus::Warn,
                    "no per-user remote, so flatpak --user installs will fail; add one with \
                     flatpak remote-add --user",
                )
            } else {
                Check::new(
                    "flatpak",
                    CheckStatus::Pass,
                    format!("per-user remotes: {}", remotes.join(", ")),
                )
            }
        }
        Ok(output) => Check::new(
            "flatpak",
            CheckStatus::Warn,
            format!("flatpak remotes --user failed with {}", output.status()),
        ),
        Err(error) => Check::new(
            "flatpak",
            CheckStatus::Warn,
            format!("{}: {error}", invocation.display()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(packages: &[(Manager, &str)]) -> Program {
        Program {
            packages: packages
                .iter()
                .map(|(manager, package)| (*manager, (*package).to_owned()))
                .collect(),
        }
    }

    fn on_path(managers: &[Manager]) -> impl Fn(Manager) -> Option<PathBuf> {
        move |manager| {
            managers
                .contains(&manager)
                .then(|| PathBuf::from(format!("/bin/{}", manager.id())))
        }
    }

    #[test]
    fn selection_takes_the_first_declared_manager_in_os_order_that_is_available() {
        let ripgrep = program(&[
            (Manager::Cargo, "ripgrep"),
            (Manager::Brew, "ripgrep"),
            (Manager::Apt, "ripgrep"),
            (Manager::Winget, "BurntSushi.ripgrep.MSVC"),
            (Manager::Scoop, "ripgrep"),
        ]);
        let all = on_path(&Manager::ALL);
        let pick = |os, available: &dyn Fn(Manager) -> Option<PathBuf>| {
            select(&ripgrep, os, available).map(|(manager, package, _)| (manager, package))
        };
        assert_eq!(pick(Os::Linux, &all), Ok((Manager::Apt, "ripgrep")));
        assert_eq!(pick(Os::Macos, &all), Ok((Manager::Brew, "ripgrep")));
        assert_eq!(
            pick(Os::Windows, &all),
            Ok((Manager::Winget, "BurntSushi.ripgrep.MSVC"))
        );
        assert_eq!(
            pick(Os::Linux, &on_path(&[Manager::Brew, Manager::Cargo])),
            Ok((Manager::Brew, "ripgrep"))
        );
        assert_eq!(
            pick(Os::Windows, &on_path(&[Manager::Cargo, Manager::Scoop])),
            Ok((Manager::Scoop, "ripgrep"))
        );
        assert_eq!(
            pick(Os::Linux, &on_path(&[Manager::Cargo])),
            Ok((Manager::Cargo, "ripgrep"))
        );
    }

    #[test]
    fn language_managers_follow_their_fixed_order() {
        let tool = program(&[
            (Manager::Go, "g"),
            (Manager::Bun, "b"),
            (Manager::Pnpm, "p"),
            (Manager::Npm, "n"),
            (Manager::Pipx, "x"),
            (Manager::Uv, "u"),
        ]);
        let mut available: Vec<Manager> = Manager::ALL.to_vec();
        for expected in ["u", "x", "n", "p", "b", "g"] {
            let (manager, package, _) = select(&tool, Os::Macos, on_path(&available)).unwrap();
            assert_eq!(package, expected);
            available.retain(|m| *m != manager);
        }
        assert!(select(&tool, Os::Macos, on_path(&available)).is_err());
    }

    #[test]
    fn a_program_without_an_available_declared_manager_is_unsupported_with_a_reason() {
        let mac_only = program(&[(Manager::Cask, "ghostty")]);
        let reason = select(&mac_only, Os::Linux, on_path(&Manager::ALL)).unwrap_err();
        assert_eq!(
            reason,
            "declares no manager loadout uses on linux (declared: cask)"
        );
        assert!(select(&mac_only, Os::Windows, on_path(&Manager::ALL)).is_err());

        let fd = program(&[(Manager::Apt, "fd-find"), (Manager::Dnf, "fd-find")]);
        let reason = select(&fd, Os::Linux, on_path(&[Manager::Pacman])).unwrap_err();
        assert_eq!(
            reason,
            "none of its managers for linux is on PATH: apt (apt-get, dpkg-query); dnf (dnf, rpm)"
        );
        let empty = select(&program(&[]), Os::Linux, on_path(&Manager::ALL)).unwrap_err();
        assert!(empty.contains("declared: none"), "{empty}");
    }

    #[test]
    fn isolation_blocks_every_program_without_looking_for_a_manager() {
        let sandbox = if cfg!(windows) {
            "C:\\sandbox"
        } else {
            "/sandbox"
        };
        let context = Context::resolve(
            Os::Linux,
            |name| (name == "LOADOUT_HOME").then(|| sandbox.into()),
            None,
        )
        .unwrap();
        let facts = Facts {
            name: "box".to_owned(),
            os: Os::Linux,
            distro: None,
            arch: crate::platform::Arch::X86_64,
            home: sandbox.into(),
        };
        let programs = BTreeMap::from([
            ("ripgrep".to_owned(), program(&[(Manager::Apt, "ripgrep")])),
            (
                "bad".to_owned(),
                program(&[(Manager::Npm, "--prefix=/etc")]),
            ),
            ("mac".to_owned(), program(&[(Manager::Cask, "ghostty")])),
        ]);
        let steps = steps(&programs, &facts, &context).unwrap();
        assert_eq!(steps.len(), 3);
        for step in &steps {
            assert_eq!(
                step.state,
                State::Blocked(ISOLATED.to_owned()),
                "{}",
                step.program
            );
            assert!(!step.needs_elevation());
            assert!(matches!(install(step, true), Err(Error::NotMissing { .. })));
        }
    }

    /// Manager answers that can't be classified cancel a supervised run at the layer that
    /// knows the answer, so nothing starts after them; classified answers never cancel. The
    /// managers are scripts given by path, so no PATH or real manager is involved.
    #[cfg(unix)]
    mod cancellation {
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        use super::super::*;
        use crate::process::owned::{Faults, Owned, Record};

        /// A fake manager: records each call in `calls`, then prints `stdout` and exits `code`.
        fn manager(dir: &Path, name: &str, stdout: &str, code: i32) -> PathBuf {
            let path = dir.join(name);
            let calls = dir.join(format!("{name}.calls"));
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nprintf '%s' '{stdout}'\nexit {code}\n",
                calls.display()
            );
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn package(manager: Manager, executable: PathBuf) -> Package {
            Package {
                manager,
                id: "tool".to_owned(),
                executable,
            }
        }

        #[test]
        fn an_unexpected_exit_cancels_and_no_later_query_starts() {
            let dir = tempfile::tempdir().unwrap();
            let containment = Containment::inherited_for_tests();
            let broken = package(Manager::Cargo, manager(dir.path(), "broken", "", 2));
            assert!(observe(&broken, &containment).is_err());
            assert!(containment.is_cancelled());

            let healthy = package(Manager::Cargo, manager(dir.path(), "healthy", "", 0));
            assert!(observe(&healthy, &containment).is_err());
            assert!(
                !dir.path().join("healthy.calls").exists(),
                "a cancelled run must start no query"
            );
        }

        #[test]
        fn malformed_json_cancels() {
            let dir = tempfile::tempdir().unwrap();
            let containment = Containment::inherited_for_tests();
            let npm = package(Manager::Npm, manager(dir.path(), "npm", "{not json", 0));
            assert!(observe(&npm, &containment).is_err());
            assert!(containment.is_cancelled());
        }

        #[test]
        fn an_unclassified_answer_after_an_install_cancels() {
            let dir = tempfile::tempdir().unwrap();
            let containment = Containment::inherited_for_tests();
            let missing = Missing {
                package: package(Manager::Cargo, manager(dir.path(), "cargo", "", 2)),
                scope: Scope::User,
                containment: containment.clone(),
            };
            let error = verified("tool", &missing).unwrap_err();
            assert!(matches!(error, Error::Detect { .. }), "{error}");
            assert!(containment.is_cancelled());
        }

        #[test]
        fn a_run_cancelled_during_its_install_starts_no_confirming_query() {
            let dir = tempfile::tempdir().unwrap();
            let containment = Containment::inherited_for_tests();
            let missing = Missing {
                package: package(Manager::Cargo, manager(dir.path(), "cargo", "", 0)),
                scope: Scope::User,
                containment: containment.clone(),
            };
            containment.cancel().unwrap().cancel();
            let error = verified("tool", &missing).unwrap_err();
            assert!(matches!(error, Error::Unconfirmed { .. }), "{error}");
            assert!(
                !dir.path().join("cargo.calls").exists(),
                "a cancelled run must start no query"
            );
        }

        /// A confirming query that started before the cancel and then failed is a real failure:
        /// the flag set meanwhile never turns it into a skip.
        #[test]
        fn a_query_failing_after_a_cancel_that_landed_mid_query_still_fails() {
            let dir = tempfile::tempdir().unwrap();
            let sleep = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|dir| dir.join("sleep"))
                .find(|path| path.is_file())
                .expect("a sleep command on the test's PATH");
            let (started, go) = (dir.path().join("started"), dir.path().join("go"));
            let cargo = script(
                dir.path(),
                "cargo",
                &format!(
                    ": > '{}'\nn=0; while [ ! -e '{}' ] && [ $n -lt 300 ]; do '{}' 0.1; n=$((n + 1)); done\nexit 2\n",
                    started.display(),
                    go.display(),
                    sleep.display()
                ),
            );
            let containment = Containment::inherited_for_tests();
            let missing = Missing {
                package: package(Manager::Cargo, cargo),
                scope: Scope::User,
                containment: containment.clone(),
            };
            let flag = containment.cancel().unwrap().clone();
            let canceller = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                while !started.exists() {
                    assert!(Instant::now() < deadline, "the query never started");
                    std::thread::sleep(Duration::from_millis(10));
                }
                flag.cancel();
                std::fs::write(go, "").unwrap();
            });
            let error = verified("tool", &missing).unwrap_err();
            canceller.join().unwrap();
            assert!(matches!(error, Error::Detect { .. }), "{error}");
            assert!(containment.is_cancelled());
        }

        #[test]
        fn a_classified_missing_answer_does_not_cancel() {
            let dir = tempfile::tempdir().unwrap();
            let containment = Containment::inherited_for_tests();
            let cargo = package(Manager::Cargo, manager(dir.path(), "cargo", "", 0));
            assert!(matches!(
                observe(&cargo, &containment),
                Ok(Observation::Missing(Scope::User))
            ));
            let npm = package(
                Manager::Npm,
                manager(dir.path(), "npm", r#"{"name":"lib","dependencies":{}}"#, 0),
            );
            assert!(matches!(
                observe(&npm, &containment),
                Ok(Observation::Missing(Scope::User))
            ));
            assert!(!containment.is_cancelled());
        }

        /// A fake manager from a shell body, given by path.
        fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        #[test]
        fn a_malformed_but_complete_answer_cancels() {
            let dir = tempfile::tempdir().unwrap();
            let containment = Containment::inherited_for_tests();
            let npm = package(
                Manager::Npm,
                manager(dir.path(), "npm", r#"{"dependencies":{}}"#, 0),
            );
            assert!(observe(&npm, &containment).is_err());
            assert!(containment.is_cancelled());
        }

        /// pacman's documented "missing" answer is exit 1 with "was not found": a nonzero exit
        /// that is a classification, not a failure.
        #[test]
        fn a_documented_nonzero_missing_answer_does_not_cancel() {
            let dir = tempfile::tempdir().unwrap();
            let containment = Containment::inherited_for_tests();
            let pacman = package(
                Manager::Pacman,
                script(
                    dir.path(),
                    "pacman",
                    "echo \"error: package 'tool' was not found\" >&2\nexit 1\n",
                ),
            );
            assert!(matches!(
                observe(&pacman, &containment),
                Ok(Observation::Missing(Scope::System))
            ));
            assert!(!containment.is_cancelled());
        }

        /// winget answering that no installer applies in either scope is the classified
        /// `Unsupported`, through the same wrapper `observe` uses.
        #[test]
        fn an_unsupported_eligibility_answer_does_not_cancel() {
            let dir = tempfile::tempdir().unwrap();
            let containment = Containment::inherited_for_tests();
            let winget = script(
                dir.path(),
                "winget",
                "printf 'Found X [A.B]\\nInstaller:\\n  No applicable installer found; see logs for more details.\\n  Installer Url: https://x\\n'\n",
            );
            let answer = unclassified(
                &containment,
                detect::eligibility(&containment, Manager::Winget, &winget, "A.B"),
            );
            assert!(
                matches!(answer, Ok(Eligibility::Unsupported(_))),
                "{answer:?}"
            );
            assert!(!containment.is_cancelled());
        }

        /// The public producer flow, in a child process whose PATH holds only a fake `cargo`:
        /// `steps` finds two missing programs, the re-check before the first install gets an
        /// unexpected exit, and then the second install starts neither a query nor an installer.
        #[test]
        #[ignore = "run in a child process by the tests below"]
        fn producer_flow() {
            let Some(dir) = std::env::var_os("LOADOUT_TEST_PRODUCER") else {
                return;
            };
            if std::env::var_os("LOADOUT_TEST_PRODUCER_HOLD").is_some() {
                // Held live for a control, doing nothing else; it ends by itself as a fallback.
                std::thread::sleep(Duration::from_secs(60));
                return;
            }
            let dir = PathBuf::from(dir);
            let os = Os::current().unwrap();
            let home = dir.join("home");
            let context =
                Context::resolve(os, |name| std::env::var_os(name), Some(home.clone())).unwrap();
            let context = Context {
                containment: Containment::inherited_for_tests(),
                ..context
            };
            let facts = Facts {
                name: "box".to_owned(),
                os,
                distro: None,
                arch: crate::platform::Arch::X86_64,
                home,
            };
            let programs = BTreeMap::from([
                ("a".to_owned(), super::program(&[(Manager::Cargo, "a")])),
                ("b".to_owned(), super::program(&[(Manager::Cargo, "b")])),
            ]);
            let found = steps(&programs, &facts, &context).unwrap();
            assert!(
                found
                    .iter()
                    .all(|step| matches!(step.state, State::Missing(_))),
                "{found:?}"
            );
            std::fs::write(dir.join("fail"), "").unwrap();
            let first = install(&found[0], false).unwrap_err();
            assert!(matches!(first, Error::Detect { .. }), "{first}");
            assert!(context.containment.is_cancelled());
            std::fs::remove_file(dir.join("fail")).unwrap();
            let calls = std::fs::read_to_string(dir.join("calls")).unwrap();
            let second = install(&found[1], false).unwrap_err();
            assert!(matches!(second, Error::NotStarted { .. }), "{second}");
            assert_eq!(
                std::fs::read_to_string(dir.join("calls")).unwrap(),
                calls,
                "no query or installer may start after the cancel"
            );
            assert!(!dir.join("installed-a").exists() && !dir.join("installed-b").exists());
        }

        /// A fake `cargo` in its own `bin`: it records each call in `calls`, answers the
        /// installed-list query (exit 2 while `fail` exists), and an install writes
        /// `installed-<name>`.
        fn fake_cargo(dir: &Path) -> PathBuf {
            let bin = dir.join("bin");
            std::fs::create_dir(&bin).unwrap();
            let root = dir.display().to_string();
            script(
                &bin,
                "cargo",
                &format!(
                    "echo \"$*\" >> '{root}/calls'\n\
                     if [ \"$1\" = install ] && [ \"$2\" = --list ]; then\n\
                     if [ -e '{root}/fail' ]; then exit 2; fi\nexit 0\nfi\n\
                     if [ \"$1\" = install ]; then : > \"{root}/installed-$2\"; exit 0; fi\nexit 64\n"
                ),
            );
            bin
        }

        /// `producer_flow` in a child process with only `bin` on its PATH, owned from the
        /// moment it exists. `hold` keeps it live and idle instead, for the wrapper's own
        /// failure controls.
        fn producer(dir: &Path, bin: &Path, hold: bool) -> Owned {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "programs::tests::cancellation::producer_flow",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env_clear()
                .env("PATH", bin)
                .env("LOADOUT_TEST_PRODUCER", dir)
                .stdin(std::process::Stdio::null());
            if hold {
                command.env("LOADOUT_TEST_PRODUCER_HOLD", "1");
            }
            Owned::new(command.spawn().unwrap())
        }

        #[test]
        fn a_semantic_failure_stops_later_queries_and_installers_through_the_public_api() {
            let dir = tempfile::tempdir().unwrap();
            let bin = fake_cargo(dir.path());
            let mut flow = producer(dir.path(), &bin, false);
            let status = flow
                .finish_within(Duration::from_secs(60), Duration::from_secs(10))
                .unwrap_or_else(|detail| panic!("the producer flow did not finish: {detail}"));
            assert!(status.success(), "the producer flow failed: {status}");
        }

        /// The wrapper's failure paths, on a producer held live: a failure right after the
        /// spawn, a first observation that fails before the OS is asked (the child stays
        /// pinned), and a termination that fails. Each stops the child or reports why it could
        /// not, within a bound, keeping the first error. The guard's own controls cover a real
        /// OS error, which on Unix leaves the child alone.
        #[test]
        fn the_producer_wrapper_stops_its_child_on_every_failure() {
            let dir = tempfile::tempdir().unwrap();
            let bin = fake_cargo(dir.path());

            let record = Arc::new(Mutex::new(Record::default()));
            let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _flow = producer(dir.path(), &bin, true).recording(&record);
                panic!("injected: failure right after the spawn");
            }));
            assert!(failed.is_err());
            assert!(
                record.lock().unwrap().exit.is_some(),
                "the held producer was not stopped"
            );

            let mut flow = producer(dir.path(), &bin, true);
            flow.inject(Faults {
                before_poll: 1,
                ..Faults::default()
            });
            let detail = flow
                .finish_within(Duration::from_secs(60), Duration::from_secs(10))
                .unwrap_err();
            assert!(
                detail.contains("cannot observe the child: injected"),
                "{detail}"
            );
            assert!(detail.ends_with("it was stopped"), "{detail}");
            assert_eq!(flow.kills(), 1);

            let mut flow = producer(dir.path(), &bin, true);
            flow.inject(Faults {
                kill: true,
                ..Faults::default()
            });
            let started = Instant::now();
            let detail = flow
                .finish_within(Duration::from_millis(500), Duration::from_secs(1))
                .unwrap_err();
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{:?}",
                started.elapsed()
            );
            assert!(detail.contains("did not exit within"), "{detail}");
            assert!(detail.contains("could not be killed"), "{detail}");
            assert!(detail.contains("has not exited"), "{detail}");
            assert!(
                flow.observe().unwrap().is_none(),
                "the held producer still runs"
            );
            flow.inject(Faults::default());
            assert_eq!(flow.stop(Duration::from_secs(10)), Ok(()));
        }

        #[test]
        fn the_foreground_has_no_run_to_cancel() {
            let dir = tempfile::tempdir().unwrap();
            let broken = package(Manager::Cargo, manager(dir.path(), "broken", "", 2));
            let foreground = Containment::OwnGroup;
            assert!(observe(&broken, &foreground).is_err());
            assert!(!foreground.is_cancelled());
            let again = package(Manager::Cargo, manager(dir.path(), "again", "", 0));
            assert!(observe(&again, &foreground).is_ok());
            assert!(dir.path().join("again.calls").exists());
        }
    }

    #[test]
    fn install_rechecks_a_step_against_path_before_anything_runs() {
        let step = Step {
            program: "ripgrep".to_owned(),
            state: State::Missing(Missing {
                package: Package {
                    manager: Manager::Apt,
                    id: "ripgrep".to_owned(),
                    executable: "/nonexistent/apt-get".into(),
                },
                scope: Scope::System,
                containment: Containment::OwnGroup,
            }),
        };
        assert!(step.needs_elevation());
        let error = install(&step, false).unwrap_err();
        assert!(matches!(error, Error::Changed { .. }), "{error}");

        let mut wrong_scope = step.clone();
        if let State::Missing(missing) = &mut wrong_scope.state {
            missing.scope = Scope::User;
        }
        let error = install(&wrong_scope, true).unwrap_err();
        assert_eq!(
            error.to_string(),
            "programs.ripgrep: the machine changed after the plan (apt never installs in user scope), so loadout installed nothing; plan again"
        );

        let mut option = step;
        if let State::Missing(missing) = &mut option.state {
            missing.package.id = "-oDebug=1".to_owned();
        }
        assert!(matches!(install(&option, true), Err(Error::Package { .. })));
    }
}
