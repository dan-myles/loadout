use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::context::Context;
use crate::platform::{Arch, Os};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Platform(#[from] crate::platform::Error),
    #[error("cannot read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("cannot write {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Facts {
    pub name: String,
    pub os: Os,
    pub distro: Option<String>,
    pub arch: Arch,
    pub home: PathBuf,
    /// The profiles (tag layers) chosen on this machine and saved in `machine.toml`.
    pub profiles: Vec<String>,
}

pub fn detect(context: &Context) -> Result<Facts, Error> {
    let os = Os::current()?;
    let distro = match os {
        Os::Linux => distro(Path::new("/etc/os-release")),
        Os::Macos | Os::Windows => None,
    };
    let saved = read(context)?;
    let (name, profiles) = match saved {
        Some(file) => (file.name, file.profiles),
        None => (hostname(), Vec::new()),
    };
    Ok(Facts {
        name,
        os,
        distro,
        arch: Arch::current()?,
        home: context.home.clone(),
        profiles,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameSource {
    Saved,
    Hostname,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    pub name: String,
    pub source: NameSource,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MachineFile {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<String>,
    // Left out when empty, so a machine with no profiles keeps a file older versions can read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    profiles: Vec<String>,
}

/// Saves this machine's name, its loadout's URL, and its chosen profiles in
/// `<state>/machine.toml` (init, U9). Once saved, the name no longer follows the hostname. The
/// state dir must already exist (init takes the state lock first, which creates it). The file is
/// written to a new name in the state dir, synced, and renamed over the old one, so a crash leaves
/// the old file or the new one. Like [`name`], it refuses an existing `machine.toml` that is a
/// link or not a regular file, and it refuses a remote that carries a password, because loadout
/// stores no credentials.
pub fn save(context: &Context, name: &str, remote: &str, profiles: &[String]) -> Result<(), Error> {
    write(
        context,
        MachineFile {
            name: name.to_owned(),
            remote: Some(remote.to_owned()),
            profiles: profiles.to_vec(),
        },
    )
}

/// Replaces this machine's chosen profiles and keeps its saved name and URL. A machine with no
/// saved name gets the hostname's saved with them, as `init` would.
pub fn save_profiles(context: &Context, profiles: &[String]) -> Result<(), Error> {
    let file = match read(context)? {
        Some(file) => MachineFile {
            profiles: profiles.to_vec(),
            ..file
        },
        None => MachineFile {
            name: hostname(),
            remote: None,
            profiles: profiles.to_vec(),
        },
    };
    write(context, file)
}

/// The profiles saved on this machine, in the order they were chosen; none before any are saved.
pub fn profiles(context: &Context) -> Result<Vec<String>, Error> {
    Ok(read(context)?.map(|file| file.profiles).unwrap_or_default())
}

fn write(context: &Context, mut file: MachineFile) -> Result<(), Error> {
    let path = context.machine_file();
    let invalid = |message: &str| Error::Invalid {
        path: path.clone(),
        message: message.to_owned(),
    };
    if !crate::id::is_id(&file.name) {
        return Err(invalid(&format!(
            "machine name {:?} must be lowercase letters, digits, - or _",
            file.name
        )));
    }
    if file.remote.as_deref().is_some_and(has_password) {
        return Err(invalid(
            "the loadout URL carries a password; loadout stores no credentials, so use your Git credential helper or SSH keys",
        ));
    }
    if let Some(problem) = profile_problem(&file.profiles) {
        return Err(invalid(&problem));
    }
    let mut seen = std::collections::BTreeSet::new();
    file.profiles.retain(|profile| seen.insert(profile.clone()));
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Err(invalid("must be a regular file, not a link or folder")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::Read { path, source }),
    }
    let state = context.state.canonicalize().map_err(|source| Error::Read {
        path: context.state.clone(),
        source,
    })?;
    if !state.is_dir() {
        return Err(invalid("the state dir is not a folder"));
    }
    let text =
        toml::to_string(&file).map_err(|error| invalid(&format!("cannot encode it: {error}")))?;
    write_new(&state, &state.join("machine.toml"), text.as_bytes()).map_err(|source| Error::Write {
        path: path.clone(),
        source,
    })
}

fn profile_problem(profiles: &[String]) -> Option<String> {
    profiles
        .iter()
        .find(|profile| !crate::id::is_id(profile))
        .map(|profile| format!("profile {profile:?} must be lowercase letters, digits, - or _"))
}

/// Whether a URL's authority names a password, as in `https://user:token@host/repo`.
pub(crate) fn has_password(remote: &str) -> bool {
    remote
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default())
        .and_then(|authority| authority.rsplit_once('@'))
        .is_some_and(|(userinfo, _)| userinfo.contains(':'))
}

fn write_new(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let temp = dir.join(format!(
        ".machine.toml.new.{:x}{nanos:x}",
        std::process::id()
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&temp)?;
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

pub fn name(context: &Context) -> Result<Name, Error> {
    Ok(match read(context)? {
        Some(file) => Name {
            name: file.name,
            source: NameSource::Saved,
        },
        None => Name {
            name: hostname(),
            source: NameSource::Hostname,
        },
    })
}

fn hostname() -> String {
    from_hostname(&gethostname::gethostname().to_string_lossy())
}

fn read(context: &Context) -> Result<Option<MachineFile>, Error> {
    let path = context.machine_file();
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::Read { path, source }),
        Ok(_) => {}
    }
    let canonical = |path: &Path| {
        path.canonicalize().map_err(|source| Error::Read {
            path: path.to_path_buf(),
            source,
        })
    };
    let real = canonical(&path)?;
    if !real.starts_with(canonical(&context.state)?) {
        return Err(Error::Invalid {
            path,
            message: "resolves outside the state dir, which loadout never reads".to_owned(),
        });
    }
    match fs::metadata(&real) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(Error::Invalid {
                path,
                message: "must be a regular file".to_owned(),
            });
        }
        Err(source) => return Err(Error::Read { path, source }),
    }
    let text = fs::read_to_string(&real).map_err(|source| Error::Read {
        path: path.clone(),
        source,
    })?;
    let file: MachineFile = toml::from_str(&text).map_err(|error| Error::Invalid {
        path: path.clone(),
        message: error.message().to_owned(),
    })?;
    if !crate::id::is_id(&file.name) {
        return Err(Error::Invalid {
            path,
            message: format!(
                "machine name {:?} must be lowercase letters, digits, - or _",
                file.name
            ),
        });
    }
    if let Some(message) = profile_problem(&file.profiles) {
        return Err(Error::Invalid { path, message });
    }
    Ok(Some(file))
}

pub fn from_hostname(hostname: &str) -> String {
    let host = hostname.split('.').next().unwrap_or_default();
    let mapped: String = host
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_matches(|c| c == '-' || c == '_');
    if trimmed.is_empty() {
        "machine".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn distro(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    parse_os_release(&text)
}

fn parse_os_release(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.strip_prefix("ID=")?.trim().trim_matches(['"', '\'']);
        (!value.is_empty()).then(|| value.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostname_becomes_a_valid_machine_name() {
        assert_eq!(from_hostname("Dans-MacBook-Pro.local"), "dans-macbook-pro");
        assert_eq!(from_hostname("DESKTOP 7G2K"), "desktop-7g2k");
        assert_eq!(from_hostname("--"), "machine");
        assert_eq!(from_hostname(""), "machine");
        assert!(crate::id::is_id(&from_hostname("Zoë's PC")));
    }

    #[test]
    fn os_release_id_is_read_unquoted() {
        let text = "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n";
        assert_eq!(parse_os_release(text).as_deref(), Some("ubuntu"));
        assert_eq!(
            parse_os_release("ID=\"opensuse-tumbleweed\"").as_deref(),
            Some("opensuse-tumbleweed")
        );
        assert_eq!(parse_os_release("NAME=x"), None);
    }

    #[test]
    fn saved_name_wins_over_hostname() {
        let dir = tempfile::tempdir().unwrap();
        let context = Context::resolve(
            Os::Linux,
            |name| (name == "LOADOUT_STATE_DIR").then(|| dir.path().as_os_str().to_owned()),
            Some("/home/a".into()),
        )
        .unwrap();
        assert_eq!(name(&context).unwrap().source, NameSource::Hostname);

        fs::write(context.machine_file(), "name = \"work-laptop\"\n").unwrap();
        assert_eq!(
            name(&context).unwrap(),
            Name {
                name: "work-laptop".to_owned(),
                source: NameSource::Saved
            }
        );

        fs::write(context.machine_file(), "name = \"Bad Name\"\n").unwrap();
        assert!(matches!(name(&context), Err(Error::Invalid { .. })));
    }

    #[test]
    fn save_persists_the_name_and_remote_and_refuses_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let context = Context::resolve(
            Os::Linux,
            |name| (name == "LOADOUT_STATE_DIR").then(|| dir.path().as_os_str().to_owned()),
            Some("/home/a".into()),
        )
        .unwrap();
        save(
            &context,
            "work-laptop",
            "git@github.com:me/loadout.git",
            &[],
        )
        .unwrap();
        assert_eq!(
            name(&context).unwrap(),
            Name {
                name: "work-laptop".to_owned(),
                source: NameSource::Saved
            }
        );
        let text = fs::read_to_string(context.machine_file()).unwrap();
        assert!(
            text.contains("remote = \"git@github.com:me/loadout.git\""),
            "{text}"
        );
        save(
            &context,
            "work-laptop",
            "https://github.com/me/loadout.git",
            &[],
        )
        .unwrap();
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file is left"
        );

        let before = fs::read(context.machine_file()).unwrap();
        for (name_value, remote) in [
            ("Bad Name", "https://github.com/me/loadout.git"),
            ("work", "https://me:token@github.com/me/loadout.git"),
        ] {
            assert!(matches!(
                save(&context, name_value, remote, &[]),
                Err(Error::Invalid { .. })
            ));
        }
        assert_eq!(fs::read(context.machine_file()).unwrap(), before);
        assert!(!has_password("https://me@github.com/me/loadout.git"));
        assert!(!has_password("git@github.com:me/loadout.git"));
    }

    #[test]
    fn profiles_are_saved_with_the_name_and_remote_kept() {
        let dir = tempfile::tempdir().unwrap();
        let context = Context::resolve(
            Os::Linux,
            |name| (name == "LOADOUT_STATE_DIR").then(|| dir.path().as_os_str().to_owned()),
            Some("/home/a".into()),
        )
        .unwrap();
        assert!(profiles(&context).unwrap().is_empty());
        save_profiles(&context, &["work".to_owned()]).unwrap();
        assert_eq!(name(&context).unwrap().source, NameSource::Saved);

        save(&context, "box", "https://x/y.git", &[]).unwrap();
        assert!(
            !fs::read_to_string(context.machine_file())
                .unwrap()
                .contains("profiles"),
            "a machine with no profiles keeps the file older versions read"
        );
        let chosen = ["development", "work", "development"].map(str::to_owned);
        save_profiles(&context, &chosen).unwrap();
        assert_eq!(profiles(&context).unwrap(), ["development", "work"]);
        let text = fs::read_to_string(context.machine_file()).unwrap();
        assert!(text.contains("name = \"box\""), "{text}");
        assert!(text.contains("remote = \"https://x/y.git\""), "{text}");

        assert!(matches!(
            save_profiles(&context, &["Bad Profile".to_owned()]),
            Err(Error::Invalid { .. })
        ));
        assert_eq!(fs::read_to_string(context.machine_file()).unwrap(), text);

        fs::write(context.machine_file(), "name = \"box\"\nprofiles = [\"../x\"]\n").unwrap();
        assert!(matches!(profiles(&context), Err(Error::Invalid { .. })));
        assert!(matches!(name(&context), Err(Error::Invalid { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_a_linked_machine_file() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let context = Context::resolve(
            Os::Linux,
            |name| (name == "LOADOUT_STATE_DIR").then(|| dir.path().as_os_str().to_owned()),
            Some("/home/a".into()),
        )
        .unwrap();
        let target = outside.path().join("auth.json");
        fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, context.machine_file()).unwrap();
        assert!(matches!(
            save(&context, "box", "https://x/y.git", &[]),
            Err(Error::Invalid { .. })
        ));
        assert_eq!(fs::read_to_string(&target).unwrap(), "secret");
    }
}
