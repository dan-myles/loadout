import hashlib
import http.server
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
import warnings
import zipfile
from functools import partial
from pathlib import Path

SCRIPT = Path(__file__).with_name("install.ps1")
TARGET = "x86_64-pc-windows-msvc"
PREFIX = f"loadout-{TARGET}/"
SHELLS = [shell for shell in ("powershell", "pwsh") if shutil.which(shell)]
TIMEOUT = 300
SERVER_STOP_TIMEOUT = 30

# Loads install.ps1's functions and constants through the PowerShell parser, never its body, then
# runs each requested call and records its value or error message.
DRIVER = r"""
param([string]$Installer, [string]$CallsPath, [string]$ResultsPath)
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw "install.ps1 does not parse: $($errors[0])" }
foreach ($statement in $ast.EndBlock.Statements) {
    $isFunction = $statement -is [System.Management.Automation.Language.FunctionDefinitionAst]
    $isConstant = $statement -is [System.Management.Automation.Language.AssignmentStatementAst] -and
        @('Repo', 'RecordKeys', 'MaxRedirects', 'LoopbackHosts') -contains $statement.Left.VariablePath.UserPath
    if ($isFunction -or $isConstant) { . ([ScriptBlock]::Create($statement.Extent.Text)) }
}
$results = New-Object System.Collections.Generic.List[object]
foreach ($call in (Get-Content -LiteralPath $CallsPath -Raw -Encoding UTF8 | ConvertFrom-Json)) {
    $arguments = @($call.args)
    try {
        $value = & $call.fn @arguments
        if ($value -is [Uri]) { $value = $value.AbsoluteUri }
        $results.Add([pscustomobject]@{ ok = $true; value = $value; error = $null })
    } catch {
        $results.Add([pscustomobject]@{ ok = $false; value = $null; error = $_.Exception.Message })
    }
}
[IO.File]::WriteAllText($ResultsPath, (ConvertTo-Json -InputObject $results.ToArray() -Depth 6), (New-Object Text.UTF8Encoding($false)))
"""

ARGV_PROGRAM = (
    "import json, sys; "
    "open(sys.argv[1], 'w', encoding='utf-8').write(json.dumps(sys.argv[2:])); "
    "sys.exit(7)"
)
ARGV_CASES = [
    ["plain"],
    [""],
    ["", ""],
    ["a b"],
    ['say "hi"'],
    ["trailing\\"],
    ["box one\\"],
    ["C:\\path with space\\"],
    ["\\\\server\\share\\"],
    ["tab\there"],
    ["ünïcødé ✓ 漢字"],
    ['\\"'],
    ['a\\\\"b'],
    ["--name", "", "--no-service"],
    ["%PATH%", "^&|<>"],
]


def argv_calls(directory):
    # One runtime's Invoke-Native calls; each child writes its argv inside that runtime's directory.
    return [
        {"fn": "Invoke-Native", "args": [sys.executable, ["-c", ARGV_PROGRAM, str(Path(directory) / f"argv-{index}.json")] + case]}
        for index, case in enumerate(ARGV_CASES)
    ]


def argv_mismatches(directory, results):
    problems = []
    if len(results) != len(ARGV_CASES):
        problems.append(f"{len(results)} results for {len(ARGV_CASES)} cases")
    for index, (case, result) in enumerate(zip(ARGV_CASES, results)):
        if not result["ok"]:
            problems.append(f"case {index}: {result['error']}")
            continue
        if result["value"] != 7:
            problems.append(f"case {index}: exit {result['value']}, not 7")
        written = Path(directory) / f"argv-{index}.json"
        if not written.exists():
            problems.append(f"case {index}: the child wrote no {written.name}")
            continue
        received = json.loads(written.read_text(encoding="utf-8"))
        if received != case:
            problems.append(f"case {index}: sent {case!r}, child received {received!r}")
    return problems


def zip_calls(directory, zip_paths):
    # Export-ZipBinary opens its output with CreateNew, so each runtime needs its own output names.
    return [
        {"fn": "Export-ZipBinary", "args": [str(zip_path), TARGET, str(Path(directory) / f"out-{index}.exe")]}
        for index, zip_path in enumerate(zip_paths)
    ]


def record_text(binary, binary_sha, installer="install.ps1", newline="\n", extra=""):
    lines = [
        "# loadout install record. Written by install.sh or install.ps1 and read by loadout self-update. Do not edit.",
        "schema = 1",
        f'installer = "{installer}"',
        'repository = "dan-myles/loadout"',
        f'source = "https://github.com/dan-myles/loadout/releases/latest/download/loadout-{TARGET}.zip"',
        f'target = "{TARGET}"',
        'version = "1.2.3"',
        f'archive_sha256 = "{"a" * 64}"',
        'binary = "' + binary.replace("\\", "\\\\").replace('"', '\\"') + '"',
        f'binary_sha256 = "{binary_sha}"',
    ]
    return newline.join(lines) + newline + extra


@unittest.skipUnless(os.name == "nt" and SHELLS, "needs Windows PowerShell")
class InstallPs1FunctionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        (self.tmp / "driver.ps1").write_text(DRIVER, encoding="utf-8-sig")

    def run_calls(self, build_calls):
        # Every runtime gets a fresh directory for its calls, results and any files its calls write,
        # and is reported with its own directory so assertions never read another runtime's output.
        outcomes = {}
        for shell in SHELLS:
            directory = self.tmp / f"runtime-{shell}"
            directory.mkdir()
            calls = build_calls(directory)
            calls_path = directory / "calls.json"
            results_path = directory / "results.json"
            calls_path.write_text(json.dumps(calls), encoding="utf-8")
            completed = subprocess.run(
                [shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File",
                 str(self.tmp / "driver.ps1"), str(SCRIPT), str(calls_path), str(results_path)],
                cwd=str(directory),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                universal_newlines=True,
                timeout=TIMEOUT,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            loaded = json.loads(results_path.read_text(encoding="utf-8-sig"))
            results = loaded if isinstance(loaded, list) else [loaded]
            self.assertEqual(len(results), len(calls), f"{shell} returned a result for every call")
            outcomes[shell] = (directory, calls, results)
        return outcomes

    def assert_outcomes(self, build_calls, expected_ok):
        outcomes = self.run_calls(build_calls)
        for shell, (_, calls, results) in outcomes.items():
            self.assertEqual(len(calls), len(expected_ok))
            for call, result, ok in zip(calls, results, expected_ok):
                with self.subTest(shell=shell, call=json.dumps(call["args"], ensure_ascii=False)):
                    self.assertEqual(result["ok"], ok, result["error"])
        return outcomes

    def test_native_arguments_round_trip_with_exit_code(self):
        for shell, (directory, _, results) in self.run_calls(argv_calls).items():
            with self.subTest(shell=shell):
                self.assertEqual(argv_mismatches(directory, results), [])

    def test_base_url_policy(self):
        cases = {
            "https://mirror.example/loadout": True,
            "http://127.0.0.1:8000/": True,
            "http://localhost/": True,
            "http://[::1]:9000": True,
            "http://example.com/": False,
            "http://127.0.0.1:80@example.invalid/": False,
            "http://127.0.0.1.example.invalid/": False,
            "http://localhost.example.invalid/": False,
            "https://example.com/a?b": False,
            "https://example.com/#f": False,
            "ftp://127.0.0.1/": False,
            "not a url": False,
        }
        calls = [{"fn": "Get-BaseUri", "args": [url]} for url in cases]
        self.assert_outcomes(lambda directory: calls, list(cases.values()))

    def test_https_only_hop_policy(self):
        cases = [
            (["https://github.com/x", False], True),
            (["http://github.com/x", False], False),
            (["http://127.0.0.1:8000/x", False], False),
            (["http://127.0.0.1:8000/x", True], True),
            (["http://example.com/x", True], False),
            (["https://user:pw@example.com/x", False], False),
        ]
        calls = [{"fn": "Assert-DownloadUri", "args": args} for args, _ in cases]
        self.assert_outcomes(lambda directory: calls, [ok for _, ok in cases])

    def test_sidecar_must_be_exactly_one_line(self):
        asset = f"loadout-{TARGET}.zip"
        digest = "b" * 64
        line = f"{digest}  {asset}\n"
        cases = [
            (line.encode("ascii"), True),
            (f"{digest}  {asset}\n\n".encode(), False),
            (f"{digest}  {asset}\r\n".encode(), False),
            (f"{digest}  {asset}".encode(), False),
            (f"{digest}  {asset}\nextra\n".encode(), False),
            (f"{digest.upper()}  {asset}\n".encode(), False),
            (f"{digest}  loadout-other.zip\n".encode(), False),
            (b"\xef\xbb\xbf" + line.encode("ascii"), False),
            (line.encode("utf-16"), False),
            (line.encode("utf-16-le"), False),
            (line.encode("utf-16-be"), False),
            (line.encode("utf-32"), False),
            (f"{digest}\u00a0 {asset}\n".encode("utf-8"), False),
            (line.encode("ascii")[:-1] + b"\x00", False),
        ]
        calls = []
        for index, (data, _) in enumerate(cases):
            path = self.tmp / f"sidecar-{index}"
            path.write_bytes(data)
            calls.append({"fn": "Read-Sidecar", "args": [str(path), asset]})
        self.assert_outcomes(lambda directory: calls, [ok for _, ok in cases])

    def write_zip(self, name, entries):
        path = self.tmp / name
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            with zipfile.ZipFile(path, "w") as archive:
                for entry_name, data, mode in entries:
                    info = zipfile.ZipInfo(entry_name)
                    info.create_system = 3
                    info.external_attr = mode << 16
                    archive.writestr(info, data)
        return path

    def test_zip_layout_is_checked_and_only_the_binary_is_extracted(self):
        exe = (f"{PREFIX}loadout.exe", b"MZ fake", 0o100755)
        good = [(PREFIX, b"", 0o040755), exe, (f"{PREFIX}README.md", b"readme", 0o100644)]
        cases = {
            "good": (good, True),
            "duplicate binary": (good + [exe], False),
            "binary is a symlink": ([(f"{PREFIX}loadout.exe", b"C:\\Windows\\notepad.exe", 0o120777)], False),
            "entry outside the prefix": (good + [("evil.exe", b"x", 0o100644)], False),
            "nested entry": (good + [(f"{PREFIX}sub/loadout.exe", b"x", 0o100644)], False),
            "parent traversal": (good + [(f"{PREFIX}../evil.exe", b"x", 0o100644)], False),
            "backslash name": (good + [(f"{PREFIX}..\\evil.exe", b"x", 0o100644)], False),
            "missing binary": ([(f"{PREFIX}README.md", b"readme", 0o100644)], False),
        }
        zip_paths = [self.write_zip(f"case-{index}.zip", entries) for index, (entries, _) in enumerate(cases.values())]
        outcomes = self.assert_outcomes(lambda directory: zip_calls(directory, zip_paths), [ok for _, ok in cases.values()])
        for shell, (directory, _, _) in outcomes.items():
            with self.subTest(shell=shell):
                self.assertEqual((directory / "out-0.exe").read_bytes(), b"MZ fake")
                self.assertFalse((directory / "evil.exe").exists())
        self.assertFalse((self.tmp / "evil.exe").exists())

    def test_record_parser_accepts_only_the_schema_1_data_format(self):
        binary = "C:\\Users\\me\\AppData\\Local\\Programs\\loadout\\loadout.exe"
        good = record_text(binary, "c" * 64)
        cases = {
            "good": (good, True),
            "byte order mark": ("\ufeff" + good, False),
            "crlf": (record_text(binary, "c" * 64, newline="\r\n"), False),
            "no final newline": (good[:-1], False),
            "unknown key": (good + 'extra = "x"\n', False),
            "duplicate key": (good + 'version = "9"\n', False),
            "missing key": (good.replace('version = "1.2.3"\n', ""), False),
            "schema as string": (good.replace("schema = 1", 'schema = "1"'), False),
            "bare value": (good.replace('version = "1.2.3"', "version = 1.2.3"), False),
            "control in comment": (good.replace("Do not edit.", "Do not\tedit."), False),
            "expression": (good.replace('version = "1.2.3"', 'version = "$(Remove-Item x)"\n$(Remove-Item x)'), False),
        }
        calls = []
        for index, (text, _) in enumerate(cases.values()):
            path = self.tmp / f"record-{index}.toml"
            path.write_bytes(text.encode("utf-8"))
            calls.append({"fn": "Read-InstallRecord", "args": [str(path)]})
        self.assert_outcomes(lambda directory: calls, [ok for _, ok in cases.values()])

    def test_record_must_bind_installer_target_path_and_current_hash(self):
        dest = self.tmp / "Programs" / "loadout" / "loadout.exe"
        dest.parent.mkdir(parents=True)
        dest.write_bytes(b"MZ installed")
        digest = hashlib.sha256(b"MZ installed").hexdigest()
        cases = {
            "matching": (record_text(str(dest), digest), None),
            "path differs only in case": (record_text(str(dest).upper(), digest), None),
            "edited binary": (record_text(str(dest), "d" * 64), "changed after it was installed"),
            "other installer": (record_text(str(dest), digest, installer="install.sh"), "another installer"),
            "other path": (record_text(str(dest) + ".old", digest), "another binary path"),
        }
        calls = []
        for index, (text, _) in enumerate(cases.values()):
            path = self.tmp / f"bind-{index}.toml"
            path.write_bytes(text.encode("utf-8"))
            calls.append({"fn": "Get-RecordMismatch", "args": [str(path), str(dest), TARGET]})
        calls.append({"fn": "Get-RecordMismatch", "args": [str(self.tmp / "absent.toml"), str(dest), TARGET]})
        expected = [why for _, why in cases.values()] + ["there is no install record"]
        self.assertEqual(len(expected), len(calls))
        for shell, (_, _, results) in self.run_calls(lambda directory: calls).items():
            for result, why in zip(results, expected):
                with self.subTest(shell=shell, why=why):
                    self.assertTrue(result["ok"], result["error"])
                    if why is None:
                        self.assertIsNone(result["value"])
                    else:
                        self.assertIn(why, result["value"])


class RuntimeIsolationControlTest(unittest.TestCase):
    """Checks the per-runtime fixture plumbing itself; needs no PowerShell, so it runs everywhere."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)

    def write_argv_outputs(self, directory, received):
        directory.mkdir()
        for index, argv in enumerate(received):
            (directory / f"argv-{index}.json").write_text(json.dumps(argv), encoding="utf-8")

    def test_each_runtime_writes_into_its_own_directory(self):
        first, second = self.tmp / "runtime-powershell", self.tmp / "runtime-pwsh"
        for build in (argv_calls, lambda directory: zip_calls(directory, [self.tmp / "a.zip", self.tmp / "b.zip"])):
            first_outputs = [call["args"][-1] if call["fn"] == "Export-ZipBinary" else call["args"][1][2] for call in build(first)]
            second_outputs = [call["args"][-1] if call["fn"] == "Export-ZipBinary" else call["args"][1][2] for call in build(second)]
            self.assertTrue(all(Path(path).parent == first for path in first_outputs))
            self.assertTrue(all(Path(path).parent == second for path in second_outputs))
            self.assertFalse(set(first_outputs) & set(second_outputs))

    def test_first_runtime_mismatch_is_reported_even_when_the_next_runtime_is_correct(self):
        ok = [{"ok": True, "value": 7, "error": None} for _ in ARGV_CASES]
        wrong = [list(case) for case in ARGV_CASES]
        wrong[3] = ["a", "b"]
        self.write_argv_outputs(self.tmp / "runtime-powershell", wrong)
        self.write_argv_outputs(self.tmp / "runtime-pwsh", ARGV_CASES)
        first = argv_mismatches(self.tmp / "runtime-powershell", ok)
        self.assertEqual(len(first), 1)
        self.assertIn("case 3", first[0])
        self.assertEqual(argv_mismatches(self.tmp / "runtime-pwsh", ok), [])

    def test_missing_output_wrong_exit_and_short_results_are_reported(self):
        directory = self.tmp / "runtime-powershell"
        self.write_argv_outputs(directory, ARGV_CASES[:-1])
        results = [{"ok": True, "value": 7, "error": None} for _ in ARGV_CASES]
        results[0] = {"ok": True, "value": 0, "error": None}
        problems = argv_mismatches(directory, results)
        self.assertTrue(any("case 0: exit 0" in problem for problem in problems))
        self.assertTrue(any(f"case {len(ARGV_CASES) - 1}: the child wrote no" in problem for problem in problems))
        self.assertTrue(any("results for" in problem for problem in argv_mismatches(directory, results[:-1])))


FAKE_LOADOUT_CS = r"""
using System;
using System.IO;
public static class FakeLoadout {
    public static int Main(string[] args) {
        if (args.Length == 1 && args[0] == "--version") { Console.WriteLine("loadout 1.2.3"); return 0; }
        File.WriteAllText(Environment.GetEnvironmentVariable("LT_RECEIVED"), String.Join("\n", args) + "\n");
        return Int32.Parse(Environment.GetEnvironmentVariable("LT_CHILD_EXIT"));
    }
}
"""

# Runs install.ps1 the two documented ways: as a script file (so it uses `exit`) and as an in-memory
# script block (so it must set $LASTEXITCODE instead). It then reports the code it observed and
# proves the host kept running.
WRAPPER = r"""
$ErrorActionPreference = 'Stop'
$params = @{
    BaseUrl = $env:LT_BASE
    LoadoutUrl = $env:LT_URL
    InitArgs = [string[]]@(Get-Content -LiteralPath $env:LT_ARGS -Raw -Encoding UTF8 | ConvertFrom-Json)
}
$global:LASTEXITCODE = 0
if ($env:LT_MODE -eq 'file') {
    & $env:LT_INSTALLER @params
} else {
    & ([scriptblock]::Create([IO.File]::ReadAllText($env:LT_INSTALLER))) @params
}
$code = $global:LASTEXITCODE
[Console]::Out.WriteLine("wrapper-alive $code")
exit $code
"""


def native_target():
    import winreg

    key_path = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment"
    with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, key_path) as key:
        native = winreg.QueryValueEx(key, "PROCESSOR_ARCHITECTURE")[0]
    return {"AMD64": "x86_64", "ARM64": "aarch64"}[native] + "-pc-windows-msvc"


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass


class FixtureServer:
    # Serves a directory on loopback from its own daemon thread. It doesn't use serve_forever or
    # shutdown(), whose wait has no deadline: the loop checks an owned stop event at least every POLL
    # seconds, and both the wait for a request and accept() are bounded by POLL.
    POLL = 0.1

    def __init__(self, directory):
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), partial(QuietHandler, directory=str(directory)))
        try:
            self.httpd.timeout = self.POLL
            self.httpd.socket.settimeout(self.POLL)
            self.stop_requested = threading.Event()
            self.thread = threading.Thread(target=self.serve, daemon=True)
            self.base = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        except BaseException:
            self.httpd.server_close()
            raise

    def serve(self):
        while not self.stop_requested.is_set():
            self.httpd.handle_request()

    def start(self):
        self.thread.start()

    def stop(self, timeout):
        # One deadline covers the whole stop: request it, then a single bounded join. True only if
        # the serving thread has actually ended.
        self.stop_requested.set()
        if self.thread.ident is None:
            return True
        self.thread.join(timeout)
        return not self.thread.is_alive()

    def close(self):
        self.httpd.server_close()


def stop_fixture_server(server, timeout=SERVER_STOP_TIMEOUT):
    if not server.stop(timeout):
        raise RuntimeError(f"fixture server thread {server.thread.name} still running {timeout} s after stop")


class FixtureServerControlTest(unittest.TestCase):
    # The wrapper fixture's lifetime, checked without PowerShell, so it runs anywhere.

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp, True)
        (self.tmp / "asset.txt").write_bytes(b"served")

    def start_server(self, server_class=FixtureServer):
        server = server_class(self.tmp)
        self.addCleanup(server.close)
        server.start()
        self.addCleanup(stop_fixture_server, server)
        return server

    def test_normal_stop_ends_the_serving_thread_within_the_deadline(self):
        server = self.start_server()
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(f"{server.base}/asset.txt", timeout=10) as response:
            self.assertEqual(response.read(), b"served")
        began = time.monotonic()
        self.assertTrue(server.stop(5))
        self.assertLess(time.monotonic() - began, 5)
        self.assertFalse(server.thread.is_alive())

    def test_stopping_again_or_before_start_returns_at_once(self):
        server = self.start_server()
        self.assertTrue(server.stop(5))
        began = time.monotonic()
        self.assertTrue(server.stop(5))
        self.assertLess(time.monotonic() - began, 1)
        unstarted = FixtureServer(self.tmp)
        self.addCleanup(unstarted.close)
        self.assertTrue(unstarted.stop(5))
        stop_fixture_server(unstarted, 5)

    def test_a_server_that_ignores_stop_fails_within_the_deadline(self):
        release = threading.Event()

        class Stuck(FixtureServer):
            def serve(self):
                release.wait(60)

        server = self.start_server(Stuck)
        self.addCleanup(release.set)
        began = time.monotonic()
        self.assertFalse(server.stop(0.5))
        with self.assertRaisesRegex(RuntimeError, "still running"):
            stop_fixture_server(server, 0.5)
        self.assertLess(time.monotonic() - began, 3)
        self.assertTrue(server.thread.is_alive())
        release.set()
        self.assertTrue(server.stop(5))


@unittest.skipUnless(
    os.name == "nt" and shutil.which("powershell") and shutil.which("git"),
    "needs Windows PowerShell 5.1 (to build the fixture) and git",
)
class InstallPs1WrapperTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = Path(tempfile.mkdtemp())
        cls.addClassCleanup(shutil.rmtree, cls.root, True)
        served = cls.root / "served"
        served.mkdir()
        source = cls.root / "FakeLoadout.cs"
        source.write_text(FAKE_LOADOUT_CS, encoding="utf-8")
        exe = cls.root / "loadout.exe"
        subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command",
             "Add-Type -TypeDefinition ([IO.File]::ReadAllText($env:LT_SOURCE)) "
             "-OutputAssembly $env:LT_EXE -OutputType ConsoleApplication"],
            cwd=str(cls.root),
            env=dict(os.environ, LT_SOURCE=str(source), LT_EXE=str(exe)),
            check=True,
            timeout=TIMEOUT,
        )
        target = native_target()
        asset = f"loadout-{target}.zip"
        with zipfile.ZipFile(served / asset, "w") as archive:
            for name, data, mode in (
                (f"loadout-{target}/", b"", 0o040755),
                (f"loadout-{target}/loadout.exe", exe.read_bytes(), 0o100755),
            ):
                info = zipfile.ZipInfo(name)
                info.create_system = 3
                info.external_attr = mode << 16
                archive.writestr(info, data)
        digest = hashlib.sha256((served / asset).read_bytes()).hexdigest()
        (served / f"{asset}.sha256").write_bytes(f"{digest}  {asset}\n".encode("ascii"))
        (cls.root / "wrapper.ps1").write_text(WRAPPER, encoding="utf-8-sig")
        server = FixtureServer(served)
        cls.addClassCleanup(server.close)
        server.start()
        cls.addClassCleanup(stop_fixture_server, server)
        cls.base = server.base

    def run_installer(self, shell, command, init_args, child_exit, extra_env=None):
        case = Path(tempfile.mkdtemp(dir=str(self.root)))
        (case / "args.json").write_text(json.dumps(init_args), encoding="utf-8")
        received = case / "received.txt"
        env = dict(
            os.environ,
            LOCALAPPDATA=str(case / "LocalAppData"),
            LT_BASE=self.base,
            LT_URL="https://example.invalid/me/loadout.git",
            LT_ARGS=str(case / "args.json"),
            LT_INSTALLER=str(SCRIPT),
            LT_RECEIVED=str(received),
            LT_CHILD_EXIT=str(child_exit),
            **(extra_env or {}),
        )
        completed = subprocess.run(
            [shell, "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass"] + command,
            cwd=str(case),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            universal_newlines=True,
            timeout=TIMEOUT,
        )
        installed = case / "LocalAppData" / "Programs" / "loadout" / "loadout.exe"
        return completed, received, installed

    def wrapped(self, shell, mode, init_args, child_exit):
        return self.run_installer(shell, ["-File", str(self.root / "wrapper.ps1")], init_args, child_exit,
                                  {"LT_MODE": mode})

    def test_child_exit_code_is_the_installer_exit_code(self):
        for shell in SHELLS:
            for mode in ("file", "block"):
                for child_exit in (0, 9):
                    with self.subTest(shell=shell, mode=mode, child_exit=child_exit):
                        completed, received, installed = self.wrapped(shell, mode, ["--name", "box one", ""], child_exit)
                        self.assertEqual(completed.returncode, child_exit, completed.stderr)
                        self.assertIn(f"wrapper-alive {child_exit}", completed.stdout)
                        self.assertTrue(installed.exists())
                        self.assertEqual(
                            received.read_text(encoding="utf-8").split("\n")[:-1],
                            ["init", "https://example.invalid/me/loadout.git", "--name", "box one", ""],
                        )

    def test_init_start_failure_completes_with_1_not_a_stale_success(self):
        too_long = ["x" * 40000]
        for shell in SHELLS:
            for mode in ("file", "block"):
                with self.subTest(shell=shell, mode=mode):
                    completed, received, installed = self.wrapped(shell, mode, too_long, 0)
                    self.assertEqual(completed.returncode, 1, completed.stderr)
                    self.assertIn("wrapper-alive 1", completed.stdout)
                    self.assertIn("could not run", completed.stderr)
                    self.assertTrue(installed.exists())
                    self.assertFalse(received.exists())

    def test_direct_file_invocation_forwards_a_nonzero_child_exit(self):
        for shell in SHELLS:
            with self.subTest(shell=shell):
                command = ["-File", str(SCRIPT), "-BaseUrl", self.base,
                           "-LoadoutUrl", "https://example.invalid/me/loadout.git", "-InitArgs", "laptop"]
                completed, received, installed = self.run_installer(shell, command, [], 9)
                self.assertEqual(completed.returncode, 9, completed.stderr)
                self.assertTrue(installed.exists())
                self.assertEqual(
                    received.read_text(encoding="utf-8").split("\n")[:-1],
                    ["init", "https://example.invalid/me/loadout.git", "laptop"],
                )


if __name__ == "__main__":
    unittest.main()
