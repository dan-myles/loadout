//! `loadout init` never shows a credential that is part of the loadout URL.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const SECRET: &str = "SENTINEL-SECRET-7f3a";

fn init(url: &str, state: &Path, home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_loadout"))
        .args(["init", url, "--no-service"])
        .env("LOADOUT_HOME", home)
        .env("LOADOUT_STATE_DIR", state)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
}

fn assert_hidden(output: &Output) {
    for stream in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(stream);
        assert!(!text.contains(SECRET), "the secret was shown: {text}");
    }
}

#[test]
fn a_url_with_a_password_is_refused_before_anything_shows_it() {
    let dir = tempfile::tempdir().unwrap();
    let (home, state) = (dir.path().join("home"), dir.path().join("state"));
    fs::create_dir(&home).unwrap();
    let output = init(
        &format!("https://me:{SECRET}@example.invalid/me/loadout.git"),
        &state,
        &home,
    );
    assert_eq!(output.status.code(), Some(1));
    assert_hidden(&output);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("carries a password"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!state.exists(), "nothing is written for a refused URL");
}

#[test]
fn a_token_as_the_user_name_is_never_shown() {
    let dir = tempfile::tempdir().unwrap();
    let (home, state) = (dir.path().join("home"), dir.path().join("state"));
    fs::create_dir(&home).unwrap();
    // Nothing listens on the discard port, so the clone fails without leaving this machine.
    let output = init(
        &format!("https://{SECRET}@127.0.0.1:9/me/loadout.git"),
        &state,
        &home,
    );
    assert_ne!(output.status.code(), Some(0));
    assert_hidden(&output);
    assert!(!state.join("loadout").exists());
    assert!(!state.join("machine.toml").exists());
}
