use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::context;
use crate::manifest::Manager;

use super::Scope;
use super::manager::{self, Invocation};

pub(super) struct Captured {
    pub(super) code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

impl Captured {
    fn success(&self) -> bool {
        self.code == Some(0)
    }

    fn hresult(&self) -> Option<u32> {
        #[expect(
            clippy::cast_sign_loss,
            reason = "winget reports HRESULTs as negative i32 codes"
        )]
        self.code.map(|code| code as u32)
    }

    pub(super) fn status(&self) -> String {
        match self.code {
            #[expect(
                clippy::cast_sign_loss,
                reason = "show HRESULTs the way winget documents them"
            )]
            Some(code) if code < 0 => format!("exit code {:#010X}", code as u32),
            Some(code) => format!("exit code {code}"),
            None => "killed by a signal".to_owned(),
        }
    }

    /// The tail of what the manager printed, for an error the user can act on.
    pub(super) fn tail(&self) -> String {
        let text = format!("{}\n{}", self.stdout.trim(), self.stderr.trim());
        let lines: Vec<&str> = text.trim().lines().collect();
        let tail = lines[lines.len().saturating_sub(20)..].join("\n");
        if tail.is_empty() {
            String::new()
        } else {
            format!(":\n{tail}")
        }
    }

    fn unexpected(&self) -> String {
        format!("unexpected {}{}", self.status(), self.tail())
    }
}

/// Runs a manager with no stdin and messages in the C locale, so the not-installed messages
/// below match and no prompt can wait on input.
pub(super) fn capture(invocation: &Invocation) -> io::Result<Captured> {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .envs(invocation.env.iter().copied())
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    let output = command.output()?;
    Ok(Captured {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn query<I, S>(program: &Path, args: I) -> Result<Captured, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let invocation = Invocation::new(
        program,
        args.into_iter().map(|arg| arg.as_ref().to_os_string()),
    );
    capture(&invocation).map_err(|error| format!("cannot run {}: {error}", invocation.display()))
}

fn helper(tool: &str) -> Result<PathBuf, String> {
    context::which(tool).ok_or_else(|| format!("{tool} is not on PATH"))
}

/// Answers "is this exact package installed?" with the manager's own query. `Ok(true)` needs
/// positive evidence; `Ok(false)` needs the manager's documented not-found answer. Anything
/// else is an error, so a broken manager is never read as "installed".
pub(super) fn installed(manager: Manager, program: &Path, package: &str) -> Result<bool, String> {
    match manager {
        Manager::Brew => brew(
            &query(program, ["info", "--json=v2", "--formula", package])?,
            false,
        ),
        Manager::Cask => brew(
            &query(program, ["info", "--json=v2", "--cask", package])?,
            true,
        ),
        Manager::Apt => dpkg(&query(
            &helper("dpkg-query")?,
            ["-W", "-f", "${db:Status-Status} ", package],
        )?),
        Manager::Dnf => rpm(&query(&helper("rpm")?, ["-q", package])?),
        Manager::Pacman => pacman(&query(program, ["-Q", package])?),
        Manager::Zypper => zypper(&query(
            program,
            [
                "--non-interactive",
                "--no-refresh",
                "search",
                "--installed-only",
                "--match-exact",
                "--type",
                "package",
                package,
            ],
        )?),
        Manager::Flatpak => flatpak(&query(program, ["info", package])?),
        Manager::Snap => snap(&query(program, ["list", package])?),
        Manager::Winget => winget_list(&query(
            program,
            [
                "list",
                "--id",
                package,
                "--exact",
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        )?),
        Manager::Scoop => scoop(&query(program, ["prefix", scoop_app(package)])?),
        Manager::Choco => choco(
            &query(program, ["list", "--exact", package, "--limit-output"])?,
            package,
        ),
        Manager::Npm | Manager::Pnpm => node(
            &query(program, ["ls", "--global", "--depth=0", "--json", package])?,
            package,
        ),
        Manager::Bun => bun(&query(program, ["pm", "ls", "--global"])?, package),
        Manager::Cargo => cargo(&query(program, ["install", "--list"])?, package),
        Manager::Uv => uv(&query(program, ["tool", "list"])?, package),
        Manager::Pipx => pipx(&query(program, ["list", "--json"])?, package),
        Manager::Go => go(program, package),
    }
}

const WINGET_NO_APPLICABLE_INSTALLER: u32 = 0x8A15_0010;
const WINGET_NO_APPLICATIONS_FOUND: u32 = 0x8A15_0014;

/// Winget's scope is per package: the user scope when the package has a user installer,
/// otherwise the machine scope, which needs `--elevated`.
pub(super) fn scope(manager: Manager, program: &Path, package: &str) -> Result<Scope, String> {
    if let Some(scope) = manager::scope(manager) {
        return Ok(scope);
    }
    let shown = query(
        program,
        [
            "show",
            "--id",
            package,
            "--exact",
            "--scope",
            "user",
            "--accept-source-agreements",
            "--disable-interactivity",
        ],
    )?;
    winget_show(&shown)
}

fn winget_show(shown: &Captured) -> Result<Scope, String> {
    match shown.hresult() {
        Some(0) => Ok(Scope::User),
        Some(WINGET_NO_APPLICABLE_INSTALLER) => Ok(Scope::System),
        _ => Err(shown.unexpected()),
    }
}

fn json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text.trim()).map_err(|error| format!("unreadable JSON: {error}"))
}

fn brew(output: &Captured, cask: bool) -> Result<bool, String> {
    if !output.success() {
        return Err(output.unexpected());
    }
    let info = json(&output.stdout)?;
    let list = if cask { "casks" } else { "formulae" };
    match info.get(list).and_then(Value::as_array).map(Vec::as_slice) {
        Some([entry]) => match entry.get("installed") {
            Some(Value::Array(versions)) => Ok(!versions.is_empty()),
            Some(Value::String(version)) => Ok(!version.is_empty()),
            Some(Value::Null) => Ok(false),
            _ => Err(format!("brew info has no {list}[0].installed")),
        },
        _ => Err(format!(
            "brew info did not return exactly one entry in {list}"
        )),
    }
}

fn dpkg(output: &Captured) -> Result<bool, String> {
    match output.code {
        Some(0) => Ok(output
            .stdout
            .split_whitespace()
            .any(|status| status == "installed")),
        Some(1) => Ok(false),
        _ => Err(output.unexpected()),
    }
}

fn rpm(output: &Captured) -> Result<bool, String> {
    match output.code {
        Some(0) => Ok(true),
        Some(1) if output.stdout.contains("is not installed") => Ok(false),
        _ => Err(output.unexpected()),
    }
}

fn pacman(output: &Captured) -> Result<bool, String> {
    match output.code {
        Some(0) => Ok(true),
        Some(1) if output.stderr.contains("was not found") => Ok(false),
        _ => Err(output.unexpected()),
    }
}

const ZYPPER_NOT_FOUND: i32 = 104;

fn zypper(output: &Captured) -> Result<bool, String> {
    match output.code {
        Some(0) => Ok(true),
        Some(ZYPPER_NOT_FOUND) => Ok(false),
        _ => Err(output.unexpected()),
    }
}

fn flatpak(output: &Captured) -> Result<bool, String> {
    match output.code {
        Some(0) => Ok(true),
        Some(1) if output.stderr.contains("not installed") => Ok(false),
        _ => Err(output.unexpected()),
    }
}

fn snap(output: &Captured) -> Result<bool, String> {
    match output.code {
        Some(0) => Ok(true),
        Some(1) if output.stderr.contains("no matching snaps installed") => Ok(false),
        _ => Err(output.unexpected()),
    }
}

fn winget_list(output: &Captured) -> Result<bool, String> {
    match output.hresult() {
        Some(0) => Ok(true),
        Some(WINGET_NO_APPLICATIONS_FOUND) => Ok(false),
        _ => Err(output.unexpected()),
    }
}

fn scoop_app(package: &str) -> &str {
    package.rsplit('/').next().unwrap_or(package)
}

fn scoop(output: &Captured) -> Result<bool, String> {
    let said = |text: &str| output.stdout.contains(text) || output.stderr.contains(text);
    match output.code {
        Some(0) if !output.stdout.trim().is_empty() => Ok(true),
        Some(1) if said("Could not find app path") => Ok(false),
        _ => Err(output.unexpected()),
    }
}

const CHOCO_NO_RESULTS: i32 = 2;

fn choco(output: &Captured, package: &str) -> Result<bool, String> {
    match output.code {
        Some(0) => Ok(output.stdout.lines().any(|line| {
            line.split_once('|')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case(package))
        })),
        Some(CHOCO_NO_RESULTS) => Ok(false),
        _ => Err(output.unexpected()),
    }
}

/// npm prints `{"dependencies": {...}}` and pnpm a list of those, exiting 1 when the filter
/// matches nothing.
fn node(output: &Captured, package: &str) -> Result<bool, String> {
    if !matches!(output.code, Some(0 | 1)) {
        return Err(output.unexpected());
    }
    let tree = json(&output.stdout).map_err(|error| format!("{error}{}", output.tail()))?;
    let roots = match &tree {
        Value::Array(roots) => roots.as_slice(),
        root => std::slice::from_ref(root),
    };
    if let Some(error) = roots.iter().find_map(|root| root.get("error")) {
        return Err(format!("the manager reported an error: {error}"));
    }
    Ok(roots.iter().any(|root| {
        root.get("dependencies")
            .and_then(|dependencies| dependencies.get(package))
            .is_some_and(|dependency| dependency.get("missing") != Some(&Value::Bool(true)))
    }))
}

fn bun(output: &Captured, package: &str) -> Result<bool, String> {
    if !output.success() {
        return Err(output.unexpected());
    }
    Ok(output.stdout.lines().any(|line| {
        line.split_once("── ").is_some_and(|(_, entry)| {
            let name = match entry.char_indices().skip(1).find(|(_, c)| *c == '@') {
                Some((at, _)) => &entry[..at],
                None => entry,
            };
            name == package
        })
    }))
}

fn cargo(output: &Captured, package: &str) -> Result<bool, String> {
    if !output.success() {
        return Err(output.unexpected());
    }
    Ok(output
        .stdout
        .lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .any(|line| line.split_whitespace().next() == Some(package)))
}

/// PEP 503: Python package names compare case-insensitively, with runs of `-`, `_`, and `.`
/// equal.
fn python_name(name: &str) -> String {
    let mut normal = String::with_capacity(name.len());
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !normal.ends_with('-') {
                normal.push('-');
            }
        } else {
            normal.push(c.to_ascii_lowercase());
        }
    }
    normal
}

fn uv(output: &Captured, package: &str) -> Result<bool, String> {
    if !output.success() {
        return Err(output.unexpected());
    }
    let wanted = python_name(package);
    Ok(output
        .stdout
        .lines()
        .filter(|line| !line.starts_with(['-', ' ', '\t']))
        .filter_map(|line| line.split_whitespace().next())
        .any(|name| python_name(name) == wanted))
}

fn pipx(output: &Captured, package: &str) -> Result<bool, String> {
    if !output.success() {
        return Err(output.unexpected());
    }
    let list = json(&output.stdout)?;
    let venvs = list
        .get("venvs")
        .and_then(Value::as_object)
        .ok_or("pipx list --json has no venvs table")?;
    let wanted = python_name(package);
    Ok(venvs.iter().any(|(name, venv)| {
        python_name(name) == wanted
            || venv
                .pointer("/metadata/main_package/package")
                .and_then(Value::as_str)
                .is_some_and(|main| python_name(main) == wanted)
    }))
}

/// `go install` names the binary after the last path element, skipping a `/vN` major-version
/// suffix.
fn go_binary(package: &str) -> &str {
    let mut parts = package.rsplit('/');
    let last = parts.next().unwrap_or(package);
    let versioned = last.len() > 1
        && last.starts_with('v')
        && last[1..].bytes().all(|b| b.is_ascii_digit())
        && last != "v1"
        && last != "v0";
    match parts.next() {
        Some(previous) if versioned => previous,
        _ => last,
    }
}

/// Go keeps no list of installed tools, so this finds the binary `go install` would write and
/// asks `go version -m` which package built it.
fn go(program: &Path, package: &str) -> Result<bool, String> {
    let env = query(program, ["env", "GOBIN", "GOPATH"])?;
    if !env.success() {
        return Err(env.unexpected());
    }
    let mut lines = env.stdout.lines();
    let gobin = lines.next().unwrap_or_default().trim();
    let gopath = lines.next().unwrap_or_default().trim();
    let dir = if gobin.is_empty() {
        std::env::split_paths(gopath)
            .next()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                format!("go env reports neither GOBIN nor an absolute GOPATH ({gopath:?})")
            })?
            .join("bin")
    } else {
        PathBuf::from(gobin)
    };
    let binary = dir.join(format!(
        "{}{}",
        go_binary(package),
        std::env::consts::EXE_SUFFIX
    ));
    match std::fs::symlink_metadata(&binary) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot inspect {}: {error}", binary.display())),
        Ok(_) => {}
    }
    let built = query(
        program,
        [OsStr::new("version"), OsStr::new("-m"), binary.as_os_str()],
    )?;
    if !built.success() {
        return Err(built.unexpected());
    }
    match go_path(&built.stdout) {
        Some(path) if path == package => Ok(true),
        Some(path) => Err(format!(
            "{} was built from {path}, not {package}; loadout will not overwrite it",
            binary.display()
        )),
        None => Err(format!(
            "go version -m {} names no package path",
            binary.display()
        )),
    }
}

fn go_path(build_info: &str) -> Option<&str> {
    build_info.lines().find_map(|line| {
        let mut fields = line.trim_start().split('\t');
        (fields.next() == Some("path"))
            .then(|| fields.next())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            code: Some(code),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    #[expect(clippy::cast_possible_wrap, reason = "HRESULT as an exit code")]
    fn hresult(code: u32) -> i32 {
        code as i32
    }

    #[test]
    fn brew_reads_the_installed_field_of_one_entry() {
        let formula = |installed: &str| {
            out(
                0,
                &format!(r#"{{"formulae":[{{"name":"rg","installed":{installed}}}],"casks":[]}}"#),
                "",
            )
        };
        assert_eq!(brew(&formula(r#"[{"version":"14.1.0"}]"#), false), Ok(true));
        assert_eq!(brew(&formula("[]"), false), Ok(false));
        let cask = |installed: &str| {
            out(
                0,
                &format!(
                    r#"{{"formulae":[],"casks":[{{"token":"firefox","installed":{installed}}}]}}"#
                ),
                "",
            )
        };
        assert_eq!(brew(&cask(r#""130.0""#), true), Ok(true));
        assert_eq!(brew(&cask("null"), true), Ok(false));
        assert!(
            brew(
                &out(1, "", "Error: No available formula with the name \"nope\"."),
                false
            )
            .is_err()
        );
        assert!(brew(&out(0, "not json", ""), false).is_err());
        assert!(brew(&out(0, r#"{"formulae":[],"casks":[]}"#, ""), false).is_err());
    }

    #[test]
    fn dpkg_counts_only_the_installed_status() {
        assert_eq!(dpkg(&out(0, "installed\n", "")), Ok(true));
        assert_eq!(dpkg(&out(0, "not-installed\ninstalled\n", "")), Ok(true));
        assert_eq!(dpkg(&out(0, "config-files\n", "")), Ok(false));
        assert_eq!(
            dpkg(&out(1, "", "dpkg-query: no packages found matching x")),
            Ok(false)
        );
        assert!(dpkg(&out(2, "", "dpkg-query: error: parsing file")).is_err());
    }

    #[test]
    fn text_managers_need_their_documented_not_found_answer() {
        assert_eq!(rpm(&out(0, "git-2.45.0-1.fc40.x86_64\n", "")), Ok(true));
        assert_eq!(
            rpm(&out(1, "package nope is not installed\n", "")),
            Ok(false)
        );
        assert!(rpm(&out(1, "", "error: rpmdb: BDB0113 Thread died\n")).is_err());
        assert_eq!(pacman(&out(0, "ripgrep 14.1.0-1\n", "")), Ok(true));
        assert_eq!(
            pacman(&out(1, "", "error: package 'nope' was not found\n")),
            Ok(false)
        );
        assert!(
            pacman(&out(
                1,
                "",
                "error: failed to init transaction (unable to lock database)\n"
            ))
            .is_err()
        );
        assert_eq!(zypper(&out(0, "i | git |", "")), Ok(true));
        assert_eq!(zypper(&out(104, "No matching items found.", "")), Ok(false));
        assert!(zypper(&out(5, "", "Root privileges are required")).is_err());
        assert_eq!(
            flatpak(&out(0, "Ref: app/org.x/x86_64/stable", "")),
            Ok(true)
        );
        assert_eq!(
            flatpak(&out(1, "", "error: org.x not installed\n")),
            Ok(false)
        );
        assert!(flatpak(&out(1, "", "error: Can't open system repo\n")).is_err());
        assert_eq!(snap(&out(0, "Name Version\nhello 2.10\n", "")), Ok(true));
        assert_eq!(
            snap(&out(1, "", "error: no matching snaps installed\n")),
            Ok(false)
        );
        assert!(snap(&out(1, "", "error: cannot communicate with server\n")).is_err());
    }

    #[test]
    fn winget_is_read_from_its_documented_result_codes() {
        assert_eq!(
            winget_list(&out(
                0,
                "Name Id Version\nripgrep BurntSushi.ripgrep.MSVC 14",
                ""
            )),
            Ok(true)
        );
        assert_eq!(
            winget_list(&out(
                hresult(0x8A15_0014),
                "No installed package found matching input criteria.",
                ""
            )),
            Ok(false)
        );
        let failure = winget_list(&out(hresult(0x8A15_000F), "", "")).unwrap_err();
        assert!(failure.contains("0x8A15000F"), "{failure}");
        assert_eq!(winget_show(&out(0, "Found Foo [A.B]", "")), Ok(Scope::User));
        assert_eq!(
            winget_show(&out(
                hresult(0x8A15_0010),
                "No applicable installer found",
                ""
            )),
            Ok(Scope::System)
        );
        assert!(winget_show(&out(hresult(0x8A15_0014), "", "")).is_err());
    }

    #[test]
    fn scoop_and_choco_queries() {
        assert_eq!(scoop_app("extras/firefox"), "firefox");
        assert_eq!(scoop_app("ripgrep"), "ripgrep");
        assert_eq!(
            scoop(&out(0, "C:\\Users\\t\\scoop\\apps\\ripgrep\\current\n", "")),
            Ok(true)
        );
        assert_eq!(
            scoop(&out(1, "Could not find app path for 'ripgrep'.\n", "")),
            Ok(false)
        );
        assert!(scoop(&out(1, "", "scoop: The term 'Get-Foo' is not recognized")).is_err());
        assert!(scoop(&out(0, "", "")).is_err());
        assert_eq!(choco(&out(0, "git|2.45.0\n", ""), "Git"), Ok(true));
        assert_eq!(choco(&out(0, "", ""), "git"), Ok(false));
        assert_eq!(choco(&out(0, "git.install|2.45.0\n", ""), "git"), Ok(false));
        assert_eq!(choco(&out(2, "", ""), "git"), Ok(false));
        assert!(choco(&out(1, "", "Access denied"), "git").is_err());
    }

    #[test]
    fn node_managers_read_the_dependency_tree() {
        let npm = r#"{"name":"lib","dependencies":{"@withgraphite/graphite-cli":{"version":"1.8.6"},"cowsay":{"version":"1.6.0"}}}"#;
        assert_eq!(node(&out(0, npm, ""), "cowsay"), Ok(true));
        assert_eq!(
            node(&out(0, npm, ""), "@withgraphite/graphite-cli"),
            Ok(true)
        );
        assert_eq!(
            node(&out(1, "{\n  \"name\": \"lib\"\n}\n", ""), "cowsay"),
            Ok(false)
        );
        let pnpm = r#"[{"path":"/h/.local/share/pnpm/global/5","dependencies":{"cowsay":{"version":"1.6.0"}}}]"#;
        assert_eq!(node(&out(0, pnpm, ""), "cowsay"), Ok(true));
        assert_eq!(node(&out(0, "[]", ""), "cowsay"), Ok(false));
        assert!(node(&out(1, r#"{"error":{"code":"EACCES"}}"#, ""), "cowsay").is_err());
        assert!(node(&out(1, "ERR_PNPM_NO_GLOBAL_BIN_DIR", ""), "cowsay").is_err());
        assert!(node(&out(254, "{}", ""), "cowsay").is_err());
        let missing = r#"{"dependencies":{"cowsay":{"missing":true}}}"#;
        assert_eq!(node(&out(1, missing, ""), "cowsay"), Ok(false));
    }

    #[test]
    fn bun_cargo_uv_and_pipx_lists_match_exact_names() {
        let bun_list = "/h/.bun/install/global node_modules (3)\n├── @scope/tool@1.0.0\n├── cowsay@1.6.0\n└── stackping@github:dan-myles/stackping#5181229\n";
        assert_eq!(bun(&out(0, bun_list, ""), "cowsay"), Ok(true));
        assert_eq!(bun(&out(0, bun_list, ""), "@scope/tool"), Ok(true));
        assert_eq!(bun(&out(0, bun_list, ""), "stackping"), Ok(true));
        assert_eq!(bun(&out(0, bun_list, ""), "cow"), Ok(false));
        assert!(bun(&out(1, "", "error"), "cowsay").is_err());

        let cargo_list = "ast-grep v0.45.3:\n    ast-grep\n    sg\nripgrep v14.1.0:\n    rg\n";
        assert_eq!(cargo(&out(0, cargo_list, ""), "ripgrep"), Ok(true));
        assert_eq!(cargo(&out(0, cargo_list, ""), "sg"), Ok(false));
        assert_eq!(cargo(&out(0, cargo_list, ""), "ripgrep-all"), Ok(false));
        assert!(cargo(&out(101, "", "error: failed to parse manifest"), "ripgrep").is_err());

        let uv_list = "Pycowsay v0.0.0.2\n- pycowsay\nruff v0.6.9\n- ruff\n";
        assert_eq!(uv(&out(0, uv_list, ""), "pycowsay"), Ok(true));
        assert_eq!(uv(&out(0, uv_list, ""), "Ruff"), Ok(true));
        assert_eq!(uv(&out(0, "", "No tools installed\n"), "ruff"), Ok(false));
        assert!(uv(&out(2, "", "error: failed to read tool receipt"), "ruff").is_err());

        let pipx_list = r#"{"pipx_spec_version":"0.1","venvs":{"pycowsay":{"metadata":{"main_package":{"package":"pycowsay"}}}}}"#;
        assert_eq!(pipx(&out(0, pipx_list, ""), "PyCowsay"), Ok(true));
        assert_eq!(pipx(&out(0, pipx_list, ""), "ruff"), Ok(false));
        assert_eq!(
            pipx(
                &out(0, r#"{"pipx_spec_version":"0.1","venvs":{}}"#, ""),
                "ruff"
            ),
            Ok(false)
        );
        assert!(pipx(&out(0, "nothing has been installed with pipx", ""), "ruff").is_err());
        assert!(pipx(&out(1, "", "boom"), "ruff").is_err());
    }

    #[test]
    fn python_names_follow_pep_503() {
        assert_eq!(python_name("Py_Cow..Say"), "py-cow-say");
        assert_eq!(python_name("ruff"), "ruff");
    }

    #[test]
    fn go_binary_names_skip_major_version_suffixes() {
        assert_eq!(go_binary("github.com/rakyll/hey"), "hey");
        assert_eq!(go_binary("golang.org/x/tools/gopls"), "gopls");
        assert_eq!(go_binary("github.com/foo/bar/v2"), "bar");
        assert_eq!(go_binary("github.com/foo/v1"), "v1");
        assert_eq!(go_binary("github.com/foo/vet"), "vet");
        let info = "/h/go/bin/hey: go1.22.2\n\tpath\tgithub.com/rakyll/hey\n\tmod\tgithub.com/rakyll/hey\tv0.1.4\th1:x\n";
        assert_eq!(go_path(info), Some("github.com/rakyll/hey"));
        assert_eq!(go_path("/h/go/bin/hey: go1.22.2\n"), None);
    }
}
