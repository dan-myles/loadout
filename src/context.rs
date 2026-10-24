use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::platform::Os;

pub const HOME_OVERRIDE: &str = "LOADOUT_HOME";
pub const STATE_OVERRIDE: &str = "LOADOUT_STATE_DIR";
pub const AGENT_DIR_VARS: [&str; 4] = [
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    "GROK_HOME",
    "PI_CODING_AGENT_DIR",
];

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
    pub isolated: bool,
    pub config: PathBuf,
    pub xdg_config: PathBuf,
    pub state: PathBuf,
    pub agent_dirs: BTreeMap<&'static str, PathBuf>,
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

        let isolated = set(HOME_OVERRIDE).is_some();
        let home = match set(HOME_OVERRIDE) {
            Some(value) => absolute(HOME_OVERRIDE, value)?,
            None => default_home.ok_or(Error::NoHome)?,
        };
        let ambient = |name: &str| set(name).filter(|value| !isolated && value.is_absolute());
        let xdg =
            |name: &str, fallback: &[&str]| ambient(name).unwrap_or_else(|| join(&home, fallback));

        let xdg_config = xdg("XDG_CONFIG_HOME", &[".config"]);
        let config = match os {
            Os::Linux => xdg_config.clone(),
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
        let agent_dirs = AGENT_DIR_VARS
            .into_iter()
            .filter_map(|name| ambient(name).map(|value| (name, value)))
            .collect();

        Ok(Self {
            os,
            home,
            isolated,
            config,
            xdg_config,
            state,
            agent_dirs,
        })
    }

    pub fn expand(&self, path: &str) -> Option<PathBuf> {
        let (base, rest) = split_base(path)?;
        if self.os == Os::Windows && windows_problem(rest).is_some() {
            return None;
        }
        let base = match base {
            Base::Home => &self.home,
            Base::Config => &self.config,
        };
        Some(join(base, &rest.split('/').collect::<Vec<_>>()))
    }

    pub fn agent_dir(&self, var: &str) -> Option<&Path> {
        self.agent_dirs.get(var).map(PathBuf::as_path)
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
        && !rest.contains(['\\', '\0'])
        && rest
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    clean.then_some((base, rest))
}

const WINDOWS_DEVICES: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];

pub fn windows_problem(rest: &str) -> Option<String> {
    rest.split('/').find_map(|part| {
        let stem = part.split('.').next().unwrap_or_default().trim_end();
        let numbered = ["COM", "LPT"].iter().any(|prefix| {
            stem.get(..3).is_some_and(|head| head.eq_ignore_ascii_case(prefix))
                && stem.get(3..).is_some_and(|tail| {
                    matches!(tail, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}")
                })
        });
        if let Some(c) = part.chars().find(|c| "<>:\"|?*".contains(*c) || c.is_control()) {
            Some(format!("{part:?} contains {c:?}, which Windows paths cannot hold (drive, stream, or reserved character)"))
        } else if part.ends_with(['.', ' ']) {
            Some(format!("{part:?} ends with a dot or space, which Windows drops"))
        } else if numbered || WINDOWS_DEVICES.iter().any(|device| stem.eq_ignore_ascii_case(device)) {
            Some(format!("{part:?} is a Windows device name"))
        } else {
            None
        }
    })
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
            executable(&candidate).then_some(candidate)
        })
    })
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
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
        assert!(context.isolated);
        assert_eq!(
            context.config,
            PathBuf::from("/tmp/h/Library/Application Support")
        );
        assert_eq!(context.state, PathBuf::from("/tmp/s"));
    }

    #[test]
    fn isolated_home_ignores_ambient_xdg_and_agent_dirs() {
        let ambient = [
            ("XDG_CONFIG_HOME", "/real/.config"),
            ("XDG_STATE_HOME", "/real/.local/state"),
            ("CLAUDE_CONFIG_DIR", "/real/.claude"),
            ("CODEX_HOME", "/real/.codex"),
        ];
        let mut isolated = ambient.to_vec();
        isolated.push(("LOADOUT_HOME", "/sandbox"));
        let context = Context::resolve(Os::Linux, vars(&isolated), Some("/real".into())).unwrap();
        assert_eq!(context.config, PathBuf::from("/sandbox/.config"));
        assert_eq!(context.xdg_config, PathBuf::from("/sandbox/.config"));
        assert_eq!(
            context.state,
            PathBuf::from("/sandbox/.local/state/loadout")
        );
        assert!(context.agent_dirs.is_empty());

        isolated.push(("LOADOUT_STATE_DIR", "/explicit/state"));
        let context = Context::resolve(Os::Linux, vars(&isolated), Some("/real".into())).unwrap();
        assert_eq!(context.state, PathBuf::from("/explicit/state"));

        let context = Context::resolve(Os::Linux, vars(&ambient), Some("/real".into())).unwrap();
        assert!(!context.isolated);
        assert_eq!(context.config, PathBuf::from("/real/.config"));
        assert_eq!(context.state, PathBuf::from("/real/.local/state/loadout"));
        assert_eq!(
            context.agent_dir("CODEX_HOME"),
            Some(Path::new("/real/.codex"))
        );
        assert_eq!(context.agent_dir("GROK_HOME"), None);
    }

    #[test]
    fn empty_home_override_is_not_isolation() {
        let context = Context::resolve(
            Os::Linux,
            vars(&[("LOADOUT_HOME", "")]),
            Some("/real".into()),
        )
        .unwrap();
        assert!(!context.isolated);
        assert_eq!(context.home, PathBuf::from("/real"));
    }

    #[test]
    fn xdg_config_is_dot_config_on_every_os() {
        for os in Os::ALL {
            let context = Context::resolve(os, vars(&[]), Some("/h".into())).unwrap();
            assert_eq!(context.xdg_config, Path::new("/h").join(".config"), "{os}");
        }
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
            "~/a\0b",
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

    #[test]
    fn colons_and_device_names_are_fine_on_posix_but_refused_on_windows() {
        let posix = Context::resolve(Os::Linux, vars(&[]), Some("/home/a".into())).unwrap();
        let windows = Context::resolve(Os::Windows, vars(&[]), Some("/c/u".into())).unwrap();
        for path in [
            "~/C:/Windows/x",
            "~/foo:bar",
            "~/NUL",
            "~/nul.txt",
            "~/com1",
            "~/LPT9.log",
            "~/a?b",
            "~/trailing.",
            "~/space ",
            "{config}/D:/evil",
        ] {
            assert!(posix.expand(path).is_some(), "posix keeps {path}");
            assert_eq!(windows.expand(path), None, "windows refuses {path}");
        }
        for path in [
            "~/.config/nu/config.nu",
            "~/console",
            "~/com10",
            "~/Documents/My Notes.md",
        ] {
            assert!(windows.expand(path).is_some(), "windows keeps {path}");
            assert_eq!(
                windows_problem(path.split_once('/').unwrap().1),
                None,
                "{path}"
            );
        }
    }
}
