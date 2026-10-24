use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use loadout::context::Context;
#[cfg(unix)]
use loadout::manifest::Loadout;
#[cfg(unix)]
use loadout::platform::Os;
#[cfg(unix)]
use loadout::template;

const NOTES: &str = "[loadout]\nversion = 1\n\n\
    [files.notes]\nsource = \"files/notes.tmpl\"\ntarget = \"~/notes\"\n";

struct Sandbox {
    _dir: tempfile::TempDir,
    home: PathBuf,
    state: PathBuf,
}

fn sandbox() -> Sandbox {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let state = dir.path().join("state");
    fs::create_dir(&home).unwrap();
    Sandbox {
        _dir: dir,
        home,
        state,
    }
}

fn put(path: &Path, text: &str) -> PathBuf {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    path.to_path_buf()
}

fn manifest(source: &str) -> String {
    NOTES.replace("files/notes.tmpl", source)
}

impl Sandbox {
    fn validate(&self, checkout: &Path, json: bool) -> (i32, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_loadout"));
        command
            .arg("validate")
            .arg(checkout)
            .env("LOADOUT_HOME", &self.home)
            .env("LOADOUT_STATE_DIR", &self.state);
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert!(!self.state.exists(), "validate created the state dir");
        let mut text = String::from_utf8_lossy(&output.stdout).to_string();
        if !json {
            text = text.replace('\\', "/");
        }
        (output.status.code().unwrap(), text)
    }

    #[cfg(unix)]
    fn context(&self) -> Context {
        let home = self.home.clone();
        Context::resolve(
            Os::current().unwrap(),
            move |name| (name == "LOADOUT_HOME").then(|| home.clone().into_os_string()),
            None,
        )
        .unwrap()
    }
}

fn assert_refused(code: i32, out: &str, source: &str, reason: &str) {
    assert_eq!(code, 3, "{out}");
    assert!(
        out.contains(&format!("{source}: error: {source} overlaps ")),
        "{out}"
    );
    assert!(out.contains(reason), "{out}");
    assert!(!out.contains("SENTINEL"), "{out}");
    assert!(!out.contains("syntax error"), "{out}");
}

#[cfg(unix)]
const AUTH: &str = "an auth or OAuth store";
const PRIVATE: &str = "a private or generated folder";

#[cfg(unix)]
#[test]
fn a_template_aliasing_an_auth_store_in_a_home_checkout_is_refused_before_it_is_read() {
    let sandbox = sandbox();
    let home = &sandbox.home;
    put(&home.join("loadout.toml"), NOTES);
    let auth = put(&home.join(".codex/auth.json"), "AUTH-SENTINEL {% if %}\n");
    fs::create_dir_all(home.join("files")).unwrap();
    std::os::unix::fs::symlink(&auth, home.join("files/notes.tmpl")).unwrap();

    let (code, out) = sandbox.validate(home, false);
    assert_refused(code, &out, "files/notes.tmpl", AUTH);

    let (code, json) = sandbox.validate(home, true);
    assert_eq!(code, 3, "{json}");
    let json: serde_json::Value = serde_json::from_str(&json).unwrap();
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{json:#}");
    assert_eq!(diagnostics[0]["file"], "files/notes.tmpl");
    let message = diagnostics[0]["message"].as_str().unwrap();
    assert!(message.contains(AUTH), "{message}");
    assert!(!message.contains("SENTINEL"), "{message}");
    assert_eq!(
        fs::read_to_string(&auth).unwrap(),
        "AUTH-SENTINEL {% if %}\n"
    );
}

#[cfg(unix)]
#[test]
fn a_template_reached_through_a_folder_link_into_private_settings_is_refused() {
    let sandbox = sandbox();
    let home = &sandbox.home;
    put(&home.join("loadout.toml"), &manifest("files/t3/notes.tmpl"));
    put(&home.join(".t3/notes.tmpl"), "PRIVATE-SENTINEL {% if %}\n");
    fs::create_dir_all(home.join("files")).unwrap();
    std::os::unix::fs::symlink(home.join(".t3"), home.join("files/t3")).unwrap();

    let (code, out) = sandbox.validate(home, false);
    assert_refused(code, &out, "files/t3/notes.tmpl", PRIVATE);
}

#[cfg(windows)]
#[test]
fn a_template_reached_through_a_junction_into_private_settings_is_refused() {
    let sandbox = sandbox();
    let home = &sandbox.home;
    put(&home.join("loadout.toml"), &manifest("files/t3/notes.tmpl"));
    put(&home.join(".t3/notes.tmpl"), "PRIVATE-SENTINEL {% if %}\n");
    fs::create_dir_all(home.join("files")).unwrap();
    let status = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(home.join("files").join("t3"))
        .arg(home.join(".t3"))
        .status()
        .unwrap();
    assert!(status.success());

    let (code, out) = sandbox.validate(home, false);
    assert_refused(code, &out, "files/t3/notes.tmpl", PRIVATE);
}

#[test]
fn a_template_inside_a_private_folder_of_a_home_checkout_is_refused() {
    let sandbox = sandbox();
    let home = &sandbox.home;
    put(&home.join("loadout.toml"), &manifest(".t3/notes.tmpl"));
    put(&home.join(".t3/notes.tmpl"), "PRIVATE-SENTINEL {% if %}\n");

    let (code, out) = sandbox.validate(home, false);
    assert_refused(code, &out, ".t3/notes.tmpl", PRIVATE);
}

#[cfg(unix)]
#[test]
fn an_auth_store_alias_is_refused_even_when_the_checkout_sits_in_a_private_folder() {
    let sandbox = sandbox();
    let checkout = sandbox.home.join(".t3/worktrees/loadout");
    put(&checkout.join("loadout.toml"), NOTES);
    let auth = put(&checkout.join("codex/auth.json"), "AUTH-SENTINEL {% if %}\n");
    fs::create_dir_all(checkout.join("files")).unwrap();
    std::os::unix::fs::symlink(&auth, checkout.join("files/notes.tmpl")).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loadout"))
        .arg("validate")
        .arg(&checkout)
        .env_clear()
        .env("HOME", &sandbox.home)
        .env("CODEX_HOME", checkout.join("codex"))
        .env("LOADOUT_STATE_DIR", &sandbox.state)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout).to_string();
    assert_refused(
        output.status.code().unwrap(),
        &out,
        "files/notes.tmpl",
        AUTH,
    );
}

#[test]
fn an_ordinary_template_in_a_checkout_inside_a_private_folder_is_read() {
    let sandbox = sandbox();
    let checkout = sandbox.home.join(".t3/worktrees/loadout");
    put(&checkout.join("loadout.toml"), NOTES);
    put(&checkout.join("files/notes.tmpl"), "{% if %}\n");

    let (code, out) = sandbox.validate(&checkout, false);
    assert_eq!(code, 3, "{out}");
    assert!(
        out.contains("files/notes.tmpl: error: line 1: syntax error"),
        "{out}"
    );
    assert!(!out.contains("overlaps"), "{out}");
}

#[cfg(unix)]
#[test]
fn an_ordinary_template_alias_inside_the_checkout_is_read_and_checked() {
    let sandbox = sandbox();
    let checkout = sandbox.home.join("work/loadout");
    put(&checkout.join("loadout.toml"), NOTES);
    put(&checkout.join("files/real.tmpl"), "{% if %}\n");
    std::os::unix::fs::symlink("real.tmpl", checkout.join("files/notes.tmpl")).unwrap();

    let (code, out) = sandbox.validate(&checkout, false);
    assert_eq!(code, 3, "{out}");
    assert!(
        out.contains("files/notes.tmpl: error: line 1: syntax error"),
        "{out}"
    );
    assert!(!out.contains("overlaps"), "{out}");
}

#[test]
fn resolver_and_template_problems_are_both_reported_resolver_first() {
    let sandbox = sandbox();
    let checkout = sandbox.home.join("work/loadout");
    let text = format!(
        "{NOTES}\n[files.colon]\nsource = \"files/plain\"\ntarget = \"~/notes:today\"\n"
    );
    put(&checkout.join("loadout.toml"), &text);
    put(&checkout.join("files/notes.tmpl"), "{% if %}\n");
    put(&checkout.join("files/plain"), "plain\n");

    let (code, out) = sandbox.validate(&checkout, false);
    assert_eq!(code, 3, "{out}");
    let at = |expected: &str| {
        out.lines()
            .position(|line| line.contains(expected))
            .unwrap_or_else(|| panic!("no {expected:?} in:\n{out}"))
    };
    let resolver = at("error: files.colon.target:");
    let syntax = at("files/notes.tmpl: error: line 1: syntax error");
    assert!(resolver < syntax, "{out}");
}

#[cfg(unix)]
#[test]
fn a_template_changed_into_an_auth_alias_after_loading_is_refused_by_its_own_check() {
    let sandbox = sandbox();
    let home = &sandbox.home;
    let context = sandbox.context();
    put(&home.join("loadout.toml"), NOTES);
    put(&home.join("files/real.tmpl"), "{{ x }}\n");
    let link = home.join("files/notes.tmpl");
    std::os::unix::fs::symlink("real.tmpl", &link).unwrap();
    let (loadout, _) = Loadout::load_guarded(home, &context).unwrap();
    assert!(template::check_all_guarded(&loadout, &context).is_empty());

    let auth = put(&home.join(".codex/auth.json"), "AUTH-SENTINEL {% if %}\n");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&auth, &link).unwrap();
    let diagnostics = template::check_all_guarded(&loadout, &context);
    let messages: Vec<&str> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    let [message] = messages[..] else {
        panic!("{messages:?}");
    };
    assert!(message.starts_with("files/notes.tmpl overlaps "), "{message}");
    assert!(message.contains(AUTH), "{message}");
    assert!(!message.contains("SENTINEL"), "{message}");
}

#[cfg(unix)]
#[test]
fn validate_does_not_read_a_plain_file_source_that_aliases_an_auth_store() {
    let sandbox = sandbox();
    let home = &sandbox.home;
    let text = "[loadout]\nversion = 1\n\n\
        [files.plain]\nsource = \"files/plain\"\ntarget = \"~/plain\"\n";
    put(&home.join("loadout.toml"), text);
    let auth = put(&home.join(".codex/auth.json"), "AUTH-SENTINEL\n");
    fs::create_dir_all(home.join("files")).unwrap();
    std::os::unix::fs::symlink(&auth, home.join("files/plain")).unwrap();

    let (code, out) = sandbox.validate(home, false);
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("SENTINEL"), "{out}");
}
