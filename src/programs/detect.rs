use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::context;
use crate::manifest::Manager;

use super::Scope;
use super::manager::{self, Invocation};
use super::process::{self, Captured};

fn query<I, S>(program: &Path, args: I) -> Result<Captured, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let invocation = Invocation::new(
        program,
        args.into_iter().map(|arg| arg.as_ref().to_os_string()),
    );
    process::query(&invocation).map_err(|error| format!("{}: {error}", invocation.display()))
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
            package,
            false,
        ),
        Manager::Cask => brew(
            &query(program, ["info", "--json=v2", "--cask", package])?,
            package,
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
                "--source",
                "winget",
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        )?),
        Manager::Scoop => scoop(&query(program, ["prefix", scoop_app(package)])?),
        Manager::Choco => choco(
            &query(program, ["list", "--exact", package, "--limit-output"])?,
            package,
        ),
        Manager::Npm => match npm(
            &query(program, ["ls", "--global", "--depth=0", "--json", package])?,
            package,
        )? {
            Npm::Answered(installed) => Ok(installed),
            Npm::AbsentPath(path) => {
                npm_prefix_absent(&query(program, ["prefix", "--global"])?, &path).map(|()| false)
            }
        },
        Manager::Pnpm => pnpm(
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

/// Whether and how a missing package can be installed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Eligibility {
    Scope(Scope),
    Unsupported(String),
}

/// Winget's scope is per package. A user-scope install needs an applicable user-scope installer
/// whose per-user behaviour is known (portable); otherwise a machine-scope installer must be
/// shown to apply before the package counts as installable with `--elevated`. Winget's own
/// applicability answers both questions; loadout never assumes a machine installer exists.
pub(super) fn eligibility(
    manager: Manager,
    program: &Path,
    package: &str,
) -> Result<Eligibility, String> {
    if let Some(scope) = manager::scope(manager) {
        return Ok(Eligibility::Scope(scope));
    }
    let show = |scope: &str| {
        query(
            program,
            [
                "show",
                "--id",
                package,
                "--exact",
                "--source",
                "winget",
                "--scope",
                scope,
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        )
        .and_then(|shown| winget_installer(&shown))
    };
    let user = show("user")?;
    if user.as_deref().is_some_and(per_user_installer) {
        return Ok(Eligibility::Scope(Scope::User));
    }
    Ok(match (show("machine")?, user) {
        (Some(_), _) => Eligibility::Scope(Scope::System),
        (None, Some(kind)) => Eligibility::Unsupported(format!(
            "winget's user-scope installer is {kind:?}, whose per-user behaviour loadout can't \
             confirm (only portable installers qualify), and no machine-scope installer applies"
        )),
        (None, None) => Eligibility::Unsupported(
            "winget has no installer that applies to this machine in user or machine scope"
                .to_owned(),
        ),
    })
}

/// Portable packages, bare or nested in a zip (shown as "portable (zip)"), install into the
/// user's `WinGet\Packages` folder; seen natively with `BurntSushi.ripgrep.MSVC`. Other types,
/// including other installers nested in a zip, may install machine-wide or raise UAC whatever
/// scope was asked for.
fn per_user_installer(kind: &str) -> bool {
    matches!(kind, "portable" | "portable (zip)")
}

/// The installer type winget would use for the requested scope, or `None` when it says none
/// applies. `winget show` exits 0 even then (seen on winget 1.6), so the answer is the line under
/// its `Installer:` heading; any other text is an error, never a guess.
fn winget_installer(shown: &Captured) -> Result<Option<String>, String> {
    match shown.hresult() {
        Some(0) => {}
        Some(WINGET_NO_APPLICABLE_INSTALLER) => return Ok(None),
        _ => return Err(shown.unexpected()),
    }
    let line = shown
        .stdout
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "Installer:")
        .nth(1)
        .unwrap_or_default();
    if let Some(kind) = line.strip_prefix("Installer Type:") {
        match kind.trim() {
            "" => Err(format!(
                "winget show printed an empty installer type{}",
                shown.tail()
            )),
            kind => Ok(Some(kind.to_owned())),
        }
    } else if line.starts_with("No applicable installer found") {
        Ok(None)
    } else {
        Err(format!(
            "winget show printed no installer section{}",
            shown.tail()
        ))
    }
}

fn json(text: &str) -> Result<Value, String> {
    serde_json::from_str(text.trim()).map_err(|error| format!("unreadable JSON: {error}"))
}

/// One `brew info --json=v2` entry for the requested name (its name, tap-qualified name, alias,
/// or old name). A formula's `installed` is a list of `{ "version": … }` records; a cask's is a
/// version string, or `null` when it isn't installed.
fn brew(output: &Captured, package: &str, cask: bool) -> Result<bool, String> {
    if !output.success() {
        return Err(output.unexpected());
    }
    let info = json(&output.stdout)?;
    let list = if cask { "casks" } else { "formulae" };
    let Some([entry]) = info.get(list).and_then(Value::as_array).map(Vec::as_slice) else {
        return Err(format!(
            "brew info did not return exactly one entry in {list}"
        ));
    };
    let entry = entry
        .as_object()
        .ok_or_else(|| format!("brew info's {list} entry is not an object"))?;
    let (names, aliases) = if cask {
        (&["token", "full_token"][..], &["old_tokens"][..])
    } else {
        (&["name", "full_name"][..], &["aliases", "oldnames"][..])
    };
    let answers_for_package = names
        .iter()
        .any(|field| entry.get(*field).and_then(Value::as_str) == Some(package))
        || aliases.iter().any(|field| {
            entry
                .get(*field)
                .and_then(Value::as_array)
                .is_some_and(|names| names.iter().any(|name| name.as_str() == Some(package)))
        });
    if !answers_for_package {
        return Err(format!(
            "brew info answered for a different {}",
            if cask { "cask" } else { "formula" }
        ));
    }
    match (cask, entry.get("installed")) {
        (false, Some(Value::Array(kegs))) => {
            if kegs
                .iter()
                .all(|keg| keg.get("version").is_some_and(Value::is_string))
            {
                Ok(!kegs.is_empty())
            } else {
                Err("brew info lists an installed keg without a version".to_owned())
            }
        }
        (true, Some(Value::String(version))) if !version.is_empty() => Ok(true),
        (true, Some(Value::Null)) => Ok(false),
        _ => Err(format!(
            "brew info's {list} entry has no recognizable installed field"
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

/// What a `dependencies` table in npm's or pnpm's `ls --json` says about one package: absent,
/// installed (an object with a `version`), or explicitly `missing`. Any other shape is an error.
fn dependency(root: &Map<String, Value>, package: &str) -> Result<Option<bool>, String> {
    let Some(dependencies) = root.get("dependencies") else {
        return Ok(None);
    };
    let dependencies = dependencies
        .as_object()
        .ok_or("its dependencies field is not an object")?;
    let Some(entry) = dependencies.get(package) else {
        return Ok(None);
    };
    let entry = entry
        .as_object()
        .ok_or_else(|| format!("its entry for {package} is not an object"))?;
    if entry.get("missing") == Some(&Value::Bool(true)) {
        Ok(Some(false))
    } else if entry.get("version").is_some_and(Value::is_string) {
        Ok(Some(true))
    } else {
        Err(format!(
            "its entry for {package} has neither a version nor missing: true"
        ))
    }
}

/// What `npm ls` said: a verdict, or that a path it needed doesn't exist.
#[derive(Debug, PartialEq, Eq)]
enum Npm {
    Answered(bool),
    AbsentPath(String),
}

const NPM_ERROR_EXIT: i32 = 254;
const NPM_LSTAT_ENOENT: &str = "ENOENT: no such file or directory, lstat '";

/// `npm ls --global --json P` prints the global root, `{"name": …, "dependencies": {…}}`, and
/// exits 1 when the filter matches nothing; it prints `{"name":"lib"}` then (sampled on npm 9 and
/// 11). An installed package at exit 1 means npm found problems in the tree, which is an error.
///
/// When the configured global prefix doesn't exist yet, npm 9 exits 254 with
/// `{"error":{"code":"ENOENT","summary":"ENOENT: no such file or directory, lstat '<path>'"}}`
/// (sampled). That alone isn't absence: the caller must confirm `<path>` is npm's own global
/// prefix. Any other error stays an error.
fn npm(output: &Captured, package: &str) -> Result<Npm, String> {
    let exited_one = match output.code {
        Some(0) => false,
        Some(1) => true,
        Some(NPM_ERROR_EXIT) => return npm_absent_path(output).map(Npm::AbsentPath),
        _ => return Err(output.unexpected()),
    };
    let tree = json(&output.stdout).map_err(|error| format!("{error}{}", output.tail()))?;
    let root = tree
        .as_object()
        .ok_or("npm ls printed JSON that is not an object")?;
    if let Some(error) = root.get("error") {
        return Err(format!("npm reported an error: {error}"));
    }
    if !root.get("name").is_some_and(Value::is_string) {
        return Err("npm ls printed an object without the global root's name".to_owned());
    }
    match dependency(root, package).map_err(|error| format!("npm ls: {error}"))? {
        Some(true) if exited_one => Err(format!(
            "npm ls lists {package} but exited 1, so npm found problems in the global tree{}",
            output.tail()
        )),
        Some(installed) => Ok(Npm::Answered(installed)),
        None => Ok(Npm::Answered(false)),
    }
}

/// The one path an exit-254 `ENOENT … lstat '<path>'` names, read only from that exact summary.
fn npm_absent_path(output: &Captured) -> Result<String, String> {
    let error = json(&output.stdout)
        .ok()
        .and_then(|tree| tree.get("error").cloned())
        .ok_or_else(|| output.unexpected())?;
    if error.get("code").and_then(Value::as_str) != Some("ENOENT") {
        return Err(format!("npm ls failed: {error}"));
    }
    error
        .get("summary")
        .and_then(Value::as_str)
        .and_then(|summary| summary.strip_prefix(NPM_LSTAT_ENOENT))
        .and_then(|rest| rest.strip_suffix('\''))
        .filter(|path| !path.is_empty() && !path.contains('\''))
        .map(str::to_owned)
        .ok_or_else(|| {
            format!("npm ls failed with a missing file it doesn't name clearly: {error}")
        })
}

/// Confirms, with `npm prefix --global` from the same npm, that the path `npm ls` couldn't find is
/// exactly npm's configured global prefix. Then nothing is installed there yet.
fn npm_prefix_absent(prefix: &Captured, missing: &str) -> Result<(), String> {
    if !prefix.success() {
        return Err(format!(
            "npm ls can't find {missing:?}, and npm prefix --global failed: {}",
            prefix.unexpected()
        ));
    }
    let mut lines = prefix
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let configured = match (lines.next(), lines.next()) {
        (Some(configured), None) if Path::new(configured).is_absolute() => configured,
        _ => {
            return Err(format!(
                "npm ls can't find {missing:?}, and npm prefix --global printed no single absolute path{}",
                prefix.tail()
            ));
        }
    };
    if Path::new(configured) == Path::new(missing) {
        Ok(())
    } else {
        Err(format!(
            "npm ls can't find {missing:?}, which is not npm's global prefix {configured:?}; \
             check npm's configuration or reinstall npm"
        ))
    }
}

/// `pnpm ls --global --json P` prints a list of global projects, each `{"path": …,
/// "dependencies": {…}}`, and exits 0 either way (sampled on pnpm 12: `[{"path":
/// ".../global/v11", "private": true, "dependencies": {}}]` before any install).
fn pnpm(output: &Captured, package: &str) -> Result<bool, String> {
    if !output.success() {
        return Err(output.unexpected());
    }
    let tree = json(&output.stdout).map_err(|error| format!("{error}{}", output.tail()))?;
    let roots = tree
        .as_array()
        .filter(|roots| !roots.is_empty())
        .ok_or("pnpm ls printed JSON that is not a list of global projects")?;
    let mut installed = false;
    for root in roots {
        let root = root
            .as_object()
            .filter(|root| root.get("path").is_some_and(Value::is_string))
            .ok_or("pnpm ls printed a project without a path")?;
        if let Some(error) = root.get("error") {
            return Err(format!("pnpm reported an error: {error}"));
        }
        installed |=
            dependency(root, package).map_err(|error| format!("pnpm ls: {error}"))? == Some(true);
    }
    Ok(installed)
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

/// `pipx list --json` prints `{"venvs": {NAME: {"metadata": {"main_package": {"package": …}}}}}`.
/// Every venv must carry its main package's name; the requested package is installed when one
/// venv's main package is it.
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
    let mut installed = false;
    for (name, venv) in venvs {
        let main = venv
            .pointer("/metadata/main_package/package")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("pipx venv {name:?} has no main package name"))?;
        installed |= python_name(main) == wanted;
    }
    Ok(installed)
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
    fn error_tails_keep_the_last_lines_as_a_terminal_shows_them() {
        let progress = out(
            1,
            "- \r   \r  10%\r  50%\r 100%\r\nFailed when searching source: msstore\r\n",
            "",
        );
        assert_eq!(
            progress.tail(),
            ":\n 100%\nFailed when searching source: msstore"
        );
        let long = (1..=30)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = out(1, &long, "boom\n").tail();
        assert!(
            tail.starts_with(":\nline 12\n") && tail.ends_with("line 30\nboom"),
            "{tail}"
        );
        assert_eq!(out(1, "", "").tail(), "");
    }

    #[test]
    fn brew_needs_one_entry_for_the_requested_name_with_a_recognized_installed_field() {
        let formula = |entry: &str| out(0, &format!(r#"{{"formulae":[{entry}],"casks":[]}}"#), "");
        let rg = |installed: &str| {
            formula(&format!(
                r#"{{"name":"ripgrep","full_name":"ripgrep","aliases":["rg"],"oldnames":[],"installed":{installed}}}"#
            ))
        };
        assert_eq!(
            brew(&rg(r#"[{"version":"14.1.0"}]"#), "ripgrep", false),
            Ok(true)
        );
        assert_eq!(
            brew(&rg(r#"[{"version":"14.1.0"}]"#), "rg", false),
            Ok(true)
        );
        assert_eq!(brew(&rg("[]"), "ripgrep", false), Ok(false));
        for malformed in [r#""14.1.0""#, "null", "[null]", r#"[{"v":"1"}]"#, "{}"] {
            assert!(
                brew(&rg(malformed), "ripgrep", false).is_err(),
                "{malformed}"
            );
        }
        assert!(
            brew(&rg("[]"), "fd", false).is_err(),
            "an answer for another formula"
        );
        assert!(brew(&formula("null"), "ripgrep", false).is_err());

        let cask = |installed: &str| {
            out(
                0,
                &format!(
                    r#"{{"formulae":[],"casks":[{{"token":"firefox","full_token":"firefox","old_tokens":[],"installed":{installed}}}]}}"#
                ),
                "",
            )
        };
        assert_eq!(brew(&cask(r#""130.0""#), "firefox", true), Ok(true));
        assert_eq!(brew(&cask("null"), "firefox", true), Ok(false));
        for malformed in [r#""""#, "[]", r#"[{"version":"1"}]"#, "true"] {
            assert!(
                brew(&cask(malformed), "firefox", true).is_err(),
                "{malformed}"
            );
        }
        assert!(
            brew(
                &out(1, "", "Error: No available formula with the name \"nope\"."),
                "nope",
                false
            )
            .is_err()
        );
        assert!(brew(&out(0, "not json", ""), "ripgrep", false).is_err());
        assert!(
            brew(
                &out(0, r#"{"formulae":[],"casks":[]}"#, ""),
                "ripgrep",
                false
            )
            .is_err()
        );
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
        let portable = "Found RipGrep MSVC [BurntSushi.ripgrep.MSVC]\r\nTags:\r\n  ripgrep\r\nInstaller:\r\n  Installer Type: portable (zip)\r\n  Installer Url: https://x/rg.zip\r\n";
        assert_eq!(
            winget_installer(&out(0, portable, "")),
            Ok(Some("portable (zip)".to_owned()))
        );
        let none = "Found 7-Zip [7zip.7zip]\r\nTags:\r\n  zip\r\nInstaller:\r\n  No applicable installer found; see logs for more details.\r\n";
        assert_eq!(winget_installer(&out(0, none, "")), Ok(None));
        assert_eq!(
            winget_installer(&out(hresult(0x8A15_0010), "", "")),
            Ok(None)
        );
        assert!(
            winget_installer(&out(
                hresult(0x8A15_0014),
                "No package found matching input criteria.",
                ""
            ))
            .is_err()
        );
        assert!(winget_installer(&out(0, "Found Foo [A.B]\r\n", "")).is_err());
        for empty in ["  Installer Type:", "  Installer Type:   ", "  Installer Type:\t"] {
            let shown =
                format!("Found X [A.B]\r\nInstaller:\r\n{empty}\r\n  Installer Url: https://x\r\n");
            assert!(winget_installer(&out(0, &shown, "")).is_err(), "{empty:?}");
        }
        let localized = "Gefunden 7-Zip [7zip.7zip]\r\nInstallationsprogramm:\r\n  Typ: exe\r\n";
        assert!(winget_installer(&out(0, localized, "")).is_err());
        let extra =
            "Found 7-Zip [7zip.7zip]\r\nInstaller:\r\n  Something new\r\n  Installer Type: exe\r\n";
        assert!(winget_installer(&out(0, extra, "")).is_err());
    }

    /// Drives the real two-scope `eligibility` through a fake winget whose `show` answers per
    /// scope, so the empty-type refusal is checked on the path `steps` takes, not just the parser.
    #[cfg(unix)]
    #[test]
    fn winget_eligibility_asks_both_scopes_and_refuses_an_empty_installer_type() {
        use std::os::unix::fs::PermissionsExt;

        let none = "  No applicable installer found; see logs for more details.";
        let exe = "  Installer Type: exe";
        let mut cases = vec![
            ("  Installer Type: portable (zip)", none, "user"),
            (none, exe, "system"),
            (exe, none, "unsupported: only portable installers qualify"),
            (none, none, "unsupported: user or machine scope"),
        ];
        for empty in ["  Installer Type:", "  Installer Type:   "] {
            cases.extend([(empty, exe, "error"), (none, empty, "error"), (exe, empty, "error")]);
        }
        // Every script is written before any runs: executing a file another thread's fork may
        // still hold open for writing can fail with ETXTBSY.
        let dir = tempfile::tempdir().unwrap();
        let scripts: Vec<_> = cases
            .iter()
            .enumerate()
            .map(|(index, (user, machine, _))| {
                let winget = dir.path().join(format!("winget-{index}"));
                let script = format!(
                    "#!/bin/sh\ncase \"$*\" in\n*\"--scope user\"*) line='{user}';;\n*\"--scope machine\"*) line='{machine}';;\n*) exit 64;;\nesac\nprintf 'Found X [A.B]\\nInstaller:\\n%s\\n  Installer Url: https://x\\n' \"$line\"\n"
                );
                std::fs::write(&winget, script).unwrap();
                std::fs::set_permissions(&winget, std::fs::Permissions::from_mode(0o755)).unwrap();
                winget
            })
            .collect();
        for ((user, machine, expected), winget) in cases.iter().zip(&scripts) {
            let decided = eligibility(Manager::Winget, winget, "A.B");
            let label = format!("user {user:?}, machine {machine:?}: {decided:?}");
            match *expected {
                "user" => assert_eq!(decided, Ok(Eligibility::Scope(Scope::User)), "{label}"),
                "system" => assert_eq!(decided, Ok(Eligibility::Scope(Scope::System)), "{label}"),
                "error" => assert!(decided.is_err(), "{label}"),
                unsupported => {
                    let reason = unsupported.trim_start_matches("unsupported: ");
                    assert!(
                        matches!(&decided, Ok(Eligibility::Unsupported(why)) if why.contains(reason)),
                        "{label}"
                    );
                }
            }
        }
    }

    #[test]
    fn only_portable_installers_count_as_per_user() {
        assert!(per_user_installer("portable"));
        assert!(per_user_installer("portable (zip)"));
        for kind in [
            "exe",
            "exe (zip)",
            "msi (zip)",
            "inno",
            "nullsoft",
            "wix",
            "burn",
            "msix",
            "appx",
            "zip",
            "",
        ] {
            assert!(!per_user_installer(kind), "{kind}");
        }
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
    fn npm_needs_its_global_root_and_a_versioned_entry() {
        let installed = r#"{"name":"lib","dependencies":{"@withgraphite/graphite-cli":{"version":"1.8.6","overridden":false},"cowsay":{"version":"1.6.0","overridden":false}}}"#;
        assert_eq!(
            npm(&out(0, installed, ""), "cowsay"),
            Ok(Npm::Answered(true))
        );
        assert_eq!(
            npm(&out(0, installed, ""), "@withgraphite/graphite-cli"),
            Ok(Npm::Answered(true))
        );
        assert_eq!(
            npm(&out(1, "{\n  \"name\": \"lib\"\n}\n", ""), "cowsay"),
            Ok(Npm::Answered(false))
        );
        assert_eq!(
            npm(&out(0, installed, ""), "figlet"),
            Ok(Npm::Answered(false))
        );
        assert_eq!(
            npm(
                &out(
                    1,
                    r#"{"name":"lib","dependencies":{"cowsay":{"missing":true}}}"#,
                    ""
                ),
                "cowsay"
            ),
            Ok(Npm::Answered(false))
        );
        for (code, malformed) in [
            (0, r#"{"name":"lib","dependencies":{"cowsay":null}}"#),
            (0, r#"{"name":"lib","dependencies":{"cowsay":{}}}"#),
            (0, r#"{"name":"lib","dependencies":{"cowsay":"1.6.0"}}"#),
            (0, r#"{"name":"lib","dependencies":[]}"#),
            (1, "null"),
            (1, "[]"),
            (1, "{}"),
            (1, r#"{"error":{"code":"EACCES"}}"#),
            (1, installed),
            (1, "ERR_SOMETHING"),
            (254, r#"{"name":"lib"}"#),
        ] {
            assert!(
                npm(&out(code, malformed, ""), "cowsay").is_err(),
                "{code} {malformed}"
            );
        }
    }

    /// npm 9.2's real answer when its configured global prefix doesn't exist yet (Linux slot 7,
    /// `npm_config_prefix=/tmp/fresh-npm-prefix`).
    const NPM_ABSENT_PREFIX: &str = "{\n  \"error\": {\n    \"code\": \"ENOENT\",\n    \"summary\": \"ENOENT: no such file or directory, lstat '/tmp/fresh-npm-prefix'\",\n    \"detail\": \"This is related to npm not being able to find a file.\\n\"\n  }\n}\n";

    #[test]
    fn npm_reads_a_missing_global_prefix_only_from_its_exact_enoent_answer() {
        assert_eq!(
            npm(&out(254, NPM_ABSENT_PREFIX, ""), "cowsay"),
            Ok(Npm::AbsentPath("/tmp/fresh-npm-prefix".to_owned()))
        );
        let enoent = |summary: &str| {
            out(
                254,
                &format!(
                    r#"{{"error":{{"code":"ENOENT","summary":{}}}}}"#,
                    serde_json::to_string(summary).unwrap()
                ),
                "",
            )
        };
        assert_eq!(
            npm(
                &enoent("ENOENT: no such file or directory, lstat '/tmp/p/lib/node_modules'"),
                "cowsay"
            ),
            Ok(Npm::AbsentPath("/tmp/p/lib/node_modules".to_owned())),
            "a nested path is reported as itself; only the prefix check can accept it"
        );
        for ambiguous in [
            "ENOENT: no such file or directory, lstat '/a' and '/b'",
            "ENOENT: no such file or directory, open '/tmp/p'",
            "ENOENT: no such file or directory, lstat ''",
            "ENOENT: no such file or directory, lstat '/tmp/p",
            "something else",
        ] {
            assert!(npm(&enoent(ambiguous), "cowsay").is_err(), "{ambiguous}");
        }
        assert!(npm(&out(254, r#"{"error":{"code":"ENOENT"}}"#, ""), "cowsay").is_err());
        let eacces = NPM_ABSENT_PREFIX.replace("ENOENT\",", "EACCES\",");
        assert!(npm(&out(254, &eacces, ""), "cowsay").is_err());
        for code in [1, 2, 255] {
            assert!(
                !matches!(
                    npm(&out(code, NPM_ABSENT_PREFIX, ""), "cowsay"),
                    Ok(Npm::AbsentPath(_))
                ),
                "exit {code}"
            );
        }
        assert!(npm(&out(254, "not json", ""), "cowsay").is_err());
        assert!(npm(&out(254, r#"{"name":"lib"}"#, ""), "cowsay").is_err());
    }

    #[test]
    fn a_missing_path_is_absence_only_when_it_is_npms_own_global_prefix() {
        let abs = |path: &str| {
            if cfg!(windows) {
                format!("C:{path}")
            } else {
                path.to_owned()
            }
        };
        let missing = abs("/tmp/fresh-npm-prefix");
        let answer = |path: &str| format!("{path}\n");
        assert_eq!(
            npm_prefix_absent(&out(0, &answer(&missing), ""), &missing),
            Ok(())
        );
        for (code, printed) in [
            (0, answer(&abs("/tmp/other-prefix"))),
            (0, answer(&abs("/tmp/fresh-npm-prefix/lib"))),
            (0, answer(&abs("/tmp"))),
            (0, String::new()),
            (0, "relative/prefix\n".to_owned()),
            (
                0,
                format!("{}{}", answer(&missing), answer(&abs("/tmp/second"))),
            ),
            (1, answer(&missing)),
            (254, String::new()),
        ] {
            assert!(
                npm_prefix_absent(&out(code, &printed, ""), &missing).is_err(),
                "{code} {printed:?}"
            );
        }
        let nested = npm_prefix_absent(
            &out(0, &answer(&abs("/tmp/p")), ""),
            &abs("/tmp/p/lib/node_modules"),
        )
        .unwrap_err();
        assert!(nested.contains("is not npm's global prefix"), "{nested}");
    }

    #[test]
    fn pnpm_needs_its_global_projects_and_a_versioned_entry() {
        let empty =
            r#"[{"path":"/h/.local/share/pnpm/global/v11","private":true,"dependencies":{}}]"#;
        assert_eq!(pnpm(&out(0, empty, ""), "figlet"), Ok(false));
        let installed = r#"[{"path":"/h/.local/share/pnpm/global/v11","private":true,"dependencies":{"figlet":{"from":"figlet","version":"1.8.0"}}}]"#;
        assert_eq!(pnpm(&out(0, installed, ""), "figlet"), Ok(true));
        assert_eq!(pnpm(&out(0, installed, ""), "cowsay"), Ok(false));
        assert_eq!(pnpm(&out(0, r#"[{"path":"/g"}]"#, ""), "figlet"), Ok(false));
        for malformed in [
            "[]",
            "{}",
            "null",
            r#"[{"unexpected":true}]"#,
            r#"[{"path":"/g","dependencies":{"figlet":null}}]"#,
            r#"[{"path":"/g","dependencies":[]}]"#,
            r#"[{"path":"/g","error":{"code":"ERR_PNPM_NO_GLOBAL_BIN_DIR"}}]"#,
            "ERR_PNPM_NO_GLOBAL_BIN_DIR",
        ] {
            assert!(
                pnpm(&out(0, malformed, ""), "figlet").is_err(),
                "{malformed}"
            );
        }
        assert!(pnpm(&out(1, empty, ""), "figlet").is_err());
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

        let venv = |name: &str, main: &str| {
            format!(r#""{name}":{{"metadata":{{"main_package":{{"package":"{main}"}}}}}}"#)
        };
        let list = |venvs: &str| {
            out(
                0,
                &format!(r#"{{"pipx_spec_version":"0.1","venvs":{{{venvs}}}}}"#),
                "",
            )
        };
        let two = format!("{},{}", venv("uv", "uv"), venv("pycowsay", "pycowsay"));
        assert_eq!(pipx(&list(&two), "PyCowsay"), Ok(true));
        assert_eq!(pipx(&list(&two), "ruff"), Ok(false));
        assert_eq!(pipx(&list(""), "ruff"), Ok(false));
        assert_eq!(
            pipx(&list(&venv("cowsay-2", "pycowsay")), "pycowsay"),
            Ok(true)
        );
        assert_eq!(
            pipx(&list(&venv("pycowsay", "other")), "pycowsay"),
            Ok(false)
        );
        for malformed in [
            r#""pycowsay":null"#,
            r#""pycowsay":{}"#,
            r#""pycowsay":{"metadata":{"main_package":{}}}"#,
        ] {
            assert!(pipx(&list(malformed), "pycowsay").is_err(), "{malformed}");
        }
        assert!(pipx(&out(0, "nothing has been installed with pipx", ""), "ruff").is_err());
        assert!(pipx(&out(0, r#"{"venvs":[]}"#, ""), "ruff").is_err());
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
