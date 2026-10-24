use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::context;
use crate::manifest::Manager;
use crate::platform::Os;

use super::Scope;

const LANGUAGE: [Manager; 7] = [
    Manager::Cargo,
    Manager::Uv,
    Manager::Pipx,
    Manager::Npm,
    Manager::Pnpm,
    Manager::Bun,
    Manager::Go,
];

pub(super) fn order(os: Os) -> impl Iterator<Item = Manager> {
    let native: &[Manager] = match os {
        Os::Macos => &[Manager::Brew, Manager::Cask],
        Os::Linux => &[
            Manager::Apt,
            Manager::Dnf,
            Manager::Pacman,
            Manager::Zypper,
            Manager::Flatpak,
            Manager::Snap,
            Manager::Brew,
        ],
        Os::Windows => &[Manager::Winget, Manager::Scoop, Manager::Choco],
    };
    native.iter().chain(LANGUAGE.iter()).copied()
}

/// The executables a manager needs on PATH. The first one installs; the others answer the
/// installed-query, because apt-get and dnf can't tell "not installed" from a failure.
pub(super) fn tools(manager: Manager) -> &'static [&'static str] {
    match manager {
        Manager::Brew | Manager::Cask => &["brew"],
        Manager::Apt => &["apt-get", "dpkg-query"],
        Manager::Dnf => &["dnf", "rpm"],
        Manager::Pacman => &["pacman"],
        Manager::Zypper => &["zypper"],
        Manager::Flatpak => &["flatpak"],
        Manager::Snap => &["snap"],
        Manager::Winget => &["winget"],
        Manager::Scoop => &["scoop"],
        Manager::Choco => &["choco"],
        Manager::Npm => &["npm"],
        Manager::Pnpm => &["pnpm"],
        Manager::Bun => &["bun"],
        Manager::Cargo => &["cargo"],
        Manager::Uv => &["uv"],
        Manager::Pipx => &["pipx"],
        Manager::Go => &["go"],
    }
}

pub(super) fn available(manager: Manager) -> Option<PathBuf> {
    let mut paths = tools(manager).iter().map(|tool| context::which(tool));
    let first = paths.next()??;
    paths.all(|path| path.is_some()).then_some(first)
}

/// `None` for winget, whose scope depends on each package's installers.
pub(super) fn scope(manager: Manager) -> Option<Scope> {
    match manager {
        Manager::Apt
        | Manager::Dnf
        | Manager::Pacman
        | Manager::Zypper
        | Manager::Snap
        | Manager::Choco => Some(Scope::System),
        Manager::Winget => None,
        Manager::Brew
        | Manager::Cask
        | Manager::Flatpak
        | Manager::Scoop
        | Manager::Npm
        | Manager::Pnpm
        | Manager::Bun
        | Manager::Cargo
        | Manager::Uv
        | Manager::Pipx
        | Manager::Go => Some(Scope::User),
    }
}

/// Refuses a package id that a manager would read as an option, a version pin, or anything
/// but one exact package, so an install never runs with arguments the loadout didn't mean.
pub(super) fn check_package(manager: Manager, package: &str) -> Result<(), String> {
    if package.is_empty() {
        return Err("is empty".to_owned());
    }
    if package.starts_with('-') {
        return Err("starts with '-', which the manager would read as an option".to_owned());
    }
    if let Some(c) = package
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || "@/._+:~-".contains(*c)))
    {
        return Err(format!(
            "contains {c:?}; package ids use only letters, digits, and @ / . _ + : ~ -"
        ));
    }
    let pinned = match manager {
        Manager::Npm | Manager::Pnpm | Manager::Bun => package[1..].contains('@'),
        Manager::Cargo | Manager::Uv | Manager::Pipx | Manager::Go => package.contains('@'),
        _ => false,
    };
    if pinned {
        return Err("names a version; v1 installs the latest and never pins one".to_owned());
    }
    Ok(())
}

pub(super) struct Invocation {
    pub(super) program: PathBuf,
    pub(super) args: Vec<OsString>,
    pub(super) env: &'static [(&'static str, &'static str)],
}

impl Invocation {
    pub(super) fn new<I, S>(program: &Path, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: program.to_path_buf(),
            args: args.into_iter().map(Into::into).collect(),
            env: &[],
        }
    }

    pub(super) fn display(&self) -> String {
        std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(super) fn install(manager: Manager, program: &Path, package: &str, scope: Scope) -> Invocation {
    let args: Vec<&str> = match manager {
        Manager::Brew => vec!["install", "--formula", package],
        Manager::Cask => vec!["install", "--cask", package],
        Manager::Apt => vec!["install", "-y", "--no-upgrade", package],
        Manager::Dnf => vec!["install", "-y", package],
        Manager::Pacman => vec!["-S", "--needed", "--noconfirm", package],
        Manager::Zypper => vec!["--non-interactive", "install", package],
        Manager::Flatpak => vec!["install", "--user", "--noninteractive", package],
        Manager::Winget => vec![
            "install",
            "--id",
            package,
            "--exact",
            "--silent",
            "--accept-package-agreements",
            "--accept-source-agreements",
            "--disable-interactivity",
            "--scope",
            match scope {
                Scope::User => "user",
                Scope::System => "machine",
            },
        ],
        Manager::Choco => vec!["install", package, "-y", "--no-progress"],
        Manager::Npm => vec!["install", "--global", package],
        Manager::Pnpm | Manager::Bun => vec!["add", "--global", package],
        Manager::Snap | Manager::Scoop | Manager::Cargo | Manager::Pipx => {
            vec!["install", package]
        }
        Manager::Uv => vec!["tool", "install", package],
        Manager::Go => {
            return Invocation::new(program, ["install".to_owned(), format!("{package}@latest")]);
        }
    };
    let mut invocation = Invocation::new(program, args);
    invocation.env = match manager {
        Manager::Brew | Manager::Cask => &[("HOMEBREW_NO_INSTALL_UPGRADE", "1")],
        Manager::Apt => &[("DEBIAN_FRONTEND", "noninteractive")],
        _ => &[],
    };
    invocation
}

const WINGET_ALREADY_INSTALLED: u32 = 0x8A15_0061;

pub(super) fn install_succeeded(manager: Manager, code: i32) -> bool {
    #[expect(
        clippy::cast_sign_loss,
        reason = "winget reports HRESULTs as negative i32 codes"
    )]
    let hresult = code as u32;
    match manager {
        Manager::Choco => matches!(code, 0 | 1641 | 3010),
        Manager::Winget => code == 0 || hresult == WINGET_ALREADY_INSTALLED,
        _ => code == 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order_ids(os: Os) -> Vec<&'static str> {
        order(os).map(Manager::id).collect()
    }

    #[test]
    fn each_os_tries_its_own_managers_then_the_language_managers() {
        let language = ["cargo", "uv", "pipx", "npm", "pnpm", "bun", "go"];
        let expect = |native: &[&'static str]| [native, &language].concat();
        assert_eq!(order_ids(Os::Macos), expect(&["brew", "cask"]));
        assert_eq!(
            order_ids(Os::Linux),
            expect(&["apt", "dnf", "pacman", "zypper", "flatpak", "snap", "brew"])
        );
        assert_eq!(
            order_ids(Os::Windows),
            expect(&["winget", "scoop", "choco"])
        );
    }

    #[test]
    fn every_manager_is_reachable_on_some_os() {
        for manager in Manager::ALL {
            assert!(
                Os::ALL
                    .into_iter()
                    .any(|os| order(os).any(|m| m == manager)),
                "{}",
                manager.id()
            );
        }
    }

    #[test]
    fn system_managers_are_exactly_the_ones_that_need_root_or_admin() {
        let system: Vec<_> = Manager::ALL
            .into_iter()
            .filter(|manager| scope(*manager) == Some(Scope::System))
            .map(Manager::id)
            .collect();
        assert_eq!(system, ["apt", "dnf", "pacman", "zypper", "snap", "choco"]);
        assert_eq!(scope(Manager::Winget), None);
    }

    #[test]
    fn package_ids_that_could_become_options_or_pins_are_refused() {
        for (manager, package) in [
            (Manager::Apt, "-oDebug::pkgProblemResolver=1"),
            (Manager::Npm, "--prefix=/etc"),
            (Manager::Brew, ""),
            (Manager::Apt, "rip grep"),
            (Manager::Apt, "lib*"),
            (Manager::Winget, "Foo.Bar;calc"),
            (Manager::Scoop, "a&b"),
            (Manager::Choco, "x\"y"),
            (Manager::Pipx, "httpie[socks]"),
            (Manager::Npm, "cowsay@1.6.0"),
            (Manager::Bun, "@scope/name@2"),
            (Manager::Cargo, "ripgrep@14.0.0"),
            (Manager::Go, "golang.org/x/tools/gopls@v0.16.0"),
            (Manager::Uv, "ruff@0.6"),
        ] {
            assert!(check_package(manager, package).is_err(), "{package:?}");
        }
        for (manager, package) in [
            (Manager::Npm, "@withgraphite/graphite-cli"),
            (Manager::Brew, "python@3.12"),
            (Manager::Cask, "firefox@developer-edition"),
            (Manager::Apt, "libstdc++6:amd64"),
            (Manager::Winget, "BurntSushi.ripgrep.MSVC"),
            (Manager::Scoop, "extras/firefox"),
            (Manager::Flatpak, "org.mozilla.firefox"),
            (Manager::Go, "github.com/rakyll/hey"),
            (Manager::Pipx, "pycowsay"),
        ] {
            assert_eq!(check_package(manager, package), Ok(()), "{package:?}");
        }
    }

    #[test]
    fn install_passes_the_package_as_one_exact_argument() {
        let program = Path::new("/bin/m");
        for manager in Manager::ALL {
            let invocation = install(manager, program, "pkg.name", Scope::User);
            let exact = invocation
                .args
                .iter()
                .filter(|arg| arg.to_string_lossy().contains("pkg.name"))
                .collect::<Vec<_>>();
            assert_eq!(exact.len(), 1, "{}", manager.id());
            let expected = if manager == Manager::Go {
                "pkg.name@latest"
            } else {
                "pkg.name"
            };
            assert_eq!(exact[0], expected, "{}", manager.id());
        }
    }

    #[test]
    fn winget_scope_follows_the_step_and_success_codes_are_per_manager() {
        let program = Path::new("winget");
        let user = install(Manager::Winget, program, "A.B", Scope::User).display();
        let machine = install(Manager::Winget, program, "A.B", Scope::System).display();
        assert!(user.ends_with("--scope user"), "{user}");
        assert!(machine.ends_with("--scope machine"), "{machine}");
        assert!(install_succeeded(Manager::Choco, 3010));
        assert!(!install_succeeded(Manager::Apt, 3010));
        #[expect(clippy::cast_possible_wrap, reason = "HRESULT as an exit code")]
        let already = 0x8A15_0061_u32 as i32;
        assert!(install_succeeded(Manager::Winget, already));
        assert!(!install_succeeded(Manager::Winget, 1));
    }
}
