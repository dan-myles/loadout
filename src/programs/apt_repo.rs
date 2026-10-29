//! The third-party apt repository an `apt` package comes from: its signing key and its
//! sources.list entry, written before the package is installed.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::manifest::AptRepo;

/// A signing key larger than this is refused rather than read.
const KEY_LIMIT: u64 = 1024 * 1024;
const KEYRINGS: &str = "/etc/apt/keyrings";
const SOURCES: &str = "/etc/apt/sources.list.d";
const ARMOR: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";

/// Where a program's repository lives once added, and the sources.list line that names it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) key: PathBuf,
    pub(super) list: PathBuf,
    pub(super) line: String,
}

/// The repository with `{codename}` filled in. `None` when a field needs the codename and the
/// machine doesn't report one.
pub(super) fn fill(repo: &AptRepo, codename: Option<&str>) -> Option<AptRepo> {
    let fill = |value: &str| match codename {
        _ if !value.contains("{codename}") => Some(value.to_owned()),
        Some(codename) => Some(value.replace("{codename}", codename)),
        None => None,
    };
    Some(AptRepo {
        url: fill(&repo.url)?,
        key: fill(&repo.key)?,
        suite: Some(fill(repo.suite.as_deref().unwrap_or("{codename}"))?),
        components: repo.components.clone(),
    })
}

/// apt reads a keyring by its extension: `.asc` must be ASCII-armored, `.gpg` binary.
pub(super) fn entry(program: &str, repo: &AptRepo, key: &[u8]) -> Entry {
    let extension = if key.trim_ascii_start().starts_with(ARMOR.as_bytes()) {
        "asc"
    } else {
        "gpg"
    };
    let key = PathBuf::from(KEYRINGS).join(format!("loadout-{program}.{extension}"));
    let components = repo
        .components
        .clone()
        .unwrap_or_else(|| vec!["main".to_owned()]);
    let line = format!(
        "deb [signed-by={}] {} {} {}\n",
        key.display(),
        repo.url,
        repo.suite.as_deref().unwrap_or_default(),
        components.join(" ")
    );
    Entry {
        key,
        list: list(program),
        line,
    }
}

/// Whether a source other than `own` already names `url`. apt refuses two entries for one
/// repository with different `signed-by` keys, so a repository the machine already has is left
/// as it is.
pub(super) fn configured(url: &str, own: &Path) -> bool {
    let lists = fs::read_dir(SOURCES)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .chain([PathBuf::from("/etc/apt/sources.list")]);
    lists
        .filter(|path| path != own)
        .filter_map(|path| fs::read_to_string(path).ok())
        .any(|text| names(&text, url))
}

fn names(text: &str, url: &str) -> bool {
    let url = url.trim_end_matches('/');
    text.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .flat_map(str::split_whitespace)
        .any(|word| word.trim_end_matches('/') == url)
}

pub(super) fn list(program: &str) -> PathBuf {
    PathBuf::from(SOURCES).join(format!("loadout-{program}.list"))
}

pub(super) fn codename(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        let value = line
            .strip_prefix("VERSION_CODENAME=")?
            .trim()
            .trim_matches(['"', '\'']);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

pub(super) fn fetch_key(url: &str) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .proxy(ureq::Proxy::try_from_env())
        .max_redirects(5)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_global(Some(Duration::from_mins(2)))
        .user_agent(format!("loadout/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let response = agent
        .get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?;
    let mut key = Vec::new();
    response
        .into_body()
        .into_reader()
        .take(KEY_LIMIT + 1)
        .read_to_end(&mut key)
        .map_err(|error| format!("{url}: {error}"))?;
    if key.len() as u64 > KEY_LIMIT {
        return Err(format!("{url}: the key is larger than {KEY_LIMIT} bytes"));
    }
    if key.is_empty() {
        return Err(format!("{url}: the key is empty"));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(suite: Option<&str>, components: Option<&[&str]>) -> AptRepo {
        AptRepo {
            url: "https://pkgs.example.com/{codename}".to_owned(),
            key: "https://pkgs.example.com/{codename}.gpg".to_owned(),
            suite: suite.map(str::to_owned),
            components: components.map(|c| c.iter().map(|s| (*s).to_owned()).collect()),
        }
    }

    #[test]
    fn codename_fills_every_field_and_is_the_default_suite() {
        let filled = fill(&repo(None, None), Some("jammy")).unwrap();
        assert_eq!(filled.url, "https://pkgs.example.com/jammy");
        assert_eq!(filled.key, "https://pkgs.example.com/jammy.gpg");
        assert_eq!(filled.suite.as_deref(), Some("jammy"));
    }

    #[test]
    fn a_needed_codename_the_machine_lacks_is_refused() {
        assert_eq!(fill(&repo(None, None), None), None);
        let fixed = AptRepo {
            url: "https://a.example".to_owned(),
            key: "https://a.example/k".to_owned(),
            suite: Some("stable".to_owned()),
            components: None,
        };
        assert_eq!(fill(&fixed, None), Some(fixed.clone()));
    }

    #[test]
    fn the_line_signs_with_a_keyring_named_for_the_program() {
        let filled = fill(
            &repo(Some("stable"), Some(&["main", "extra"])),
            Some("jammy"),
        )
        .unwrap();
        let entry = entry("tailscaled", &filled, b"\x99\x01binary");
        assert_eq!(
            entry.key,
            PathBuf::from("/etc/apt/keyrings/loadout-tailscaled.gpg")
        );
        assert_eq!(
            entry.list,
            PathBuf::from("/etc/apt/sources.list.d/loadout-tailscaled.list")
        );
        assert_eq!(
            entry.line,
            "deb [signed-by=/etc/apt/keyrings/loadout-tailscaled.gpg] \
             https://pkgs.example.com/jammy stable main extra\n"
        );
    }

    #[test]
    fn an_armored_key_gets_the_asc_extension() {
        let filled = fill(&repo(None, None), Some("noble")).unwrap();
        let key = format!("\n{ARMOR}\nabc\n");
        let entry = entry("x", &filled, key.as_bytes());
        assert_eq!(entry.key, PathBuf::from("/etc/apt/keyrings/loadout-x.asc"));
        assert!(entry.line.ends_with(" noble main\n"));
    }

    #[test]
    fn a_source_names_a_repository_in_one_line_or_deb822_form() {
        let url = "https://pkgs.example.com/ubuntu";
        let line = "deb [signed-by=/k.gpg] https://pkgs.example.com/ubuntu/ jammy main\n";
        assert!(names(line, url));
        assert!(names(
            "Types: deb\nURIs: https://pkgs.example.com/ubuntu\n",
            url
        ));
        assert!(!names(
            "# deb https://pkgs.example.com/ubuntu jammy main\n",
            url
        ));
        assert!(!names(
            "deb https://pkgs.example.com/ubuntu-ports jammy main\n",
            url
        ));
    }

    #[test]
    fn codename_comes_from_version_codename() {
        let text = "NAME=\"Ubuntu\"\nVERSION_CODENAME=jammy\nUBUNTU_CODENAME=jammy\n";
        assert_eq!(codename(text).as_deref(), Some("jammy"));
        assert_eq!(codename("ID=debian\n"), None);
        assert_eq!(codename("VERSION_CODENAME=\"\"\n"), None);
    }
}
