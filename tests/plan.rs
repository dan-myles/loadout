use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use loadout::context::Context;
use loadout::plan::{self, Action, Change, Content, Observed, Target};
use loadout::platform::Os;
use loadout::state::{self, State};

/// A synthetic machine: an isolated home, a state dir that doesn't exist yet, and a loadout
/// checkout beside them.
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
    let home = home.as_os_str().to_owned();
    let state = state.as_os_str().to_owned();
    Context::resolve(
        Os::current().unwrap(),
        move |name: &str| -> Option<OsString> {
            match name {
                "LOADOUT_HOME" => Some(home.clone()),
                "LOADOUT_STATE_DIR" => Some(state.clone()),
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

fn build(machine: &Machine) -> Result<plan::Built, plan::Error> {
    build_from(machine, &machine.loadout)
}

fn build_from(machine: &Machine, checkout: &Path) -> Result<plan::Built, plan::Error> {
    let state = State::load(&machine.context).unwrap();
    plan::build(
        &machine.context,
        checkout,
        &state,
        None,
        plan::Options::default(),
    )
}

fn at<'a>(built: &'a plan::Built, path: &Path) -> &'a Action {
    built
        .plan
        .actions
        .iter()
        .find(|action| {
            action.target
                == Target::Path {
                    path: path.to_path_buf(),
                }
        })
        .unwrap_or_else(|| {
            panic!(
                "no action for {}: {:#?}",
                path.display(),
                built.plan.actions
            )
        })
}

fn file_bytes(action: &Action) -> &[u8] {
    match &action.content {
        Some(Content::File(file)) => &file.bytes,
        other => panic!("not a file: {other:?}"),
    }
}

fn untouched(machine: &Machine) {
    assert!(
        !machine.state.exists(),
        "planning never creates the state dir or the lock"
    );
}

#[test]
fn a_fresh_machine_plans_a_create_for_each_file_skill_and_instructions_and_writes_nothing() {
    let machine = machine(&[
        (
            "loadout.toml",
            &format!(
                "{HEADER}[agents]\nenabled = [\"claude\"]\n\
                 [files.git]\nsource = \"files/gitconfig\"\ntarget = \"~/.gitconfig\"\n\
                 [skills.review]\nsource = \"skills/review\"\n\
                 [instructions.global]\nsource = \"AGENTS.md\"\n"
            ),
        ),
        ("files/gitconfig", "[user]\n"),
        ("skills/review/SKILL.md", "review\n"),
        ("AGENTS.md", "be kind\n"),
    ]);
    let built = build(&machine).unwrap();
    let gitconfig = at(&built, &machine.home.join(".gitconfig"));
    let skill = at(
        &built,
        &machine.home.join(".claude").join("skills").join("review"),
    );
    let instructions = at(&built, &machine.home.join(".claude").join("CLAUDE.md"));
    for action in [gitconfig, skill, instructions] {
        assert_eq!(action.change, Change::Create, "{action:?}");
        assert_eq!(action.prior, Observed::Absent);
        assert!(action.content.is_some());
    }
    assert_eq!(file_bytes(gitconfig), b"[user]\n");
    assert!(matches!(skill.content, Some(Content::Tree(_))));
    assert_eq!(built.checkout, machine.loadout.canonicalize().unwrap());
    untouched(&machine);
    assert_eq!(fs::read_dir(&machine.home).unwrap().count(), 0);
}

#[test]
fn a_template_renders_with_the_machine_variables_and_facts() {
    let machine = machine(&[
        (
            "loadout.toml",
            &format!(
                "{HEADER}[variables]\nemail = \"me@example.com\"\n\
                 [files.git]\nsource = \"files/gitconfig.tmpl\"\ntarget = \"~/.gitconfig\"\n"
            ),
        ),
        (
            "files/gitconfig.tmpl",
            "email = {{ email }}\nos = {{ machine.os }}\n",
        ),
    ]);
    let built = build(&machine).unwrap();
    let os = Os::current().unwrap().name();
    assert_eq!(
        file_bytes(at(&built, &machine.home.join(".gitconfig"))),
        format!("email = me@example.com\nos = {os}\n").as_bytes()
    );
}

#[test]
fn a_missing_template_variable_stops_the_plan_with_nothing_written() {
    let machine = machine(&[
        (
            "loadout.toml",
            &format!(
                "{HEADER}[files.git]\nsource = \"files/gitconfig.tmpl\"\ntarget = \"~/.gitconfig\"\n\
                 [files.plain]\nsource = \"files/plain\"\ntarget = \"~/.plain\"\n"
            ),
        ),
        ("files/gitconfig.tmpl", "email = {{ emial }}\n"),
        ("files/plain", "x"),
    ]);
    let Err(plan::Error::Invalid(diagnostics)) = build(&machine) else {
        panic!("a missing variable must make the plan invalid");
    };
    let text: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
    assert!(
        text.iter()
            .any(|line| line.contains("files.git") && line.contains("emial")),
        "{text:?}"
    );
    untouched(&machine);
    assert_eq!(fs::read_dir(&machine.home).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn a_source_that_is_a_protected_file_is_refused_before_it_is_read() {
    let machine = machine(&[]);
    let secret = "SENTINEL-AUTH-BYTES";
    write(&machine.home, ".codex/auth.json", secret);
    write(
        &machine.home,
        "loadout.toml",
        &format!("{HEADER}[files.leak]\nsource = \".codex/auth.json\"\ntarget = \"~/leak\"\n"),
    );
    let Err(plan::Error::Invalid(diagnostics)) = build_from(&machine, &machine.home) else {
        panic!("a source that is an auth store must be refused");
    };
    let text: Vec<String> = diagnostics.iter().map(ToString::to_string).collect();
    assert!(
        text.iter().any(|line| line.contains("files.leak.source")),
        "{text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains(secret)),
        "diagnostics never carry protected content: {text:?}"
    );
    assert!(!machine.home.join("leak").exists());
}

#[cfg(unix)]
#[test]
fn a_target_reaching_a_reserved_folder_through_a_link_is_unsupported_and_never_opened() {
    let machine = machine(&[
        (
            "loadout.toml",
            &format!("{HEADER}[files.alias]\nsource = \"f\"\ntarget = \"~/alias/auth.json\"\n"),
        ),
        ("f", "replacement"),
    ]);
    write(&machine.home, ".codex/auth.json", "SENTINEL");
    std::os::unix::fs::symlink(".codex", machine.home.join("alias")).unwrap();
    let built = build(&machine).unwrap();
    let action = at(&built, &machine.home.join("alias").join("auth.json"));
    assert!(
        matches!(&action.change, Change::Unsupported(reason) if reason.contains("auth")),
        "{action:?}"
    );
    assert_eq!(action.prior, Observed::Absent);
    assert!(action.content.is_none());
    assert_eq!(
        fs::read_to_string(machine.home.join(".codex/auth.json")).unwrap(),
        "SENTINEL"
    );
}

#[test]
fn an_unowned_target_that_matches_is_adopted_and_one_that_differs_is_replaced() {
    let machine = machine(&[
        (
            "loadout.toml",
            &format!(
                "{HEADER}[files.same]\nsource = \"same\"\ntarget = \"~/.same\"\n\
                 [files.other]\nsource = \"other\"\ntarget = \"~/.other\"\n"
            ),
        ),
        ("same", "same"),
        ("other", "loadout's"),
    ]);
    fs::write(machine.home.join(".same"), "same").unwrap();
    fs::write(machine.home.join(".other"), "the user's").unwrap();
    let built = build(&machine).unwrap();
    assert_eq!(
        at(&built, &machine.home.join(".same")).change,
        Change::Adopt
    );
    let other = at(&built, &machine.home.join(".other"));
    assert_eq!(other.change, Change::Replace);
    assert!(matches!(other.prior, Observed::Present { .. }));
    assert!(!built.plan.blocked(), "a replace is not left undone");
}

#[test]
fn agents_without_an_instructions_file_get_unsupported_actions_with_no_target() {
    let machine = machine(&[
        (
            "loadout.toml",
            &format!(
                "{HEADER}[agents]\nenabled = [\"cursor\", \"muse\"]\n\
                 [instructions.global]\nsource = \"AGENTS.md\"\n"
            ),
        ),
        ("AGENTS.md", "x"),
    ]);
    let built = build(&machine).unwrap();
    let none: Vec<&Action> = built
        .plan
        .actions
        .iter()
        .filter(|action| action.target == Target::None)
        .collect();
    assert_eq!(none.len(), 2, "{:#?}", built.plan.actions);
    for action in none {
        assert!(
            matches!(action.change, Change::Unsupported(_)),
            "{action:?}"
        );
        assert_eq!(action.prior, Observed::Absent);
        assert!(action.content.is_none());
    }
    built.plan.check().unwrap();
}

#[test]
fn programs_under_isolation_are_blocked_with_no_target_and_no_manager_runs() {
    let machine = machine(&[(
        "loadout.toml",
        &format!(
            "{HEADER}[programs.ripgrep]\nbrew = \"ripgrep\"\napt = \"ripgrep\"\nwinget = \"BurntSushi.ripgrep.MSVC\"\n\
             [programs.fd]\nbrew = \"fd\"\napt = \"fd-find\"\nwinget = \"sharkdp.fd\"\n"
        ),
    )]);
    let built = build(&machine).unwrap();
    assert_eq!(built.plan.actions.len(), 2);
    for action in &built.plan.actions {
        assert_eq!(action.target, Target::None);
        assert_eq!(
            action.change,
            Change::Blocked(loadout::programs::ISOLATED.to_owned())
        );
        assert!(built.steps.get(&action.item.id).is_some());
    }
    assert!(built.plan.blocked());
    built.plan.check().unwrap();
}

#[test]
fn an_interrupted_run_blocks_every_new_plan_until_it_is_rolled_back() {
    let machine = machine(&[
        (
            "loadout.toml",
            &format!("{HEADER}[files.a]\nsource = \"a\"\ntarget = \"~/.a\"\n"),
        ),
        ("a", "x"),
    ]);
    let built = build(&machine).unwrap();
    let lock = state::lock(&machine.context).unwrap();
    let state = State::load(&machine.context).unwrap();
    let run =
        state::Run::begin(&machine.context, &built.checkout, lock, &state, &built.plan).unwrap();
    drop(run);
    let Err(plan::Error::Blocked(message)) = build(&machine) else {
        panic!("an interrupted run must block planning");
    };
    assert!(message.contains("loadout rollback"), "{message}");
    state::rollback(&machine.context, None).unwrap();
    build(&machine).unwrap();
}

#[test]
fn an_unowned_mcp_server_that_differs_is_blocked_and_its_file_is_never_touched() {
    let machine = machine(&[(
        "loadout.toml",
        &format!(
            "{HEADER}[agents]\nenabled = [\"claude\"]\n\
                 [mcp.docs]\ncommand = \"docs-server\"\n"
        ),
    )]);
    let claude = machine.home.join(".claude.json");
    let original = "{\"oauthAccount\":{\"id\":\"SENTINEL\"},\"mcpServers\":{\"docs\":{\"type\":\"stdio\",\"command\":\"the-users-own\",\"args\":[],\"env\":{}}}}";
    fs::write(&claude, original).unwrap();
    let built = build(&machine).unwrap();
    let docs = built
        .plan
        .actions
        .iter()
        .find(|action| matches!(&action.target, Target::Entry { key, .. } if key == "docs"))
        .expect("the server has an entry action");
    assert!(matches!(docs.change, Change::Blocked(_)), "{docs:?}");
    assert!(docs.content.is_none());
    assert_eq!(fs::read_to_string(&claude).unwrap(), original);
}

#[test]
#[ignore = "needs the U3 entry transaction: state::observe of an MCP entry (u3-entry-integration)"]
fn an_unowned_mcp_server_that_already_matches_is_adopted() {
    let machine = machine(&[(
        "loadout.toml",
        &format!(
            "{HEADER}[agents]\nenabled = [\"claude\"]\n\
             [mcp.docs]\ncommand = \"docs-server\"\n"
        ),
    )]);
    let claude = machine.home.join(".claude.json");
    fs::write(
        &claude,
        "{\"mcpServers\":{\"docs\":{\"type\":\"stdio\",\"command\":\"docs-server\",\"args\":[],\"env\":{}}}}",
    )
    .unwrap();
    let built = build(&machine).unwrap();
    let docs = built
        .plan
        .actions
        .iter()
        .find(|action| matches!(&action.target, Target::Entry { key, .. } if key == "docs"))
        .unwrap();
    assert_eq!(docs.change, Change::Adopt);
}

#[test]
fn a_checkout_that_is_not_a_folder_is_a_runtime_error_with_a_way_forward() {
    let machine = machine(&[]);
    let error = plan::checkout(&machine.context, None).unwrap_err();
    assert!(error.to_string().contains("--loadout"), "{error}");
    assert_eq!(
        plan::checkout(&machine.context, Some(machine.home.as_path())).unwrap(),
        machine.home
    );
}

#[test]
fn distinct_mcp_keys_in_one_document_are_separate_destinations() {
    let machine = machine(&[(
        "loadout.toml",
        &format!(
            "{HEADER}[agents]\nenabled = [\"claude\"]\n\
             [mcp.docs]\ncommand = \"docs-server\"\n\
             [mcp.search]\ncommand = \"search-server\"\n"
        ),
    )]);
    let built = build(&machine).expect("two keys in one file are not a collision");
    let keys: Vec<&str> = built
        .plan
        .actions
        .iter()
        .filter_map(|action| match &action.target {
            Target::Entry { key, .. } => Some(key.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(keys, ["docs", "search"]);
    built.plan.check().unwrap();
}

/// The whole pipeline, from the manifest: `alpha` can't take the `docs` server, whose env
/// reference it has no syntax for, so U5 refuses it at its concrete entry target; `beta` takes
/// `search` in another format, at the same document through a link. U1's path check takes the
/// custom MCP destinations, refused ones included, and would reject two formats at one written
/// path, but it is lexical, so the two spellings pass. U5's collision check compares lexical and
/// normalized forms, but of desired items only, so the refused `docs` never meets `search`
/// there. Loadout owns `docs` there as `json cursor-json`: nothing writes it now, but the
/// record stays, and so does what it owns, so `search` is blocked rather than putting a second
/// format into that document. Without the record nothing is owned, and only the interim entry
/// refusal applies.
#[cfg(unix)]
#[test]
fn an_owned_key_refused_for_its_agent_keeps_guarding_its_document_through_a_link() {
    let manifest = format!(
        "{HEADER}[agents]\nenabled = [\"alpha\", \"beta\"]\n\
         [agents.custom.alpha]\nevidence = \"e\"\n\
         mcp = {{ linux = \"~/alias/custom.json\", macos = \"~/alias/custom.json\", format = \"json\", map = \"/mcpServers\", shape = \"cursor-json\", env_syntax = \"none\" }}\n\
         [agents.custom.beta]\nevidence = \"e\"\n\
         mcp = {{ linux = \"~/real/custom.json\", macos = \"~/real/custom.json\", format = \"jsonc\", map = \"/mcpServers\", shape = \"cursor-json\", env_syntax = \"none\" }}\n\
         [mcp.docs]\ncommand = \"docs-server\"\nenv = {{ TOKEN = {{ env = \"DOCS_TOKEN\" }} }}\nagents = [\"alpha\"]\n\
         [mcp.search]\ncommand = \"search-server\"\nagents = [\"beta\"]\n"
    );
    for owned in [true, false] {
        let machine = machine(&[("loadout.toml", &manifest)]);
        fs::create_dir(machine.home.join("real")).unwrap();
        std::os::unix::fs::symlink("real", machine.home.join("alias")).unwrap();
        let docs = Target::Entry {
            file: machine.home.join("alias").join("custom.json"),
            format: loadout::manifest::McpFormat::Json,
            pointer: "/mcpServers".to_owned(),
            key: "docs".to_owned(),
            shape: loadout::manifest::McpShape::CursorJson,
        };
        if owned {
            let record = serde_json::json!({
                "target": docs,
                "item": { "kind": "mcp", "id": "docs", "agent": "alpha" },
                "sha256": "a".repeat(64),
                "run": "20261025T000000.000000000Z",
            });
            fs::create_dir_all(&machine.state).unwrap();
            fs::write(
                machine.state.join("state.json"),
                serde_json::to_vec(&serde_json::json!({ "version": 1, "records": [record] }))
                    .unwrap(),
            )
            .unwrap();
        }
        let built = build(&machine).expect("a refusal is a typed plan, not an error");
        let actions = &built.plan.actions;
        assert_eq!(
            actions.len(),
            2,
            "no removal and no action for the old record: {actions:#?}"
        );
        let refused = actions
            .iter()
            .find(|action| action.target == docs)
            .unwrap_or_else(|| panic!("docs keeps its concrete target: {actions:#?}"));
        assert!(
            matches!(&refused.change, Change::Unsupported(reason) if reason.contains("no documented env syntax")),
            "{refused:?}"
        );
        assert_eq!(refused.prior, Observed::Absent);
        assert!(refused.content.is_none());
        let search = actions
            .iter()
            .find(|action| matches!(&action.target, Target::Entry { key, .. } if key == "search"))
            .expect("search has an entry action");
        let Change::Blocked(reason) = &search.change else {
            panic!("an entry is never written on this base: {search:?}");
        };
        assert!(search.content.is_none());
        assert_eq!(
            reason.contains("still owns") && reason.contains("as json cursor-json"),
            owned,
            "{reason}"
        );
        assert!(
            reason.contains("needs the mcp module"),
            "the first refusal stays: {reason}"
        );
        assert!(!actions.iter().any(|action| action.change == Change::Remove));
        built.plan.check().unwrap();
        let records = State::load(&machine.context).unwrap();
        assert_eq!(
            records.records().count(),
            usize::from(owned),
            "the record stays as it was"
        );
        assert!(!machine.home.join("real").join("custom.json").exists());
    }
}
