//! Self-update (U9). `update` (U8) calls [`latest`] after its apply when the resolved
//! `[update] cli` is true, and [`install`] when that finds a newer release.
//!
//! Only a binary the install scripts put in place is ever replaced: the install record next to
//! it must parse exactly and bind this platform's installer, this build's target, the running
//! executable's canonical path, and its current SHA-256 (see `docs/releasing.md`). Anything
//! else, such as `cargo install`, a package manager, or a hand copy, is reported and left alone.
//! Under `LOADOUT_HOME` isolation nothing is replaced either, because the binary lives outside
//! the isolated home.

mod archive;
mod download;
mod publish;
mod record;

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;
use sha2::{Digest, Sha256};

use crate::context::Context;
use crate::diagnostic::{Check, CheckStatus};
use crate::process::{self, Containment, Invocation};

use download::{Latest, Source};
use record::Record;

/// The public repository releases come from. No token is needed to read it.
pub const REPOSITORY: &str = "dan-myles/loadout";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "self-update is off under LOADOUT_HOME isolation: the installed CLI is outside the isolated home"
    )]
    Isolated,
    #[error("self-update leaves this CLI alone: {reason}")]
    Unmanaged { reason: String },
    #[error("no loadout release is published yet")]
    NoRelease,
    #[error("cannot download the release: {detail}")]
    Download { detail: String },
    #[error("the release failed verification, so nothing was replaced: {detail}")]
    Verify { detail: String },
    #[error(
        "the installed CLI changed after loadout checked it, so nothing was replaced: {detail}"
    )]
    Changed { detail: String },
    #[error(
        "another install or self-update is changing {} right now, so this one replaced nothing",
        dir.display()
    )]
    Busy { dir: PathBuf },
    #[error("cannot {action} {}: {source}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("self-update did not finish: {detail}")]
    Publish { detail: String, restored: bool },
}

impl Error {
    /// Nothing was or will be replaced, by design or because another writer is busy with it.
    /// `update` reports these and carries on; every other error is a failed self-update (exit 1).
    pub fn skipped(&self) -> bool {
        matches!(
            self,
            Error::Isolated | Error::Unmanaged { .. } | Error::NoRelease | Error::Busy { .. }
        )
    }
}

/// A strictly newer release than the running CLI, for an installer-managed binary. Only
/// [`latest`] makes one.
#[derive(Debug, Clone)]
pub struct Release {
    tag: String,
    version: Version,
    version_text: String,
    current: Version,
    current_text: String,
    target: &'static str,
    source: Source,
    install: Install,
    containment: Containment,
}

impl Release {
    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn version(&self) -> &str {
        &self.version_text
    }

    pub fn current(&self) -> &str {
        &self.current_text
    }
}

/// The installer-managed binaries this process runs from, as their record describes them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Install {
    record: Record,
    dir: PathBuf,
    exe: PathBuf,
    background: Option<PathBuf>,
}

/// Whether a strictly newer release than the running CLI is published. `Ok(None)` means the
/// running CLI is the newest release (or newer), so none is offered. Reads nothing but the
/// running binary, its record, and one HTTPS redirect.
pub fn latest(context: &Context) -> Result<Option<Release>, Error> {
    let install = managed(context)?;
    let target = target().ok_or_else(not_a_release_build)?;
    find(
        &Source::github(),
        install,
        target,
        running_version(),
        context.containment.clone(),
    )
}

/// Replaces the installed binaries with `release`'s, then rewrites the record. The archive and
/// its `.sha256` sidecar are downloaded over HTTPS only, the sidecar must be exact and match,
/// and every binary is read from memory. Then, holding the install folder's lock, each binary is
/// staged next to its destination and must report the release's version, and the installed
/// binaries and record must still be exactly the ones [`latest`] checked, before anything is
/// replaced. So a release found before another install or self-update finished never overwrites
/// that newer install. Interrupted publication fails closed (see `publish`).
pub fn install(release: &Release) -> Result<(), Error> {
    unchanged(release)?;
    replace(release)
}

/// The installed binaries and record are still the ones `release` was found for.
fn unchanged(release: &Release) -> Result<(), Error> {
    let now = installed(
        &release.install.exe,
        cfg!(windows),
        release.target,
        &release.current,
    )
    .map_err(|detail| Error::Changed { detail })?;
    if now != release.install {
        return Err(Error::Changed {
            detail: "its install record changed".to_owned(),
        });
    }
    Ok(())
}

/// Doctor: how the CLI was installed, and so whether self-update may replace it.
pub fn checks(context: &Context) -> Vec<Check> {
    let (status, detail) = match managed(context) {
        Ok(install) => {
            let launcher = match (&install.background, cfg!(windows)) {
                (Some(_), _) | (None, false) => String::new(),
                (None, true) => {
                    "; the next self-update also installs the background launcher".to_owned()
                }
            };
            (
                CheckStatus::Pass,
                format!(
                    "installed by {} at {}; self-update replaces it when [update] cli = true{launcher}",
                    install.record.installer,
                    install.exe.display()
                ),
            )
        }
        Err(Error::Isolated) => (
            CheckStatus::Info,
            "self-update is off under LOADOUT_HOME isolation".to_owned(),
        ),
        Err(Error::Unmanaged { reason }) if !record_exists() => (
            CheckStatus::Info,
            format!("{reason}; loadout never replaces it, so update it the way you installed it"),
        ),
        Err(error) => (
            CheckStatus::Warn,
            format!(
                "{error}; reinstall with install.sh or install.ps1 to turn self-update back on"
            ),
        ),
    };
    vec![Check::new("cli install", status, detail)]
}

fn managed(context: &Context) -> Result<Install, Error> {
    if context.isolated {
        return Err(Error::Isolated);
    }
    let target = target().ok_or_else(not_a_release_build)?;
    let exe = std::env::current_exe().map_err(|source| Error::Io {
        action: "find",
        path: PathBuf::from("the running loadout binary"),
        source,
    })?;
    installed(&exe, cfg!(windows), target, &running_version())
        .map_err(|reason| Error::Unmanaged { reason })
}

fn not_a_release_build() -> Error {
    Error::Unmanaged {
        reason: "this build is not one of the release targets".to_owned(),
    }
}

fn record_exists() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| exe.parent().map(|dir| dir.join(record::FILE)))
        .is_some_and(|path| fs::symlink_metadata(path).is_ok())
}

fn running_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is semver")
}

/// The release target this binary was built for, or `None` for any other build. Linux release
/// builds are musl, so a glibc build (for example from `cargo install`) is never managed.
fn target() -> Option<&'static str> {
    if cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_env = "musl"
    )) {
        Some("x86_64-unknown-linux-musl")
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "linux",
        target_env = "musl"
    )) {
        Some("aarch64-unknown-linux-musl")
    } else if cfg!(all(target_arch = "x86_64", target_os = "macos")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(
        target_arch = "x86_64",
        target_os = "windows",
        target_env = "msvc"
    )) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(
        target_arch = "aarch64",
        target_os = "windows",
        target_env = "msvc"
    )) {
        Some("aarch64-pc-windows-msvc")
    } else {
        None
    }
}

fn asset(target: &str, windows: bool) -> String {
    format!(
        "loadout-{target}.{}",
        if windows { "zip" } else { "tar.gz" }
    )
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn sha256_file(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| sha256(&bytes))
        .map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))
}

/// Reads and binds the install record next to `exe`. Every reason it isn't installer-managed is
/// returned as text for doctor and the `Unmanaged` report.
fn installed(
    exe: &Path,
    windows: bool,
    target: &str,
    running: &Version,
) -> Result<Install, String> {
    let exe = canonical(exe)?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no folder", exe.display()))?
        .to_path_buf();
    let path = dir.join(record::FILE);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(format!("{} is not a regular file", path.display())),
        Err(_) => {
            return Err(format!(
                "there is no install record {}, so it wasn't installed by install.sh or install.ps1",
                path.display()
            ));
        }
    }
    let bytes =
        fs::read(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let record = record::parse(&bytes).map_err(|problem| {
        format!(
            "{} is not a valid install record: {problem}",
            path.display()
        )
    })?;
    let installer = if windows { "install.ps1" } else { "install.sh" };
    if record.installer != installer {
        return Err(format!(
            "{} was written by {}, not {installer}",
            path.display(),
            record.installer
        ));
    }
    if record.repository != REPOSITORY {
        return Err(format!("{} names another repository", path.display()));
    }
    if record.target != target {
        return Err(format!(
            "{} is for {}, but this build is {target}",
            path.display(),
            record.target
        ));
    }
    if canonical(Path::new(&record.binary))? != exe {
        return Err(format!("{} describes another binary path", path.display()));
    }
    if sha256_file(&exe)? != record.binary_sha256 {
        return Err(format!("{} changed after it was installed", exe.display()));
    }
    if Version::parse(&record.version).ok().as_ref() != Some(running) {
        return Err(format!(
            "{} names version {}, but {running} is running",
            path.display(),
            record.version
        ));
    }
    let launcher = dir.join(archive::expected(true)[1]);
    let background = match (&record.background, windows) {
        (None, false) => None,
        (Some(_), false) => return Err(format!("{} is a Windows record", path.display())),
        // A schema-1 install had no launcher, so anything at its path came from somewhere else.
        // The upgrade to schema 2 installs the launcher only where nothing is, as install.ps1 does.
        (None, true) => match fs::symlink_metadata(&launcher) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Ok(_) => {
                return Err(format!(
                    "{} is not in the schema 1 record {}",
                    launcher.display(),
                    path.display()
                ));
            }
            Err(error) => {
                return Err(format!("cannot inspect {}: {error}", launcher.display()));
            }
        },
        (Some((recorded, sha256)), true) => {
            // The path itself, not whatever a link there points to, must be a regular file.
            if !fs::symlink_metadata(&launcher).is_ok_and(|metadata| metadata.file_type().is_file())
            {
                return Err(format!("{} is not a regular file", launcher.display()));
            }
            if canonical(Path::new(recorded))? != canonical(&launcher)? {
                return Err(format!(
                    "{} names another background launcher",
                    path.display()
                ));
            }
            if sha256_file(&launcher)? != *sha256 {
                return Err(format!(
                    "{} changed after it was installed",
                    launcher.display()
                ));
            }
            Some(launcher)
        }
    };
    Ok(Install {
        record,
        dir,
        exe,
        background,
    })
}

fn find(
    source: &Source,
    install: Install,
    target: &'static str,
    current: Version,
    containment: Containment,
) -> Result<Option<Release>, Error> {
    let tag = match source
        .latest()
        .map_err(|detail| Error::Download { detail })?
    {
        Latest::None => return Err(Error::NoRelease),
        Latest::Tag(tag) => tag,
    };
    let version = tag
        .strip_prefix('v')
        .and_then(|version| Version::parse(version).ok())
        .filter(|version| version.pre.is_empty())
        .ok_or_else(|| Error::Verify {
            detail: format!("the latest release tag {tag:?} is not vX.Y.Z"),
        })?;
    if version <= current {
        return Ok(None);
    }
    Ok(Some(Release {
        tag,
        version_text: version.to_string(),
        version,
        current_text: current.to_string(),
        current,
        target,
        source: source.clone(),
        install,
        containment,
    }))
}

fn replace(release: &Release) -> Result<(), Error> {
    let windows = cfg!(windows);
    let install = &release.install;
    let asset = asset(release.target, windows);
    let url = release.source.asset_url(&release.tag, &asset);
    let unreachable = |detail| Error::Download { detail };
    let archive = release.source.archive(&url).map_err(unreachable)?;
    let sidecar = release
        .source
        .sidecar(&format!("{url}.sha256"))
        .map_err(unreachable)?;
    let verify = |detail| Error::Verify { detail };
    let want = download::sidecar_digest(&sidecar, &asset).map_err(verify)?;
    let got = sha256(&archive);
    if got != want {
        return Err(verify(format!(
            "{asset} has sha256 {got}, but its sidecar says {want}"
        )));
    }
    let binaries = archive::read(&archive, release.target).map_err(verify)?;

    let io_error = |action: &'static str, path: &Path| {
        let path = path.to_path_buf();
        move |source| Error::Io {
            action,
            path,
            source,
        }
    };
    // Held until this function returns, so through staging, the final check, publication, any
    // undo, and the removal of every file this run staged (those are dropped before it).
    let _lock = publish::lock(&install.dir).map_err(|error| match error {
        publish::LockError::Busy => Error::Busy {
            dir: install.dir.clone(),
        },
        publish::LockError::Failed(source) => Error::Io {
            action: "lock",
            path: install.dir.join(publish::LOCK_FILE),
            source,
        },
    })?;
    let suffix = if windows { ".exe" } else { "" };
    let mut staged = Vec::new();
    for binary in &binaries {
        let stem = binary.name.trim_end_matches(".exe");
        let file = publish::stage(&install.dir, stem, suffix, &binary.bytes, true)
            .map_err(io_error("stage", &install.dir.join(binary.name)))?;
        smoke(file.path(), stem, &release.version, &release.containment).map_err(verify)?;
        // Only a launcher the record names is replaced; a schema-1 install's goes where none is.
        let prior = if binary.name == archive::expected(true)[1] && install.background.is_none() {
            publish::Prior::Nothing
        } else {
            publish::Prior::Installed
        };
        staged.push((file, install.dir.join(binary.name), prior));
    }

    let main = &binaries[0];
    let record = Record {
        installer: install.record.installer.clone(),
        repository: REPOSITORY.to_owned(),
        source: url,
        target: release.target.to_owned(),
        version: release.version.to_string(),
        archive_sha256: got,
        binary: install.record.binary.clone(),
        binary_sha256: sha256(&main.bytes),
        background: binaries.get(1).map(|launcher| {
            let path = Path::new(&install.record.binary).with_file_name(launcher.name);
            (path.to_string_lossy().into_owned(), sha256(&launcher.bytes))
        }),
    };
    let text = record::render(&record).map_err(verify)?;
    let record_path = install.dir.join(record::FILE);
    let staged_record = publish::stage(&install.dir, record::FILE, "", text.as_bytes(), false)
        .map_err(io_error("stage", &record_path))?;

    // The last check, under the lock: every writer that takes it has finished, so the files are
    // the ones this run replaces, not a newer install that completed during the download.
    unchanged(release)?;

    // The launcher goes first on Windows: until the record names it, a run that starts in
    // between finds a launcher of the same release next to whichever main binary is in place.
    staged.reverse();
    publish::publish(staged, (staged_record, record_path)).map_err(|failure| Error::Publish {
        detail: failure.detail,
        restored: failure.restored,
    })
}

/// Runs a staged binary's `--version`, bounded and contained like a manager query, and requires
/// exactly `<name> <version>`: `loadout 1.2.3`, or `loadout-background 1.2.3` for the launcher.
fn smoke(
    path: &Path,
    name: &str,
    version: &Version,
    containment: &Containment,
) -> Result<(), String> {
    let invocation = Invocation::new(path, ["--version"]);
    let captured = process::query(&invocation, containment)
        .map_err(|error| format!("cannot run the downloaded {name} --version: {error}"))?;
    let expected = format!("{name} {version}\n");
    if !captured.success() || captured.stdout != expected {
        return Err(format!(
            "the downloaded {name} printed {:?} ({}), not {expected:?}",
            captured.stdout,
            captured.status()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
