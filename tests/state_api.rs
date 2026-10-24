use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use loadout::context::Context;
use loadout::manifest::McpFormat;
use loadout::plan::{
    Action, Change, Content, FileContent, ItemKind, ItemRef, Observed, Plan, Target,
};
use loadout::platform::Os;
use loadout::state::{self, Error, Health, Removal, Run, RunStatus, State};
use serde_json::json;

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
    context: Context,
    loadout: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let state = root.join("state");
        let loadout = root.join("checkout");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&loadout).unwrap();
        let vars = BTreeMap::from([
            ("LOADOUT_HOME", home.clone().into_os_string()),
            ("LOADOUT_STATE_DIR", state.into_os_string()),
        ]);
        let context =
            Context::resolve(Os::current().unwrap(), |name| vars.get(name).cloned(), None).unwrap();
        Self {
            _dir: dir,
            home,
            context,
            loadout,
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.home.join(relative)
    }

    fn observe(&self, path: &Path) -> Observed {
        state::observe(&self.context, &self.loadout, &target(path)).unwrap()
    }

    fn state(&self) -> State {
        State::load(&self.context).unwrap()
    }

    fn begin(&self, actions: Vec<Action>) -> Run {
        let lock = state::lock(&self.context).unwrap();
        Run::begin(
            &self.context,
            &self.loadout,
            lock,
            &self.state(),
            &Plan { actions },
        )
        .unwrap()
    }

    /// Plans `content` at `path` the way U4's planner would, then writes it and finishes.
    fn apply(&self, path: &Path, content: &Content) -> String {
        let action = self.action(path, content.clone());
        let mut run = self.begin(vec![action.clone()]);
        let id = run.id().to_owned();
        run.write(&action, content).unwrap();
        let mut state = self.state();
        run.finish(&mut state).unwrap();
        id
    }

    fn action(&self, path: &Path, content: Content) -> Action {
        let prior = self.observe(path);
        let state = self.state();
        let record = state.record(&target(path));
        let change = match (&prior, record) {
            (Observed::Absent, _) => Change::Create,
            (Observed::Present { sha256 }, Some(record)) if *sha256 == record.sha256 => {
                Change::Update
            }
            (_, Some(_)) => Change::Drifted,
            (_, None) => Change::Replace,
        };
        Action {
            item: item(),
            target: target(path),
            change,
            prior,
            content: Some(content),
        }
    }

    fn removal(&self, path: &Path) -> Action {
        Action {
            item: item(),
            target: target(path),
            change: Change::Remove,
            prior: self.observe(path),
            content: None,
        }
    }

    fn rollback(&self, run: Option<&str>) -> Result<state::RolledBack, Error> {
        state::rollback(&self.context, run)
    }
}

fn item() -> ItemRef {
    ItemRef {
        kind: ItemKind::File,
        id: "gitconfig".to_owned(),
        agent: None,
    }
}

fn target(path: &Path) -> Target {
    Target::Path {
        path: path.to_path_buf(),
    }
}

fn file(bytes: &str, executable: bool) -> Content {
    Content::File(FileContent {
        bytes: bytes.as_bytes().to_vec(),
        executable,
    })
}

fn skill(script: &str) -> Content {
    Content::Tree(BTreeMap::from([
        (
            PathBuf::from("SKILL.md"),
            FileContent {
                bytes: b"# s\n".to_vec(),
                executable: false,
            },
        ),
        (
            PathBuf::from("bin/run.sh"),
            FileContent {
                bytes: script.as_bytes().to_vec(),
                executable: true,
            },
        ),
    ]))
}

fn sha(observed: &Observed) -> &str {
    match observed {
        Observed::Present { sha256 } => sha256,
        other => panic!("expected a present target, got {other:?}"),
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn present(sha256: &str) -> Observed {
    Observed::Present {
        sha256: sha256.to_owned(),
    }
}

#[test]
fn hashes_match_the_documented_definition_byte_for_byte() {
    let x = !cfg!(windows);
    let file_hash = state::digest(&file("hello\n", false)).unwrap();
    assert_eq!(
        file_hash,
        present("e513becd6e2b1124375093cf94c1e85a554b576fa590602010baa95c8bdc39d4")
    );
    assert_eq!(
        state::digest(&file("hello\n", true)).unwrap(),
        present(if x {
            "7463625688688926f4602422c59518d181dd30495cf69ca1639c35edc1256cf4"
        } else {
            "e513becd6e2b1124375093cf94c1e85a554b576fa590602010baa95c8bdc39d4"
        })
    );
    assert_eq!(
        state::digest(&skill("#!/bin/sh\n")).unwrap(),
        present(if x {
            "95a9208278a0b1416416107f1d96afeba22622ff49ae21af4f0a1efc65746beb"
        } else {
            "3a4fde2c4e93846ca0ef47e083d64d91d0eaaaa099796a0f52200d266f40ab74"
        })
    );
    assert_eq!(
        state::digest(&Content::Entry(json!({"command": "npx", "args": ["-y"]}))).unwrap(),
        present("8c7b36fa0a0f9c32d6fdbeaaa427212a4f1de491d7c9d89c339489f4507c156a")
    );
}

#[test]
fn observe_hashes_disk_with_the_same_definition_as_desired_content() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    assert_eq!(machine.observe(&path), Observed::Absent);
    fs::write(&path, "hello\n").unwrap();
    assert_eq!(
        machine.observe(&path),
        state::digest(&file("hello\n", false)).unwrap()
    );

    let dir = machine.path("skills/review");
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::write(dir.join("SKILL.md"), "# s\n").unwrap();
    fs::write(dir.join("bin/run.sh"), "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    set_mode(&dir.join("bin/run.sh"), 0o755);
    assert_eq!(
        machine.observe(&dir),
        state::digest(&skill("#!/bin/sh\n")).unwrap()
    );
}

#[test]
fn create_records_ownership_and_rollback_deletes_it_with_new_folders() {
    let machine = Machine::new();
    let path = machine.path(".config/tool/deep/config");
    let content = file("a = 1\n", false);
    let run = machine.apply(&path, &content);

    assert_eq!(fs::read_to_string(&path).unwrap(), "a = 1\n");
    let state = machine.state();
    let record = state.record(&target(&path)).unwrap();
    assert_eq!(present(&record.sha256), state::digest(&content).unwrap());
    assert_eq!(record.run, run);
    assert_eq!(record.item, item());

    let rolled = machine.rollback(None).unwrap();
    assert_eq!(rolled.run, run);
    assert_eq!(rolled.restored, vec![target(&path)]);
    assert!(!path.exists());
    assert!(
        !machine.path(".config").exists(),
        "folders the run made are removed"
    );
    assert!(machine.state().record(&target(&path)).is_none());
    assert!(matches!(
        machine.rollback(None),
        Err(Error::AlreadyRolledBack { .. })
    ));
}

#[test]
fn update_backs_up_and_rollback_restores_bytes_mode_and_record() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    machine.apply(&path, &file("one\n", false));
    #[cfg(unix)]
    set_mode(&path, 0o600);
    let before = machine.state().record(&target(&path)).cloned().unwrap();
    let first = machine.observe(&path);

    let second = machine.apply(&path, &file("two\n", false));
    assert_eq!(fs::read_to_string(&path).unwrap(), "two\n");
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600, "an update keeps the user's mode");

    machine.rollback(Some(&second)).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\n");
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600);
    assert_eq!(machine.observe(&path), first);
    assert_eq!(machine.state().record(&target(&path)), Some(&before));
}

#[test]
fn replace_of_an_unowned_file_is_undone_by_rollback() {
    let machine = Machine::new();
    let path = machine.path(".bashrc");
    fs::write(&path, "mine\n").unwrap();
    let run = machine.apply(&path, &file("loadout's\n", false));
    assert_eq!(fs::read_to_string(&path).unwrap(), "loadout's\n");
    let backups = machine
        .context
        .state
        .join("runs")
        .join(&run)
        .join("backups");
    assert_eq!(fs::read_to_string(backups.join("0")).unwrap(), "mine\n");

    machine.rollback(None).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");
    assert!(machine.state().record(&target(&path)).is_none());
}

#[test]
fn a_target_changed_after_planning_is_never_written() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    let content = file("planned\n", false);
    let action = machine.action(&path, content.clone());
    assert_eq!(action.change, Change::Create);
    let mut run = machine.begin(vec![action.clone()]);

    fs::write(&path, "user wrote this meanwhile\n").unwrap();
    let error = run.write(&action, &content).unwrap_err();
    assert!(matches!(error, Error::Changed { .. }), "{error}");
    assert!(error.is_blocked());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "user wrote this meanwhile\n"
    );
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    assert!(state.record(&target(&path)).is_none());
    assert!(machine.state().record(&target(&path)).is_none());
}

#[test]
fn adopt_records_an_equal_target_without_writing_it() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    fs::write(&path, "same\n").unwrap();
    let content = file("same\n", false);
    let mut action = machine.action(&path, content.clone());
    action.change = Change::Adopt;
    let mut run = machine.begin(vec![action.clone()]);
    run.write(&action, &content).unwrap();
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    assert_eq!(
        present(&state.record(&target(&path)).unwrap().sha256),
        state::digest(&content).unwrap()
    );
    machine.rollback(None).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "same\n");
    assert!(machine.state().record(&target(&path)).is_none());
}

#[test]
fn removal_deletes_an_unchanged_owned_target_and_rollback_brings_it_back() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    machine.apply(&path, &file("owned\n", false));

    let action = machine.removal(&path);
    let mut run = machine.begin(vec![action.clone()]);
    assert_eq!(run.remove(&action).unwrap(), Removal::Removed);
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    assert!(!path.exists());
    assert!(state.record(&target(&path)).is_none());

    machine.rollback(None).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "owned\n");
    assert!(machine.state().record(&target(&path)).is_some());
}

#[test]
fn removal_releases_a_drifted_target_and_leaves_it_in_place() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    machine.apply(&path, &file("owned\n", false));
    fs::write(&path, "edited by the user\n").unwrap();

    let action = machine.removal(&path);
    let mut run = machine.begin(vec![action.clone()]);
    assert_eq!(run.remove(&action).unwrap(), Removal::Released);
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "edited by the user\n");
    assert!(machine.state().record(&target(&path)).is_none());

    machine.rollback(None).unwrap();
    assert!(
        machine.state().record(&target(&path)).is_some(),
        "rollback takes ownership back"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "edited by the user\n");
}

#[test]
fn trees_are_written_whole_and_rolled_back_whole() {
    let machine = Machine::new();
    let dir = machine.path(".agents/skills/review");
    machine.apply(&dir, &skill("#!/bin/sh\necho one\n"));
    assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), "# s\n");
    #[cfg(unix)]
    {
        assert_ne!(
            mode(&dir.join("bin/run.sh")) & 0o100,
            0,
            "the script is executable"
        );
        assert_eq!(mode(&dir.join("SKILL.md")) & 0o111, 0);
    }
    fs::write(dir.join("notes.txt"), "stray").unwrap();
    let state = machine.state();
    let drifted = state::audit(&machine.context, &machine.loadout, &state);
    assert_eq!(drifted[0].1, Health::Drifted);
    fs::remove_file(dir.join("notes.txt")).unwrap();

    let second = machine.apply(&dir, &skill("#!/bin/sh\necho two\n"));
    assert_eq!(
        fs::read_to_string(dir.join("bin/run.sh")).unwrap(),
        "#!/bin/sh\necho two\n"
    );
    machine.rollback(Some(&second)).unwrap();
    assert_eq!(
        fs::read_to_string(dir.join("bin/run.sh")).unwrap(),
        "#!/bin/sh\necho one\n"
    );
    assert_eq!(
        machine.observe(&dir),
        state::digest(&skill("#!/bin/sh\necho one\n")).unwrap()
    );
    let leftovers: Vec<_> = fs::read_dir(dir.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec![OsString::from("review")]);
}

#[cfg(unix)]
#[test]
fn modes_keep_read_write_bits_and_add_x_only_where_readable() {
    let machine = Machine::new();
    let cases = [
        (0o644, true, 0o755),
        (0o600, true, 0o700),
        (0o755, false, 0o644),
        (0o640, true, 0o750),
    ];
    for (index, (start, executable, expected)) in cases.into_iter().enumerate() {
        let path = machine.path(&format!("bin/tool{index}"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "old\n").unwrap();
        set_mode(&path, start);
        machine.apply(&path, &file("new\n", executable));
        assert_eq!(mode(&path), expected, "{start:o} executable={executable}");
    }
}

#[cfg(unix)]
#[test]
fn a_mode_only_change_is_a_different_hash() {
    let machine = Machine::new();
    let path = machine.path("bin/tool");
    machine.apply(&path, &file("#!/bin/sh\n", true));
    let executable = machine.observe(&path);
    set_mode(&path, 0o644);
    assert_ne!(machine.observe(&path), executable);
    let state = machine.state();
    assert_eq!(
        state::audit(&machine.context, &machine.loadout, &state)[0].1,
        Health::Drifted
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_target_keeps_its_link_and_the_real_file_is_written() {
    let machine = Machine::new();
    let real = machine.path("dotfiles/gitconfig");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, "old\n").unwrap();
    set_mode(&real, 0o600);
    let link = machine.path(".gitconfig");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    machine.apply(&link, &file("new\n", false));
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&real).unwrap(), "new\n");
    assert_eq!(mode(&real), 0o600);

    machine.rollback(None).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&real).unwrap(), "old\n");
}

#[cfg(unix)]
#[test]
fn a_dangling_symlink_target_is_created_at_its_real_destination() {
    let machine = Machine::new();
    let real = machine.path("dotfiles/zshrc");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    let link = machine.path(".zshrc");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    machine.apply(&link, &file("new\n", false));
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(&real).unwrap(), "new\n");
}

#[cfg(unix)]
#[test]
fn a_fifo_target_is_refused_without_blocking() {
    let machine = Machine::new();
    let path = machine.path(".fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    let error = state::observe(&machine.context, &machine.loadout, &target(&path)).unwrap_err();
    assert!(matches!(error, Error::Special { .. }), "{error}");
}

#[test]
fn reserved_targets_are_never_observed_or_written() {
    let machine = Machine::new();
    let auth = machine.path(".codex/auth.json");
    fs::create_dir_all(auth.parent().unwrap()).unwrap();
    fs::write(&auth, "token").unwrap();
    let inside_checkout = machine.loadout.join("loadout.toml");
    let state_file = machine.context.state.join("state.json");
    for path in [&auth, &inside_checkout, &state_file] {
        let error = state::observe(&machine.context, &machine.loadout, &target(path)).unwrap_err();
        assert!(
            matches!(error, Error::Reserved { .. }),
            "{}: {error}",
            path.display()
        );
    }

    let content = file("x", false);
    let action = Action {
        item: item(),
        target: target(&auth),
        change: Change::Replace,
        prior: present(&"0".repeat(64)),
        content: Some(content.clone()),
    };
    let mut run = machine.begin(vec![action.clone()]);
    let error = run.write(&action, &content).unwrap_err();
    assert!(matches!(error, Error::Reserved { .. }), "{error}");
    assert_eq!(fs::read_to_string(&auth).unwrap(), "token");
    let backups = machine
        .context
        .state
        .join("runs")
        .join(run.id())
        .join("backups");
    assert_eq!(
        fs::read_dir(backups).unwrap().count(),
        0,
        "no backup was taken"
    );
}

#[test]
fn mcp_entries_are_refused_until_the_mcp_module_lands() {
    let machine = Machine::new();
    let entry = Target::Entry {
        file: machine.path(".cursor/mcp.json"),
        format: McpFormat::Json,
        pointer: "/mcpServers".to_owned(),
        key: "github".to_owned(),
    };
    let error = state::observe(&machine.context, &machine.loadout, &entry).unwrap_err();
    assert!(matches!(error, Error::EntryNeedsMcp { .. }), "{error}");

    let content = Content::Entry(json!({"url": "https://example.com/mcp"}));
    let action = Action {
        item: ItemRef {
            kind: ItemKind::Mcp,
            id: "github".to_owned(),
            agent: Some("cursor".to_owned()),
        },
        target: entry,
        change: Change::Create,
        prior: Observed::Absent,
        content: Some(content.clone()),
    };
    let mut run = machine.begin(vec![action.clone()]);
    assert!(matches!(
        run.write(&action, &content),
        Err(Error::EntryNeedsMcp { .. })
    ));
    assert!(!machine.path(".cursor").exists());
}

#[test]
fn the_lock_keeps_a_second_run_out() {
    let machine = Machine::new();
    let held = state::lock(&machine.context).unwrap();
    assert!(state::busy(&machine.context).unwrap());
    let error = state::lock(&machine.context).unwrap_err();
    assert!(matches!(error, Error::Locked { .. }), "{error}");
    assert!(error.to_string().contains("another loadout run"), "{error}");
    assert!(matches!(machine.rollback(None), Err(Error::Locked { .. })));
    drop(held);
    assert!(!state::busy(&machine.context).unwrap());
    drop(state::lock(&machine.context).unwrap());
}

#[test]
fn an_interrupted_run_is_reported_and_rolled_back() {
    let machine = Machine::new();
    let done = machine.path(".one");
    let pending = machine.path(".two");
    fs::write(&done, "user\n").unwrap();
    let first = file("first\n", false);
    let second = file("second\n", false);
    let actions = vec![
        machine.action(&done, first.clone()),
        machine.action(&pending, second),
    ];
    let mut run = machine.begin(actions.clone());
    run.write(&actions[0], &first).unwrap();
    let id = run.id().to_owned();
    drop(run);

    let runs = state::runs(&machine.context).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, id);
    assert_eq!(runs[0].status, Some(RunStatus::Interrupted));
    assert_eq!((runs[0].done, runs[0].actions), (1, 2));
    assert!(machine.state().record(&target(&done)).is_none());

    let rolled = machine.rollback(None).unwrap();
    assert_eq!(rolled.restored, vec![target(&done)]);
    assert_eq!(fs::read_to_string(&done).unwrap(), "user\n");
    assert!(!pending.exists());
    assert_eq!(
        state::runs(&machine.context).unwrap()[0].status,
        Some(RunStatus::RolledBack)
    );
}

#[test]
fn a_crash_between_backup_and_write_rolls_back_from_either_side() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    fs::write(&path, "before\n").unwrap();
    let content = file("after\n", false);
    let action = machine.action(&path, content.clone());
    let mut run = machine.begin(vec![action.clone()]);
    run.write(&action, &content).unwrap();
    let journal = machine
        .context
        .state
        .join("runs")
        .join(run.id())
        .join("journal.json");
    drop(run);
    let text = fs::read_to_string(&journal).unwrap();
    fs::write(&journal, text.replace("\"done\"", "\"started\"")).unwrap();

    machine.rollback(None).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");
}

#[test]
fn rollback_refuses_to_overwrite_later_changes() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    let first = machine.apply(&path, &file("one\n", false));
    let other = machine.path(".other");
    machine.apply(&other, &file("x\n", false));
    fs::write(&path, "edited later\n").unwrap();

    let error = machine.rollback(Some(&first)).unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert!(error.is_blocked());
    assert!(error.to_string().contains(".gitconfig"), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "edited later\n");
    assert!(other.exists());
    assert!(machine.state().record(&target(&path)).is_some());
}

#[test]
fn rollback_names_unknown_runs_and_empty_history() {
    let machine = Machine::new();
    assert!(matches!(machine.rollback(None), Err(Error::NoRuns)));
    machine.apply(&machine.path(".x"), &file("x", false));
    for bad in ["nope", "../../etc", "20261024T000000.000000000Z"] {
        assert!(
            matches!(machine.rollback(Some(bad)), Err(Error::UnknownRun { .. })),
            "{bad}"
        );
    }
}

#[test]
fn a_tampered_backup_is_never_restored() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    fs::write(&path, "original\n").unwrap();
    let run = machine.apply(&path, &file("new\n", false));
    let backup = machine
        .context
        .state
        .join("runs")
        .join(&run)
        .join("backups/0");
    fs::write(&backup, "tampered\n").unwrap();
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::BackupCorrupt { .. }), "{error}");
    assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
}

#[test]
fn begin_refuses_a_plan_that_names_a_target_twice() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    let action = machine.action(&path, file("x", false));
    let lock = state::lock(&machine.context).unwrap();
    let error = Run::begin(
        &machine.context,
        &machine.loadout,
        lock,
        &machine.state(),
        &Plan {
            actions: vec![action.clone(), action],
        },
    )
    .unwrap_err();
    assert!(matches!(error, Error::Duplicate { .. }), "{error}");
}

#[test]
fn the_journal_lists_every_writing_action_before_the_first_write() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    fs::write(&path, "before\n").unwrap();
    let content = file("after\n", false);
    let mut skipped = machine.action(&machine.path(".same"), file("s", false));
    skipped.change = Change::Unchanged;
    let action = machine.action(&path, content);
    let run = machine.begin(vec![skipped, action.clone()]);
    let journal: serde_json::Value = serde_json::from_slice(
        &fs::read(
            machine
                .context
                .state
                .join("runs")
                .join(run.id())
                .join("journal.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let entries = journal["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{journal:#}");
    assert_eq!(journal["status"], "running");
    assert_eq!(entries[0]["step"], "pending");
    assert_eq!(entries[0]["backup"]["path"], "backups/0");
    assert_eq!(entries[0]["prior"]["sha256"], sha(&action.prior));
    assert_eq!(fs::read_to_string(&path).unwrap(), "before\n");
}
