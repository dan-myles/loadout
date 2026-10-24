use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use loadout::context::Context;
use loadout::plan::{
    Action, Change, Content, FileContent, ItemKind, ItemRef, Observed, Plan, Target,
};
use loadout::platform::Os;
use loadout::state::{self, Run, State};
use serde_json::Value;

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    context: Context,
}

impl Machine {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let state = root.join("state");
        fs::create_dir_all(&home).unwrap();
        let vars = BTreeMap::from([
            ("LOADOUT_HOME", home.clone().into_os_string()),
            ("LOADOUT_STATE_DIR", state.clone().into_os_string()),
        ]);
        let context =
            Context::resolve(Os::current().unwrap(), |name| vars.get(name).cloned(), None).unwrap();
        Self {
            _dir: dir,
            home,
            state,
            context,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_loadout"))
            .args(args)
            .env("LOADOUT_HOME", &self.home)
            .env("LOADOUT_STATE_DIR", &self.state)
            .output()
            .unwrap()
    }

    fn checkout(&self) -> PathBuf {
        self.state.join("loadout")
    }

    fn action(&self, path: &Path, content: &Content) -> Action {
        let target = Target::Path {
            path: path.to_path_buf(),
        };
        let prior = state::observe(&self.context, &self.checkout(), &target).unwrap();
        Action {
            item: ItemRef {
                kind: ItemKind::File,
                id: path.file_name().unwrap().to_string_lossy().into_owned(),
                agent: None,
            },
            target,
            change: if prior == Observed::Absent {
                Change::Create
            } else {
                Change::Replace
            },
            prior,
            content: Some(content.clone()),
        }
    }

    /// Writes each file in one run; with `finish` false the run stops as if the process died.
    fn run(&self, files: &[(&Path, &str)], finish: bool) -> String {
        let pairs: Vec<(Action, Content)> = files
            .iter()
            .map(|(path, text)| {
                let content = Content::File(FileContent {
                    bytes: text.as_bytes().to_vec(),
                    executable: false,
                });
                (self.action(path, &content), content)
            })
            .collect();
        let lock = state::lock(&self.context).unwrap();
        let mut state = State::load(&self.context).unwrap();
        let plan = Plan {
            actions: pairs.iter().map(|(action, _)| action.clone()).collect(),
        };
        let mut run = Run::begin(&self.context, &self.checkout(), lock, &state, &plan).unwrap();
        let id = run.id().to_owned();
        for (action, content) in &pairs {
            run.write(action, content).unwrap();
        }
        if finish {
            run.finish(&mut state).unwrap();
        }
        id
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn status_on_a_fresh_machine_is_healthy_and_creates_nothing() {
    let machine = Machine::new();
    let output = machine.cli(&["status"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("0 owned"), "{text}");
    assert!(text.contains("runs      0"), "{text}");
    assert!(
        !machine.state.exists(),
        "status never creates the state dir"
    );

    let report = json(&machine.cli(&["status", "--json"]));
    assert_eq!(report["healthy"], true);
    assert_eq!(report["records"], Value::Array(Vec::new()));
}

#[test]
fn status_reports_owned_targets_and_drift() {
    let machine = Machine::new();
    let path = machine.home.join(".gitconfig");
    let run = machine.run(&[(&path, "[user]\n")], true);

    let output = machine.cli(&["status", "--json"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report = json(&output);
    assert_eq!(report["healthy"], true);
    assert_eq!(report["records"][0]["health"], "ok");
    assert_eq!(
        report["records"][0]["target"]["path"],
        path.to_str().unwrap()
    );
    assert_eq!(report["records"][0]["item"]["id"], ".gitconfig");
    assert_eq!(report["records"][0]["run"], run.as_str());
    assert_eq!(report["runs"][0]["status"], "finished");

    fs::write(&path, "edited\n").unwrap();
    let output = machine.cli(&["status"]);
    assert_eq!(output.status.code(), Some(4));
    let text = stdout(&output);
    assert!(text.contains("drifted"), "{text}");
    assert!(text.contains("1 not as loadout left them"), "{text}");

    fs::remove_file(&path).unwrap();
    let report = json(&machine.cli(&["status", "--json"]));
    assert_eq!(report["healthy"], false);
    assert_eq!(report["records"][0]["health"], "missing");
}

#[test]
fn an_interrupted_run_shows_in_status_and_rollback_undoes_it() {
    let machine = Machine::new();
    let existing = machine.home.join(".bashrc");
    fs::write(&existing, "user's\n").unwrap();
    let created = machine.home.join(".config/tool/config");
    let run = machine.run(&[(&existing, "loadout's\n"), (&created, "new\n")], false);

    let output = machine.cli(&["status"]);
    assert_eq!(output.status.code(), Some(4));
    let text = stdout(&output);
    assert!(text.contains("interrupted"), "{text}");
    assert!(text.contains(&format!("loadout rollback {run}")), "{text}");
    let report = json(&machine.cli(&["status", "--json"]));
    assert_eq!(report["runs"][0]["status"], "interrupted");
    assert_eq!(report["runs"][0]["done"], 2);

    let output = machine.cli(&["rollback"]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&format!("rolled back run {run}: restored 2")),
        "{text}"
    );
    assert_eq!(fs::read_to_string(&existing).unwrap(), "user's\n");
    assert!(!created.exists());
    assert!(!machine.home.join(".config").exists());

    let output = machine.cli(&["status"]);
    assert_eq!(output.status.code(), Some(0), "{}", stdout(&output));
    assert!(stdout(&output).contains("rolled back"));

    let output = machine.cli(&["rollback"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("already rolled back"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn rollback_of_a_named_run_refuses_to_overwrite_later_edits() {
    let machine = Machine::new();
    let path = machine.home.join(".gitconfig");
    let first = machine.run(&[(&path, "one\n")], true);
    fs::write(&path, "edited\n").unwrap();

    let output = machine.cli(&["rollback", &first]);
    assert_eq!(output.status.code(), Some(4));
    let error = stderr(&output);
    assert!(error.contains("would overwrite changes"), "{error}");
    assert!(error.contains(".gitconfig"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "edited\n");
}

#[test]
fn rollback_errors_are_clear() {
    let machine = Machine::new();
    let output = machine.cli(&["rollback"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("no runs to roll back"),
        "{}",
        stderr(&output)
    );
    assert!(
        !machine.state.exists(),
        "a rollback with nothing to do creates nothing"
    );

    machine.run(&[(&machine.home.join(".x"), "x")], true);
    let output = machine.cli(&["rollback", "../../etc"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("there is no run ../../etc"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn rollback_while_another_run_holds_the_lock_exits_1() {
    let machine = Machine::new();
    machine.run(&[(&machine.home.join(".x"), "x")], true);
    let held = state::lock(&machine.context).unwrap();
    let output = machine.cli(&["rollback"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("another loadout run holds"),
        "{}",
        stderr(&output)
    );
    assert!(machine.home.join(".x").exists());

    let report = json(&machine.cli(&["status", "--json"]));
    assert_eq!(report["runs"][0]["status"], "finished");
    drop(held);
}

#[test]
fn status_reports_an_unreadable_journal_instead_of_hiding_it() {
    let machine = Machine::new();
    let run = machine.run(&[(&machine.home.join(".x"), "x")], true);
    fs::write(
        machine.state.join("runs").join(&run).join("journal.json"),
        "{",
    )
    .unwrap();
    let output = machine.cli(&["status", "--json"]);
    assert_eq!(output.status.code(), Some(4));
    let report = json(&output);
    assert!(report["runs"][0]["status"].is_null());
    assert!(
        report["runs"][0]["error"]
            .as_str()
            .unwrap()
            .contains("journal.json is invalid"),
        "{report:#}"
    );
}

#[test]
fn a_corrupt_state_file_is_an_error_not_an_empty_machine() {
    let machine = Machine::new();
    fs::create_dir_all(&machine.state).unwrap();
    fs::write(machine.state.join("state.json"), "not json").unwrap();
    let output = machine.cli(&["status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("state.json is invalid"),
        "{}",
        stderr(&output)
    );
}

#[cfg(unix)]
#[test]
fn a_state_file_linked_outside_the_state_dir_is_never_read() {
    let machine = Machine::new();
    fs::create_dir_all(&machine.state).unwrap();
    let outside = machine.home.join("secret.json");
    fs::write(&outside, r#"{"version":1,"records":[]}"#).unwrap();
    std::os::unix::fs::symlink(&outside, machine.state.join("state.json")).unwrap();
    let output = machine.cli(&["status"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("resolves outside"),
        "{}",
        stderr(&output)
    );
}
