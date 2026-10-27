#!/usr/bin/env python3
"""Controls for the G9 `sync` check in guest/linux_acceptance.py (standing 246): the whole
g9_self_update group runs against real throwaway Git repositories (a bare remote, the fixture work
tree and the clone) in a scratch HOME, with a scripted harness whose `sync` is played by this
script. No loadout CLI runs and nothing outside the scratch folders is touched.

  python3 -I -B tests/vm-acceptance/controls/g9_sync_controls.py [DRIVER [PREDECESSOR]]

DRIVER defaults to guest/linux_acceptance.py beside this folder. PREDECESSOR is a copy of the driver
before the repair (for example `git show 2401701:tests/vm-acceptance/guest/linux_acceptance.py`);
with it, the original failure is reproduced first: C16 sets `cli = true`, the loop's first commit
`C17 cli true` changes nothing, git exits 1, and neither G9 sync case runs.
"""

import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]
DRIVER = Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "guest" / "linux_acceptance.py"
PREDECESSOR = Path(sys.argv[2]) if len(sys.argv) > 2 else None
FIXTURE = HERE / "fixtures" / "loadout"
VERSION = "0.1.1-test.3"
CASE = "G9-sync-never-self-updates-cli-"

failures = 0


def check(name, ok, detail=""):
    global failures
    print("%s %s%s" % ("ok  " if ok else "FAIL", name, (": " + str(detail)[:400]) if detail and not ok else ""))
    failures += 0 if ok else 1


def load(path, home):
    """The driver imported with HOME at HOME, so every path it derives (and every default argument
    bound at import, like head()'s clone) lies in the scratch folder."""
    os.environ["HOME"] = str(home)
    for name in ("XDG_STATE_HOME", "XDG_CONFIG_HOME"):
        os.environ.pop(name, None)
    spec = importlib.util.spec_from_file_location("linux_acceptance_%d" % id(home), path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    assert module.HOME == home and str(module.CLONE).startswith(str(home)), (module.HOME, module.CLONE)
    return module


def setup(d):
    """A bootstrapped account: remote and work tree at C0 (the fixture), the clone at C0 with its
    hello.txt applied, an installed CLI and its record."""
    d.git("init", "-q", "--bare", "-b", "main", str(d.REMOTE), cwd=d.HOME)
    d.git("clone", "-q", str(d.REMOTE), str(d.WORK), cwd=d.HOME)
    shutil.copytree(FIXTURE, d.WORK, dirs_exist_ok=True)
    d.git("add", "-A")
    d.git("commit", "-q", "-m", "C0")
    d.git("push", "-q", "origin", "HEAD:main")
    d.CLONE.parent.mkdir(parents=True)
    d.git("clone", "-q", str(d.REMOTE), str(d.CLONE), cwd=d.HOME)
    target = d.HOME / ".config/loadout-e2e/hello.txt"
    target.parent.mkdir(parents=True)
    shutil.copyfile(d.CLONE / "files/hello.txt", target)
    d.LOCAL_BIN.mkdir(parents=True)
    d.CLI.write_bytes(b"\x7fELF fake loadout " + VERSION.encode())
    (d.LOCAL_BIN / "loadout.install.toml").write_text('version = "%s"\nbinary_sha256 = "x"\n' % VERSION)
    return target


def syncer(d, target, pull=True, apply=True, say="", touch_cli=False):
    """A scripted `loadout sync`: fast-forward the clone, apply hello.txt, optionally misbehave."""
    def sync():
        if pull:
            d.sh(["git", "-C", d.CLONE, "pull", "-q", "--ff-only"])
        if apply:
            shutil.copyfile(d.CLONE / "files/hello.txt", target)
        if touch_cli:
            d.CLI.write_bytes(d.CLI.read_bytes() + b"!")
        return 0, say
    return sync


class FakeHarness:
    """Records rows like Harness. Only `sync` is played (by SYNC); every other case is recorded as
    not played, without its asserts, since no CLI exists here."""

    def __init__(self, d, sync):
        self.d, self.sync = d, sync
        self.expected = {"version": VERSION, "managed_linux": False, "network": "offline"}
        self.inputs = d.HOME / "inputs"
        self.rows = []

    def requires(self, group, *needs):
        return True

    def env(self, extra=None, isolated=None):
        return {}

    def record(self, case, status, **fields):
        self.rows.append(dict(case=case, status=status, **fields))

    def run(self, case, argv, expect, *, env=None, timeout=600, cwd=None, asserts=None, observe=False):
        if [str(a) for a in argv] != [str(self.d.CLI), "sync"]:
            self.record(case, "not-played")
            return None
        code, text = self.sync()
        checks = asserts(code, text) if asserts else []
        ok = code in expect and all(passed for _, passed in checks)
        self.record(case, "pass" if ok else "fail", checks=[(name, bool(passed)) for name, passed in checks])
        return code

    def sync_rows(self):
        return [r for r in self.rows if r["case"].startswith(CASE)]

    def failed_checks(self):
        return {r["case"][len(CASE):]: [n for n, passed in r.get("checks", []) if not passed] for r in self.sync_rows()}


def account(path, scratch, name):
    home = Path(scratch) / name / "home"
    home.mkdir(parents=True)
    d = load(path, home)
    return d, setup(d)


def subjects(d):
    return [d.git("log", "-1", "--format=%s", rev) for rev in d.git("rev-list", "HEAD").split()]


with tempfile.TemporaryDirectory() as scratch:
    git_config = Path(scratch) / "gitconfig"
    git_config.write_text("")
    os.environ.update(GIT_CONFIG_GLOBAL=str(git_config), GIT_CONFIG_NOSYSTEM="1")

    # 1. The predecessor's failure, reproduced with real git.
    if PREDECESSOR:
        d, target = account(PREDECESSOR, scratch, "predecessor")
        h = FakeHarness(d, syncer(d, target))
        try:
            d.g9_self_update(h)
            check("predecessor: g9_self_update raises at the empty C17 commit", False, "no exception")
        except RuntimeError as error:
            text = str(error)
            check("predecessor: g9_self_update raises at the empty C17 commit",
                  "'commit', '-q', '-m', 'C17 cli true']" in text and "-> 1:" in text, text)
        check("predecessor: neither G9 sync case ran", h.sync_rows() == [], h.sync_rows())
        check("predecessor: work tree left at C16 with cli = true",
              subjects(d)[0] == "C16 cli on" and "\ncli = true\n" in d.read(d.WORK / "loadout.toml"), subjects(d))

    # 2. The repair: both values commit, sync, and pass every check.
    d, target = account(DRIVER, scratch, "repaired")
    h = FakeHarness(d, syncer(d, target))
    d.g9_self_update(h)
    rows = h.sync_rows()
    check("repaired: both G9 sync cases ran, false first (C16 had set true)",
          [r["case"] for r in rows] == [CASE + "false", CASE + "true"], [r["case"] for r in rows])
    check("repaired: both pass every check", all(r["status"] == "pass" for r in rows) and h.failed_checks() == {
        "false": [], "true": []}, rows)
    check("repaired: each pass names its five checks", all(len(r["checks"]) == 5 for r in rows), rows)
    check("repaired: three distinct commits after C0",
          subjects(d)[:3] == ["C17 cli true", "C17 cli false", "C16 cli on"], subjects(d))
    check("repaired: clone at the last commit, hello.txt applied from it",
          d.head() == d.git("rev-parse", "HEAD") and d.read(target) == d.hello("c17-true"))

    # 3. Each check is observable: a misbehaving sync fails exactly the check it breaks.
    cases = [
        ("a sync that does nothing", dict(pull=False, apply=False),
         ["clone at C17 cli %s", "manifest names cli = %s", "loadout applied"]),
        ("a sync that fetches but never applies", dict(apply=False), ["loadout applied"]),
        ("a sync that prints a self-update line", dict(say="cli: updated 0.1.1-test.1 to %s\n" % VERSION),
         ["no self-update line"]),
        ("a sync that changes the CLI", dict(touch_cli=True), ["binary and record unchanged"]),
    ]
    for n, (label, behaviour, broken) in enumerate(cases):
        d, target = account(DRIVER, scratch, "negative-%d" % n)
        h = FakeHarness(d, syncer(d, target, **behaviour))
        d.g9_self_update(h)
        want = {v: [b % v if "%s" in b else b for b in broken] for v in ("false", "true")}
        if not behaviour.get("pull", True):
            # A clone left at C0 holds the fixture manifest, which already says `cli = false`: for that
            # value only the clone and applied-file checks can tell C0 from C17, and both fail.
            want["false"].remove("manifest names cli = false")
        check("negative: %s fails exactly %s" % (label, broken),
              all(r["status"] == "fail" for r in h.sync_rows()) and h.failed_checks() == want, h.failed_checks())

    # 4. The order follows the committed manifest: from `cli = false` it starts with true.
    d, target = account(DRIVER, scratch, "from-false")
    h = FakeHarness(d, syncer(d, target))
    record = d.LOCAL_BIN / "loadout.install.toml"
    d.g9_sync_cli_values(h, record, (d.sha256(d.CLI), d.read(record)))
    check("order: from the fixture's cli = false, true then false, both pass",
          [(r["case"], r["status"]) for r in h.sync_rows()] == [(CASE + "true", "pass"), (CASE + "false", "pass")],
          h.sync_rows())

    # 5. commit_checked refuses an empty commit, and the loop records it instead of crashing.
    d, target = account(DRIVER, scratch, "empty")
    before = (d.git("rev-parse", "HEAD"), d.git("ls-remote", str(d.REMOTE), "main"))
    try:
        d.commit_checked("C17 again", {"loadout.toml": d.read(d.WORK / "loadout.toml")})
        check("commit_checked: an unchanged edit raises EmptyCommit", False, "no exception")
    except d.EmptyCommit as error:
        check("commit_checked: an unchanged edit raises EmptyCommit naming it", "C17 again" in str(error), error)
    check("commit_checked: nothing committed or pushed",
          (d.git("rev-parse", "HEAD"), d.git("ls-remote", str(d.REMOTE), "main")) == before)
    h = FakeHarness(d, syncer(d, target))

    def refuse(message, edits, cwd=None):
        raise d.EmptyCommit("%r changes nothing" % message)

    d.commit_checked = refuse
    record = d.LOCAL_BIN / "loadout.install.toml"
    d.g9_sync_cli_values(h, record, (d.sha256(d.CLI), d.read(record)))
    check("loop: an empty commit is a FAIL row for that value, and the other value still runs",
          [(r["case"], r["status"], r.get("detail", "").startswith("harness:")) for r in h.sync_rows()]
          == [(CASE + "true", "fail", True), (CASE + "false", "fail", True)], h.sync_rows())

print("%d control(s) failed" % failures if failures else "all controls passed")
sys.exit(1 if failures else 0)
