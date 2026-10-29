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
# The effective UID install.sh sees, so the Homebrew tests run the same whether the host runs them as root.
FAKE_ID = """#!/bin/sh
[ "$1" = -u ] && echo "${FAKE_UID:-1000}"
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
        self.write_exe(self.fakebin / "id", FAKE_ID)
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
        self.write_exe(tools / "id", FAKE_ID)
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

    def install(self, *args, path=None, home=None, homebrew=False, **env):
        variables = dict(
            os.environ,
            HOME=str(home or self.home),
            PATH=path or f"{self.fakebin}{os.pathsep}{os.environ['PATH']}",
            LOADOUT_TEST_ARGS=str(self.init_args),
            # The host's own Homebrew must not stand in for the one a test sets up.
            **{"HOMEBREW_PREFIX": "", **env},
        )
        return subprocess.run(
            # Only the tests about Homebrew let a host without it run Homebrew's installer.
            ["/bin/sh", str(SCRIPT), "--base-url", self.base, *(() if homebrew else ("--no-homebrew",)), *args],
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

    def failing_sha256sum(self, digest=None, after=None):
        """Wraps the real sha256sum. For input whose digest is `digest` (any input when None), once
        the file `after` exists (at once when None), it prints that valid digest, then exits 1."""
        self.write_exe(
            self.fakebin / "sha256sum",
            '#!/bin/sh\n'
            'out=$("$REAL_SHA256SUM") || exit $?\n'
            'case $out in "${FAIL_DIGEST:-${out%% *}}  "*)\n'
            '  if [ -z "$FAIL_AFTER" ] || [ -e "$FAIL_AFTER" ]; then printf \'%s\\n\' "$out"; exit 1; fi ;;\n'
            'esac\n'
            'printf \'%s\\n\' "$out"\n',
        )
        return dict(REAL_SHA256SUM=shutil.which("sha256sum"), FAIL_DIGEST=digest or "", FAIL_AFTER=str(after or ""))

    @unittest.skipUnless(shutil.which("sha256sum"), "wraps the real sha256sum")
    def test_a_checksum_tool_that_fails_after_printing_a_digest_installs_nothing(self):
        result = self.install(**self.failing_sha256sum())
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("could not hash", result.stderr)
        self.assertIn("sha256sum failed", result.stderr)
        self.assertFalse(self.dest.exists())
        self.assertFalse(self.record.exists())

    @unittest.skipUnless(shutil.which("sha256sum"), "wraps the real sha256sum")
    def test_a_hash_failure_on_the_installed_files_is_an_error_that_changes_nothing(self):
        marker = self.tmp / "download-started"
        cases = {
            "installed binary, first check": ("binary", None),
            "record, state before the download": ("record", None),
            "record, final state under the lock": ("record", marker),
        }
        for name, (which, after) in cases.items():
            with self.subTest(name):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                if marker.exists():
                    marker.unlink()
                (self.fakebin / "sha256sum").unlink(missing_ok=True)
                self.publish()
                self.install_ok()
                before = self.snapshot()
                target = sha256((self.dest if which == "binary" else self.record).read_bytes())
                self.publish(fake_loadout("1.2.4"))
                self.server.hooks[f"/{ASSET}"] = lambda: marker.touch()
                result = self.install(**self.failing_sha256sum(target, after))
                self.server.hooks.pop(f"/{ASSET}", None)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("could not hash", result.stderr)
                self.assertNotIn("was not installed by this installer", result.stderr)
                self.assertNotIn("changed while this install was downloading", result.stderr)
                self.assertEqual(self.snapshot(), before, "nothing is published")
                if after:
                    self.assertTrue(marker.exists(), "the failure came after the download began")

        # The same wrapper aimed at nothing it hashes is transparent: the upgrade goes through.
        (self.fakebin / "sha256sum").unlink()
        result = self.install(**self.failing_sha256sum("f" * 64))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.dest.read_bytes(), fake_loadout("1.2.4"))

    @unittest.skipIf(hasattr(os, "geteuid") and os.geteuid() == 0, "root can read any file")
    def test_an_unreadable_installed_binary_is_an_error_not_unmanaged(self):
        self.install_ok()
        before = self.snapshot()
        self.publish(fake_loadout("1.2.4"))
        self.dest.chmod(0o000)
        try:
            result = self.install()
        finally:
            self.dest.chmod(0o755)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("could not hash", result.stderr)
        self.assertNotIn("was not installed by this installer", result.stderr)
        self.assertEqual(self.snapshot(), before)

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

    def publish_recording_path(self, target="x86_64-apple-darwin"):
        # A loadout that records the PATH init runs with.
        prefix = f"loadout-{target}"
        binary = fake_loadout().replace(b"exit 7", b'printf "%s" "$PATH" > "$LOADOUT_TEST_ARGS.path"\nexit 7')
        archive = tar_gz([(prefix, "dir", None), (f"{prefix}/loadout", "file", binary)])
        (self.served / f"{prefix}.tar.gz").write_bytes(archive)
        (self.served / f"{prefix}.tar.gz.sha256").write_text(f"{sha256(archive)}  {prefix}.tar.gz\n")

    @property
    def brew_prefix(self):
        return self.tmp / "homebrew"

    def serve_homebrew_installer(self, body=None):
        # Like Homebrew's real installer, this puts brew in a prefix that isn't on PATH, and its
        # shellenv adds the prefix's bin, which init must then see.
        if body is None:
            bin_dir = self.brew_prefix / "bin"
            bin_dir.mkdir(parents=True)
            fake_brew = self.tmp / "fake-brew"
            fake_brew.write_text(f'#!/bin/sh\n[ "$1" = shellenv ] && echo \'export PATH="{bin_dir}:$PATH"\'\n')
            # cat and chmod only: the test PATH holds just the tools install.sh itself runs.
            body = (
                f"cat '{fake_brew}' > '{bin_dir}/brew'\n"
                f"chmod 755 '{bin_dir}/brew'\n"
                f": > '{self.tmp}/homebrew-ran'\n"
            )
        (self.served / "homebrew-install.sh").write_text("#!/bin/bash\n" + body)

    def skip_if_host_has_homebrew(self):
        # find_brew also looks in Homebrew's default prefixes, which a test PATH can't hide.
        prefixes = ("/opt/homebrew/bin/brew", "/usr/local/bin/brew", "/home/linuxbrew/.linuxbrew/bin/brew",
                    str(self.home / ".linuxbrew" / "bin" / "brew"))
        if any(Path(p).exists() for p in prefixes):
            self.skipTest("this host has Homebrew in a default prefix")

    def test_macos_without_homebrew_runs_its_installer_then_init_with_brew_on_path(self):
        self.skip_if_host_has_homebrew()
        self.publish_recording_path()
        self.serve_homebrew_installer()
        result = self.install("https://example.invalid/me/loadout.git", path=self.path_without(), homebrew=True,
                              FAKE_OS="Darwin", HOMEBREW_PREFIX=str(self.brew_prefix), SHELL="/bin/zsh")
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertTrue((self.tmp / "homebrew-ran").exists())
        self.assertIn("running its installer", result.stderr)
        self.assertIn(str(self.brew_prefix / "bin"), Path(f"{self.init_args}.path").read_text().split(":"))
        # And it stays there for later shells, such as one that runs init by hand.
        self.assertIn(f'eval "$({self.brew_prefix}/bin/brew shellenv)"', (self.home / ".zprofile").read_text())

    def test_an_installed_brew_off_path_is_put_on_path_for_init(self):
        self.skip_if_host_has_homebrew()
        self.publish_recording_path()
        self.serve_homebrew_installer()
        (self.served / "homebrew-install.sh").unlink()
        self.write_exe(self.brew_prefix / "bin" / "brew", (self.tmp / "fake-brew").read_text())
        result = self.install("https://example.invalid/me/loadout.git", path=self.path_without(), homebrew=True,
                              FAKE_OS="Darwin", HOMEBREW_PREFIX=str(self.brew_prefix))
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertNotIn("running its installer", result.stderr)
        self.assertIn(str(self.brew_prefix / "bin"), Path(f"{self.init_args}.path").read_text().split(":"))

    def test_a_failed_homebrew_install_installs_nothing(self):
        self.skip_if_host_has_homebrew()
        self.publish_recording_path()
        self.serve_homebrew_installer(body="exit 3\n")
        result = self.install(path=self.path_without(), homebrew=True, FAKE_OS="Darwin")
        self.assert_refused(result, "Homebrew's installer failed", None)

    def test_linux_without_homebrew_runs_its_installer_then_init_with_brew_on_path(self):
        self.skip_if_host_has_homebrew()
        self.publish_recording_path(TARGET)
        self.serve_homebrew_installer()
        result = self.install("https://example.invalid/me/loadout.git", path=self.path_without(), homebrew=True,
                              HOMEBREW_PREFIX=str(self.brew_prefix))
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertTrue((self.tmp / "homebrew-ran").exists())
        self.assertIn(str(self.brew_prefix / "bin"), Path(f"{self.init_args}.path").read_text().split(":"))

    def test_root_or_sudo_never_runs_homebrews_installer(self):
        self.skip_if_host_has_homebrew()
        self.serve_homebrew_installer()
        path = self.path_without()
        for os_name in ("Linux", "Darwin"):
            for env, guidance in (
                ({}, "Run this installer again as your own user, or pass --no-homebrew"),
                ({"SUDO_USER": "someone"}, "Run it again as someone, without sudo, or pass --no-homebrew"),
            ):
                with self.subTest(os=os_name, env=env):
                    result = self.install(path=path, homebrew=True, FAKE_OS=os_name, FAKE_UID="0", **env)
                    self.assert_refused(result, "Homebrew must not be installed as root", None)
                    self.assertIn(guidance, result.stderr)
                    self.assertNotIn("running its installer", result.stderr)
                    self.assertFalse((self.tmp / "homebrew-ran").exists())

    def test_root_installs_with_no_homebrew_or_an_existing_brew(self):
        with_brew = self.path_without()
        self.write_exe(Path(with_brew) / "brew", "#!/bin/sh\nexit 0\n")
        for args, path in (((), with_brew), (("--no-homebrew",), self.path_without("curl-kept"))):
            with self.subTest(args=args):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                result = self.install(*args, path=path, homebrew=True, FAKE_UID="0", SUDO_USER="someone")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn("running its installer", result.stderr)
                self.assertTrue(self.dest.exists())

    def test_the_profile_gets_each_path_line_once(self):
        for shell, os_name, name in (
            ("/bin/bash", "Linux", ".bashrc"),
            ("/bin/bash", "Darwin", ".bash_profile"),
            ("/usr/bin/zsh", "Linux", ".zprofile"),
            ("/usr/bin/fish", "Linux", ".profile"),
        ):
            with self.subTest(shell=shell, os=os_name):
                if os_name == "Darwin":
                    self.publish_recording_path()
                shutil.rmtree(self.home, ignore_errors=True)
                self.home.mkdir()
                for _ in range(2):
                    self.install_ok(SHELL=shell, FAKE_OS=os_name)
                profile = (self.home / name).read_text()
                self.assertEqual(profile.count('export PATH="$HOME/.local/bin:$PATH"'), 1, profile)

    def test_bashrc_gets_the_path_lines_above_its_interactive_guard(self):
        guard = "case $- in\n    *i*) ;;\n      *) return;;\nesac\n"
        (self.home / ".bashrc").write_text(guard)
        self.install_ok(SHELL="/bin/bash", FAKE_OS="Linux")
        profile = (self.home / ".bashrc").read_text()
        line = 'export PATH="$HOME/.local/bin:$PATH"'
        self.assertLess(profile.index(line), profile.index(guard), profile)
        self.assertTrue(profile.endswith(guard), profile)
        self.assertEqual(profile.count(line), 1, profile)
        self.assertEqual(list(self.home.glob(".bashrc.loadout-*")), [])

    def test_no_modify_path_leaves_the_profile_alone(self):
        result = self.install_ok("--no-modify-path", SHELL="/bin/bash")
        self.assertFalse((self.home / ".bashrc").exists())
        self.assertIn('add this to your shell profile: export PATH="$HOME/.local/bin:$PATH"', result.stderr)

    def test_linux_without_git_stops_before_homebrew(self):
        self.serve_homebrew_installer(body=f": > '{self.tmp}/homebrew-ran'\n")
        result = self.install(path=str(self.fakebin), homebrew=True)
        self.assert_refused(result, "git is required", None)
        self.assertFalse((self.tmp / "homebrew-ran").exists())

    def test_macos_with_homebrew_or_no_homebrew_never_fetches_its_installer(self):
        self.publish_recording_path()
        with_brew = self.path_without()
        self.write_exe(Path(with_brew) / "brew", "#!/bin/sh\nexit 0\n")
        for args, path in (((), with_brew), (("--no-homebrew",), self.path_without("curl-kept"))):
            with self.subTest(args=args):
                shutil.rmtree(self.home / ".local", ignore_errors=True)
                result = self.install(*args, path=path, homebrew=True, FAKE_OS="Darwin")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn("running its installer", result.stderr)

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
