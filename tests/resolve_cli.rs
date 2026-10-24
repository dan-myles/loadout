use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/resolve")
        .join(name)
}

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("home")).unwrap();
        Self { dir }
    }

    fn validate(&self, loadout: &Path, json: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_loadout"));
        command
            .arg("validate")
            .arg(loadout)
            .env("LOADOUT_HOME", self.dir.path().join("home"))
            .env("LOADOUT_STATE_DIR", self.dir.path().join("state"));
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    }

    fn untouched(&self) {
        assert_eq!(
            fs::read_dir(self.dir.path().join("home")).unwrap().count(),
            0,
            "validate writes nothing into the home"
        );
        assert!(!self.dir.path().join("state").exists());
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .replace('\\', "/")
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn edited_fleet(edits: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("fleet");
    copy(&fixture("fleet"), &root);
    for (path, text) in edits {
        fs::write(root.join(path), text).unwrap();
    }
    (dir, root)
}

const STATIC_COLON_WARNING: &str =
    "loadout.toml: warning: files.notes.target: \"notes:today\" contains ':'";

#[test]
fn the_fleet_resolves_cleanly_for_every_machine_on_every_os() {
    let sandbox = Sandbox::new();
    let output = sandbox.validate(&fixture("fleet"), false);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "{text}");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with(STATIC_COLON_WARNING), "{text}");
    assert!(
        lines[1].ends_with("tests/resolve/fleet (7 layers, 18 items, 1 warnings)"),
        "{text}"
    );
    sandbox.untouched();

    let output = sandbox.validate(&fixture("fleet"), true);
    assert_eq!(output.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], true);
    assert_eq!(report["diagnostics"].as_array().unwrap().len(), 1);
    sandbox.untouched();
}

#[test]
fn dropping_the_windows_override_fails_every_machine_on_windows_only() {
    let (_dir, root) = edited_fleet(&[(
        "os/windows.toml",
        "[files.shell]\ntarget = \"~/Documents/shellrc\"\n",
    )]);
    let output = Sandbox::new().validate(&root, false);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains(
            "loadout.toml: error: files.notes.target: \"notes:today\" contains ':', which Windows paths cannot hold (drive, stream, or reserved character), so ~/notes:today has no valid Windows path for files.notes; remove or override it in os/windows.toml, or add a when condition (on mini: windows; studio: windows; tower: windows; any other unlisted machine: windows)"
        ),
        "{text}"
    );
    assert!(text.contains("invalid: "), "{text}");
}

#[test]
fn a_machine_whose_tag_order_puts_a_colliding_target_last_is_the_only_one_that_fails() {
    let (_dir, root) = edited_fleet(&[
        (
            "tags/work.toml",
            &format!(
                "{}\n[files.notes]\ntarget = \"~/.shellrc\"\n",
                fs::read_to_string(fixture("fleet/tags/work.toml")).unwrap()
            ),
        ),
        (
            "tags/laptop.toml",
            &format!(
                "{}\n[files.notes]\ntarget = \"~/notes-laptop\"\n",
                fs::read_to_string(fixture("fleet/tags/laptop.toml")).unwrap()
            ),
        ),
    ]);
    let output = Sandbox::new().validate(&root, false);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains(
            "tags/work.toml: error: files.notes.target: files.notes (~/.shellrc) and files.shell (~/.shellrc) both write ~/.shellrc (on tower: macos, linux)"
        ),
        "tower lists work last, so work's target wins there: {text}"
    );
    assert!(
        !text
            .lines()
            .any(|line| line.contains("both write") && line.contains("studio")),
        "studio lists laptop last: {text}"
    );
    assert!(
        text.contains("files.notes: adds this item here because no broader layer declares it for this machine, but it is incomplete: missing source (on studio: windows; tower: windows)"),
        "os/windows.toml removed notes, so a tag edit on Windows is an incomplete add: {text}"
    );
}

#[test]
fn each_conflict_is_reported_once_with_the_machines_and_oses_it_hits() {
    let sandbox = Sandbox::new();
    let output = sandbox.validate(&fixture("conflicts"), false);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{text}");
    for expected in [
        "tags/work.toml: error: mcp.tools: has both command (stdio) and url (remote) after merging loadout.toml, tags/work.toml; to switch transports, write replace = true with the complete server in the narrower layer (on studio: macos, linux, windows)",
        "tags/work.toml: error: instructions.work: instructions.main, instructions.work all reach claude; an agent takes one instructions file, so narrow them with agents = [...] (on studio: macos, linux, windows)",
        "tags/work.toml: error: files.cfg-native.target: files.cfg-native ({config}/app/settings) and files.cfg (~/.config/app/settings) both write ~/.config/app/settings (on studio: linux)",
        "tags/gaming.toml: error: skills.half: adds this item here because no broader layer declares it for this machine, but it is incomplete: missing source (on tower: macos, linux, windows)",
        "tags/gaming.toml: warning: mcp.extra.agents: names \"cursor\", which is not enabled for this machine, so it gets nothing (on tower: macos, linux, windows)",
        "tags/gaming.toml: error: files.profile-upper.target: files.profile-upper (~/.Profile) and files.profile (~/.profile) both write ~/.Profile (on tower: macos, windows)",
        "tags/gaming.toml: error: files.colon.target: \"scores:high\" contains ':', which Windows paths cannot hold (drive, stream, or reserved character), so ~/scores:high has no valid Windows path for files.colon; remove or override it in os/windows.toml, or add a when condition (on tower: windows)",
        "invalid: ",
    ] {
        assert!(
            text.lines().any(|line| line.contains(expected)),
            "missing {expected:?} in:\n{text}"
        );
    }
    assert!(
        !text.contains("~/notes:2026 has no valid Windows path"),
        "os/windows.toml fixes the global target for every machine but tower: {text}"
    );
    assert!(text.ends_with("(6 errors, 3 warnings)\n"), "{text}");
    sandbox.untouched();
}

#[test]
fn resolution_diagnostics_in_json_carry_their_layer_file_and_key() {
    let output = Sandbox::new().validate(&fixture("conflicts"), true);
    assert_eq!(output.status.code(), Some(3));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], false);
    assert_eq!(report["layers"].as_array().unwrap().len(), 4);
    let diagnostics = report["diagnostics"].as_array().unwrap();
    let collision = diagnostics
        .iter()
        .find(|diagnostic| diagnostic["key"] == "files.cfg-native.target")
        .expect("the Linux collision is reported");
    assert_eq!(collision["severity"], "error");
    assert_eq!(
        collision["file"].as_str().unwrap().replace('\\', "/"),
        "tags/work.toml"
    );
    assert!(
        collision["message"]
            .as_str()
            .unwrap()
            .ends_with("(on studio: linux)")
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic["severity"] == "error")
            .count(),
        6
    );
}
