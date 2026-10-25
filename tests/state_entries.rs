use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use loadout::context::Context;
use loadout::manifest::{McpFormat, McpShape};
use loadout::mcp;
use loadout::plan::{Action, Change, Content, ItemKind, ItemRef, Observed, Plan, Target};
use loadout::platform::Os;
use loadout::state::{self, Error, Health, Removal, Run, State};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Every path under a machine's root: its kind, its bytes or link target, and its Unix mode.
type Snapshot = BTreeMap<PathBuf, (String, Vec<u8>, u32)>;

/// What a snapshot records for a file it must not read: kind, length, and Unix mode.
type Shape = (String, u64, u32);

struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    context: Context,
    loadout: PathBuf,
}

impl Machine {
    fn new() -> Self {
        Self::at(tempfile::tempdir().unwrap())
    }

    /// Unix socket paths are limited to about 100 bytes, so socket fixtures get a short root.
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
        let vars = BTreeMap::from([
            ("LOADOUT_HOME", home.as_os_str().to_owned()),
            (
                "LOADOUT_STATE_DIR",
                home.join(".local/state/loadout").into_os_string(),
            ),
        ]);
        let context =
            Context::resolve(Os::current().unwrap(), |name| vars.get(name).cloned(), None).unwrap();
        Self {
            _dir: dir,
            root,
            home,
            context,
            loadout,
        }
    }

    /// Not isolated, with `GROK_HOME` outside home: two distinct Grok configs, the override and
    /// the default `~/.grok`, both legitimate targets.
    fn with_grok_home() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let loadout = home.join("checkout");
        fs::create_dir_all(&loadout).unwrap();
        let vars = BTreeMap::from([
            ("GROK_HOME", root.join("grok-home").into_os_string()),
            (
                "LOADOUT_STATE_DIR",
                home.join(".local/state/loadout").into_os_string(),
            ),
        ]);
        let context = Context::resolve(
            Os::current().unwrap(),
            |name| vars.get(name).cloned(),
            Some(home.clone()),
        )
        .unwrap();
        Self {
            _dir: dir,
            root,
            home,
            context,
            loadout,
        }
    }

    /// Every file, folder, and link under the machine's root, with bytes, link targets, and Unix
    /// modes; the state dir, backups, journals, documents, scratch, and lock files included.
    /// It reads every file, so it must not run while this process holds a lock on one: Windows
    /// refuses to read a file another handle has locked. Root metadata such as mtimes and
    /// Windows attributes is not recorded.
    fn snapshot(&self) -> Snapshot {
        self.snapshot_apart(&[]).0
    }

    /// `snapshot`, with exactly the files `apart` names taken out and inventoried by metadata
    /// alone (type, length, mode; `None` when absent), never read: a lock file a live handle
    /// holds, or a Grok lock file that taking Grok's lock may create. Nothing else is left out,
    /// so another file of the same name elsewhere still counts.
    fn snapshot_apart(&self, apart: &[PathBuf]) -> (Snapshot, BTreeMap<PathBuf, Option<Shape>>) {
        fn walk(dir: &Path, apart: &[PathBuf], out: &mut Snapshot) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if apart.contains(&path) {
                    continue;
                }
                let metadata = fs::symlink_metadata(&path).unwrap();
                let mode = mode_of(&metadata);
                if metadata.file_type().is_symlink() {
                    let target = fs::read_link(&path).unwrap();
                    out.insert(path, ("link".to_owned(), target.as_os_str().as_encoded_bytes().to_vec(), mode));
                } else if metadata.is_dir() {
                    out.insert(path.clone(), ("dir".to_owned(), Vec::new(), mode));
                    walk(&path, apart, out);
                } else if metadata.is_file() {
                    out.insert(path.clone(), ("file".to_owned(), fs::read(&path).unwrap(), mode));
                } else {
                    out.insert(path, ("special".to_owned(), Vec::new(), mode));
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.root, apart, &mut out);
        let inventory = apart
            .iter()
            .map(|path| {
                let shape = fs::symlink_metadata(path).ok().map(|metadata| {
                    let kind = if metadata.file_type().is_symlink() {
                        "link"
                    } else if metadata.is_file() {
                        "file"
                    } else {
                        "other"
                    };
                    (kind.to_owned(), metadata.len(), mode_of(&metadata))
                });
                (path.clone(), shape)
            })
            .collect();
        (out, inventory)
    }

    /// The state lock file a live `Run` holds.
    fn lock_file(&self) -> PathBuf {
        self.context.state.join("lock")
    }

    /// A snapshot taken while a `Run` holds the state lock, which this checks: the lock file is
    /// the one file left out, recorded by metadata, because Windows refuses to read it now.
    /// Compare its bytes with `snapshot` once the run has dropped.
    fn snapshot_while_running(&self) -> (Snapshot, Option<Shape>) {
        assert!(
            state::busy(&self.context).unwrap(),
            "a live run holds the state lock"
        );
        let lock = self.lock_file();
        let (snapshot, mut inventory) = self.snapshot_apart(std::slice::from_ref(&lock));
        (snapshot, inventory.remove(&lock).flatten())
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.home.join(relative)
    }

    fn observe(&self, target: &Target) -> Result<Observed, Error> {
        state::observe(&self.context, &self.loadout, target)
    }

    fn state(&self) -> State {
        State::load(&self.context).unwrap()
    }

    fn begin(&self, actions: Vec<Action>) -> Result<Run, Error> {
        let lock = state::lock(&self.context).unwrap();
        Run::begin(
            &self.context,
            &self.loadout,
            lock,
            &self.state(),
            &Plan { actions },
        )
    }

    /// Plans `value` for `target` the way U4 will, writes it, and finishes; returns the run id.
    fn apply(&self, target: &Target, value: &Value) -> String {
        let action = self.action(target, Some(value));
        let mut run = self.begin(vec![action.clone()]).unwrap();
        let id = run.id().to_owned();
        run.write(&action, action.content.as_ref().unwrap()).unwrap();
        let mut state = self.state();
        run.finish(&mut state).unwrap();
        id
    }

    /// The action U4 would plan: an unowned key equal to the desired value is adopted, and an
    /// unowned key with another value is blocked, never written.
    fn action(&self, target: &Target, value: Option<&Value>) -> Action {
        let prior = self.observe(target).unwrap();
        let state = self.state();
        let desired = value.map(|value| state::digest(&Content::Entry(value.clone())).unwrap());
        let change = match (value, &prior, state.record(target)) {
            (None, _, _) => Change::Remove,
            (Some(_), Observed::Absent, _) => Change::Create,
            (Some(_), Observed::Present { sha256 }, Some(record)) if *sha256 == record.sha256 => {
                Change::Update
            }
            (Some(_), _, Some(_)) => Change::Drifted,
            (Some(_), _, None) if desired.as_ref() == Some(&prior) => Change::Adopt,
            (Some(_), _, None) => Change::Blocked("an unowned server has this name".to_owned()),
        };
        Action {
            item: ItemRef {
                kind: ItemKind::Mcp,
                id: key_of(target).to_owned(),
                agent: Some("agent".to_owned()),
            },
            target: target.clone(),
            change,
            prior,
            content: value.map(|value| Content::Entry(value.clone())),
        }
    }

    fn remove(&self, target: &Target) -> Removal {
        let action = self.action(target, None);
        let mut run = self.begin(vec![action.clone()]).unwrap();
        let removal = run.remove(&action).unwrap();
        let mut state = self.state();
        run.finish(&mut state).unwrap();
        removal
    }

    fn rollback(&self) -> Result<state::RolledBack, Error> {
        state::rollback(&self.context, None)
    }

    fn run_dir(&self, run: &str) -> PathBuf {
        self.context.state.join("runs").join(run)
    }

    fn edit_journal(&self, run: &str, edit: impl FnOnce(&mut Value)) {
        let path = self.run_dir(run).join("journal.json");
        let mut journal: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        edit(&mut journal);
        fs::write(&path, serde_json::to_vec_pretty(&journal).unwrap()).unwrap();
    }

    /// Every byte loadout keeps in its state dir, for proving what never reaches it.
    fn state_bytes(&self) -> Vec<u8> {
        fn walk(dir: &Path, out: &mut Vec<u8>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.extend(fs::read(&path).unwrap());
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.context.state, &mut out);
        out
    }
}

fn entry(file: PathBuf, format: McpFormat, pointer: &str, key: &str, shape: McpShape) -> Target {
    Target::Entry {
        file,
        format,
        pointer: pointer.to_owned(),
        key: key.to_owned(),
        shape,
    }
}

fn cursor(machine: &Machine, key: &str) -> Target {
    let file = machine.path(".cursor/mcp.json");
    entry(file, McpFormat::Json, "/mcpServers", key, McpShape::CursorJson)
}

fn claude(machine: &Machine, key: &str) -> Target {
    let file = machine.path(".claude.json");
    entry(file, McpFormat::Json, "/mcpServers", key, McpShape::ClaudeJson)
}

fn codex(machine: &Machine, key: &str) -> Target {
    let file = machine.path(".codex/config.toml");
    entry(file, McpFormat::Toml, "/mcp_servers", key, McpShape::CodexToml)
}

fn grok(machine: &Machine, key: &str) -> Target {
    let file = machine.path(".grok/config.toml");
    entry(file, McpFormat::Toml, "/mcp_servers", key, McpShape::GrokToml)
}

fn muse(machine: &Machine, key: &str) -> Target {
    let file = machine.path(".config/muse/settings.json");
    entry(file, McpFormat::Json, "/mcp_servers", key, McpShape::MuseJson)
}

fn pi(machine: &Machine, key: &str) -> Target {
    let file = machine.path(".pi/agent/mcp.json");
    entry(file, McpFormat::Json, "/mcpServers", key, McpShape::PiJson)
}

fn opencode(machine: &Machine, key: &str) -> Target {
    let file = machine.path(".config/opencode/opencode.jsonc");
    entry(file, McpFormat::Jsonc, "/mcp", key, McpShape::OpencodeJson)
}

fn parts(target: &Target) -> (&Path, McpFormat, &str, &str) {
    match target {
        Target::Entry {
            file,
            format,
            pointer,
            key,
            ..
        } => (file, *format, pointer, key),
        _ => unreachable!("an entry target"),
    }
}

fn key_of(target: &Target) -> &str {
    parts(target).3
}

fn server(name: &str) -> Value {
    json!({"command": "npx", "args": ["-y", name]})
}

/// The servers the document holds now, as the agent would read them.
fn servers(target: &Target) -> mcp::Servers {
    let (file, format, pointer, _) = parts(target);
    let bytes = fs::read(file).unwrap_or_default();
    mcp::parse(&bytes, format, pointer).unwrap()
}

fn document_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"loadout:document:v1\n");
    hasher.update(u64::try_from(bytes.len()).unwrap().to_be_bytes());
    hasher.update(bytes);
    hasher.finalize().iter().fold(String::new(), |mut out, byte| {
        write!(out, "{byte:02x}").unwrap();
        out
    })
}

fn scratch(target: &Target, run: &str, suffix: &str) -> PathBuf {
    scratch_at(target, run, 0, suffix)
}

/// A scratch name of journal entry `index` in `run`: `.tmp` or `-rollback.tmp` and so on.
fn scratch_at(target: &Target, run: &str, index: usize, suffix: &str) -> PathBuf {
    let (file, ..) = parts(target);
    let name = file.file_name().unwrap().to_str().unwrap();
    file.with_file_name(format!(".{name}.loadout-{run}-{index}{suffix}"))
}

#[cfg(unix)]
fn mode_of(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn mode_of(_: &fs::Metadata) -> u32 {
    0
}

/// One run that writes `values[i]` to `targets[i]` in order, so rollback restores the last
/// first. Every target's change comes from the U4-style helper.
fn run_many(machine: &Machine, targets: &[&Target], values: &[Value]) -> String {
    let actions: Vec<Action> = targets
        .iter()
        .zip(values)
        .map(|(target, value)| machine.action(target, Some(value)))
        .collect();
    let mut run = machine.begin(actions.clone()).unwrap();
    let id = run.id().to_owned();
    for action in &actions {
        run.write(action, action.content.as_ref().unwrap()).unwrap();
    }
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    id
}

/// The bytes rollback will stage to remove `key` from the document as it is now, computed with
/// the same public `mcp::edit` the product uses.
fn without_key(target: &Target) -> Vec<u8> {
    let (file, format, pointer, key) = parts(target);
    let shape = match target {
        Target::Entry { shape, .. } => *shape,
        _ => unreachable!("an entry target"),
    };
    let bytes = fs::read(file).unwrap();
    mcp::edit(&bytes, format, pointer, shape, &mcp::Servers::new(), &[key.to_owned()]).unwrap()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

// 1
#[test]
fn a_key_is_created_in_an_absent_document_for_every_format_and_rolled_back_leaving_it() {
    let machine = Machine::new();
    for target in [
        cursor(&machine, "one"),
        codex(&machine, "one"),
        muse(&machine, "one"),
        opencode(&machine, "one"),
    ] {
        let (file, ..) = parts(&target);
        assert!(!file.exists());
        machine.apply(&target, &server("one"));
        assert_eq!(servers(&target).get("one"), Some(&server("one")));
        let state = machine.state();
        let audit = state::audit(&machine.context, &machine.loadout, &state);
        assert!(audit.iter().all(|(_, health)| *health == Health::Ok));
        machine.rollback().unwrap();
        assert!(!servers(&target).contains_key("one"));
        assert!(file.exists(), "a document the run created is left in place");
    }
    let settings: Value =
        serde_json::from_slice(&fs::read(machine.path(".config/muse/settings.json")).unwrap())
            .unwrap();
    assert_eq!(settings["schema_version"], 1);
}

// 2
#[test]
fn an_update_beside_secrets_never_copies_them_into_the_state_dir() {
    let machine = Machine::new();
    let file = machine.path(".claude.json");
    fs::write(
        &file,
        r#"{"oauthAccount":{"token":"SECRET-OAUTH"},"projects":{"/x":{}},"mcpServers":{"other":{"command":"x","env":{"TOKEN":"SECRET-OTHER"}}}}"#,
    )
    .unwrap();
    let target = claude(&machine, "mine");
    machine.apply(&target, &server("v1"));
    let second = machine.apply(&target, &server("v2"));
    let document: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(document["oauthAccount"]["token"], "SECRET-OAUTH");
    assert_eq!(document["projects"], json!({"/x": {}}));
    assert_eq!(document["mcpServers"]["other"]["env"]["TOKEN"], "SECRET-OTHER");
    assert_eq!(document["mcpServers"]["mine"], server("v2"));
    let kept = machine.state_bytes();
    for secret in [&b"SECRET-OAUTH"[..], b"SECRET-OTHER"] {
        assert!(
            !kept.windows(secret.len()).any(|window| window == secret),
            "{} reached the state dir",
            String::from_utf8_lossy(secret)
        );
    }
    let backup: Value =
        serde_json::from_slice(&fs::read(machine.run_dir(&second).join("backups/0")).unwrap())
            .unwrap();
    assert_eq!(backup, server("v1"), "only the owned key's value is backed up");
}

// 3
#[test]
fn comments_formatting_bom_and_crlf_survive() {
    let machine = Machine::new();
    let jsonc = machine.path(".config/opencode/opencode.jsonc");
    fs::create_dir_all(jsonc.parent().unwrap()).unwrap();
    fs::write(&jsonc, "{\n  // keep me\n  \"theme\": \"dark\",\n  \"mcp\": {}\n}\n").unwrap();
    machine.apply(&opencode(&machine, "one"), &server("one"));
    let text = fs::read_to_string(&jsonc).unwrap();
    assert!(text.contains("// keep me") && text.contains("\"theme\": \"dark\""), "{text}");

    let toml = machine.path(".codex/config.toml");
    fs::create_dir_all(toml.parent().unwrap()).unwrap();
    fs::write(&toml, "# mine\nmodel = \"x\"\n\n[projects.\"/a\"]\ntrust_level = \"trusted\"\n").unwrap();
    machine.apply(&codex(&machine, "one"), &server("one"));
    let text = fs::read_to_string(&toml).unwrap();
    assert!(text.starts_with("# mine\nmodel = \"x\"\n"), "{text}");
    assert!(text.contains("[projects.\"/a\"]\ntrust_level = \"trusted\""), "{text}");

    let json = machine.path(".cursor/mcp.json");
    fs::create_dir_all(json.parent().unwrap()).unwrap();
    fs::write(&json, "\u{feff}{\r\n  \"mcpServers\": {}\r\n}\r\n").unwrap();
    machine.apply(&cursor(&machine, "one"), &server("one"));
    let text = fs::read_to_string(&json).unwrap();
    assert!(text.starts_with('\u{feff}'), "the BOM survives");
    assert!(text.contains("\r\n"), "CRLF survives: {text:?}");
}

// 3, symlinked document
#[cfg(unix)]
#[test]
fn a_symlinked_document_keeps_its_link_and_its_mode() {
    let machine = Machine::new();
    let real = machine.path("dotfiles/mcp.json");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    fs::write(&real, "{\"mcpServers\":{}}\n").unwrap();
    set_mode(&real, 0o640);
    fs::create_dir_all(machine.path(".cursor")).unwrap();
    std::os::unix::fs::symlink(&real, machine.path(".cursor/mcp.json")).unwrap();
    machine.apply(&cursor(&machine, "one"), &server("one"));
    assert!(
        fs::symlink_metadata(machine.path(".cursor/mcp.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::read_to_string(&real).unwrap().contains("\"one\""));
    assert_eq!(mode(&real), 0o640);
}

// 4
#[test]
fn two_keys_in_one_run_roll_back_together_and_an_unrelated_live_edit_is_kept() {
    let machine = Machine::new();
    let a = cursor(&machine, "a");
    let b = cursor(&machine, "b");
    let actions = vec![
        machine.action(&a, Some(&server("a"))),
        machine.action(&b, Some(&server("b"))),
    ];
    let mut run = machine.begin(actions.clone()).unwrap();
    for action in &actions {
        run.write(action, action.content.as_ref().unwrap()).unwrap();
    }
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    assert!(servers(&a).contains_key("a") && servers(&a).contains_key("b"));

    let (file, ..) = parts(&a);
    let mut document: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
    document["mcpServers"]["user"] = json!({"command": "mine"});
    fs::write(file, serde_json::to_vec_pretty(&document).unwrap()).unwrap();

    let rolled = machine.rollback().unwrap();
    assert_eq!(rolled.restored.len(), 2);
    let now = servers(&a);
    assert!(!now.contains_key("a") && !now.contains_key("b"));
    assert_eq!(now.get("user"), Some(&json!({"command": "mine"})));
}

// 5
#[test]
fn an_owned_key_changed_after_the_run_blocks_rollback_and_changes_nothing() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    machine.apply(&target, &server("one"));
    let (file, ..) = parts(&target);
    let mut document: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
    document["mcpServers"]["one"] = json!({"command": "edited"});
    fs::write(file, serde_json::to_vec(&document).unwrap()).unwrap();
    let before = fs::read(file).unwrap();
    let error = machine.rollback().unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert_eq!(fs::read(file).unwrap(), before);
}

// 6
#[test]
fn removal_backs_up_and_removes_an_owned_key_and_releases_a_changed_or_missing_one() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    machine.apply(&target, &server("one"));
    assert_eq!(machine.remove(&target), Removal::Removed);
    assert!(!servers(&target).contains_key("one"));
    machine.rollback().unwrap();
    assert_eq!(servers(&target).get("one"), Some(&server("one")));

    let drifted = cursor(&machine, "two");
    machine.apply(&drifted, &server("two"));
    let (file, ..) = parts(&drifted);
    let mut document: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
    document["mcpServers"]["two"] = json!({"command": "edited"});
    fs::write(file, serde_json::to_vec(&document).unwrap()).unwrap();
    assert_eq!(machine.remove(&drifted), Removal::Released);
    assert_eq!(servers(&drifted).get("two"), Some(&json!({"command": "edited"})));
    assert!(machine.state().record(&drifted).is_none());

    let gone = cursor(&machine, "three");
    machine.apply(&gone, &server("three"));
    let mut document: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
    document["mcpServers"]
        .as_object_mut()
        .unwrap()
        .remove("three");
    fs::write(file, serde_json::to_vec(&document).unwrap()).unwrap();
    assert_eq!(machine.remove(&gone), Removal::Released);
}

// 7
#[test]
fn a_key_changed_after_planning_is_never_written() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let action = machine.action(&target, Some(&server("one")));
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, "{\"mcpServers\":{\"one\":{\"command\":\"theirs\"}}}").unwrap();
    let error = run
        .write(&action, action.content.as_ref().unwrap())
        .unwrap_err();
    assert!(matches!(error, Error::Changed { .. }), "{error}");
    assert_eq!(
        fs::read_to_string(file).unwrap(),
        "{\"mcpServers\":{\"one\":{\"command\":\"theirs\"}}}"
    );
}

// 8
#[cfg(unix)]
#[test]
fn adopting_an_equal_unowned_key_records_it_without_rewriting_the_document() {
    use std::os::unix::fs::MetadataExt;
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        file,
        serde_json::to_vec(&json!({"mcpServers": {"one": server("one")}})).unwrap(),
    )
    .unwrap();
    let before = fs::metadata(file).unwrap();
    let action = machine.action(&target, Some(&server("one")));
    assert_eq!(action.change, Change::Adopt);
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    run.write(&action, action.content.as_ref().unwrap()).unwrap();
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    let after = fs::metadata(file).unwrap();
    assert_eq!((before.ino(), before.mtime()), (after.ino(), after.mtime()));
    assert!(state.record(&target).is_some());
}

// 9
#[test]
fn grok_s_lock_is_honored_on_write_and_never_taken_by_observe() {
    let machine = Machine::new();
    let target = grok(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, "model = \"x\"\n").unwrap();
    let lock_path = machine.path(".grok/.config-init.lock");
    assert!(machine.observe(&target).is_ok());
    assert!(!lock_path.exists(), "observe and plan never create Grok's lock");

    let held = fs::File::create(&lock_path).unwrap();
    held.try_lock().unwrap();
    let action = machine.action(&target, Some(&server("one")));
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    let error = run
        .write(&action, action.content.as_ref().unwrap())
        .unwrap_err();
    assert!(
        matches!(&error, Error::Mcp { source: mcp::Error::Lock { .. }, .. }),
        "{error}"
    );
    assert!(error.is_blocked());
    assert_eq!(fs::read_to_string(file).unwrap(), "model = \"x\"\n");
    drop(held);
}

// 10
#[test]
fn unparseable_documents_are_typed_blocked_errors() {
    let machine = Machine::new();
    let cases: [(Target, &str, fn(&mcp::Error) -> bool); 3] = [
        (
            cursor(&machine, "x"),
            "{\"mcpServers\":{\"a\":{},\"a\":{}}}",
            |error| matches!(error, mcp::Error::Duplicate { .. }),
        ),
        (
            claude(&machine, "x"),
            "{\"mcpServers\": ",
            |error| matches!(error, mcp::Error::Parse { .. }),
        ),
        (
            codex(&machine, "x"),
            "[mcp_servers.a]\nwhen = 1979-05-27T07:32:00Z\n",
            |error| matches!(error, mcp::Error::Value { .. }),
        ),
    ];
    for (target, text, kind) in cases {
        let (file, ..) = parts(&target);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
        let error = machine.observe(&target).unwrap_err();
        assert!(
            matches!(&error, Error::Mcp { source, .. } if kind(source)),
            "{error}"
        );
        assert!(error.is_blocked(), "{error}");
        assert_eq!(fs::read_to_string(file).unwrap(), text);
    }
}

// 10, edit-time refusals
#[test]
fn documents_loadout_will_not_edit_are_refused_without_a_write() {
    let machine = Machine::new();
    let cases: [(Target, &str, fn(&mcp::Error) -> bool); 3] = [
        (
            codex(&machine, "x"),
            "[mcp_servers.a]\r\ncommand = \"a\"\nargs = []\r\n",
            |error| matches!(error, mcp::Error::MixedNewlines),
        ),
        (
            muse(&machine, "x"),
            "{\"schema_version\":2,\"mcp_servers\":{}}",
            |error| matches!(error, mcp::Error::Schema { .. }),
        ),
        (
            pi(&machine, "my-srv"),
            "{\"mcpServers\":{\"my_srv\":{\"command\":\"x\"}}}",
            |error| matches!(error, mcp::Error::Collision { .. }),
        ),
    ];
    for (target, text, kind) in cases {
        let (file, ..) = parts(&target);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
        let action = machine.action(&target, Some(&server("x")));
        let mut run = machine.begin(vec![action.clone()]).unwrap();
        let error = run
            .write(&action, action.content.as_ref().unwrap())
            .unwrap_err();
        assert!(
            matches!(&error, Error::Mcp { source, .. } if kind(source)),
            "{error}"
        );
        assert!(error.is_blocked(), "{error}");
        assert_eq!(fs::read_to_string(file).unwrap(), text);
    }
}

// 10, contract faults
#[test]
fn a_shape_written_in_another_format_is_refused_at_begin() {
    let machine = Machine::new();
    let target = entry(
        machine.path(".cursor/mcp.json"),
        McpFormat::Json,
        "/mcpServers",
        "x",
        McpShape::CodexToml,
    );
    let action = Action {
        item: ItemRef {
            kind: ItemKind::Mcp,
            id: "x".to_owned(),
            agent: None,
        },
        target,
        change: Change::Create,
        prior: Observed::Absent,
        content: Some(Content::Entry(server("x"))),
    };
    let error = machine.begin(vec![action]).unwrap_err();
    assert!(matches!(error, Error::InvalidContent { .. }), "{error}");
    assert!(!error.is_blocked());
}

// 11
#[test]
fn an_agent_owned_map_is_reserved_and_never_read() {
    let machine = Machine::new();
    let file = machine.path(".claude.json");
    fs::write(&file, r#"{"oauthAccount":{"token":"SECRET"}}"#).unwrap();
    let oauth = entry(
        file.clone(),
        McpFormat::Json,
        "/oauthAccount",
        "token",
        McpShape::ClaudeJson,
    );
    let projects = entry(
        machine.path(".codex/config.toml"),
        McpFormat::Toml,
        "/projects",
        "x",
        McpShape::CodexToml,
    );
    for target in [oauth, projects] {
        let error = machine.observe(&target).unwrap_err();
        assert!(matches!(error, Error::Reserved { .. }), "{error}");
    }
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        r#"{"oauthAccount":{"token":"SECRET"}}"#
    );
}

// 12
#[test]
fn a_tampered_key_backup_is_never_restored() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    machine.apply(&target, &server("v1"));
    let second = machine.apply(&target, &server("v2"));
    fs::write(machine.run_dir(&second).join("backups/0"), "{\"command\":\"evil\"}").unwrap();
    let (file, ..) = parts(&target);
    let before = fs::read(file).unwrap();
    let error = machine.rollback().unwrap_err();
    assert!(matches!(error, Error::BackupCorrupt { .. }), "{error}");
    assert_eq!(fs::read(file).unwrap(), before);
}

// 12
#[test]
fn malformed_entry_journals_and_records_are_invalid() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    machine.apply(&target, &server("v1"));
    let second = machine.apply(&target, &server("v2"));
    machine.edit_journal(&second, |journal| {
        journal["entries"][0]["backup"]["modes"] = json!({"": 420});
    });
    assert!(matches!(machine.rollback(), Err(Error::Invalid { .. })));
    machine.edit_journal(&second, |journal| {
        journal["entries"][0]["backup"]["modes"] = json!({});
    });

    let path = machine.context.state.join("state.json");
    let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let edits: [fn(&mut Value); 4] = [
        |file| {
            file["records"][0]["target"]
                .as_object_mut()
                .unwrap()
                .remove("shape");
        },
        |file| file["records"][0]["target"]["pointer"] = json!("mcpServers"),
        |file| file["records"][0]["target"]["shape"] = json!("codex-toml"),
        |file| file["records"][0]["target"] = json!({"type": "none"}),
    ];
    for edit in edits {
        let mut file = original.clone();
        edit(&mut file);
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        assert!(
            matches!(State::load(&machine.context), Err(Error::Invalid { .. })),
            "{file}"
        );
    }
    fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
}

// 12, None
#[test]
fn a_target_with_no_destination_is_refused_before_any_io() {
    let machine = Machine::new();
    assert!(matches!(
        machine.observe(&Target::None),
        Err(Error::NoTarget { .. })
    ));
    let action = Action {
        item: ItemRef {
            kind: ItemKind::Instructions,
            id: "x".to_owned(),
            agent: Some("cursor".to_owned()),
        },
        target: Target::None,
        change: Change::Create,
        prior: Observed::Absent,
        content: Some(Content::Entry(server("x"))),
    };
    assert!(matches!(
        machine.begin(vec![action]),
        Err(Error::NoTarget { .. })
    ));
    assert!(!machine.context.state.join("runs").exists());
}

/// A finished run whose journal is rewritten to look stopped right after its staged document
/// landed: step `started`, and a file on the run's `.tmp` scratch name built from the bytes that
/// landed (which are exactly the bytes the run staged and journaled).
fn stopped_after_staging(
    machine: &Machine,
    target: &Target,
    tmp_bytes: impl FnOnce(&[u8]) -> Vec<u8>,
) -> (String, PathBuf) {
    let run = machine.apply(target, &server("one"));
    machine.edit_journal(&run, |journal| {
        journal["status"] = json!("running");
        journal["entries"][0]["step"] = json!("started");
    });
    let landed = fs::read(parts(target).0).unwrap();
    let tmp = scratch(target, &run, ".tmp");
    fs::write(&tmp, tmp_bytes(&landed)).unwrap();
    (run, tmp)
}

// 13
#[test]
fn the_run_s_own_staged_document_is_cleaned_up_by_rollback() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let (_, tmp) = stopped_after_staging(&machine, &target, <[u8]>::to_vec);
    let rolled = machine.rollback().unwrap();
    assert!(!tmp.exists(), "the exact staged bytes are the run's own");
    assert!(rolled.leftovers.is_empty());
    assert!(!servers(&target).contains_key("one"));
}

// 20
#[test]
fn a_foreign_document_holding_the_same_key_is_never_taken_for_the_run_s_scratch() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let foreign =
        serde_json::to_vec(&json!({"mcpServers": {"one": server("one")}, "user": 1})).unwrap();
    let (_, tmp) = stopped_after_staging(&machine, &target, |_| foreign.clone());
    let rolled = machine.rollback().unwrap();
    assert_eq!(rolled.leftovers, vec![tmp.clone()]);
    assert_eq!(fs::read(&tmp).unwrap(), foreign, "kept, not deleted");
    assert!(!servers(&target).contains_key("one"));
}

/// Occupants that are never rollback's own staging file.
enum Occupant {
    SameKeyDocument,
    Folder,
    #[cfg(unix)]
    Link,
    #[cfg(unix)]
    Socket,
}

// 20
#[test]
fn a_foreign_occupant_of_a_s_rollback_staging_name_blocks_b_too() {
    let mut occupants = vec![Occupant::SameKeyDocument, Occupant::Folder];
    #[cfg(unix)]
    occupants.extend([Occupant::Link, Occupant::Socket]);
    for occupant in occupants {
        #[cfg(unix)]
        let machine = Machine::short();
        #[cfg(not(unix))]
        let machine = Machine::new();
        let a = cursor(&machine, "a");
        let b = claude(&machine, "b");
        let run = run_many(&machine, &[&a, &b], &[server("a"), server("b")]);
        let retmp = scratch_at(&a, &run, 0, "-rollback.tmp");
        match occupant {
            Occupant::SameKeyDocument => fs::write(
                &retmp,
                serde_json::to_vec(&json!({"mcpServers": {"a": server("a")}})).unwrap(),
            )
            .unwrap(),
            Occupant::Folder => fs::create_dir(&retmp).unwrap(),
            #[cfg(unix)]
            Occupant::Link => {
                std::os::unix::fs::symlink(parts(&a).0, &retmp).unwrap();
            }
            #[cfg(unix)]
            Occupant::Socket => {
                drop(std::os::unix::net::UnixListener::bind(&retmp).unwrap());
            }
        }
        let before = machine.snapshot();
        let error = machine.rollback().unwrap_err();
        assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
        assert!(error.is_blocked());
        assert_eq!(machine.snapshot(), before, "B is restored first, yet nothing changed");
    }
}

// 21
#[test]
fn a_rollback_that_stopped_with_its_complete_restore_staged_finishes_on_the_next_try() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let run = machine.apply(&target, &server("one"));
    let (file, ..) = parts(&target);
    let current = fs::read(file).unwrap();
    let planned = without_key(&target);
    fs::write(scratch(&target, &run, "-rollback.tmp"), &planned).unwrap();
    machine.edit_journal(&run, |journal| {
        journal["entries"][0]["rollback"] =
            json!({"base": document_hash(&current), "staged": document_hash(&planned)});
    });
    machine.rollback().unwrap();
    assert!(!scratch(&target, &run, "-rollback.tmp").exists());
    assert_eq!(fs::read(file).unwrap(), planned);
}

// 21
#[test]
fn a_rollback_intent_journaled_before_staging_needs_no_scratch() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let run = machine.apply(&target, &server("one"));
    let current = fs::read(parts(&target).0).unwrap();
    let planned = without_key(&target);
    machine.edit_journal(&run, |journal| {
        journal["entries"][0]["rollback"] =
            json!({"base": document_hash(&current), "staged": document_hash(&planned)});
    });
    machine.rollback().unwrap();
    assert!(!servers(&target).contains_key("one"));
}

// 21
#[test]
fn a_restore_that_already_renamed_is_kept_on_the_next_try() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let run = machine.apply(&target, &server("one"));
    let (file, ..) = parts(&target);
    let current = fs::read(file).unwrap();
    let planned = without_key(&target);
    fs::write(file, &planned).unwrap();
    machine.edit_journal(&run, |journal| {
        journal["entries"][0]["rollback"] =
            json!({"base": document_hash(&current), "staged": document_hash(&planned)});
    });
    let rolled = machine.rollback().unwrap();
    assert!(rolled.restored.is_empty(), "the key already holds its prior");
    assert_eq!(fs::read(file).unwrap(), planned);
}

// 21
#[test]
fn a_multi_key_rollback_that_stopped_halfway_finishes_the_rest() {
    let machine = Machine::new();
    let a = cursor(&machine, "a");
    let b = cursor(&machine, "b");
    let run = run_many(&machine, &[&a, &b], &[server("a"), server("b")]);
    let (file, ..) = parts(&b);
    let current = fs::read(file).unwrap();
    let planned = without_key(&b);
    fs::write(file, &planned).unwrap();
    machine.edit_journal(&run, |journal| {
        journal["entries"][1]["rollback"] =
            json!({"base": document_hash(&current), "staged": document_hash(&planned)});
    });
    let rolled = machine.rollback().unwrap();
    assert_eq!(rolled.restored, vec![a.clone()]);
    let now = servers(&a);
    assert!(!now.contains_key("a") && !now.contains_key("b"));
}

// 21
#[test]
fn a_partial_or_unrecorded_rollback_staging_file_is_never_taken_for_the_restore() {
    for recorded in [true, false] {
        let machine = Machine::new();
        let target = cursor(&machine, "one");
        let run = machine.apply(&target, &server("one"));
        let current = fs::read(parts(&target).0).unwrap();
        let planned = without_key(&target);
        let partial = &planned[..planned.len() / 2];
        let retmp = scratch(&target, &run, "-rollback.tmp");
        fs::write(&retmp, partial).unwrap();
        if recorded {
            machine.edit_journal(&run, |journal| {
                journal["entries"][0]["rollback"] =
                    json!({"base": document_hash(&current), "staged": document_hash(&planned)});
            });
        }
        let before = machine.snapshot();
        let error = machine.rollback().unwrap_err();
        assert!(matches!(error, Error::RollbackBlocked { .. }), "{recorded}: {error}");
        assert_eq!(machine.snapshot(), before, "partial bytes kept, nothing restored");
    }
}

// 22
#[test]
fn staged_document_hashes_are_validated() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let run = machine.apply(&target, &server("one"));
    let edits: [fn(&mut Value); 3] = [
        |journal| journal["entries"][0]["document"]["staged"] = json!("not-a-hash"),
        |journal| {
            journal["entries"][0]["step"] = json!("started");
            journal["entries"][0]
                .as_object_mut()
                .unwrap()
                .remove("document");
        },
        |journal| journal["entries"][0]["rollback"] = json!({"base": "x", "staged": "y"}),
    ];
    let path = machine.run_dir(&run).join("journal.json");
    let original = fs::read(&path).unwrap();
    for edit in edits {
        fs::write(&path, &original).unwrap();
        machine.edit_journal(&run, edit);
        assert!(matches!(machine.rollback(), Err(Error::Invalid { .. })));
    }

    let path_target = Target::Path {
        path: machine.path(".gitconfig"),
    };
    let content = Content::File(loadout::plan::FileContent {
        bytes: b"x".to_vec(),
        executable: false,
    });
    let action = Action {
        item: ItemRef {
            kind: ItemKind::File,
            id: "gitconfig".to_owned(),
            agent: None,
        },
        target: path_target,
        change: Change::Create,
        prior: Observed::Absent,
        content: Some(content.clone()),
    };
    let mut path_run = machine.begin(vec![action.clone()]).unwrap();
    let id = path_run.id().to_owned();
    path_run.write(&action, &content).unwrap();
    let mut state = machine.state();
    path_run.finish(&mut state).unwrap();
    machine.edit_journal(&id, |journal| {
        journal["entries"][0]["document"] = json!({"base": "1".repeat(64), "staged": "2".repeat(64)});
    });
    assert!(matches!(machine.rollback(), Err(Error::Invalid { .. })));
}

// 14
#[test]
fn replacing_an_unowned_key_is_refused_before_anything_is_journaled() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, "{\"mcpServers\":{\"one\":{\"command\":\"theirs\"}}}").unwrap();
    let mut action = machine.action(&target, Some(&server("one")));
    action.change = Change::Replace;
    let error = machine.begin(vec![action]).unwrap_err();
    assert!(matches!(error, Error::WrongChange { .. }), "{error}");
    assert!(!error.is_blocked(), "a literal Replace is a contract fault, exit 1");
    assert!(!machine.context.state.join("runs").exists());
}

// 17
#[cfg(unix)]
#[test]
fn a_folder_in_the_document_s_place_is_never_listed_or_read() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file).unwrap();
    fs::write(file.join("marker"), b"untouched").unwrap();
    set_mode(file, 0o300);
    let observed = machine.observe(&target);
    let action = Action {
        item: ItemRef {
            kind: ItemKind::Mcp,
            id: "one".to_owned(),
            agent: None,
        },
        target: target.clone(),
        change: Change::Create,
        prior: Observed::Absent,
        content: Some(Content::Entry(server("one"))),
    };
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    let written = run.write(&action, action.content.as_ref().unwrap());
    set_mode(file, 0o700);
    for result in [observed.map(|_| ()), written] {
        let error = result.unwrap_err();
        assert!(
            matches!(error, Error::NotADocument { .. }),
            "listing the unreadable folder would have been an I/O error: {error}"
        );
        assert!(error.is_blocked());
    }
    assert_eq!(fs::read(file.join("marker")).unwrap(), b"untouched");
}

// 17, rollback preflight
#[test]
fn a_document_replaced_by_a_folder_blocks_rollback() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    machine.apply(&target, &server("one"));
    let (file, ..) = parts(&target);
    fs::remove_file(file).unwrap();
    fs::create_dir(file).unwrap();
    fs::write(file.join("marker"), b"untouched").unwrap();
    let error = machine.rollback().unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert_eq!(fs::read(file.join("marker")).unwrap(), b"untouched");
}

// 18
#[cfg(unix)]
#[test]
fn a_socket_in_the_document_s_place_is_refused_without_opening_it() {
    let machine = Machine::short();
    let target = cursor(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let _socket = std::os::unix::net::UnixListener::bind(file).unwrap();
    assert!(matches!(
        machine.observe(&target),
        Err(Error::Special { .. })
    ));
}

// 23
#[cfg(unix)]
#[test]
fn a_new_shared_document_is_private_and_an_existing_one_keeps_its_mode() {
    let machine = Machine::new();
    let fresh = cursor(&machine, "one");
    machine.apply(&fresh, &server("one"));
    assert_eq!(mode(parts(&fresh).0), 0o600);

    for existing in [0o644, 0o600] {
        let target = claude(&machine, "one");
        let (file, ..) = parts(&target);
        let _ = fs::remove_file(file);
        fs::write(file, "{}").unwrap();
        set_mode(file, existing);
        let action = machine.action(&target, Some(&server("one")));
        let mut run = machine.begin(vec![action.clone()]).unwrap();
        run.write(&action, action.content.as_ref().unwrap()).unwrap();
        let mut state = machine.state();
        run.finish(&mut state).unwrap();
        assert_eq!(mode(file), existing);
        machine.rollback().unwrap();
    }
}

/// Two owned keys, A then B, each updated from v1 to v2 in one run, so rollback must `SetKey`
/// both, B first.
fn owned_updates(machine: &Machine, a: &Target, b: &Target) -> String {
    run_many(machine, &[a, b], &[server("a1"), server("b1")]);
    run_many(machine, &[a, b], &[server("a2"), server("b2")])
}

fn set_json(target: &Target, edit: impl FnOnce(&mut Value)) {
    let (file, ..) = parts(target);
    let mut document: Value = serde_json::from_slice(&fs::read(file).unwrap()).unwrap();
    edit(&mut document);
    fs::write(file, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
}

// E1 (groups 4, 10)
#[test]
fn an_uneditable_document_blocks_rollback_before_b_is_restored() {
    let breaks: [(&str, fn(&Machine) -> Target, fn(&Target)); 4] = [
        ("muse schema 2, SetKey", |machine| muse(machine, "a"), |a| {
            set_json(a, |document| document["schema_version"] = json!(2));
        }),
        ("pi collision, SetKey", |machine| pi(machine, "my-srv"), |a| {
            set_json(a, |document| {
                document["mcpServers"]["my_srv"] = json!({"command": "theirs"});
            });
        }),
        ("map no longer an object, refused on read", |machine| cursor(machine, "a"), |a| {
            set_json(a, |document| document["mcpServers"] = json!("gone"));
        }),
        ("mixed newlines, SetKey", |machine| codex(machine, "a"), |a| {
            let (file, ..) = parts(a);
            let mut text = fs::read_to_string(file).unwrap();
            text.push_str("# added\r\n");
            fs::write(file, text).unwrap();
        }),
    ];
    for (case, make, break_a) in breaks {
        let machine = Machine::new();
        let a = make(&machine);
        let b = claude(&machine, "b");
        owned_updates(&machine, &a, &b);
        break_a(&a);
        let before = machine.snapshot();
        let error = machine.rollback().unwrap_err();
        assert!(matches!(error, Error::RollbackBlocked { .. }), "{case}: {error}");
        assert_eq!(machine.snapshot(), before, "{case}: B is restored first, yet nothing changed");
    }
}

// E1, RemoveKey
#[test]
fn an_uneditable_document_blocks_a_key_removal_rollback_before_b_is_restored() {
    let machine = Machine::new();
    let a = codex(&machine, "a");
    let b = claude(&machine, "b");
    run_many(&machine, &[&a, &b], &[server("a"), server("b")]);
    let (file, ..) = parts(&a);
    let mut text = fs::read_to_string(file).unwrap();
    text.push_str("# added\r\n");
    fs::write(file, text).unwrap();
    let before = machine.snapshot();
    let error = machine.rollback().unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert_eq!(machine.snapshot(), before);
}

// E1 positives
#[test]
fn editable_documents_restore_including_muse_at_schema_1() {
    let machine = Machine::new();
    let a = muse(&machine, "a");
    let b = claude(&machine, "b");
    owned_updates(&machine, &a, &b);
    set_json(&b, |document| document["theme"] = json!("unrelated, edited after the run"));
    machine.rollback().unwrap();
    assert_eq!(servers(&a).get("a"), Some(&server("a1")));
    assert_eq!(servers(&b).get("b"), Some(&server("b1")));
    let document: Value = serde_json::from_slice(&fs::read(parts(&b).0).unwrap()).unwrap();
    assert_eq!(document["theme"], "unrelated, edited after the run");
}

// E2 (groups 8, 14)
#[test]
fn an_unowned_key_with_another_value_is_never_written_whatever_the_label() {
    for label in [Change::Adopt, Change::Update, Change::Drifted, Change::Create] {
        let machine = Machine::new();
        let target = cursor(&machine, "one");
        let (file, ..) = parts(&target);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "{\"mcpServers\":{\"one\":{\"command\":\"theirs\"}}}").unwrap();
        assert!(matches!(
            machine.action(&target, Some(&server("one"))).change,
            Change::Blocked(_)
        ));
        let action = Action {
            item: ItemRef {
                kind: ItemKind::Mcp,
                id: "one".to_owned(),
                agent: None,
            },
            target: target.clone(),
            change: label.clone(),
            prior: machine.observe(&target).unwrap(),
            content: Some(Content::Entry(server("one"))),
        };
        // The inactive state lock file, before any run holds it.
        drop(state::lock(&machine.context).unwrap());
        let idle = machine.snapshot();
        let mut run = machine.begin(vec![action.clone()]).unwrap();
        let id = run.id().to_owned();
        // From here until the run drops it holds the state lock, so the lock file is observed
        // by metadata only.
        let (before, held) = machine.snapshot_while_running();
        let error = run
            .write(&action, action.content.as_ref().unwrap())
            .unwrap_err();
        assert!(matches!(error, Error::UnownedEntry { .. }), "{label:?}: {error}");
        assert!(error.is_blocked());
        let (after, still_held) = machine.snapshot_while_running();
        assert_eq!(after, before, "{label:?}: no backup, no started, no edit");
        assert_eq!(still_held, held);
        let journal: Value = serde_json::from_slice(
            &fs::read(machine.run_dir(&id).join("journal.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(journal["entries"][0]["step"], "pending");
        assert_eq!(journal["entries"][0]["backup"]["kind"], "pending");
        assert!(journal["entries"][0].get("document").is_none());
        assert_eq!(fs::read_dir(machine.run_dir(&id).join("backups")).unwrap().count(), 0);
        drop(run);
        let mut released = machine.snapshot();
        let lock = machine.lock_file();
        assert_eq!(
            released.remove(&lock),
            idle.get(&lock).cloned(),
            "{label:?}: the inactive lock file's bytes and mode"
        );
        assert_eq!(released, before, "{label:?}: nothing else changed once the run dropped");
    }
}

// E2 positives
#[test]
fn a_forced_update_of_an_owned_drifted_key_backs_up_the_drift_and_rolls_back_to_it() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    machine.apply(&target, &server("v1"));
    set_json(&target, |document| {
        document["mcpServers"]["one"] = json!({"command": "drifted"});
    });
    let action = machine.action(&target, Some(&server("v2")));
    assert_eq!(action.change, Change::Drifted);
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    let id = run.id().to_owned();
    run.write(&action, action.content.as_ref().unwrap()).unwrap();
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    let backup: Value =
        serde_json::from_slice(&fs::read(machine.run_dir(&id).join("backups/0")).unwrap()).unwrap();
    assert_eq!(backup, json!({"command": "drifted"}));
    machine.rollback().unwrap();
    assert_eq!(servers(&target).get("one"), Some(&json!({"command": "drifted"})));
}

// E2, removal of an unowned key
#[test]
fn removing_a_key_loadout_does_not_own_is_refused() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, "{\"mcpServers\":{\"one\":{\"command\":\"theirs\"}}}").unwrap();
    let action = machine.action(&target, None);
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    assert!(matches!(run.remove(&action), Err(Error::NotOwned { .. })));
    assert_eq!(
        fs::read_to_string(file).unwrap(),
        "{\"mcpServers\":{\"one\":{\"command\":\"theirs\"}}}"
    );
}

// 8, portable
#[test]
fn an_equal_adopt_writes_nothing_and_backs_up_nothing() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        file,
        serde_json::to_vec(&json!({"mcpServers": {"one": server("one")}})).unwrap(),
    )
    .unwrap();
    let action = machine.action(&target, Some(&server("one")));
    assert_eq!(action.change, Change::Adopt);
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    let id = run.id().to_owned();
    let before = fs::metadata(file).unwrap().modified().unwrap();
    let bytes = fs::read(file).unwrap();
    run.write(&action, action.content.as_ref().unwrap()).unwrap();
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    assert_eq!(fs::read(file).unwrap(), bytes);
    assert_eq!(fs::metadata(file).unwrap().modified().unwrap(), before);
    assert_eq!(fs::read_dir(machine.run_dir(&id).join("backups")).unwrap().count(), 0);
    assert!(state.record(&target).is_some());
    machine.rollback().unwrap();
    assert_eq!(fs::read(file).unwrap(), bytes, "rollback keeps an adopted key");
}

// E3 (groups 12, 22)
#[test]
fn entry_journals_must_match_what_actually_happened() {
    let edits: [(&str, fn(&mut Value)); 8] = [
        ("written done without its document", |journal| {
            journal["entries"][0].as_object_mut().unwrap().remove("document");
        }),
        ("failed without its document", |journal| {
            journal["entries"][0]["step"] = json!("failed");
            journal["entries"][0].as_object_mut().unwrap().remove("document");
        }),
        ("a written replace with a pending backup", |journal| {
            journal["entries"][0]["backup"]["kind"] = json!("pending");
        }),
        ("a file backup for a key", |journal| {
            journal["entries"][0]["backup"]["kind"] = json!("file");
            journal["entries"][0]["backup"]["modes"] = json!({"": 384});
        }),
        ("another entry's slot", |journal| {
            journal["entries"][0]["backup"]["path"] = json!("backups/1");
        }),
        ("another run's slot", |journal| {
            let run = journal["run"].as_str().unwrap().to_owned();
            journal["entries"][0]["backup"]["path"] = json!(format!("../{run}x/backups/0"));
        }),
        ("a bad base hash", |journal| {
            journal["entries"][0]["document"]["base"] = json!("x");
        }),
        ("a target without its shape", |journal| {
            journal["entries"][0]["target"]
                .as_object_mut()
                .unwrap()
                .remove("shape");
        }),
    ];
    for (case, edit) in edits {
        let machine = Machine::new();
        let target = cursor(&machine, "one");
        machine.apply(&target, &server("v1"));
        let run = machine.apply(&target, &server("v2"));
        machine.edit_journal(&run, edit);
        let before = machine.snapshot();
        assert!(
            matches!(machine.rollback(), Err(Error::Invalid { .. })),
            "{case}"
        );
        assert_eq!(machine.snapshot(), before, "{case}");
    }
}

// E3 positives
#[test]
fn a_true_no_op_and_a_release_journal_validly_and_roll_back() {
    let machine = Machine::new();
    let adopted = cursor(&machine, "kept");
    let (file, ..) = parts(&adopted);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(
        file,
        serde_json::to_vec(&json!({"mcpServers": {"kept": server("kept")}})).unwrap(),
    )
    .unwrap();
    let run = machine.apply(&adopted, &server("kept"));
    let journal: Value =
        serde_json::from_slice(&fs::read(machine.run_dir(&run).join("journal.json")).unwrap())
            .unwrap();
    assert_eq!(journal["entries"][0]["backup"]["kind"], "pending");
    assert!(journal["entries"][0].get("document").is_none());
    machine.rollback().unwrap();

    let released = cursor(&machine, "two");
    machine.apply(&released, &server("two"));
    set_json(&released, |document| {
        document["mcpServers"]["two"] = json!({"command": "edited"});
    });
    assert_eq!(machine.remove(&released), Removal::Released);
    machine.rollback().unwrap();
    assert_eq!(servers(&released).get("two"), Some(&json!({"command": "edited"})));
}

// E3, a linked backup
#[cfg(unix)]
#[test]
fn a_key_backup_replaced_by_a_link_is_never_restored() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    machine.apply(&target, &server("v1"));
    let run = machine.apply(&target, &server("v2"));
    let backup = machine.run_dir(&run).join("backups/0");
    let elsewhere = machine.path("elsewhere.json");
    fs::write(&elsewhere, serde_json::to_vec(&server("v1")).unwrap()).unwrap();
    fs::remove_file(&backup).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &backup).unwrap();
    let before = machine.snapshot();
    assert!(matches!(machine.rollback(), Err(Error::Invalid { .. })));
    assert_eq!(machine.snapshot(), before);
}

// 12, records
#[test]
fn an_entry_record_with_an_empty_key_is_invalid() {
    let machine = Machine::new();
    machine.apply(&cursor(&machine, "one"), &server("one"));
    let path = machine.context.state.join("state.json");
    let mut file: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    file["records"][0]["target"]["key"] = json!("");
    fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    assert!(matches!(
        State::load(&machine.context),
        Err(Error::Invalid { .. })
    ));
}

// 17, remove
#[test]
fn removing_a_key_whose_document_became_a_folder_reads_nothing() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    machine.apply(&target, &server("one"));
    let action = machine.action(&target, None);
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    let (file, ..) = parts(&target);
    fs::remove_file(file).unwrap();
    fs::create_dir(file).unwrap();
    fs::write(file.join("marker"), b"untouched").unwrap();
    let error = run.remove(&action).unwrap_err();
    assert!(matches!(error, Error::NotADocument { .. }), "{error}");
    assert!(error.is_blocked());
    assert_eq!(fs::read(file.join("marker")).unwrap(), b"untouched");
}

// 23, contrast with ordinary files
#[cfg(unix)]
#[test]
fn only_a_new_shared_mcp_document_is_forced_private() {
    let machine = Machine::new();
    let probe = machine.path("probe");
    fs::write(&probe, "").unwrap();
    let default = mode(&probe);
    let path = machine.path("notes.txt");
    let content = Content::File(loadout::plan::FileContent {
        bytes: b"x".to_vec(),
        executable: false,
    });
    let action = Action {
        item: ItemRef {
            kind: ItemKind::File,
            id: "notes".to_owned(),
            agent: None,
        },
        target: Target::Path { path: path.clone() },
        change: Change::Create,
        prior: Observed::Absent,
        content: Some(content.clone()),
    };
    let mut run = machine.begin(vec![action.clone()]).unwrap();
    run.write(&action, &content).unwrap();
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    assert_eq!(mode(&path), default, "an ordinary new file keeps the process default");
    let fresh = cursor(&machine, "one");
    machine.apply(&fresh, &server("one"));
    assert_eq!(mode(parts(&fresh).0), 0o600);
}

fn grok_at(home: &Path, key: &str) -> Target {
    entry(
        home.join("config.toml"),
        McpFormat::Toml,
        "/mcp_servers",
        key,
        McpShape::GrokToml,
    )
}

fn hold(lock: &Path) -> fs::File {
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock)
        .unwrap();
    file.try_lock().unwrap();
    file
}

// E4 (group 9)
#[test]
fn every_grok_lock_is_taken_before_rollback_changes_anything() {
    let machine = Machine::with_grok_home();
    let default_home = machine.path(".grok");
    let override_home = machine.root.join("grok-home");
    for home in [&default_home, &override_home] {
        fs::create_dir_all(home).unwrap();
        fs::write(home.join("config.toml"), "model = \"x\"\n").unwrap();
    }
    let a = grok_at(&override_home, "a");
    let b = grok_at(&default_home, "b");
    let c = cursor(&machine, "c");
    run_many(&machine, &[&a, &b, &c], &[server("a"), server("b"), server("c")]);
    for contended in [&default_home, &override_home] {
        let other = if contended == &default_home {
            &override_home
        } else {
            &default_home
        };
        let held = hold(&contended.join(".config-init.lock"));
        // The two selected lock files are inventoried apart, by metadata: one is held, so
        // Windows won't let it be read. The forward run already created both.
        let locks = [
            default_home.join(".config-init.lock"),
            override_home.join(".config-init.lock"),
        ];
        let (before, locks_before) = machine.snapshot_apart(&locks);
        let error = machine.rollback().unwrap_err();
        assert!(
            matches!(&error, Error::Mcp { source: mcp::Error::Lock { .. }, .. }),
            "{error}"
        );
        assert!(error.is_blocked());
        let (after, locks_after) = machine.snapshot_apart(&locks);
        assert_eq!(after, before, "nothing changed while a lock was held");
        assert_eq!(locks_after, locks_before, "both lock files existed, empty, and still do");
        assert!(
            locks_after
                .values()
                .all(|shape| shape.as_ref().is_some_and(|(kind, len, _)| kind == "file" && *len == 0))
        );
        let freed = hold(&other.join(".config-init.lock"));
        drop(freed);
        drop(held);
    }
    machine.rollback().unwrap();
    for target in [&a, &b, &c] {
        assert!(!servers(target).contains_key(key_of(target)));
    }
}

// E4, aliases
#[cfg(unix)]
#[test]
fn aliases_of_one_grok_config_share_one_lock_without_deadlock() {
    let machine = Machine::new();
    let home = machine.path(".grok");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("config.toml"), "model = \"x\"\n").unwrap();
    let alias = machine.path("grok-alias");
    std::os::unix::fs::symlink(&home, &alias).unwrap();
    let canonical = grok_at(&home, "a");
    let aliased = grok_at(&alias, "b");
    let ids = [
        mcp::lock_id(&machine.context, &machine.loadout, parts(&canonical).0).unwrap(),
        mcp::lock_id(&machine.context, &machine.loadout, parts(&aliased).0).unwrap(),
    ];
    assert!(ids[0].is_some());
    assert_eq!(ids[0], ids[1], "an alias names the same lock");
    assert!(
        mcp::lock_id(&machine.context, &machine.loadout, parts(&cursor(&machine, "x")).0)
            .unwrap()
            .is_none()
    );
    run_many(&machine, &[&canonical, &aliased], &[server("a"), server("b")]);
    machine.rollback().unwrap();
    let now = servers(&canonical);
    assert!(!now.contains_key("a") && !now.contains_key("b"));
}

// E4, read-only paths take no lock
#[test]
fn audit_reads_a_grok_key_while_grok_holds_its_lock() {
    let machine = Machine::new();
    let target = grok(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, "model = \"x\"\n").unwrap();
    machine.apply(&target, &server("one"));
    let held = hold(&machine.path(".grok/.config-init.lock"));
    let state = machine.state();
    let audit = state::audit(&machine.context, &machine.loadout, &state);
    assert_eq!(audit[0].1, Health::Ok);
    drop(held);
}

// 5, two targets in one run
#[test]
fn one_changed_key_blocks_the_whole_run_s_rollback_and_changes_nothing() {
    let machine = Machine::new();
    let a = cursor(&machine, "a");
    let b = claude(&machine, "b");
    run_many(&machine, &[&a, &b], &[server("a"), server("b")]);
    set_json(&a, |document| {
        document["mcpServers"]["a"] = json!({"command": "edited"});
    });
    let before = machine.snapshot();
    let error = machine.rollback().unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert_eq!(machine.snapshot(), before, "B is restored first, yet nothing changed");
}

// 9, released
#[test]
fn a_grok_key_is_written_once_grok_releases_its_lock() {
    let machine = Machine::new();
    let target = grok(&machine, "one");
    let (file, ..) = parts(&target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, "model = \"x\"\n").unwrap();
    drop(hold(&machine.path(".grok/.config-init.lock")));
    machine.apply(&target, &server("one"));
    assert_eq!(servers(&target).get("one"), Some(&server("one")));
    assert!(fs::read_to_string(file).unwrap().starts_with("model = \"x\"\n"));
}

// 13, before staging and partial staging
#[test]
fn a_forward_edit_stopped_before_or_during_staging_rolls_back_without_its_scratch() {
    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let (run, tmp) = stopped_after_staging(&machine, &target, |landed| {
        landed[..landed.len() / 2].to_vec()
    });
    let partial = fs::read(&tmp).unwrap();
    let rolled = machine.rollback().unwrap();
    assert_eq!(rolled.run, run);
    assert_eq!(rolled.leftovers, vec![tmp.clone()], "partial bytes are never the run's own");
    assert_eq!(fs::read(&tmp).unwrap(), partial);
    assert!(!servers(&target).contains_key("one"));

    let machine = Machine::new();
    let target = cursor(&machine, "one");
    let run = machine.apply(&target, &server("one"));
    machine.edit_journal(&run, |journal| {
        journal["status"] = json!("running");
        journal["entries"][0]["step"] = json!("started");
    });
    fs::remove_file(parts(&target).0).unwrap();
    let rolled = machine.rollback().unwrap();
    assert!(rolled.restored.is_empty(), "nothing was staged, so the key already holds its prior");
    assert!(!parts(&target).0.exists());
    assert!(machine.state().record(&target).is_none());
}

/// Removes every target in one run, the way U4 removes what the loadout no longer declares, and
/// returns the run.
fn remove_many(machine: &Machine, targets: &[&Target]) -> String {
    let actions: Vec<Action> = targets
        .iter()
        .map(|target| machine.action(target, None))
        .collect();
    let mut run = machine.begin(actions.clone()).unwrap();
    let id = run.id().to_owned();
    for action in &actions {
        assert_eq!(run.remove(action).unwrap(), Removal::Removed);
    }
    let mut state = machine.state();
    run.finish(&mut state).unwrap();
    id
}

fn write_document(target: &Target, document: &Value) {
    let (file, ..) = parts(target);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, serde_json::to_vec_pretty(document).unwrap()).unwrap();
}

/// Whether any entry of `run`'s journal records a rollback intent.
fn rollback_intents(machine: &Machine, run: &str) -> usize {
    let journal: Value =
        serde_json::from_slice(&fs::read(machine.run_dir(run).join("journal.json")).unwrap())
            .unwrap();
    journal["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry.get("rollback").is_some())
        .count()
}

// ER1 (groups 4, 10): two equal Pi names are adopted without a write, then both removed in one
// run with an independent C. Each restore alone fits the empty map; together they collide.
#[test]
fn restores_that_only_collide_together_block_the_whole_rollback() {
    let machine = Machine::new();
    let a = pi(&machine, "my-srv");
    let b = pi(&machine, "my_srv");
    let c = claude(&machine, "c");
    write_document(
        &a,
        &json!({"mcpServers": {"my-srv": server("a"), "my_srv": server("b")}}),
    );
    run_many(&machine, &[&a, &b], &[server("a"), server("b")]);
    machine.apply(&c, &server("c"));
    let run = remove_many(&machine, &[&a, &b, &c]);
    assert!(servers(&a).is_empty());
    let before = machine.snapshot();
    let error = machine.rollback().unwrap_err();
    match &error {
        Error::RollbackBlocked { targets, .. } => {
            assert!(targets.iter().any(|target| target.contains("collides")), "{targets:?}");
        }
        other => panic!("{other}"),
    }
    assert!(error.is_blocked());
    assert_eq!(
        machine.snapshot(),
        before,
        "C and then B would be restored first, yet nothing changed"
    );
    assert_eq!(rollback_intents(&machine, &run), 0);
}

// ER1, positive: restores to one document that fit together all land, in one rollback.
#[test]
fn restores_to_one_document_that_fit_together_all_land() {
    let machine = Machine::new();
    let a = pi(&machine, "alpha");
    let b = pi(&machine, "beta");
    let c = claude(&machine, "c");
    write_document(
        &a,
        &json!({"mcpServers": {"alpha": server("a"), "beta": server("b")}}),
    );
    run_many(&machine, &[&a, &b], &[server("a"), server("b")]);
    machine.apply(&c, &server("c"));
    remove_many(&machine, &[&a, &b, &c]);
    let rolled = machine.rollback().unwrap();
    assert_eq!(rolled.restored, vec![c.clone(), b.clone(), a.clone()]);
    let now = servers(&a);
    assert_eq!(now.get("alpha"), Some(&server("a")));
    assert_eq!(now.get("beta"), Some(&server("b")));
    assert_eq!(servers(&c).get("c"), Some(&server("c")));
}

// ER1, aliases: a second spelling of the same document, through a linked folder, is checked
// with it. (Assumes the KeyMerge check accepts the alias spelling, as it does for Grok's.)
#[cfg(unix)]
#[test]
fn an_alias_of_the_same_document_is_checked_with_it() {
    let machine = Machine::new();
    let a = pi(&machine, "my-srv");
    write_document(
        &a,
        &json!({"mcpServers": {"my-srv": server("a"), "my_srv": server("b")}}),
    );
    let alias = machine.path("pi-alias");
    std::os::unix::fs::symlink(machine.path(".pi/agent"), &alias).unwrap();
    let b = entry(
        alias.join("mcp.json"),
        McpFormat::Json,
        "/mcpServers",
        "my_srv",
        McpShape::PiJson,
    );
    run_many(&machine, &[&a, &b], &[server("a"), server("b")]);
    let run = remove_many(&machine, &[&a, &b]);
    let before = machine.snapshot();
    let error = machine.rollback().unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    assert_eq!(machine.snapshot(), before);
    assert_eq!(rollback_intents(&machine, &run), 0);
}

// E4, the metadata exception exactly: a blocked rollback creates the selected Grok lock files
// (empty), and nothing else. A same-named file elsewhere is an ordinary file in the snapshot.
#[test]
fn a_blocked_rollback_creates_only_the_grok_lock_files_it_selected() {
    let machine = Machine::with_grok_home();
    let default_home = machine.path(".grok");
    let override_home = machine.root.join("grok-home");
    for home in [&default_home, &override_home] {
        fs::create_dir_all(home).unwrap();
        fs::write(home.join("config.toml"), "model = \"x\"\n").unwrap();
    }
    let a = grok_at(&override_home, "a");
    let b = grok_at(&default_home, "b");
    let c = cursor(&machine, "c");
    run_many(&machine, &[&a, &b, &c], &[server("a"), server("b"), server("c")]);
    let locks = [
        default_home.join(".config-init.lock"),
        override_home.join(".config-init.lock"),
    ];
    for lock in &locks {
        fs::remove_file(lock).unwrap();
    }
    let stray = machine.path("elsewhere/.config-init.lock");
    fs::create_dir_all(stray.parent().unwrap()).unwrap();
    fs::write(&stray, "not a lock loadout selected").unwrap();
    set_json(&c, |document| {
        document["mcpServers"]["c"] = json!({"command": "edited"});
    });
    let (before, locks_before) = machine.snapshot_apart(&locks);
    assert!(locks_before.values().all(Option::is_none));
    assert!(before.contains_key(&stray));
    let error = machine.rollback().unwrap_err();
    assert!(matches!(error, Error::RollbackBlocked { .. }), "{error}");
    let (after, locks_after) = machine.snapshot_apart(&locks);
    assert_eq!(after, before, "only the selected lock files may differ");
    assert!(
        locks_after
            .values()
            .all(|shape| shape.as_ref().is_some_and(|(kind, len, _)| kind == "file" && *len == 0)),
        "locks are taken before preflight blocks, so both were created empty: {locks_after:?}"
    );
}
