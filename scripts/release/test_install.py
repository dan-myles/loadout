import hashlib
import http.server
import io
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
import threading
import unittest
from functools import partial
from pathlib import Path

try:
    import tomllib
except ImportError:
    tomllib = None

try:
    import fcntl
except ImportError:
    fcntl = None

SCRIPT = Path(__file__).with_name("install.sh")
TIMEOUT = 120
TARGET = "x86_64-unknown-linux-musl"
PREFIX = f"loadout-{TARGET}"
ASSET = f"{PREFIX}.tar.gz"
LOCK = ".loadout.install.lock"
# Every command install.sh runs, for a PATH that leaves some of them out.
TOOLS = (
    "cat", "chmod", "cmp", "curl", "flock", "git", "grep", "gzip", "iconv", "mkdir", "mktemp", "mv",
    "perl", "rm", "sed", "sha256sum", "shasum", "tail", "tar", "tr", "wc",
)
FAKE_UNAME = """#!/bin/sh
case $1 in
  -s) echo "${FAKE_OS:-Linux}" ;;
  -m) echo "${FAKE_ARCH:-x86_64}" ;;
esac
"""


def fake_loadout(version="1.2.3"):
    return f"""#!/bin/sh
if [ "$1" = --version ]; then echo "loadout {version}"; exit 0; fi
printf '%s\\n' "$@" > "$LOADOUT_TEST_ARGS"
exit 7
""".encode()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def tar_gz(members):
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w:gz") as tar:
        for name, kind, payload in members:
            info = tarfile.TarInfo(name)
            if kind == "dir":
                info.type = tarfile.DIRTYPE
                info.mode = 0o755
                tar.addfile(info)
            elif kind == "symlink":
                info.type = tarfile.SYMTYPE
                info.linkname = payload
                tar.addfile(info)
            elif kind == "hardlink":
                info.type = tarfile.LNKTYPE
                info.linkname = payload
                tar.addfile(info)
            else:
                info.size = len(payload)
                info.mode = 0o755
                tar.addfile(info, io.BytesIO(payload))
    return raw.getvalue()


def release_members(binary):
    return [(PREFIX, "dir", None), (f"{PREFIX}/loadout", "file", binary), (f"{PREFIX}/README.md", "file", b"readme\n")]


class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        # A hook runs once, before its path is answered, so a test can act while a download waits.
        hook = self.server.hooks.pop(self.path, None)
        if hook:
            hook()
        location = self.server.redirects.get(self.path)
        if location:
            self.send_response(302)
            self.send_header("Location", location)
            self.end_headers()
            return
        super().do_GET()


@unittest.skipIf(os.name == "nt", "install.sh is for macOS and Linux")
@unittest.skipUnless(shutil.which("curl") and shutil.which("git"), "install.sh needs curl and git")
class InstallShTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.home = self.tmp / "home"
        self.home.mkdir()
        self.served = self.tmp / "served"
        self.served.mkdir()
        self.fakebin = self.tmp / "fakebin"
        self.fakebin.mkdir()
        self.write_exe(self.fakebin / "uname", FAKE_UNAME)
        self.init_args = self.tmp / "init-args"
        self.cwd = self.tmp / "cwd"
        self.cwd.mkdir()
        self.archive = self.publish()

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(self.served)))
        server.redirects = {}
        server.hooks = {}
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        self.server = server
        self.base = f"http://127.0.0.1:{server.server_address[1]}"

    @property
    def bin_dir(self):
        return self.home / ".local" / "bin"

    @property
    def dest(self):
        return self.bin_dir / "loadout"

    @property
    def record(self):
        return self.bin_dir / "loadout.install.toml"

    @property
    def lock(self):
        return self.bin_dir / LOCK

    def hold_lock(self):
        # flock(2), as loadout self-update and the installer itself take it.
        handle = open(self.lock, "a")
        self.addCleanup(handle.close)
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return handle

    def path_without(self, *missing):
        tools = self.tmp / ("tools-without-" + "-".join(missing))
        tools.mkdir()
        for name in TOOLS:
            found = shutil.which(name)
            if name not in missing and found:
                (tools / name).symlink_to(found)
        self.write_exe(tools / "uname", FAKE_UNAME)
        return str(tools)

    def write_exe(self, path, text):
        path.write_text(text)
        path.chmod(0o755)

    def publish(self, binary=None, members=None, sidecar=None):
        archive = tar_gz(members if members is not None else release_members(binary or fake_loadout()))
        (self.served / ASSET).write_bytes(archive)
        if sidecar is None:
            sidecar = f"{sha256(archive)}  {ASSET}\n"
        (self.served / f"{ASSET}.sha256").write_bytes(sidecar.encode() if isinstance(sidecar, str) else sidecar)
        return archive

    def install(self, *args, path=None, home=None, **env):
        variables = dict(
            os.environ,
            HOME=str(home or self.home),
            PATH=path or f"{self.fakebin}{os.pathsep}{os.environ['PATH']}",
            LOADOUT_TEST_ARGS=str(self.init_args),
            **env,
        )
        return subprocess.run(
            ["/bin/sh", str(SCRIPT), "--base-url", self.base, *args],
            cwd=str(self.cwd),
            env=variables,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            timeout=TIMEOUT,
        )

    def install_ok(self, *args, **env):
        result = self.install(*args, **env)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def snapshot(self):
        if not self.bin_dir.exists():
            return None
        def content(path):
            if path.is_symlink():
                return None
            return "folder" if path.is_dir() else path.read_bytes()

        return {p.name: (p.is_symlink(), content(p)) for p in self.bin_dir.iterdir()}

    def assert_refused(self, result, message, before):
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(message, result.stderr)
        self.assertEqual(self.snapshot(), before, "a refused install must not change the install directory")

    def read_record(self):
        values = {}
        for line in self.record.read_text(encoding="utf-8").splitlines():
            if not line.startswith("#"):
                key, value = line.split(" = ", 1)
                values[key] = value
        return values

    def test_fresh_install_writes_binary_and_schema_1_record(self):
        self.install_ok()
        self.assertEqual(self.dest.read_bytes(), fake_loadout())
        self.assertTrue(os.access(self.dest, os.X_OK))
        self.assertEqual(sorted(p.name for p in self.bin_dir.iterdir()), [LOCK, "loadout", "loadout.install.toml"])
        self.assertEqual(self.lock.read_bytes(), b"")
        lines = self.record.read_text(encoding="utf-8").splitlines()
        self.assertTrue(lines[0].startswith("# loadout install record."))
        self.assertEqual(
            lines[1:],
            [
                "schema = 1",
                'installer = "install.sh"',
                'repository = "dan-myles/loadout"',
                f'source = "{self.base}/{ASSET}"',
                f'target = "{TARGET}"',
                'version = "1.2.3"',
                f'archive_sha256 = "{sha256(self.archive)}"',
                f'binary = "{os.path.realpath(self.dest)}"',
                f'binary_sha256 = "{sha256(fake_loadout())}"',
            ],
        )
        self.assertFalse(self.init_args.exists(), "init must not run without a loadout URL")

    @unittest.skipIf(tomllib is None, "needs tomllib (Python 3.11+)")
    def test_record_is_valid_toml_for_paths_with_quotes_and_backslashes(self):
        home = self.tmp / 'we"ird\\home'
        home.mkdir()
        self.home = home
        self.install_ok(home=home)
        with open(self.record, "rb") as handle:
            record = tomllib.load(handle)
        self.assertEqual(record["schema"], 1)
        self.assertEqual(record["binary"], os.path.realpath(self.dest))
        self.assertEqual(record["binary_sha256"], sha256(fake_loadout()))
        self.assertEqual(record["archive_sha256"], sha256(self.archive))

    def test_hashes_and_paths_round_trip_under_quotes_and_backslashes(self):
        # Record values are TOML basic strings that only escape backslash and double quote, which
        # JSON decodes the same way, so this checks every value without needing tomllib. The staged
        # binary, the destination and the download all live under such paths: given a name with a
        # backslash, sha256sum and shasum prefix the digest with one.
        downloads = self.tmp / 'tm"p\\dir'
        downloads.mkdir()
        home = self.tmp / 'we"ird\\home'
        home.mkdir()
        self.home = home
        self.install_ok(home=home, TMPDIR=str(downloads))
        record = {key: json.loads(value) for key, value in self.read_record().items() if key != "schema"}
        self.assertEqual(record["binary"], os.path.realpath(self.dest))
        self.assertEqual(record["binary_sha256"], sha256(fake_loadout()))
        self.assertEqual(record["archive_sha256"], sha256(self.archive))
        self.assertEqual(record["source"], f"{self.base}/{ASSET}")
        self.assertEqual(record["version"], "1.2.3")

        # The next install reads that record back as its own and upgrades in place.
        self.publish(fake_loadout("1.2.4"))
        self.install_ok(home=home, TMPDIR=str(downloads))
        self.assertEqual(self.dest.read_bytes(), fake_loadout("1.2.4"))
        self.assertEqual(json.loads(self.read_record()["binary_sha256"]), sha256(fake_loadout("1.2.4")))

    def test_a_checksum_tool_printing_anything_but_a_digest_installs_nothing(self):
        self.write_exe(self.fakebin / "sha256sum", "#!/bin/sh\ncat >/dev/null\nprintf '%s  -\\n' '\\0123'\n")
        result = self.install()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("could not hash", result.stderr)
        self.assertFalse(self.dest.exists())
        self.assertFalse(self.record.exists())

    def test_record_names_the_canonical_binary_path(self):
        real_home = self.tmp / "real-home"
        real_home.mkdir()
        os.rmdir(self.home)
        self.home.symlink_to(real_home)
        self.install_ok()
        canonical = os.path.realpath(real_home / ".local" / "bin" / "loadout")
        self.assertNotEqual(str(self.dest), canonical)
        self.assertEqual(self.read_record()["binary"], f'"{canonical}"')

    def test_managed_install_is_upgraded_in_place(self):
        self.install_ok()
        self.publish(fake_loadout("1.2.4"))
        self.install_ok()
        self.assertEqual(self.dest.read_bytes(), fake_loadout("1.2.4"))
        self.assertEqual(self.read_record()["version"], '"1.2.4"')
        self.assertEqual(self.read_record()["binary_sha256"], f'"{sha256(fake_loadout("1.2.4"))}"')

    def test_checksum_mismatch_keeps_the_managed_binary_and_record(self):
        self.install_ok()
        before = self.snapshot()
        self.publish(fake_loadout("1.2.4"), sidecar=f"{'0' * 64}  {ASSET}\n")
        self.assert_refused(self.install(), "checksum mismatch", before)

    def test_unmanaged_binary_is_never_replaced(self):
        self.bin_dir.mkdir(parents=True)
        self.dest.write_text("hand-copied")
        before = self.snapshot()
        self.assert_refused(self.install(), "was not installed by this installer", before)

    def test_binary_with_a_mismatching_record_is_never_replaced(self):
        def tamper_binary():
            self.dest.write_bytes(fake_loadout() + b"# edited\n")

        def edit(old, new):
            return lambda: self.record.write_bytes(self.record.read_bytes().replace(old.encode(), new.encode()))

        def raw(change):
            return lambda: self.record.write_bytes(change(self.record.read_bytes()))

        cases = {
            "binary edited after install": tamper_binary,
            "record missing": lambda: self.record.unlink(),
            "other installer": edit('installer = "install.sh"', 'installer = "install.ps1"'),
            "other target": edit(f'target = "{TARGET}"', 'target = "aarch64-unknown-linux-musl"'),
            "other path": edit('binary = "', 'binary = "/elsewhere'),
            "unknown key": lambda: self.record.write_text(self.record.read_text() + 'extra = "x"\n'),
            "duplicate key": lambda: self.record.write_text(self.record.read_text() + 'version = "9.9.9"\n'),
            "not a string": edit('version = "1.2.3"', "version = 1.2.3"),
            "carriage return": edit("\n", "\r\n"),
            "schema 2": edit("schema = 1", "schema = 2"),
            "quoted schema 1": edit("schema = 1", 'schema = "1"'),
            "quoted schema 2": edit("schema = 1", 'schema = "2"'),
            "missing final LF": raw(lambda data: data[:-1]),
            "invalid UTF-8": raw(lambda data: data.replace(b"Do not edit.", b"Do not edit.\xff")),
            "NUL byte": raw(lambda data: data.replace(b"Do not edit.", b"Do not\x00 edit.")),
            "byte order mark": raw(lambda data: b"\xef\xbb\xbf" + data),
            "evaluated text": edit('version = "1.2.3"', 'version = "$(touch pwned)"\nx=`touch pwned`'),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                self.publish()
                self.install_ok()
                mutate()
                before = self.snapshot()
                self.publish(fake_loadout("1.2.4"))
                self.assert_refused(self.install(), "was not installed by this installer", before)
                self.assertFalse((self.bin_dir / "pwned").exists())
                self.assertFalse((self.cwd / "pwned").exists())

    def test_orphan_record_without_a_binary_is_replaced(self):
        self.bin_dir.mkdir(parents=True)
        self.record.write_text("stale\n")
        self.install_ok()
        self.assertEqual(self.read_record()["installer"], '"install.sh"')

    def test_symlinked_binary_or_record_is_left_alone(self):
        other = self.tmp / "other-file"
        other.write_text("someone else's")
        for name in ("loadout", "loadout.install.toml"):
            with self.subTest(name):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                self.bin_dir.mkdir(parents=True)
                (self.bin_dir / name).symlink_to(other)
                before = self.snapshot()
                self.assert_refused(self.install(), "another installer may own it", before)
                self.assertEqual(other.read_text(), "someone else's")

    def test_other_staging_files_survive_success_and_failure(self):
        self.bin_dir.mkdir(parents=True)
        sentinels = {
            ".loadout.new.sentinel": b"other invocation",
            ".loadout.install.toml.new.sentinel": b"other invocation",
            "loadout.exe.new": b"fixed name",
            "loadout.old": b"fixed name",
        }
        for name, data in sentinels.items():
            (self.bin_dir / name).write_bytes(data)
        result = self.install(FAKE_ARCH="arm64")
        self.assertEqual(result.returncode, 1)
        self.install_ok()
        for name, data in sentinels.items():
            self.assertEqual((self.bin_dir / name).read_bytes(), data, name)
        self.assertEqual(
            sorted(p.name for p in self.bin_dir.iterdir()),
            sorted(list(sentinels) + [LOCK, "loadout", "loadout.install.toml"]),
        )

    @unittest.skipIf(fcntl is None, "needs fcntl")
    def test_a_held_install_lock_changes_nothing(self):
        self.install_ok()
        self.publish(fake_loadout("1.2.4"))
        before = self.snapshot()
        handle = self.hold_lock()
        self.assert_refused(self.install(), "another install or loadout self-update is changing", before)
        handle.close()
        self.install_ok()
        self.assertEqual(self.dest.read_bytes(), fake_loadout("1.2.4"))

    @unittest.skipIf(fcntl is None, "needs fcntl")
    @unittest.skipUnless(shutil.which("perl"), "needs perl")
    def test_without_flock_the_lock_is_taken_with_perl(self):
        path = self.path_without("flock")
        self.install_ok(path=path)
        self.publish(fake_loadout("1.2.4"))
        before = self.snapshot()
        handle = self.hold_lock()
        self.assert_refused(self.install(path=path), "another install or loadout self-update is changing", before)
        handle.close()
        self.install_ok(path=path)
        self.assertEqual(self.dest.read_bytes(), fake_loadout("1.2.4"))

    def test_without_flock_or_perl_nothing_is_installed(self):
        result = self.install(path=self.path_without("flock", "perl"))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("flock or perl is required", result.stderr)
        self.assertFalse(self.dest.exists())
        self.assertFalse(self.record.exists())

    def test_a_linked_or_folder_lock_file_is_refused(self):
        other = self.tmp / "other-file"
        other.write_text("someone else's")
        for name, make in (("symlink", lambda: self.lock.symlink_to(other)), ("folder", lambda: self.lock.mkdir())):
            with self.subTest(name):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                self.bin_dir.mkdir(parents=True)
                make()
                before = self.snapshot()
                self.assert_refused(self.install(), "can't be locked", before)
                self.assertEqual(other.read_text(), "someone else's")

    @unittest.skipIf(fcntl is None, "needs fcntl")
    def test_an_install_that_finished_during_the_download_is_never_overwritten(self):
        self.install_ok()
        self.publish(fake_loadout("1.2.4"))
        newer = fake_loadout("1.2.5")

        def finish_newer_install():
            # What another install leaves, written under the lock as it would be.
            with open(self.lock, "a") as handle:
                fcntl.flock(handle, fcntl.LOCK_EX)
                text = self.record.read_text()
                self.dest.write_bytes(newer)
                self.record.write_text(
                    text.replace(sha256(fake_loadout()), sha256(newer)).replace('version = "1.2.3"', 'version = "1.2.5"')
                )

        self.server.hooks[f"/{ASSET}"] = finish_newer_install
        result = self.install()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("changed while this install was downloading", result.stderr)
        self.assertEqual(self.dest.read_bytes(), newer)
        self.assertEqual(self.read_record()["version"], '"1.2.5"')
        self.assertEqual(sorted(p.name for p in self.bin_dir.iterdir()), [LOCK, "loadout", "loadout.install.toml"])

    def test_a_binary_edited_during_the_download_is_left_alone(self):
        self.install_ok()
        self.publish(fake_loadout("1.2.4"))
        self.server.hooks[f"/{ASSET}"] = lambda: self.dest.write_bytes(b"edited by hand")
        result = self.install()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(self.dest.read_bytes(), b"edited by hand")

    def test_sidecar_must_be_exactly_one_line(self):
        archive_sha = sha256(self.archive)
        line = f"{archive_sha}  {ASSET}"
        for name, sidecar in {
            "other asset": f"{archive_sha}  loadout-other.tar.gz\n".encode(),
            "extra line": f"{line}\nextra\n".encode(),
            "extra newline": f"{line}\n\n".encode(),
            "crlf": f"{line}\r\n".encode(),
            "no newline": line.encode(),
            "NUL instead of LF": line.encode() + b"\x00",
            "uppercase hex": f"{archive_sha.upper()}  {ASSET}\n".encode(),
            "UTF-8 byte order mark": b"\xef\xbb\xbf" + f"{line}\n".encode(),
            "UTF-16": f"{line}\n".encode("utf-16"),
            "non-ASCII separator": f"{archive_sha}\u00a0 {ASSET}\n".encode(),
        }.items():
            with self.subTest(name):
                (self.served / f"{ASSET}.sha256").write_bytes(sidecar)
                self.assert_refused(self.install(), ".sha256", None)

    def test_archive_layout_is_checked_before_extraction(self):
        binary = ("file", fake_loadout())
        victim = self.tmp / "victim"
        victim.write_bytes(b"outside the release")
        for name, members in {
            "duplicate binary": release_members(fake_loadout()) + [(f"{PREFIX}/loadout", *binary)],
            "binary is a symlink": [(PREFIX, "dir", None), (f"{PREFIX}/loadout", "symlink", "/bin/sh")],
            "entry outside the prefix": release_members(fake_loadout()) + [("evil", *binary)],
            "parent traversal": release_members(fake_loadout()) + [(f"{PREFIX}/../evil", *binary)],
            "nested entry": release_members(fake_loadout()) + [(f"{PREFIX}/sub/loadout", *binary)],
            "missing binary": [(PREFIX, "dir", None), (f"{PREFIX}/README.md", "file", b"x")],
            "binary hardlinked inside": [
                (PREFIX, "dir", None),
                (f"{PREFIX}/README.md", "file", fake_loadout()),
                (f"{PREFIX}/loadout", "hardlink", f"{PREFIX}/README.md"),
            ],
            "binary hardlinked outside": [(PREFIX, "dir", None), (f"{PREFIX}/loadout", "hardlink", str(victim))],
            "binary hardlinked upward": [(PREFIX, "dir", None), (f"{PREFIX}/loadout", "hardlink", "../../victim")],
            "extra hardlink entry": release_members(fake_loadout()) + [(f"{PREFIX}/extra", "hardlink", str(victim))],
        }.items():
            with self.subTest(name):
                self.publish(members=members)
                result = self.install()
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertFalse(self.dest.exists())
                self.assertFalse((self.home / "evil").exists())
                self.assertEqual(victim.read_bytes(), b"outside the release")
                self.assertEqual(victim.stat().st_nlink, 1)

    def test_requested_tag_must_match_the_binary_version_exactly(self):
        self.publish(fake_loadout("1.2.3-RC.1"))
        self.assert_refused(self.install("--version", "v1.2.3-rc.1"), "contains loadout 1.2.3-RC.1", None)
        self.publish(fake_loadout("1.2.3"))
        self.assert_refused(self.install("--version", "v9.9.9"), "contains loadout 1.2.3", None)

    def test_missing_release_asset_fails_without_installing(self):
        result = self.install(FAKE_ARCH="arm64")
        self.assert_refused(result, "download failed", None)
        self.assertIn("loadout-aarch64-unknown-linux-musl.tar.gz", result.stderr)

    def test_loopback_http_never_follows_redirects(self):
        self.server.redirects[f"/{ASSET}"] = f"/{ASSET}.moved"
        shutil.copy(self.served / ASSET, self.served / f"{ASSET}.moved")
        self.assert_refused(self.install(), "download failed", None)

    def test_wget_alone_is_not_enough(self):
        only = self.tmp / "wget-only"
        only.mkdir()
        self.write_exe(only / "uname", FAKE_UNAME)
        self.write_exe(only / "wget", "#!/bin/sh\nexit 0\n")
        (only / "git").symlink_to(shutil.which("git"))
        self.assert_refused(self.install(path=str(only)), "curl is required", None)

    def test_unsupported_platforms_are_refused(self):
        for env, message in (
            ({"FAKE_OS": "FreeBSD"}, "unsupported operating system FreeBSD"),
            ({"FAKE_ARCH": "riscv64"}, "unsupported CPU architecture riscv64"),
        ):
            with self.subTest(env=env):
                self.assert_refused(self.install(**env), message, None)

    def test_missing_git_prints_guidance_and_installs_nothing(self):
        for os_name, hint in (("Linux", "sudo apt install git"), ("Darwin", "xcode-select --install")):
            with self.subTest(os=os_name):
                result = self.install(path=str(self.fakebin), FAKE_OS=os_name)
                self.assert_refused(result, "git is required", None)
                self.assertIn(hint, result.stderr)

    def test_control_character_in_home_is_refused_before_any_change(self):
        home = self.tmp / "bad\nhome"
        home.mkdir()
        result = self.install(home=home)
        self.assertEqual(result.returncode, 1)
        self.assertIn("control character", result.stderr)
        self.assertFalse((home / ".local").exists())

    def test_loadout_url_and_extra_arguments_reach_init_unchanged(self):
        url = "https://example.invalid/me/loadout.git"
        result = self.install(url, "--name", "box one", "", "--no-service")
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(
            self.init_args.read_text().split("\n")[:-1],
            ["init", url, "--name", "box one", "", "--no-service"],
        )

    def test_progress_never_shows_url_credentials_or_init_arguments(self):
        secret = "SENTINEL-SECRET-7f3a"
        for url, shown in (
            (f"https://me:{secret}@example.invalid/me/loadout.git", "https://***@example.invalid/me/loadout.git"),
            (f"https://{secret}@example.invalid/me/loadout.git", "https://***@example.invalid/me/loadout.git"),
            ("git@github.com:me/loadout.git", "git@github.com:me/loadout.git"),
        ):
            with self.subTest(url=url):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                result = self.install(url, "--name", f"box-{secret}")
                self.assertEqual(result.returncode, 7, result.stderr)
                self.assertNotIn(secret, result.stdout + result.stderr)
                self.assertIn(f"init {shown} and the init arguments you gave", result.stderr)
                self.assertEqual(
                    self.init_args.read_text().split("\n")[:-1],
                    ["init", url, "--name", f"box-{secret}"],
                    "init still gets exactly what was given",
                )

    def test_usage_errors_exit_2_before_any_download(self):
        for args in (
            ("--", "-x"),
            ("--version", "1.2.3"),
            ("--version", "V1.2.3"),
            ("--version", "v1.2.3;rm"),
            ("--base-url", "http://example.com"),
            ("--base-url", "http://127.0.0.1:80@example.invalid"),
            ("--base-url", "http://127.0.0.1.example.invalid/"),
            ("--base-url", "http://localhost.example.invalid"),
            ("--base-url", "http://127.0.0.1:8x/"),
            ("--base-url", "http://127.0.0.1:80:90/"),
            ("--base-url", "https://example.com/a?b"),
            ("--base-url", "https://example.com/#f"),
            ("--base-url", "https://"),
            ("--base-url", "ftp://127.0.0.1/"),
            ("--unknown",),
        ):
            with self.subTest(args=args):
                result = self.install(*args)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIsNone(self.snapshot())


if __name__ == "__main__":
    unittest.main()
