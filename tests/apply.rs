use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use loadout::apply::{self, Mode, Options, Outcome};
use loadout::context::Context;
use loadout::plan::{self, Change};
use loadout::platform::Os;
use loadout::state::{self, RunStatus, State};

struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
    loadout: PathBuf,
    context: Context,
}

fn machine(files: &[(&str, &str)]) -> Machine {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let state = dir.path().join("state");
    let loadout = dir.path().join("loadout");
    fs::create_dir(&home).unwrap();
    for (path, text) in files {
        write(&loadout, path, text);
    }
    let context = context_for(&home, &state);
    Machine {
        _dir: dir,
        home,
        state,
        loadout,
        context,
    }
}

fn context_for(home: &Path, state: &Path) -> Context {
    let vars = (home.as_os_str().to_owned(), state.as_os_str().to_owned());
    Context::resolve(
        Os::current().unwrap(),
        move |name: &str| -> Option<OsString> {
            match name {
                "LOADOUT_HOME" => Some(vars.0.clone()),
                "LOADOUT_STATE_DIR" => Some(vars.1.clone()),
                _ => None,
            }
        },
        None,
    )
    .unwrap()
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

const HEADER: &str = "[loadout]\nversion = 1\n";

const INTERACTIVE: Options = Options {
    force: false,
    elevated: false,
    mode: Mode::Interactive,
};

impl Machine {
    fn apply(&self, options: Options) -> Result<apply::Report, apply::Error> {
        apply::run(&self.context, &self.loadout, options)
    }

    fn manifest(&self, text: &str) {
        write(&self.loadout, "loadout.toml", &format!("{HEADER}{text}"));
    }

    fn runs(&self) -> Vec<state::RunSummary> {
        state::runs(&self.context).unwrap()
    }

    fn plan(&self) -> plan::Built {
        let state = State::load(&self.context).unwrap();
        plan::build(
            &self.context,
            &self.loadout,
            &state,
            None,
            plan::Options::default(),
        )
        .unwrap()
    }
}

fn outcome_at<'a>(report: &'a apply::Report, path: &Path) -> &'a Outcome {
    report
        .outcomes
        .iter()
        .find(|(action, _)| action.target == plan::Target::Path { path: path.to_path_buf() })
        .map(|(_, outcome)| outcome)
        .unwrap_or_else(|| panic!("no outcome for {}: {:#?}", path.display(), report.outcomes))
}

const TWO_FILES: &str =
    "[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n[files.b]\nsource = \"b\"\ntarget = \"~/.b\"\n";

#[test]
fn apply_writes_records_and_a_second_apply_changes_nothing_and_starts_no_run() {
    let machine = machine(&[("a", "first"), ("b", "second")]);
    machine.manifest(TWO_FILES);
    let report = machine.apply(INTERACTIVE).unwrap();
    assert!(!report.failed(), "{report:#?}");
    assert!(!report.unhealthy());
    assert_eq!(outcome_at(&report, &machine.home.join(".a")), &Outcome::Done);
    assert_eq!(fs::read_to_string(machine.home.join(".a")).unwrap(), "first");
    assert_eq!(fs::read_to_string(machine.home.join(".b")).unwrap(), "second");
    assert!(report.run.is_some());
    let runs = machine.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, Some(RunStatus::Finished));
    assert_eq!(runs[0].actions, 2);

    let again = machine.apply(INTERACTIVE).unwrap();
    assert_eq!(again.run, None, "a run with nothing to write starts no run");
    assert!(again
        .outcomes
        .iter()
        .all(|(_, outcome)| *outcome == Outcome::Unchanged));
    assert_eq!(machine.runs().len(), 1, "no empty journal was created");
    assert!(machine
        .plan()
        .plan
        .actions
        .iter()
        .all(|action| action.change == Change::Unchanged));
}

#[test]
fn a_missing_template_variable_stops_apply_before_the_lock_or_any_write() {
    let machine = machine(&[("a", "x"), ("t.tmpl", "{{ nope }}")]);
    machine.manifest(
        "[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n[files.t]\nsource = \"t.tmpl\"\ntarget = \"~/.t\"\n",
    );
    let Err(apply::Error::Plan(plan::Error::Invalid(_))) = machine.apply(INTERACTIVE) else {
        panic!("a missing variable must stop apply");
    };
    assert!(!machine.state.exists(), "no state dir and no lock file");
    assert_eq!(fs::read_dir(&machine.home).unwrap().count(), 0);
}

#[test]
fn a_drifted_target_is_skipped_until_force_backs_it_up_and_overwrites_it() {
    let machine = machine(&[("a", "loadout's")]);
    machine.manifest("[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n");
    machine.apply(INTERACTIVE).unwrap();
    let target = machine.home.join(".a");
    fs::write(&target, "the user's edit").unwrap();

    let skipped = machine.apply(INTERACTIVE).unwrap();
    assert!(matches!(outcome_at(&skipped, &target), Outcome::Skipped(_)));
    assert!(skipped.unhealthy());
    assert_eq!(fs::read_to_string(&target).unwrap(), "the user's edit");
    assert!(machine.plan().plan.blocked());

    let forced = machine
        .apply(Options {
            force: true,
            ..INTERACTIVE
        })
        .unwrap();
    assert_eq!(outcome_at(&forced, &target), &Outcome::Done);
    assert_eq!(fs::read_to_string(&target).unwrap(), "loadout's");
    state::rollback(&machine.context, None).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "the user's edit");
}

#[test]
fn an_unowned_target_is_replaced_only_by_interactive_apply() {
    let machine = machine(&[("a", "loadout's")]);
    machine.manifest("[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n");
    let target = machine.home.join(".a");
    fs::write(&target, "the user's").unwrap();

    let background = machine
        .apply(Options {
            force: true,
            elevated: true,
            mode: Mode::Background,
        })
        .unwrap();
    assert!(matches!(outcome_at(&background, &target), Outcome::Skipped(_)));
    assert!(background.unhealthy());
    assert_eq!(fs::read_to_string(&target).unwrap(), "the user's");
    assert!(machine.runs().is_empty(), "nothing was journaled");

    let interactive = machine.apply(INTERACTIVE).unwrap();
    assert_eq!(outcome_at(&interactive, &target), &Outcome::Done);
    assert_eq!(fs::read_to_string(&target).unwrap(), "loadout's");
    state::rollback(&machine.context, None).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "the user's");
}

#[test]
fn an_item_the_loadout_drops_is_removed_and_rollback_brings_it_back() {
    let machine = machine(&[("a", "first"), ("b", "second")]);
    machine.manifest(TWO_FILES);
    machine.apply(INTERACTIVE).unwrap();
    machine.manifest("[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n");
    let report = machine.apply(INTERACTIVE).unwrap();
    assert_eq!(
        outcome_at(&report, &machine.home.join(".b")),
        &Outcome::Removed
    );
    assert!(!machine.home.join(".b").exists());
    assert_eq!(fs::read_to_string(machine.home.join(".a")).unwrap(), "first");
    state::rollback(&machine.context, None).unwrap();
    assert_eq!(fs::read_to_string(machine.home.join(".b")).unwrap(), "second");
}

#[test]
fn a_second_apply_while_the_lock_is_held_is_refused_and_a_held_lock_can_be_passed_in() {
    let machine = machine(&[("a", "x")]);
    machine.manifest("[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n");
    let lock = state::lock(&machine.context).unwrap();
    let Err(apply::Error::State(state::Error::Locked { .. })) = machine.apply(INTERACTIVE) else {
        panic!("a second run must not start while the lock is held");
    };
    assert!(!machine.home.join(".a").exists());

    let report = apply::run_under_lock(&machine.context, &machine.loadout, lock, INTERACTIVE).unwrap();
    assert_eq!(outcome_at(&report, &machine.home.join(".a")), &Outcome::Done);
    state::lock(&machine.context).expect("run_under_lock releases the lock it was given");
}

#[test]
fn an_interrupted_run_blocks_apply_with_no_new_run() {
    let machine = machine(&[("a", "x")]);
    machine.manifest("[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n");
    let built = machine.plan();
    let lock = state::lock(&machine.context).unwrap();
    let state = State::load(&machine.context).unwrap();
    drop(state::Run::begin(&machine.context, &built.checkout, lock, &state, &built.plan).unwrap());
    let Err(apply::Error::Plan(plan::Error::Blocked(message))) = machine.apply(INTERACTIVE) else {
        panic!("an interrupted run must block apply");
    };
    assert!(message.contains("rollback"), "{message}");
    assert_eq!(machine.runs().len(), 1);
    assert!(!machine.home.join(".a").exists());
    let held = state::lock(&machine.context).unwrap();
    let Err(apply::Error::Plan(plan::Error::Blocked(_))) =
        apply::run_under_lock(&machine.context, &machine.loadout, held, INTERACTIVE)
    else {
        panic!("the gate also holds under a caller's lock");
    };
}

#[test]
fn programs_blocked_by_isolation_are_reported_and_never_journaled() {
    let machine = machine(&[("a", "x")]);
    machine.manifest(
        "[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n\
         [programs.ripgrep]\nbrew = \"ripgrep\"\napt = \"ripgrep\"\nwinget = \"BurntSushi.ripgrep.MSVC\"\n",
    );
    let report = machine
        .apply(Options {
            elevated: true,
            ..INTERACTIVE
        })
        .unwrap();
    let program = report
        .outcomes
        .iter()
        .find(|(action, _)| action.item.id == "ripgrep")
        .unwrap();
    assert_eq!(program.0.target, plan::Target::None);
    assert_eq!(
        program.1,
        Outcome::Blocked(loadout::programs::ISOLATED.to_owned())
    );
    assert!(report.unhealthy());
    assert_eq!(machine.runs()[0].actions, 1, "only the file was journaled");
}

#[cfg(unix)]
#[test]
fn a_user_file_keeps_its_execute_bit_and_instructions_never_get_one() {
    use std::os::unix::fs::PermissionsExt;

    let machine = machine(&[("bin/tool", "#!/bin/sh\n"), ("AGENTS.md", "x")]);
    machine.manifest(
        "[agents]\nenabled = [\"claude\"]\n\
         [files.tool]\nsource = \"bin/tool\"\ntarget = \"~/.local/bin/tool\"\n\
         [instructions.global]\nsource = \"AGENTS.md\"\n",
    );
    for source in ["bin/tool", "AGENTS.md"] {
        fs::set_permissions(machine.loadout.join(source), fs::Permissions::from_mode(0o755)).unwrap();
    }
    let report = machine.apply(INTERACTIVE).unwrap();
    assert!(!report.failed(), "{report:#?}");
    let mode = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode();
    assert_ne!(mode(machine.home.join(".local/bin/tool")) & 0o111, 0);
    assert_eq!(mode(machine.home.join(".claude/CLAUDE.md")) & 0o111, 0);
}

/// Every entry under `root` without following links: a file's bytes and mode, a folder, or a
/// link's literal target. Two snapshots differ on any write, mode change, scratch file, or link.
fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, String> {
    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let kind = metadata.file_type();
            if kind.is_symlink() {
                out.insert(relative, format!("link {}", fs::read_link(&path).unwrap().display()));
            } else if kind.is_dir() {
                out.insert(relative, format!("dir {}", mode(&metadata)));
                walk(root, &path, out);
            } else {
                let bytes = fs::read(&path).unwrap();
                out.insert(relative, format!("file {} {bytes:?}", mode(&metadata)));
            }
        }
    }
    #[cfg(unix)]
    fn mode(metadata: &fs::Metadata) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    }
    #[cfg(not(unix))]
    fn mode(metadata: &fs::Metadata) -> u32 {
        u32::from(metadata.permissions().readonly())
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[cfg(unix)]
fn alias(machine: &Machine) {
    fs::create_dir_all(machine.home.join("real")).unwrap();
    std::os::unix::fs::symlink("real", machine.home.join("alias")).unwrap();
}

#[cfg(unix)]
fn change_at<'a>(built: &'a plan::Built, path: &Path) -> &'a Change {
    &built
        .plan
        .actions
        .iter()
        .find(|action| action.target == plan::Target::Path { path: path.to_path_buf() })
        .unwrap_or_else(|| panic!("no action for {}: {:#?}", path.display(), built.plan.actions))
        .change
}

#[cfg(unix)]
#[test]
fn an_owned_file_declared_again_through_a_link_is_never_removed() {
    let machine = machine(&[("tool", "#!/bin/sh\n")]);
    alias(&machine);
    machine.manifest("[files.tool]\nsource = \"tool\"\ntarget = \"~/real/tool\"\n");
    machine.apply(INTERACTIVE).unwrap();
    let real = machine.home.join("real/tool");
    assert!(real.is_file());

    machine.manifest("[files.tool]\nsource = \"tool\"\ntarget = \"~/alias/tool\"\n");
    let built = machine.plan();
    assert!(
        matches!(change_at(&built, &real), Change::Blocked(reason) if reason.contains("loadout owns")),
        "{:#?}",
        built.plan.actions
    );
    assert_eq!(change_at(&built, &machine.home.join("alias/tool")), &Change::Adopt);

    let before = fs::read(&real).unwrap();
    let report = machine.apply(INTERACTIVE).unwrap();
    assert!(matches!(outcome_at(&report, &real), Outcome::Blocked(_)));
    assert!(report.unhealthy());
    assert_eq!(fs::read(&real).unwrap(), before, "the declared file survives");
    assert!(
        !snapshot(&machine.home)
            .keys()
            .any(|path| path.to_string_lossy().contains(".loadout-")),
        "no scratch is left behind"
    );
}

#[cfg(unix)]
#[test]
fn two_owned_spellings_of_one_file_are_never_removed_while_one_is_declared_or_both_are_dropped() {
    let machine = machine(&[("tool", "x")]);
    alias(&machine);
    fs::write(machine.home.join("real/tool"), "x").unwrap();
    machine.manifest("");
    let real = machine.home.join("real/tool");
    let via = machine.home.join("alias/tool");

    // Records left by an older build: both spellings adopted. U4 itself now refuses to declare
    // both, so the fixture writes them through state's own public run API.
    let content = loadout::plan::Content::File(loadout::plan::FileContent {
        bytes: b"x".to_vec(),
        executable: false,
    });
    let adopt = |path: &Path| plan::Action {
        item: plan::ItemRef {
            kind: plan::ItemKind::File,
            id: path.file_name().unwrap().to_string_lossy().into_owned(),
            agent: None,
        },
        target: plan::Target::Path { path: path.to_path_buf() },
        change: Change::Adopt,
        prior: state::observe(&machine.context, &machine.loadout, &plan::Target::Path { path: path.to_path_buf() }).unwrap(),
        content: Some(content.clone()),
    };
    let mut adopt_via = adopt(&via);
    adopt_via.item.id = "via".to_owned();
    let fixture = plan::Plan {
        actions: vec![adopt(&real), adopt_via],
    };
    let lock = state::lock(&machine.context).unwrap();
    let mut records = State::load(&machine.context).unwrap();
    let mut run = state::Run::begin(&machine.context, &machine.loadout, lock, &records, &fixture).unwrap();
    for action in &fixture.actions {
        run.write(action, &content).unwrap();
    }
    run.finish(&mut records).unwrap();
    assert_eq!(State::load(&machine.context).unwrap().records().count(), 2);

    machine.manifest("[files.via]\nsource = \"tool\"\ntarget = \"~/alias/tool\"\n");
    let built = machine.plan();
    assert!(matches!(change_at(&built, &real), Change::Blocked(_)), "{:#?}", built.plan.actions);
    let report = machine.apply(INTERACTIVE).unwrap();
    assert!(matches!(outcome_at(&report, &real), Outcome::Blocked(_)));
    assert_eq!(fs::read_to_string(&real).unwrap(), "x");

    machine.manifest("");
    let built = machine.plan();
    for path in [&real, &via] {
        assert!(
            matches!(change_at(&built, path), Change::Blocked(reason) if reason.contains("removes neither")),
            "{:#?}",
            built.plan.actions
        );
    }
    let report = machine.apply(INTERACTIVE).unwrap();
    assert!(report.unhealthy());
    assert_eq!(fs::read_to_string(&real).unwrap(), "x");
}

#[cfg(unix)]
#[test]
fn two_declared_spellings_of_one_file_are_invalid_before_any_write_with_equal_or_unequal_bytes() {
    for (first, second) in [("same", "same"), ("one", "other")] {
        let machine = machine(&[("a", first), ("b", second)]);
        alias(&machine);
        machine.manifest(
            "[files.a]\nsource = \"a\"\ntarget = \"~/real/tool\"\n[files.b]\nsource = \"b\"\ntarget = \"~/alias/tool\"\n",
        );
        let before = snapshot(&machine.home);
        let Err(apply::Error::Plan(plan::Error::Invalid(diagnostics))) = machine.apply(INTERACTIVE) else {
            panic!("two writers of one file must be invalid: {first} {second}");
        };
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("are one destination")),
            "{diagnostics:?}"
        );
        assert_eq!(snapshot(&machine.home), before, "no partial write");
        assert!(!machine.state.exists(), "no state, journal, or lock");
    }
}

#[cfg(unix)]
#[test]
fn distinct_files_through_a_folder_link_are_both_written() {
    let machine = machine(&[("a", "a"), ("b", "b")]);
    alias(&machine);
    machine.manifest(
        "[files.a]\nsource = \"a\"\ntarget = \"~/alias/a\"\n[files.b]\nsource = \"b\"\ntarget = \"~/real/b\"\n",
    );
    let report = machine.apply(INTERACTIVE).unwrap();
    assert!(!report.failed() && !report.unhealthy(), "{report:#?}");
    assert_eq!(fs::read_to_string(machine.home.join("real/a")).unwrap(), "a");
    assert_eq!(fs::read_to_string(machine.home.join("real/b")).unwrap(), "b");
}

#[test]
fn a_lock_for_another_state_dir_is_refused_before_any_work_on_every_route() {
    for manifest in [
        "",
        "[programs.ripgrep]\nbrew = \"ripgrep\"\napt = \"ripgrep\"\nwinget = \"BurntSushi.ripgrep.MSVC\"\n",
        "[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n",
    ] {
        let machine = machine(&[("a", "x")]);
        machine.manifest(manifest);
        fs::create_dir(&machine.state).unwrap();
        let other = tempfile::tempdir().unwrap();
        let elsewhere = context_for(&machine.home, &other.path().join("state"));
        let wrong = state::lock(&elsewhere).unwrap();
        let before = snapshot(&machine.home);
        let result = apply::run_under_lock(&machine.context, &machine.loadout, wrong, INTERACTIVE);
        assert!(
            matches!(result, Err(apply::Error::State(state::Error::WrongLock { .. }))),
            "{manifest:?}: {result:?}"
        );
        assert_eq!(snapshot(&machine.home), before, "{manifest:?}");
        assert!(
            snapshot(&machine.state).is_empty(),
            "{manifest:?}: the check creates no lock file, run, or record"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_lock_taken_through_a_link_to_the_same_state_dir_is_accepted() {
    let machine = machine(&[("a", "x")]);
    machine.manifest("[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n");
    fs::create_dir(&machine.state).unwrap();
    let link = machine.state.with_file_name("state-link");
    std::os::unix::fs::symlink(&machine.state, &link).unwrap();
    let through_link = context_for(&machine.home, &link);
    let lock = state::lock(&through_link).unwrap();
    let report = apply::run_under_lock(&machine.context, &machine.loadout, lock, INTERACTIVE).unwrap();
    assert_eq!(outcome_at(&report, &machine.home.join(".a")), &Outcome::Done);
    state::lock(&machine.context).expect("the lock is released once the report is built");
}

#[cfg(target_os = "linux")]
#[test]
fn on_linux_an_alias_and_a_case_spelling_in_an_unproven_folder_are_both_blocked_before_any_write() {
    for (first, second) in [("same", "same"), ("one", "other")] {
        let machine = machine(&[("a", first), ("b", second)]);
        alias(&machine);
        machine.manifest(
            "[files.a]\nsource = \"a\"\ntarget = \"~/alias/Foo\"\n[files.b]\nsource = \"b\"\ntarget = \"~/real/foo\"\n",
        );
        let built = machine.plan();
        for path in [machine.home.join("alias/Foo"), machine.home.join("real/foo")] {
            assert!(
                matches!(change_at(&built, &path), Change::Blocked(reason) if reason.contains("can't tell how")),
                "an empty deciding folder proves nothing: {:#?}",
                built.plan.actions
            );
        }
        let before = snapshot(&machine.home);
        let report = machine.apply(INTERACTIVE).unwrap();
        assert!(report.unhealthy());
        assert_eq!(snapshot(&machine.home), before, "neither spelling was written");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn on_linux_a_proven_case_sensitive_folder_keeps_an_alias_and_a_case_spelling_apart() {
    let machine = machine(&[("a", "a"), ("b", "b")]);
    alias(&machine);
    fs::write(machine.home.join("real/seed"), "witness").unwrap();
    machine.manifest(
        "[files.a]\nsource = \"a\"\ntarget = \"~/alias/Foo\"\n[files.b]\nsource = \"b\"\ntarget = \"~/real/foo\"\n",
    );
    let report = machine.apply(INTERACTIVE).unwrap();
    assert!(!report.failed() && !report.unhealthy(), "{report:#?}");
    assert_eq!(fs::read_to_string(machine.home.join("real/Foo")).unwrap(), "a");
    assert_eq!(fs::read_to_string(machine.home.join("real/foo")).unwrap(), "b");
}

#[cfg(target_os = "linux")]
#[test]
fn on_linux_an_owned_real_spelling_and_a_declared_alias_case_spelling_follow_the_folder_proof() {
    let machine = machine(&[("tool", "x")]);
    alias(&machine);
    machine.manifest("[files.tool]\nsource = \"tool\"\ntarget = \"~/real/foo\"\n");
    machine.apply(INTERACTIVE).unwrap();
    machine.manifest("[files.tool]\nsource = \"tool\"\ntarget = \"~/alias/Foo\"\n");
    let built = machine.plan();
    assert_eq!(
        change_at(&built, &machine.home.join("real/foo")),
        &Change::Remove,
        "real/foo proves its folder case-sensitive, so the two spellings are two files"
    );
    assert_eq!(change_at(&built, &machine.home.join("alias/Foo")), &Change::Create);
}

#[test]
fn a_wrong_lock_is_refused_before_the_interrupted_gate_on_every_route() {
    for manifest in [
        "",
        "[programs.ripgrep]\nbrew = \"ripgrep\"\napt = \"ripgrep\"\nwinget = \"BurntSushi.ripgrep.MSVC\"\n",
        "[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n",
        "[agents]\nenabled = [\"claude\"]\n[mcp.docs]\ncommand = \"docs-server\"\n",
        "[files.poison]\nsource = \"poison.tmpl\"\ntarget = \"~/.poison\"\n",
    ] {
        let machine = machine(&[("a", "x"), ("poison.tmpl", "{{ never_defined }}")]);
        machine.manifest(manifest);
        let witness = plan::Plan {
            actions: vec![plan::Action {
                item: plan::ItemRef {
                    kind: plan::ItemKind::File,
                    id: "witness".to_owned(),
                    agent: None,
                },
                target: plan::Target::Path {
                    path: machine.home.join(".witness"),
                },
                change: Change::Create,
                prior: plan::Observed::Absent,
                content: Some(plan::Content::File(plan::FileContent {
                    bytes: b"w".to_vec(),
                    executable: false,
                })),
            }],
        };
        let own = state::lock(&machine.context).unwrap();
        let state = State::load(&machine.context).unwrap();
        drop(state::Run::begin(&machine.context, &machine.loadout, own, &state, &witness).unwrap());
        let before_runs = machine.runs();
        let before_state = snapshot(&machine.state);
        let before_home = snapshot(&machine.home);

        let other = tempfile::tempdir().unwrap();
        let wrong = state::lock(&context_for(&machine.home, &other.path().join("state"))).unwrap();
        let result = apply::run_under_lock(&machine.context, &machine.loadout, wrong, INTERACTIVE);
        assert!(
            matches!(result, Err(apply::Error::State(state::Error::WrongLock { .. }))),
            "{manifest:?}: the interrupted gate would have answered first: {result:?}"
        );
        assert_eq!(machine.runs(), before_runs, "{manifest:?}");
        assert_eq!(snapshot(&machine.state), before_state, "{manifest:?}");
        assert_eq!(snapshot(&machine.home), before_home, "{manifest:?}");
    }
}

/// Needs U3's `Run::finish_with_lock`, which this base doesn't have yet.
#[cfg(unix)]
#[test]
fn a_finish_that_cannot_save_its_records_keeps_every_outcome_and_exits_one() {
    use std::os::unix::fs::PermissionsExt;

    let machine = machine(&[("a", "x")]);
    machine.manifest("[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n");
    fs::create_dir_all(machine.state.join("runs")).unwrap();
    let lock = state::lock(&machine.context).unwrap();
    fs::set_permissions(&machine.state, fs::Permissions::from_mode(0o555)).unwrap();
    let writable = fs::write(machine.state.join("probe"), "x").is_ok();
    if writable {
        fs::set_permissions(&machine.state, fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!("running with permission to write a 0555 folder (root?); skipped");
        return;
    }
    let report = apply::run_under_lock(&machine.context, &machine.loadout, lock, INTERACTIVE);
    fs::set_permissions(&machine.state, fs::Permissions::from_mode(0o700)).unwrap();
    let report = report.unwrap();
    assert!(report.finish_error.is_some(), "{report:#?}");
    assert!(report.failed(), "a finish error exits 1");
    assert_eq!(
        outcome_at(&report, &machine.home.join(".a")),
        &Outcome::Done,
        "the write that reached the disk is still reported"
    );
    state::lock(&machine.context).expect("the lock is released after the report");
}
