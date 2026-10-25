//! `sync::clone` and `sync::fetch` with the real `git`, against bare remotes in scratch dirs. No
//! network and no host state: every repository, the home, and the state dir are temporary.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use loadout::context::Context;
use loadout::platform::Os;
use loadout::state;
use loadout::sync::{self, Blocked, Error, Fetched};

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

/// Runs git in `dir` with a fixed identity and no signing, and returns its stdout.
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
            "-c",
            "init.defaultBranch=main",
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

struct Scratch {
    _dir: tempfile::TempDir,
    remote: PathBuf,
    work: PathBuf,
    context: Context,
}

impl Scratch {
    /// A bare remote whose `main` holds `loadout.toml`, a work clone to push from, and a
    /// context with its own home and state dir.
    fn new() -> Self {
        Self::with_state(|dir| dir.join("state"))
    }

    /// `new`, with the state dir `state` gives for the scratch folder.
    fn with_state(state: impl FnOnce(&Path) -> PathBuf) -> Self {
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
        fs::write(work.join("loadout.toml"), "[loadout]\nversion = 1\n").unwrap();
        git(&work, &["add", "loadout.toml"]);
        git(&work, &["commit", "--quiet", "-m", "first"]);
        git(&work, &["push", "--quiet", "-u", "origin", "main"]);
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        let state = state(dir.path());
        let vars: Vec<(&str, OsString)> = vec![
            ("LOADOUT_HOME", home.into_os_string()),
            ("LOADOUT_STATE_DIR", state.into_os_string()),
        ];
        let context = Context::resolve(
            Os::current().unwrap(),
            |name| {
                vars.iter()
                    .find(|(var, _)| *var == name)
                    .map(|(_, value)| value.clone())
            },
            None,
        )
        .unwrap();
        Self {
            _dir: dir,
            remote,
            work,
            context,
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

    fn clone(&self) -> PathBuf {
        let lock = relock(&self.context);
        sync::clone(&self.url(), &self.context, &lock).unwrap();
        sync::checkout(&self.context)
    }

    fn fetch(&self) -> Fetched {
        let lock = relock(&self.context);
        sync::fetch(&self.context, &lock).unwrap()
    }

    /// A new commit on the remote's `main`, pushed from the work clone.
    fn push(&self, file: &str, text: &str) -> String {
        fs::write(self.work.join(file), text).unwrap();
        git(&self.work, &["add", file]);
        git(&self.work, &["commit", "--quiet", "-m", file]);
        git(&self.work, &["push", "--quiet", "origin", "main"]);
        git(&self.work, &["rev-parse", "HEAD"])
    }
}

fn head(clone: &Path) -> String {
    git(clone, &["rev-parse", "HEAD"])
}

#[test]
fn a_clone_is_up_to_date_and_then_fast_forwards_to_a_new_remote_commit() {
    let scratch = Scratch::new();
    let clone = scratch.clone();
    assert!(clone.join("loadout.toml").is_file());
    assert_eq!(git(&clone, &["symbolic-ref", "--short", "HEAD"]), "main");
    let first = head(&clone);
    assert_eq!(
        scratch.fetch(),
        Fetched::UpToDate {
            head: first.clone()
        }
    );

    let second = scratch.push("notes", "new\n");
    assert_eq!(
        scratch.fetch(),
        Fetched::Advanced {
            from: first,
            to: second.clone(),
        }
    );
    assert_eq!(head(&clone), second);
    assert_eq!(fs::read_to_string(clone.join("notes")).unwrap(), "new\n");
}

/// On Windows the canonical state dir is a verbatim path (`\\?\C:\...`), which git can't use as a
/// clone destination or a repository. With the state dir given in exactly that form, the real
/// git still clones, fetches, and fast-forwards, and doctor's local reads of the clone pass.
#[cfg(windows)]
#[test]
fn a_verbatim_state_dir_clones_fetches_and_reads_with_the_real_git() {
    let scratch = Scratch::with_state(|dir| {
        let state = dir.join("state");
        fs::create_dir(&state).unwrap();
        let canonical = fs::canonicalize(&state).unwrap();
        assert!(
            canonical.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            canonical.display()
        );
        canonical
    });
    scratch.clone();
    // The test's own git runs in the clone through the plain path, not the verbatim one.
    let clone = scratch.work.with_file_name("state").join("loadout");
    assert!(clone.join("loadout.toml").is_file());
    let first = head(&clone);
    assert_eq!(
        scratch.fetch(),
        Fetched::UpToDate {
            head: first.clone()
        }
    );
    let second = scratch.push("notes", "new\n");
    assert_eq!(
        scratch.fetch(),
        Fetched::Advanced {
            from: first,
            to: second.clone(),
        }
    );
    assert_eq!(head(&clone), second);
    assert_eq!(fs::read_to_string(clone.join("notes")).unwrap(), "new\n");
    let checks = sync::checks(&scratch.context);
    let checkout = checks
        .iter()
        .find(|check| check.name == "checkout")
        .unwrap();
    assert_eq!(
        checkout.status,
        loadout::diagnostic::CheckStatus::Pass,
        "{}",
        checkout.detail
    );
}

/// A password, or a token in the user name of an HTTP(S) URL, is refused before git runs: no
/// clone, no temporary folder, nothing but the lock, and the secret appears in no message.
#[test]
fn a_url_carrying_a_credential_is_refused_before_anything_is_written() {
    for url in [
        "https://user:SENTINELsecret@example.invalid/loadout.git",
        "https://ghp_SENTINELtoken@example.invalid/loadout.git",
        "http://SENTINELtoken@example.invalid/loadout.git",
    ] {
        let scratch = Scratch::new();
        let lock = relock(&scratch.context);
        let error = sync::clone(url, &scratch.context, &lock).unwrap_err();
        assert!(matches!(error, Error::Credential), "{url}: {error}");
        for shown in [
            error.to_string(),
            format!("{error:?}"),
            format!("{error:#}"),
        ] {
            assert!(!shown.contains("SENTINEL"), "{url}: {shown}");
        }
        let left: Vec<_> = fs::read_dir(&scratch.context.state)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "lock")
            .collect();
        assert!(
            left.is_empty(),
            "{url}: nothing but the lock may exist: {left:?}"
        );
    }
}

#[test]
fn an_existing_clone_is_never_overwritten() {
    let scratch = Scratch::new();
    let clone = scratch.clone();
    fs::write(clone.join("marker"), "mine").unwrap();
    let lock = relock(&scratch.context);
    let error = sync::clone(&scratch.url(), &scratch.context, &lock).unwrap_err();
    assert!(matches!(error, Error::Exists { .. }), "{error}");
    assert_eq!(fs::read_to_string(clone.join("marker")).unwrap(), "mine");
}

#[test]
fn a_failed_clone_leaves_no_temporary_folder() {
    let scratch = Scratch::new();
    let lock = relock(&scratch.context);
    let missing = format!("{}-missing", scratch.url());
    assert!(sync::clone(&missing, &scratch.context, &lock).is_err());
    let left: Vec<_> = fs::read_dir(&scratch.context.state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".loadout-clone-") || name == "loadout")
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn local_changes_block_the_fetch_and_stay_untouched() {
    for untracked in [false, true] {
        let scratch = Scratch::new();
        let clone = scratch.clone();
        let before = head(&clone);
        let (file, text) = if untracked {
            ("new-file", "untracked\n")
        } else {
            ("loadout.toml", "[loadout]\nversion = 1\n# edited\n")
        };
        fs::write(clone.join(file), text).unwrap();
        scratch.push("notes", "remote\n");
        assert_eq!(
            scratch.fetch(),
            Fetched::Blocked {
                reason: Blocked::Dirty
            },
            "untracked: {untracked}"
        );
        assert_eq!(head(&clone), before);
        assert_eq!(fs::read_to_string(clone.join(file)).unwrap(), text);
        assert!(!clone.join("notes").exists());
    }
}

#[test]
fn an_unfinished_merge_blocks_the_fetch() {
    let scratch = Scratch::new();
    let clone = scratch.clone();
    let before = head(&clone);
    fs::write(clone.join(".git").join("MERGE_HEAD"), format!("{before}\n")).unwrap();
    assert_eq!(
        scratch.fetch(),
        Fetched::Blocked {
            reason: Blocked::InProgress
        }
    );
    assert_eq!(head(&clone), before);
}

#[test]
fn a_detached_head_or_a_branch_without_upstream_blocks_the_fetch() {
    let scratch = Scratch::new();
    let clone = scratch.clone();
    git(&clone, &["checkout", "--quiet", "--detach"]);
    assert_eq!(
        scratch.fetch(),
        Fetched::Blocked {
            reason: Blocked::Detached
        }
    );

    git(&clone, &["checkout", "--quiet", "main"]);
    git(&clone, &["branch", "--unset-upstream"]);
    assert_eq!(
        scratch.fetch(),
        Fetched::Blocked {
            reason: Blocked::NoUpstream
        }
    );
}

#[test]
fn local_commits_are_never_pushed_or_discarded() {
    let scratch = Scratch::new();
    let clone = scratch.clone();
    fs::write(clone.join("local"), "mine\n").unwrap();
    git(&clone, &["add", "local"]);
    git(&clone, &["commit", "--quiet", "-m", "local"]);
    let mine = head(&clone);
    assert_eq!(
        scratch.fetch(),
        Fetched::Blocked {
            reason: Blocked::Ahead
        }
    );
    assert_eq!(head(&clone), mine);

    scratch.push("notes", "remote\n");
    assert_eq!(
        scratch.fetch(),
        Fetched::Blocked {
            reason: Blocked::Diverged
        }
    );
    assert_eq!(head(&clone), mine, "the branch must not move");
    assert!(
        !clone.join("notes").exists(),
        "the working tree must not change"
    );
    assert_eq!(
        git(&scratch.remote, &["log", "-1", "--format=%s", "main"]),
        "notes",
        "nothing was pushed"
    );
}

#[cfg(unix)]
#[test]
fn hooks_in_the_clone_never_run() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let clone = scratch.clone();
    let marker = clone.parent().unwrap().join("hook-ran");
    for hook in ["post-merge", "post-checkout", "reference-transaction"] {
        let path = clone.join(".git").join("hooks").join(hook);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            format!("#!/bin/sh\necho {hook} >> '{}'\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    scratch.push("notes", "remote\n");
    assert!(matches!(scratch.fetch(), Fetched::Advanced { .. }));
    assert!(
        !marker.exists(),
        "a hook ran: {:?}",
        fs::read_to_string(&marker)
    );
}

#[cfg(unix)]
#[test]
fn a_linked_clone_is_refused() {
    let scratch = Scratch::new();
    let clone = scratch.clone();
    let elsewhere = scratch.context.state.parent().unwrap().join("elsewhere");
    fs::rename(&clone, &elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &clone).unwrap();
    let lock = relock(&scratch.context);
    let error = sync::fetch(&scratch.context, &lock).unwrap_err();
    assert!(matches!(error, Error::NotAClone { .. }), "{error}");
}

#[test]
fn the_lock_of_another_state_dir_is_refused() {
    let scratch = Scratch::new();
    scratch.clone();
    let other = Scratch::new();
    let foreign = relock(&other.context);
    let error = sync::fetch(&scratch.context, &foreign).unwrap_err();
    assert!(
        matches!(error, Error::State(state::Error::WrongLock { .. })),
        "{error}"
    );
}

#[test]
fn the_clone_is_where_plan_and_apply_read_by_default() {
    let scratch = Scratch::new();
    assert_eq!(
        sync::checkout(&scratch.context),
        loadout::plan::default_checkout(&scratch.context)
    );
}
