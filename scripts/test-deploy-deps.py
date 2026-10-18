#!/usr/bin/env python3
"""Exercise deployment prerequisite installation without changing the host."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HELPER = Path(__file__).with_name("deploy-deps.sh")
MOCK = r'''#!/usr/bin/python3
import json, os, pathlib, sys
root = pathlib.Path(os.environ["TEST_ROOT"])
command, args = pathlib.Path(sys.argv[0]).name, sys.argv[1:]
if command in ("doas", "sudo"):
    command, *args = args
if command == "env":
    command, args = args[1], args[2:]
with (root / "calls").open("a") as stream:
    stream.write(json.dumps([command, args]) + "\n")
if command == "apt-get" and args == ["update"]:
    sys.exit(0)
package = args[-1]
if package in os.environ.get("FAIL_PACKAGES", "").split(","):
    sys.exit(1)
if not os.environ.get("NO_INSTALL"):
    name = {"7zip": "7zz", "7-zip": "7zz", "p7zip": "7z",
            "p7zip-full": "7z", "upx-ucl": "upx"}.get(package, package)
    binary = root / "tools" / name
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
'''


class PrerequisiteTests(unittest.TestCase):
    def exercise(self, manager=None, tools=(), platform="Linux", wrapper="doas",
                 fail=(), no_install=False, private_tools=(), helper=HELPER):
        with tempfile.TemporaryDirectory(prefix="postdoc-deps-test-") as tmp:
            root = Path(tmp)
            for directory in ("commands", "tools", "private"):
                (root / directory).mkdir()
            for name in filter(None, (manager, wrapper)):
                executable = root / "commands" / name
                executable.write_text(MOCK)
                executable.chmod(0o755)
            for directory, names in (("tools", tools), ("private", private_tools)):
                for name in names:
                    executable = root / directory / name
                    executable.write_text("#!/bin/sh\nexit 0\n")
                    executable.chmod(0o755)
            env = {
                **os.environ,
                "PATH": str(root / "commands") + ":" + str(root / "private"),
                "TEST_ROOT": str(root),
                "FAIL_PACKAGES": ",".join(fail),
                "NO_INSTALL": "1" if no_install else "",
            }
            command = '''
set -eu
SUDO="$1"
log() { printf '%s\n' "$*"; }
die() { printf '%s\n' "$*" >&2; exit 1; }
. "$2"
SERVICE_PATH="$3"
ensure_runtime_tools "$4"
'''
            result = subprocess.run(
                ["/bin/sh", "-c", command, "deps-test", wrapper or "", str(helper),
                 str(root / "tools"), platform],
                env=env, capture_output=True, text=True,
            )
            calls = root / "calls"
            operations = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
            installed = sorted(p.name for p in (root / "tools").iterdir())
            return result, operations, installed

    def test_complete_host_needs_no_package_manager(self):
        for sevenzip in ("7z", "7za", "7zz", "7zr"):
            with self.subTest(sevenzip=sevenzip):
                result, calls, _ = self.exercise(tools=("rizin", "upx", "innoextract", sevenzip))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, [])

    def test_linux_package_managers_install_only_missing_tool(self):
        for manager in ("apt-get", "dnf", "yum", "zypper", "pacman", "xbps-install"):
            with self.subTest(manager=manager):
                result, calls, tools = self.exercise(manager=manager, tools=("rizin", "upx", "innoextract"))
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("7zz", tools)
                self.assertEqual([args[-1] for _, args in calls if args != ["update"]], ["7zip"])
                if manager == "pacman":
                    self.assertEqual(calls[0][1], ["-S", "--needed", "--noconfirm", "7zip"])

    def test_freebsd_installs_all_runtime_tools(self):
        result, calls, tools = self.exercise(manager="pkg", platform="FreeBSD")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(tools, ["7zz", "innoextract", "rizin", "upx"])
        self.assertEqual([args[-1] for _, args in calls], ["rizin", "upx", "innoextract", "7-zip"])

    def test_older_apt_package_names(self):
        result, calls, tools = self.exercise(manager="apt-get", tools=("rizin", "innoextract"),
                                             fail=("upx-ucl", "7zip"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("upx", tools)
        self.assertIn("7z", tools)
        self.assertEqual([args[-1] for _, args in calls], ["update", "upx-ucl", "upx", "7zip", "p7zip-full"])

    def test_sudo_and_root_installation(self):
        for wrapper in ("sudo", None):
            with self.subTest(wrapper=wrapper):
                result, _, _ = self.exercise(manager="pkg", platform="FreeBSD", wrapper=wrapper)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_package_failure_blocks_deployment(self):
        result, _, _ = self.exercise(manager="pacman", tools=("rizin", "upx", "innoextract"),
                                    fail=("7zip", "p7zip"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("7zip is still unavailable", result.stderr)

    def test_package_success_without_executable_still_blocks(self):
        result, _, _ = self.exercise(manager="pkg", platform="FreeBSD", no_install=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rizin is still unavailable", result.stderr)

    def test_operator_private_path_does_not_satisfy_service(self):
        result, _, _ = self.exercise(private_tools=("rizin", "upx", "innoextract", "7zz"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing runtime tools", result.stderr)

    def test_freebsd_without_pkg_fails(self):
        result, _, _ = self.exercise(platform="FreeBSD")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pkg is required", result.stderr)


if __name__ == "__main__":
    unittest.main()
