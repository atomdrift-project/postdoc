#!/usr/bin/env python3
"""Exercise third-party scanner installation without changing the host."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent

# One mock for every command the installer runs. It records each call, and
# does what the real command would leave behind inside the fixture.
MOCK = r'''#!{python}
import json, os, pathlib, subprocess, sys, time
root = pathlib.Path(os.environ["TEST_ROOT"])
command, args = pathlib.Path(sys.argv[0]).name, sys.argv[1:]
target = sys.argv[0]
while command in ("sudo", "doas", "env"):
    if command == "env":
        while args and "=" in args[0]:
            args = args[1:]
    target, args = args[0], args[1:]
    command = pathlib.Path(target).name
with (root / "calls").open("a") as stream:
    stream.write(json.dumps([command, args]) + "\n")
# What privilege wraps but this mock does not stand in for runs for real.
if command not in {mocked} and not command.startswith("python3."):
    os.execvp(target, [target, *args])
tools, etc = root / "tools", root / "etc"

def tool(name, version="1.0"):
    path = tools / name
    path.write_text("#!/bin/sh\necho " + name + " version " + version + "\n")
    path.chmod(0o755)

def python(path):
    # Another Python, answered by this same mock.
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text((root / "commands" / "python3").read_text())
    path.chmod(0o755)

# A clamd that answers PING (or, with CLAMD_SILENT, a socket nothing
# answers on), outliving this mock until the test is over.
CLAMD = """
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
s.listen()
s.settimeout(30)
try:
    while sys.argv[2] != "silent":
        c, _ = s.accept()
        c.recv(16)
        c.sendall(b"PONG\\0")
        c.close()
    s.accept()
except OSError:
    pass
"""

def listen(path):
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        path.unlink()
    subprocess.Popen([sys.executable, "-c", CLAMD, str(path), os.environ.get("CLAMD_SILENT") or "answer"],
                     start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    for _ in range(100):
        if path.exists():
            break
        time.sleep(0.05)

if command in ("apt-get", "pkg", "brew") and "install" in args:
    for package in args[args.index("install") + 1:]:
        if package in ("clamav", "clamav-daemon"):
            tool("clamd")
            (etc / "clamd.conf").write_text("Example\n#LocalSocket /tmp/x\n")
            (etc / "freshclam.conf").write_text("Example\nDatabaseMirror x\n")
        elif package == "malcontent":
            tool("mal", "1.28.1")
        elif package == "python3.13":
            python(root / "commands" / package)
        elif package == "python@3.13":
            python(root / "prefix" / "bin" / "python3.13")
elif command == "brew" and args == ["--prefix"]:
    print(root / "prefix")
elif command == "brew" and args[:1] == ["services"] and args[1] in ("start", "restart"):
    listen(pathlib.Path(os.environ["CLAMD_SOCKET"]))
elif command == "systemctl" and args[0] == "cat":
    sys.exit(0 if args[1] in os.environ.get("UNITS", "").split(",") else 1)
elif command == "systemctl" and args[0] == "restart":
    listen(pathlib.Path(os.environ["CLAMD_SOCKET"]))
elif command == "systemctl" and args[:2] == ["enable", "--now"]:
    if "freshclam" in args[2]:
        (root / "db" / "daily.cvd").write_text("db")
    else:
        listen(pathlib.Path(os.environ["CLAMD_SOCKET"]))
elif command == "id":
    print("nobody")
elif command.startswith("python3") and args[:1] == ["-c"] and "version_info" in args[1]:
    print(command[len("python"):] if "." in command else os.environ.get("PYTHON_VERSION", "3.12"))
elif command.startswith("python3") and args[:2] == ["-m", "venv"]:
    venv = pathlib.Path(args[-1])
    (venv / "bin").mkdir(parents=True, exist_ok=True)
    pip = venv / "bin" / "pip"
    pip.write_text("#!/bin/sh\nexec " + str(root / "commands" / "python3-pip") + " \"$@\"\n")
    pip.chmod(0o755)
elif command == "python3":
    sys.exit(subprocess.call([sys.executable, *args]))
elif command == "python3-pip":
    venv_bin = pathlib.Path(os.environ["SCANNER_PREFIX"]) / "lib/postdoc-guarddog/bin"
    path = venv_bin / "guarddog"
    path.write_text("#!/bin/sh\necho guarddog, version 3.0.1\n")
    path.chmod(0o755)
'''

COMMANDS = ("sudo", "apt-get", "pkg", "brew", "systemctl", "freshclam", "usermod", "id",
            "python3", "python3-pip", "sysrc", "service", "pw")


class ScannerInstallTests(unittest.TestCase):
    def exercise(self, platform="Linux", scanners="clamav,malcontent,guarddog", manager="apt-get",
                 units=(), tools=(), user="postdoc", clamd_conf=None, python="3.12", silent=False):
        with tempfile.TemporaryDirectory(prefix="postdoc-scanners-test-") as tmp:
            root = Path(tmp)
            for directory in ("commands", "tools", "etc", "db", "prefix", "scripts"):
                (root / directory).mkdir()
            mock = MOCK.replace("{python}", sys.executable).replace("{mocked}", repr(set(COMMANDS) - {"sudo"}))
            for name in COMMANDS:
                if name in ("apt-get", "pkg", "brew") and name != manager:
                    continue
                path = root / "commands" / name
                path.write_text(mock)
                path.chmod(0o755)
            for name in tools:
                path = root / "tools" / name
                path.write_text("#!/bin/sh\necho " + name + " version 1.0\n")
                path.chmod(0o755)
            if clamd_conf is not None:
                (root / "etc" / "clamd.conf").write_text(clamd_conf.replace("{socket}", str(root / "run" / "clamd.ctl")))
            socket = root / "run" / "clamd.ctl"
            # Every absolute path the installer touches is moved into the fixture.
            script = (SCRIPTS / "deploy-scanners.sh").read_text()
            for old, new in (
                ('"/etc/clamav/clamd.conf /etc/clamd.d/scan.conf /etc/clamd.conf /usr/local/etc/clamd.conf"', f'"{root}/etc/clamd.conf"'),
                ('"/etc/clamav/freshclam.conf /etc/freshclam.conf /usr/local/etc/freshclam.conf"', f'"{root}/etc/freshclam.conf"'),
                ('"/var/lib/clamav /var/db/clamav"', f'"{root}/db"'),
                ("/run/clamav/clamd.ctl", str(socket)),
                ("/var/run/clamav/clamd.sock", str(socket)),
            ):
                self.assertIn(old, script)
                script = script.replace(old, new)
            (root / "scripts" / "deploy-scanners.sh").write_text(script)
            shutil.copy(SCRIPTS / "nono-stub-wheel.py", root / "scripts")
            env = {
                "PATH": f"{root}/commands:/usr/bin:/bin",
                "TEST_ROOT": str(root),
                "SCANNERS": scanners,
                "SCANNER_PREFIX": str(root / "prefix"),
                "UNITS": ",".join(units),
                "CLAMD_SOCKET": str(socket),
                "CLAMD_WAIT": "4",
                "FRESHCLAM_WAIT": "4",
                "HOME": str(root),
                "PYTHON_VERSION": python,
                "CLAMD_SILENT": "silent" if silent else "",
            }
            command = '''
set -eu
SUDO=sudo
log() { printf '%s\\n' "$*"; }
die() { printf '%s\\n' "$*" >&2; exit 1; }
DEPLOY_DIR="$1"
DEPLOY_TMP="$2"
SERVICE_PATH="$3"
. "$DEPLOY_DIR/deploy-scanners.sh"
if [ "$4" = Darwin ]; then SCANNER_PREFIX="$(brew --prefix)"; fi
ensure_scanners "$4" "$5"
echo "changed=${DEPLOY_CHANGED:-0}"
'''
            result = subprocess.run(
                ["/bin/sh", "-c", command, "scanners-test", str(root / "scripts"), tmp,
                 f"{root}/tools:{root}/prefix/bin", platform, user],
                env=env, capture_output=True, text=True, timeout=60,
            )
            calls_file = root / "calls"
            calls = [json.loads(line) for line in calls_file.read_text().splitlines()] if calls_file.exists() else []
            conf = (root / "etc" / "clamd.conf").read_text() if (root / "etc" / "clamd.conf").exists() else ""
            linked = (root / "prefix" / "bin" / "guarddog").exists()
            # Which Python the virtualenv was built with.
            built = [command for command, args in calls if args[:2] == ["-m", "venv"]]
            return result, calls, conf, linked, built

    def test_none_installs_nothing(self):
        result, calls, _, _, _ = self.exercise(scanners="none", manager=None)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [])
        self.assertIn("Third-party scanners: none", result.stdout)

    def test_an_unknown_scanner_is_refused(self):
        result, _, _, _, _ = self.exercise(scanners="clamav,virustotal")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unknown scanner virustotal", result.stderr)

    def test_scanners_that_answer_are_left_alone(self):
        result, calls, _, _, _ = self.exercise(scanners="malcontent,guarddog", tools=("mal", "guarddog"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [])

    def test_linux_clamav_is_installed_configured_and_started(self):
        result, calls, conf, _, _ = self.exercise(
            scanners="clamav", units=("clamav-freshclam.service", "clamav-daemon.service"))
        self.assertEqual(result.returncode, 0, result.stderr)
        installs = [args for command, args in calls if command == "apt-get" and "install" in args]
        self.assertEqual(installs[0][-2:], ["clamav-daemon", "clamav-freshclam"])
        self.assertIn("#Example", conf)
        self.assertNotIn("\nExample", "\n" + conf)
        self.assertIn("LocalSocketMode 666", conf)
        systemctl = [args for command, args in calls if command == "systemctl" and args[0] != "cat"]
        self.assertEqual(systemctl, [["enable", "--now", "clamav-freshclam.service"],
                                     ["enable", "clamav-daemon.service"], ["restart", "clamav-daemon.service"]],
                         "freshclam first (clamd will not start without databases); clamd restarted "
                         "because its limits were raised")
        self.assertIn("ClamAV ready", result.stdout)

    def test_packaged_scan_limits_are_raised_to_postdocs_body_limit(self):
        # Debian's generated clamd.conf sets its limits outright.
        debian = "LocalSocket {socket}\nStreamMaxLength 25M\nMaxFileSize 25M\nMaxScanSize 100M\n"
        result, _, conf, _, _ = self.exercise(
            scanners="clamav", units=("clamav-freshclam.service", "clamav-daemon.service"), tools=("clamd",),
            clamd_conf=debian)
        self.assertEqual(result.returncode, 0, result.stderr)
        for limit in ("StreamMaxLength", "MaxFileSize", "MaxScanSize"):
            self.assertEqual([line for line in conf.splitlines() if line.startswith(limit)], [limit + " 1024M"])

    def test_limits_already_raised_leave_a_running_clamd_alone(self):
        raised = "LocalSocket {socket}\nStreamMaxLength 1024M\nMaxFileSize 1024M\nMaxScanSize 1024M\n"
        result, calls, _, _, _ = self.exercise(
            scanners="clamav", units=("clamav-freshclam.service", "clamav-daemon.service"), tools=("clamd",),
            clamd_conf=raised)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(["restart", "clamav-daemon.service"], [args for command, args in calls if command == "systemctl"])
        self.assertIn(["systemctl", ["enable", "--now", "clamav-daemon.service"]], calls)

    def test_a_socket_clamd_does_not_answer_on_fails_the_deploy(self):
        # As when systemd holds the socket while clamd is still loading.
        result, _, _, _, _ = self.exercise(
            scanners="clamav", units=("clamav-freshclam.service", "clamav-daemon.service"), silent=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("did not answer", result.stderr)

    def test_a_socket_kept_to_a_group_brings_the_service_user_into_it(self):
        result, calls, _, _, _ = self.exercise(
            scanners="clamav", units=("clamav-freshclam.service", "clamav-daemon.service"))
        self.assertEqual(result.returncode, 0, result.stderr)
        added = [args for command, args in calls if command == "usermod"]
        self.assertEqual(len(added), 1, calls)
        self.assertEqual(added[0][-1], "postdoc")
        self.assertIn("changed=1", result.stdout, "a running service keeps the groups it started with")

    def test_clamd_that_never_opens_its_socket_fails_the_deploy(self):
        result, _, _, _, _ = self.exercise(scanners="clamav", units=("clamav-freshclam.service",))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("none of clamav-daemon.service", result.stderr)

    def test_guarddog_goes_into_a_virtualenv_linked_onto_the_path(self):
        result, calls, _, linked, built = self.exercise(scanners="guarddog")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(linked)
        self.assertEqual(built, ["python3"])
        self.assertIn("--clear", [args for command, args in calls if command == "python3"][-1],
                      "a virtualenv a failed install left behind is rebuilt")
        pip = [args for command, args in calls if command == "python3-pip"]
        self.assertEqual(pip[0][-1], "guarddog>=3")
        self.assertNotIn("--find-links", pip[0])

    def test_a_python_too_new_for_guarddog_brings_in_3_13(self):
        result, calls, _, linked, built = self.exercise(scanners="guarddog", python="3.15")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(linked)
        installs = [args for command, args in calls if command == "apt-get" and "install" in args]
        self.assertEqual(installs[0][-1], "python3.13")
        self.assertEqual(built, ["python3.13"], "pygit2 has no wheels past 3.13")

    def test_macos_guarddog_brings_in_homebrews_python_3_13_when_none_fits(self):
        result, calls, _, linked, built = self.exercise(platform="Darwin", scanners="guarddog", manager="brew")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(linked)
        self.assertIn(["brew", ["install", "python@3.13"]], calls)
        self.assertEqual(built, ["python3.13"])

    def test_freebsd_guarddog_installs_against_the_sandbox_stub(self):
        result, calls, _, linked, _ = self.exercise(platform="FreeBSD", scanners="guarddog", manager="pkg")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(linked)
        self.assertIn(["pkg", ["install", "-y", "yara", "libgit2"]], calls)
        pip = [args for command, args in calls if command == "python3-pip"][0]
        self.assertIn("--find-links", pip)
        self.assertTrue(pip[pip.index("-c") + 1].endswith("overrides.txt"))

    def test_macos_installs_malcontent_from_homebrew(self):
        result, calls, _, _, _ = self.exercise(platform="Darwin", scanners="malcontent", manager="brew")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(["brew", ["install", "malcontent"]], calls)
        self.assertIn("malcontent ready", result.stdout)


if __name__ == "__main__":
    unittest.main()
