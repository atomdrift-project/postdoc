#!/usr/bin/env python3
"""Verify rollout routing and failure gates with fake SSH and remote hosts."""
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
    if args[-1]=='true': sys.exit(0)
    if 'probe' in args[-1]: print('{"status":"ok","uptime_secs":1}'); sys.exit(0)
    script=sys.stdin.read()
    (root/(host+'.remote')).write_text(script)
    env={**os.environ,'HOME':str(root/'hosts'/host),'REMOTE_HOST':host}
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
if cmd=='curl': print('{"status":"ok"}'); sys.exit(0)
if cmd=='systemctl':
    unit=args[-1]
    expected='postdoc.service' if host=='server' else 'postdoc-worker.service'
    sys.exit(0 if unit==expected else 1)
if cmd=='sc.exe': sys.exit(1)
raise AssertionError((cmd,args))
'''


class RolloutTests(unittest.TestCase):
    def exercise(self, fail=False, override=""):
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
            env = {**os.environ, "PATH": str(commands) + ":" + os.environ["PATH"],
                   "ROLLOUT_TEST_ROOT": str(root), "HOPPER_NODE": "queue", "HOSTS": "worker server-idle",
                   "PHASES": "hopper workers servers", "URL": override, "FAIL_QUEUE": "1" if fail else ""}
            result = subprocess.run(["/bin/sh", str(script)], env=env, text=True, capture_output=True, timeout=30)
            calls = {host: [json.loads(line) for line in (root / (host + ".calls")).read_text().splitlines()]
                     for host in ("queue", "worker", "server") if (root / (host + ".calls")).exists()}
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


if __name__ == "__main__":
    unittest.main()
