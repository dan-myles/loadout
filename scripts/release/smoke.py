#!/usr/bin/env python3
"""Smoke-test a packaged loadout archive as a user would receive it.

Verifies the .sha256 sidecar, extracts the archive, and runs the binary's help, doctor and
validate commands against the repo's foundation fixtures, with LOADOUT_HOME and LOADOUT_STATE_DIR
pointed at a throwaway directory so the host is never touched.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import zipfile
from pathlib import Path

PACKAGE = "loadout"
BACKGROUND = "loadout-background"


class SmokeError(Exception):
    pass


def verify_checksum(archive):
    sidecar = archive.with_name(archive.name + ".sha256")
    expected = sidecar.read_bytes().decode("ascii")
    actual = f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n"
    if expected != actual:
        raise SmokeError(f"{sidecar.name} does not match {archive.name}")
    print(f"PASS checksum {actual.split()[0]}")


def extract(archive, into):
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as zf:
            zf.extractall(into)
        prefix = archive.name[: -len(".zip")]
    else:
        with tarfile.open(archive) as tar:
            if hasattr(tarfile, "data_filter"):
                tar.extractall(into, filter="data")
            else:
                tar.extractall(into)
        prefix = archive.name[: -len(".tar.gz")]
    exe = PACKAGE + (".exe" if archive.name.endswith(".zip") else "")
    binary = into / prefix / exe
    if not binary.is_file():
        raise SmokeError(f"{prefix}/{exe} is missing from {archive.name}")
    if os.name != "nt" and not os.access(binary, os.X_OK):
        raise SmokeError(f"{prefix}/{exe} is not executable")
    if archive.name.endswith(".zip") and not binary.with_name(BACKGROUND + ".exe").is_file():
        raise SmokeError(f"{prefix}/{BACKGROUND}.exe is missing from {archive.name}")
    return binary


def background_version(binary, version):
    launcher = binary.with_name(BACKGROUND + ".exe")
    result = subprocess.run(
        [str(launcher), "--version"], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        universal_newlines=True,
    )
    expected = f"{BACKGROUND} {version}\n"
    if result.returncode != 0 or result.stdout != expected:
        raise SmokeError(f"{launcher.name} --version printed {result.stdout!r}, want {expected!r}")
    print(f"PASS {launcher.name} --version")


def expect(name, binary, args, code, env, contains=None):
    result = subprocess.run(
        [str(binary)] + args,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        universal_newlines=True,
    )
    if result.returncode != code:
        raise SmokeError(
            f"{name}: exit {result.returncode}, want {code}\n{result.stdout}{result.stderr}"
        )
    if contains and contains not in result.stdout:
        raise SmokeError(f"{name}: stdout lacks {contains!r}\n{result.stdout}")
    print(f"PASS {name} (exit {code})")
    return result.stdout


def doctor(binary, env):
    git = shutil.which("git", path=env["PATH"])
    report = json.loads(expect("doctor --json", binary, ["doctor", "--json"], 0 if git else 4, env))
    failed = [check["name"] for check in report["checks"] if check["status"] == "fail"]
    if failed != ([] if git else ["git"]) or report["healthy"] != bool(git):
        raise SmokeError(f"doctor: git on PATH={bool(git)}, but failed checks are {failed}")
    print(f"PASS doctor failed checks {failed} with git on PATH={bool(git)}")


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("archive", type=Path)
    parser.add_argument("--fixtures", type=Path, default=Path("tests/foundation"))
    args = parser.parse_args(argv)

    archive = args.archive.resolve()
    fixtures = args.fixtures.resolve()
    verify_checksum(archive)
    scratch = Path(tempfile.mkdtemp(prefix="loadout-smoke-"))
    try:
        binary = extract(archive, scratch / "unpacked")
        home = scratch / "home"
        home.mkdir()
        state = scratch / "state"
        env = dict(os.environ, LOADOUT_HOME=str(home), LOADOUT_STATE_DIR=str(state))

        expect("--help", binary, ["--help"], 0, env, contains="validate")
        version = expect("--version", binary, ["--version"], 0, env).split()[1]
        if binary.name.endswith(".exe"):
            background_version(binary, version)
        expect("unknown command", binary, ["no-such-command"], 2, env)
        doctor(binary, env)
        expect("validate complete", binary, ["validate", str(fixtures / "complete")], 0, env,
               contains="valid: ")
        valid = json.loads(expect("validate complete --json", binary,
                                  ["validate", str(fixtures / "complete"), "--json"], 0, env))
        if valid.get("valid") is not True:
            raise SmokeError("validate --json did not report valid: true")
        expect("validate invalid", binary, ["validate", str(fixtures / "invalid")], 3, env,
               contains="invalid: ")
        if state.exists() or any(home.iterdir()):
            raise SmokeError("doctor or validate wrote into the isolated home or state dir")
        print("PASS isolated home and state untouched")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except SmokeError as error:
        print(f"FAIL {error}", file=sys.stderr)
        sys.exit(1)
