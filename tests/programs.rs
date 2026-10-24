use std::collections::BTreeMap;
use std::env;
use std::fs;

use loadout::context::Context;
use loadout::machine::Facts;
use loadout::manifest::ProgramPatch;
use loadout::platform::{Arch, Os};
use loadout::programs::{self, State, Step};
use loadout::resolve::Program;
use serde::Deserialize;
use serde_json::{Value, json};

const PROBE: &str = "LOADOUT_PROGRAMS_PROBE";
const RESULT: &str = "PROBE-RESULT ";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    os: Os,
    #[serde(default)]
    install: bool,
    #[serde(default)]
    elevated: bool,
    programs: BTreeMap<String, ProgramPatch>,
}

/// Drives the real `programs` API in this process's environment: the other tests re-run it
/// with PATH holding only fake managers, and VM E2E runs it against real ones. It reads a
/// request TOML from `LOADOUT_PROGRAMS_PROBE` and prints one JSON result line.
#[test]
#[ignore = "run with a controlled PATH by the recorder tests and by VM E2E"]
fn probe() {
    let Some(path) = env::var_os(PROBE) else {
        return;
    };
    let request: Request = toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    let context = Context::resolve(request.os, |name| env::var_os(name), env::home_dir()).unwrap();
    let facts = Facts {
        name: "probe".to_owned(),
        os: request.os,
        distro: None,
        arch: Arch::current().unwrap(),
        home: context.home.clone(),
    };
    let programs: BTreeMap<String, Program> = request
        .programs
        .iter()
        .map(|(id, patch)| {
            let packages = patch
                .packages()
                .map(|(manager, package)| (manager, package.to_owned()))
                .collect();
            (id.clone(), Program { packages })
        })
        .collect();
    let result = match programs::steps(&programs, &facts, &context) {
        Err(error) => json!({ "error": error.to_string() }),
        Ok(steps) => Value::Array(
            steps
                .iter()
                .map(|step| {
                    let mut out = describe(step);
                    if request.install && matches!(step.state, State::Missing { .. }) {
                        out["install"] = match programs::install(step, request.elevated) {
                            Ok(()) => json!("ok"),
                            Err(error) => json!(error.to_string()),
                        };
                    }
                    out
                })
                .collect(),
        ),
    };
    println!("{RESULT}{result}");
}

fn describe(step: &Step) -> Value {
    let program = &step.program;
    match &step.state {
        State::Installed(package) => json!({
            "program": program, "state": "installed",
            "manager": package.manager.id(), "package": package.id,
        }),
        State::Missing { package, scope } => json!({
            "program": program, "state": "missing",
            "manager": package.manager.id(), "package": package.id,
            "scope": format!("{scope:?}").to_lowercase(),
            "needs_elevation": step.needs_elevation(),
        }),
        State::Unsupported(reason) => {
            json!({ "program": program, "state": "unsupported", "reason": reason })
        }
        State::Blocked(reason) => {
            json!({ "program": program, "state": "blocked", "reason": reason })
        }
    }
}

#[cfg(unix)]
mod recorded {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;

    const HEADER: &str = r#"#!/bin/sh
{ printf '%s' 'NAME'; for a in "$@"; do printf ' %s' "$a"; done; printf ' | LC_ALL=%s\n' "$LC_ALL"; } >> "$FAKE_LOG"
if [ "$FAKE_FAIL" = 'NAME' ]; then echo 'NAME: database is locked' >&2; exit 3; fi
S="$FAKE_STATE"
mark() { [ -n "$FAKE_LIE" ] || : > "$S/$1"; }
"#;

    /// Recorders that answer like each real manager: the installed-query reads a marker file
    /// that the install command writes.
    const FAKES: &[(&str, &str)] = &[
        (
            "brew",
            r#"case "$1" in
install) printf 'HOMEBREW_NO_INSTALL_UPGRADE=%s\n' "$HOMEBREW_NO_INSTALL_UPGRADE" >> "$FAKE_LOG"
  case "$2" in --cask) mark cask;; *) mark brew;; esac; exit 0;;
info) case "$3" in
  --cask) if [ -e "$S/cask" ]; then v='"1.0"'; else v=null; fi
    printf '{"formulae":[],"casks":[{"token":"%s","installed":%s}]}\n' "$4" "$v";;
  *) if [ -e "$S/brew" ]; then v='[{"version":"1.0"}]'; else v='[]'; fi
    printf '{"formulae":[{"name":"%s","installed":%s}],"casks":[]}\n' "$4" "$v";;
  esac; exit 0;;
esac; exit 64"#,
        ),
        (
            "apt-get",
            r#"case "$1" in install) printf 'DEBIAN_FRONTEND=%s\n' "$DEBIAN_FRONTEND" >> "$FAKE_LOG"; mark apt; exit 0;; esac; exit 64"#,
        ),
        (
            "dpkg-query",
            r#"if [ -e "$S/apt" ]; then printf 'installed\n'; exit 0; fi
echo "dpkg-query: no packages found matching $4" >&2; exit 1"#,
        ),
        (
            "dnf",
            r#"case "$1" in install) mark dnf; exit 0;; esac; exit 64"#,
        ),
        (
            "rpm",
            r#"if [ -e "$S/dnf" ]; then echo "$2-1.0-1.x86_64"; exit 0; fi
echo "package $2 is not installed"; exit 1"#,
        ),
        (
            "pacman",
            r#"case "$1" in -S) mark pacman; exit 0;;
-Q) if [ -e "$S/pacman" ]; then echo "$2 1.0-1"; exit 0; fi; echo "error: package '$2' was not found" >&2; exit 1;;
esac; exit 64"#,
        ),
        (
            "zypper",
            r#"case " $* " in *" install "*) mark zypper; exit 0;;
*" search "*) if [ -e "$S/zypper" ]; then echo 'i | pkg-zypper | x | package'; exit 0; fi; echo 'No matching items found.'; exit 104;;
esac; exit 64"#,
        ),
        (
            "flatpak",
            r#"case "$1" in install) mark flatpak; exit 0;;
info) if [ -e "$S/flatpak" ]; then echo "Ref: app/$2/x86_64/stable"; exit 0; fi; echo "error: $2 not installed" >&2; exit 1;;
remotes) echo flathub; exit 0;;
esac; exit 64"#,
        ),
        (
            "snap",
            r#"case "$1" in install) mark snap; exit 0;;
list) if [ -e "$S/snap" ]; then printf 'Name Version\n%s 1.0\n' "$2"; exit 0; fi; echo 'error: no matching snaps installed' >&2; exit 1;;
esac; exit 64"#,
        ),
        (
            "winget",
            r#"case "$1" in list|show) exit 0;; install) mark winget; exit 0;; esac; exit 64"#,
        ),
        (
            "scoop",
            r#"case "$1" in install) mark scoop; exit 0;;
prefix) if [ -e "$S/scoop" ]; then echo "/h/scoop/apps/$2/current"; exit 0; fi; echo "Could not find app path for '$2'."; exit 1;;
esac; exit 64"#,
        ),
        (
            "choco",
            r#"case "$1" in install) mark choco; exit 0;;
list) if [ -e "$S/choco" ]; then echo "$3|1.0.0"; fi; exit 0;;
esac; exit 64"#,
        ),
        (
            "npm",
            r#"case "$1" in install) mark npm; exit 0;;
ls) if [ -e "$S/npm" ]; then printf '{"name":"lib","dependencies":{"%s":{"version":"1.0.0"}}}\n' "$5"; exit 0; fi
  printf '{\n  "name": "lib"\n}\n'; exit 1;;
esac; exit 64"#,
        ),
        (
            "pnpm",
            r#"case "$1" in add) mark pnpm; exit 0;;
ls) if [ -e "$S/pnpm" ]; then printf '[{"path":"/g","dependencies":{"%s":{"version":"1.0.0"}}}]\n' "$5"; else printf '[]\n'; fi; exit 0;;
esac; exit 64"#,
        ),
        (
            "bun",
            r#"case "$1" in add) mark bun; exit 0;;
pm) printf '/h/.bun/install/global node_modules (1)\n'; if [ -e "$S/bun" ]; then printf '└── pkg-bun@1.0.0\n'; fi; exit 0;;
esac; exit 64"#,
        ),
        (
            "cargo",
            r#"case "$1 $2" in "install --list") printf 'pkg-cargo-extra v1.0.0:\n    extra\n'
  if [ -e "$S/cargo" ]; then printf 'pkg-cargo v1.0.0:\n    pkg-cargo\n'; fi; exit 0;;
install*) mark cargo; exit 0;;
esac; exit 64"#,
        ),
        (
            "uv",
            r#"case "$1 $2" in "tool install") mark uv; exit 0;;
"tool list") if [ -e "$S/uv" ]; then printf 'pkg-uv v1.0\n- pkg-uv\n'; else echo 'No tools installed' >&2; fi; exit 0;;
esac; exit 64"#,
        ),
        (
            "pipx",
            r#"case "$1" in install) mark pipx; exit 0;;
list) if [ -e "$S/pipx" ]; then printf '{"pipx_spec_version":"0.1","venvs":{"pkg-pipx":{}}}\n'
  else printf '{"pipx_spec_version":"0.1","venvs":{}}\n'; fi; exit 0;;
esac; exit 64"#,
        ),
        (
            "go",
            r#"case "$1" in env) printf '%s\n\n' "$S/gobin"; exit 0;;
install) [ -n "$FAKE_LIE" ] || : > "$S/gobin/hello"; exit 0;;
version) printf '%s: go1.22\n\tpath\texample.com/tools/hello\n' "$3"; exit 0;;
esac; exit 64"#,
        ),
        ("sudo", r#"[ "$1" = -- ] && shift; exec "$@""#),
        (
            "env",
            r#"while [ $# -gt 0 ]; do case "$1" in *=*) export "$1"; shift;; *) break;; esac; done; exec "$@""#,
        ),
    ];

    struct Bench {
        dir: tempfile::TempDir,
    }

    struct Run {
        result: Value,
        log: Vec<String>,
    }

    impl Bench {
        /// A fake PATH holding only the named tools, so no real manager can ever be reached.
        fn new(tools: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            for sub in ["bin", "state/gobin", "home"] {
                fs::create_dir_all(dir.path().join(sub)).unwrap();
            }
            for tool in tools {
                let body = FAKES
                    .iter()
                    .find(|(name, _)| name == tool)
                    .unwrap_or_else(|| panic!("no fake {tool}"))
                    .1;
                let path = dir.path().join("bin").join(tool);
                fs::write(&path, format!("{}{body}\n", HEADER.replace("NAME", tool))).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self { dir }
        }

        fn bin(&self) -> PathBuf {
            self.dir.path().join("bin")
        }

        fn command(&self, program: &Path) -> Command {
            let mut command = Command::new(program);
            command
                .env_clear()
                .env("PATH", self.bin())
                .env("HOME", self.dir.path().join("home"))
                .env("FAKE_LOG", self.dir.path().join("log"))
                .env("FAKE_STATE", self.dir.path().join("state"));
            command
        }

        fn probe(&self, request: &str, extra: &[(&str, &str)]) -> Run {
            let input = self.dir.path().join("request.toml");
            fs::write(&input, request).unwrap();
            let _ = fs::remove_file(self.dir.path().join("log"));
            let output = self
                .command(&env::current_exe().unwrap())
                .args([
                    "--exact",
                    "probe",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(PROBE, &input)
                .envs(extra.iter().copied())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let line = stdout
                .lines()
                .find_map(|line| line.split_once(RESULT).map(|(_, result)| result))
                .unwrap_or_else(|| {
                    panic!(
                        "probe printed no result\n{stdout}\n{}",
                        String::from_utf8_lossy(&output.stderr)
                    )
                });
            Run {
                result: serde_json::from_str(line).unwrap(),
                log: self.log(),
            }
        }

        fn log(&self) -> Vec<String> {
            fs::read_to_string(self.dir.path().join("log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Run {
        fn commands(&self, bin: &Path) -> Vec<String> {
            let prefix = format!("{}/", bin.display());
            self.log
                .iter()
                .filter_map(|line| line.split_once(" | LC_ALL="))
                .map(|(command, _)| command.replace(&prefix, "<bin>/"))
                .collect()
        }

        fn step(&self, program: &str) -> &Value {
            self.result
                .as_array()
                .unwrap_or_else(|| panic!("no steps: {}", self.result))
                .iter()
                .find(|step| step["program"] == program)
                .unwrap_or_else(|| panic!("no step {program}: {}", self.result))
        }
    }

    fn running_as_root() -> bool {
        use std::os::unix::fs::MetadataExt;
        cfg!(target_os = "linux")
            && fs::metadata("/proc/self").is_ok_and(|metadata| metadata.uid() == 0)
    }

    fn request(os: &str, install: bool, elevated: bool, programs: &[(&str, &str, &str)]) -> String {
        use std::fmt::Write;
        let mut text = format!("os = \"{os}\"\ninstall = {install}\nelevated = {elevated}\n");
        for (id, manager, package) in programs {
            writeln!(text, "[programs.{id}]\n{manager} = \"{package}\"").unwrap();
        }
        text
    }

    fn fake_tools() -> Vec<&'static str> {
        FAKES.iter().map(|(name, _)| *name).collect()
    }

    struct Case {
        os: &'static str,
        manager: &'static str,
        package: &'static str,
        scope: &'static str,
        query: &'static str,
        install: &'static str,
    }

    const CASES: &[Case] = &[
        Case {
            os: "macos",
            manager: "brew",
            package: "pkg-brew",
            scope: "user",
            query: "brew info --json=v2 --formula pkg-brew",
            install: "brew install --formula pkg-brew",
        },
        Case {
            os: "macos",
            manager: "cask",
            package: "pkg-cask",
            scope: "user",
            query: "brew info --json=v2 --cask pkg-cask",
            install: "brew install --cask pkg-cask",
        },
        Case {
            os: "linux",
            manager: "apt",
            package: "pkg-apt",
            scope: "system",
            query: "dpkg-query -W -f ${db:Status-Status}  pkg-apt",
            install: "apt-get install -y --no-upgrade pkg-apt",
        },
        Case {
            os: "linux",
            manager: "dnf",
            package: "pkg-dnf",
            scope: "system",
            query: "rpm -q pkg-dnf",
            install: "dnf install -y pkg-dnf",
        },
        Case {
            os: "linux",
            manager: "pacman",
            package: "pkg-pacman",
            scope: "system",
            query: "pacman -Q pkg-pacman",
            install: "pacman -S --needed --noconfirm pkg-pacman",
        },
        Case {
            os: "linux",
            manager: "zypper",
            package: "pkg-zypper",
            scope: "system",
            query: "zypper --non-interactive --no-refresh search --installed-only --match-exact --type package pkg-zypper",
            install: "zypper --non-interactive install pkg-zypper",
        },
        Case {
            os: "linux",
            manager: "flatpak",
            package: "org.example.Pkg",
            scope: "user",
            query: "flatpak info org.example.Pkg",
            install: "flatpak install --user --noninteractive org.example.Pkg",
        },
        Case {
            os: "linux",
            manager: "snap",
            package: "pkg-snap",
            scope: "system",
            query: "snap list pkg-snap",
            install: "snap install pkg-snap",
        },
        Case {
            os: "linux",
            manager: "brew",
            package: "pkg-brew",
            scope: "user",
            query: "brew info --json=v2 --formula pkg-brew",
            install: "brew install --formula pkg-brew",
        },
        Case {
            os: "windows",
            manager: "scoop",
            package: "extras/pkg-scoop",
            scope: "user",
            query: "scoop prefix pkg-scoop",
            install: "scoop install extras/pkg-scoop",
        },
        Case {
            os: "windows",
            manager: "choco",
            package: "pkg-choco",
            scope: "system",
            query: "choco list --exact pkg-choco --limit-output",
            install: "choco install pkg-choco -y --no-progress",
        },
        Case {
            os: "linux",
            manager: "cargo",
            package: "pkg-cargo",
            scope: "user",
            query: "cargo install --list",
            install: "cargo install pkg-cargo",
        },
        Case {
            os: "linux",
            manager: "uv",
            package: "pkg-uv",
            scope: "user",
            query: "uv tool list",
            install: "uv tool install pkg-uv",
        },
        Case {
            os: "linux",
            manager: "pipx",
            package: "pkg-pipx",
            scope: "user",
            query: "pipx list --json",
            install: "pipx install pkg-pipx",
        },
        Case {
            os: "linux",
            manager: "npm",
            package: "@scope/pkg-npm",
            scope: "user",
            query: "npm ls --global --depth=0 --json @scope/pkg-npm",
            install: "npm install --global @scope/pkg-npm",
        },
        Case {
            os: "linux",
            manager: "pnpm",
            package: "pkg-pnpm",
            scope: "user",
            query: "pnpm ls --global --depth=0 --json pkg-pnpm",
            install: "pnpm add --global pkg-pnpm",
        },
        Case {
            os: "linux",
            manager: "bun",
            package: "pkg-bun",
            scope: "user",
            query: "bun pm ls --global",
            install: "bun add --global pkg-bun",
        },
        Case {
            os: "linux",
            manager: "go",
            package: "example.com/tools/hello",
            scope: "user",
            query: "go env GOBIN GOPATH",
            install: "go install example.com/tools/hello@latest",
        },
    ];

    fn single(case: &Case) -> String {
        request(case.os, true, true, &[("tool", case.manager, case.package)])
    }

    #[test]
    fn every_manager_installs_only_a_missing_package_with_exact_arguments() {
        for case in CASES {
            let bench = Bench::new(&fake_tools());
            let first = bench.probe(&single(case), &[]);
            let step = first.step("tool");
            let label = format!("{} {}: {}", case.os, case.manager, first.result);
            assert_eq!(step["state"], "missing", "{label}");
            assert_eq!(step["manager"], case.manager, "{label}");
            assert_eq!(step["package"], case.package, "{label}");
            assert_eq!(step["scope"], case.scope, "{label}");
            assert_eq!(step["install"], "ok", "{label}");

            let commands = first.commands(&bench.bin());
            assert_eq!(commands[0], case.query, "{label}");
            let installs: Vec<&String> = commands
                .iter()
                .filter(|command| command.contains(case.install))
                .collect();
            let install = case.install.to_owned();
            let expected = if case.scope != "system" || running_as_root() {
                vec![install]
            } else if case.manager == "apt" {
                let env = format!("env DEBIAN_FRONTEND=noninteractive <bin>/{install}");
                vec![format!("sudo -- {env}"), env, install]
            } else {
                vec![format!("sudo -- <bin>/{install}"), install]
            };
            assert_eq!(
                installs,
                expected.iter().collect::<Vec<_>>(),
                "{label}\n{commands:#?}"
            );
            let last_install = commands
                .iter()
                .rposition(|command| command.contains(case.install))
                .unwrap();
            assert!(
                commands[last_install + 1..].contains(&commands[0]),
                "the install is verified with the installed-query: {label}\n{commands:#?}"
            );
            assert!(
                first
                    .log
                    .iter()
                    .filter(|line| line.contains(" | LC_ALL="))
                    .all(|line| line.ends_with(" | LC_ALL=C")),
                "every manager runs in the C locale: {label}\n{:#?}",
                first.log
            );

            let second = bench.probe(&single(case), &[]);
            assert_eq!(second.step("tool")["state"], "installed", "{label}");
            let commands = second.commands(&bench.bin());
            assert!(
                !commands
                    .iter()
                    .any(|command| command.contains(case.install)),
                "an installed program is never installed again: {label}\n{commands:#?}"
            );
        }
    }

    #[test]
    fn managers_get_their_noninteractive_environment() {
        let bench = Bench::new(&fake_tools());
        bench.probe(
            &request("linux", true, true, &[("rg", "apt", "ripgrep")]),
            &[],
        );
        assert!(
            bench
                .log()
                .contains(&"DEBIAN_FRONTEND=noninteractive".to_owned())
        );
        bench.probe(
            &request("macos", true, false, &[("rg", "brew", "ripgrep")]),
            &[],
        );
        assert!(
            bench
                .log()
                .contains(&"HOMEBREW_NO_INSTALL_UPGRADE=1".to_owned())
        );
    }

    #[test]
    fn winget_reports_an_installed_package_from_its_list_query() {
        let bench = Bench::new(&fake_tools());
        let run = bench.probe(
            &request(
                "windows",
                true,
                true,
                &[("rg", "winget", "BurntSushi.ripgrep.MSVC")],
            ),
            &[],
        );
        assert_eq!(run.step("rg")["state"], "installed", "{}", run.result);
        assert_eq!(
            run.commands(&bench.bin()),
            [
                "winget list --id BurntSushi.ripgrep.MSVC --exact --accept-source-agreements --disable-interactivity"
            ]
        );
    }

    #[test]
    fn a_failed_installed_query_is_an_error_and_never_reads_as_installed() {
        for case in CASES {
            let failing = match case.manager {
                "apt" => "dpkg-query",
                "dnf" => "rpm",
                "cask" => "brew",
                other => other,
            };
            let bench = Bench::new(&fake_tools());
            let run = bench.probe(&single(case), &[("FAKE_FAIL", failing)]);
            let error = run.result["error"]
                .as_str()
                .unwrap_or_else(|| panic!("{} must fail, got {}", case.manager, run.result));
            assert!(
                error.contains("cannot tell whether") && error.contains("database is locked"),
                "{error}"
            );
            assert!(
                !run.commands(&bench.bin())
                    .iter()
                    .any(|command| command.contains(case.install)),
                "nothing is installed after a failed query: {:#?}",
                run.log
            );
        }
        let bench = Bench::new(&fake_tools());
        let run = bench.probe(
            &request("windows", false, false, &[("rg", "winget", "A.B")]),
            &[("FAKE_FAIL", "winget")],
        );
        assert!(
            run.result["error"]
                .as_str()
                .unwrap()
                .contains("exit code 3"),
            "{}",
            run.result
        );
    }

    #[test]
    fn a_success_that_installs_nothing_is_an_error() {
        let bench = Bench::new(&fake_tools());
        let run = bench.probe(
            &request("linux", true, false, &[("cow", "pipx", "pycowsay")]),
            &[("FAKE_LIE", "1")],
        );
        assert_eq!(
            run.step("cow")["install"],
            "programs.cow: pipx reported success, but \"pycowsay\" is still not installed"
        );
    }

    #[test]
    fn a_failed_install_reports_the_command_status_and_output() {
        let bench = Bench::new(&["pipx"]);
        let script = bench.bin().join("pipx");
        let text = fs::read_to_string(&script).unwrap().replace(
            "install) mark pipx; exit 0;;",
            "install) echo 'No matching distribution found for nope' >&2; exit 1;;",
        );
        fs::write(&script, text).unwrap();
        let run = bench.probe(
            &request("linux", true, false, &[("nope", "pipx", "nope")]),
            &[],
        );
        let error = run.step("nope")["install"].as_str().unwrap().to_owned();
        let pipx = bench.bin().join("pipx");
        assert_eq!(
            error,
            format!(
                "programs.nope: {} install nope failed with exit code 1:\nNo matching distribution found for nope",
                pipx.display()
            )
        );
    }

    #[test]
    fn system_scope_installs_need_elevation_and_never_reach_the_manager_without_it() {
        for (os, manager, package) in [
            ("linux", "apt", "ripgrep"),
            ("linux", "dnf", "ripgrep"),
            ("linux", "pacman", "ripgrep"),
            ("linux", "zypper", "ripgrep"),
            ("linux", "snap", "ripgrep"),
            ("windows", "choco", "ripgrep"),
        ] {
            let bench = Bench::new(&fake_tools());
            let run = bench.probe(&request(os, true, false, &[("rg", manager, package)]), &[]);
            let step = run.step("rg");
            assert_eq!(step["needs_elevation"], true, "{}", run.result);
            assert_eq!(
                step["install"],
                format!(
                    "programs.rg: {manager} installs \"ripgrep\" system-wide, which needs --elevated"
                )
            );
            let commands = run.commands(&bench.bin());
            assert_eq!(
                commands.len(),
                1,
                "only the installed-query ran: {commands:#?}"
            );
        }
    }

    #[test]
    fn elevation_without_sudo_is_an_error_not_a_direct_run() {
        if running_as_root() {
            return;
        }
        let tools: Vec<&str> = fake_tools()
            .into_iter()
            .filter(|tool| *tool != "sudo")
            .collect();
        let bench = Bench::new(&tools);
        let run = bench.probe(
            &request("linux", true, true, &[("rg", "apt", "ripgrep")]),
            &[],
        );
        assert_eq!(
            run.step("rg")["install"],
            "programs.rg: installing \"ripgrep\" with apt needs sudo, which is not on PATH"
        );
        assert!(
            !run.log
                .iter()
                .any(|line| line.starts_with("apt-get install"))
        );
    }

    #[test]
    fn the_first_declared_manager_on_path_wins_and_none_means_unsupported() {
        let bench = Bench::new(&["flatpak", "cargo", "pipx"]);
        let text = "os = \"linux\"\n[programs.rg]\napt = \"ripgrep\"\ncargo = \"ripgrep\"\nflatpak = \"org.x.Rg\"\n\
                    [programs.mac]\ncask = \"ghostty\"\n[programs.tool]\ndnf = \"t\"\nuv = \"t\"\n";
        let run = bench.probe(text, &[]);
        assert_eq!(run.step("rg")["manager"], "flatpak", "{}", run.result);
        assert_eq!(
            run.step("mac")["reason"],
            "declares no manager loadout uses on linux (declared: cask)"
        );
        assert_eq!(
            run.step("tool")["reason"],
            "none of its managers for linux is on PATH: dnf (dnf, rpm); uv (uv)"
        );
        assert_eq!(run.commands(&bench.bin()), ["flatpak info org.x.Rg"]);
    }

    #[test]
    fn a_package_that_could_be_an_option_is_refused_before_any_manager_runs() {
        let bench = Bench::new(&fake_tools());
        let run = bench.probe(
            &request("linux", true, true, &[("evil", "npm", "--prefix=/etc")]),
            &[],
        );
        assert_eq!(
            run.result["error"],
            "programs.evil: npm package \"--prefix=/etc\" starts with '-', which the manager would read as an option"
        );
        assert!(run.log.is_empty(), "{:#?}", run.log);
    }

    #[test]
    fn isolation_blocks_every_program_and_runs_no_manager() {
        let bench = Bench::new(&fake_tools());
        let home = bench.dir.path().join("home");
        let run = bench.probe(
            &request(
                "linux",
                true,
                true,
                &[("rg", "apt", "ripgrep"), ("cow", "pipx", "pycowsay")],
            ),
            &[("LOADOUT_HOME", home.to_str().unwrap())],
        );
        for program in ["rg", "cow"] {
            assert_eq!(run.step(program)["state"], "blocked");
            assert_eq!(
                run.step(program)["reason"],
                "programs are not installed under LOADOUT_HOME isolation"
            );
        }
        assert!(run.log.is_empty(), "{:#?}", run.log);
    }

    fn doctor(bench: &Bench, extra: &[(&str, &str)]) -> Vec<Value> {
        let output = bench
            .command(Path::new(env!("CARGO_BIN_EXE_loadout")))
            .args(["doctor", "--json"])
            .env("LOADOUT_STATE_DIR", bench.dir.path().join("loadout-state"))
            .envs(extra.iter().copied())
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        report["checks"].as_array().unwrap().clone()
    }

    fn check<'a>(checks: &'a [Value], name: &str) -> Option<&'a Value> {
        checks.iter().find(|check| check["name"] == name)
    }

    #[test]
    fn doctor_names_available_and_missing_managers_and_what_elevation_needs() {
        let bench = Bench::new(&["apt-get", "dpkg-query", "flatpak", "pipx", "npm", "sudo"]);
        let checks = doctor(&bench, &[]);
        let programs = check(&checks, "programs").unwrap();
        assert_eq!(programs["status"], "info");
        assert_eq!(
            programs["detail"],
            "per user: flatpak, pipx, npm; with --elevated: apt; \
             not on PATH: dnf, pacman, zypper, snap, brew, cargo, uv, pnpm, bun, go"
        );
        if !running_as_root() {
            assert_eq!(
                check(&checks, "elevation").unwrap()["detail"],
                format!(
                    "--elevated runs apt through {}",
                    bench.bin().join("sudo").display()
                )
            );
        }
        assert_eq!(
            check(&checks, "flatpak").unwrap()["detail"],
            "per-user remotes: flathub"
        );
        assert_eq!(
            bench
                .log()
                .iter()
                .map(|line| line.split(" | ").next().unwrap())
                .collect::<Vec<_>>(),
            ["flatpak remotes --user --columns=name"]
        );
    }

    #[test]
    fn doctor_warns_without_any_manager_or_sudo() {
        let bench = Bench::new(&["apt-get", "dpkg-query"]);
        let checks = doctor(&bench, &[]);
        if !running_as_root() {
            let elevation = check(&checks, "elevation").unwrap();
            assert_eq!(elevation["status"], "warn");
            assert_eq!(
                elevation["detail"],
                "sudo is not on PATH, so --elevated cannot install with apt"
            );
        }
        let empty = Bench::new(&[]);
        let checks = doctor(&empty, &[]);
        assert_eq!(check(&checks, "programs").unwrap()["status"], "warn");
        assert!(check(&checks, "elevation").is_none());
    }

    #[test]
    fn doctor_under_isolation_runs_no_manager() {
        let bench = Bench::new(&["flatpak", "pipx"]);
        let home = bench.dir.path().join("home");
        let checks = doctor(&bench, &[("LOADOUT_HOME", home.to_str().unwrap())]);
        assert_eq!(
            check(&checks, "programs").unwrap()["detail"],
            "per user: flatpak, pipx; not on PATH: apt, dnf, pacman, zypper, snap, brew, cargo, uv, npm, pnpm, bun, go; \
             programs are not installed under LOADOUT_HOME isolation"
        );
        assert!(check(&checks, "flatpak").is_none());
        assert!(bench.log().is_empty(), "{:#?}", bench.log());
    }
}
