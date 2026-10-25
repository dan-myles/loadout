#!/usr/bin/env python3
"""Loadout full-CLI acceptance inside a disposable Linux guest or a hosted macOS runner
(vm-setup, standing 128; successor v2 under standing 134 for the repaired U8 R1-R10 / U9 R1-R6;
v3 adds the native gates named by the U8 residual, U9 hash-status and U4 I1 reviews).

Runs the real installed CLI against a guest-local bare Git remote, a loopback release server and
fake user-scope managers, in the guest user's real HOME (never the Paperclip host). Every case
records argv, exit, expected exits, duration, stdout/stderr, and its assertions to
~/acc/out/results.jsonl; nothing stops at the first failure except a failed group precondition,
which marks the dependent groups not-run.

  python3 -I linux_acceptance.py --in /srv/loadout-acc/in --mode tester [--long-timer] [--long-deadline]
  python3 -I linux_acceptance.py --in /srv/loadout-acc/in --mode std
  python3 -I linux_acceptance.py --in "$PWD/in" --mode macos [--long-timer]   # hosted macOS runner only

Inputs under --in (pushed by host/campaign): expected.json, fixtures/loadout/, fakebin/linux/cargo,
release/<variant>/..., install.sh, optional tests/ (guest-native release test binary).
"""

import argparse
import hashlib
import http.server
import json
import os
import re
import shutil
import signal
import socketserver
import subprocess
import sys
import threading
import time
from pathlib import Path

HOME = Path.home()
IS_MAC = sys.platform == "darwin"
OS_NAME = "macos" if IS_MAC else "linux"
ACC = HOME / "acc"
OUT = ACC / "out"
CASES = OUT / "cases"
if IS_MAC:
    STATE = HOME / "Library" / "Application Support" / "loadout"
else:
    STATE = Path(os.environ.get("XDG_STATE_HOME") or HOME / ".local" / "state") / "loadout"
CLONE = STATE / "loadout"
CONFIG = Path(os.environ.get("XDG_CONFIG_HOME") or HOME / ".config")
UNITS = CONFIG / "systemd" / "user"
LOCAL_BIN = HOME / ".local" / "bin"
CLI = LOCAL_BIN / "loadout"
FAKE = ACC / "fake"
FAKE_STATE = FAKE / "fake-state"
WORK = ACC / "work"
REMOTE = ACC / "remote.git"
REMOTE_URL = "file://" + str(REMOTE)
TOKEN_SENTINEL = "sentinel-token-value-7f3a"
SERVICE = "loadout-update.service"
TIMER = "loadout-update.timer"
LAUNCHD_LABEL = "dev.loadout.update"
PLIST = HOME / "Library" / "LaunchAgents" / (LAUNCHD_LABEL + ".plist")
URL_SECRET = "EXAMPLE_SECRET_9c1d"
# v3: a folder name with a space, accented and CJK letters and an ampersand, for scheduler values.
UNICODE_DIR = ACC / "bin \u00fcn\u00ef\u96ea & co"
SCHED_DIR = PLIST.parent if IS_MAC else UNITS


class Harness:
    def __init__(self, inputs, mode):
        self.inputs = inputs
        self.mode = mode
        self.expected = json.loads((inputs / "expected.json").read_text())
        self.results = OUT / ("results-%s.jsonl" % mode)
        self.counts = {"pass": 0, "fail": 0, "observe": 0, "not-run": 0}
        self.blocked = {}
        CASES.mkdir(parents=True, exist_ok=True)

    # Recording -------------------------------------------------------------------------------

    def record(self, case, status, **fields):
        entry = {"case": case, "status": status, "mode": self.mode, "time": time.time()}
        entry.update(fields)
        with self.results.open("a") as out:
            out.write(json.dumps(entry, sort_keys=True) + "\n")
        self.counts[status] = self.counts.get(status, 0) + 1
        mark = {"pass": "PASS", "fail": "FAIL", "observe": "OBS ", "not-run": "SKIP"}[status]
        print("%s %s %s" % (mark, case, fields.get("detail", "")), flush=True)

    def check(self, case, ok, detail):
        self.record(case, "pass" if ok else "fail", detail=detail)
        return ok

    def env(self, extra=None, isolated=None):
        env = {k: v for k, v in os.environ.items() if not k.startswith("LOADOUT_")}
        env["PATH"] = "%s:%s:/usr/local/bin:/usr/bin:/bin" % (FAKE / "bin", LOCAL_BIN)
        env["E2E_MCP_TOKEN"] = TOKEN_SENTINEL
        env["LC_ALL"] = "C.UTF-8"
        if isolated is not None:
            env["LOADOUT_HOME"] = str(isolated)
        if extra:
            env.update(extra)
        return env

    def run(self, case, argv, expect, *, env=None, timeout=600, cwd=None, asserts=None,
            observe=False):
        """Runs argv, records it, and evaluates expected exits plus extra assertions."""
        argv = [str(a) for a in argv]
        started = time.monotonic()
        stem = CASES / case
        try:
            done = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True,
                                  env=env or self.env(), timeout=timeout, cwd=cwd)
            code, out, err = done.returncode, done.stdout, done.stderr
        except subprocess.TimeoutExpired as expired:
            code, out, err = "timeout", expired.stdout or b"", expired.stderr or b""
        elapsed = round(time.monotonic() - started, 3)
        stem.with_suffix(".stdout").write_bytes(out)
        stem.with_suffix(".stderr").write_bytes(err)
        text = out.decode("utf-8", "replace") + err.decode("utf-8", "replace")
        failures = []
        if not observe and code not in expect:
            failures.append("exit %s not in %s" % (code, sorted(expect, key=str)))
        if TOKEN_SENTINEL in text:
            failures.append("secret sentinel printed")
        for name, ok in (asserts(code, text) if asserts else []):
            if not ok:
                failures.append(name)
        status = "observe" if observe and not failures else ("fail" if failures else "pass")
        self.record(case, status, argv=argv, exit=code, expect=sorted(expect, key=str),
                    seconds=elapsed, detail="; ".join(failures) or "exit %s" % code)
        return code, text

    def skip_rest(self, group, reason):
        self.blocked[group] = reason

    def requires(self, group, *needs):
        for need in needs:
            if need in self.blocked:
                self.record(group, "not-run", detail="needs %s: %s" % (need, self.blocked[need]))
                self.skip_rest(group, "dependency %s" % need)
                return False
        return True


# Small helpers ---------------------------------------------------------------------------------

def sh(argv, cwd=None, check=True, env=None, timeout=120):
    """Fixture maintenance (git plumbing, systemctl queries); not the system under test."""
    done = subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True, text=True,
                          env=env, timeout=timeout, stdin=subprocess.DEVNULL)
    if check and done.returncode != 0:
        raise RuntimeError("%s -> %s: %s" % (argv, done.returncode, done.stderr.strip()))
    return done.stdout.strip()


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def snapshot(*roots):
    """path -> (kind, mode, sha256 or link target); missing roots are recorded as absent."""
    seen = {}
    for root in roots:
        root = Path(root)
        if not os.path.lexists(root):
            seen[str(root)] = ("absent",)
            continue
        paths = [root] + (sorted(root.rglob("*")) if root.is_dir() and not root.is_symlink() else [])
        for path in paths:
            st = path.lstat()
            if path.is_symlink():
                seen[str(path)] = ("link", os.readlink(path))
            elif path.is_dir():
                seen[str(path)] = ("dir", oct(st.st_mode & 0o7777))
            else:
                seen[str(path)] = ("file", oct(st.st_mode & 0o7777), sha256(path))
    return seen


def git(*args, cwd=WORK):
    return sh(["git", "-c", "user.name=Loadout E2E", "-c", "user.email=e2e@example.invalid",
               "-c", "commit.gpgsign=false", *args], cwd=cwd)


def head(repo=CLONE):
    return sh(["git", "--git-dir", Path(repo) / ".git", "rev-parse", "HEAD"])


def commit(message, edits):
    """edits: relative path -> new text (None deletes). Commits and pushes to the bare remote."""
    for rel, text in edits.items():
        path = WORK / rel
        if text is None:
            path.unlink()
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
    git("add", "-A")
    git("commit", "-q", "-m", message)
    git("push", "-q", "origin", "HEAD:main")
    return git("rev-parse", "HEAD")


def manifest(update=None, extra=""):
    """loadout.toml from the fixture with [update] fields replaced."""
    text = (WORK / "loadout.toml").read_text()
    for key, value in (update or {}).items():
        text = re.sub(r"(?m)^%s = .*$" % key, "%s = %s" % (key, value), text, count=1)
    return text + extra


def hello(tag):
    return "loadout-e2e hello sentinel-%s\n" % tag


def read(path):
    try:
        return Path(path).read_text()
    except OSError as error:
        return "<unreadable: %s>" % error


def json_file(path):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError) as error:
        return {"<error>": str(error)}


def processes_matching(pattern):
    """PIDs of this user's processes whose argv contains pattern (reads only /proc/<pid>/cmdline,
    or `ps` on macOS)."""
    found = []
    if IS_MAC:
        listing = sh(["ps", "-U", str(os.getuid()), "-o", "pid=,command="], check=False)
        for line in listing.splitlines():
            pid, _, argv = line.strip().partition(" ")
            if pattern in argv and "linux_acceptance.py" not in argv:
                found.append((int(pid), argv.strip()))
        return found
    uid = os.getuid()
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            if entry.stat().st_uid != uid:
                continue
            argv = (entry / "cmdline").read_bytes().replace(b"\0", b" ").decode("utf-8", "replace")
        except OSError:
            continue
        if pattern in argv and "linux_acceptance.py" not in argv:
            found.append((int(entry.name), argv.strip()))
    return found


def wait_for(predicate, seconds, step=0.5):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(step)
    return predicate()


class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        with (OUT / "http.log").open("a") as log:
            log.write("%s %s\n" % (self.address_string(), args[0] % args[1:]))


class Unauthorized(http.server.BaseHTTPRequestHandler):
    """Every request needs credentials: a Git client asks for them, and must not hang."""
    hits = 0

    def do_GET(self):
        Unauthorized.hits += 1
        self.send_response(401)
        self.send_header("WWW-Authenticate", 'Basic realm="loadout-e2e"')
        self.send_header("Content-Length", "0")
        self.end_headers()

    do_POST = do_GET

    def log_message(self, *args):
        pass


def serve(handler, directory=None):
    if directory is not None:
        handler = _bind(handler, directory)
    server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), handler)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def _bind(handler, directory):
    class Bound(handler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=str(directory), **kwargs)
    return Bound


# Groups ----------------------------------------------------------------------------------------

def g0_facts(h):
    facts = {
        "uname": sh(["uname", "-a"]),
        "os_release": read("/etc/os-release"),
        "id": sh(["id"]),
        "git": sh(["git", "--version"]),
        "python": sys.version,
        "sudo_n": subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode,
    }
    if IS_MAC:
        facts["sw_vers"] = sh(["sw_vers"], check=False)
        facts["gui_domain"] = sh(["launchctl", "print", "gui/%d" % os.getuid()], check=False)[:400]
        facts["console_user"] = sh(["stat", "-f", "%Su", "/dev/console"], check=False)
        detail = "macOS %s console=%s" % (facts["sw_vers"].replace("\n", " "), facts["console_user"])
    else:
        facts["systemd_user"] = sh(["systemctl", "--user", "is-system-running"], check=False)
        facts["linger"] = sh(["loginctl", "show-user", os.environ.get("USER", ""), "-p", "Linger"], check=False)
        facts["default_route"] = sh(["ip", "route", "show", "default"], check=False)
        detail = "systemd=%s linger=%s route=%r" % (facts["systemd_user"], facts["linger"], facts["default_route"])
    (OUT / ("facts-%s.json" % h.mode)).write_text(json.dumps(facts, indent=2, sort_keys=True))
    h.record("G0-facts", "observe", detail=detail)
    offline = subprocess.run(["curl", "-sS", "-m", "5", "-o", "/dev/null", "https://github.com"],
                             capture_output=True).returncode != 0
    if h.expected.get("network") == "offline":
        h.check("G0-offline", offline, "guest has no route to github.com (network cut by campaign)")
    for name in ("release_good_asset_sha256", "binary_sha256"):
        if name not in h.expected:
            h.record("G0-inputs", "fail", detail="expected.json lacks %s" % name)
            h.skip_rest("inputs", "expected.json incomplete")
            return


def g1_fixture(h):
    """Bare remote with commit C1 from fixtures/loadout, fake managers, loopback servers."""
    for path in (WORK, REMOTE, FAKE):
        if path.exists():
            h.record("G1-fixture", "fail", detail="%s already exists; reset the guest" % path)
            h.skip_rest("fixture", "dirty guest")
            return None
    (FAKE / "bin").mkdir(parents=True)
    FAKE_STATE.mkdir()
    shutil.copy2(h.inputs / "fakebin" / "linux" / "cargo", FAKE / "bin" / "cargo")
    os.chmod(FAKE / "bin" / "cargo", 0o755)
    sh(["git", "init", "-q", "--bare", "-b", "main", REMOTE])
    shutil.copytree(h.inputs / "fixtures" / "loadout", WORK)
    sh(["git", "init", "-q", "-b", "main"], cwd=WORK)
    git("remote", "add", "origin", REMOTE_URL)
    git("add", "-A")
    git("commit", "-q", "-m", "C1 fixture")
    git("push", "-q", "-u", "origin", "main")
    release, port = serve(Quiet, h.inputs / "release")
    auth, auth_port = serve(Unauthorized)
    h.release_url = "http://127.0.0.1:%d" % port
    h.auth_url = "http://127.0.0.1:%d/private/loadout.git" % auth_port
    h.record("G1-fixture", "pass", detail="C1 %s, release %s" % (git("rev-parse", "HEAD"), h.release_url))
    return release, auth


def g2_installer(h):
    """install.sh against loopback variants, each in its own throwaway HOME."""
    if not h.requires("G2", "fixture", "inputs"):
        return
    script = h.inputs / "install.sh"
    want = h.expected["binary_sha256"]

    def installed_in(home):
        return home / ".local" / "bin" / "loadout"

    def install(case, home, variant, expect, extra=(), asserts=None):
        home.mkdir(parents=True, exist_ok=True)
        env = h.env({"HOME": str(home)})
        return h.run(case, ["sh", script, *extra, "--base-url", "%s/%s" % (h.release_url, variant)],
                     expect, env=env, timeout=120, asserts=asserts)

    for variant in ("bad-sha", "bad-sidecar", "sidecar-two-lines", "outside-entry", "nested-entry",
                    "duplicate-binary", "symlink-binary", "missing-binary", "not-runnable",
                    "wrong-name-fake"):
        home = ACC / "ihome" / variant
        install("G2-refuse-%s" % variant, home, variant, {1},
                asserts=lambda code, text, home=home: [("nothing installed", not os.path.lexists(installed_in(home)))])
    home = ACC / "ihome" / "policy"
    h.run("G2-refuse-plain-http-remote", ["sh", script, "--base-url", "http://example.invalid/x"], {2},
          env=h.env({"HOME": str(home)}), timeout=60)

    home = ACC / "ihome" / "good"
    install("G2-good", home, "good", {0}, asserts=lambda code, text: [
        ("binary is the artifact", os.path.exists(installed_in(home)) and sha256(installed_in(home)) == want),
        ("record written", (installed_in(home).parent / "loadout.install.toml").is_file()),
    ])
    install("G2-good-again", home, "good", {0})
    record = installed_in(home).parent / "loadout.install.toml"
    install("G2-newer-fake", home, "newer-fake", {0}, asserts=lambda code, text: [
        ("record names 9.9.9", 'version = "9.9.9"' in read(record))])
    install("G2-back-to-good", home, "good", {0}, asserts=lambda code, text: [
        ("binary is the artifact again", sha256(installed_in(home)) == want)])
    install("G2-version-mismatch", home, "newer-fake", {1}, extra=("--version", "v%s" % h.expected["version"]),
            asserts=lambda code, text: [("binary unchanged", sha256(installed_in(home)) == want)])
    with installed_in(home).open("ab") as binary:
        binary.write(b"\n# changed after install\n")
    changed = sha256(installed_in(home))
    install("G2-refuse-changed-binary", home, "good", {1}, asserts=lambda code, text: [
        ("binary left alone", sha256(installed_in(home)) == changed)])
    home = ACC / "ihome" / "foreign"
    (home / ".local" / "bin").mkdir(parents=True)
    installed_in(home).write_text("#!/bin/sh\necho someone else's loadout\n")
    before = sha256(installed_in(home))
    install("G2-refuse-foreign-binary", home, "good", {1}, asserts=lambda code, text: [
        ("foreign binary unchanged", sha256(installed_in(home)) == before)])

    # v3, U9 H2 (non-root input): an installed binary this user can't read is a hash error, never
    # "unmanaged", and nothing is replaced. Root reads mode 000 anyway, so as root it is not run.
    home = ACC / "ihome" / "unreadable"
    install("G2-unreadable-setup", home, "good", {0})
    binary = installed_in(home)
    if os.geteuid() == 0:
        h.record("G2-unreadable-installed-binary", "not-run", detail="running as root: mode 000 is no denial")
    elif binary.exists():
        mode = binary.stat().st_mode & 0o7777
        before = snapshot(binary.parent)
        os.chmod(binary, 0)
        try:
            install("G2-unreadable-installed-binary", home, "good", {1}, asserts=lambda code, text: [
                ("says could not hash", "could not hash" in text),
                ("never unmanaged", "unmanaged" not in text.lower())])
        finally:
            os.chmod(binary, mode)
        h.check("G2-unreadable-files-unchanged", snapshot(binary.parent) == before,
                "uid %d; bytes and modes compared after restoring %s" % (os.geteuid(), oct(mode)))

    # v3: the shasum fallback (macOS's own tool; perl's shasum on Linux), through a PATH holding
    # every system tool except sha256sum.
    shim = ACC / "no-sha256sum-bin"
    system_path = "/usr/bin:/bin:/usr/sbin:/sbin"
    if shutil.which("shasum", path=system_path) is None:
        h.record("G2-shasum-route", "not-run", detail="no shasum in %s" % system_path)
    else:
        shim.mkdir(parents=True)
        for folder in system_path.split(":"):
            for tool in sorted(Path(folder).iterdir()):
                link = shim / tool.name
                if tool.name != "sha256sum" and not os.path.lexists(link) and os.access(tool, os.X_OK):
                    link.symlink_to(tool)
        home = ACC / "ihome" / "shasum"
        home.mkdir(parents=True)
        env = h.env({"HOME": str(home), "PATH": str(shim)})
        h.check("G2-shasum-route-has-no-sha256sum", shutil.which("sha256sum", path=str(shim)) is None
                and shutil.which("shasum", path=str(shim)) is not None, "PATH %s" % shim)
        h.run("G2-shasum-route-good", ["sh", script, "--base-url", "%s/good" % h.release_url], {0}, env=env,
              timeout=180, asserts=lambda code, text: [
                  ("binary is the artifact", installed_in(home).exists() and sha256(installed_in(home)) == want),
                  ("record written", (installed_in(home).parent / "loadout.install.toml").is_file())])
        h.run("G2-shasum-route-again", ["sh", script, "--base-url", "%s/good" % h.release_url], {0}, env=env,
              timeout=180)
        h.run("G2-shasum-route-bad-sha", ["sh", script, "--base-url", "%s/bad-sha" % h.release_url], {1},
              env=h.env({"HOME": str(ACC / "ihome" / "shasum-bad"), "PATH": str(shim)}), timeout=180,
              asserts=lambda code, text: [("nothing installed",
                                           not os.path.lexists(installed_in(ACC / "ihome" / "shasum-bad")))])

    # v2, U9 R6: the installer's progress lines never echo a password in the forwarded loadout URL;
    # the installed CLI's init then refuses that URL (U8 R2).
    home = ACC / "ihome" / "redact"
    home.mkdir(parents=True)
    h.run("G2-forwarded-url-redacted", ["sh", script, "--base-url", "%s/good" % h.release_url,
                                        "https://user:%s@example.invalid/l.git" % URL_SECRET, "--no-service"],
          {1}, env=h.env({"HOME": str(home)}), timeout=180,
          asserts=lambda code, text: [("secret not echoed", URL_SECRET not in text)])

    # v2, U9 R2: installs into one folder are serialized; whatever wins, the record binds the binary.
    home = ACC / "ihome" / "concurrent"
    home.mkdir(parents=True)
    env = h.env({"HOME": str(home)})
    procs = []
    for variant in ("good", "newer-fake"):
        log = (CASES / ("G2-concurrent-" + variant)).with_suffix(".bg").open("wb")
        procs.append(subprocess.Popen(["sh", str(script), "--base-url", "%s/%s" % (h.release_url, variant)],
                                      stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, env=env))
    codes = [proc.wait(timeout=300) for proc in procs]
    record = installed_in(home).parent / "loadout.install.toml"
    bound = re.search(r'binary_sha256 = "([0-9a-f]{64})"', read(record))
    version = re.search(r'(?m)^version = "([^"]+)"', read(record))
    reported = sh([installed_in(home), "--version"], check=False) if installed_in(home).exists() else ""
    h.check("G2-concurrent-installs-consistent",
            set(codes) <= {0, 1} and 0 in codes and bound is not None and version is not None
            and bound.group(1) == sha256(installed_in(home)) and reported == "loadout %s" % version.group(1),
            "exits %s, record version %s, binary says %r" % (codes, version and version.group(1), reported))


def g3_bootstrap(h):
    """install.sh + init in the real HOME: the documented one-line bootstrap."""
    if not h.requires("G3", "fixture", "inputs"):
        return
    name = {"tester": "vm-linux", "std": "vm-linux-std", "macos": "vm-macos"}[h.mode]
    argv = ["sh", h.inputs / "install.sh", "--base-url", "%s/good" % h.release_url, REMOTE_URL,
            "--name", name]
    if h.mode in ("tester", "macos"):
        argv.append("--no-service")
    c1 = git("rev-parse", "HEAD")
    code, text = h.run("G3-bootstrap", argv, {0}, timeout=900)
    ok = code == 0 and CLONE.is_dir()
    claude_json = json_file(HOME / ".claude.json")
    checks = [
        ("cli is the artifact", CLI.is_file() and sha256(CLI) == h.expected["binary_sha256"]),
        ("install record", (LOCAL_BIN / "loadout.install.toml").is_file()),
        ("clone at C1", CLONE.is_dir() and head() == c1),
        ("machine.toml 0600", (STATE / "machine.toml").is_file()
         and oct((STATE / "machine.toml").stat().st_mode & 0o777) == "0o600"),
        ("machine named", ('name = "%s"' % name) in read(STATE / "machine.toml")),
        ("user file", read(HOME / ".config/loadout-e2e/hello.txt") == hello("c1")),
        ("template rendered", "for %s on %s" % (name, OS_NAME) in read(HOME / ".config/loadout-e2e/gitconfig")
         and "# tagged e2e" in read(HOME / ".config/loadout-e2e/gitconfig")),
        ("claude skill", "sentinel-skill" in read(HOME / ".claude/skills/review/SKILL.md")),
        ("codex skill", "sentinel-skill" in read(HOME / ".agents/skills/review/SKILL.md")),
        ("claude instructions", "sentinel-instructions" in read(HOME / ".claude/CLAUDE.md")),
        ("codex instructions", "sentinel-instructions" in read(HOME / ".codex/AGENTS.md")),
        ("claude mcp", set(claude_json.get("mcpServers", {})) >= {"e2e-stdio", "e2e-remote"}),
        ("claude mcp env reference", "E2E_MCP_TOKEN" in read(HOME / ".claude.json")),
        ("codex mcp", "e2e-stdio" in read(HOME / ".codex/config.toml")),
        ("fake cargo installed", (FAKE_STATE / "cargo").exists()),
    ]
    for path in (HOME / ".claude.json", HOME / ".codex/config.toml", STATE / "machine.toml"):
        checks.append(("no secret in %s" % path.name, TOKEN_SENTINEL not in read(path)))
    for label, passed in checks:
        h.check("G3-" + re.sub(r"[^a-z0-9]+", "-", label.lower()).strip("-"), passed, label)
    if not ok:
        h.skip_rest("bootstrap", "bootstrap failed")
        return
    h.run("G3-status", [CLI, "status"], {0})
    h.run("G3-status-json", [CLI, "status", "--json"], {0},
          asserts=lambda code, text: [("json", _is_json(CASES / "G3-status-json.stdout"))])
    h.run("G3-doctor-json", [CLI, "doctor", "--json"], {0, 4}, observe=True)
    h.run("G3-version", [CLI, "--version"], {0}, asserts=lambda code, text: [
        ("version", text.strip() == "loadout %s" % h.expected["version"])])
    h.run("G3-help", [CLI, "--help"], {0}, asserts=lambda code, text: [
        ("commands listed", all(word in text for word in ("init", "update", "service", "plan", "apply")))])


def _is_json(path):
    try:
        json.loads(Path(path).read_text())
        return True
    except (OSError, ValueError):
        return False


def g4_init_refusals(h):
    if not h.requires("G4", "bootstrap"):
        return
    watched = (STATE, HOME / ".config/loadout-e2e", HOME / ".claude", HOME / ".claude.json")
    before = snapshot(*watched)
    h.run("G4-second-init", [CLI, "init", REMOTE_URL, "--no-service"], {1}, asserts=lambda code, text: [
        ("says already set up", "already set up" in text), ("nothing changed", snapshot(*watched) == before)])
    aside = STATE / "loadout.acceptance-aside"
    CLONE.rename(aside)
    try:
        h.run("G4-name-conflict", [CLI, "init", REMOTE_URL, "--name", "other-name", "--no-service"], {1},
              asserts=lambda code, text: [("names the saved name", "already named" in text),
                                          ("no clone made", not CLONE.exists())])
    finally:
        if CLONE.exists():
            shutil.rmtree(CLONE)
        aside.rename(CLONE)
    iso = ACC / "iso-password"
    h.run("G4-password-url", [CLI, "init", "https://user:%s@example.invalid/l.git" % URL_SECRET, "--no-service"], {1},
          env=h.env(isolated=iso), asserts=lambda code, text: [
              ("password never printed (U9 R6)", URL_SECRET not in text),
              ("no clone", not any(iso.rglob("loadout/.git")) if iso.exists() else True)])
    # v2, U8 R2: an HTTPS token in the username is refused before Git runs and never stored.
    iso = ACC / "iso-token"
    h.run("G4-token-username-url", [CLI, "init", "https://%s@example.invalid/l.git" % URL_SECRET, "--no-service"],
          {1}, env=h.env(isolated=iso), asserts=lambda code, text: [
              ("token never printed", URL_SECRET not in text),
              ("no clone", not any(iso.rglob("loadout/.git")) if iso.exists() else True),
              ("nothing stored", not any(URL_SECRET in read(path) for path in iso.rglob("*") if path.is_file())
               if iso.exists() else True)])
    iso = ACC / "iso-ssh-user"
    h.run("G4-ssh-username-url-allowed", [CLI, "init", "ssh://git@127.0.0.1:9/l.git", "--no-service"], {1},
          env=h.env(isolated=iso), timeout=120, asserts=lambda code, text: [
              ("reached git, not refused as a credential", "password" not in text.lower() and "credential" not in text.lower())])
    units_before = snapshot(SCHED_DIR)
    iso = ACC / "iso-service"
    h.run("G4-isolated-init-with-service", [CLI, "init", REMOTE_URL, "--name", "vm-linux"], {4},
          env=h.env(isolated=iso), asserts=lambda code, text: [
              ("no real scheduler files", snapshot(SCHED_DIR) == units_before)])
    iso = ACC / "iso-noprompt"
    h.run("G4-no-tty-no-name", [CLI, "init", REMOTE_URL, "--no-service"], {0, 4}, env=h.env(isolated=iso),
          timeout=180, observe=True)


def g5_apply_cycle(h):
    if not h.requires("G5", "bootstrap"):
        return
    target = HOME / ".config/loadout-e2e/hello.txt"
    h.run("G5-plan-clean", [CLI, "plan"], {0})
    h.run("G5-plan-json", [CLI, "plan", "--json"], {0},
          asserts=lambda code, text: [("json", _is_json(CASES / "G5-plan-json.stdout"))])
    with target.open("a") as drift:
        drift.write("local drift\n")
    drifted = read(target)
    h.run("G5-status-drift", [CLI, "status"], {4})
    h.run("G5-plan-drift", [CLI, "plan"], {4})
    h.run("G5-apply-drift-refused", [CLI, "apply"], {4},
          asserts=lambda code, text: [("drift kept", read(target) == drifted)])
    h.run("G5-apply-force", [CLI, "apply", "--force"], {0},
          asserts=lambda code, text: [("source restored", read(target) == hello("c1"))])
    h.run("G5-status-clean", [CLI, "status"], {0})
    h.run("G5-rollback", [CLI, "rollback"], {0},
          asserts=lambda code, text: [("backup restored", read(target) == drifted)])
    h.run("G5-reapply", [CLI, "apply", "--force"], {0},
          asserts=lambda code, text: [("source again", read(target) == hello("c1"))])
    h.run("G5-validate", [CLI, "validate", CLONE], {0})
    invalid = ACC / "invalid-loadout"
    shutil.copytree(h.inputs / "fixtures" / "loadout", invalid)
    with (invalid / "loadout.toml").open("a") as text:
        text.write("\n[no_such_table]\nkey = 1\n")
    h.run("G5-validate-invalid", [CLI, "validate", invalid], {3})
    h.run("G5-plan-invalid", [CLI, "plan", "--loadout", invalid], {3})
    missing = ACC / "missing-variable"
    shutil.copytree(h.inputs / "fixtures" / "loadout", missing)
    (missing / "files/gitconfig.tmpl").write_text("[user]\n\tname = {{ undefined_variable }}\n")
    watched = (HOME / ".config/loadout-e2e", STATE / "machine.toml")
    before = snapshot(*watched)
    h.run("G5-missing-variable-before-writes", [CLI, "apply", "--loadout", missing], {3},
          asserts=lambda code, text: [("nothing written", snapshot(*watched) == before)])


def g6_update(h):
    if not h.requires("G6", "bootstrap"):
        return
    target = HOME / ".config/loadout-e2e/hello.txt"
    gitconfig = HOME / ".config/loadout-e2e/gitconfig"
    h.run("G6-up-to-date", [CLI, "update"], {0})
    c2 = commit("C2", {"files/hello.txt": hello("c2"),
                       "loadout.toml": manifest().replace('greeting = "c1"', 'greeting = "c2"')})
    h.run("G6-fast-forward", [CLI, "update"], {0}, asserts=lambda code, text: [
        ("clone at C2", head() == c2), ("file updated", read(target) == hello("c2")),
        ("template updated", "greeting c2" in read(gitconfig))])
    c3 = commit("C3", {"files/hello.txt": hello("c3")})

    def blocked(case, setup, undo):
        setup()
        tree_before = snapshot(CLONE / "files", target)
        branch_before = head()
        try:
            h.run(case, [CLI, "update"], {4}, asserts=lambda code, text: [
                ("says blocked", "blocked" in text), ("branch unchanged", head() == branch_before),
                ("tree and target unchanged", snapshot(CLONE / "files", target) == tree_before)])
        finally:
            undo()

    tracked = CLONE / "files/hello.txt"
    blocked("G6-dirty-tracked", lambda: tracked.write_text("edited in the clone\n"),
            lambda: sh(["git", "-C", CLONE, "checkout", "--", "files/hello.txt"]))
    stray = CLONE / "untracked.txt"
    blocked("G6-dirty-untracked", lambda: stray.write_text("x\n"), stray.unlink)
    merge_head = CLONE / ".git/MERGE_HEAD"
    blocked("G6-in-progress", lambda: merge_head.write_text(c2 + "\n"), merge_head.unlink)
    blocked("G6-detached", lambda: sh(["git", "-C", CLONE, "checkout", "-q", "--detach"]),
            lambda: sh(["git", "-C", CLONE, "checkout", "-q", "main"]))
    blocked("G6-no-upstream", lambda: sh(["git", "-C", CLONE, "branch", "--unset-upstream"]),
            lambda: sh(["git", "-C", CLONE, "branch", "-q", "--set-upstream-to=origin/main", "main"]))
    h.run("G6-after-blocks", [CLI, "update"], {0}, asserts=lambda code, text: [
        ("clone at C3", head() == c3), ("file c3", read(target) == hello("c3"))])

    def local_commit():
        (CLONE / "local.txt").write_text("local\n")
        git("add", "local.txt", cwd=CLONE)
        git("commit", "-q", "-m", "local", cwd=CLONE)

    def reset_to_remote():
        sh(["git", "-C", CLONE, "reset", "-q", "--hard", "origin/main"])

    blocked("G6-ahead", local_commit, reset_to_remote)
    c4 = None

    def diverge():
        nonlocal c4
        local_commit()
        c4 = commit("C4", {"files/hello.txt": hello("c4")})

    blocked("G6-diverged", diverge, reset_to_remote)
    h.run("G6-after-diverged", [CLI, "update"], {0}, asserts=lambda code, text: [
        ("clone at C4", head() == c4), ("file c4", read(target) == hello("c4"))])

    alt = ACC / "alt-checkout"
    sh(["git", "clone", "-q", REMOTE_URL, alt])
    sh(["git", "-C", alt, "checkout", "-q", "--detach", git("rev-list", "--max-parents=0", "HEAD")])
    h.run("G6-loadout-dir", [CLI, "update", "--loadout", alt], {0}, asserts=lambda code, text: [
        ("not fetched", "not fetched" in text), ("clone untouched", head() == c4),
        ("alt applied", read(target) == hello("c1"))])
    h.run("G6-back-to-clone", [CLI, "update"], {0},
          asserts=lambda code, text: [("file c4", read(target) == hello("c4"))])
    h.run("G6-background-loadout-refused", [CLI, "update", "--background", "--loadout", alt], {2})

    before = snapshot(HOME / ".config/loadout-e2e")
    c5 = commit("C5 missing variable", {"files/gitconfig.tmpl": "[user]\n\tname = {{ undefined_variable }}\n"})
    h.run("G6-missing-variable", [CLI, "update"], {3}, asserts=lambda code, text: [
        ("targets unchanged", snapshot(HOME / ".config/loadout-e2e") == before)])
    h.record("G6-missing-variable-clone", "observe", detail="clone HEAD %s (C5 %s)" % (head(), c5))
    c6 = commit("C6 fixed", {"files/gitconfig.tmpl": (h.inputs / "fixtures/loadout/files/gitconfig.tmpl").read_text()})
    h.run("G6-fixed", [CLI, "update"], {0}, asserts=lambda code, text: [("clone at C6", head() == c6)])
    c6 = git_index_file(h)
    interrupted(h, c6)


def git_index_file(h):
    """v2, U8 R1: an inherited GIT_INDEX_FILE is never used for the clone's reads or writes.

    The external index is empty, so a dirty check through it would see every file deleted, and a
    fast-forward through it would rewrite it. Both the foreground and the by-hand background route
    run with it exported."""
    external = ACC / "external-index"
    sh(["git", "--git-dir", CLONE / ".git", "--work-tree", CLONE, "read-tree", "--empty"],
       env=dict(os.environ, GIT_INDEX_FILE=str(external)))
    before = sha256(external)
    target = HOME / ".config/loadout-e2e/hello.txt"
    last = None
    for case, argv in (("G6-git-index-file-foreground", [CLI, "update"]),
                       ("G6-git-index-file-background", [CLI, "update", "--background"])):
        last = commit("C-index " + case, {"files/hello.txt": hello(case[-10:])})
        h.run(case, argv, {0}, env=h.env({"GIT_INDEX_FILE": str(external)}), timeout=900,
              asserts=lambda code, text, want=last, tag=case[-10:]: [
                  ("external index unchanged", sha256(external) == before),
                  ("clone at the new commit", head() == want),
                  ("managed index agrees with the tree",
                   sh(["git", "-C", CLONE, "status", "--porcelain"], check=False) == ""),
                  ("applied", read(target) == hello(tag))])
    return last


def start_hanging(h, argv, label):
    """Removes the fake cargo marker and holds its install, then starts argv in its own session."""
    (FAKE_STATE / "cargo").unlink(missing_ok=True)
    (FAKE_STATE / "hang").write_text("hold\n")
    log = (CASES / label).with_suffix(".bg")
    process = subprocess.Popen([str(a) for a in argv], stdin=subprocess.DEVNULL, stdout=log.open("wb"),
                               stderr=subprocess.STDOUT, env=h.env(), start_new_session=True)
    reached = wait_for(lambda: bool(processes_matching("install pkg-cargo")), 120)
    return process, reached


def interrupted(h, c6):
    """Locked and interrupted-run gates with a real killed apply (fake installer held)."""
    process, reached = start_hanging(h, [CLI, "update"], "G6-hung-update")
    if not h.check("G6-hang-reached", reached, "fake cargo install is running under update"):
        os.killpg(process.pid, signal.SIGKILL)
        (FAKE_STATE / "hang").unlink()
        return
    h.run("G6-locked-update", [CLI, "update"], {1})
    h.run("G6-locked-apply", [CLI, "apply"], {1})
    # v2, U8 Locked correction: a background worker that finds the lock held is recorded as locked.
    h.run("G6-locked-background", [CLI, "update", "--background"], {1}, timeout=300, asserts=lambda code, text: [
        ("recorded locked", json_file(STATE / "background/last-run.json").get("result") == "locked")])
    os.killpg(process.pid, signal.SIGKILL)
    process.wait(timeout=30)
    for pid, _ in processes_matching("install pkg-cargo"):
        os.kill(pid, signal.SIGKILL)
    (FAKE_STATE / "hang").unlink()
    # v5 (standing 167): at H 21187ad a program is installed only after the run's journal is finished
    # (src/apply/mod.rs:165-173), so killing a held install leaves a FINISHED run, not an interrupted
    # one. These cases assert that behaviour. They are not interrupted-run gates and get no
    # interrupted-recovery credit; the real-CLI interrupted-write gate is recorded as not run.
    target = HOME / ".config/loadout-e2e/hello.txt"
    h.run("G6-status-after-killed-install", [CLI, "status"], {0}, asserts=lambda code, text: [
        ("no run reported interrupted", "interrupted" not in text.lower()),
        ("the killed install never finished", not (FAKE_STATE / "cargo").exists())])
    h.record("G6-interrupted-journal-gate", "not-run",
             detail="UNEXERCISED: a killed held program install leaves a finished journal by design "
                    "(apply/mod.rs:165-173); no real-CLI kill inside a journaled write is induced")
    prior = read(target)
    c7 = commit("C7", {"files/hello.txt": hello("c7")})
    h.run("G6-update-after-killed-install", [CLI, "update"], {0}, asserts=lambda code, text: [
        ("fetched C7", head() == c7), ("file c7", read(target) == hello("c7")),
        ("the program installed this time", (FAKE_STATE / "cargo").exists())])
    h.run("G6-rollback-latest-finished-run", [CLI, "rollback"], {0}, asserts=lambda code, text: [
        ("the C7 run's file is restored", read(target) == prior)])
    h.run("G6-update-after-rollback", [CLI, "update"], {0}, asserts=lambda code, text: [
        ("clone at C7", head() == c7), ("file c7 again", read(target) == hello("c7")),
        ("cargo still installed", (FAKE_STATE / "cargo").exists())])


def g7_background_by_hand(h):
    if not h.requires("G7", "bootstrap"):
        return
    records = STATE / "background"
    before = snapshot(records)
    h.run("G7-worker-by-hand", [CLI, "update", "--background", "--worker"], {1},
          asserts=lambda code, text: [("nothing written", snapshot(records) == before)])
    c8 = commit("C8", {"files/hello.txt": hello("c8")})
    h.run("G7-supervisor-by-hand", [CLI, "update", "--background"], {0}, timeout=900, asserts=lambda code, text: [
        ("clone at C8", head() == c8),
        ("supervisor exited 0", json_file(records / "supervisor.json").get("code") == 0),
        ("last run applied", json_file(records / "last-run.json").get("result") == "applied"),
        ("log has no content", "sentinel-c8" not in read(records / "worker.log")),
        ("records 0600", all(oct(p.stat().st_mode & 0o777) == "0o600" for p in records.iterdir() if p.is_file()))])
    h.run("G7-status-background", [CLI, "status"], {0},
          asserts=lambda code, text: [("background line", "background" in text)])
    h.run("G7-service-status-not-installed", [CLI, "service", "status", "--json"], {0, 4}, observe=True)

    sh(["git", "-C", CLONE, "remote", "set-url", "origin", h.auth_url])
    try:
        started = time.monotonic()
        h.run("G7-credentials-background-no-hang", [CLI, "update", "--background"], {1, 4}, timeout=600,
              asserts=lambda code, text: [("bounded", time.monotonic() - started < 300),
                                          ("server asked", Unauthorized.hits > 0),
                                          ("recorded failed or blocked",
                                           json_file(records / "last-run.json").get("result") in ("failed", "blocked"))])
        h.run("G7-credentials-foreground-no-tty", [CLI, "update"], {1, 4}, timeout=300)
    finally:
        sh(["git", "-C", CLONE, "remote", "set-url", "origin", REMOTE_URL])
    h.run("G7-status-after-failed-background", [CLI, "status"], {4}, observe=True)

    c9 = commit("C9", {"files/hello.txt": hello("c9")})
    process, reached = start_hanging(h, [CLI, "update", "--background"], "G7-supervisor-kill")
    if h.check("G7-hang-reached", reached, "fake install under the supervised worker"):
        workers = processes_matching("--worker")
        os.kill(process.pid, signal.SIGKILL)
        process.wait(timeout=30)
        gone = wait_for(lambda: not processes_matching("--worker") and not processes_matching("install pkg-cargo"), 120)
        h.check("G7-worker-dies-with-supervisor", gone, "workers before kill: %s" % workers)
        for pid, _ in processes_matching("install pkg-cargo") + processes_matching("--worker"):
            os.kill(pid, signal.SIGKILL)
    (FAKE_STATE / "hang").unlink()
    # v2, U8 R5: a killed supervisor leaves missing or stale evidence, which is never healthy.
    h.run("G7-status-after-kill", [CLI, "status"], {4})
    h.run("G7-rollback-after-kill", [CLI, "rollback"], {0, 4}, observe=True)
    h.run("G7-update-after-kill", [CLI, "update"], {0}, asserts=lambda code, text: [("clone at C9", head() == c9)])
    stale_records(h, records)
    linked_records(h, records)


def stale_records(h, records):
    """v2, U8 R5: a worker record without its own supervisor record is not a healthy run.

    Synthetic, guest-only: the supervisor record of a good run is moved aside by hand."""
    commit("C-stale", {"files/hello.txt": hello("stale")})
    h.run("G7-good-run-before-stale", [CLI, "update", "--background"], {0}, timeout=900)
    h.run("G7-status-good-run", [CLI, "status"], {0})
    aside = ACC / "supervisor.json.aside"
    (records / "supervisor.json").rename(aside)
    try:
        h.run("G7-status-missing-supervisor-record", [CLI, "status"], {4})
        h.run("G7-service-status-missing-supervisor-record", [CLI, "service", "status"], {0, 4}, observe=True)
    finally:
        aside.rename(records / "supervisor.json")
    h.run("G7-status-restored", [CLI, "status"], {0})


def linked_records(h, records):
    """v2, U8 R3: the records folder replaced by a link is neither read through nor written or
    removed through; an outside sentinel stays byte-identical."""
    outside = ACC / "outside-records"
    outside.mkdir()
    for name in ("supervisor.json", "last-run.json", "worker.log", "task.xml"):
        (outside / name).write_text("outside sentinel %s\n" % name)
    before = snapshot(outside)
    aside = ACC / "background.aside"
    records.rename(aside)
    records.symlink_to(outside, target_is_directory=True)
    try:
        h.run("G7-linked-records-status", [CLI, "status"], {1, 4},
              asserts=lambda code, text: [("outside unchanged", snapshot(outside) == before)])
        h.run("G7-linked-records-background", [CLI, "update", "--background"], {1, 4}, timeout=900,
              asserts=lambda code, text: [("outside unchanged", snapshot(outside) == before)])
        h.run("G7-linked-records-service-uninstall", [CLI, "service", "uninstall"], {0, 1, 4}, observe=True,
              asserts=lambda code, text: [("outside unchanged", snapshot(outside) == before)])
    finally:
        records.unlink()
        aside.rename(records)
    h.run("G7-status-after-link-restored", [CLI, "status"], {0, 4}, observe=True)


def unit_state(unit):
    text = sh(["systemctl", "--user", "show", unit, "-p", "ActiveState", "-p", "Result", "-p",
               "ExecMainStatus", "-p", "ExecMainStartTimestampMonotonic"], check=False)
    return dict(line.split("=", 1) for line in text.splitlines() if "=" in line)


def run_service(h, case, expect_status, extra_checks=None, timeout=900):
    """Starts the installed unit once, as its timer would, and waits for it to finish."""
    started = unit_state(SERVICE).get("ExecMainStartTimestampMonotonic")
    sh(["systemctl", "--user", "start", SERVICE])
    finished = wait_for(lambda: unit_state(SERVICE).get("ExecMainStartTimestampMonotonic") != started
                        and unit_state(SERVICE).get("ActiveState") in ("inactive", "failed"), timeout, 1)
    state = unit_state(SERVICE)
    failures = [] if finished else ["did not finish in %ss" % timeout]
    wanted = {str(code) for code in (expect_status if isinstance(expect_status, (set, tuple)) else (expect_status,))}
    if state.get("ExecMainStatus") not in wanted:
        failures.append("ExecMainStatus %s, wanted %s" % (state.get("ExecMainStatus"), sorted(wanted)))
    for label, ok in (extra_checks() if extra_checks else []):
        if not ok:
            failures.append(label)
    h.record(case, "fail" if failures else "pass", detail="; ".join(failures) or json.dumps(state, sort_keys=True))
    sh(["systemctl", "--user", "reset-failed", SERVICE], check=False)


def g8_service(h):
    if not h.requires("G8", "bootstrap"):
        return
    systemd = sh(["systemctl", "--user", "is-system-running"], check=False)
    if not h.check("G8-user-session", systemd in ("running", "degraded"), "systemd --user: %s" % systemd):
        h.skip_rest("service", "no systemd user session")
        return
    records = STATE / "background"
    target = HOME / ".config/loadout-e2e/hello.txt"
    if h.mode == "tester":
        units_before = snapshot(UNITS)
        iso = ACC / "iso-service-cmds"
        for action in ("install", "uninstall"):
            h.run("G8-isolated-%s" % action, [CLI, "service", action], {4}, env=h.env(isolated=iso),
                  asserts=lambda code, text: [("units unchanged", snapshot(UNITS) == units_before)])
        h.run("G8-status-before", [CLI, "service", "status", "--json"], {0, 4}, observe=True)
        planted_scratch(h)
        h.run("G8-install", [CLI, "service", "install"], {0})
    # The timer may start a run as soon as it is enabled; let it finish before driving runs by hand.
    idle = wait_for(lambda: unit_state(SERVICE).get("ActiveState") not in ("active", "activating"), 900, 2)
    h.check("G8-idle-after-install", idle, "no background run left running after install")
    unit_text = read(UNITS / SERVICE)
    timer_text = read(UNITS / TIMER)
    service_toml = STATE / "service.toml"
    for label, ok in (
            ("unit Type=exec", "Type=exec" in unit_text),
            ("unit RuntimeMaxSec=3720", "RuntimeMaxSec=3720" in unit_text),
            ("unit KillMode=control-group", "KillMode=control-group" in unit_text),
            ("unit NoNewPrivileges", "NoNewPrivileges=yes" in unit_text),
            ("unit runs the installed cli", str(CLI) in unit_text and "--background" in unit_text
             and "--service" in unit_text and str(service_toml) in unit_text),
            ("timer interval 15", re.search(r"OnUnitInactiveSec=15", timer_text) is not None),
            ("timer enabled", sh(["systemctl", "--user", "is-enabled", TIMER], check=False) == "enabled"),
            ("timer active", sh(["systemctl", "--user", "is-active", TIMER], check=False) == "active"),
            ("service.toml 0600", service_toml.is_file() and oct(service_toml.stat().st_mode & 0o777) == "0o600"),
            ("service.toml captures fake PATH", str(FAKE / "bin") in read(service_toml)),
            ("service.toml has no LOADOUT_HOME or secret", "LOADOUT_HOME" not in read(service_toml)
             and TOKEN_SENTINEL not in read(service_toml) and "E2E_MCP_TOKEN" not in read(service_toml))):
        h.check("G8-" + re.sub(r"[^a-z0-9]+", "-", label.lower()).strip("-"), ok, label)
    h.run("G8-service-status", [CLI, "service", "status"], {0})
    native_formats(h, "G8-native-formats")
    status_json_case(h, "G8-definition-matches")
    changed_settings(h, service_toml)

    c10 = commit("C10 service", {"files/hello.txt": hello("c10")})
    run_service(h, "G8-run-applies", 0, lambda: [
        ("clone at C10", head() == c10), ("file c10", read(target) == hello("c10")),
        ("supervisor 0", json_file(records / "supervisor.json").get("code") == 0),
        ("no content in log", "sentinel-c10" not in read(records / "worker.log"))])
    h.run("G8-status-after-run", [CLI, "status"], {0})
    stray = CLONE / "untracked.txt"
    stray.write_text("x\n")
    commit("C11 while dirty", {"files/hello.txt": hello("c11")})
    run_service(h, "G8-run-blocked-dirty", 4, lambda: [
        ("target unchanged", read(target) == hello("c10")),
        ("recorded blocked", json_file(records / "last-run.json").get("result") == "blocked")])
    h.run("G8-status-blocked", [CLI, "status"], {4})
    h.run("G8-service-status-blocked", [CLI, "service", "status"], {4})
    stray.unlink()
    run_service(h, "G8-run-after-clean", 0, lambda: [("file c11", read(target) == hello("c11"))])

    c12 = commit("C12 auto off", {"files/hello.txt": hello("c12"), "loadout.toml": manifest({"auto": "false"})})
    run_service(h, "G8-run-disabled", 0, lambda: [
        ("fetched C12", head() == c12), ("not applied", read(target) == hello("c11")),
        ("recorded disabled", json_file(records / "last-run.json").get("result") == "disabled")])
    h.run("G8-install-refuses-auto-false", [CLI, "service", "install"], {4})
    h.run("G8-doctor-warns-disabled", [CLI, "doctor"], {0, 4}, observe=True, asserts=lambda code, text: [
        ("doctor says updates are disabled (U8 R10)", "disabled" in text.lower())])
    commit("C13 auto on", {"loadout.toml": manifest({"auto": "true"})})
    run_service(h, "G8-run-reenabled", 0, lambda: [("file c12", read(target) == hello("c12"))])

    moved = ACC / "moved-service.toml"
    shutil.copy2(service_toml, moved)
    h.run("G8-moved-record-refused", [CLI, "update", "--background", "--service", moved], {1, 4}, timeout=300)

    if h.args.long_timer:
        c14 = commit("C14 timer", {"files/hello.txt": hello("c14")})
        fired = wait_for(lambda: head() == c14, 20 * 60, 10)
        h.check("G8-timer-fires", fired, "timer applied C14 within 20 minutes of the last run")
    if h.args.long_deadline:
        deadline_case(h, records)
        soft_cancel_case(h, records)

    if h.mode == "std":
        return  # G10 still needs the installed unit; it uninstalls at its end.
    other_state_job(h)
    non_utf8_destination(h, service_toml)
    unicode_job(h, service_toml)
    h.run("G8-uninstall", [CLI, "service", "uninstall"], {0}, asserts=lambda code, text: [
        ("unit files gone", not (UNITS / SERVICE).exists() and not (UNITS / TIMER).exists()),
        ("service.toml gone", not service_toml.exists()),
        ("timer unknown", sh(["systemctl", "--user", "is-enabled", TIMER], check=False) != "enabled")])
    h.run("G8-status-after-uninstall", [CLI, "service", "status", "--json"], {0, 4}, observe=True)
    uninstall_without_definitions(h, service_toml)
    partial_uninstall(h, service_toml)


def planted_scratch(h):
    """v2, U8 R4: a symlink already sitting at a scheduler scratch name is never followed,
    truncated or cleaned up; the outside sentinel stays byte-identical."""
    UNITS.mkdir(parents=True, exist_ok=True)
    sentinel = ACC / "scratch-sentinel.txt"
    sentinel.write_text("outside sentinel\n")
    planted = [UNITS / (name + ".loadout-tmp") for name in (SERVICE, TIMER)]
    for link in planted:
        link.symlink_to(sentinel)
    h.run("G8-install-planted-scratch", [CLI, "service", "install"], {0, 1, 4}, observe=True,
          asserts=lambda code, text: [("sentinel unchanged", read(sentinel) == "outside sentinel\n"),
                                      ("planted links left alone", all(link.is_symlink() for link in planted))])
    for link in planted:
        link.unlink()


def names_cadence_change(text, requested, registered):
    """v5 (standing 167): one line names the cadence the loadout now asks for ("N minutes"), the
    cadence the job is registered with (M), and the repair (`service install`). Wording around them
    is free; v4 looked for the literal word "interval", which the product does not print."""
    for line in text.lower().splitlines():
        if (re.search(r"\b%d minutes\b" % requested, line) and re.search(r"\b%d\b" % registered, line)
                and "service install" in line):
            return True
    return False


def changed_settings(h, service_toml):
    """v2, U8 R9/R10: the recorded route ignores a broken ambient Context; status and doctor
    name an interval the loadout changed after install."""
    h.run("G8-recorded-route-ignores-ambient", [CLI, "update", "--background", "--service", service_toml], {0},
          env=h.env({"LOADOUT_STATE_DIR": "relative/state", "LOADOUT_HOME": "relative/home"}), timeout=900)
    commit("C-interval 30", {"loadout.toml": manifest({"interval_minutes": "30"})})
    sh(["git", "-C", CLONE, "pull", "-q", "--ff-only"])
    h.run("G8-status-names-changed-interval", [CLI, "service", "status"], {0, 4}, observe=True,
          asserts=lambda code, text: [("names requested 30, registered 15 and the reinstall",
                                       names_cadence_change(text, 30, 15))])
    h.run("G8-doctor-names-changed-interval", [CLI, "doctor"], {0, 4}, observe=True,
          asserts=lambda code, text: [("names requested 30, registered 15 and the reinstall",
                                       names_cadence_change(text, 30, 15))])
    commit("C-interval 15", {"loadout.toml": manifest({"interval_minutes": "15"})})
    sh(["git", "-C", CLONE, "pull", "-q", "--ff-only"])


def other_state_job(h):
    """v2, U8 R7: the one per-user job installed for another state dir makes this state's
    service status unhealthy instead of matching by name."""
    other = ACC / "state-b"
    env_b = h.env({"LOADOUT_STATE_DIR": str(other)})
    h.run("G8-other-state-init", [CLI, "init", REMOTE_URL, "--name", "vm-linux", "--no-service"], {0, 4},
          env=env_b, observe=True)
    h.run("G8-other-state-install", [CLI, "service", "install"], {0}, env=env_b)
    h.run("G8-status-job-belongs-to-other-state", [CLI, "service", "status"], {4})
    h.run("G8-other-state-uninstall", [CLI, "service", "uninstall"], {0}, env=env_b)
    h.run("G8-reinstall-after-other-state", [CLI, "service", "install"], {0})
    h.run("G8-status-after-reinstall", [CLI, "service", "status"], {0})
    h.run("G8-update-after-other-state", [CLI, "update"], {0, 4}, observe=True)


def non_utf8_destination(h, service_toml):
    """v2, U8 R8: an honored agent-dir override that isn't UTF-8 is refused before any write,
    or recorded exactly; never silently dropped."""
    odd = ACC / os.fsdecode(b"codex-\xff")
    units = snapshot(UNITS)
    record = read(service_toml)
    code, _ = h.run("G8-non-utf8-codex-home", [CLI, "service", "install"], {0, 4},
                    env=h.env({"CODEX_HOME": str(odd)}), observe=True)
    if code == 4:
        h.check("G8-non-utf8-refused-before-write", snapshot(UNITS) == units and read(service_toml) == record,
                "units and service.toml unchanged")
    elif code == 0:
        commit("C-codex", {"instructions/AGENTS.md": "# loadout-e2e instructions sentinel-instructions codex\n"})
        run_service(h, "G8-non-utf8-recorded-exactly", 0, lambda: [
            ("background wrote into the override", "codex" in read(odd / "AGENTS.md"))])
    h.run("G8-reinstall-plain", [CLI, "service", "install"], {0})


def uninstall_without_definitions(h, service_toml):
    """v2, U8 R6: uninstall asks the scheduler even when the local definitions are gone, and only
    then removes the record and reports success."""
    h.run("G8-install-for-missing-definitions", [CLI, "service", "install"], {0})
    (UNITS / SERVICE).unlink()
    (UNITS / TIMER).unlink()
    code, _ = h.run("G8-uninstall-missing-definitions", [CLI, "service", "uninstall"], {0, 1, 4}, observe=True)
    active = sh(["systemctl", "--user", "is-active", TIMER], check=False)
    if code == 0:
        h.check("G8-uninstall-missing-definitions-truthful", active != "active" and not service_toml.exists(),
                "timer %s, service.toml %s" % (active, service_toml.exists()))
    else:
        h.check("G8-uninstall-missing-definitions-kept-record", service_toml.exists(), "record kept on refusal")
    if active == "active":  # guest maintenance only, recorded: stop what the CLI left registered
        sh(["systemctl", "--user", "disable", "--now", TIMER], check=False)
        h.record("G8-guest-cleanup-timer", "observe", detail="timer was still active; disabled by the harness")
    sh(["systemctl", "--user", "daemon-reload"], check=False)


def status_json_case(h, case, env=None, cli=CLI):
    """`service status --json`: the job read back from the real scheduler agrees with service.toml.
    The exit may still be 4 for the background half ("no run yet", a blocked run)."""
    stdout = (CASES / case).with_suffix(".stdout")

    def asserts(code, text):
        try:
            report = json.loads(stdout.read_text())
        except ValueError:
            return [("status --json is JSON", False)]
        service = report.get("service") or {}
        scheduler = service.get("scheduler") or {}
        return [("no problem", service.get("problem") is None),
                ("no mismatch", service.get("mismatch") is None),
                ("scheduler registered", scheduler.get("registered") is True),
                ("recorded executable is this one", service.get("exe_current") is True),
                ("every scheduler file present", all(f.get("present") and f.get("regular")
                                                     for f in service.get("files", [])))]
    return h.run(case, [cli, "service", "status", "--json"], {0, 4}, env=env, asserts=asserts)


def native_formats(h, case):
    """v3, U8 B1/B2 native gate: keeps the scheduler's own output bytes, the formats the CLI
    parses (systemd multi-unit show blocks, launchctl's print layout)."""
    if IS_MAC:
        argvs = {"launchctl-print": ["launchctl", "print", "gui/%d/%s" % (os.getuid(), LAUNCHD_LABEL)],
                 "plist": ["plutil", "-p", PLIST]}
    else:
        argvs = {"systemctl-show": ["systemctl", "--user", "show", SERVICE, TIMER],
                 "systemctl-cat": ["systemctl", "--user", "cat", SERVICE, TIMER]}
    seen = {}
    for name, argv in argvs.items():
        done = subprocess.run([str(a) for a in argv], capture_output=True, stdin=subprocess.DEVNULL, timeout=60)
        (CASES / ("%s.%s.raw" % (case, name))).write_bytes(done.stdout + b"\n--- stderr ---\n" + done.stderr)
        seen[name] = "exit %s, %d bytes, non-ASCII %s" % (done.returncode, len(done.stdout),
                                                          any(byte > 127 for byte in done.stdout))
    h.record(case, "observe", detail=json.dumps(seen, sort_keys=True))


def unicode_job(h, service_toml):
    """v3, U8 B1/B2 native gate (scheduler Unicode): the CLI run from a folder whose name has a
    space, accented and CJK letters and an ampersand. Its path goes into the real scheduler
    definition, is read back as matching, and the scheduler really starts it."""
    prefix = "M8" if IS_MAC else "G8"
    UNICODE_DIR.mkdir(parents=True)
    copy = UNICODE_DIR / "loadout"
    shutil.copy2(CLI, copy)
    target = HOME / ".config/loadout-e2e/hello.txt"
    records = STATE / "background"
    try:
        h.run(prefix + "-unicode-install", [copy, "service", "install"], {0})
        # A newly enabled job may start a run at once; let it end before driving one by hand.
        if IS_MAC:
            launchd_drain()
        else:
            wait_for(lambda: unit_state(SERVICE).get("ActiveState") not in ("active", "activating"), 900, 2)
        native_formats(h, prefix + "-unicode-native-formats")
        status_json_case(h, prefix + "-unicode-definition-matches", cli=copy)
        h.run(prefix + "-unicode-status-from-installed-cli", [CLI, "service", "status"], {4})
        tag = prefix.lower() + "-unicode"
        commit("C-unicode", {"files/hello.txt": hello(tag)})
        runner = run_launchd if IS_MAC else run_service
        runner(h, prefix + "-unicode-run", 0, lambda: [
            ("applied by the copy's job", read(target) == hello(tag)),
            ("supervisor 0", json_file(records / "supervisor.json").get("code") == 0)])
        h.run(prefix + "-unicode-uninstall", [copy, "service", "uninstall"], {0}, asserts=lambda code, text: [
            ("service.toml gone", not service_toml.exists())])
    finally:
        h.run(prefix + "-reinstall-after-unicode", [CLI, "service", "install"], {0})


def partial_uninstall(h, service_toml):
    """v3, U8 B3 native gate (partial removal): one of the two unit files deleted by hand while
    the job is loaded. Uninstall still stops what systemd has loaded, removes the other file, and
    only then the record."""
    for case, gone in (("timer-file-deleted", TIMER), ("service-file-deleted", SERVICE)):
        h.run("G8-install-for-%s" % case, [CLI, "service", "install"], {0})
        (UNITS / gone).unlink()
        h.run("G8-uninstall-%s" % case, [CLI, "service", "uninstall"], {0}, asserts=lambda code, text: [
            ("timer not active", sh(["systemctl", "--user", "is-active", TIMER], check=False) != "active"),
            ("no unit file left", not (UNITS / SERVICE).exists() and not (UNITS / TIMER).exists()),
            ("service.toml gone", not service_toml.exists())])
        if sh(["systemctl", "--user", "is-active", TIMER], check=False) == "active":
            sh(["systemctl", "--user", "stop", TIMER], check=False)
            h.record("G8-guest-cleanup-%s" % case, "observe", detail="timer was still active; stopped by the harness")
        sh(["systemctl", "--user", "daemon-reload"], check=False)
        h.run("G8-status-after-%s" % case, [CLI, "service", "status", "--json"], {0, 4}, observe=True)


def parse_launchd(out):
    """v5 (standing 169): one `launchctl print` snapshot. state is the WHOLE value ("not running",
    "running", "spawn scheduled", ...); v4 kept only its first word, so "not" and "spawn" both read as
    finished."""
    state = re.search(r"(?m)^\s*state = (.+?)\s*$", out)
    runs = re.search(r"(?m)^\s*runs = (\d+)", out)
    last = re.search(r"(?m)^\s*last exit code = (-?\d+)", out)  # "(never exited)" is no code
    pid = re.search(r"(?m)^\s*pid = (\d+)", out)
    return {"loaded": state is not None, "state": state.group(1) if state else None,
            "runs": int(runs.group(1)) if runs else 0, "last": last.group(1) if last else None,
            "pid": int(pid.group(1)) if pid else None}


def launchd_state():
    return parse_launchd(sh(["launchctl", "print", "gui/%d/%s" % (os.getuid(), LAUNCHD_LABEL)], check=False))


def launchd_completed(snap, before_runs):
    """Finished, from ONE snapshot: loaded, more runs than before, state exactly "not running", no
    pid and an exit code recorded. Pending, spawn-scheduled, throttled or running is not finished. The
    exit code VALUE is not compared, so two successes in a row (both 0) still complete."""
    return bool(snap["loaded"] and snap["runs"] > before_runs and snap["state"] == "not running"
                and snap["pid"] is None and snap["last"] is not None)


def launchd_drain(timeout=900):
    """Settled: exactly "not running" with no pid, and the run count unchanged across 10 s, so no
    kickstart or StartInterval run is still pending."""
    def settled():
        first = launchd_state()
        if first["state"] != "not running" or first["pid"] is not None:
            return False
        time.sleep(10)
        second = launchd_state()
        return second["state"] == "not running" and second["pid"] is None and second["runs"] == first["runs"]
    return wait_for(settled, timeout, 2)


def supervisor_run():
    """The CLI's own run identity: supervisor.json `run` (`<secs>-<pid>-<nanos>`) and the file's mtime."""
    path = STATE / "background/supervisor.json"
    record = json_file(path)
    try:
        mtime = path.stat().st_mtime
    except OSError:
        mtime = None
    return record.get("run"), mtime


def run_pid(run):
    parts = str(run or "").split("-")
    return int(parts[1]) if len(parts) == 3 and parts[1].isdigit() else None


KICKSTART_SECONDS = 120


def kickstart_request(run=subprocess.run, seconds=KICKSTART_SECONDS):
    """v6 (standing 180): one bounded `launchctl kickstart -p` request, the same 120 s request/capture
    budget as v4's sh(). Returns (pid, None) with the pid kickstart printed, or (None, failure) for a
    timeout, a start error, a nonzero exit or no pid. Separate from the completion wait."""
    argv = ["launchctl", "kickstart", "-p", "gui/%d/%s" % (os.getuid(), LAUNCHD_LABEL)]
    try:
        kick = run(argv, capture_output=True, text=True, timeout=seconds, stdin=subprocess.DEVNULL)
    except subprocess.TimeoutExpired:
        return None, "harness: kickstart request did not return within %ss" % seconds
    except OSError as error:
        return None, "harness: kickstart request could not start: %s" % error
    out = (kick.stdout or "").strip()
    if kick.returncode != 0:
        return None, "harness: kickstart exit %s: %s" % (kick.returncode, (kick.stderr or "").strip()[:200])
    if not out.isdigit():
        return None, "harness: kickstart -p printed no pid (%r)" % out[:80]
    return int(out), None


def run_launchd(h, case, expect_code, extra_checks=None, timeout=900):
    """Starts the loaded agent once (`launchctl kickstart -p`) and waits for THAT run (v5, standing
    169): a pending run is drained first; completion is one coherent snapshot; the supervisor record
    must name a new run whose pid is the one kickstart started."""
    failures = []
    if not launchd_drain():
        failures.append("harness: the agent never settled before the kickstart")
    base = launchd_state()
    run_before, _ = supervisor_run()
    t0 = time.time()
    pid, request_failure = kickstart_request()
    if request_failure:
        # v6 (standing 180): no requested run to wait for; recorded before any completion wait.
        failures.append(request_failure)
        h.record(case, "fail", detail="; ".join(failures))
        return
    finished = wait_for(lambda: launchd_completed(launchd_state(), base["runs"]), timeout, 1)
    state = launchd_state()
    run_after, mtime = supervisor_run()
    if not finished:
        failures.append("requested run did not complete in %ss (%s)" % (timeout, json.dumps(state, sort_keys=True)))
    if run_after in (None, run_before) or mtime is None or mtime < t0 - 1:
        failures.append("supervisor record names no new run (before %s, after %s)" % (run_before, run_after))
    elif run_pid(run_after) != pid:
        failures.append("supervisor run %s is not the kickstarted pid %s" % (run_after, pid))
    if state["last"] != str(expect_code):
        failures.append("last exit code %s, wanted %s" % (state["last"], expect_code))
    for label, ok in (extra_checks() if extra_checks else []):
        if not ok:
            failures.append(label)
    h.record(case, "fail" if failures else "pass", detail="; ".join(failures) or json.dumps(state, sort_keys=True))


def g8_launchd(h):
    """macOS (hosted runner only): the real launchd agent in the runner user's GUI domain."""
    if not h.requires("M8", "bootstrap"):
        return
    domain = subprocess.run(["launchctl", "print", "gui/%d" % os.getuid()], capture_output=True, text=True)
    (CASES / "M8-gui-domain.txt").write_text(domain.stdout[:20000] + domain.stderr)
    if not h.check("M8-gui-domain", domain.returncode == 0,
                   "launchctl print gui/%d exit %s: %s" % (os.getuid(), domain.returncode, domain.stderr.strip()[:200])):
        h.skip_rest("service", "no launchd GUI domain on this runner (evidence in M8-gui-domain.txt)")
        return
    records = STATE / "background"
    target = HOME / ".config/loadout-e2e/hello.txt"
    service_toml = STATE / "service.toml"
    agents_before = snapshot(PLIST.parent)
    h.run("M8-isolated-install", [CLI, "service", "install"], {4}, env=h.env(isolated=ACC / "iso-launchd"),
          asserts=lambda code, text: [("LaunchAgents unchanged", snapshot(PLIST.parent) == agents_before)])
    h.run("M8-install", [CLI, "service", "install"], {0})
    idle = launchd_drain()
    h.check("M8-idle-after-install", idle, "no run left running after install")
    plist = {}
    try:
        plist = json.loads(sh(["plutil", "-convert", "json", "-o", "-", PLIST]))
    except (RuntimeError, ValueError) as error:
        h.record("M8-plist-readable", "fail", detail=str(error))
    for label, ok in (
            ("program arguments", plist.get("ProgramArguments") == [str(CLI), "update", "--background", "--service",
                                                                    str(service_toml)]),
            ("start interval 900", plist.get("StartInterval") == 900),
            ("background process type", plist.get("ProcessType") == "Background"),
            ("not run at load", plist.get("RunAtLoad") in (None, False)),
            ("loaded in gui domain", launchd_state()["loaded"]),
            ("service.toml 0600", service_toml.is_file() and oct(service_toml.stat().st_mode & 0o777) == "0o600")):
        h.check("M8-" + re.sub(r"[^a-z0-9]+", "-", label.lower()).strip("-"), ok, label)
    h.run("M8-service-status", [CLI, "service", "status"], {0})
    c10 = commit("C10 launchd", {"files/hello.txt": hello("c10")})
    run_launchd(h, "M8-run-applies", 0, lambda: [
        ("clone at C10", head() == c10), ("file c10", read(target) == hello("c10")),
        ("supervisor 0", json_file(records / "supervisor.json").get("code") == 0)])
    stray = CLONE / "untracked.txt"
    stray.write_text("x\n")
    commit("C11 while dirty", {"files/hello.txt": hello("c11")})
    run_launchd(h, "M8-run-blocked-dirty", 4, lambda: [("target unchanged", read(target) == hello("c10"))])
    h.run("M8-status-blocked", [CLI, "status"], {4})
    stray.unlink()
    run_launchd(h, "M8-run-after-clean", 0, lambda: [("file c11", read(target) == hello("c11"))])
    c12 = commit("C12 auto off", {"files/hello.txt": hello("c12"), "loadout.toml": manifest({"auto": "false"})})
    run_launchd(h, "M8-run-disabled", 0, lambda: [
        ("fetched", head() == c12), ("not applied", read(target) == hello("c11")),
        ("recorded disabled", json_file(records / "last-run.json").get("result") == "disabled")])
    h.run("M8-install-refuses-auto-false", [CLI, "service", "install"], {4})
    commit("C13 auto on", {"loadout.toml": manifest({"auto": "true"})})
    run_launchd(h, "M8-run-reenabled", 0, lambda: [("file c12", read(target) == hello("c12"))])
    if h.args.long_timer:
        # v5 (standing 169): settle every earlier kickstart first, then prove a NEW run that the
        # harness did not request (StartInterval) applied C14. v4 could credit a pending kickstart.
        settled = launchd_drain()
        base = launchd_state()
        run_before, _ = supervisor_run()
        t0 = time.time()
        c14 = commit("C14 interval", {"files/hello.txt": hello("c14")})

        def fired():
            run_after, mtime = supervisor_run()
            return (launchd_completed(launchd_state(), base["runs"]) and run_after not in (None, run_before)
                    and mtime is not None and mtime >= t0 and head() == c14 and read(target) == hello("c14"))
        ok = settled and wait_for(fired, 18 * 60, 10)
        h.check("M8-start-interval-fires", ok,
                "settled baseline (runs %s, run %s), then a scheduled run with no kickstart applied C14 within 18 minutes"
                % (base["runs"], run_before))
    native_formats(h, "M8-native-formats")
    status_json_case(h, "M8-definition-matches")
    unicode_job(h, service_toml)
    h.run("M8-uninstall", [CLI, "service", "uninstall"], {0}, asserts=lambda code, text: [
        ("plist gone", not PLIST.exists()), ("unloaded", not launchd_state()["loaded"]),
        ("service.toml gone", not service_toml.exists())])
    # v3, U8 B3 (partial removal): the plist deleted by hand while the agent is loaded.
    h.run("M8-install-for-plist-deleted", [CLI, "service", "install"], {0})
    PLIST.unlink()
    h.run("M8-uninstall-plist-deleted", [CLI, "service", "uninstall"], {0}, asserts=lambda code, text: [
        ("unloaded", not launchd_state()["loaded"]), ("service.toml gone", not service_toml.exists())])
    if launchd_state()["loaded"]:
        sh(["launchctl", "bootout", "gui/%d/%s" % (os.getuid(), LAUNCHD_LABEL)], check=False)
        h.record("M8-guest-cleanup-plist-deleted", "observe", detail="agent was still loaded; booted out by the harness")


def deadline_case(h, records):
    """The production deadline once: a held install through the real unit (about 61 minutes)."""
    (FAKE_STATE / "cargo").unlink()
    (FAKE_STATE / "hang").write_text("hold\n")
    commit("C15 deadline", {"files/hello.txt": hello("c15")})
    run_service(h, "G8-deadline", 1, lambda: [
        ("no fake installer left", not processes_matching("install pkg-cargo")),
        ("supervisor timed out", "timed" in json.dumps(json_file(records / "supervisor.json")))], timeout=66 * 60)
    (FAKE_STATE / "hang").unlink()
    h.run("G8-lock-free-after-deadline", [CLI, "status"], {0, 4}, observe=True)
    h.run("G8-rollback-after-deadline", [CLI, "rollback"], {0, 4}, observe=True)
    h.run("G8-update-after-deadline", [CLI, "update"], {0})


def soft_cancel_case(h, records):
    """v3, U4 I1 / U8 native gate (CLI worker cancellation receipt): the production soft deadline
    (55 minutes) asks a running worker to stop while its install is held; the install is let go
    at 57 minutes, before the 60-minute hard deadline. The worker then finishes by itself and its
    own record says cancelled, exit 4; the supervisor saw a clean exit 4."""
    (FAKE_STATE / "cargo").unlink(missing_ok=True)
    (FAKE_STATE / "hang").write_text("hold\n")
    commit("C15b soft cancel", {"files/hello.txt": hello("c15b")})
    target = HOME / ".config/loadout-e2e/hello.txt"
    started = unit_state(SERVICE).get("ExecMainStartTimestampMonotonic")
    t0 = time.monotonic()
    sh(["systemctl", "--user", "start", SERVICE])
    reached = wait_for(lambda: bool(processes_matching("install pkg-cargo")), 300)
    held = False
    if reached:
        time.sleep(max(0.0, t0 + 57 * 60 - time.monotonic()))
        held = bool(processes_matching("install pkg-cargo"))
    (FAKE_STATE / "hang").unlink()
    finished = wait_for(lambda: unit_state(SERVICE).get("ExecMainStartTimestampMonotonic") != started
                        and unit_state(SERVICE).get("ActiveState") in ("inactive", "failed"), 6 * 60, 2)
    state = unit_state(SERVICE)
    last = json_file(records / "last-run.json")
    supervisor = json_file(records / "supervisor.json")
    (CASES / "G8-soft-cancel.records.json").write_text(json.dumps(
        {"unit": state, "last-run": last, "supervisor": supervisor}, indent=2, sort_keys=True))
    failures = [label for label, ok in (
        ("install reached", reached), ("install still held past the soft deadline", held), ("finished", finished),
        ("ExecMainStatus 4", state.get("ExecMainStatus") == "4"),
        ("worker record cancelled", last.get("cancelled") is True),
        ("worker record exit 4", last.get("exit") == 4),
        ("worker record result applied", last.get("result") == "applied"),
        ("worker record says cancelled", "cancel" in str(last.get("message", "")).lower()),
        ("supervisor exited by itself", supervisor.get("outcome") == "exited"),
        ("supervisor saw the soft request", supervisor.get("cancelled") is True),
        ("supervisor code 4", supervisor.get("code") == 4),
        ("records pair by run id", supervisor.get("run") is not None and last.get("supervisor") == supervisor.get("run")),
        ("journaled write done before the cancel", read(target) == hello("c15b"))) if not ok]
    h.record("G8-soft-cancel-receipt", "fail" if failures else "pass",
             detail="; ".join(failures) or json.dumps(state, sort_keys=True))
    sh(["systemctl", "--user", "reset-failed", SERVICE], check=False)
    h.run("G8-status-after-soft-cancel", [CLI, "status"], {4})
    h.run("G8-update-after-soft-cancel", [CLI, "update"], {0})


def g9_self_update(h):
    if not h.requires("G9", "bootstrap"):
        return
    commit("C16 cli on", {"loadout.toml": manifest({"cli": "true"})})
    record = LOCAL_BIN / "loadout.install.toml"
    managed = h.expected.get("managed_" + OS_NAME, False)
    pristine = (sha256(CLI), read(record))
    h.run("G9-doctor-cli-install", [CLI, "doctor", "--json"], {0, 4}, observe=True)
    unmanaged = ACC / "unmanaged" / "loadout"
    unmanaged.parent.mkdir(parents=True)
    shutil.copy2(CLI, unmanaged)
    h.run("G9-unmanaged-skip", [unmanaged, "update"], {0}, asserts=lambda code, text: [
        ("skip says install record", "install record" in text or "not one of the release targets" in text),
        ("installed cli untouched", (sha256(CLI), read(record)) == pristine)])
    if managed and h.expected.get("network") == "offline":
        h.run("G9-managed-offline", [CLI, "update"], {1}, asserts=lambda code, text: [
            ("binary and record unchanged", (sha256(CLI), read(record)) == pristine)])
    else:
        h.run("G9-managed-offline", [CLI, "update"], {0, 1}, observe=True)
    record.write_text(read(record).replace('version = "%s"' % h.expected["version"], 'version = "0.0.1"'))
    h.run("G9-tampered-record-skip", [CLI, "update"], {0}, asserts=lambda code, text: [
        ("binary unchanged", sha256(CLI) == pristine[0])])
    h.run("G9-tampered-record-doctor", [CLI, "doctor", "--json"], {0, 4}, observe=True)
    record.write_text(pristine[1])
    iso = ACC / "iso-self-update"
    h.run("G9-isolated-init", [CLI, "init", REMOTE_URL, "--name", "vm-linux", "--no-service"], {0, 4},
          env=h.env(isolated=iso), observe=True)
    h.run("G9-isolated-skip", [CLI, "update"], {0, 4}, env=h.env(isolated=iso), asserts=lambda code, text: [
        ("self-update skip names isolation", re.search(r"self-update[^\n]*isolation", text) is not None)])
    tests = h.inputs / "tests"
    if tests.is_dir():
        for binary in sorted(tests.iterdir()):
            if binary.name.startswith("loadout-") and os.access(binary, os.X_OK):
                h.run("G9-release-unit-tests-%s" % binary.name, [binary, "release::", "--test-threads", "2"], {0},
                      timeout=900)
    commit("C17 cli off", {"loadout.toml": manifest({"cli": "false"})})
    h.run("G9-cli-off", [CLI, "update"], {0})


def g10_std_elevation(h):
    """Standard user: a system-wide program is blocked without --elevated and never prompts."""
    if not h.requires("G10", "bootstrap"):
        return
    h.check("G10-no-sudo", subprocess.run(["sudo", "-n", "true"], capture_output=True).returncode != 0,
            "standard user has no passwordless sudo")
    extra = (WORK / "os/linux.toml").read_text() + '\n[programs.e2e-apt]\napt = "loadout-e2e-no-such-package"\n'
    commit("C-std apt", {"os/linux.toml": extra})
    history = "/var/log/apt/history.log"
    before = snapshot(history)
    h.run("G10-foreground-needs-elevated", [CLI, "update"], {4}, timeout=300, asserts=lambda code, text: [
        ("names --elevated", "--elevated" in text), ("apt untouched", snapshot(history) == before)])
    run_service(h, "G10-background-blocked", 4, lambda: [("apt untouched", snapshot(history) == before)])
    commit("C-std apt removed", {"os/linux.toml": (h.inputs / "fixtures/loadout/os/linux.toml").read_text()})
    run_service(h, "G10-background-clean", 0)
    h.run("G10-uninstall", [CLI, "service", "uninstall"], {0})


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--in", dest="inputs", type=Path, required=True)
    parser.add_argument("--mode", choices=("tester", "std", "macos"), required=True)
    parser.add_argument("--long-timer", action="store_true")
    parser.add_argument("--long-deadline", action="store_true")
    args = parser.parse_args()
    OUT.mkdir(parents=True, exist_ok=True)
    h = Harness(args.inputs, args.mode)
    h.args = args
    started = time.time()
    g0_facts(h)
    servers = g1_fixture(h)
    groups = [g2_installer, g3_bootstrap, g4_init_refusals, g5_apply_cycle, g6_update,
              g7_background_by_hand, g8_service, g9_self_update]
    if args.mode == "std":
        groups = [g3_bootstrap, g8_service, g10_std_elevation]
    if args.mode == "macos":
        if not IS_MAC:
            sys.exit("--mode macos runs only on a macOS host")
        groups = [g2_installer, g3_bootstrap, g4_init_refusals, g5_apply_cycle, g6_update,
                  g7_background_by_hand, g8_launchd, g9_self_update]
    for group in groups:
        try:
            group(h)
        except Exception as error:  # a harness fault fails the group, never the whole campaign
            h.record(group.__name__, "fail", detail="harness error: %r" % (error,))
    summary = {"mode": args.mode, "counts": h.counts, "seconds": round(time.time() - started),
               "blocked": h.blocked}
    (OUT / ("summary-%s.json" % args.mode)).write_text(json.dumps(summary, indent=2, sort_keys=True))
    print(json.dumps(summary, sort_keys=True))
    if servers:
        for server in servers:
            server.shutdown()
    sys.exit(0 if h.counts["fail"] == 0 else 1)


if __name__ == "__main__":
    main()
