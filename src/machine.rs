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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Facts {
    pub name: String,
    pub os: Os,
    pub distro: Option<String>,
    pub arch: Arch,
    pub home: PathBuf,
}

pub fn detect(context: &Context) -> Result<Facts, Error> {
    let os = Os::current()?;
    let distro = match os {
        Os::Linux => distro(Path::new("/etc/os-release")),
        Os::Macos | Os::Windows => None,
    };
    Ok(Facts {
        name: name(context)?.name,
        os,
        distro,
        arch: Arch::current()?,
        home: context.home.clone(),
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineFile {
    name: String,
    #[expect(dead_code, reason = "written by init; read by sync")]
    remote: Option<String>,
}

pub fn name(context: &Context) -> Result<Name, Error> {
    let path = context.machine_file();
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Name {
                name: from_hostname(&gethostname::gethostname().to_string_lossy()),
                source: NameSource::Hostname,
            });
        }
        Err(source) => return Err(Error::Read { path, source }),
    };
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
    Ok(Name {
        name: file.name,
        source: NameSource::Saved,
    })
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
}
