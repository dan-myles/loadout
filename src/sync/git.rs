//! The system `git`, always run with an argv array, never through a shell.
//!
//! Every call names its repository and work tree explicitly (`--git-dir`, `--work-tree`) and
//! inherits none of `process::GIT_REDIRECTS` (`GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`,
//! `GIT_CONFIG_PARAMETERS`, ...), so nothing in the caller's environment can redirect it to
//! another repository, index, or configuration. It runs with hooks pointed at a folder that
//! never exists, no fsmonitor, no submodule recursion, and no `ext::` transport (a URL that would
//! run a command). Local reads go through the bounded query path loadout uses for package
//! managers. Clone, fetch and merge talk to the remote or change the checkout: in the foreground
//! they inherit the terminal, so the user's credential helper or SSH key can ask for a
//! passphrase; in a supervised background run they get no terminal and every prompt git and its
//! helpers honour is turned off.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::context::{self, Context};
use crate::process::{self, Containment, Invocation};

use super::Error;

/// Turns off the prompts git and the common credential helpers honour. Local reads never need a
/// credential, so they get this in the foreground too.
const QUIET: &[(&str, &str)] = &[
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GCM_INTERACTIVE", "never"),
    ("SSH_ASKPASS_REQUIRE", "never"),
    ("GIT_OPTIONAL_LOCKS", "0"),
];

/// Where hooks would be looked up: a name in the state dir that loadout never creates.
const NO_HOOKS: &str = "loadout.no-hooks";

pub(super) struct Git<'a> {
    program: PathBuf,
    context: &'a Context,
    hooks: PathBuf,
}

/// What a local read printed, and whether it succeeded.
pub(super) struct Answer {
    pub(super) code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

impl Answer {
    pub(super) fn ok(&self) -> bool {
        self.code == Some(0)
    }

    pub(super) fn line(&self) -> &str {
        self.stdout.trim_end_matches(['\n', '\r'])
    }
}

impl<'a> Git<'a> {
    /// The `git` on PATH, or `Error::NoGit`. `state` is the canonical state dir.
    pub(super) fn find(context: &'a Context, state: &Path) -> Result<Self, Error> {
        let program = context::which("git").ok_or(Error::NoGit)?;
        Ok(Self {
            program,
            context,
            hooks: state.join(NO_HOOKS),
        })
    }

    fn background(&self) -> bool {
        matches!(self.context.containment, Containment::Inherited(_))
    }

    /// The settings every call starts with, before the subcommand.
    fn base(&self, repo: Option<&Path>) -> Vec<OsString> {
        let mut args: Vec<OsString> = Vec::new();
        if let Some(repo) = repo {
            let mut dir = OsString::from("--git-dir=");
            dir.push(repo.join(".git"));
            let mut tree = OsString::from("--work-tree=");
            tree.push(repo);
            args.push(dir);
            args.push(tree);
        }
        let mut hooks = OsString::from("core.hooksPath=");
        hooks.push(&self.hooks);
        for setting in [
            hooks,
            OsString::from("core.fsmonitor=false"),
            OsString::from("submodule.recurse=false"),
            OsString::from("protocol.ext.allow=never"),
        ] {
            args.push(OsString::from("-c"));
            args.push(setting);
        }
        args
    }

    /// A local read in `repo`, bounded like a package manager query.
    pub(super) fn read(&self, repo: &Path, args: &[&str]) -> Result<Answer, Error> {
        let mut all = self.base(Some(repo));
        all.extend(args.iter().map(OsString::from));
        let mut invocation = Invocation::new(&self.program, all);
        invocation.env = QUIET;
        let captured =
            process::query(&invocation, &self.context.containment).map_err(|error| Error::Git {
                action: describe(args),
                detail: error.to_string(),
            })?;
        Ok(Answer {
            code: captured.code,
            stdout: captured.stdout,
            stderr: captured.stderr,
        })
    }

    /// A local read that must succeed, returning its one line of output.
    pub(super) fn value(&self, repo: &Path, args: &[&str]) -> Result<String, Error> {
        let answer = self.read(repo, args)?;
        if answer.ok() {
            Ok(answer.line().to_owned())
        } else {
            Err(failed(args, &answer))
        }
    }

    /// Clone, fetch or merge: it may talk to the remote and changes the checkout. `repo` is
    /// `None` for a clone, whose destination is in `args`.
    pub(super) fn change(&self, repo: Option<&Path>, args: &[OsString]) -> Result<(), Error> {
        let mut all = self.base(repo);
        all.extend(args.iter().cloned());
        let action = describe_os(args);
        if self.background() {
            let mut invocation = Invocation::new(&self.program, all);
            invocation.env = QUIET;
            let captured =
                process::install(&invocation, &self.context.containment).map_err(|error| {
                    Error::Git {
                        action: action.clone(),
                        detail: error.to_string(),
                    }
                })?;
            if captured.success() {
                Ok(())
            } else {
                Err(Error::Git {
                    action,
                    detail: format!("{}: {}", captured.status(), redact_text(&captured.stderr)),
                })
            }
        } else {
            let mut command = Command::new(&self.program);
            for name in process::GIT_REDIRECTS {
                command.env_remove(name);
            }
            let status = command
                .args(&all)
                .env_remove(process::WORKER_NONCE)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|error| Error::Git {
                    action: action.clone(),
                    detail: spawn_detail(&error),
                })?;
            if status.success() {
                Ok(())
            } else {
                Err(Error::Git {
                    action,
                    detail: format!("git exited with {status}; its message is above"),
                })
            }
        }
    }
}

fn spawn_detail(error: &io::Error) -> String {
    format!("cannot start git: {error}")
}

fn describe(args: &[&str]) -> String {
    format!("git {}", args.first().copied().unwrap_or_default())
}

fn describe_os(args: &[OsString]) -> String {
    format!(
        "git {}",
        args.first()
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default()
    )
}

pub(super) fn failed(args: &[&str], answer: &Answer) -> Error {
    let status = match answer.code {
        Some(code) => format!("exit {code}"),
        None => "a signal".to_owned(),
    };
    Error::Git {
        action: describe(args),
        detail: format!("{status}: {}", redact_text(answer.stderr.trim())),
    }
}

/// A clone URL that carries a credential: a password in any URL, or any user name at all in an
/// `http(s)://` or `ftp(s)://` URL, where the "user name" is usually a token. Loadout stores no
/// credential, so it never takes one on the command line, where it would land in the clone's
/// config. An SSH user name (`ssh://git@host/...`, `git@host:path`) is not a credential and is
/// kept.
pub(super) fn has_credential(url: &str) -> bool {
    let Some(userinfo) = userinfo(url) else {
        return false;
    };
    if userinfo.contains(':') {
        return true;
    }
    url.split_once("://").is_some_and(|(scheme, _)| {
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ftp" | "ftps"
        )
    })
}

/// The URL as loadout shows it: any userinfo of a `scheme://` URL (a token can be the user
/// name) becomes `***`. An scp-like `user@host:path` keeps its SSH user name.
pub fn redact(url: &str) -> String {
    match url.find("://") {
        Some(scheme) => {
            let rest = &url[scheme + 3..];
            let authority = rest.find('/').map_or(rest, |slash| &rest[..slash]);
            match authority.rfind('@') {
                Some(at) => format!("{}***{}", &url[..scheme + 3], &rest[at..]),
                None => url.to_owned(),
            }
        }
        None => url.to_owned(),
    }
}

/// `redact` applied to every word of git's output, which can echo the remote URL.
fn redact_text(text: &str) -> String {
    text.split(' ')
        .map(|word| {
            if word.contains("://") {
                redact(word)
            } else {
                word.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn userinfo(url: &str) -> Option<&str> {
    match url.find("://") {
        Some(scheme) => {
            let rest = &url[scheme + 3..];
            let authority = rest.find('/').map_or(rest, |slash| &rest[..slash]);
            authority.rfind('@').map(|at| &authority[..at])
        }
        None => {
            let at = url.find('@')?;
            let colon = url.find(':')?;
            (at < colon && !url.starts_with(['/', '.'])).then(|| &url[..at])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_in_the_url_is_found_in_every_url_form() {
        for credential in [
            "https://user:secret@example.com/repo.git",
            "https://user:secret@example.com",
            "ssh://git:pw@example.com:22/repo.git",
            "https://ghp_SENTINELtoken@github.com/u/r.git",
            "HTTPS://token@example.com/repo.git",
            "http://x-access-token@example.com/r.git",
            "https://@example.com/r.git",
            "ftp://user@example.com/r.git",
        ] {
            assert!(has_credential(credential), "{credential}");
        }
        for plain in [
            "host:path/with:at@sign",
            "https://example.com/repo.git",
            "ssh://git@example.com/repo.git",
            "git@github.com:dan-myles/loadout.git",
            "file:///tmp/remote.git",
            "/tmp/remote.git",
            "https://example.com/a:b@c",
        ] {
            assert!(!has_credential(plain), "{plain}");
        }
    }

    #[test]
    fn userinfo_is_hidden_wherever_a_url_is_shown() {
        assert_eq!(
            redact("https://ghp_token@github.com/u/r.git"),
            "https://***@github.com/u/r.git"
        );
        assert_eq!(redact("https://u:p@example.com"), "https://***@example.com");
        assert_eq!(
            redact("https://example.com/r.git"),
            "https://example.com/r.git"
        );
        assert_eq!(redact("git@github.com:u/r.git"), "git@github.com:u/r.git");
        assert_eq!(
            redact_text("fatal: unable to access 'https://t@h/r/': 403"),
            "fatal: unable to access 'https://***@h/r/': 403"
        );
    }
}
