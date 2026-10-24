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
        Self::at(tempfile::tempdir().unwrap())
    }

    /// Unix socket paths are limited to about 100 bytes (`SUN_LEN`), and a macOS temp dir alone
    /// takes most of that, so socket fixtures get a short root in `/tmp` whatever `TMPDIR` is.
    #[cfg(unix)]
    fn short() -> Self {
        Self::at(
            tempfile::Builder::new()
                .prefix("lo")
                .tempdir_in("/tmp")
                .unwrap(),
        )
    }

    fn at(dir: tempfile::TempDir) -> Self {
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let loadout = home.join("checkout");
        fs::create_dir_all(&loadout).unwrap();
        let context = isolated(&home, &home.join(".local/state/loadout"));
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

    fn run_dir(&self, run: &str) -> PathBuf {
        self.context.state.join("runs").join(run)
    }

    fn edit_journal(&self, run: &str, edit: impl FnOnce(&mut serde_json::Value)) {
        let path = self.run_dir(run).join("journal.json");
        let mut journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut journal);
        fs::write(&path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
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

fn isolated(home: &Path, state: &Path) -> Context {
    let vars = BTreeMap::from([
        ("LOADOUT_HOME", home.as_os_str().to_owned()),
        ("LOADOUT_STATE_DIR", state.as_os_str().to_owned()),
    ]);
    Context::resolve(Os::current().unwrap(), |name| vars.get(name).cloned(), None).unwrap()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

/// Writes the skill that `skill(script)` describes directly to disk, with the same modes.
fn write_skill(dir: &Path, script: &str) {
    fs::create_dir_all(dir.join("bin")).unwrap();
    fs::write(dir.join("SKILL.md"), "# s\n").unwrap();
    fs::write(dir.join("bin/run.sh"), script).unwrap();
    #[cfg(unix)]
    {
        set_mode(&dir.join("SKILL.md"), 0o644);
        set_mode(&dir.join("bin/run.sh"), 0o755);
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
fn a_special_file_target_or_child_is_refused_without_opening_it() {
    // A socket stands in for a FIFO here because creating one needs no child process, and this
    // test binary must not fork while other tests hold state locks. The guest E2E covers a FIFO.
    let machine = Machine::short();
    let path = machine.path(".socket");
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let error = state::observe(&machine.context, &machine.loadout, &target(&path)).unwrap_err();
    assert!(matches!(error, Error::Special { .. }), "{error}");

    let dir = machine.path(".agents/skills/review");
    write_skill(&dir, "#!/bin/sh\n");
    let _inner = std::os::unix::net::UnixListener::bind(dir.join("bin/agent.sock")).unwrap();
    let error = state::observe(&machine.context, &machine.loadout, &target(&dir)).unwrap_err();
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

fn two_target_run(machine: &Machine) -> (PathBuf, PathBuf, String) {
    let a = machine.path(".a");
    let b = machine.path(".b");
    fs::write(&a, "a-before").unwrap();
    fs::write(&b, "b-before").unwrap();
    let contents = [file("a-after", false), file("b-after", false)];
    let actions = vec![
        machine.action(&a, contents[0].clone()),
        machine.action(&b, contents[1].clone()),
    ];
    let mut run = machine.begin(actions.clone());
    for (action, content) in actions.iter().zip(&contents) {
        run.write(action, content).unwrap();
    }
    let id = run.id().to_owned();
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    (a, b, id)
}

#[test]
fn rollback_checks_every_backup_before_restoring_anything() {
    let machine = Machine::new();
    let (a, b, id) = two_target_run(&machine);
    fs::write(machine.run_dir(&id).join("backups/0"), "tampered").unwrap();
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::BackupCorrupt { .. }), "{error}");
    assert_eq!(read(&a), "a-after");
    assert_eq!(
        read(&b),
        "b-after",
        "B comes first in rollback order and stays untouched"
    );
}

#[test]
fn rollback_reads_the_state_file_before_restoring_anything() {
    let machine = Machine::new();
    let (a, b, _) = two_target_run(&machine);
    fs::write(machine.context.state.join("state.json"), "not json").unwrap();
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::Invalid { .. }), "{error}");
    assert_eq!(
        (read(&a), read(&b)),
        ("a-after".to_owned(), "b-after".to_owned())
    );
}

#[test]
fn a_rollback_that_stopped_halfway_finishes_on_the_next_try() {
    let machine = Machine::new();
    let (a, b, _) = two_target_run(&machine);
    fs::write(&b, "b-before").unwrap();
    let rolled = machine.rollback(None).unwrap();
    assert_eq!(rolled.restored, vec![target(&a)]);
    assert_eq!(
        (read(&a), read(&b)),
        ("a-before".to_owned(), "b-before".to_owned())
    );
}

/// Recreates the moment between the two renames of a folder swap: the run moved the old tree
/// to its `.old` scratch name and stopped before moving the new one in.
fn stopped_swap(machine: &Machine, step: &str) -> (PathBuf, PathBuf, String) {
    let dir = machine.path(".agents/skills/review");
    machine.apply(&dir, &skill("#!/bin/sh\necho one\n"));
    let run = machine.apply(&dir, &skill("#!/bin/sh\necho two\n"));
    let old = dir.with_file_name(format!(".review.loadout-{run}-0.old"));
    fs::remove_dir_all(&dir).unwrap();
    write_skill(&old, "#!/bin/sh\necho one\n");
    machine.edit_journal(&run, |journal| {
        journal["status"] = json!("running");
        journal["entries"][0]["step"] = json!(step);
    });
    (dir, old, run)
}

#[test]
fn a_folder_swap_that_stopped_between_renames_is_rolled_back() {
    for step in ["started", "failed"] {
        let machine = Machine::new();
        let (dir, old, _) = stopped_swap(&machine, step);
        let rolled = machine.rollback(None).unwrap();
        assert_eq!(rolled.restored, vec![target(&dir)], "{step}");
        assert_eq!(
            read(&dir.join("bin/run.sh")),
            "#!/bin/sh\necho one\n",
            "{step}"
        );
        assert!(!old.exists(), "the verified scratch is cleaned up ({step})");
        assert!(rolled.leftovers.is_empty());
    }
}

#[test]
fn a_forged_scratch_folder_never_counts_as_the_run_s_own() {
    let machine = Machine::new();
    let (dir, old, _) = stopped_swap(&machine, "started");
    fs::write(old.join("bin/run.sh"), "forged").unwrap();
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert!(!dir.exists());
    assert_eq!(
        read(&old.join("bin/run.sh")),
        "forged",
        "never deleted on a guess"
    );
}

#[cfg(unix)]
#[test]
fn a_linked_scratch_name_never_counts_as_the_run_s_own() {
    let machine = Machine::new();
    let (dir, old, _) = stopped_swap(&machine, "started");
    let elsewhere = machine.path("elsewhere");
    fs::rename(&old, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &old).unwrap();
    assert!(matches!(
        machine.rollback(None),
        Err(Error::RollbackBlocked { .. })
    ));
    assert!(!dir.exists());
    assert!(elsewhere.join("SKILL.md").exists());
}

#[test]
fn unverifiable_scratch_is_reported_and_left_in_place() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    let run = machine.apply(&path, &file("x", false));
    let stray = path.with_file_name(format!("..gitconfig.loadout-{run}-0.tmp"));
    fs::write(&stray, "not the run's content").unwrap();
    let rolled = machine.rollback(None).unwrap();
    assert!(!path.exists());
    assert_eq!(rolled.leftovers, vec![stray.clone()]);
    assert_eq!(read(&stray), "not the run's content");
}

fn tree_run(machine: &Machine) -> (PathBuf, String) {
    let dir = machine.path(".agents/skills/review");
    write_skill(&dir, "#!/bin/sh\necho mine\n");
    let run = machine.apply(&dir, &skill("#!/bin/sh\necho loadout\n"));
    (dir, run)
}

#[test]
fn a_journal_folder_key_that_climbs_out_is_refused_before_any_write() {
    let machine = Machine::new();
    let (dir, run) = tree_run(&machine);
    machine.edit_journal(&run, |journal| {
        journal["entries"][0]["backup"]["dirs"]["../../escape"] = json!(0o777);
    });
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::Invalid { .. }), "{error}");
    assert!(!machine.path(".agents/escape").exists());
    assert!(!machine.path("escape").exists());
    assert_eq!(read(&dir.join("bin/run.sh")), "#!/bin/sh\necho loadout\n");
}

#[test]
fn journal_created_folders_must_lead_to_their_target() {
    let machine = Machine::new();
    let keep = machine.path("keep-me");
    fs::create_dir(&keep).unwrap();
    let path = machine.path(".config/tool/config");
    let run = machine.apply(&path, &file("x", false));
    machine.edit_journal(&run, |journal| {
        journal["entries"][0]["created"] = json!([keep]);
    });
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::Invalid { .. }), "{error}");
    assert!(keep.is_dir(), "an unrelated empty folder survives");
    assert!(path.exists());
}

#[test]
fn a_backup_must_come_from_its_own_slot_in_its_own_run() {
    let machine = Machine::new();
    let (dir, run) = tree_run(&machine);
    let other = machine.apply(&machine.path(".other"), &file("o", false));
    for path in [
        "backups/1".to_owned(),
        format!("../{other}/backups/0"),
        "/etc/passwd".to_owned(),
    ] {
        machine.edit_journal(&run, |journal| {
            journal["entries"][0]["backup"]["path"] = json!(path);
        });
        let error = machine.rollback(Some(&run)).unwrap_err();
        assert!(matches!(error, Error::Invalid { .. }), "{path}: {error}");
        assert_eq!(read(&dir.join("bin/run.sh")), "#!/bin/sh\necho loadout\n");
    }
}

#[test]
fn journal_targets_outside_the_target_roots_are_refused() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    let run = machine.apply(&path, &file("x", false));
    let outside = machine.home.parent().unwrap().join("outside");
    fs::write(&outside, "x").unwrap();
    machine.edit_journal(&run, |journal| {
        journal["entries"][0]["target"]["path"] = json!(outside);
    });
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::OutsideRoots { .. }), "{error}");
    assert_eq!(
        read(&outside),
        "x",
        "a file matching the run's result is still not deleted"
    );
}

#[test]
fn a_relative_checkout_or_target_in_the_journal_is_invalid() {
    let machine = Machine::new();
    let run = machine.apply(&machine.path(".gitconfig"), &file("x", false));
    machine.edit_journal(&run, |journal| {
        journal["loadout"] = json!("relative/checkout");
    });
    assert!(matches!(machine.rollback(None), Err(Error::Invalid { .. })));
    machine.edit_journal(&run, |journal| {
        journal["loadout"] = json!(machine.loadout);
        journal["entries"][0]["target"]["path"] = json!("relative/.gitconfig");
    });
    assert!(matches!(machine.rollback(None), Err(Error::Invalid { .. })));
}

#[test]
fn a_state_file_target_must_be_absolute_and_clean() {
    let machine = Machine::new();
    machine.apply(&machine.path(".gitconfig"), &file("x", false));
    let path = machine.context.state.join("state.json");
    let mut file: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    file["records"][0]["target"]["path"] = json!("../escape");
    fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    assert!(matches!(
        State::load(&machine.context),
        Err(Error::Invalid { .. })
    ));
}

#[test]
fn agent_dir_overrides_outside_home_are_supported_targets() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let codex = root.join("opt/codex");
    fs::create_dir_all(&home).unwrap();
    let vars = BTreeMap::from([
        ("CODEX_HOME", codex.as_os_str().to_owned()),
        ("LOADOUT_STATE_DIR", home.join(".state").into_os_string()),
    ]);
    let context = Context::resolve(
        Os::current().unwrap(),
        |name| vars.get(name).cloned(),
        Some(home.clone()),
    )
    .unwrap();
    let checkout = home.join(".state/loadout");
    let inside = target(&codex.join("AGENTS.md"));
    let body = file("instructions", false);
    let action = Action {
        item: item(),
        target: inside.clone(),
        change: Change::Create,
        prior: state::observe(&context, &checkout, &inside).unwrap(),
        content: Some(body.clone()),
    };
    let lock = state::lock(&context).unwrap();
    let mut state = State::load(&context).unwrap();
    let plan = Plan {
        actions: vec![action.clone()],
    };
    let mut run = Run::begin(&context, &checkout, lock, &state, &plan).unwrap();
    run.write(&action, &body).unwrap();
    run.finish(&mut state).unwrap();
    assert_eq!(read(&codex.join("AGENTS.md")), "instructions");
    state::rollback(&context, None).unwrap();
    assert!(!codex.join("AGENTS.md").exists());

    let outside = target(&root.join("opt/other/AGENTS.md"));
    assert!(matches!(
        state::observe(&context, &checkout, &outside),
        Err(Error::OutsideRoots { .. })
    ));
}

#[test]
fn a_lock_only_admits_runs_on_its_own_state_dir() {
    let a = Machine::new();
    let b = Machine::new();
    let held = state::lock(&b.context).unwrap();
    let wrong = state::lock(&a.context).unwrap();
    let error =
        Run::begin(&b.context, &b.loadout, wrong, &b.state(), &Plan::default()).unwrap_err();
    assert!(matches!(error, Error::WrongLock { .. }), "{error}");
    assert!(!b.context.state.join("runs").exists());
    drop(held);
}

#[cfg(unix)]
#[test]
fn a_lock_taken_through_an_alias_of_the_same_state_dir_is_accepted() {
    let machine = Machine::new();
    fs::create_dir_all(&machine.context.state).unwrap();
    let alias = machine.path("state-alias");
    std::os::unix::fs::symlink(&machine.context.state, &alias).unwrap();
    let lock = state::lock(&isolated(&machine.home, &alias)).unwrap();
    let run = Run::begin(
        &machine.context,
        &machine.loadout,
        lock,
        &machine.state(),
        &Plan::default(),
    );
    assert!(run.is_ok(), "{:?}", run.err());
}

#[cfg(unix)]
#[test]
fn a_planted_runs_link_never_receives_a_journal() {
    let machine = Machine::new();
    fs::create_dir_all(&machine.context.state).unwrap();
    let outside = machine.home.parent().unwrap().join("outside-runs");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, machine.context.state.join("runs")).unwrap();
    let lock = state::lock(&machine.context).unwrap();
    let error = Run::begin(
        &machine.context,
        &machine.loadout,
        lock,
        &machine.state(),
        &Plan::default(),
    )
    .unwrap_err();
    assert!(matches!(error, Error::Invalid { .. }), "{error}");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn an_xdg_config_home_outside_home_is_a_supported_target_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    let xdg = root.join("xdg");
    fs::create_dir_all(&home).unwrap();
    let vars = BTreeMap::from([
        ("XDG_CONFIG_HOME", xdg.as_os_str().to_owned()),
        ("LOADOUT_STATE_DIR", home.join(".state").into_os_string()),
    ]);
    let context = Context::resolve(
        Os::current().unwrap(),
        |name| vars.get(name).cloned(),
        Some(home.clone()),
    )
    .unwrap();
    assert_eq!(context.xdg_config, xdg);
    let checkout = home.join(".state/loadout");
    let inside = target(&xdg.join("opencode/AGENTS.md"));
    let body = file("rules", false);
    let action = Action {
        item: item(),
        target: inside.clone(),
        change: Change::Create,
        prior: state::observe(&context, &checkout, &inside).unwrap(),
        content: Some(body.clone()),
    };
    let lock = state::lock(&context).unwrap();
    let mut state = State::load(&context).unwrap();
    let plan = Plan {
        actions: vec![action.clone()],
    };
    let mut run = Run::begin(&context, &checkout, lock, &state, &plan).unwrap();
    run.write(&action, &body).unwrap();
    run.finish(&mut state).unwrap();
    assert_eq!(read(&xdg.join("opencode/AGENTS.md")), "rules");
    state::rollback(&context, None).unwrap();
    assert!(!xdg.join("opencode").exists());
}

#[test]
fn a_backup_mode_changed_in_the_journal_alone_is_refused() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    fs::write(&path, "mine").unwrap();
    let run = machine.apply(&path, &file("loadout's", false));
    machine.edit_journal(&run, |journal| {
        journal["entries"][0]["backup"]["modes"][""] = json!(0o666);
    });
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::Invalid { .. }), "{error}");
    assert_eq!(read(&path), "loadout's");
}

#[test]
fn a_foreign_file_on_a_rollback_staging_name_blocks_before_any_restore() {
    let machine = Machine::new();
    let (a, b, id) = two_target_run(&machine);
    let marker = a.with_file_name(format!("..a.loadout-{id}-0-rollback.tmp"));
    fs::write(&marker, "someone else's").unwrap();
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert!(error.to_string().contains("in the way"), "{error}");
    assert_eq!(read(&a), "a-after");
    assert_eq!(
        read(&b),
        "b-after",
        "B comes first in rollback order and stays untouched"
    );
    assert_eq!(read(&marker), "someone else's");
}

#[test]
fn a_foreign_rollback_staging_name_beside_a_target_needing_no_restore_is_only_reported() {
    let machine = Machine::new();
    let (a, b, id) = two_target_run(&machine);
    fs::write(&a, "a-before").unwrap();
    let marker = a.with_file_name(format!("..a.loadout-{id}-0-rollback.tmp"));
    fs::write(&marker, "someone else's").unwrap();
    let rolled = machine.rollback(None).unwrap();
    assert_eq!(rolled.restored, vec![target(&b)]);
    assert_eq!(rolled.leftovers, vec![marker.clone()]);
    assert_eq!(read(&b), "b-before");
    assert_eq!(read(&marker), "someone else's");
}

#[test]
fn a_leftover_old_beside_a_done_action_never_overrides_a_later_deletion() {
    let machine = Machine::new();
    let dir = machine.path(".agents/skills/review");
    machine.apply(&dir, &skill("#!/bin/sh\necho one\n"));
    let run = machine.apply(&dir, &skill("#!/bin/sh\necho two\n"));
    let old = dir.with_file_name(format!(".review.loadout-{run}-0.old"));
    write_skill(&old, "#!/bin/sh\necho one\n");
    fs::remove_dir_all(&dir).unwrap();
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert!(!dir.exists(), "the later deletion stands");
    assert_eq!(read(&old.join("bin/run.sh")), "#!/bin/sh\necho one\n");
}

#[test]
fn a_rollback_that_stopped_mid_swap_is_recovered_from_its_own_old() {
    let machine = Machine::new();
    let dir = machine.path(".agents/skills/review");
    machine.apply(&dir, &skill("#!/bin/sh\necho one\n"));
    let run = machine.apply(&dir, &skill("#!/bin/sh\necho two\n"));
    let reold = dir.with_file_name(format!(".review.loadout-{run}-0-rollback.old"));
    fs::rename(&dir, &reold).unwrap();
    let rolled = machine.rollback(None).unwrap();
    assert_eq!(rolled.restored, vec![target(&dir)]);
    assert_eq!(read(&dir.join("bin/run.sh")), "#!/bin/sh\necho one\n");
    assert!(
        !reold.exists(),
        "the verified rollback scratch is cleaned up"
    );
}

#[test]
fn an_occupied_rollback_old_blocks_a_restore_that_must_move_the_target_aside() {
    let machine = Machine::new();
    let (dir, run) = tree_run(&machine);
    let reold = dir.with_file_name(format!(".review.loadout-{run}-0-rollback.old"));
    write_skill(&reold, "#!/bin/sh\necho someone else\n");
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert!(error.to_string().contains("in the way"), "{error}");
    assert_eq!(read(&dir.join("bin/run.sh")), "#!/bin/sh\necho loadout\n");
    assert_eq!(
        read(&reold.join("bin/run.sh")),
        "#!/bin/sh\necho someone else\n"
    );
}

#[test]
fn rollback_restores_use_exact_modes_so_occupied_probe_names_never_matter() {
    let machine = Machine::new();
    let (dir, run) = tree_run(&machine);
    let stage = dir.with_file_name(format!(".review.loadout-{run}-0-rollback.tmp"));
    let mut probes = Vec::new();
    for suffix in [".mode", ".fmode", ".dmode"] {
        let mut name = stage.as_os_str().to_owned();
        name.push(suffix);
        let probe = PathBuf::from(name);
        fs::write(&probe, "someone else's").unwrap();
        probes.push(probe);
    }
    machine.rollback(None).unwrap();
    assert_eq!(read(&dir.join("bin/run.sh")), "#!/bin/sh\necho mine\n");
    for probe in &probes {
        assert_eq!(read(probe), "someone else's", "{}", probe.display());
    }

    let (a, b, id) = two_target_run(&machine);
    let probe = a.with_file_name(format!("..a.loadout-{id}-0-rollback.tmp.mode"));
    fs::write(&probe, "someone else's").unwrap();
    machine.rollback(None).unwrap();
    assert_eq!(
        (read(&a), read(&b)),
        ("a-before".to_owned(), "b-before".to_owned())
    );
    assert_eq!(read(&probe), "someone else's");
}

#[test]
fn an_occupied_rollback_old_blocks_deleting_a_created_folder() {
    let machine = Machine::new();
    let dir = machine.path(".agents/skills/review");
    let run = machine.apply(&dir, &skill("#!/bin/sh\necho new\n"));
    let reold = dir.with_file_name(format!(".review.loadout-{run}-0-rollback.old"));
    fs::write(&reold, "someone else's").unwrap();
    let error = machine.rollback(None).unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert_eq!(read(&dir.join("bin/run.sh")), "#!/bin/sh\necho new\n");
    assert_eq!(read(&reold), "someone else's");
}

#[test]
fn a_rollback_tmp_that_verifies_as_the_prior_is_reused() {
    let machine = Machine::new();
    let (a, b, id) = two_target_run(&machine);
    let stale = a.with_file_name(format!("..a.loadout-{id}-0-rollback.tmp"));
    fs::write(&stale, "a-before").unwrap();
    let rolled = machine.rollback(None).unwrap();
    assert_eq!(
        (read(&a), read(&b)),
        ("a-before".to_owned(), "b-before".to_owned())
    );
    assert!(
        !stale.exists(),
        "a verified stale staging file is cleared and its name reused"
    );
    assert!(rolled.leftovers.is_empty());
}

fn observe_in(context: &Context, path: &Path) -> Result<Observed, Error> {
    state::observe(context, &context.state.join("loadout"), &target(path))
}

#[cfg(unix)]
#[test]
fn a_symlinked_spelling_of_home_and_its_real_path_are_one_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let real = root.join("real-home");
    let outside = root.join("outside");
    fs::create_dir_all(&real).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let link = root.join("link-home");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("outside-link")).unwrap();
    std::os::unix::fs::symlink(&outside, real.join("out-link")).unwrap();

    let through_link = isolated(&link, &root.join("state"));
    assert!(observe_in(&through_link, &real.join(".gitconfig")).is_ok());
    let at_real = isolated(&real, &root.join("state"));
    assert!(observe_in(&at_real, &link.join(".gitconfig")).is_ok());
    assert!(
        observe_in(&at_real, &real.join("out-link/x")).is_ok(),
        "a link below a root keeps its lexical membership"
    );
    for outside in [
        root.join("outside-link/x"),
        outside.join("x"),
        root.join("real-home2/x"),
    ] {
        assert!(
            matches!(
                observe_in(&at_real, &outside),
                Err(Error::OutsideRoots { .. })
            ),
            "{}",
            outside.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn the_canonical_spelling_of_a_temp_home_is_the_same_root() {
    let dir = tempfile::tempdir().unwrap();
    let lexical = dir.path().join("home");
    fs::create_dir_all(&lexical).unwrap();
    let canonical = lexical.canonicalize().unwrap();
    let state = dir.path().join("state");
    assert!(observe_in(&isolated(&lexical, &state), &canonical.join(".x")).is_ok());
    assert!(observe_in(&isolated(&canonical, &state), &lexical.join(".x")).is_ok());
}

#[cfg(target_os = "linux")]
#[test]
fn a_case_variant_of_home_is_another_folder_on_linux() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let context = isolated(&home, &root.join("state"));
    assert!(matches!(
        observe_in(&context, &root.join("HOME/.x")),
        Err(Error::OutsideRoots { .. })
    ));
}

#[cfg(unix)]
#[test]
fn non_utf8_names_differing_only_in_invalid_bytes_are_different_roots() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let home = root.join(OsStr::from_bytes(b"home-\xff"));
    let twin = root.join(OsStr::from_bytes(b"home-\xfe"));
    assert_eq!(home.to_string_lossy(), twin.to_string_lossy());
    let context = isolated(&home, &root.join("state"));
    assert_eq!(
        observe_in(&context, &home.join(".x")).unwrap(),
        Observed::Absent
    );
    assert!(matches!(
        observe_in(&context, &twin.join(".x")),
        Err(Error::OutsideRoots { .. })
    ));
}

/// `std::fs::canonicalize` on Windows returns the verbatim `\\?\C:\…` spelling; this is the
/// ordinary `C:\…` spelling of the same path.
#[cfg(windows)]
fn ordinary(path: &Path) -> PathBuf {
    let text = path.to_str().unwrap();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(text))
}

#[cfg(windows)]
#[test]
fn verbatim_case_and_ordinary_spellings_of_home_are_one_root_on_windows() {
    let machine = Machine::new();
    let home = ordinary(&machine.home);
    assert!(
        machine.home.to_str().unwrap().starts_with(r"\\?\"),
        "canonical home is verbatim"
    );
    let context = isolated(&home, &ordinary(&machine.context.state));
    let text = home.to_str().unwrap();
    let lower_drive = format!("{}{}", text[..1].to_ascii_lowercase(), &text[1..]);
    for spelling in [
        machine.home.join(".gitconfig"),
        home.join(".gitconfig"),
        PathBuf::from(lower_drive).join(".gitconfig"),
        PathBuf::from(text.to_ascii_uppercase()).join(".gitconfig"),
    ] {
        assert!(
            observe_in(&context, &spelling).is_ok(),
            "{}",
            spelling.display()
        );
    }
    let sibling = format!("{text}2");
    let other_drive = format!("Z{}", &text[1..]);
    for outside in [
        PathBuf::from(&sibling).join(".x"),
        PathBuf::from(format!(r"\\?\{sibling}")).join(".x"),
        PathBuf::from(&other_drive).join(".x"),
        PathBuf::from(format!(r"\\?\{other_drive}")).join(".x"),
        PathBuf::from(r"C:relative\.x"),
        PathBuf::from(r"relative\.x"),
        home.join(r"..\escape\.x"),
    ] {
        assert!(
            matches!(
                observe_in(&context, &outside),
                Err(Error::OutsideRoots { .. })
            ),
            "{}",
            outside.display()
        );
    }
    let auth = home.join(r".codex\auth.json");
    fs::create_dir_all(auth.parent().unwrap()).unwrap();
    fs::write(&auth, "SYNTHETIC").unwrap();
    let verbatim_auth = machine.home.join(r".codex\auth.json");
    assert!(matches!(
        observe_in(&context, &verbatim_auth),
        Err(Error::Reserved { .. })
    ));
    assert_eq!(read(&auth), "SYNTHETIC");
}

/// One full lifecycle with the target spelled one way and the context the other: `observe`,
/// `Run::begin` and `write`, `audit` (status), then a rollback that really restores the file.
#[cfg(windows)]
fn lifecycle(context: &Context, target_home: &Path) {
    let path = target_home.join(".gitconfig");
    fs::write(&path, "mine").unwrap();
    let body = file("loadout's", false);
    let checkout = context.state.join("loadout");
    let action = Action {
        item: item(),
        target: target(&path),
        change: Change::Replace,
        prior: state::observe(context, &checkout, &target(&path)).unwrap(),
        content: Some(body.clone()),
    };
    let lock = state::lock(context).unwrap();
    let mut state = State::load(context).unwrap();
    let plan = Plan {
        actions: vec![action.clone()],
    };
    let mut run = Run::begin(context, &checkout, lock, &state, &plan).unwrap();
    run.write(&action, &body).unwrap();
    run.finish(&mut state).unwrap();
    assert_eq!(read(&path), "loadout's");
    let state = State::load(context).unwrap();
    assert_eq!(state::audit(context, &checkout, &state)[0].1, Health::Ok);
    let rolled = state::rollback(context, None).unwrap();
    assert_eq!(rolled.restored, vec![target(&path)]);
    assert_eq!(read(&path), "mine");
}

#[cfg(windows)]
#[test]
fn a_verbatim_target_runs_its_whole_lifecycle_under_an_ordinary_context() {
    let machine = Machine::new();
    let context = isolated(&ordinary(&machine.home), &ordinary(&machine.context.state));
    lifecycle(&context, &machine.home);
}

#[cfg(windows)]
#[test]
fn an_ordinary_target_runs_its_whole_lifecycle_under_a_verbatim_context() {
    let machine = Machine::new();
    lifecycle(&machine.context, &ordinary(&machine.home));
}

#[cfg(windows)]
#[test]
fn a_run_written_in_one_spelling_is_audited_and_rolled_back_in_the_other() {
    let machine = Machine::new();
    let path = machine.path(".gitconfig");
    fs::write(&path, "mine").unwrap();
    machine.apply(&path, &file("loadout's", false));
    let context = isolated(&ordinary(&machine.home), &ordinary(&machine.context.state));
    let state = State::load(&context).unwrap();
    let audit = state::audit(&context, &context.state.join("loadout"), &state);
    assert_eq!(
        audit[0].1,
        Health::Ok,
        "a finished owned target is not unchecked"
    );
    let rolled = state::rollback(&context, None).unwrap();
    assert_eq!(rolled.restored, vec![target(&path)]);
    assert_eq!(read(&path), "mine");
}

#[cfg(windows)]
#[test]
fn a_target_outside_every_spelling_of_home_is_refused_untouched() {
    let machine = Machine::new();
    let context = isolated(&ordinary(&machine.home), &ordinary(&machine.context.state));
    let sibling = PathBuf::from(format!("{}2", ordinary(&machine.home).display()));
    fs::create_dir_all(&sibling).unwrap();
    let outside = sibling.join(".gitconfig");
    fs::write(&outside, "outside").unwrap();
    let mut permissions = fs::metadata(&outside).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&outside, permissions).unwrap();
    let before = fs::metadata(&outside).unwrap();
    for spelling in [
        outside.clone(),
        PathBuf::from(format!(r"\\?\{}", outside.display())),
    ] {
        assert!(matches!(
            observe_in(&context, &spelling),
            Err(Error::OutsideRoots { .. })
        ));
        let body = file("loadout's", false);
        let action = Action {
            item: item(),
            target: target(&spelling),
            change: Change::Replace,
            prior: present(&"0".repeat(64)),
            content: Some(body),
        };
        let lock = state::lock(&context).unwrap();
        let error = Run::begin(
            &context,
            &context.state.join("loadout"),
            lock,
            &State::load(&context).unwrap(),
            &Plan {
                actions: vec![action],
            },
        )
        .unwrap_err();
        assert!(matches!(error, Error::OutsideRoots { .. }), "{error}");
    }
    let after = fs::metadata(&outside).unwrap();
    assert_eq!(read(&outside), "outside");
    assert!(after.permissions().readonly());
    assert_eq!(after.modified().unwrap(), before.modified().unwrap());
    assert_eq!(after.len(), before.len());
    let mut permissions = after.permissions();
    #[expect(
        clippy::permissions_set_readonly_false,
        reason = "Windows only: clears the read-only attribute so the temp dir can be removed"
    )]
    permissions.set_readonly(false);
    fs::set_permissions(&outside, permissions).unwrap();
}
