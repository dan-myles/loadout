import gzip
import hashlib
import importlib.util
import os
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest import mock

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

    def test_tar_is_plain_ustar_with_no_extension_entries(self):
        # Self-update reads release archives raw and refuses GNU and PAX entries, so every header
        # here must be a USTAR folder ('5') or regular file ('0').
        archive, _ = self.build("x86_64-unknown-linux-musl", "a")
        raw = gzip.decompress(archive.read_bytes())
        offset, types = 0, []
        while raw[offset:offset + 512].strip(b"\0"):
            block = raw[offset:offset + 512]
            self.assertEqual(block[257:265], b"ustar\x0000")
            types.append(block[156:157])
            size = int(block[124:136].strip(b"\0 ") or b"0", 8)
            offset += 512 + (size + 511) // 512 * 512
        self.assertEqual(types, [b"5", b"0", b"0"])

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


class MembersTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.binary = self.root / "loadout"
        self.binary.write_bytes(b"bin")

    def symlink(self, link, target):
        try:
            os.symlink(target, link)
        except (OSError, NotImplementedError):
            self.skipTest("creating symlinks is not permitted here")

    def test_ships_regular_root_files_and_skips_missing_ones(self):
        (self.root / "README.md").write_text("readme")
        (self.root / "LICENSE-MIT").write_text("mit")
        _, entries = package.members(self.root, self.binary, "t")
        self.assertEqual(
            [name for name, _, _ in entries],
            ["loadout-t/LICENSE-MIT", "loadout-t/README.md", "loadout-t/loadout"],
        )

    def test_symlink_to_file_outside_checkout_is_rejected(self):
        outside = Path(tempfile.mkdtemp()) / "private.txt"
        outside.write_text("local secret")
        self.symlink(self.root / "README.md", outside)
        with self.assertRaisesRegex(package.ReleaseError, "README.md is a symlink"):
            package.members(self.root, self.binary, "t")

    def test_symlink_inside_checkout_is_also_rejected(self):
        (self.root / "docs.md").write_text("docs")
        self.symlink(self.root / "LICENSE", self.root / "docs.md")
        with self.assertRaisesRegex(package.ReleaseError, "LICENSE is a symlink"):
            package.members(self.root, self.binary, "t")


@unittest.skipIf(os.name == "nt", "fake binary is a POSIX shell script")
class SmokeVersionTest(unittest.TestCase):
    def smoke(self, printed, expected):
        binary = Path(tempfile.mkdtemp()) / "loadout"
        binary.write_text(f"#!/bin/sh\necho '{printed}'\n")
        binary.chmod(0o755)
        with mock.patch.object(package, "host_can_run", return_value=True):
            package.smoke_test(binary, "any-target", expected, required=True)

    def test_matching_version_passes(self):
        self.smoke("loadout 0.1.0", "0.1.0")

    def test_version_with_extra_build_info_passes(self):
        self.smoke("loadout 0.1.0 (abc1234 2026-10-24)", "0.1.0")

    def test_version_that_only_contains_expected_fails(self):
        for printed in ("loadout 10.1.0", "loadout 0.1.0-beta.1", "loadout 0.1.01"):
            with self.subTest(printed=printed), self.assertRaises(package.ReleaseError):
                self.smoke(printed, "0.1.0")

    def test_wrong_program_name_fails(self):
        with self.assertRaises(package.ReleaseError):
            self.smoke("other 0.1.0", "0.1.0")


class WindowsLauncherTest(unittest.TestCase):
    def test_only_windows_targets_ship_the_background_launcher(self):
        self.assertEqual(package.binary_names("x86_64-unknown-linux-musl"), ["loadout"])
        self.assertEqual(package.binary_names("aarch64-apple-darwin"), ["loadout"])
        for target in ("x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"):
            self.assertEqual(package.binary_names(target), ["loadout", "loadout-background"])

    def test_members_include_every_binary_as_executable(self):
        root = Path(tempfile.mkdtemp())
        (root / "loadout.exe").write_bytes(b"main")
        (root / "loadout-background.exe").write_bytes(b"launcher")
        (root / "README.md").write_text("readme")
        _, entries = package.members(root, root / "loadout.exe", "t", [root / "loadout-background.exe"])
        self.assertEqual(
            [(name, mode) for name, _, mode in entries],
            [
                ("loadout-t/README.md", 0o644),
                ("loadout-t/loadout-background.exe", 0o755),
                ("loadout-t/loadout.exe", 0o755),
            ],
        )


@unittest.skipIf(os.name == "nt", "fake binary is a POSIX shell script")
class BackgroundSmokeTest(unittest.TestCase):
    def smoke(self, printed):
        binary = Path(tempfile.mkdtemp()) / "loadout-background"
        binary.write_text(f"#!/bin/sh\nprintf '{printed}'\n")
        binary.chmod(0o755)
        with mock.patch.object(package, "host_can_run", return_value=True):
            package.smoke_background(binary, "any-target", "1.2.3", required=True)

    def test_exact_line_passes(self):
        self.smoke("loadout-background 1.2.3\\n")

    def test_anything_else_fails(self):
        for printed in (
            "loadout 1.2.3\\n",
            "loadout-background 1.2.30\\n",
            "loadout-background 1.2.3",
            "loadout-background 1.2.3 (extra)\\n",
        ):
            with self.subTest(printed=printed), self.assertRaises(package.ReleaseError):
                self.smoke(printed)


if __name__ == "__main__":
    unittest.main()
