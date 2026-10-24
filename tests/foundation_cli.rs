use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/foundation")
        .join(name)
}

fn loadout(args: &[&str], home: &Path, state: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loadout"))
        .args(args)
        .env("LOADOUT_HOME", home)
        .env("LOADOUT_STATE_DIR", state)
        .output()
        .unwrap()
}

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = dir.path().join("state");
        fs::create_dir(&home).unwrap();
        Self {
            _dir: dir,
            home,
            state,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        loadout(args, &self.home, &self.state)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn validate_text(text: &str) -> (i32, String) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("loadout.toml"), text).unwrap();
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["validate", dir.path().to_str().unwrap()]);
    (output.status.code().unwrap(), stdout(&output))
}

#[test]
fn help_lists_only_working_commands() {
    let output = Sandbox::new().run(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = stdout(&output);
    assert!(help.contains("doctor"), "{help}");
    assert!(help.contains("validate"), "{help}");
    for future in [
        "apply", "plan", "status", "rollback", "update", "service", "init",
    ] {
        assert!(
            !help
                .lines()
                .any(|line| line.trim_start().starts_with(future)),
            "{future} must not be advertised before it works:\n{help}"
        );
    }
}

#[test]
fn version_prints_package_version() {
    let output = Sandbox::new().run(&["--version"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        stdout(&output).trim(),
        format!("loadout {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_command_is_a_usage_error() {
    let output = Sandbox::new().run(&["apply"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn complete_fixture_is_valid() {
    let output = Sandbox::new().run(&["validate", fixture("complete").to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    assert!(stdout(&output).contains("valid:"));
}

#[test]
fn complete_fixture_json_lists_every_layer() {
    let output = Sandbox::new().run(&["validate", fixture("complete").to_str().unwrap(), "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], true);
    let layers: Vec<String> = report["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| layer["file"].as_str().unwrap().replace('\\', "/"))
        .collect();
    assert_eq!(
        layers,
        [
            "loadout.toml",
            "os/linux.toml",
            "os/macos.toml",
            "os/windows.toml",
            "tags/gaming.toml",
            "tags/laptop.toml",
            "tags/work.toml",
            "machines/work-laptop.toml",
        ]
    );
    assert_eq!(report["diagnostics"].as_array().unwrap().len(), 0);
}

#[test]
fn invalid_fixture_reports_each_mistake() {
    let output = Sandbox::new().run(&["validate", fixture("invalid").to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3));
    let text = stdout(&output).replace('\\', "/");
    for expected in [
        "os/beos.toml: error: unknown OS layer",
        "tags/work.toml: error: TOML parse error",
        "unknown field `sorce`",
        "tag \"missing-tag\" has no layer file",
        "tag \"work\" is listed twice",
        "machines/ghost.toml: error: machine \"ghost\" has a layer file but no [machines.ghost] entry",
        "variables.machine: `machine` is reserved",
        "update.interval_minutes: must be at least 15",
        "agents.enabled: unknown agent \"nobody\"",
        "agents.custom.acme.mcp.linux: \"/etc/acme.json\" must start with ~/",
        "agents.custom.acme.mcp.format: shape codex-toml needs format toml",
        "agents.custom.acme.mcp.map: must be a pointer",
        "agents.custom.claude: claude is a built-in agent",
        "agents.custom.claude.evidence: say where these paths come from",
        "files.escape.source: \"../outside\" must be a relative path",
        "files.escape.target: \"/etc/passwd\" must start with ~/",
        "files.half: missing source",
        "skills.empty.source: skills/empty has no SKILL.md",
        "mcp.both: a server has either command (stdio) or url (remote), not both",
        "warning: mcp.leak.headers.Authorization: looks like a credential",
        "programs.nothing: the global layer has nothing to remove",
        "invalid:",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
}

#[test]
fn invalid_fixture_json_carries_file_and_key() {
    let output = Sandbox::new().run(&["validate", fixture("invalid").to_str().unwrap(), "--json"]);
    assert_eq!(output.status.code(), Some(3));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], false);
    let diagnostics = report["diagnostics"].as_array().unwrap();
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["severity"] == "error"
            && diagnostic["file"] == "loadout.toml"
            && diagnostic["key"] == "files.half"
            && diagnostic["message"] == "missing source"
    }));
}

#[test]
fn missing_directory_and_root_file_are_invalid() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["validate", sandbox.home.join("nope").to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stdout(&output).contains("is not a directory"));

    let output = sandbox.run(&["validate", sandbox.home.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stdout(&output).contains("loadout.toml: error: cannot read"));
}

#[test]
fn header_is_required_and_versioned() {
    let (code, text) = validate_text("[variables]\na = \"b\"\n");
    assert_eq!(code, 3);
    assert!(text.contains("missing [loadout] table"), "{text}");

    let (code, text) = validate_text("[loadout]\nversion = 2\n");
    assert_eq!(code, 3);
    assert!(text.contains("loadout.version is 2"), "{text}");

    let (code, text) = validate_text("[loadout]\nversion = 1\n");
    assert_eq!(code, 0, "{text}");
}

#[test]
fn global_layer_cannot_edit_or_remove() {
    let (code, text) = validate_text(
        "[loadout]\nversion = 1\n[agents]\nenabled = { add = [\"claude\"] }\n[files.a]\nremove = true\n",
    );
    assert_eq!(code, 3);
    assert!(
        text.contains("agents.enabled: the global layer has no inherited list to edit"),
        "{text}"
    );
    assert!(
        text.contains("files.a: the global layer has nothing to remove"),
        "{text}"
    );
}

#[test]
fn remove_stands_alone_and_replace_must_be_complete() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("loadout.toml"),
        "[loadout]\nversion = 1\n[machines.box]\ntags = [\"t\"]\n",
    )
    .unwrap();
    fs::create_dir(dir.path().join("tags")).unwrap();
    fs::write(
        dir.path().join("tags/t.toml"),
        "[files.a]\nremove = true\ntarget = \"~/.a\"\n[programs.p]\nreplace = true\n[mcp.m]\nreplace = true\n",
    )
    .unwrap();
    let output = Sandbox::new().run(&["validate", dir.path().to_str().unwrap()]);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        text.contains("files.a: remove = true stands alone"),
        "{text}"
    );
    assert!(
        text.contains("programs.p: a program needs at least one manager"),
        "{text}"
    );
    assert!(
        text.contains("mcp.m: missing command (stdio server) or url"),
        "{text}"
    );
}

#[test]
fn unknown_keys_and_wrong_types_are_rejected() {
    let (code, text) = validate_text("[loadout]\nversion = 1\nfoo = 1\n");
    assert_eq!(code, 3);
    assert!(text.contains("unknown field `foo`"), "{text}");

    let (code, text) = validate_text(
        "[loadout]\nversion = 1\n[programs.x]\nbrew = \"x\"\nwhen = { os = \"beos\" }\n",
    );
    assert_eq!(code, 3, "{text}");

    let (code, text) = validate_text(
        "[loadout]\nversion = 1\n[agents.custom.x]\nevidence = \"e\"\nskills = { linux = \"~/x\" }\nmcp = { linux = \"~/x.json\", format = \"json\", map = \"/m\", shape = \"made-up\", env_syntax = \"none\" }\n",
    );
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("made-up"), "{text}");
}

#[cfg(unix)]
#[test]
fn symlinked_source_cannot_escape_the_loadout() {
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "x").unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("link")).unwrap();
    fs::write(
        dir.path().join("loadout.toml"),
        "[loadout]\nversion = 1\n[files.a]\nsource = \"link\"\ntarget = \"~/.a\"\n",
    )
    .unwrap();
    let output = Sandbox::new().run(&["validate", dir.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(3));
    assert!(stdout(&output).contains("link resolves outside the loadout"));
}

#[test]
fn doctor_passes_in_an_isolated_home() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["doctor", "--json"]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let checks = report["checks"].as_array().unwrap();
    let check = |name: &str| {
        checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("no {name} check"))
            .clone()
    };
    assert_eq!(check("home")["status"], "pass");
    assert!(
        check("home")["detail"]
            .as_str()
            .unwrap()
            .contains("LOADOUT_HOME")
    );
    assert_eq!(check("platform")["status"], "pass");
    assert!(
        check("state dir")["detail"]
            .as_str()
            .unwrap()
            .contains("not created yet")
    );
    assert!(
        check("machine name")["detail"]
            .as_str()
            .unwrap()
            .contains("from the hostname")
    );
    let git_ok = check("git")["status"] == "pass";
    assert_eq!(report["healthy"], git_ok);
    assert_eq!(output.status.code(), Some(if git_ok { 0 } else { 4 }));
    assert!(
        !sandbox.state.exists(),
        "doctor must not create the state dir"
    );
}

#[test]
fn doctor_reads_a_saved_machine_name() {
    let sandbox = Sandbox::new();
    fs::create_dir(&sandbox.state).unwrap();
    fs::write(
        sandbox.state.join("machine.toml"),
        "name = \"work-laptop\"\n",
    )
    .unwrap();
    let text = stdout(&sandbox.run(&["doctor"]));
    assert!(text.contains("work-laptop (saved in"), "{text}");
}

#[test]
fn doctor_fails_on_a_broken_machine_file_or_missing_home() {
    let sandbox = Sandbox::new();
    fs::create_dir(&sandbox.state).unwrap();
    fs::write(sandbox.state.join("machine.toml"), "name = \"Not Valid\"\n").unwrap();
    let output = sandbox.run(&["doctor"]);
    assert_eq!(output.status.code(), Some(4));
    assert!(stdout(&output).contains("FAIL  machine name"));

    let missing = sandbox.home.join("missing");
    let output = loadout(&["doctor"], &missing, &sandbox.state);
    assert_eq!(output.status.code(), Some(4));
    assert!(stdout(&output).contains("FAIL  home"));
}

#[test]
fn relative_home_override_is_a_runtime_error() {
    let sandbox = Sandbox::new();
    let output = loadout(&["doctor"], Path::new("relative"), &sandbox.state);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("LOADOUT_HOME must be an absolute path")
    );
}
