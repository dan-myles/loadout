#!/usr/bin/env python3
"""Controls for the launchd deadline and soft-cancel cases' process evidence in
guest/linux_acceptance.py: the bounded `ps` identity query, complete captures, the termination
verdict, and both callers driven against a scripted launchd and process table. Nothing here
touches launchd or signals a real process; the real-process controls use only children this
script starts.

  python3 -I tests/vm-acceptance/controls/launchd_deadline_controls.py
"""

import importlib.util
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import types
from pathlib import Path

DRIVER = Path(__file__).resolve().parents[1] / "guest" / "linux_acceptance.py"
spec = importlib.util.spec_from_file_location("linux_acceptance", DRIVER)
d = importlib.util.module_from_spec(spec)
spec.loader.exec_module(d)

failures = 0


def check(name, ok, detail=""):
    global failures
    print("%s %s%s" % ("ok  " if ok else "FAIL", name, (": " + detail) if detail and not ok else ""))
    failures += 0 if ok else 1


START = "Mon Oct 26 10:08:58 2026"


def answers(returncode=0, stdout="", stderr="", raises=None):
    """A fake subprocess.run returning one answer, remembering how it was called."""
    calls = []

    def run(argv, **kwargs):
        calls.append((argv, kwargs))
        if raises is not None:
            raise raises
        return subprocess.CompletedProcess(argv, returncode, stdout, stderr)
    run.calls = calls
    return run


# process_identity: every answer class -------------------------------------------------------

for name, run, want in (
        ("absent: exit 1, no output", answers(1), "absent"),
        ("present: one start time", answers(0, START + "\n"), "present"),
        ("present: single-digit day", answers(0, "Sun Oct  5 01:02:03 2026    \n"), "present"),
        ("exit 2 is unavailable", answers(2), "unavailable"),
        ("exit 1 with stderr is unavailable", answers(1, "", "ps: process id too large: 999999"), "unavailable"),
        ("exit 0 with stderr is unavailable", answers(0, START, "ps: warning"), "unavailable"),
        ("start error is unavailable", answers(raises=OSError("no ps")), "unavailable"),
        ("timeout is unavailable", answers(raises=subprocess.TimeoutExpired(["ps"], 10)), "unavailable"),
        ("empty success is unavailable", answers(0, ""), "unavailable"),
        ("malformed output is unavailable", answers(0, "garbage\n"), "unavailable"),
        ("two start times are unavailable", answers(0, START + "\n" + START + "\n"), "unavailable")):
    status, value = d.process_identity(4242, run=run)
    check("identity %s" % name, status == want, "%s %r" % (status, value))
    if status == "present":
        check("identity %s keeps pid and start" % name, value[0] == 4242 and value[1] == value[1].strip())
probe = answers(1)
d.process_identity(7, run=probe)
check("identity query is bounded", probe.calls[0][1].get("timeout") == d.PS_SECONDS
      and probe.calls[0][0] == ["ps", "-o", "lstart=", "-p", "7"], repr(probe.calls))

# Real processes: live, reused pid, zombie, reaped -----------------------------------------------

child = subprocess.Popen(["sleep", "30"])
time.sleep(0.3)
status, ident = d.process_identity(child.pid)
check("real live child is present", status == "present", "%s %r" % (status, ident))
if status == "present":
    check("real live child is alive", d.termination_verdict([ident]) == ([ident], []))
    check("real pid with another start time counts as gone",
          d.termination_verdict([(child.pid, "Thu Jan  1 00:00:00 1970")]) == ([], []))
    os.kill(child.pid, signal.SIGKILL)
    time.sleep(0.5)
    check("real zombie is still alive", d.termination_verdict([ident]) == ([ident], []))
    child.wait()
    check("real reaped child is gone", d.termination_verdict([ident]) == ([], [])
          and d.process_identity(child.pid) == ("absent", None))
else:
    child.kill()
    child.wait()

# capture_identities and termination_verdict ----------------------------------------------------


def scripted(table):
    """query(pid) answering from table[pid], a list consumed in order (the last answer repeats)."""
    seen = {}

    def query(pid):
        script = table[pid]
        n = seen.get(pid, 0)
        seen[pid] = n + 1
        return script[min(n, len(script) - 1)]
    return query


present = lambda pid: ("present", (pid, START))  # noqa: E731
ids, missed = d.capture_identities("worker", [], query=scripted({}))
check("capture of nothing fails", ids == [] and missed == ["no worker process to capture"], repr(missed))
ids, missed = d.capture_identities("worker", [1, 2], query=scripted({1: [present(1)], 2: [("absent", None)]}))
check("capture with one already gone fails, not filtered", ids == [(1, START)] and len(missed) == 1
      and "pid 2 not captured" in missed[0], repr(missed))
ids, missed = d.capture_identities("installer", [3], query=scripted({3: [("unavailable", "ps -p 3 exit 2")]}))
check("capture with a failed query fails", ids == [] and "exit 2" in missed[0], repr(missed))
ids, missed = d.capture_identities("installer", [3, 4], query=scripted({3: [present(3)], 4: [present(4)]}))
check("complete capture has no failure", ids == [(3, START), (4, START)] and missed == [])
alive, unavailable = d.termination_verdict(
    [(1, START), (2, START), (3, START), (4, START)],
    query=scripted({1: [("absent", None)], 2: [("unavailable", "timeout")], 3: [present(3)],
                    4: [("present", (4, "Tue Oct 27 00:00:00 2026"))]}))
check("verdict: absent and reused gone, unavailable never gone, same identity alive",
      alive == [(3, START)] and unavailable == [((2, START), "timeout")], "%r %r" % (alive, unavailable))

# Both callers against a scripted launchd -------------------------------------------------------

KICK = 4242
RUN = "1793027000-%d-1" % KICK


class Harness:
    def __init__(self):
        self.records = {}

    def record(self, case, status, **fields):
        self.records[case] = (status, fields.get("detail", ""))

    def run(self, case, argv, expect, **kwargs):
        self.records[case] = ("pass", "fake")
        return 0, ""


def drive(case, table, listed, records, last):
    """Runs one caller with launchd, ps listings and identity answers scripted; returns its record."""
    scratch = Path(tempfile.mkdtemp())
    phase = {"after": False}
    fakes = {
        "FAKE_STATE": scratch / "fake-state", "CASES": scratch / "cases", "HOME": scratch,
        "launchd_drain": lambda timeout=900: True,
        "launchd_state": lambda: ({"loaded": True, "state": "not running", "runs": 1, "last": last, "pid": None}
                                  if phase["after"] else
                                  {"loaded": True, "state": "not running", "runs": 0, "last": "0", "pid": None}),
        "supervisor_run": lambda: (RUN, time.time() + 1) if phase["after"] else ("old", time.time() - 100),
        "kickstart_request": lambda: (phase.update(after=True), (KICK, None))[1],
        "processes_matching": lambda pattern: listed.get(pattern, []),
        "process_identity": scripted(table),
        "commit": lambda message, edits: "c",
        "time": types.SimpleNamespace(monotonic=time.monotonic, time=time.time, sleep=lambda s: None),
    }
    saved = {name: getattr(d, name) for name in fakes}
    killed = []
    real_kill = os.kill
    try:
        for name, value in fakes.items():
            setattr(d, name, value)
        os.kill = lambda pid, sig: killed.append(pid)
        d.FAKE_STATE.mkdir(parents=True)
        d.CASES.mkdir()
        background = scratch / "background"
        background.mkdir()
        for name, record in records.items():
            (background / name).write_text(json.dumps(record))
        target = scratch / "hello.txt"
        target.write_text(d.hello("c15b"))
        h = Harness()
        if case == "M8-deadline":
            d.launchd_deadline_case(h, background)
        else:
            d.launchd_soft_cancel_case(h, background, target)
        return h.records[case], killed
    finally:
        os.kill = real_kill
        for name, value in saved.items():
            setattr(d, name, value)


listed = {"install pkg-cargo": [(100, "sh cargo install pkg-cargo")], "--worker": [(200, "loadout --worker")]}
timed_out = {"supervisor.json": {"outcome": "timed-out", "code": None, "run": RUN}}

for name, table, lst, want, needle in (
        ("verified gone", {100: [present(100), ("absent", None)], 200: [present(200), ("absent", None)]},
         listed, "pass", ""),
        ("installer query unavailable afterwards", {100: [present(100), ("unavailable", "ps -p 100 exit 2")],
                                                    200: [present(200), ("absent", None)]},
         listed, "fail", "captured installer processes terminated"),
        ("worker already gone at capture", {100: [present(100), ("absent", None)], 200: [("absent", None)]},
         listed, "fail", "worker pid 200 not captured"),
        ("no worker listed", {100: [present(100), ("absent", None)]},
         {"install pkg-cargo": listed["install pkg-cargo"]}, "fail", "no worker process to capture"),
        ("installer capture malformed", {100: [("unavailable", "printed no single start time")],
                                         200: [present(200), ("absent", None)]},
         listed, "fail", "installer pid 100 not captured"),
        ("installer still alive", {100: [present(100)], 200: [present(200), ("absent", None)]},
         listed, "fail", "captured installer processes terminated")):
    (status, detail), killed = drive("M8-deadline", table, lst, timed_out, "1")
    check("deadline caller: %s -> %s" % (name, want), status == want and needle in detail, "%s: %s" % (status, detail))
    if name == "installer still alive":
        check("deadline caller kills only a verified-alive identity", killed == [100], repr(killed))
    if name == "installer query unavailable afterwards":
        check("deadline caller never signals an unavailable identity", killed == [], repr(killed))

soft = {"supervisor.json": {"outcome": "exited", "cancelled": True, "code": 4, "run": RUN},
        "last-run.json": {"cancelled": True, "exit": 4, "result": "applied", "message": "cancelled at the soft deadline",
                          "supervisor": RUN}}
for name, table, lst, want, needle in (
        ("held, released, both exited", {100: [present(100), present(100), ("absent", None)],
                                         200: [present(200), ("absent", None)]}, listed, "pass", ""),
        ("worker never captured", {100: [present(100), present(100), ("absent", None)]},
         {"install pkg-cargo": listed["install pkg-cargo"]}, "fail", "no worker process to capture"),
        ("held query unavailable at release", {100: [present(100), ("unavailable", "timeout"), ("absent", None)],
                                               200: [present(200), ("absent", None)]},
         listed, "fail", "install still held past the soft deadline"),
        ("installer gone before release", {100: [present(100), ("absent", None)],
                                           200: [present(200), ("absent", None)]},
         listed, "fail", "install still held past the soft deadline"),
        ("worker still alive", {100: [present(100), present(100), ("absent", None)], 200: [present(200)]},
         listed, "fail", "worker exited"),
        ("worker query unavailable afterwards", {100: [present(100), present(100), ("absent", None)],
                                                 200: [present(200), ("unavailable", "exit 2")]},
         listed, "fail", "worker exited")):
    (status, detail), _ = drive("M8-soft-cancel-receipt", table, lst, soft, "4")
    check("soft-cancel caller: %s -> %s" % (name, want), status == want and needle in detail, "%s: %s" % (status, detail))

print("all controls passed" if not failures else "%d control(s) failed" % failures)
sys.exit(1 if failures else 0)
