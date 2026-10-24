use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::Os;

pub const HOME_OVERRIDE: &str = "LOADOUT_HOME";
pub const STATE_OVERRIDE: &str = "LOADOUT_STATE_DIR";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot find the home directory; set {HOME_OVERRIDE}")]
    NoHome,
    #[error("{name} must be an absolute path, got {value:?}")]
    NotAbsolute { name: &'static str, value: PathBuf },
    #[error(transparent)]
    Platform(#[from] crate::platform::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub os: Os,
    pub home: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
}

impl Context {
    pub fn from_env() -> Result<Self, Error> {
        Self::resolve(Os::current()?, |name| env::var_os(name), env::home_dir())
    }

    pub fn resolve(
        os: Os,
        var: impl Fn(&str) -> Option<OsString>,
        default_home: Option<PathBuf>,
    ) -> Result<Self, Error> {
        let set = |name: &str| {
            var(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let absolute = |name: &'static str, value: PathBuf| {
            if value.is_absolute() {
                Ok(value)
            } else {
                Err(Error::NotAbsolute { name, value })
            }
        };

        let home = match set(HOME_OVERRIDE) {
            Some(value) => absolute(HOME_OVERRIDE, value)?,
            None => default_home.ok_or(Error::NoHome)?,
        };
        let xdg = |name: &str, fallback: &[&str]| {
            set(name)
                .filter(|value| value.is_absolute())
                .unwrap_or_else(|| join(&home, fallback))
        };

        let config = match os {
            Os::Linux => xdg("XDG_CONFIG_HOME", &[".config"]),
            Os::Macos => join(&home, &["Library", "Application Support"]),
            Os::Windows => join(&home, &["AppData", "Roaming"]),
        };
        let state = match set(STATE_OVERRIDE) {
            Some(value) => absolute(STATE_OVERRIDE, value)?,
            None => match os {
                Os::Linux => xdg("XDG_STATE_HOME", &[".local", "state"]).join("loadout"),
                Os::Macos => join(&home, &["Library", "Application Support", "loadout"]),
                Os::Windows => join(&home, &["AppData", "Local", "loadout"]),
            },
        };

        Ok(Self {
            os,
            home,
            config,
            state,
        })
    }

    pub fn expand(&self, path: &str) -> Option<PathBuf> {
        let (base, rest) = split_base(path)?;
        let base = match base {
            Base::Home => &self.home,
            Base::Config => &self.config,
        };
        Some(join(base, &rest.split('/').collect::<Vec<_>>()))
    }

    pub fn machine_file(&self) -> PathBuf {
        self.state.join("machine.toml")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    Home,
    Config,
}

pub fn split_base(path: &str) -> Option<(Base, &str)> {
    let (base, rest) = match path.strip_prefix("~/") {
        Some(rest) => (Base::Home, rest),
        None => (Base::Config, path.strip_prefix("{config}/")?),
    };
    let clean = !rest.is_empty()
        && !rest.contains('\\')
        && rest
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    clean.then_some((base, rest))
}

fn join(base: &Path, parts: &[&str]) -> PathBuf {
    parts
        .iter()
        .fold(base.to_path_buf(), |path, part| path.join(part))
}

pub fn which(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let extensions: Vec<OsString> = if cfg!(windows) {
        env::var_os("PATHEXT")
            .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"))
            .to_string_lossy()
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(OsString::from)
            .collect()
    } else {
        vec![OsString::new()]
    };
    env::split_paths(&path).find_map(|dir| {
        extensions.iter().find_map(|extension| {
            let mut name = OsString::from(program);
            name.push(extension);
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn linux_defaults_follow_xdg_fallbacks() {
        let context = Context::resolve(Os::Linux, vars(&[]), Some("/home/a".into())).unwrap();
        assert_eq!(context.config, PathBuf::from("/home/a/.config"));
        assert_eq!(context.state, PathBuf::from("/home/a/.local/state/loadout"));
    }

    #[test]
    fn linux_honours_absolute_xdg_and_ignores_relative() {
        let context = Context::resolve(
            Os::Linux,
            vars(&[("XDG_CONFIG_HOME", "/x/config"), ("XDG_STATE_HOME", "rel")]),
            Some("/home/a".into()),
        )
        .unwrap();
        assert_eq!(context.config, PathBuf::from("/x/config"));
        assert_eq!(context.state, PathBuf::from("/home/a/.local/state/loadout"));
    }

    #[test]
    fn overrides_replace_home_and_state() {
        let context = Context::resolve(
            Os::Macos,
            vars(&[("LOADOUT_HOME", "/tmp/h"), ("LOADOUT_STATE_DIR", "/tmp/s")]),
            Some("/Users/a".into()),
        )
        .unwrap();
        assert_eq!(context.home, PathBuf::from("/tmp/h"));
        assert_eq!(
            context.config,
            PathBuf::from("/tmp/h/Library/Application Support")
        );
        assert_eq!(context.state, PathBuf::from("/tmp/s"));
    }

    #[test]
    fn windows_dirs_derive_from_home() {
        let context = Context::resolve(Os::Windows, vars(&[]), Some("/c/u".into())).unwrap();
        assert_eq!(
            context.config,
            Path::new("/c/u").join("AppData").join("Roaming")
        );
        assert_eq!(
            context.state,
            Path::new("/c/u")
                .join("AppData")
                .join("Local")
                .join("loadout")
        );
    }

    #[test]
    fn relative_override_is_rejected() {
        let error = Context::resolve(Os::Linux, vars(&[("LOADOUT_HOME", "h")]), None).unwrap_err();
        assert!(matches!(error, Error::NotAbsolute { .. }));
    }

    #[test]
    fn expand_accepts_only_clean_home_and_config_paths() {
        let context = Context::resolve(Os::Linux, vars(&[]), Some("/home/a".into())).unwrap();
        assert_eq!(
            context.expand("~/.config/git/config"),
            Some(PathBuf::from("/home/a/.config/git/config"))
        );
        assert_eq!(
            context.expand("{config}/Code/User/settings.json"),
            Some(PathBuf::from("/home/a/.config/Code/User/settings.json"))
        );
        for bad in [
            "/etc/passwd",
            "~/../x",
            "~/",
            "~/a//b",
            "relative",
            "~/a\\b",
            "{data}/x",
        ] {
            assert_eq!(context.expand(bad), None, "{bad}");
        }
    }
}
