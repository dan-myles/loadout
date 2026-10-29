//! Profiles chosen on the machine, through the real binary: `init --profile`, `loadout profile`,
//! and the plan, apply, and sync that follow, for a machine the loadout never declares. Every run
//! uses `LOADOUT_HOME` and `LOADOUT_STATE_DIR` in a temp dir and `--no-service`, and stdin is not
//! a terminal, so nothing ever asks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn git(dir: &Path, args: &[&str]) {
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
}

const BASE: &str = "[loadout]\nversion = 1\n\
                    [files.base]\nsource = \"files/base\"\ntarget = \"~/base.md\"\n";

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    remote: PathBuf,
    work: PathBuf,
}

impl Machine {
    /// A remote whose global layer places `~/base.md` and whose `development` and `work` profiles
    /// each place their own file, with `extra` appended to `loadout.toml`.
    fn new(extra: &str) -> Self {
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
        fs::create_dir_all(work.join("files")).unwrap();
        fs::create_dir_all(work.join("tags")).unwrap();
        fs::write(work.join("loadout.toml"), format!("{BASE}{extra}")).unwrap();
        fs::write(work.join("files").join("base"), "base\n").unwrap();
        for profile in ["development", "work"] {
            fs::write(work.join("files").join(profile), format!("{profile}\n")).unwrap();
            fs::write(
                work.join("tags").join(format!("{profile}.toml")),
                format!(
                    "[files.{profile}]\nsource = \"files/{profile}\"\ntarget = \"~/{profile}.md\"\n"
                ),
            )
            .unwrap();
        }
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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_loadout"))
            .args(args)
            .env("LOADOUT_HOME", &self.home)
            .env("LOADOUT_STATE_DIR", &self.state)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn init(&self, extra: &[&str]) -> Output {
        let url = self.url();
        let mut args = vec!["init", url.as_str(), "--no-service"];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn has(&self, file: &str) -> bool {
        self.home.join(file).exists()
    }

    fn machine_file(&self) -> String {
        fs::read_to_string(self.state.join("machine.toml")).unwrap_or_default()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[track_caller]
fn assert_code(output: &Output, code: i32) {
    assert_eq!(output.status.code(), Some(code), "{}", text(output));
}

#[test]
fn an_undeclared_machine_gets_its_chosen_profile_on_init_and_every_later_run() {
    let machine = Machine::new("");
    let init = machine.init(&["--profile", "development"]);
    assert_code(&init, 0);
    assert!(text(&init).contains("profiles: development"), "{}", text(&init));
    assert!(machine.has("base.md") && machine.has("development.md"));
    assert!(!machine.has("work.md"), "only the chosen profile applies");
    assert!(
        machine.machine_file().contains("profiles = [\"development\"]"),
        "{}",
        machine.machine_file()
    );

    let plan = machine.run(&["plan"]);
    assert_code(&plan, 0);
    let shown = text(&plan);
    assert!(
        shown.contains("development.md") && shown.contains("0 of 2 actions would change"),
        "plan reads the same profiles init applied: {shown}"
    );

    fs::write(machine.work.join("files").join("development"), "v2\n").unwrap();
    git(&machine.work, &["commit", "--quiet", "-am", "v2"]);
    git(&machine.work, &["push", "--quiet", "origin", "main"]);
    let sync = machine.run(&["sync"]);
    assert_code(&sync, 0);
    assert_eq!(
        fs::read_to_string(machine.home.join("development.md")).unwrap(),
        "v2\n"
    );
}

#[test]
fn without_a_terminal_init_chooses_no_profile_and_profile_set_changes_it_later() {
    let machine = Machine::new("");
    let init = machine.init(&[]);
    assert_code(&init, 0);
    let shown = text(&init);
    assert!(shown.contains("no profiles chosen"), "{shown}");
    assert!(
        shown.contains("profiles in this loadout: development, work"),
        "{shown}"
    );
    assert!(machine.has("base.md"));
    assert!(!machine.has("development.md") && !machine.has("work.md"));
    assert!(!machine.machine_file().contains("profiles"));

    let list = machine.run(&["profile"]);
    assert_code(&list, 0);
    assert!(
        text(&list).contains("this machine has no profiles chosen"),
        "{}",
        text(&list)
    );

    assert_code(&machine.run(&["profile", "set", "work,development"]), 0);
    assert_code(&machine.run(&["apply"]), 0);
    assert!(machine.has("work.md") && machine.has("development.md"));

    let json = machine.run(&["profile", "list", "--json"]);
    assert_code(&json, 0);
    let listing: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(listing["profiles"][0]["name"], "development");
    assert_eq!(listing["profiles"][0]["chosen"], true);
    assert_eq!(listing["profiles"][0]["declared"], false);

    assert_code(&machine.run(&["profile", "set"]), 0);
    assert_code(&machine.run(&["apply"]), 0);
    assert!(machine.has("base.md"));
    assert!(
        !machine.has("work.md") && !machine.has("development.md"),
        "clearing the profiles removes what they placed"
    );

    let choose = machine.run(&["profile", "choose"]);
    assert_code(&choose, 1);
    assert!(
        text(&choose).contains("use `loadout profile set PROFILE...`"),
        "{}",
        text(&choose)
    );
}

#[test]
fn an_unknown_profile_changes_and_applies_nothing() {
    let machine = Machine::new("");
    let init = machine.init(&["--profile", "gaming"]);
    assert_code(&init, 3);
    assert!(
        text(&init).contains("no profile gaming in this loadout"),
        "{}",
        text(&init)
    );
    assert!(!machine.has("base.md"), "init applied nothing");
    assert!(!machine.machine_file().contains("profiles"));

    assert_code(&machine.run(&["profile", "set", "development"]), 0);
    let before = machine.machine_file();
    let set = machine.run(&["profile", "set", "development", "gaming"]);
    assert_code(&set, 3);
    assert!(text(&set).contains("nothing changed"), "{}", text(&set));
    assert_eq!(machine.machine_file(), before);
}

#[test]
fn a_chosen_profile_the_loadout_drops_blocks_the_sync_instead_of_removing_its_items() {
    let machine = Machine::new("");
    assert_code(&machine.init(&["--profile", "development"]), 0);
    git(&machine.work, &["rm", "--quiet", "tags/development.toml"]);
    git(&machine.work, &["commit", "--quiet", "-m", "drop development"]);
    git(&machine.work, &["push", "--quiet", "origin", "main"]);

    let sync = machine.run(&["sync"]);
    assert_code(&sync, 3);
    assert!(
        text(&sync).contains("this machine uses profile \"development\""),
        "{}",
        text(&sync)
    );
    assert!(machine.has("development.md"), "nothing was removed");

    assert_code(&machine.run(&["profile", "set"]), 0);
    assert_code(&machine.run(&["sync"]), 0);
    assert!(!machine.has("development.md"));
}

#[test]
fn a_declared_machine_keeps_its_tags_and_adds_its_chosen_profiles() {
    let machine = Machine::new("[machines.box]\ntags = [\"work\"]\n");
    assert_code(&machine.init(&["--name", "box"]), 0);
    assert!(machine.has("work.md"), "the declared tag still applies");
    assert!(!machine.has("development.md"));

    let list = machine.run(&["profile"]);
    assert!(
        text(&list).contains("work (from [machines.box] in loadout.toml)"),
        "{}",
        text(&list)
    );

    assert_code(&machine.run(&["profile", "set", "development"]), 0);
    assert_code(&machine.run(&["apply"]), 0);
    assert!(machine.has("work.md") && machine.has("development.md"));

    assert_code(&machine.run(&["profile", "set"]), 0);
    assert_code(&machine.run(&["apply"]), 0);
    assert!(
        machine.has("work.md"),
        "clearing the chosen profiles never drops a declared tag"
    );
}

#[test]
fn validate_checks_each_profile_for_a_machine_the_loadout_never_names() {
    let machine = Machine::new("");
    fs::write(
        machine.work.join("tags").join("work.toml"),
        "[files.work]\ntarget = \"~/work.md\"\n",
    )
    .unwrap();
    let validate = machine.run(&["validate", machine.work.to_str().unwrap()]);
    assert_code(&validate, 3);
    assert!(
        text(&validate).contains("a machine with only profile work"),
        "{}",
        text(&validate)
    );
    assert!(
        !text(&validate).contains("development"),
        "{}",
        text(&validate)
    );
}
