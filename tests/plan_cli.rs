use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    loadout: PathBuf,
}

impl Sandbox {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let state = dir.path().join("state");
        let loadout = dir.path().join("loadout");
        fs::create_dir(&home).unwrap();
        for (path, text) in files {
            let path = loadout.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        Self {
            _dir: dir,
            home,
            state,
            loadout,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_loadout"))
            .args(args)
            .env("LOADOUT_HOME", &self.home)
            .env("LOADOUT_STATE_DIR", &self.state)
            .output()
            .unwrap()
    }

    fn with_loadout(&self, command: &str, extra: &[&str]) -> Output {
        let mut args = vec![command, "--loadout", self.loadout.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.run(&args)
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .replace('\\', "/")
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

const FILES: &[(&str, &str)] = &[
    (
        "loadout.toml",
        "[loadout]\nversion = 1\n[variables]\nname = \"Dana\"\n\
         [files.git]\nsource = \"files/gitconfig.tmpl\"\ntarget = \"~/.gitconfig\"\n\
         [files.notes]\nsource = \"files/notes\"\ntarget = \"~/notes.md\"\n",
    ),
    ("files/gitconfig.tmpl", "name = {{ name }}\n"),
    ("files/notes", "SOURCE-BYTES-NOT-IN-JSON\n"),
];

#[test]
fn plan_lists_each_change_and_writes_nothing() {
    let sandbox = Sandbox::new(FILES);
    let output = sandbox.with_loadout("plan", &[]);
    let text = stdout(&output);
    assert_eq!(code(&output), 0, "{text}");
    assert!(text.contains("create"), "{text}");
    assert!(text.contains("(files.git)"), "{text}");
    assert!(
        text.contains("2 of 2 actions would change this machine"),
        "{text}"
    );
    assert!(!sandbox.state.exists());
    assert_eq!(fs::read_dir(&sandbox.home).unwrap().count(), 0);
}

#[test]
fn plan_json_carries_targets_and_changes_but_never_content() {
    let sandbox = Sandbox::new(FILES);
    let output = sandbox.with_loadout("plan", &["--json"]);
    assert_eq!(code(&output), 0);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let actions = report["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 2);
    for action in actions {
        assert_eq!(action["change"], "create");
        assert_eq!(action["prior"]["observed"], "absent");
        assert_eq!(action["target"]["type"], "path");
        assert!(action.get("content").is_none());
    }
    assert!(!stdout(&output).contains("SOURCE-BYTES-NOT-IN-JSON"));
}

#[test]
fn apply_then_plan_and_status_show_a_converged_machine() {
    let sandbox = Sandbox::new(FILES);
    let output = sandbox.with_loadout("apply", &[]);
    assert_eq!(code(&output), 0, "{}", stdout(&output));
    assert_eq!(
        fs::read_to_string(sandbox.home.join(".gitconfig")).unwrap(),
        "name = Dana\n"
    );
    assert!(stdout(&output).contains("loadout rollback"));

    let plan = sandbox.with_loadout("plan", &[]);
    assert_eq!(code(&plan), 0);
    assert!(
        stdout(&plan).contains("0 of 2 actions would change this machine"),
        "{}",
        stdout(&plan)
    );
    assert_eq!(code(&sandbox.run(&["status"])), 0);
}

#[test]
fn a_missing_variable_is_invalid_for_plan_and_apply_and_nothing_is_written() {
    let sandbox = Sandbox::new(&[
        (
            "loadout.toml",
            "[loadout]\nversion = 1\n[files.git]\nsource = \"g.tmpl\"\ntarget = \"~/.gitconfig\"\n",
        ),
        ("g.tmpl", "{{ undefined_name }}\n"),
    ]);
    for command in ["plan", "apply"] {
        let output = sandbox.with_loadout(command, &[]);
        let text = stdout(&output);
        assert_eq!(code(&output), 3, "{command}: {text}");
        assert!(text.contains("undefined_name"), "{command}: {text}");
    }
    assert!(!sandbox.state.exists());
    assert!(!sandbox.home.join(".gitconfig").exists());
}

#[test]
fn drift_exits_four_until_apply_force() {
    let sandbox = Sandbox::new(FILES);
    assert_eq!(code(&sandbox.with_loadout("apply", &[])), 0);
    fs::write(sandbox.home.join("notes.md"), "edited by hand\n").unwrap();
    assert_eq!(code(&sandbox.with_loadout("plan", &[])), 4);
    let skipped = sandbox.with_loadout("apply", &[]);
    assert_eq!(code(&skipped), 4, "{}", stdout(&skipped));
    assert!(stdout(&skipped).contains("skipped"));
    assert_eq!(
        fs::read_to_string(sandbox.home.join("notes.md")).unwrap(),
        "edited by hand\n"
    );
    assert_eq!(code(&sandbox.with_loadout("apply", &["--force"])), 0);
    assert_eq!(
        fs::read_to_string(sandbox.home.join("notes.md")).unwrap(),
        "SOURCE-BYTES-NOT-IN-JSON\n"
    );
}

#[test]
fn without_a_checkout_plan_and_apply_say_how_to_name_one() {
    let sandbox = Sandbox::new(&[]);
    for command in ["plan", "apply"] {
        let output = sandbox.run(&[command]);
        assert_eq!(code(&output), 1);
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("--loadout"), "{error}");
    }
    assert!(!sandbox.state.exists());
}

#[test]
fn an_interrupted_run_blocks_plan_and_apply_until_rollback() {
    let sandbox = Sandbox::new(FILES);
    assert_eq!(code(&sandbox.with_loadout("apply", &[])), 0);
    let runs = sandbox.state.join("runs");
    let run = fs::read_dir(&runs).unwrap().next().unwrap().unwrap().path();
    let journal = run.join("journal.json");
    let text = fs::read_to_string(&journal).unwrap();
    fs::write(&journal, text.replace("\"finished\"", "\"running\"")).unwrap();
    for command in ["plan", "apply"] {
        let output = sandbox.with_loadout(command, &[]);
        assert_eq!(code(&output), 4, "{command}: {}", stdout(&output));
        assert!(stdout(&output).contains("loadout rollback"));
    }
    assert_eq!(fs::read_dir(&runs).unwrap().count(), 1, "no new run");
    assert_eq!(code(&sandbox.run(&["rollback"])), 0);
    assert_eq!(code(&sandbox.with_loadout("plan", &[])), 0);
}

#[test]
fn programs_under_isolation_are_blocked_in_plan_and_apply() {
    let sandbox = Sandbox::new(&[(
        "loadout.toml",
        "[loadout]\nversion = 1\n[programs.ripgrep]\nbrew = \"ripgrep\"\napt = \"ripgrep\"\nwinget = \"BurntSushi.ripgrep.MSVC\"\n",
    )]);
    let plan = sandbox.with_loadout("plan", &["--json"]);
    assert_eq!(code(&plan), 4);
    let report: Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(report["actions"][0]["target"]["type"], "none");
    assert_eq!(report["actions"][0]["change"], "blocked");
    let apply = sandbox.with_loadout("apply", &["--elevated"]);
    assert_eq!(code(&apply), 4, "{}", stdout(&apply));
    assert!(!sandbox.state.join("runs").exists());
}
