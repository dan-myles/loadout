//! Reading the binaries out of a verified release archive, in memory.
//!
//! Nothing is extracted to disk here: every entry's name and type is checked first, only the
//! expected binaries are read, and each read is bounded. A release archive holds exactly
//! `loadout-<target>/`, the expected binaries, and optionally `README.md` and `LICENSE*`, all as
//! plain names directly inside that folder (see `scripts/release/package.py`).

use std::io::Read;

const MAX_ENTRIES: usize = 64;
/// One binary larger than this is refused rather than read.
const BINARY_LIMIT: u64 = 128 * 1024 * 1024;
/// The sum of the sizes every entry declares; an archive claiming more is refused before any of
/// its content is decompressed.
const DECLARED_LIMIT: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Binary {
    pub(super) name: &'static str,
    pub(super) bytes: Vec<u8>,
}

/// The binaries a release for `windows` (or not) ships, main CLI first. Windows adds the
/// background launcher the scheduled task starts (U8), so the CLI stays a console program.
pub(super) fn expected(windows: bool) -> &'static [&'static str] {
    if windows {
        &["loadout.exe", "loadout-background.exe"]
    } else {
        &["loadout"]
    }
}

/// Where an entry sits in the layout, from its raw name.
#[derive(Debug, PartialEq, Eq)]
enum Place<'a> {
    Folder,
    Leaf(&'a str),
}

fn place<'a>(name: &'a [u8], target: &str) -> Result<Place<'a>, String> {
    let name = std::str::from_utf8(name).map_err(|_| "an entry name is not UTF-8".to_owned())?;
    let folder = format!("loadout-{target}");
    if name == folder || name == format!("{folder}/") {
        return Ok(Place::Folder);
    }
    let leaf = name
        .strip_prefix(&folder)
        .and_then(|rest| rest.strip_prefix('/'))
        .ok_or_else(|| format!("entry {name:?} is outside {folder}/"))?;
    let plain = !leaf.is_empty()
        && leaf != "."
        && leaf != ".."
        && leaf
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !plain {
        return Err(format!(
            "entry {name:?} is not a plain name directly inside {folder}/"
        ));
    }
    Ok(Place::Leaf(leaf))
}

/// Whether `leaf` may be in the folder, and if so whether it is one of the binaries to read.
fn wanted(leaf: &str, binaries: &[&'static str]) -> Result<Option<&'static str>, String> {
    if let Some(binary) = binaries.iter().find(|binary| **binary == leaf) {
        return Ok(Some(binary));
    }
    if leaf == "README.md" || leaf.starts_with("LICENSE") {
        return Ok(None);
    }
    Err(format!("{leaf:?} is not part of a loadout release"))
}

struct Collect {
    binaries: &'static [&'static str],
    found: Vec<Binary>,
    entries: usize,
    declared: u64,
}

impl Collect {
    fn new(windows: bool) -> Self {
        Self {
            binaries: expected(windows),
            found: Vec::new(),
            entries: 0,
            declared: 0,
        }
    }

    fn count(&mut self, size: u64) -> Result<(), String> {
        self.entries += 1;
        self.declared = self.declared.saturating_add(size);
        if self.entries > MAX_ENTRIES || self.declared > DECLARED_LIMIT {
            return Err("the archive is larger than any loadout release".to_owned());
        }
        Ok(())
    }

    fn file(&mut self, leaf: &str, size: u64, content: impl Read) -> Result<(), String> {
        let Some(name) = wanted(leaf, self.binaries)? else {
            return Ok(());
        };
        if self.found.iter().any(|binary| binary.name == name) {
            return Err(format!("the archive holds {name} more than once"));
        }
        if size > BINARY_LIMIT {
            return Err(format!(
                "{name} in the archive is larger than any loadout binary"
            ));
        }
        let mut bytes = Vec::new();
        content
            .take(BINARY_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read {name} from the archive: {error}"))?;
        if u64::try_from(bytes.len()).ok() != Some(size) {
            return Err(format!(
                "{name} in the archive is not the size its header declares"
            ));
        }
        self.found.push(Binary { name, bytes });
        Ok(())
    }

    fn finish(self) -> Result<Vec<Binary>, String> {
        let mut ordered = Vec::new();
        for name in self.binaries {
            let binary = self
                .found
                .iter()
                .find(|binary| binary.name == *name)
                .ok_or_else(|| format!("the archive has no {name}"))?;
            ordered.push(binary.clone());
        }
        Ok(ordered)
    }
}

/// The expected binaries from a `.tar.gz` release archive (macOS and Linux).
#[cfg(not(windows))]
pub(super) fn read(archive: &[u8], target: &str) -> Result<Vec<Binary>, String> {
    read_tar_gz(archive, target, false)
}

/// The expected binaries from a `.zip` release archive (Windows).
#[cfg(windows)]
pub(super) fn read(archive: &[u8], target: &str) -> Result<Vec<Binary>, String> {
    read_zip(archive, target, true)
}

#[cfg(not(windows))]
fn read_tar_gz(archive: &[u8], target: &str, windows: bool) -> Result<Vec<Binary>, String> {
    let unreadable = |error: std::io::Error| format!("cannot read the archive: {error}");
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut collect = Collect::new(windows);
    for entry in tar.entries().map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let size = entry.size();
        collect.count(size)?;
        let kind = entry.header().entry_type();
        let raw = entry.path_bytes().into_owned();
        match place(&raw, target)? {
            Place::Folder if kind.is_dir() => {}
            Place::Leaf(leaf) if kind.is_file() => collect.file(leaf, size, entry)?,
            _ => {
                return Err(format!(
                    "entry {:?} is a {kind:?}, but a release holds only regular files and its folder",
                    String::from_utf8_lossy(&raw)
                ));
            }
        }
    }
    collect.finish()
}

#[cfg(windows)]
fn read_zip(archive: &[u8], target: &str, windows: bool) -> Result<Vec<Binary>, String> {
    let unreadable = |error: zip::result::ZipError| format!("cannot read the archive: {error}");
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).map_err(unreadable)?;
    let mut collect = Collect::new(windows);
    for index in 0..zip.len() {
        let entry = zip.by_index(index).map_err(unreadable)?;
        let size = entry.size();
        collect.count(size)?;
        let raw = entry.name_raw().to_vec();
        let regular = entry.is_file()
            && !entry.is_symlink()
            && entry
                .unix_mode()
                .is_none_or(|mode| mode & 0o170_000 == 0o100_000);
        match place(&raw, target)? {
            Place::Folder if entry.is_dir() => {}
            Place::Leaf(leaf) if regular => collect.file(leaf, size, entry)?,
            _ => {
                return Err(format!(
                    "entry {:?} is not a regular file, but a release holds only regular files and its folder",
                    String::from_utf8_lossy(&raw)
                ));
            }
        }
    }
    collect.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_must_be_plain_and_inside_the_folder() {
        let target = "x86_64-unknown-linux-musl";
        assert_eq!(
            place(b"loadout-x86_64-unknown-linux-musl/", target).unwrap(),
            Place::Folder
        );
        assert_eq!(
            place(b"loadout-x86_64-unknown-linux-musl/loadout", target).unwrap(),
            Place::Leaf("loadout")
        );
        for bad in [
            b"loadout".as_slice(),
            b"evil",
            b"loadout-x86_64-unknown-linux-musl/../evil",
            b"loadout-x86_64-unknown-linux-musl/sub/loadout",
            b"loadout-x86_64-unknown-linux-musl/..",
            b"loadout-x86_64-unknown-linux-musl/a\\b",
            b"/loadout-x86_64-unknown-linux-musl/loadout",
            b"loadout-x86_64-unknown-linux-muslx/loadout",
            b"loadout-x86_64-unknown-linux-musl/\xff",
        ] {
            assert!(
                place(bad, target).is_err(),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(wanted("README.md", expected(false)).unwrap(), None);
        assert_eq!(wanted("LICENSE-MIT", expected(false)).unwrap(), None);
        assert!(wanted("install.sh", expected(false)).is_err());
        assert!(wanted("loadout-background.exe", expected(false)).is_err());
    }

    #[cfg(not(windows))]
    mod tar_gz {
        use super::super::*;

        const TARGET: &str = "x86_64-unknown-linux-musl";

        enum Kind {
            Dir,
            File(&'static [u8]),
            Symlink(&'static str),
            Hardlink(&'static str),
        }

        fn archive(entries: &[(&str, Kind)]) -> Vec<u8> {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            for (name, kind) in entries {
                let mut header = tar::Header::new_ustar();
                header.set_mode(0o755);
                match kind {
                    Kind::Dir => {
                        header.set_entry_type(tar::EntryType::Directory);
                        header.set_size(0);
                        builder
                            .append_data(&mut header, name, std::io::empty())
                            .unwrap();
                    }
                    Kind::File(data) => {
                        header.set_entry_type(tar::EntryType::Regular);
                        header.set_size(data.len() as u64);
                        builder.append_data(&mut header, name, *data).unwrap();
                    }
                    Kind::Symlink(to) | Kind::Hardlink(to) => {
                        let link = if matches!(kind, Kind::Symlink(_)) {
                            tar::EntryType::Symlink
                        } else {
                            tar::EntryType::Link
                        };
                        header.set_entry_type(link);
                        header.set_size(0);
                        builder.append_link(&mut header, name, to).unwrap();
                    }
                }
            }
            builder.into_inner().unwrap().finish().unwrap()
        }

        fn good() -> Vec<(&'static str, Kind)> {
            vec![
                ("loadout-x86_64-unknown-linux-musl", Kind::Dir),
                (
                    "loadout-x86_64-unknown-linux-musl/loadout",
                    Kind::File(b"\x7fELF fake"),
                ),
                (
                    "loadout-x86_64-unknown-linux-musl/README.md",
                    Kind::File(b"readme"),
                ),
            ]
        }

        #[test]
        fn a_release_layout_yields_its_binary() {
            let binaries = read(&archive(&good()), TARGET).unwrap();
            assert_eq!(
                binaries,
                vec![Binary {
                    name: "loadout",
                    bytes: b"\x7fELF fake".to_vec()
                }]
            );
        }

        #[test]
        fn anything_else_is_refused() {
            let cases: Vec<(&str, Vec<(&str, Kind)>)> = vec![
                (
                    "missing binary",
                    vec![(
                        "loadout-x86_64-unknown-linux-musl/README.md",
                        Kind::File(b"r"),
                    )],
                ),
                ("duplicate binary", {
                    let mut entries = good();
                    entries.push((
                        "loadout-x86_64-unknown-linux-musl/loadout",
                        Kind::File(b"two"),
                    ));
                    entries
                }),
                (
                    "symlinked binary",
                    vec![(
                        "loadout-x86_64-unknown-linux-musl/loadout",
                        Kind::Symlink("/bin/sh"),
                    )],
                ),
                (
                    "hardlinked binary",
                    vec![
                        (
                            "loadout-x86_64-unknown-linux-musl/README.md",
                            Kind::File(b"x"),
                        ),
                        (
                            "loadout-x86_64-unknown-linux-musl/loadout",
                            Kind::Hardlink("loadout-x86_64-unknown-linux-musl/README.md"),
                        ),
                    ],
                ),
                ("unexpected file", {
                    let mut entries = good();
                    entries.push((
                        "loadout-x86_64-unknown-linux-musl/install.sh",
                        Kind::File(b"x"),
                    ));
                    entries
                }),
                ("outside the folder", {
                    let mut entries = good();
                    entries.push(("evil", Kind::File(b"x")));
                    entries
                }),
                ("nested", {
                    let mut entries = good();
                    entries.push((
                        "loadout-x86_64-unknown-linux-musl/sub/loadout",
                        Kind::File(b"x"),
                    ));
                    entries
                }),
                (
                    "folder as a file",
                    vec![
                        ("loadout-x86_64-unknown-linux-musl", Kind::File(b"x")),
                        (
                            "loadout-x86_64-unknown-linux-musl/loadout",
                            Kind::File(b"bin"),
                        ),
                    ],
                ),
            ];
            for (name, entries) in cases {
                assert!(
                    read(&archive(&entries), TARGET).is_err(),
                    "{name} must be refused"
                );
            }
            assert!(read(b"not gzip at all", TARGET).is_err());
        }
    }
}
