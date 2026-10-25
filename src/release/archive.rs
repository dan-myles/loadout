//! Reading the binaries out of a verified release archive, in memory.
//!
//! Nothing is extracted to disk here: every entry's name and type is checked first, only the
//! expected binaries are read, and each read is bounded. A release archive holds exactly
//! `loadout-<target>/`, the expected binaries, and optionally `README.md` and `LICENSE*`, all as
//! plain names directly inside that folder (see `scripts/release/package.py`). A `.tar.gz` is
//! plain USTAR, with no GNU or PAX extension entries.

use std::io::Read;

const MAX_ENTRIES: usize = 64;
/// One binary larger than this is refused rather than read.
const BINARY_LIMIT: u64 = 128 * 1024 * 1024;
/// The sum of the sizes every entry declares; an archive claiming more is refused before any of
/// its content is decompressed.
const DECLARED_LIMIT: u64 = 1024 * 1024 * 1024;
/// All the decompressed tar stream an archive within the limits above can need: every entry's
/// header and padded content, and the end-of-archive blocks.
#[cfg(not(windows))]
const STREAM_LIMIT: u64 = DECLARED_LIMIT + (MAX_ENTRIES as u64 + 2) * 1024;

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

    /// Counts one physical entry, before any of its content is read.
    fn count(&mut self, size: u64) -> Result<(), String> {
        self.entries += 1;
        self.declared = self.declared.saturating_add(size);
        if self.entries > MAX_ENTRIES || size > BINARY_LIMIT || self.declared > DECLARED_LIMIT {
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
    read_tar(
        flate2::read::GzDecoder::new(archive).take(STREAM_LIMIT),
        target,
        windows,
    )
}

/// Reads the tar's physical entries raw. The tar crate's default iterator reads a GNU long name or
/// long link, or a PAX header, into memory with no bound before yielding the entry it describes,
/// so those would escape every limit here. Raw, each header is counted against the limits and
/// refused unless it is a USTAR folder or regular file, before any of its content is read.
#[cfg(not(windows))]
fn read_tar(stream: impl Read, target: &str, windows: bool) -> Result<Vec<Binary>, String> {
    let unreadable = |error: std::io::Error| format!("cannot read the archive: {error}");
    let mut tar = tar::Archive::new(stream);
    let mut collect = Collect::new(windows);
    for entry in tar.entries().map_err(unreadable)?.raw(true) {
        let entry = entry.map_err(unreadable)?;
        let size = entry.size();
        collect.count(size)?;
        let header = entry.header();
        let kind = header.entry_type();
        let raw = entry.path_bytes().into_owned();
        let shown = String::from_utf8_lossy(&raw).into_owned();
        if !(kind.is_dir() || kind.is_file()) {
            return Err(format!(
                "entry {shown:?} is a {kind:?}, but a release holds only regular files and its folder"
            ));
        }
        if header.as_ustar().is_none() {
            return Err(format!(
                "entry {shown:?} is not a USTAR entry, but a release archive is plain USTAR"
            ));
        }
        match place(&raw, target)? {
            Place::Folder if kind.is_dir() && size == 0 => {}
            Place::Leaf(leaf) if kind.is_file() => collect.file(leaf, size, entry)?,
            _ => {
                return Err(format!(
                    "entry {shown:?} is a {kind:?}, but a release holds only regular files and its folder"
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

        const BINARY: &str = "loadout-x86_64-unknown-linux-musl/loadout";
        const EXTENSIONS: [tar::EntryType; 5] = [
            tar::EntryType::GNULongName,
            tar::EntryType::GNULongLink,
            tar::EntryType::XHeader,
            tar::EntryType::XGlobalHeader,
            tar::EntryType::GNUSparse,
        ];

        /// One raw header block, with whatever size it claims.
        fn header(name: &str, kind: tar::EntryType, size: u64) -> Vec<u8> {
            let mut header = tar::Header::new_ustar();
            header.set_path(name).unwrap();
            header.set_entry_type(kind);
            header.set_mode(0o644);
            header.set_size(size);
            header.set_cksum();
            header.as_bytes().to_vec()
        }

        fn gzip(bytes: &[u8]) -> Vec<u8> {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(bytes).unwrap();
            encoder.finish().unwrap()
        }

        /// A tar stream that never ends, and how much of it was read.
        struct Endless {
            start: std::io::Cursor<Vec<u8>>,
            read: std::rc::Rc<std::cell::Cell<u64>>,
        }

        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let mut read = self.start.read(buf)?;
                if read == 0 {
                    buf.fill(b'a');
                    read = buf.len();
                }
                self.read.set(self.read.get() + read as u64);
                Ok(read)
            }
        }

        /// Reads `start` followed by endless content, returning the error and the bytes read.
        fn refused_after(start: Vec<u8>) -> (String, u64) {
            let read = std::rc::Rc::new(std::cell::Cell::new(0));
            let stream = Endless {
                start: std::io::Cursor::new(start),
                read: std::rc::Rc::clone(&read),
            };
            let error = read_tar(stream, TARGET, false).unwrap_err();
            (error, read.get())
        }

        #[test]
        fn huge_gnu_and_pax_metadata_is_refused_from_its_header_alone() {
            for kind in EXTENSIONS {
                for size in [BINARY_LIMIT + 1, 1 << 40] {
                    let (error, consumed) = refused_after(header(BINARY, kind, size));
                    assert!(
                        error.contains("larger than any loadout release"),
                        "{kind:?} {size}: {error}"
                    );
                    assert_eq!(consumed, 512, "{kind:?} {size}: only its header is read");
                }
            }
        }

        #[test]
        fn small_or_truncated_gnu_and_pax_metadata_is_refused_unread() {
            for kind in EXTENSIONS {
                let (error, consumed) = refused_after(header(BINARY, kind, 30));
                assert!(
                    error.contains("a release holds only regular files"),
                    "{kind:?}: {error}"
                );
                assert_eq!(consumed, 512, "{kind:?}: only its header is read");

                // Declares 1000 bytes and ends after 100: still refused from the header.
                let mut truncated = header(BINARY, kind, 1000);
                truncated.extend_from_slice(&[b'a'; 100]);
                let error = read(&gzip(&truncated), TARGET).unwrap_err();
                assert!(
                    error.contains("a release holds only regular files"),
                    "{kind:?}: {error}"
                );
            }
        }

        #[test]
        fn compressed_metadata_far_larger_than_the_limits_is_refused() {
            // A small gzip whose first entry claims 2 GiB of long name, with 8 MiB of it present.
            let mut stream = header(BINARY, tar::EntryType::GNULongName, 2 << 30);
            stream.resize(stream.len() + (8 << 20), b'a');
            let compressed = gzip(&stream);
            assert!(compressed.len() < stream.len() / 100);
            let error = read(&compressed, TARGET).unwrap_err();
            assert!(error.contains("larger than any loadout release"), "{error}");

            // A well-formed PAX header before an otherwise valid binary entry.
            let record = b"30 path=loadout-x/loadout-two\n";
            let mut pax = header(BINARY, tar::EntryType::XHeader, record.len() as u64);
            pax.extend_from_slice(record);
            pax.resize(1024, 0);
            pax.extend_from_slice(&header(BINARY, tar::EntryType::Regular, 3));
            pax.extend_from_slice(b"bin");
            pax.resize(1536 + 1024, 0);
            let error = read(&gzip(&pax), TARGET).unwrap_err();
            assert!(
                error.contains("a release holds only regular files"),
                "{error}"
            );
        }

        #[test]
        fn every_entry_is_held_to_the_size_limit_and_read_exactly() {
            let (error, consumed) = refused_after(header(
                "loadout-x86_64-unknown-linux-musl/README.md",
                tar::EntryType::Regular,
                BINARY_LIMIT + 1,
            ));
            assert!(error.contains("larger than any loadout release"), "{error}");
            assert_eq!(consumed, 512);

            // A binary whose content ends before its declared size.
            let mut short = header(BINARY, tar::EntryType::Regular, 100);
            short.extend_from_slice(&[b'a'; 10]);
            assert!(read_tar(short.as_slice(), TARGET, false).is_err());

            // A header block cut short.
            let cut = header(BINARY, tar::EntryType::Regular, 0)[..300].to_vec();
            assert!(
                read_tar(cut.as_slice(), TARGET, false)
                    .unwrap_err()
                    .contains("cannot read the archive")
            );

            // A folder entry that claims content.
            let mut folder = header(
                "loadout-x86_64-unknown-linux-musl",
                tar::EntryType::Directory,
                512,
            );
            folder.resize(1024, 0);
            assert!(read_tar(folder.as_slice(), TARGET, false).is_err());
        }

        #[test]
        fn only_ustar_headers_are_accepted() {
            let mut gnu = tar::Header::new_gnu();
            gnu.set_path(BINARY).unwrap();
            gnu.set_entry_type(tar::EntryType::Regular);
            gnu.set_mode(0o755);
            gnu.set_size(3);
            gnu.set_cksum();
            let mut stream = gnu.as_bytes().to_vec();
            stream.extend_from_slice(b"bin");
            stream.resize(1024 + 1024, 0);
            let error = read_tar(stream.as_slice(), TARGET, false).unwrap_err();
            assert!(error.contains("USTAR"), "{error}");
        }

        #[test]
        fn too_many_entries_are_refused() {
            let mut entries = good();
            for _ in 0..MAX_ENTRIES {
                entries.push((
                    "loadout-x86_64-unknown-linux-musl/README.md",
                    Kind::File(b""),
                ));
            }
            assert!(
                read(&archive(&entries), TARGET)
                    .unwrap_err()
                    .contains("larger than any loadout release")
            );
        }
    }
}
