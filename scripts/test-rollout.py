#!/usr/bin/env python3
"""Verify rollout routing and failure gates with fake SSH and remote hosts."""
import datetime
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
MOCK = r'''#!/usr/bin/python3
import json, os, pathlib, subprocess, sys
root = pathlib.Path(os.environ['ROLLOUT_TEST_ROOT'])
cmd,args=pathlib.Path(sys.argv[0]).name,sys.argv[1:]
host=os.environ.get('REMOTE_HOST','controller')
if cmd=='hostname': print('controller'); sys.exit(0)
if cmd=='ssh':
    if '-O' in args: sys.exit(0)
    host=args[args.index('--')+1] if '--' in args else args[-2]
    remote=' '.join(args[args.index('--')+2:]) if '--' in args else args[-1]
    # Reachability, and whether the host's login shell has `sh` on its PATH.
    if remote in ('true','exit 0'): sys.exit(0)
    if remote=='sh -c "exit 0"': sys.exit(1 if os.environ.get('ROLLOUT_TEST_NO_SH') else 0)
    if 'probe' in remote: print('{"status":"ok","uptime_secs":1}'); sys.exit(0)
    script=sys.stdin.read()
    (root/(host+'.remote')).write_text(script)
    (root/(host+'.shell')).write_text(remote)
    env={**os.environ,'HOME':str(root/'hosts'/host),'REMOTE_HOST':host}
    # On a Windows host `/usr/bin` is Git for Windows' own coreutils directory.
    # Here it is the controller's, which would shadow the fake commands, so the
    # line is recorded above (and asserted on) but not executed.
    if script.startswith('PATH=/usr/bin:'): script=script.split('\n',1)[1]
    run=subprocess.run(['/bin/sh','-s'],input=script,text=True,env=env)
    sys.exit(run.returncode)
if cmd=='git':
    if args[0]=='rev-parse': print('origin/main')
    sys.exit(0)
if cmd=='make':
    with (root/(host+'.calls')).open('a') as output:
        output.write(json.dumps({'cwd':os.getcwd(),'args':args,'HOPPER':os.environ.get('HOPPER'),'IDLE':os.environ.get('IDLE'),'BIND':os.environ.get('BIND')})+'\n')
    if host=='queue' and os.environ.get('FAIL_QUEUE'): sys.exit(1)
    print('No changes; leaving service running'); sys.exit(0)
if cmd=='curl':
    # The roster fetch writes the dashboard's answer to the file after -o; the
    # hopper health probe just wants a body.
    if '-o' in args:
        pathlib.Path(args[args.index('-o')+1]).write_text(os.environ.get('ROLLOUT_TEST_WORKERS','[]'))
        sys.exit(0)
    print('{"status":"ok"}'); sys.exit(0)
if cmd=='systemctl':
    unit=args[-1]
    expected='postdoc.service' if host=='server' else 'postdoc-worker.service'
    sys.exit(0 if unit==expected else 1)
if cmd=='sc.exe': sys.exit(1)
raise AssertionError((cmd,args))
'''


def seen(ago):
    """A last_seen the way hopper writes it: UTC, nanosecond precision."""
    when = datetime.datetime.now(datetime.timezone.utc) - ago
    return when.strftime("%Y-%m-%dT%H:%M:%S.%f") + "123Z"


class RolloutTests(unittest.TestCase):
    def exercise(self, fail=False, override="", hosts="worker server-idle",
                 phases="hopper workers servers", **extra):
        with tempfile.TemporaryDirectory(prefix="postdoc-rollout-test-") as temporary:
            root = Path(temporary)
            commands = root / "bin"
            commands.mkdir()
            for command in ("hostname", "ssh", "git", "make", "curl", "systemctl", "sc.exe"):
                path = commands / command
                path.write_text(MOCK)
                path.chmod(0o755)
            units = root / "units"
            units.mkdir()
            (units / "postdoc.service").write_text('ExecStart=/bin/postdoc serve --hopper https://old.example\n')
            (units / "postdoc-worker.service").write_text('ExecStart=/bin/postdoc worker --url https://queue.example\n')
            for host in ("server", "worker", "queue"):
                repo = root / "hosts" / host / ("hopper" if host == "queue" else "postdoc")
                repo.mkdir(parents=True)
                shutil.copytree(SCRIPTS, repo / "scripts")
            configs = root / "config"
            configs.mkdir()
            (configs / "serve.json").write_text(json.dumps(dict(HOPPER="none", IDLE="0", BIND="127.0.0.1:50555")))
            (configs / "worker.json").write_text(json.dumps(dict(URL="https://custom-worker.example")))
            script = root / "rollout.sh"
            source = (SCRIPTS / "rollout.sh").read_text()
            source = source.replace("/etc/systemd/system", str(units)).replace("/etc/postdoc", str(configs))
            script.write_text(source)
            # MIN_FREE_GB=0 unless a test sets it: the preflight reads the real
            # disk, and a test must not depend on how full the developer's is.
            env = {**os.environ, "PATH": str(commands) + ":" + os.environ["PATH"],
                   "ROLLOUT_TEST_ROOT": str(root), "HOPPER_NODE": "queue", "HOSTS": hosts,
                   "PHASES": phases, "URL": override, "FAIL_QUEUE": "1" if fail else "",
                   "MIN_FREE_GB": "0", **extra}
            result = subprocess.run(["/bin/sh", str(script)], env=env, text=True, capture_output=True, timeout=30)
            calls = {host: [json.loads(line) for line in (root / (host + ".calls")).read_text().splitlines()]
                     for host in ("queue", "worker", "server") if (root / (host + ".calls")).exists()}
            self.files = {path.name: path.read_text() for path in root.glob("*.shell")}
            self.files.update({path.name: path.read_text() for path in root.glob("*.remote")})
            return result, calls

    def test_hopper_first_and_saved_server_optout_survives(self):
        result, calls = self.exercise()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(calls["queue"][0]["cwd"].endswith("/hopper"))
        self.assertEqual(calls["queue"][0]["args"], ["deploy"])
        self.assertEqual(calls["server"][0]["HOPPER"], "none")
        self.assertEqual(calls["server"][0]["IDLE"], "0")
        self.assertEqual(calls["server"][0]["BIND"], "127.0.0.1:50555")
        self.assertIn("URL=https://custom-worker.example", calls["worker"][0]["args"])
        # The tree is already reset to upstream; a pull on top could only fail.
        self.assertEqual(calls["server"][0]["args"], ["deploy-server", "PULL=0"])
        self.assertIn("PULL=0", calls["worker"][0]["args"])

    def test_hopper_failure_blocks_every_analysis_host(self):
        result, calls = self.exercise(fail=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(set(calls), {"queue"})

    def test_explicit_override_is_shell_data(self):
        address = "https://queue.example/o'neil"
        result, calls = self.exercise(override=address)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls["server"][0]["HOPPER"], address)
        self.assertIn("URL=" + address, calls["worker"][0]["args"])

    def test_live_roster_is_the_dashboards_worker_list(self):
        day = datetime.timedelta(days=1)
        fleet = [
            {"name": "worker:10.0.0.1", "version": "0.9.5", "last_seen": seen(datetime.timedelta(seconds=5))},
            {"name": "server-idle", "version": "0.9.5", "last_seen": seen(datetime.timedelta(seconds=5))},
            # hopper's own built-in worker: redeployed with hopper, not a host
            {"name": "local", "version": "0.9.5", "last_seen": seen(datetime.timedelta(seconds=5))},
            # aged out of the window
            {"name": "gone:10.0.0.9", "version": "0.9.0", "last_seen": seen(30 * day)},
            # a host seen as a worker and, more recently, as a server's idle worker
            {"name": "both:10.0.0.2", "version": "0.9.0", "last_seen": seen(2 * day)},
            {"name": "both-idle", "version": "0.9.5", "last_seen": seen(datetime.timedelta(hours=1))},
        ]
        result, calls = self.exercise(hosts="", phases="workers servers", DRY_RUN="1",
                                      ROLLOUT_TEST_WORKERS=json.dumps(fleet))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("workers: worker\n", result.stdout)
        self.assertIn("servers: both server\n", result.stdout)
        self.assertNotIn("local", result.stdout)
        self.assertNotIn("gone", result.stdout)
        self.assertEqual(calls, {}, "a dry run must deploy nothing")

    def test_a_hopper_without_the_route_is_refused_with_a_remedy(self):
        # Hopper answers every unknown path with the dashboard page.
        result, calls = self.exercise(hosts="", phases="workers servers", DRY_RUN="1",
                                      ROLLOUT_TEST_WORKERS="<!DOCTYPE html><title>hopper</title>")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a worker list", result.stderr)
        self.assertIn("PHASES=hopper", result.stderr)
        self.assertNotIn("Traceback", result.stderr)
        self.assertEqual(calls, {})

    def test_empty_fleet_is_an_error_not_a_quiet_success(self):
        result, calls = self.exercise(hosts="", phases="workers servers", DRY_RUN="1",
                                      ROLLOUT_TEST_WORKERS="[]")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no workers", result.stderr)

    def test_a_host_with_sh_on_its_path_is_driven_with_plain_sh(self):
        result, _ = self.exercise(hosts="worker", phases="workers")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.files["worker.shell"], "sh -s")
        self.assertFalse(self.files["worker.remote"].startswith("PATH="))

    def test_a_host_without_sh_on_its_path_is_driven_with_git_for_windows_sh(self):
        # Windows hosts: the ssh login shell is cmd.exe, which has neither `sh`
        # nor Git's coreutils on its PATH.
        result, _ = self.exercise(hosts="worker", phases="workers", ROLLOUT_TEST_NO_SH="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.files["worker.shell"], r"C:\Progra~1\Git\usr\bin\sh.exe -s")
        self.assertTrue(self.files["worker.remote"].startswith("PATH=/usr/bin:$PATH; export PATH\n"))

    def test_too_little_disk_stops_the_host_before_any_deploy(self):
        result, calls = self.exercise(hosts="worker", phases="workers", MIN_FREE_GB="999999999")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not enough free disk for a build", result.stdout + result.stderr)
        self.assertEqual(calls, {}, "the build must not start on a full disk")

    def test_min_free_gb_is_digits_only_because_a_remote_shell_does_arithmetic_on_it(self):
        result, calls = self.exercise(hosts="worker", phases="workers", MIN_FREE_GB="0;echo pwned")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("MIN_FREE_GB must be", result.stderr)
        self.assertEqual(calls, {})


if __name__ == "__main__":
    unittest.main()
