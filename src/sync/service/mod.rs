//! The per-user scheduled job that runs `loadout update --background` (architecture section 8):
//! a systemd user timer on Linux, a launchd agent on macOS, a Scheduled Task on Windows. Each is
//! installed for the current user only, with no elevation, and never under `LOADOUT_HOME`
//! isolation, because every scheduler acts on the real user whatever `Context` says.
//!
//! `install` records what the job needs in `<state>/service.toml`: the executable, the
//! interval, the home dir, and the few environment variables `Context` and the package
//! managers read. A background run rebuilds its `Context` from that file alone, so it writes
//! exactly where the user's CLI did at install time. Nothing credential-like is recorded.

mod launchd;
mod schtasks;
mod systemd;

use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::{self, Context, STATE_OVERRIDE};
use crate::diagnostic::{Check, CheckStatus};
use crate::manifest::MIN_INTERVAL_MINUTES;
use crate::platform::Os;
use crate::process::{self, Captured, Invocation};
use crate::state::{self, Private};

/// The Windows launcher's file stem. It sits beside `loadout.exe` and is what the Scheduled
/// Task starts, so no console window opens at each run.
pub const LAUNCHER: &str = "loadout-background";

const VERSION: u32 = 1;
const MAX_RECORD: u64 = 64 * 1024;

/// The environment variables a background run takes from `service.toml` rather than from the
/// scheduler: the ones `Context` resolves directories from, and the search path package managers
/// and git are found on.
const CAPTURED: &[&str] = &[
    "PATH",
    "PATHEXT",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    STATE_OVERRIDE,
];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "services are not installed or removed under LOADOUT_HOME isolation: the scheduler acts on the real user, whatever LOADOUT_HOME says"
    )]
    Isolated,
    #[error(
        "there is no systemd user session here ({0}); loadout's Linux service is a systemd user timer"
    )]
    NoUserSession(String),
    #[error(
        "{path} is missing; on Windows the service runs {LAUNCHER}.exe, which must sit beside loadout.exe"
    )]
    MissingLauncher { path: PathBuf },
    #[error("{path:?} {reason}, so loadout won't write it into a scheduler file")]
    UnsafePath { path: PathBuf, reason: &'static str },
    #[error("cannot use loadout's own executable: {0}")]
    Exe(String),
    #[error("the update interval must be at least {MIN_INTERVAL_MINUTES} minutes, got {0}")]
    Interval(u32),
    #[error("{command} failed: {detail}")]
    Scheduler { command: String, detail: String },
    #[error("cannot {action} {path}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error(
        "{path} was recorded for a different environment (its {what} differs); run `loadout service install` again"
    )]
    Moved { path: PathBuf, what: &'static str },
    #[error(transparent)]
    State(#[from] state::Error),
}

impl Error {
    /// A refusal about this machine or how loadout was installed, not a failure of the tool:
    /// the command exits 4.
    pub fn is_unhealthy(&self) -> bool {
        matches!(
            self,
            Error::Isolated
                | Error::NoUserSession(_)
                | Error::MissingLauncher { .. }
                | Error::UnsafePath { .. }
                | Error::Interval(_)
                | Error::Moved { .. }
        )
    }
}

/// What `<state>/service.toml` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recorded {
    pub version: u32,
    /// `loadout` itself (on Windows, `loadout.exe`; the task starts the launcher beside it).
    pub exe: PathBuf,
    pub interval_minutes: u32,
    /// Seconds since the Unix epoch.
    pub installed: u64,
    pub home: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl Recorded {
    /// The `Context` a background run uses: resolved from the recorded home and variables only,
    /// never from the scheduler's environment, and refused unless it lands on the same dirs as
    /// at install time.
    pub fn context(&self, os: Os, path: &Path) -> Result<Context, Error> {
        let context = Context::resolve(
            os,
            |name| self.env.get(name).map(OsString::from),
            Some(self.home.clone()),
        )
        .map_err(|error| Error::Invalid {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let moved = |what| Error::Moved {
            path: path.to_path_buf(),
            what,
        };
        if context.home != self.home {
            return Err(moved("home dir"));
        }
        if context.config != self.config {
            return Err(moved("config dir"));
        }
        if context.state != self.state {
            return Err(moved("state dir"));
        }
        Ok(context)
    }

    /// The variables a background worker is started with, on top of the scheduler's.
    pub fn env(&self) -> impl Iterator<Item = (&str, &str)> {
        self.env
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// Reads the `service.toml` a scheduler named, and the `Context` it records. The file must be a
/// regular file, and it must be the `service.toml` of the very state dir it records (read back
/// through the state dir's own trust rules), so a file planted elsewhere can't steer a run.
pub fn load(path: &Path) -> Result<(Recorded, Context), Error> {
    let invalid = |message: String| Error::Invalid {
        path: path.to_path_buf(),
        message,
    };
    let metadata = fs::symlink_metadata(path).map_err(io("inspect", path))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid("must be a regular file, not a link".to_owned()));
    }
    if metadata.len() > MAX_RECORD {
        return Err(invalid(format!("is larger than {MAX_RECORD} bytes")));
    }
    let bytes = fs::read(path).map_err(io("read", path))?;
    let recorded = parse(&bytes).map_err(invalid)?;
    let context = recorded.context(
        Os::current().map_err(|error| invalid(error.to_string()))?,
        path,
    )?;
    let own = state::read_private(&context, Private::Service)?;
    let same_file =
        fs::canonicalize(path).ok() == fs::canonicalize(Private::Service.path(&context)).ok();
    if own.as_deref() != Some(bytes.as_slice()) || !same_file {
        return Err(invalid(format!(
            "is not the service.toml of the state dir it records ({})",
            recorded.state.display()
        )));
    }
    Ok((recorded, context))
}

fn parse(bytes: &[u8]) -> Result<Recorded, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "is not valid UTF-8".to_owned())?;
    let recorded: Recorded = toml::from_str(text).map_err(|error| error.to_string())?;
    if recorded.version != VERSION {
        return Err(format!(
            "has version {}, expected {VERSION}",
            recorded.version
        ));
    }
    Ok(recorded)
}

/// What `install` set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub exe: PathBuf,
    pub interval_minutes: u32,
    /// Things worth telling the user that didn't stop the install.
    pub warnings: Vec<String>,
}

/// Installs or replaces the scheduled job, running every `interval_minutes`. The caller takes
/// the interval from the loadout's resolved `[update]`, and doesn't call this when
/// `[update] auto = false`.
pub fn install(context: &Context, interval_minutes: u32) -> Result<Installed, Error> {
    if context.isolated {
        return Err(Error::Isolated);
    }
    if interval_minutes < MIN_INTERVAL_MINUTES {
        return Err(Error::Interval(interval_minutes));
    }
    let mut warnings = Vec::new();
    let exe = own_exe(&mut warnings)?;
    let program = match context.os {
        Os::Windows => launcher_beside(&exe)?,
        Os::Linux | Os::Macos => exe.clone(),
    };
    let service = Private::Service.path(context);
    let program_text = safe(&program)?;
    let service_text = safe(&service)?;
    if context.os == Os::Linux {
        systemd::session(context)?;
    }
    let recorded = Recorded {
        version: VERSION,
        exe: exe.clone(),
        interval_minutes,
        installed: super::now(),
        home: context.home.clone(),
        config: context.config.clone(),
        state: context.state.clone(),
        env: captured(&mut warnings),
    };
    let text = toml::to_string(&recorded).map_err(|error| Error::Invalid {
        path: service.clone(),
        message: error.to_string(),
    })?;
    let written = state::write_private(context, Private::Service, text.as_bytes())?;
    match context.os {
        Os::Linux => systemd::install(context, program_text, service_text, interval_minutes)?,
        Os::Macos => launchd::install(
            context,
            program_text,
            service_text,
            &written,
            interval_minutes,
        )?,
        Os::Windows => schtasks::install(context, program_text, service_text, interval_minutes)?,
    }
    Ok(Installed {
        exe,
        interval_minutes,
        warnings,
    })
}

/// Removes the scheduled job and `service.toml`. A job that isn't there is not an error.
pub fn uninstall(context: &Context) -> Result<(), Error> {
    if context.isolated {
        return Err(Error::Isolated);
    }
    match context.os {
        Os::Linux => systemd::uninstall(context)?,
        Os::Macos => launchd::uninstall(context)?,
        Os::Windows => schtasks::uninstall(context)?,
    }
    state::remove_private(context, Private::Service)?;
    Ok(())
}

/// One scheduler file loadout writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct File {
    pub path: PathBuf,
    pub present: bool,
}

/// What the scheduler itself says about the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scheduler {
    pub registered: bool,
    pub detail: String,
}

/// The job as it is now. Read-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// `service.toml`, when it could be read.
    pub recorded: Option<Recorded>,
    /// Why `service.toml` couldn't be used, when it exists.
    pub problem: Option<String>,
    pub files: Vec<File>,
    /// `None` under isolation, where the scheduler is never asked.
    pub scheduler: Option<Scheduler>,
    /// Whether the recorded executable is the one running now.
    pub exe_current: Option<bool>,
}

impl Status {
    /// Something is installed: `service.toml`, a scheduler file, or a registered job.
    pub fn installed(&self) -> bool {
        self.recorded.is_some()
            || self.problem.is_some()
            || self.files.iter().any(|file| file.present)
            || self
                .scheduler
                .as_ref()
                .is_some_and(|scheduler| scheduler.registered)
    }

    /// Nothing is installed, or everything is: `service.toml` readable and for this
    /// environment, every scheduler file present, the job registered, and the recorded
    /// executable the current one.
    pub fn healthy(&self) -> bool {
        if !self.installed() {
            return true;
        }
        self.recorded.is_some()
            && self.problem.is_none()
            && self.files.iter().all(|file| file.present)
            && self
                .scheduler
                .as_ref()
                .is_none_or(|scheduler| scheduler.registered)
            && self.exe_current != Some(false)
    }
}

/// Reads the job's state: `service.toml`, the scheduler files, and (outside isolation) what the
/// scheduler reports. It never writes.
pub fn status(context: &Context) -> Result<Status, Error> {
    let path = Private::Service.path(context);
    let (recorded, problem) = match state::read_private(context, Private::Service) {
        Ok(None) => (None, None),
        Ok(Some(bytes)) => match parse(&bytes) {
            Ok(recorded) if recorded.state != context.state => (
                Some(recorded),
                Some(
                    Error::Moved {
                        path,
                        what: "state dir",
                    }
                    .to_string(),
                ),
            ),
            Ok(recorded) => (Some(recorded), None),
            Err(message) => (None, Some(Error::Invalid { path, message }.to_string())),
        },
        Err(error) => (None, Some(error.to_string())),
    };
    let files = match context.os {
        Os::Linux => systemd::files(context),
        Os::Macos => vec![launchd::file(context)],
        Os::Windows => Vec::new(),
    }
    .into_iter()
    .map(|path| File {
        present: fs::symlink_metadata(&path).is_ok(),
        path,
    })
    .collect();
    let scheduler = if context.isolated {
        None
    } else {
        Some(match context.os {
            Os::Linux => systemd::registered(context),
            Os::Macos => launchd::registered(context, &Private::Service.path(context)),
            Os::Windows => schtasks::registered(context),
        })
    };
    let exe_current = recorded.as_ref().map(|recorded| {
        env::current_exe()
            .and_then(fs::canonicalize)
            .is_ok_and(|current| current == recorded.exe)
    });
    Ok(Status {
        recorded,
        problem,
        files,
        scheduler,
        exe_current,
    })
}

pub(super) fn checks(context: &Context) -> Vec<Check> {
    let name = "service";
    let status = match status(context) {
        Ok(status) => status,
        Err(error) => return vec![Check::new(name, CheckStatus::Warn, error.to_string())],
    };
    let check = if !status.installed() {
        Check::new(
            name,
            CheckStatus::Info,
            "no background update is installed; `loadout service install` adds one",
        )
    } else if status.healthy() {
        let every = status
            .recorded
            .as_ref()
            .map(|recorded| format!("every {} minutes", recorded.interval_minutes))
            .unwrap_or_default();
        Check::new(name, CheckStatus::Pass, format!("installed, {every}"))
    } else {
        Check::new(name, CheckStatus::Warn, describe(&status))
    };
    vec![check]
}

/// Why an installed job isn't healthy, for `doctor` and `service status`.
pub fn describe(status: &Status) -> String {
    if let Some(problem) = &status.problem {
        return problem.clone();
    }
    if status.recorded.is_none() {
        return "a scheduler file exists but service.toml doesn't; run `loadout service install` again".to_owned();
    }
    if let Some(missing) = status.files.iter().find(|file| !file.present) {
        return format!(
            "{} is missing; run `loadout service install` again",
            missing.path.display()
        );
    }
    if let Some(scheduler) = &status.scheduler
        && !scheduler.registered
    {
        return format!(
            "the scheduler doesn't have the job ({}); run `loadout service install` again",
            scheduler.detail
        );
    }
    if status.exe_current == Some(false) {
        return "the job runs a different loadout executable than this one; run `loadout service install` again to switch".to_owned();
    }
    "installed".to_owned()
}

/// This executable, absolute and resolved. Refused when it no longer exists (Linux reports a
/// replaced binary as ` (deleted)`) or isn't a regular file.
fn own_exe(warnings: &mut Vec<String>) -> Result<PathBuf, Error> {
    let exe = env::current_exe().map_err(|error| Error::Exe(error.to_string()))?;
    if exe.to_string_lossy().ends_with(" (deleted)") {
        return Err(Error::Exe(format!(
            "{} was replaced or deleted while running",
            exe.display()
        )));
    }
    let exe = fs::canonicalize(&exe)
        .map_err(|error| Error::Exe(format!("{}: {error}", exe.display())))?;
    if !fs::metadata(&exe).is_ok_and(|metadata| metadata.is_file()) {
        return Err(Error::Exe(format!(
            "{} is not a regular file",
            exe.display()
        )));
    }
    let components: Vec<Component<'_>> = exe.components().collect();
    if components.windows(2).any(|pair| {
        pair[0].as_os_str() == "target"
            && matches!(pair[1].as_os_str().to_str(), Some("debug" | "release"))
    }) {
        warnings.push(format!(
            "{} looks like a development build in a Cargo target dir; the job runs it as long as it stays there",
            exe.display()
        ));
    }
    Ok(exe)
}

/// The Windows launcher beside `exe`, which must be a regular file.
fn launcher_beside(exe: &Path) -> Result<PathBuf, Error> {
    let path = exe.with_file_name(format!("{LAUNCHER}.exe"));
    if fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
        Ok(path)
    } else {
        Err(Error::MissingLauncher { path })
    }
}

/// `path` as text that every scheduler file can carry: valid UTF-8, with no control
/// character (a newline or a NUL would end or split a line, an entry, or an argument).
pub(crate) fn safe(path: &Path) -> Result<&str, Error> {
    let text = path.to_str().ok_or_else(|| Error::UnsafePath {
        path: path.to_path_buf(),
        reason: "is not valid UTF-8",
    })?;
    if text.chars().any(char::is_control) {
        return Err(Error::UnsafePath {
            path: path.to_path_buf(),
            reason: "contains a control character",
        });
    }
    Ok(text)
}

/// The variables in `CAPTURED` and `context::AGENT_DIR_VARS` that are set, non-empty and valid
/// UTF-8 in this process.
fn captured(warnings: &mut Vec<String>) -> BTreeMap<String, String> {
    CAPTURED
        .iter()
        .chain(context::AGENT_DIR_VARS.iter())
        .filter_map(|name| {
            let value = env::var_os(name).filter(|value| !value.is_empty())?;
            match value.into_string() {
                Ok(value) => Some(((*name).to_owned(), value)),
                Err(_) => {
                    warnings.push(format!(
                        "{name} is not valid UTF-8, so the background job won't have it"
                    ));
                    None
                }
            }
        })
        .collect()
}

/// Runs a scheduler command (bounded, like a manager query) and returns what it printed.
fn query(context: &Context, program: &str, args: &[&str]) -> Result<Captured, Error> {
    let path = context::which(program).ok_or_else(|| Error::Scheduler {
        command: program.to_owned(),
        detail: "it is not on PATH".to_owned(),
    })?;
    let invocation = Invocation::new(&path, args.iter().copied());
    process::query(&invocation, &context.containment).map_err(|error| Error::Scheduler {
        command: invocation.display(),
        detail: error.to_string(),
    })
}

/// A scheduler command that must succeed.
fn required(context: &Context, program: &str, args: &[&str]) -> Result<Captured, Error> {
    let captured = query(context, program, args)?;
    if captured.success() {
        Ok(captured)
    } else {
        Err(Error::Scheduler {
            command: format!("{program} {}", args.join(" ")),
            detail: format!("{}: {}", captured.status(), captured.stderr.trim()),
        })
    }
}

/// Writes a scheduler file the user owns, through a temporary file renamed into place.
fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io("create", parent))?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".loadout-tmp");
    let temp = PathBuf::from(temp);
    fs::write(&temp, bytes).map_err(io("write", &temp))?;
    fs::rename(&temp, path).map_err(io("replace", path))
}

/// Removes a scheduler file loadout wrote, if it is there.
fn remove(path: &Path) -> Result<(), Error> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result.map_err(io("remove", path)),
    }
}

fn io(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io {
        action,
        path,
        source,
    }
}

/// XML text: the five characters XML gives meaning escaped. Used by the plist and the task.
fn xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded(state: &Path) -> Recorded {
        Recorded {
            version: VERSION,
            exe: PathBuf::from("/opt/loadout/bin/loadout"),
            interval_minutes: 60,
            installed: 1,
            home: PathBuf::from("/home/u"),
            config: PathBuf::from("/home/u/.config"),
            state: state.to_path_buf(),
            env: BTreeMap::from([("LOADOUT_STATE_DIR".to_owned(), state.display().to_string())]),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_record_round_trips_and_rebuilds_the_same_context() {
        let state = PathBuf::from("/srv/loadout-state");
        let record = recorded(&state);
        let text = toml::to_string(&record).unwrap();
        assert_eq!(parse(text.as_bytes()).unwrap(), record);
        let context = record
            .context(Os::Linux, Path::new("service.toml"))
            .unwrap();
        assert_eq!(context.state, state);
        assert_eq!(context.home, PathBuf::from("/home/u"));
        assert!(!context.isolated);
    }

    #[cfg(unix)]
    #[test]
    fn a_record_for_another_environment_is_refused() {
        let mut record = recorded(Path::new("/srv/loadout-state"));
        record.env.clear();
        let error = record
            .context(Os::Linux, Path::new("service.toml"))
            .unwrap_err();
        assert!(
            matches!(
                error,
                Error::Moved {
                    what: "state dir",
                    ..
                }
            ),
            "{error}"
        );
        assert!(error.is_unhealthy());
    }

    #[test]
    fn unknown_fields_and_other_versions_are_refused() {
        let text = toml::to_string(&recorded(Path::new("/s"))).unwrap();
        assert!(parse(format!("{text}token = \"x\"\n").as_bytes()).is_err());
        assert!(parse(text.replace("version = 1", "version = 2").as_bytes()).is_err());
        assert!(parse(b"\xff").is_err());
    }

    #[test]
    fn paths_with_control_characters_or_invalid_text_are_refused() {
        assert!(safe(Path::new("/a b/c%d$e\"f&g<h")).is_ok());
        for bad in ["/a\nb", "/a\0b", "/a\rb", "/a\u{7}b"] {
            let error = safe(Path::new(bad)).unwrap_err();
            assert!(error.is_unhealthy(), "{bad:?}: {error}");
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let path = Path::new(std::ffi::OsStr::from_bytes(b"/a\xffb"));
            assert!(safe(path).is_err());
        }
    }

    #[test]
    fn xml_escapes_every_special_character() {
        assert_eq!(xml(r#"a&b<c>d"e'f"#), "a&amp;b&lt;c&gt;d&quot;e&apos;f");
    }

    #[test]
    fn an_install_under_isolation_is_refused_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let context = Context::resolve(
            Os::current().unwrap(),
            |name| match name {
                "LOADOUT_HOME" => Some(dir.path().join("home").into_os_string()),
                "LOADOUT_STATE_DIR" => Some(state.clone().into_os_string()),
                _ => None,
            },
            None,
        )
        .unwrap();
        assert!(matches!(install(&context, 60), Err(Error::Isolated)));
        assert!(matches!(uninstall(&context), Err(Error::Isolated)));
        assert!(!state.exists(), "nothing may be written");
        let status = status(&context).unwrap();
        assert!(status.scheduler.is_none(), "the scheduler is never asked");
        assert!(!status.installed());
    }
}
