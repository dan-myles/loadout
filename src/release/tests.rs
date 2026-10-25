use super::*;

const RECORD_HEADER: &str = "# loadout install record. Written by install.sh or install.ps1 and read by loadout self-update. Do not edit.";

#[test]
fn only_skips_are_skipped() {
    assert!(Error::Isolated.skipped());
    assert!(Error::NoRelease.skipped());
    assert!(
        Error::Unmanaged {
            reason: String::new()
        }
        .skipped()
    );
    assert!(
        !Error::Verify {
            detail: String::new()
        }
        .skipped()
    );
    assert!(
        !Error::Download {
            detail: String::new()
        }
        .skipped()
    );
    assert!(
        !Error::Changed {
            detail: String::new()
        }
        .skipped()
    );
}

#[test]
fn isolation_is_never_managed() {
    let home = tempfile::tempdir().unwrap();
    let context = Context::resolve(
        crate::platform::Os::Linux,
        |name| (name == "LOADOUT_HOME").then(|| home.path().as_os_str().to_owned()),
        None,
    )
    .unwrap();
    assert!(matches!(latest(&context), Err(Error::Isolated)));
    assert_eq!(checks(&context)[0].status, CheckStatus::Info);
}

#[cfg(unix)]
mod unix {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write as _};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    use super::*;

    const TARGET: &str = "x86_64-unknown-linux-musl";
    const ASSET: &str = "loadout-x86_64-unknown-linux-musl.tar.gz";
    const WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";

    type Routes = Arc<Mutex<BTreeMap<String, (u16, Vec<(String, String)>, Vec<u8>)>>>;

    /// A loopback HTTP server answering from a route table; anything else is a 404.
    struct Server {
        base: String,
        routes: Routes,
    }

    impl Server {
        fn start() -> Server {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes: Routes = Arc::default();
            let served = Arc::clone(&routes);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    if reader.read_line(&mut request).is_err() {
                        continue;
                    }
                    let mut line = String::new();
                    while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                        line.clear();
                    }
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let (status, headers, body) = served
                        .lock()
                        .unwrap()
                        .get(&path)
                        .cloned()
                        .unwrap_or((404, Vec::new(), b"not found".to_vec()));
                    let mut head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    );
                    for (name, value) in headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
            Server { base, routes }
        }

        fn route(&self, path: &str, status: u16, headers: &[(&str, &str)], body: &[u8]) {
            self.routes.lock().unwrap().insert(
                path.to_owned(),
                (
                    status,
                    headers
                        .iter()
                        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                        .collect(),
                    body.to_vec(),
                ),
            );
        }

        fn source(&self) -> Source {
            Source {
                site: self.base.clone(),
                repository: REPOSITORY.to_owned(),
                https_only: false,
            }
        }

        fn publish(&self, tag: &str, archive: &[u8], sidecar: Option<String>) {
            let location = format!("{}/{REPOSITORY}/releases/tag/{tag}", self.base);
            self.route(
                &format!("/{REPOSITORY}/releases/latest"),
                302,
                &[("Location", &location)],
                b"",
            );
            let asset = format!("/{REPOSITORY}/releases/download/{tag}/{ASSET}");
            self.route(&asset, 200, &[], archive);
            let sidecar = sidecar.unwrap_or_else(|| format!("{}  {ASSET}\n", sha256(archive)));
            self.route(&format!("{asset}.sha256"), 200, &[], sidecar.as_bytes());
        }
    }

    fn script(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo \"loadout {version}\"; fi\n")
            .into_bytes()
    }

    fn release_archive(binary: &[u8]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut folder = tar::Header::new_ustar();
        folder.set_entry_type(tar::EntryType::Directory);
        folder.set_mode(0o755);
        folder.set_size(0);
        builder
            .append_data(&mut folder, format!("loadout-{TARGET}"), std::io::empty())
            .unwrap();
        let mut file = tar::Header::new_ustar();
        file.set_entry_type(tar::EntryType::Regular);
        file.set_mode(0o755);
        file.set_size(binary.len() as u64);
        builder
            .append_data(&mut file, format!("loadout-{TARGET}/loadout"), binary)
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// An installer-managed `loadout` 1.0.0 in a fresh folder, as install.sh leaves it.
    struct Installed {
        dir: tempfile::TempDir,
        exe: PathBuf,
    }

    impl Installed {
        fn new() -> Installed {
            let dir = tempfile::tempdir().unwrap();
            let exe = dir.path().canonicalize().unwrap().join("loadout");
            fs::write(&exe, script("1.0.0")).unwrap();
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
            let installed = Installed { dir, exe };
            installed.write_record(|text| text);
            installed
        }

        fn record_path(&self) -> PathBuf {
            self.exe.with_file_name(record::FILE)
        }

        fn write_record(&self, edit: impl Fn(String) -> String) {
            let text = format!(
                "{RECORD_HEADER}\nschema = 1\ninstaller = \"install.sh\"\nrepository = \"{REPOSITORY}\"\n\
                 source = \"https://example.invalid/{ASSET}\"\ntarget = \"{TARGET}\"\nversion = \"1.0.0\"\n\
                 archive_sha256 = \"{}\"\nbinary = \"{}\"\nbinary_sha256 = \"{}\"\n",
                "a".repeat(64),
                self.exe.display(),
                sha256(&fs::read(&self.exe).unwrap()),
            );
            fs::write(self.record_path(), edit(text)).unwrap();
        }

        fn install(&self) -> Result<Install, String> {
            installed(&self.exe, false, TARGET, &Version::new(1, 0, 0))
        }

        /// Every entry of the folder: a file's bytes, a link's target, or a folder marker.
        fn snapshot(&self) -> BTreeMap<String, Vec<u8>> {
            fs::read_dir(self.dir.path())
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let kind = entry.file_type().unwrap();
                    let content = if kind.is_symlink() {
                        let target = fs::read_link(entry.path()).unwrap();
                        [b"link:".as_slice(), target.as_os_str().as_encoded_bytes()].concat()
                    } else if kind.is_dir() {
                        b"folder".to_vec()
                    } else {
                        fs::read(entry.path()).unwrap()
                    };
                    (entry.file_name().into_string().unwrap(), content)
                })
                .collect()
        }

        fn launcher(&self) -> PathBuf {
            self.exe.with_file_name("loadout-background.exe")
        }

        /// The record install.ps1 writes: schema 1, or schema 2 naming `background` and its hash.
        fn write_windows_record(&self, background: Option<(&Path, &str)>) {
            let mut text = format!(
                "{RECORD_HEADER}\nschema = {}\ninstaller = \"install.ps1\"\nrepository = \"{REPOSITORY}\"\n\
                 source = \"https://example.invalid/loadout-{WINDOWS_TARGET}.zip\"\ntarget = \"{WINDOWS_TARGET}\"\n\
                 version = \"1.0.0\"\narchive_sha256 = \"{}\"\nbinary = \"{}\"\nbinary_sha256 = \"{}\"\n",
                if background.is_some() { 2 } else { 1 },
                "a".repeat(64),
                self.exe.display(),
                sha256(&fs::read(&self.exe).unwrap()),
            );
            if let Some((path, sha)) = background {
                text.push_str(&format!(
                    "background = \"{}\"\nbackground_sha256 = \"{sha}\"\n",
                    path.display()
                ));
            }
            fs::write(self.record_path(), text).unwrap();
        }

        /// The binding as a Windows build sees it. `installed` takes the platform as a parameter, so
        /// the Windows rules run here on Unix too.
        fn windows_install(&self) -> Result<Install, String> {
            installed(&self.exe, true, WINDOWS_TARGET, &Version::new(1, 0, 0))
        }
    }

    fn release_for(server: &Server, fixture: &Installed) -> Result<Option<Release>, Error> {
        find(
            &server.source(),
            fixture.install().unwrap(),
            TARGET,
            Version::new(1, 0, 0),
            Containment::OwnGroup,
        )
    }

    #[test]
    fn only_a_strictly_newer_stable_release_is_offered() {
        let server = Server::start();
        let fixture = Installed::new();
        for (tag, newer) in [
            ("v1.0.0", false),
            ("v0.9.9", false),
            ("v1.0.1", true),
            ("v2.0.0", true),
        ] {
            server.publish(tag, b"unused", None);
            let release = release_for(&server, &fixture).unwrap();
            assert_eq!(release.is_some(), newer, "{tag}");
            if let Some(release) = release {
                assert_eq!(release.tag(), tag);
                assert_eq!(release.current(), "1.0.0");
            }
        }
        server.publish("v1.1.0-beta.1", b"unused", None);
        assert!(matches!(
            release_for(&server, &fixture),
            Err(Error::Verify { .. })
        ));
    }

    #[test]
    fn no_published_release_is_a_skip() {
        let server = Server::start();
        let error = release_for(&server, &Installed::new()).unwrap_err();
        assert!(matches!(error, Error::NoRelease) && error.skipped());
    }

    #[test]
    fn a_newer_release_replaces_the_binary_then_the_record() {
        let server = Server::start();
        let fixture = Installed::new();
        let new_binary = script("1.2.0");
        let archive = release_archive(&new_binary);
        server.publish("v1.2.0", &archive, None);
        let release = release_for(&server, &fixture).unwrap().unwrap();
        install(&release).unwrap();

        assert_eq!(fs::read(&fixture.exe).unwrap(), new_binary);
        let record = record::parse(&fs::read(fixture.record_path()).unwrap()).unwrap();
        assert_eq!(record.schema(), 1);
        assert_eq!(record.installer, "install.sh");
        assert_eq!(record.version, "1.2.0");
        assert_eq!(record.archive_sha256, sha256(&archive));
        assert_eq!(record.binary, fixture.exe.display().to_string());
        assert_eq!(record.binary_sha256, sha256(&new_binary));
        assert_eq!(
            fixture.snapshot().keys().cloned().collect::<Vec<_>>(),
            ["loadout", "loadout.install.toml"]
        );
        assert!(installed(&fixture.exe, false, TARGET, &Version::new(1, 2, 0)).is_ok());
    }

    #[test]
    fn a_bad_download_changes_nothing() {
        let good = release_archive(&script("1.2.0"));
        let cases: Vec<(&str, Vec<u8>, Option<String>)> = vec![
            (
                "sidecar for another asset",
                good.clone(),
                Some(format!("{}  loadout-other.tar.gz\n", sha256(&good))),
            ),
            (
                "sidecar with CRLF",
                good.clone(),
                Some(format!("{}  {ASSET}\r\n", sha256(&good))),
            ),
            (
                "hash mismatch",
                good.clone(),
                Some(format!("{}  {ASSET}\n", "0".repeat(64))),
            ),
            (
                "binary reports another version",
                release_archive(&script("1.1.9")),
                None,
            ),
            ("not a release archive", b"garbage".to_vec(), None),
        ];
        for (name, archive, sidecar) in cases {
            let server = Server::start();
            let fixture = Installed::new();
            let before = fixture.snapshot();
            server.publish("v1.2.0", &archive, sidecar);
            let release = release_for(&server, &fixture).unwrap().unwrap();
            let error = install(&release).unwrap_err();
            assert!(matches!(error, Error::Verify { .. }), "{name}: {error}");
            assert_eq!(
                fixture.snapshot(),
                before,
                "{name} must leave the install untouched"
            );
        }
    }

    #[test]
    fn a_binary_changed_after_the_check_is_not_replaced() {
        let server = Server::start();
        let fixture = Installed::new();
        server.publish("v1.2.0", &release_archive(&script("1.2.0")), None);
        let release = release_for(&server, &fixture).unwrap().unwrap();
        fs::write(&fixture.exe, b"edited by hand").unwrap();
        let before = fixture.snapshot();
        assert!(matches!(install(&release), Err(Error::Changed { .. })));
        assert_eq!(fixture.snapshot(), before);
    }

    #[test]
    fn provenance_must_bind_installer_target_path_hash_and_version() {
        let fixture = Installed::new();
        assert!(fixture.install().is_ok());
        let cases: Vec<(&str, Box<dyn Fn(String) -> String>)> = vec![
            (
                "other installer",
                Box::new(|text| text.replace("\"install.sh\"", "\"install.ps1\"")),
            ),
            (
                "other repository",
                Box::new(|text| text.replace(REPOSITORY, "someone/else")),
            ),
            (
                "other target",
                Box::new(|text| text.replace(TARGET, "aarch64-unknown-linux-musl")),
            ),
            (
                "other path",
                Box::new(|text| text.replace("binary = \"/", "binary = \"/elsewhere/")),
            ),
            (
                "other version",
                Box::new(|text| text.replace("version = \"1.0.0\"", "version = \"0.9.0\"")),
            ),
            (
                "other hash",
                Box::new(|text| {
                    let line = text
                        .lines()
                        .find(|line| line.starts_with("binary_sha256"))
                        .unwrap()
                        .to_owned();
                    text.replace(&line, &format!("binary_sha256 = \"{}\"", "f".repeat(64)))
                }),
            ),
            (
                "windows schema",
                Box::new(|text| {
                    text.replace("schema = 1", "schema = 2")
                        + &format!(
                            "background = \"/x/loadout-background.exe\"\nbackground_sha256 = \"{}\"\n",
                            "c".repeat(64)
                        )
                }),
            ),
            (
                "malformed",
                Box::new(|text| text.replace("schema = 1", "schema = \"1\"")),
            ),
        ];
        for (name, edit) in cases {
            fixture.write_record(edit);
            assert!(
                fixture.install().is_err(),
                "{name} must leave the binary unmanaged"
            );
        }
        fs::remove_file(fixture.record_path()).unwrap();
        assert!(fixture.install().unwrap_err().contains("no install record"));
        std::os::unix::fs::symlink(
            fixture.dir.path().join("elsewhere.toml"),
            fixture.record_path(),
        )
        .unwrap();
        assert!(fixture.install().is_err());
    }

    fn remove(path: &Path) {
        if fs::symlink_metadata(path).unwrap().is_dir() {
            fs::remove_dir(path).unwrap();
        } else {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn a_schema_1_windows_install_refuses_anything_at_the_launcher_path() {
        let fixture = Installed::new();
        fixture.write_windows_record(None);
        assert_eq!(fixture.windows_install().unwrap().background, None);
        let launcher = fixture.launcher();
        let outside = fixture.dir.path().join("elsewhere.exe");
        fs::write(&outside, b"someone else's").unwrap();
        let cases: Vec<(&str, Box<dyn Fn()>)> = vec![
            (
                "foreign file",
                Box::new(|| fs::write(&launcher, b"someone else's launcher").unwrap()),
            ),
            (
                "link to a file",
                Box::new(|| std::os::unix::fs::symlink(&outside, &launcher).unwrap()),
            ),
            (
                "dangling link",
                Box::new(|| {
                    std::os::unix::fs::symlink(fixture.dir.path().join("missing"), &launcher)
                        .unwrap();
                }),
            ),
            ("folder", Box::new(|| fs::create_dir(&launcher).unwrap())),
        ];
        for (name, make) in cases {
            make();
            let before = fixture.snapshot();
            let error = fixture.windows_install().unwrap_err();
            assert!(
                error.contains("is not in the schema 1 record"),
                "{name}: {error}"
            );
            assert_eq!(fixture.snapshot(), before, "{name} must stay as it was");
            remove(&launcher);
        }
        assert!(fixture.windows_install().is_ok());
    }

    #[test]
    fn a_schema_2_launcher_must_be_the_regular_file_its_record_names() {
        let fixture = Installed::new();
        let launcher = fixture.launcher();
        fs::write(&launcher, b"launcher").unwrap();
        let sha = sha256(b"launcher");
        fixture.write_windows_record(Some((&launcher, &sha)));
        assert_eq!(
            fixture.windows_install().unwrap().background,
            Some(launcher.clone())
        );

        // A link at the launcher path is refused, even to a file with the recorded bytes and
        // whichever of the two paths the record names.
        let real = fixture.exe.with_file_name("real-launcher.exe");
        fs::rename(&launcher, &real).unwrap();
        std::os::unix::fs::symlink(&real, &launcher).unwrap();
        for recorded in [&launcher, &real] {
            fixture.write_windows_record(Some((recorded, &sha)));
            let before = fixture.snapshot();
            let error = fixture.windows_install().unwrap_err();
            assert!(error.contains("is not a regular file"), "{error}");
            assert_eq!(fixture.snapshot(), before);
        }

        fs::remove_file(&launcher).unwrap();
        fixture.write_windows_record(Some((&launcher, &sha)));
        assert!(
            fixture
                .windows_install()
                .unwrap_err()
                .contains("is not a regular file")
        );
        fs::write(&launcher, b"edited").unwrap();
        assert!(
            fixture
                .windows_install()
                .unwrap_err()
                .contains("changed after it was installed")
        );
    }
}
