//! A program installed from a GitHub release asset. Plan looks only at the install path, so it
//! stays offline; apply picks the newest matching release, downloads the asset, checks it
//! against the sha256 GitHub publishes for it, and puts it in place. The program updates itself
//! from then on: loadout never looks at a newer release.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::manifest::{AssetKind, GithubRelease};
use crate::platform::{Arch, Os};

/// An asset larger than this is refused rather than downloaded.
const ASSET_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
/// The release list is one page of the newest releases.
const LIST_LIMIT: u64 = 16 * 1024 * 1024;
const PER_PAGE: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Kind {
    /// `~/.local/bin/<id>`, with a launcher in `~/.local/share/applications`.
    AppImage,
    /// Unpacked to `~/.local/share/loadout/github/<id>`, `bin` linked from `~/.local/bin`.
    Archive { bin: String },
    /// `<app>.app` from a zip, moved to `~/Applications`.
    App { name: String },
}

/// One program's release, resolved for this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    id: String,
    repo: String,
    tag: Option<String>,
    prerelease: bool,
    asset: String,
    kind: Kind,
    home: PathBuf,
}

/// The release asset apply chose.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Asset {
    pub(super) tag: String,
    pub(super) name: String,
    pub(super) url: String,
    pub(super) sha256: String,
}

impl Target {
    /// `Err` is why the release doesn't apply to this machine.
    pub(super) fn new(
        id: &str,
        release: &GithubRelease,
        os: Os,
        arch: Arch,
        home: &Path,
    ) -> Result<Self, String> {
        let os_name = match os {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Windows => return Err("github releases install on linux and macos only".to_owned()),
        };
        let platform = format!("{os_name}-{}", arch.name());
        let asset = release.asset.get(&platform).ok_or_else(|| {
            let declared: Vec<&str> = release.asset.keys().map(String::as_str).collect();
            format!(
                "its github release has no asset for {platform} (declared: {})",
                declared.join(", ")
            )
        })?;
        let kind = match AssetKind::of(asset) {
            Some(AssetKind::AppImage) => Kind::AppImage,
            Some(AssetKind::Zip) => Kind::App {
                name: release.app.clone().ok_or("a .zip asset needs app")?,
            },
            Some(AssetKind::TarGz) => Kind::Archive {
                bin: release.bin.clone().ok_or("a .tar.gz asset needs bin")?,
            },
            None => return Err(format!("{asset:?} is not an .AppImage, .tar.gz, or .zip")),
        };
        Ok(Self {
            id: id.to_owned(),
            repo: release.repo.clone(),
            tag: release.tag.clone(),
            prerelease: release.prerelease,
            asset: asset.clone(),
            kind,
            home: home.to_path_buf(),
        })
    }

    /// What plan and the journal call this install: the repository and where it lands, so two
    /// programs from one repository stay apart.
    pub(super) fn package_id(&self) -> String {
        let place = match &self.kind {
            Kind::AppImage => self.id.clone(),
            Kind::Archive { bin } => bin.clone(),
            Kind::App { name } => format!("{name}.app"),
        };
        format!("{}:{place}", self.repo)
    }

    fn bin_dir(&self) -> PathBuf {
        self.home.join(".local/bin")
    }

    fn unpacked(&self) -> PathBuf {
        self.home.join(".local/share/loadout/github").join(&self.id)
    }

    /// Installed when its install path exists. An app counts in `/Applications` too, where a
    /// manual or cask install puts it.
    pub(super) fn installed(&self) -> bool {
        match &self.kind {
            Kind::AppImage => self.bin_dir().join(&self.id).is_file(),
            Kind::Archive { bin } => self.bin_dir().join(bin).is_file(),
            Kind::App { name } => {
                let app = format!("{name}.app");
                self.home.join("Applications").join(&app).is_dir()
                    || Path::new("/Applications").join(&app).is_dir()
            }
        }
    }

    pub(super) fn find(&self) -> Result<Asset, String> {
        let url = format!(
            "https://api.github.com/repos/{}/releases?per_page={PER_PAGE}",
            self.repo
        );
        let response = agent()
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|error| format!("{url}: {error}"))?;
        let mut text = String::new();
        response
            .into_body()
            .into_reader()
            .take(LIST_LIMIT)
            .read_to_string(&mut text)
            .map_err(|error| format!("{url}: {error}"))?;
        let releases: Vec<Release> =
            serde_json::from_str(&text).map_err(|error| format!("{url}: {error}"))?;
        pick(&releases, self.tag.as_deref(), self.prerelease, &self.asset).ok_or_else(|| {
            format!(
                "no {}release of {} has an asset matching {:?}{}",
                if self.prerelease { "" } else { "stable " },
                self.repo,
                self.asset,
                self.tag
                    .as_ref()
                    .map(|tag| format!(" under a tag matching {tag:?}"))
                    .unwrap_or_default()
            )
        })
    }

    /// Downloads `asset`, checks it, and puts it in place. `unzip` runs `ditto` for an app, so
    /// the caller's containment covers it. `cancelled` is asked before each step that writes.
    pub(super) fn install(
        &self,
        asset: &Asset,
        unzip: &dyn Fn(&Path, &Path) -> Result<(), String>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String> {
        let staging = self.home.join(".local/share/loadout/github").join(format!(
            ".staging-{}-{}",
            self.id,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).map_err(|error| place_error(&staging, &error))?;
        let result = (|| {
            let file = staging.join(&asset.name);
            download(asset, &file)?;
            if cancelled() {
                return Err("the run was cancelled before the download was put in place".to_owned());
            }
            match &self.kind {
                Kind::AppImage => self.place_appimage(&file),
                Kind::Archive { bin } => self.place_archive(&file, &staging.join("unpacked"), bin),
                Kind::App { name } => {
                    let unpacked = staging.join("unpacked");
                    unzip(&file, &unpacked)?;
                    self.place_app(&unpacked, name)
                }
            }
        })();
        let _ = fs::remove_dir_all(&staging);
        result
    }

    fn place_appimage(&self, file: &Path) -> Result<(), String> {
        let bin = self.bin_dir();
        fs::create_dir_all(&bin).map_err(|error| place_error(&bin, &error))?;
        let target = bin.join(&self.id);
        executable(file)?;
        replace(file, &target)?;
        let applications = self.home.join(".local/share/applications");
        fs::create_dir_all(&applications).map_err(|error| place_error(&applications, &error))?;
        let launcher = applications.join(format!("loadout-{}.desktop", self.id));
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName={}\nExec=\"{}\" %U\nTerminal=false\n",
            self.id,
            target.display()
        );
        fs::write(&launcher, entry).map_err(|error| place_error(&launcher, &error))
    }

    fn place_archive(&self, file: &Path, unpacked: &Path, bin: &str) -> Result<(), String> {
        unpack_tar_gz(file, unpacked)?;
        let root = single_dir(unpacked).unwrap_or_else(|| unpacked.to_path_buf());
        let inside = root.join(bin);
        if !inside.is_file() {
            return Err(format!("{} has no {bin:?} at its top level", self.asset));
        }
        executable(&inside)?;
        let dest = self.unpacked();
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|error| place_error(parent, &error))?;
        }
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|error| place_error(&dest, &error))?;
        }
        fs::rename(&root, &dest).map_err(|error| place_error(&dest, &error))?;
        let bin_dir = self.bin_dir();
        fs::create_dir_all(&bin_dir).map_err(|error| place_error(&bin_dir, &error))?;
        let link = bin_dir.join(bin);
        if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link).map_err(|error| place_error(&link, &error))?;
        }
        symlink(&dest.join(bin), &link)
    }

    fn place_app(&self, unpacked: &Path, name: &str) -> Result<(), String> {
        let app = format!("{name}.app");
        let found = unpacked.join(&app);
        if !found.is_dir() {
            return Err(format!("{} has no {app:?} at its top level", self.asset));
        }
        let applications = self.home.join("Applications");
        fs::create_dir_all(&applications).map_err(|error| place_error(&applications, &error))?;
        let target = applications.join(&app);
        if target.exists() {
            fs::remove_dir_all(&target).map_err(|error| place_error(&target, &error))?;
        }
        fs::rename(&found, &target).map_err(|error| place_error(&target, &error))
    }
}

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(serde::Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

/// The newest release, in GitHub's order (newest first), that is published, allowed, tagged to
/// match, and has an asset matching the pattern.
fn pick(releases: &[Release], tag: Option<&str>, prerelease: bool, pattern: &str) -> Option<Asset> {
    releases
        .iter()
        .filter(|release| !release.draft && (prerelease || !release.prerelease))
        .filter(|release| tag.is_none_or(|tag| matches(tag, &release.tag_name)))
        .find_map(|release| {
            let asset = release
                .assets
                .iter()
                .find(|asset| matches(pattern, &asset.name))?;
            Some(Asset {
                tag: release.tag_name.clone(),
                name: asset.name.clone(),
                url: asset.browser_download_url.clone(),
                sha256: asset.digest.clone().unwrap_or_default(),
            })
        })
}

/// `*` matches any run of characters; everything else matches itself.
pub(super) fn matches(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    let Some((last, middle)) = parts.split_last() else {
        return rest.is_empty();
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .proxy(ureq::Proxy::try_from_env())
        .max_redirects(5)
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_global(Some(Duration::from_mins(30)))
        .user_agent(format!("loadout/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// GitHub publishes `sha256:<hex>` for every asset; one without it is refused unverified.
fn download(asset: &Asset, file: &Path) -> Result<(), String> {
    let expected = asset
        .sha256
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64)
        .ok_or_else(|| {
            format!(
                "{} in {} has no sha256 digest from GitHub, so loadout won't install it unverified",
                asset.name, asset.tag
            )
        })?;
    let response = agent()
        .get(&asset.url)
        .call()
        .map_err(|error| format!("{}: {error}", asset.url))?;
    let mut reader = response.into_body().into_reader().take(ASSET_LIMIT + 1);
    let mut out = fs::File::create(file).map_err(|error| place_error(file, &error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 1 << 16];
    let mut total: u64 = 0;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", asset.url))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > ASSET_LIMIT {
            return Err(format!("{} is larger than {ASSET_LIMIT} bytes", asset.name));
        }
        hasher.update(&buffer[..read]);
        out.write_all(&buffer[..read])
            .map_err(|error| place_error(file, &error))?;
    }
    out.sync_all().map_err(|error| place_error(file, &error))?;
    let actual = hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut out, byte| {
            write!(out, "{byte:02x}").expect("writing to a String never fails");
            out
        });
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "{} has sha256 {actual}, not the {expected} GitHub published",
            asset.name
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn unpack_tar_gz(file: &Path, dest: &Path) -> Result<(), String> {
    let reader = fs::File::open(file).map_err(|error| place_error(file, &error))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(reader));
    archive.set_preserve_permissions(true);
    archive
        .unpack(dest)
        .map_err(|error| format!("cannot unpack {}: {error}", file.display()))
}

#[cfg(not(unix))]
fn unpack_tar_gz(file: &Path, _dest: &Path) -> Result<(), String> {
    Err(format!("cannot unpack {} on this OS", file.display()))
}

/// The one directory an archive holds at its top level, if that is all it holds.
fn single_dir(dir: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(dir).ok()?.flatten();
    let only = entries.next()?;
    (entries.next().is_none() && only.file_type().ok()?.is_dir()).then(|| only.path())
}

#[cfg(unix)]
fn executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|error| place_error(path, &error))
}

#[cfg(not(unix))]
fn executable(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link).map_err(|error| place_error(link, &error))
}

#[cfg(not(unix))]
fn symlink(_target: &Path, link: &Path) -> Result<(), String> {
    Err(format!("cannot link {} on this OS", link.display()))
}

fn replace(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to).map_err(|error| place_error(to, &error))
}

fn place_error(path: &Path, error: &io::Error) -> String {
    format!("{}: {error}", path.display())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn release(asset: &[(&str, &str)], bin: Option<&str>, app: Option<&str>) -> GithubRelease {
        GithubRelease {
            repo: "owner/tool".to_owned(),
            asset: asset
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            tag: Some("v*-nightly.*".to_owned()),
            prerelease: true,
            bin: bin.map(str::to_owned),
            app: app.map(str::to_owned),
        }
    }

    fn listed(tag: &str, prerelease: bool, assets: &[&str]) -> Release {
        Release {
            tag_name: tag.to_owned(),
            draft: false,
            prerelease,
            assets: assets
                .iter()
                .map(|name| ReleaseAsset {
                    name: (*name).to_owned(),
                    browser_download_url: format!("https://example.com/{name}"),
                    digest: Some(format!("sha256:{}", "a".repeat(64))),
                })
                .collect(),
        }
    }

    #[test]
    fn star_matches_any_run_of_text() {
        assert!(matches(
            "T3-Code-*-x86_64.AppImage",
            "T3-Code-0.1.6-nightly.1-x86_64.AppImage"
        ));
        assert!(matches("v*-nightly.*", "v0.1.6-nightly.20261029.3892"));
        assert!(!matches("v*-nightly.*", "v0.1.5"));
        assert!(!matches(
            "T3-Code-*-x86_64.AppImage",
            "T3-Code-0.1.6-arm64.AppImage"
        ));
        assert!(matches("exact", "exact"));
        assert!(!matches("exact", "exactly"));
        assert!(matches("a*b*c", "abc"));
        assert!(!matches("ab*ba", "aba"));
    }

    #[test]
    fn the_newest_allowed_release_with_a_matching_asset_wins() {
        let releases = [
            listed("android-beta-735", true, &["app.apk"]),
            listed(
                "v0.1.6-nightly.2",
                true,
                &["t3-0.1.6-nightly.2-linux-x64.tar.gz"],
            ),
            listed("v0.1.5", false, &["t3-0.1.5-linux-x64.tar.gz"]),
            listed(
                "v0.1.5-nightly.1",
                true,
                &["t3-0.1.5-nightly.1-linux-x64.tar.gz"],
            ),
        ];
        let pattern = "t3-*-linux-x64.tar.gz";
        let nightly = pick(&releases, Some("v*-nightly.*"), true, pattern).unwrap();
        assert_eq!(nightly.tag, "v0.1.6-nightly.2");
        assert_eq!(nightly.name, "t3-0.1.6-nightly.2-linux-x64.tar.gz");
        assert_eq!(pick(&releases, None, false, pattern).unwrap().tag, "v0.1.5");
        assert_eq!(pick(&releases, None, false, "*.dmg"), None);
    }

    #[test]
    fn each_platform_takes_its_own_asset_and_install_path() {
        let home = Path::new("/h");
        let spec = release(
            &[
                ("linux-x86_64", "T3-Code-*-x86_64.AppImage"),
                ("macos-aarch64", "T3-Code-*-arm64.zip"),
            ],
            None,
            Some("T3 Code (Nightly)"),
        );
        let linux = Target::new("t3-code", &spec, Os::Linux, Arch::X86_64, home).unwrap();
        assert_eq!(linux.kind, Kind::AppImage);
        assert_eq!(linux.package_id(), "owner/tool:t3-code");
        let mac = Target::new("t3-code", &spec, Os::Macos, Arch::Aarch64, home).unwrap();
        assert_eq!(
            mac.kind,
            Kind::App {
                name: "T3 Code (Nightly)".to_owned()
            }
        );
        assert_eq!(mac.package_id(), "owner/tool:T3 Code (Nightly).app");
        let missing = Target::new("t3-code", &spec, Os::Linux, Arch::Aarch64, home).unwrap_err();
        assert!(missing.contains("no asset for linux-aarch64"), "{missing}");
        assert!(Target::new("t3-code", &spec, Os::Windows, Arch::X86_64, home).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_archive_unpacks_and_links_its_bin_and_a_bad_digest_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let spec = release(&[("linux-x86_64", "tool-*.tar.gz")], Some("tool"), None);
        let target = Target::new("tool", &spec, Os::Linux, Arch::X86_64, &home).unwrap();
        assert!(!target.installed());

        let archive = dir.path().join("tool-1.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            fs::File::create(&archive).unwrap(),
            flate2::Compression::fast(),
        ));
        let body = b"#!/bin/sh\necho hi\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "tool-1/tool", &body[..])
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();

        let unpacked = dir.path().join("unpacked");
        target.place_archive(&archive, &unpacked, "tool").unwrap();
        assert!(target.installed());
        let link = home.join(".local/bin/tool");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            home.join(".local/share/loadout/github/tool/tool")
        );

        let unverified = Asset {
            tag: "v1".to_owned(),
            name: "tool-1.tar.gz".to_owned(),
            url: "https://example.invalid/tool-1.tar.gz".to_owned(),
            sha256: String::new(),
        };
        let error = download(&unverified, &dir.path().join("x")).unwrap_err();
        assert!(error.contains("no sha256 digest"), "{error}");
    }
}
