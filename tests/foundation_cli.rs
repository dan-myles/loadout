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
        "tag \"missing-tag\" has no valid layer file",
        "tag \"work\" is listed twice",
        "machines.box.tags: tag \"work\" has no valid layer file tags/work.toml",
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

fn validate_tree(files: &[(&str, &str)]) -> (i32, String) {
    let dir = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let output = Sandbox::new().run(&["validate", dir.path().to_str().unwrap()]);
    (output.status.code().unwrap(), stdout(&output))
}

#[test]
fn nul_in_a_target_is_rejected_everywhere() {
    let (code, text) = validate_tree(&[
        (
            "loadout.toml",
            "[loadout]\nversion = 1\n[files.x]\nsource = \"f\"\ntarget = \"~/a\\u0000b\"\n",
        ),
        ("f", "x"),
    ]);
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("files.x.target"), "{text}");
}

#[test]
fn windows_path_rules_apply_only_where_windows_is_reached() {
    let (code, text) = validate_tree(&[
        (
            "loadout.toml",
            "[loadout]\nversion = 1\n[files.anywhere]\nsource = \"f\"\ntarget = \"~/C:/Windows/x\"\n[files.unix]\nsource = \"f\"\ntarget = \"~/notes:2026\"\nwhen = { os = [\"linux\", \"macos\"] }\n",
        ),
        ("f", "x"),
        (
            "os/linux.toml",
            "[files.colon]\nsource = \"f\"\ntarget = \"~/.local/share/a:b\"\n",
        ),
    ]);
    assert_eq!(code, 0, "{text}");
    assert!(
        text.contains("warning: files.anywhere.target: \"C:\" contains ':'"),
        "{text}"
    );
    assert!(!text.contains("files.unix"), "{text}");
    assert!(!text.contains("files.colon"), "{text}");

    let (code, text) = validate_tree(&[
        (
            "loadout.toml",
            "[loadout]\nversion = 1\n[agents.custom.dv]\nevidence = \"e\"\nskills = { windows = \"{config}/D:/evil\", linux = \"~/dv:skills\" }\n",
        ),
        ("f", "x"),
        (
            "os/windows.toml",
            "[files.device]\nsource = \"f\"\ntarget = \"~/Documents/NUL.txt\"\n",
        ),
    ]);
    assert_eq!(code, 3, "{text}");
    assert!(
        text.contains("error: agents.custom.dv.skills.windows: \"D:\" contains ':'"),
        "{text}"
    );
    assert!(!text.contains("skills.linux"), "{text}");
    assert!(
        text.contains(
            "os/windows.toml: error: files.device.target: \"NUL.txt\" is a Windows device name"
        ),
        "{text}"
    );
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

#[cfg(unix)]
fn skill_case(build: impl FnOnce(&Path, &Path)) -> (i32, String) {
    let scratch = tempfile::tempdir().unwrap();
    let outside = scratch.path().join("outside");
    let root = scratch.path().join("loadout");
    fs::create_dir_all(outside.join("dir")).unwrap();
    fs::write(outside.join("key"), "synthetic outside file").unwrap();
    fs::create_dir_all(root.join("skill")).unwrap();
    fs::write(
        root.join("loadout.toml"),
        "[loadout]\nversion = 1\n[skills.x]\nsource = \"skill\"\n",
    )
    .unwrap();
    build(&root, &outside);
    let output = Sandbox::new().run(&["validate", root.to_str().unwrap(), "--json"]);
    (output.status.code().unwrap(), stdout(&output))
}

#[cfg(unix)]
#[test]
fn skill_contents_cannot_escape_through_links() {
    use std::os::unix::fs::symlink;

    let (code, text) = skill_case(|root, outside| {
        symlink(outside.join("key"), root.join("skill/SKILL.md")).unwrap();
    });
    assert_eq!(code, 3, "escaping SKILL.md: {text}");
    assert!(
        text.contains("skill/SKILL.md links outside the loadout"),
        "{text}"
    );

    let (code, text) = skill_case(|root, outside| {
        fs::write(root.join("skill/SKILL.md"), "x").unwrap();
        fs::create_dir(root.join("skill/scripts")).unwrap();
        symlink(outside.join("key"), root.join("skill/scripts/leak")).unwrap();
    });
    assert_eq!(code, 3, "nested escaping file: {text}");
    assert!(
        text.contains("skill/scripts/leak links outside the loadout"),
        "{text}"
    );

    let (code, text) = skill_case(|root, outside| {
        fs::write(root.join("skill/SKILL.md"), "x").unwrap();
        symlink(outside.join("dir"), root.join("skill/dir")).unwrap();
    });
    assert_eq!(code, 3, "escaping folder: {text}");

    let (code, text) = skill_case(|root, _| {
        fs::write(root.join("skill/SKILL.md"), "x").unwrap();
        symlink(root.join("skill"), root.join("skill/loop")).unwrap();
    });
    assert_eq!(code, 3, "self loop: {text}");
    assert!(text.contains("skill/loop links to a folder"), "{text}");

    let (code, text) = skill_case(|root, _| {
        fs::write(root.join("skill/SKILL.md"), "x").unwrap();
        symlink(root.join("missing"), root.join("skill/broken")).unwrap();
    });
    assert_eq!(code, 3, "broken link: {text}");
    assert!(text.contains("skill/broken is a broken link"), "{text}");

    let (code, text) = skill_case(|root, _| {
        fs::write(root.join("shared.md"), "x").unwrap();
        symlink(root.join("shared.md"), root.join("skill/SKILL.md")).unwrap();
    });
    assert_eq!(code, 0, "in-loadout file link stays valid: {text}");
}

const MINIMAL: &str = "[loadout]\nversion = 1\n";
const BAD_LAYER: &str = "[nonsense]\nx = 1\n";

#[test]
fn a_layer_path_that_is_not_a_folder_is_an_error() {
    for dir in ["os", "tags", "machines"] {
        let (code, text) = validate_tree(&[("loadout.toml", MINIMAL), (dir, "not a folder")]);
        assert_eq!(code, 3, "{dir}: {text}");
        assert!(
            text.contains(&format!(
                "{dir}: error: must be a folder of <name>.toml layer files"
            )),
            "{text}"
        );
    }
}

#[test]
fn only_known_folder_metadata_is_ignored() {
    let (code, text) = validate_tree(&[
        ("loadout.toml", MINIMAL),
        ("os/.DS_Store", "finder"),
        ("tags/.gitkeep", ""),
        ("machines/.gitkeep", ""),
    ]);
    assert_eq!(code, 0, "{text}");

    let (code, text) = validate_tree(&[("loadout.toml", MINIMAL), ("os/.hidden.toml", "")]);
    assert_eq!(code, 3, "{text}");
}

#[cfg(unix)]
#[test]
fn an_unreadable_layer_folder_is_an_error_not_an_absent_layer() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("loadout.toml"), MINIMAL).unwrap();
    fs::create_dir(dir.path().join("os")).unwrap();
    fs::write(dir.path().join("os/linux.toml"), BAD_LAYER).unwrap();
    fs::set_permissions(dir.path().join("os"), fs::Permissions::from_mode(0o000)).unwrap();
    let readable = fs::read_dir(dir.path().join("os")).is_ok();
    let output = Sandbox::new().run(&["validate", dir.path().to_str().unwrap(), "--json"]);
    fs::set_permissions(dir.path().join("os"), fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("running with permission to read a 000 folder (root?); skipped");
        return;
    }
    assert_eq!(output.status.code(), Some(3), "{}", stdout(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], false);
    assert!(stdout(&output).contains("cannot read"));
}

#[cfg(unix)]
#[test]
fn manifests_and_layer_folders_cannot_live_outside_the_loadout() {
    use std::os::unix::fs::symlink;

    let scratch = tempfile::tempdir().unwrap();
    let outside = scratch.path().join("outside");
    fs::create_dir_all(outside.join("os")).unwrap();
    fs::write(outside.join("linux.toml"), "").unwrap();
    fs::write(outside.join("loadout.toml"), MINIMAL).unwrap();

    let case = |name: &str, build: &dyn Fn(&Path)| {
        let root = scratch.path().join(name);
        fs::create_dir_all(&root).unwrap();
        build(&root);
        let output = Sandbox::new().run(&["validate", root.to_str().unwrap()]);
        (output.status.code().unwrap(), stdout(&output))
    };

    let (code, text) = case("layer-file", &|root| {
        fs::write(root.join("loadout.toml"), MINIMAL).unwrap();
        fs::create_dir(root.join("os")).unwrap();
        symlink(outside.join("linux.toml"), root.join("os/linux.toml")).unwrap();
    });
    assert_eq!(code, 3, "{text}");
    assert!(
        text.contains("os/linux.toml: error: resolves outside the loadout"),
        "{text}"
    );

    let (code, text) = case("layer-dir", &|root| {
        fs::write(root.join("loadout.toml"), MINIMAL).unwrap();
        symlink(outside.join("os"), root.join("os")).unwrap();
    });
    assert_eq!(code, 3, "{text}");
    assert!(
        text.contains("os: error: resolves outside the loadout"),
        "{text}"
    );

    let (code, text) = case("root-file", &|root| {
        symlink(outside.join("loadout.toml"), root.join("loadout.toml")).unwrap();
    });
    assert_eq!(code, 3, "{text}");
    assert!(
        text.contains("loadout.toml: error: resolves outside the loadout"),
        "{text}"
    );

    let (code, text) = case("inside-link", &|root| {
        fs::write(root.join("loadout.toml"), MINIMAL).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::write(root.join("shared/linux.toml"), "").unwrap();
        fs::create_dir(root.join("os")).unwrap();
        symlink(root.join("shared/linux.toml"), root.join("os/linux.toml")).unwrap();
    });
    assert_eq!(code, 0, "a link inside the loadout stays valid: {text}");
}

#[test]
fn machine_tags_must_be_ids_that_name_a_loaded_layer() {
    let (code, text) = validate_tree(&[
        (
            "loadout.toml",
            "[loadout]\nversion = 1\n[machines.box]\ntags = [\"../other\", \"Work\", \"../loadout\"]\n",
        ),
        ("other.toml", ""),
        ("tags/work.toml", ""),
    ]);
    assert_eq!(code, 3, "{text}");
    for tag in ["../other", "Work", "../loadout"] {
        assert!(
            text.contains(&format!(
                "machines.box.tags: tag {tag:?} must be lowercase letters"
            )),
            "{tag}: {text}"
        );
    }

    let (code, text) = validate_tree(&[
        (
            "loadout.toml",
            "[loadout]\nversion = 1\n[machines.box]\ntags = [\"work\"]\n",
        ),
        ("tags/work.toml", ""),
    ]);
    assert_eq!(code, 0, "{text}");
}

#[test]
fn remote_urls_and_map_pointers_must_be_well_formed() {
    let (code, text) = validate_text(
        "[loadout]\nversion = 1\n[mcp.x]\nurl = \"https://\"\n[agents.custom.c]\nevidence = \"e\"\nmcp = { linux = \"~/c.json\", format = \"json\", map = \"/mcp~2servers\", shape = \"cursor-json\", env_syntax = \"none\" }\n",
    );
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("mcp.x.url: needs a host name"), "{text}");
    assert!(
        text.contains("agents.custom.c.mcp.map: must be a pointer"),
        "{text}"
    );
}

fn validate_url(url: &str) -> (i32, String, Value) {
    let text = format!("[loadout]\nversion = 1\n[mcp.x]\nurl = \"{url}\"\n");
    let (code, human) = validate_text(&text);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("loadout.toml"), &text).unwrap();
    let output = Sandbox::new().run(&["validate", dir.path().to_str().unwrap(), "--json"]);
    assert_eq!(
        output.status.code().unwrap(),
        code,
        "{url}: human and JSON exits differ"
    );
    (code, human, serde_json::from_slice(&output.stdout).unwrap())
}

#[test]
fn bracketed_hosts_must_be_real_ipv6_addresses() {
    for bad in [
        "https://[::::]/mcp",
        "https://[abcd]/mcp",
        "https://[abc]/mcp",
        "https://[1:2:3]/mcp",
        "https://[::1::2]/mcp",
        "https://[12345::]/mcp",
        "https://[127.0.0.1]/mcp",
    ] {
        let (code, human, json) = validate_url(bad);
        assert_eq!(code, 3, "{bad}: {human}");
        assert!(human.contains("mcp.x.url: host ["), "{bad}: {human}");
        assert_eq!(json["valid"], false, "{bad}");
        assert_eq!(json["diagnostics"][0]["key"], "mcp.x.url", "{bad}");
    }
    for good in [
        "https://[2001:db8::1]:65535/mcp",
        "https://[::ffff:192.0.2.1]/mcp",
        "https://example.com./mcp",
    ] {
        let (code, human, json) = validate_url(good);
        assert_eq!(code, 0, "{good}: {human}");
        assert_eq!(json["valid"], true, "{good}");
    }
}

#[cfg(unix)]
#[test]
fn a_root_manifest_that_is_not_a_regular_file_is_invalid_not_a_hang() {
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant};

    let mkfifo = |path: &Path| {
        let status = Command::new("mkfifo").arg(path).status().unwrap();
        assert!(status.success());
    };
    let scratch = tempfile::tempdir().unwrap();
    let fifo = scratch.path().join("fifo");
    fs::create_dir(&fifo).unwrap();
    mkfifo(&fifo.join("loadout.toml"));
    let linked = scratch.path().join("linked");
    fs::create_dir(&linked).unwrap();
    mkfifo(&linked.join("pipe"));
    symlink(linked.join("pipe"), linked.join("loadout.toml")).unwrap();
    let dir = scratch.path().join("dir");
    fs::create_dir_all(dir.join("loadout.toml")).unwrap();

    for root in [&fifo, &linked, &dir] {
        for json in [false, true] {
            let mut args = vec!["validate", root.to_str().unwrap()];
            if json {
                args.push("--json");
            }
            let sandbox = Sandbox::new();
            let mut child = Command::new(env!("CARGO_BIN_EXE_loadout"))
                .args(&args)
                .env("LOADOUT_HOME", &sandbox.home)
                .env("LOADOUT_STATE_DIR", &sandbox.state)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let started = Instant::now();
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if started.elapsed() > Duration::from_secs(10) {
                    child.kill().unwrap();
                    panic!("validate {args:?} hung on a non-regular loadout.toml");
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            let output = child.wait_with_output().unwrap();
            assert_eq!(status.code(), Some(3), "{args:?}");
            assert!(
                stdout(&output).contains("must be a regular file"),
                "{args:?}: {}",
                stdout(&output)
            );
        }
    }

    let regular = scratch.path().join("regular");
    fs::create_dir_all(regular.join("shared")).unwrap();
    fs::write(regular.join("shared/root.toml"), MINIMAL).unwrap();
    symlink(
        regular.join("shared/root.toml"),
        regular.join("loadout.toml"),
    )
    .unwrap();
    let output = Sandbox::new().run(&["validate", regular.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
}

#[cfg(unix)]
#[test]
fn tag_references_never_consult_a_rejected_outside_tags_folder() {
    use std::os::unix::fs::symlink;

    let scratch = tempfile::tempdir().unwrap();
    let outside = scratch.path().join("outside-tags");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("work.toml"), "").unwrap();
    let root = scratch.path().join("loadout");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("loadout.toml"),
        "[loadout]\nversion = 1\n[machines.box]\ntags = [\"work\"]\n",
    )
    .unwrap();
    symlink(&outside, root.join("tags")).unwrap();
    let output = Sandbox::new().run(&["validate", root.to_str().unwrap()]);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(
        text.contains("tags: error: resolves outside the loadout"),
        "{text}"
    );
    assert!(
        text.contains("tag \"work\" has no valid layer file tags/work.toml"),
        "the outside work.toml must not satisfy the reference: {text}"
    );
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
fn isolated_home_ignores_ambient_xdg_dirs() {
    let sandbox = Sandbox::new();
    let real = tempfile::tempdir().unwrap();
    let saved = real.path().join("state/loadout");
    fs::create_dir_all(&saved).unwrap();
    fs::write(
        saved.join("machine.toml"),
        "name = \"outside-isolated-home\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_loadout"))
        .args(["doctor", "--json"])
        .env("LOADOUT_HOME", &sandbox.home)
        .env_remove("LOADOUT_STATE_DIR")
        .env("XDG_CONFIG_HOME", real.path().join("config"))
        .env("XDG_STATE_HOME", real.path().join("state"))
        .env("CODEX_HOME", real.path().join("codex"))
        .output()
        .unwrap();
    let text = stdout(&output);
    assert!(!text.contains("outside-isolated-home"), "{text}");
    assert!(!text.contains(real.path().to_str().unwrap()), "{text}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let detail = |name: &str| {
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["name"] == name)
            .unwrap()["detail"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(detail("config dir").starts_with(sandbox.home.to_str().unwrap()));
    assert!(detail("state dir").starts_with(sandbox.home.to_str().unwrap()));
}

#[test]
fn empty_home_override_is_not_reported_as_isolation() {
    let sandbox = Sandbox::new();
    let output = Command::new(env!("CARGO_BIN_EXE_loadout"))
        .arg("doctor")
        .env("HOME", &sandbox.home)
        .env("LOADOUT_HOME", "")
        .env("LOADOUT_STATE_DIR", &sandbox.state)
        .output()
        .unwrap();
    let text = stdout(&output);
    assert!(text.contains(sandbox.home.to_str().unwrap()), "{text}");
    assert!(!text.contains("from LOADOUT_HOME"), "{text}");
}

#[cfg(unix)]
#[test]
fn doctor_lists_only_executable_managers() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    let bin = tempfile::tempdir().unwrap();
    for (name, mode) in [("npm", 0o644), ("cargo", 0o755)] {
        let path = bin.path().join(name);
        fs::write(&path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_loadout"))
        .args(["doctor", "--json"])
        .env("PATH", bin.path())
        .env("LOADOUT_HOME", &sandbox.home)
        .env("LOADOUT_STATE_DIR", &sandbox.state)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let managers = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "managers")
        .unwrap()["detail"]
        .clone();
    assert_eq!(managers, "cargo");
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
