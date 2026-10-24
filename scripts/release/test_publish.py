import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("publish.sh")
FAKE_GH = """#!/usr/bin/env bash
printf '%s\\n' "$*" >> "$GH_LOG"
if [[ $1 == release && $2 == view ]]; then
  case $FAKE_RELEASE in
    absent) echo "release not found" >&2; exit 1 ;;
    draft) echo true ;;
    published) echo false ;;
  esac
fi
"""


@unittest.skipUnless(shutil.which("bash"), "publish.sh needs bash")
class PublishTest(unittest.TestCase):
    def publish(self, tag, existing):
        tmp = Path(tempfile.mkdtemp())
        (tmp / "gh").write_text(FAKE_GH)
        (tmp / "gh").chmod(0o755)
        log = tmp / "gh.log"
        env = dict(
            os.environ,
            PATH=f"{tmp}{os.pathsep}{os.environ['PATH']}",
            GH_LOG=str(log),
            FAKE_RELEASE=existing,
        )
        result = subprocess.run(
            ["bash", str(SCRIPT), tag, "dist/a.tar.gz", "dist/SHA256SUMS"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
        )
        calls = log.read_text().splitlines() if log.exists() else []
        return result, [c for c in calls if not c.startswith("release view")]

    def test_new_prerelease_is_marked_prerelease_and_not_latest(self):
        result, calls = self.publish("v0.1.0-beta.1", "absent")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            calls,
            [
                "release create v0.1.0-beta.1 dist/a.tar.gz dist/SHA256SUMS --draft --verify-tag"
                " --generate-notes --title loadout v0.1.0-beta.1 --prerelease --latest=false"
            ],
        )

    def test_new_stable_release_is_not_a_prerelease(self):
        result, calls = self.publish("v1.2.3", "absent")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(calls), 1)
        self.assertTrue(calls[0].startswith("release create v1.2.3 "))
        self.assertTrue(calls[0].endswith(" --prerelease=false"))

    def test_build_metadata_alone_is_stable(self):
        _, calls = self.publish("v1.2.3+build.7", "absent")
        self.assertTrue(calls[0].endswith(" --prerelease=false"))

    def test_hyphen_inside_build_metadata_is_stable(self):
        _, calls = self.publish("v1.2.3+build-7", "absent")
        self.assertTrue(calls[0].endswith(" --prerelease=false"))

    def test_existing_draft_is_reclassified_before_upload(self):
        result, calls = self.publish("v0.2.0-rc.1", "draft")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            calls,
            [
                "release edit v0.2.0-rc.1 --prerelease --latest=false",
                "release upload v0.2.0-rc.1 dist/a.tar.gz dist/SHA256SUMS --clobber",
            ],
        )

    def test_existing_stable_draft_clears_prerelease_flag(self):
        _, calls = self.publish("v0.2.0", "draft")
        self.assertEqual(calls[0], "release edit v0.2.0 --prerelease=false")

    def test_published_release_is_never_modified(self):
        result, calls = self.publish("v1.0.0", "published")
        self.assertEqual(result.returncode, 1)
        self.assertIn("already published", result.stderr)
        self.assertEqual(calls, [])

    def test_tag_without_v_prefix_is_rejected(self):
        result, calls = self.publish("1.0.0", "absent")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
