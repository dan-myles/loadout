//! The whole command surface in one machine's life, through the real binary: `init` from a
//! `file://` remote, then `plan`, `apply`, `status`, `update` after a new commit, and
//! `rollback`. Every run uses `LOADOUT_HOME` and `LOADOUT_STATE_DIR` in a temp dir, so nothing
//! on the host is read or written, no scheduler is asked anything (`--no-service`), and
//! self-update is refused by isolation before any network request.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=loadout test",
            "-c",
            "user.email=test@loadout.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git must be installed to run these tests");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    remote: PathBuf,
    work: PathBuf,
}

impl Machine {
    /// A bare remote holding `manifest` and one user file, and an empty home and state dir.
    fn new(manifest: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        let work = dir.path().join("work");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "--bare", "--quiet"]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        git(
            dir.path(),
            &["clone", "--quiet", remote.to_str().unwrap(), "work"],
        );
        git(&work, &["checkout", "--quiet", "-B", "main"]);
        fs::create_dir(work.join("files")).unwrap();
        fs::write(work.join("loadout.toml"), manifest).unwrap();
        fs::write(work.join("files").join("notes"), "first\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", "first"]);
        git(&work, &["push", "--quiet", "-u", "origin", "main"]);
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        let state = dir.path().join("state");
        Self {
            _dir: dir,
            home,
            state,
            remote,
            work,
        }
    }

    fn url(&self) -> String {
        let path = self.remote.to_str().unwrap().replace('\\', "/");
        if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        }
    }

    fn push(&self, text: &str) {
        fs::write(self.work.join("files").join("notes"), text).unwrap();
        git(&self.work, &["commit", "--quiet", "-am", "notes"]);
        git(&self.work, &["push", "--quiet", "origin", "main"]);
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_loadout"))
            .args(args)
            .env("LOADOUT_HOME", &self.home)
            .env("LOADOUT_STATE_DIR", &self.state)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap()
    }

    fn notes(&self) -> Option<String> {
        fs::read_to_string(self.home.join("notes.md")).ok()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

const FILES: &str = "[loadout]\nversion = 1\n\
                     [files.notes]\nsource = \"files/notes\"\ntarget = \"~/notes.md\"\n";

#[test]
fn init_plan_apply_status_update_and_rollback_run_one_machine_end_to_end() {
    let machine = Machine::new(&format!("{FILES}[update]\ncli = true\n"));

    let init = machine.run(&["init", &machine.url(), "--name", "box", "--no-service"]);
    assert_eq!(code(&init), Some(0), "{}", text(&init));
    assert!(
        machine.state.join("loadout").join(".git").is_dir(),
        "{}",
        text(&init)
    );
    assert_eq!(
        machine.notes().as_deref(),
        Some("first\n"),
        "{}",
        text(&init)
    );

    let plan = machine.run(&["plan"]);
    assert_eq!(code(&plan), Some(0), "{}", text(&plan));
    let apply = machine.run(&["apply"]);
    assert_eq!(code(&apply), Some(0), "{}", text(&apply));
    let status = machine.run(&["status"]);
    assert_eq!(code(&status), Some(0), "{}", text(&status));

    machine.push("second\n");
    let update = machine.run(&["update"]);
    assert_eq!(code(&update), Some(0), "{}", text(&update));
    let shown = text(&update);
    assert!(shown.contains("fetched: fast-forwarded"), "{shown}");
    assert_eq!(machine.notes().as_deref(), Some("second\n"));
    // `[update] cli = true`: the self-update is attempted after the apply, and isolation skips
    // it visibly without changing the exit.
    assert!(
        shown.contains("cli: not updated:") && shown.contains("LOADOUT_HOME"),
        "{shown}"
    );

    let rollback = machine.run(&["rollback"]);
    assert_eq!(code(&rollback), Some(0), "{}", text(&rollback));
    assert_eq!(
        machine.notes().as_deref(),
        Some("first\n"),
        "rolling back the update's run restores what init applied: {}",
        text(&rollback)
    );
}

#[test]
fn update_never_tries_to_update_the_cli_when_the_loadout_turns_it_off() {
    let machine = Machine::new(&format!("{FILES}[update]\ncli = false\n"));
    let init = machine.run(&["init", &machine.url(), "--name", "box", "--no-service"]);
    assert_eq!(code(&init), Some(0), "{}", text(&init));

    let update = machine.run(&["update"]);
    assert_eq!(code(&update), Some(0), "{}", text(&update));
    assert!(
        !text(&update).contains("cli:"),
        "no self-update line at all: {}",
        text(&update)
    );
}

#[test]
fn a_blocked_update_applies_nothing_and_never_reaches_the_self_update() {
    let machine = Machine::new(&format!("{FILES}[update]\ncli = true\n"));
    let init = machine.run(&["init", &machine.url(), "--name", "box", "--no-service"]);
    assert_eq!(code(&init), Some(0), "{}", text(&init));
    let clone = machine.state.join("loadout");
    fs::write(clone.join("files").join("notes"), "local edit\n").unwrap();
    machine.push("second\n");

    let update = machine.run(&["update"]);
    assert_eq!(code(&update), Some(4), "{}", text(&update));
    assert!(text(&update).contains("blocked:"), "{}", text(&update));
    assert!(!text(&update).contains("cli:"), "{}", text(&update));
    assert_eq!(
        machine.notes().as_deref(),
        Some("first\n"),
        "nothing applied"
    );
}
