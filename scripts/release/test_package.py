import hashlib
import importlib.util
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

spec = importlib.util.spec_from_file_location("package", Path(__file__).with_name("package.py"))
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)

EPOCH = 1761264000
ENTRIES = [
    ("loadout-t/README.md", b"readme\n", 0o644),
    ("loadout-t/loadout", b"\x7fELF binary", 0o755),
]


class ArchiveTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())

    def build(self, target, subdir):
        return package.write_archive(self.tmp / subdir, "loadout-t", ENTRIES, target, EPOCH)

    def test_unix_archive_is_byte_identical_across_runs(self):
        first, _ = self.build("x86_64-unknown-linux-musl", "a")
        second, _ = self.build("x86_64-unknown-linux-musl", "b")
        self.assertEqual(first.name, "loadout-t.tar.gz")
        self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_windows_archive_is_byte_identical_across_runs(self):
        first, _ = self.build("x86_64-pc-windows-msvc", "a")
        second, _ = self.build("x86_64-pc-windows-msvc", "b")
        self.assertEqual(first.name, "loadout-t.zip")
        self.assertEqual(first.read_bytes(), second.read_bytes())

    def test_tar_members_have_fixed_metadata(self):
        archive, _ = self.build("aarch64-apple-darwin", "a")
        with tarfile.open(archive) as tar:
            infos = {i.name: i for i in tar.getmembers()}
            self.assertEqual(sorted(infos), ["loadout-t", "loadout-t/README.md", "loadout-t/loadout"])
            self.assertTrue(infos["loadout-t"].isdir())
            self.assertEqual(infos["loadout-t/loadout"].mode, 0o755)
            self.assertEqual(infos["loadout-t/README.md"].mode, 0o644)
            for info in infos.values():
                self.assertEqual((info.mtime, info.uid, info.gid, info.uname), (EPOCH, 0, 0, ""))
            self.assertEqual(tar.extractfile("loadout-t/loadout").read(), b"\x7fELF binary")

    def test_zip_members_keep_unix_modes(self):
        archive, _ = self.build("aarch64-pc-windows-msvc", "a")
        with zipfile.ZipFile(archive) as zf:
            self.assertEqual(zf.namelist(), ["loadout-t/", "loadout-t/README.md", "loadout-t/loadout"])
            self.assertEqual(zf.getinfo("loadout-t/loadout").external_attr >> 16, 0o100755)
            self.assertEqual(zf.read("loadout-t/loadout"), b"\x7fELF binary")
            self.assertIsNone(zf.testzip())

    def test_checksum_file_matches_sha256sum_format(self):
        archive, checksum = self.build("x86_64-unknown-linux-musl", "a")
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.assertEqual(checksum.name, "loadout-t.tar.gz.sha256")
        self.assertEqual(checksum.read_bytes(), f"{digest}  loadout-t.tar.gz\n".encode())


class SmokeTest(unittest.TestCase):
    def test_required_smoke_fails_when_host_cannot_run_target(self):
        with self.assertRaisesRegex(package.ReleaseError, "cannot execute"):
            package.smoke_test(Path("loadout"), "riscv64gc-unknown-none-elf", "0.1.0", required=True)

    def test_optional_smoke_skips_when_host_cannot_run_target(self):
        package.smoke_test(Path("loadout"), "riscv64gc-unknown-none-elf", "0.1.0")


if __name__ == "__main__":
    unittest.main()
