use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const ROOT: &str = "[loadout]\nversion = 1\n\n[variables]\nemail = \"dan@example.com\"\n\n";

struct Case {
    _dir: tempfile::TempDir,
    loadout: PathBuf,
    home: PathBuf,
    state: PathBuf,
}

impl Case {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let loadout = dir.path().join("loadout");
        let home = dir.path().join("home");
        let state = dir.path().join("state");
        fs::create_dir(&home).unwrap();
        for (path, contents) in files {
            let path = loadout.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
        Self {
            _dir: dir,
            loadout,
            home,
            state,
        }
    }

    fn command(&self, json: bool) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_loadout"));
        command
            .arg("validate")
            .arg(&self.loadout)
            .env("LOADOUT_HOME", &self.home)
            .env("LOADOUT_STATE_DIR", &self.state);
        if json {
            command.arg("--json");
        }
        command
    }

    fn validate(&self) -> (i32, String) {
        let output = self.command(false).output().unwrap();
        self.assert_untouched();
        (output.status.code().unwrap(), text(&output))
    }

    fn validate_json(&self) -> (i32, Value) {
        let output = self.command(true).output().unwrap();
        self.assert_untouched();
        (
            output.status.code().unwrap(),
            serde_json::from_slice(&output.stdout).unwrap(),
        )
    }

    fn assert_untouched(&self) {
        assert_eq!(
            fs::read_dir(&self.home).unwrap().count(),
            0,
            "validate wrote into the home"
        );
        assert!(!self.state.exists(), "validate created the state dir");
    }
}

fn text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn root(extra: &str) -> Vec<u8> {
    format!("{ROOT}{extra}").into_bytes()
}

const GITCONFIG: &str =
    "[files.gitconfig]\nsource = \"files/gitconfig.tmpl\"\ntarget = \"~/.gitconfig\"\n";

#[test]
fn a_template_syntax_error_makes_the_loadout_invalid() {
    let case = Case::new(&[
        ("loadout.toml", &root(GITCONFIG)),
        (
            "files/gitconfig.tmpl",
            b"[user]\n\temail = {{ email }}\n{% if machine.os == \"windows\" %}\nautocrlf = true\n",
        ),
    ]);
    let (code, out) = case.validate();
    assert_eq!(code, 3, "{out}");
    assert!(
        out.contains(
            "files/gitconfig.tmpl: error: line 3: syntax error: unexpected end of input, expected end of block"
        ),
        "{out}"
    );
    assert!(out.contains("invalid: "), "{out}");
    assert!(out.contains("(1 errors, 0 warnings)"), "{out}");

    let (code, json) = case.validate_json();
    assert_eq!(code, 3);
    assert_eq!(json["valid"], false);
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 1, "{json:#}");
    assert_eq!(diagnostics[0]["severity"], "error");
    assert_eq!(diagnostics[0]["file"], "files/gitconfig.tmpl");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("line 3: syntax error:")
    );
}

#[test]
fn a_template_needs_no_machine_or_variable_to_validate() {
    let case = Case::new(&[
        ("loadout.toml", &root(GITCONFIG)),
        (
            "files/gitconfig.tmpl",
            b"{{ email }} {{ only_on_the_work_laptop }}\n{% if machine.os == \"windows\" %}{{ machine.home }}{% else %}{{ machine.distro or \"\" }}{% endif %}\n",
        ),
    ]);
    let (code, out) = case.validate();
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("valid: "), "{out}");
}

#[test]
fn includes_imports_and_extends_are_invalid() {
    for statement in [
        "{% include \"files/other.tmpl\" %}",
        "{% import \"files/other.tmpl\" as other %}",
        "{% extends \"files/other.tmpl\" %}",
    ] {
        let case = Case::new(&[
            ("loadout.toml", &root(GITCONFIG)),
            (
                "files/gitconfig.tmpl",
                format!("ok\n{statement}\n").as_bytes(),
            ),
            ("files/other.tmpl", b"other"),
        ]);
        let (code, out) = case.validate();
        assert_eq!(code, 3, "{out}");
        assert!(
            out.contains("files/gitconfig.tmpl: error: line 2:"),
            "{out}"
        );
        assert!(
            out.contains("cannot include, import, or extend files"),
            "{out}"
        );
    }
}

#[test]
fn every_layer_template_is_checked_once_on_any_os() {
    let case = Case::new(&[
        ("loadout.toml", &root(&format!("{GITCONFIG}\n[machines.box]\ntags = [\"work\"]\n"))),
        (
            "os/windows.toml",
            b"[files.profile]\nsource = \"files/profile.ps1.tmpl\"\ntarget = \"~/Documents/profile.ps1\"\n",
        ),
        (
            "tags/work.toml",
            b"[files.gitconfig]\nsource = \"files/work.tmpl\"\n\n[files.copy]\nsource = \"files/gitconfig.tmpl\"\ntarget = \"~/.gitconfig-copy\"\n",
        ),
        ("files/gitconfig.tmpl", b"{% if %}"),
        ("files/profile.ps1.tmpl", b"{{ email "),
        ("files/work.tmpl", b"{% endif %}"),
    ]);
    let (code, out) = case.validate();
    assert_eq!(code, 3, "{out}");
    for file in [
        "files/gitconfig.tmpl",
        "files/profile.ps1.tmpl",
        "files/work.tmpl",
    ] {
        assert_eq!(
            out.matches(&format!("{file}: error: line 1: syntax error"))
                .count(),
            1,
            "{file} must be reported exactly once:\n{out}"
        );
    }
}

#[test]
fn only_tmpl_user_files_are_parsed() {
    let broken = b"{% if %} {{ unclosed";
    let case = Case::new(&[
        (
            "loadout.toml",
            &root(
                "[agents]\nenabled = [\"claude\"]\n\n\
                 [files.plain]\nsource = \"files/plain.conf\"\ntarget = \"~/.plain\"\n\n\
                 [skills.review]\nsource = \"skills/review\"\n\n\
                 [instructions.global]\nsource = \"instructions/AGENTS.md.tmpl\"\n",
            ),
        ),
        ("files/plain.conf", broken),
        ("skills/review/SKILL.md", broken),
        ("skills/review/notes.tmpl", broken),
        ("instructions/AGENTS.md.tmpl", broken),
    ]);
    let (code, out) = case.validate();
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("syntax error"), "{out}");
    assert!(
        out.contains("only user files are rendered; this .tmpl file is copied as is"),
        "{out}"
    );
}

#[cfg(unix)]
#[test]
fn a_template_outside_the_loadout_is_refused_without_reading_it() {
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.tmpl");
    fs::write(&secret, "{% if %} SECRET-CONTENT").unwrap();
    let case = Case::new(&[("loadout.toml", &root(GITCONFIG))]);
    fs::create_dir_all(case.loadout.join("files")).unwrap();
    std::os::unix::fs::symlink(&secret, case.loadout.join("files/gitconfig.tmpl")).unwrap();

    let (code, out) = case.validate();
    assert_eq!(code, 3, "{out}");
    assert!(
        out.contains("files/gitconfig.tmpl resolves outside the loadout"),
        "{out}"
    );
    assert!(!out.contains("syntax error"), "{out}");
    assert!(!out.contains("SECRET-CONTENT"), "{out}");
}

#[cfg(unix)]
#[test]
fn a_fifo_template_is_refused_without_blocking() {
    let case = Case::new(&[("loadout.toml", &root(GITCONFIG))]);
    fs::create_dir_all(case.loadout.join("files")).unwrap();
    let status = Command::new("mkfifo")
        .arg(case.loadout.join("files/gitconfig.tmpl"))
        .status()
        .unwrap();
    assert!(status.success());

    let mut child = case.command(false).stdout(Stdio::piped()).spawn().unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            child.kill().unwrap();
            panic!("validate blocked on a FIFO template");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(
        text(&output).contains("files/gitconfig.tmpl is not a file"),
        "{}",
        text(&output)
    );
    case.assert_untouched();
}

#[test]
fn a_template_that_is_not_utf8_is_invalid() {
    let case = Case::new(&[
        ("loadout.toml", &root(GITCONFIG)),
        ("files/gitconfig.tmpl", b"name = \xff\xfe\n"),
    ]);
    let (code, out) = case.validate();
    assert_eq!(code, 3, "{out}");
    assert!(
        out.contains("files/gitconfig.tmpl: error: a template must be UTF-8 text"),
        "{out}"
    );
}
