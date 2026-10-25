//! `loadout update` and `loadout service` through the real binary, with the real `git` against
//! bare remotes in scratch dirs, and a fake `git` where the exact argv and environment matter.
//! Every run uses `LOADOUT_HOME` and `LOADOUT_STATE_DIR` in a temp dir, so nothing on the host
//! is read or written, and no scheduler is ever asked to change anything.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use loadout::context::Context;
use loadout::platform::Os;
use loadout::state;
use loadout::sync;

/// The state lock, taken once no other process holds it. When this test just dropped an earlier
/// lock, a child that another test thread is starting can hold a copy of that lock's descriptor
/// until it execs, so this waits up to 5 seconds for `state::busy` to clear before taking the
/// lock. Still held then, or any error, fails the test.
fn relock(context: &Context) -> state::Lock {
    let until = Instant::now() + Duration::from_secs(5);
    while state::busy(context).unwrap() {
        assert!(
            Instant::now() < until,
            "the state lock is still held 5 seconds later"
        );
        thread::sleep(Duration::from_millis(20));
    }
    state::lock(context).unwrap()
}

const LOADOUT: &str = "[loadout]\nversion = 1\n\
                       [files.notes]\nsource = \"files/notes\"\ntarget = \"~/notes.md\"\n";

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
    /// A remote holding a loadout with one user file, and an empty home and state dir.
    fn new(loadout: &str) -> Self {
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
        fs::write(work.join("loadout.toml"), loadout).unwrap();
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

    fn context(&self) -> Context {
        let vars: Vec<(&str, OsString)> = vec![
            ("LOADOUT_HOME", self.home.clone().into_os_string()),
            ("LOADOUT_STATE_DIR", self.state.clone().into_os_string()),
        ];
        Context::resolve(
            Os::current().unwrap(),
            |name| {
                vars.iter()
                    .find(|(var, _)| *var == name)
                    .map(|(_, value)| value.clone())
            },
            None,
        )
        .unwrap()
    }

    /// Loadout's clone, made the way `init` makes it.
    fn clone(&self) -> PathBuf {
        let context = self.context();
        let lock = relock(&context);
        let path = self.remote.to_str().unwrap().replace('\\', "/");
        let url = if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        };
        sync::clone(&url, &context, &lock).unwrap();
        sync::checkout(&context)
    }

    fn push(&self, text: &str) {
        fs::write(self.work.join("files").join("notes"), text).unwrap();
        git(&self.work, &["commit", "--quiet", "-am", "notes"]);
        git(&self.work, &["push", "--quiet", "origin", "main"]);
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_loadout"));
        command
            .args(args)
            .env("LOADOUT_HOME", &self.home)
            .env("LOADOUT_STATE_DIR", &self.state);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn notes(&self) -> Option<String> {
        fs::read_to_string(self.home.join("notes.md")).ok()
    }

    fn record(&self, name: &str) -> serde_json::Value {
        let path = self.state.join("background").join(name);
        serde_json::from_slice(
            &fs::read(&path)
                .unwrap_or_else(|error| panic!("{} is missing: {error}", path.display())),
        )
        .unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn update_fetches_fast_forwards_and_applies_the_new_commit() {
    let machine = Machine::new(LOADOUT);
    machine.clone();
    let first = machine.run(&["update"]);
    assert_eq!(first.status.code(), Some(0), "{}", text(&first));
    assert!(
        text(&first).contains("fetched: up to date"),
        "{}",
        text(&first)
    );
    assert_eq!(machine.notes().as_deref(), Some("first\n"));

    machine.push("second\n");
    let second = machine.run(&["update"]);
    assert_eq!(second.status.code(), Some(0), "{}", text(&second));
    assert!(
        text(&second).contains("fetched: fast-forwarded"),
        "{}",
        text(&second)
    );
    assert_eq!(machine.notes().as_deref(), Some("second\n"));
}

/// Git variables in the caller's environment that would redirect git elsewhere (an index file,
/// another repository) reach no git process: the planted ones keep every byte, and loadout's own
/// clone advances with its own index.
#[test]
fn inherited_git_redirections_never_touch_another_index_or_repository() {
    let machine = Machine::new(LOADOUT);
    let clone = machine.clone();
    let root = machine.home.parent().unwrap().to_path_buf();
    let planted_index = root.join("planted-index");
    fs::write(&planted_index, b"PLANTED-INDEX-BYTES").unwrap();
    let planted_head = git(&machine.work, &["rev-parse", "HEAD"]);
    machine.push("second\n");
    let pushed = git(&machine.work, &["rev-parse", "HEAD"]);
    assert_ne!(planted_head, pushed);

    let output = machine
        .command(&["update"])
        .env("GIT_INDEX_FILE", &planted_index)
        .env("GIT_DIR", machine.work.join(".git"))
        .env("GIT_WORK_TREE", &machine.work)
        .env("GIT_OBJECT_DIRECTORY", root.join("planted-objects"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(fs::read(&planted_index).unwrap(), b"PLANTED-INDEX-BYTES");
    assert!(!root.join("planted-objects").exists());
    assert_eq!(git(&machine.work, &["rev-parse", "HEAD"]), pushed);
    assert_eq!(
        git(&clone, &["rev-parse", "HEAD"]),
        pushed,
        "the clone must advance"
    );
    assert_eq!(
        git(&clone, &["status", "--porcelain", "--untracked-files=all"]),
        "",
        "the clone's own index must match its new commit"
    );
    assert_eq!(machine.notes().as_deref(), Some("second\n"));
}

/// An unfinished run blocks `update` before it fetches, in the foreground and under the
/// supervisor: exit 4, the clone's branch and its view of origin don't move, and nothing is
/// applied. The run is seeded through the public `Run::begin` and left unfinished.
#[test]
fn an_interrupted_run_blocks_update_before_the_fetch() {
    let machine = Machine::new(LOADOUT);
    let clone = machine.clone();
    let context = machine.context();
    let lock = relock(&context);
    let run = state::Run::begin(
        &context,
        &clone,
        lock,
        &state::State::load(&context).unwrap(),
        &loadout::plan::Plan::default(),
    )
    .unwrap();
    drop(run);
    let head = git(&clone, &["rev-parse", "HEAD"]);
    let tracking = git(&clone, &["rev-parse", "refs/remotes/origin/main"]);
    machine.push("second\n");

    let foreground = machine.run(&["update"]);
    assert_eq!(foreground.status.code(), Some(4), "{}", text(&foreground));
    assert!(
        text(&foreground).contains("didn't finish"),
        "{}",
        text(&foreground)
    );

    let background = machine.run(&["update", "--background"]);
    assert_eq!(background.status.code(), Some(4), "{}", text(&background));
    let record = machine.record("last-run.json");
    assert_eq!(record["result"], "blocked", "{record}");
    assert_eq!(
        record["fetched"],
        serde_json::Value::Null,
        "no fetch: {record}"
    );

    assert_eq!(git(&clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git(&clone, &["rev-parse", "refs/remotes/origin/main"]),
        tracking,
        "nothing was fetched"
    );
    assert!(machine.notes().is_none(), "nothing may be applied");
}

#[test]
fn a_dirty_clone_blocks_update_and_nothing_is_applied() {
    let machine = Machine::new(LOADOUT);
    let clone = machine.clone();
    fs::write(clone.join("files").join("notes"), "edited in the clone\n").unwrap();
    machine.push("second\n");
    let output = machine.run(&["update"]);
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert!(
        text(&output).contains("blocked: the clone has local changes"),
        "{}",
        text(&output)
    );
    assert!(machine.notes().is_none(), "nothing may be applied");
    assert_eq!(
        fs::read_to_string(clone.join("files").join("notes")).unwrap(),
        "edited in the clone\n"
    );
}

#[test]
fn update_without_a_clone_says_how_to_make_one() {
    let machine = Machine::new(LOADOUT);
    let output = machine.run(&["update"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("loadout init"), "{}", text(&output));
}

#[test]
fn update_with_another_checkout_applies_it_without_fetching_it() {
    let machine = Machine::new(LOADOUT);
    let before = git(&machine.work, &["rev-parse", "HEAD"]);
    fs::write(machine.work.join("files").join("notes"), "uncommitted\n").unwrap();
    let output = machine.run(&["update", "--loadout", machine.work.to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(text(&output).contains("not fetched:"), "{}", text(&output));
    assert_eq!(machine.notes().as_deref(), Some("uncommitted\n"));
    assert_eq!(git(&machine.work, &["rev-parse", "HEAD"]), before);
    assert!(
        !machine.state.join("loadout").exists(),
        "no clone may be made"
    );
}

#[test]
fn background_refuses_another_checkout_at_parse_time() {
    let machine = Machine::new(LOADOUT);
    let output = machine.run(&["update", "--background", "--loadout", "x"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(!machine.state.exists());
}

#[test]
fn a_worker_started_by_hand_refuses_before_it_locks_or_writes() {
    let machine = Machine::new(LOADOUT);
    let output = machine
        .command(&["update", "--background", "--worker"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("refusing to run as a background worker"),
        "{}",
        text(&output)
    );
    assert!(!machine.state.exists(), "nothing may be written");
}

#[test]
fn a_background_run_is_supervised_applies_and_leaves_its_records() {
    let machine = Machine::new(LOADOUT);
    machine.clone();
    machine.push("second\n");
    let output = machine.run(&["update", "--background"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(machine.notes().as_deref(), Some("second\n"));

    let supervisor = machine.record("supervisor.json");
    assert_eq!(supervisor["outcome"], "exited", "{supervisor}");
    assert_eq!(supervisor["code"], 0, "{supervisor}");
    assert_eq!(
        supervisor["problem"],
        serde_json::Value::Null,
        "{supervisor}"
    );
    let run = machine.record("last-run.json");
    assert_eq!(run["result"], "applied", "{run}");
    assert_eq!(run["fetched"]["result"], "advanced", "{run}");
    assert_eq!(run["exit"], 0, "{run}");
    assert!(supervisor["run"].is_string(), "{supervisor}");
    assert_eq!(
        run["supervisor"], supervisor["run"],
        "the worker record is this run's"
    );
    let log = fs::read_to_string(machine.state.join("background").join("worker.log")).unwrap();
    assert!(log.contains("fetched: fast-forwarded"), "{log}");
    assert!(
        !log.contains("second"),
        "the log never holds file content: {log}"
    );

    let status = machine.run(&["status"]);
    assert_eq!(status.status.code(), Some(0), "{}", text(&status));
    assert!(text(&status).contains("background"), "{}", text(&status));
}

/// The scheduled job's route takes its `Context` from the service record alone: ambient
/// overrides that `Context` would reject (relative paths) don't stop it, and it applies to the
/// recorded home and state.
#[test]
fn a_recorded_service_run_ignores_an_unusable_ambient_environment() {
    let machine = Machine::new(LOADOUT);
    machine.clone();
    machine.push("second\n");
    let path = std::env::var_os("PATH").unwrap_or_default();
    let state = machine.state.clone().into_os_string();
    let var = |name: &str| match name {
        "LOADOUT_STATE_DIR" => Some(state.clone()),
        "PATH" => Some(path.clone()),
        _ => None,
    };
    let context =
        Context::resolve(Os::current().unwrap(), var, Some(machine.home.clone())).unwrap();
    assert!(!context.isolated);
    let exe = fs::canonicalize(env!("CARGO_BIN_EXE_loadout")).unwrap();
    let recorded = loadout::sync::service::Recorded::new(&context, &exe, 60, var).unwrap();
    let service = state::write_private(
        &context,
        state::Private::Service,
        toml::to_string(&recorded).unwrap().as_bytes(),
    )
    .unwrap();

    let output = machine
        .command(&[
            "update",
            "--background",
            "--service",
            service.to_str().unwrap(),
        ])
        .env("LOADOUT_HOME", "relative-home")
        .env("LOADOUT_STATE_DIR", "relative-state")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(machine.notes().as_deref(), Some("second\n"));
    let supervisor = machine.record("supervisor.json");
    let run = machine.record("last-run.json");
    assert_eq!(run["result"], "applied", "{run}");
    assert_eq!(run["supervisor"], supervisor["run"], "{run}");
}

/// A background run that finds another run holding the lock does nothing, exits 1, and is
/// recorded as locked, not as a failure.
#[test]
fn a_background_run_that_finds_the_lock_held_is_recorded_as_locked() {
    let machine = Machine::new(LOADOUT);
    machine.clone();
    machine.push("second\n");
    let held = relock(&machine.context());
    let output = machine.run(&["update", "--background"]);
    drop(held);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    let run = machine.record("last-run.json");
    assert_eq!(run["result"], "locked", "{run}");
    assert_eq!(run["exit"], 1, "{run}");
    assert!(machine.notes().is_none(), "nothing may be applied");
}

#[test]
fn a_blocked_background_run_is_recorded_and_makes_status_unhealthy() {
    let machine = Machine::new(LOADOUT);
    let clone = machine.clone();
    git(&clone, &["checkout", "--quiet", "--detach"]);
    let output = machine.run(&["update", "--background"]);
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    let run = machine.record("last-run.json");
    assert_eq!(run["result"], "blocked", "{run}");
    assert_eq!(run["fetched"]["reason"], "detached", "{run}");
    assert!(machine.notes().is_none());

    let status = machine.run(&["status"]);
    assert_eq!(status.status.code(), Some(4), "{}", text(&status));
    let service = machine.run(&["service", "status"]);
    assert_eq!(service.status.code(), Some(4), "{}", text(&service));
}

#[test]
fn a_loadout_that_turns_updates_off_stops_the_background_run_before_planning() {
    let machine = Machine::new(&format!("{LOADOUT}[update]\nauto = false\n"));
    machine.clone();
    let output = machine.run(&["update", "--background"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_eq!(machine.record("last-run.json")["result"], "disabled");
    assert!(machine.notes().is_none(), "nothing may be applied");
}

#[test]
fn services_are_never_installed_or_removed_under_isolation() {
    let machine = Machine::new(LOADOUT);
    machine.clone();
    for action in ["install", "uninstall"] {
        let output = machine.run(&["service", action]);
        assert_eq!(output.status.code(), Some(4), "{action}: {}", text(&output));
        assert!(
            text(&output).contains("isolation"),
            "{action}: {}",
            text(&output)
        );
    }
    assert!(!machine.state.join("service.toml").exists());
    assert!(!machine.home.join(".config").exists());
    assert!(!machine.home.join("Library").exists());

    let status = machine.run(&["service", "status", "--json"]);
    assert_eq!(status.status.code(), Some(0), "{}", text(&status));
    let report: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(report["installed"], false, "{report}");
    assert_eq!(
        report["service"]["scheduler"],
        serde_json::Value::Null,
        "{report}"
    );
}

#[test]
fn doctor_reports_the_clone_the_service_and_the_background_run() {
    let machine = Machine::new(LOADOUT);
    machine.clone();
    let output = machine.run(&["doctor", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let checks = report["checks"].as_array().unwrap();
    for name in ["checkout", "service", "background"] {
        let check = checks
            .iter()
            .find(|check| check["name"] == name)
            .unwrap_or_else(|| panic!("no {name} check: {report}"));
        assert_ne!(check["status"], "fail", "{check}");
    }
    let entries = fs::read_dir(&machine.state).unwrap().count();
    machine.run(&["doctor", "--json"]);
    assert_eq!(
        fs::read_dir(&machine.state).unwrap().count(),
        entries,
        "doctor never writes"
    );
}

/// A fake `git` that records its argv and the prompt variables, then says the clone has no
/// branch, so `update` stops at the first query.
#[cfg(unix)]
fn fake_git(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    fs::create_dir(&bin).unwrap();
    let log = dir.join("git.log");
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" \"prompt=$GIT_TERMINAL_PROMPT\" \"index=$GIT_INDEX_FILE dir=$GIT_DIR\" >> '{}'\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[cfg(unix)]
#[test]
fn every_git_call_names_its_repository_and_turns_hooks_and_prompts_off() {
    let machine = Machine::new(LOADOUT);
    machine.clone();
    let root = machine.home.parent().unwrap().to_path_buf();
    let bin = fake_git(&root);
    let output = machine
        .command(&["update"])
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("GIT_INDEX_FILE", root.join("planted-index"))
        .env("GIT_DIR", root.join("planted-repo"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert!(
        text(&output).contains("no branch checked out"),
        "{}",
        text(&output)
    );
    let log = fs::read_to_string(root.join("git.log")).unwrap();
    let first = log.lines().next().unwrap();
    assert!(first.contains("--git-dir="), "{log}");
    assert!(first.contains("--work-tree="), "{log}");
    assert!(first.contains("core.hooksPath="), "{log}");
    assert!(first.contains("loadout.no-hooks"), "{log}");
    assert!(first.contains("protocol.ext.allow=never"), "{log}");
    assert!(first.contains("core.fsmonitor=false"), "{log}");
    assert!(first.contains("symbolic-ref"), "{log}");
    assert!(log.contains("prompt=0"), "local reads never prompt: {log}");
    assert!(
        log.lines()
            .filter(|line| line.starts_with("index="))
            .all(|line| line == "index= dir="),
        "no inherited index or repository may reach git: {log}"
    );
}
