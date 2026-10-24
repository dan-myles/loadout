use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use crate::context::{self, Context};
use crate::diagnostic::{Check, CheckStatus};
use crate::machine::Facts;
use crate::manifest::Manager;
use crate::platform::Os;
use crate::resolve::Program;

mod detect;
mod manager;
mod process;

use detect::Eligibility;
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

/// A package `steps` found missing, and the scope its install needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Missing {
    package: Package,
    scope: Scope,
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
                state(id, program, facts.os)?
            };
            Ok(Step {
                program: id.clone(),
                state,
            })
        })
        .collect()
}

fn state(id: &str, program: &Program, os: Os) -> Result<State, Error> {
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
        match observe(&package).map_err(|detail| detect_error(id, &package, detail))? {
            Observation::Installed => State::Installed(package),
            Observation::Missing(scope) => State::Missing(Missing { package, scope }),
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
/// missing package, whether and in which scope it can be installed.
fn observe(package: &Package) -> Result<Observation, String> {
    if detect::installed(package.manager, &package.executable, &package.id)? {
        return Ok(Observation::Installed);
    }
    Ok(
        match detect::eligibility(package.manager, &package.executable, &package.id)? {
            Eligibility::Scope(scope) => Observation::Missing(scope),
            Eligibility::Unsupported(reason) => Observation::Unsupported(reason),
        },
    )
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
    match observe(package).map_err(|detail| detect_error(program, package, detail))? {
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
    let output = process::install(&invocation).map_err(|source| Error::Spawn {
        program: program.to_owned(),
        command: command.clone(),
        source,
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
    match detect::installed(manager, &package.executable, &package.id) {
        Ok(true) => Ok(Installed::Now),
        Ok(false) => Err(Error::NotInstalled {
            program: program.to_owned(),
            manager: manager.id(),
            package: package.id.clone(),
        }),
        Err(detail) => Err(detect_error(program, package, detail)),
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
        checks.push(flatpak_remotes());
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

fn flatpak_remotes() -> Check {
    let Some(flatpak) = manager::available(Manager::Flatpak) else {
        return Check::new("flatpak", CheckStatus::Warn, "flatpak is no longer on PATH");
    };
    let invocation = Invocation::new(&flatpak, ["remotes", "--user", "--columns=name"]);
    match process::query(&invocation) {
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
