#!/usr/bin/env python3
"""Run the real Unix deployment scripts against a disposable mock host."""
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
MOCK = r'''#!/usr/bin/python3
import filecmp, json, os, pathlib, shutil, subprocess, sys
root = pathlib.Path(os.environ['MOCK_ROOT'])
state_path = root / 'state.json'
s = json.loads(state_path.read_text())
cmd, args = pathlib.Path(sys.argv[0]).name, sys.argv[1:]
if cmd in ('doas', 'sudo'):
    cmd, *args = args
if cmd == 'env':
    while args and '=' in args[0]: args.pop(0)
    cmd, *args = args
s['calls'].append([cmd, args])
def done(code=0):
    state_path.write_text(json.dumps(s)); sys.exit(code)
if cmd == 'id': print('1000'); done()
if cmd == 'uname': print(os.environ['MOCK_PLATFORM']); done()
if cmd == 'getent':
    if s['user']: print('postdoc:x:999:999:postdoc:' + s['home'] + ':/usr/sbin/nologin'); done()
    done(2)
if cmd == 'pw':
    if args[0] == 'usershow': done(0 if s['user'] else 1)
    s['user'] = True; done()
if cmd == 'useradd': s['user'] = True; done()
if cmd == 'readlink': print(os.path.realpath(args[-1])); done()
if cmd == 'grep': done(subprocess.run(['/usr/bin/grep', *args]).returncode)
if cmd == 'pacman':
    package=args[-1]
    names={'base-devel':['cc','c++'],'pkgconf':['pkg-config']}.get(package,[package])
    for name in names:
        binary=root/'bin'/name; binary.write_text('#!/bin/sh\nexit 0\n'); binary.chmod(0o755)
    done()
if cmd == 'rustup':
    binary=root/'bin/rustc'; binary.write_text("#!/bin/sh\necho 'rustc 1.99.1 (updated)'\n"); binary.chmod(0o755)
    done()
if cmd == 'cmp': done(0 if pathlib.Path(args[-1]).exists() and filecmp.cmp(args[-2], args[-1], shallow=False) else 1)
if cmd == 'install':
    paths=[]; mode=0o755; i=0
    while i<len(args):
        if args[i] in ('-o','-g'): i+=2; continue
        if args[i] == '-m': mode=int(args[i+1],8); i+=2; continue
        if args[i].startswith('-m'): mode=int(args[i][2:],8)
        elif not args[i].startswith('-'): paths.append(args[i])
        i+=1
    if '-d' in args:
        for p in paths: pathlib.Path(p).mkdir(parents=True,exist_ok=True); pathlib.Path(p).chmod(mode)
    else:
        shutil.copyfile(paths[0],paths[1]); pathlib.Path(paths[1]).chmod(mode)
    done()
if cmd == 'mktemp':
    import tempfile
    fd,name=tempfile.mkstemp(dir=pathlib.Path(args[0]).parent); os.close(fd); print(name); done()
if cmd == 'mv': os.replace(args[-2],args[-1]); done()
if cmd == 'rm':
    for p in args:
        if not p.startswith('-'): pathlib.Path(p).unlink(missing_ok=True)
    done()
if cmd == 'python3':
    assert 'health' in args
    done(1 if os.environ.get('MOCK_HEALTH_FAIL') else 0)
if cmd == 'systemctl':
    action=args[0]
    if action == '--version': print('systemd 256'); done()
    if action == 'show': print('not-found'); done()
    if action in ('daemon-reload','enable','disable'): done()
    if action == 'is-enabled': print('disabled'); done(1)
    if action in ('restart','start'):
        if action == 'restart' or not s['running']: s['starts']+=1
        s['running']=True; done()
    if action == 'is-active': done(0 if args[-1].startswith('postdoc') and s['running'] else 3)
    if action == 'stop': done()
    raise AssertionError(args)
if cmd == 'sysrc':
    if args[0] == '-n':
        if args[1] in s['rcconf']: print(s['rcconf'][args[1]]); done()
        done(1)
    k,v=args[0].split('=',1); s['rcconf'][k]=v; done()
if cmd == 'service':
    if args[-1] in ('restart','start'): s['starts']+=1; s['running']=True; done()
    if args[-1] in ('onestatus','status'): done(0 if args[0].startswith('postdoc') and s['running'] else 1)
    done()
if cmd in ('chown','touch','journalctl'): done()
raise AssertionError((cmd,args))
'''


class Host:
    def __init__(self, platform="Linux", existing=False):
        self.temp = tempfile.TemporaryDirectory(prefix="postdoc-host-test-")
        # Resolved, as deploy resolves the service home: macOS's /var is a symlink.
        self.root = Path(self.temp.name).resolve()
        self.platform = platform
        self.scripts = self.root / "scripts"
        shutil.copytree(SCRIPTS, self.scripts)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.home = self.root / "service-home"
        self.operator = self.root / "operator"
        (self.operator / ".tok").mkdir(parents=True)
        for name in ("hopper", "scan", "llm", "openrouter"):
            (self.operator / ".tok" / name).write_text(name + " token\n")
        self.config = self.root / "config"
        self.units = self.root / "units"
        self.units.mkdir()
        self.build = self.root / "postdoc-build"
        self.build.write_text("#!/bin/sh\nexit 0\n")
        self.build.chmod(0o755)
        state = dict(calls=[], user=existing, home=str(self.home), running=False, starts=0, rcconf={})
        (self.root / "state.json").write_text(json.dumps(state))
        for command in ("doas", "id", "uname", "getent", "pw", "useradd", "readlink", "cmp", "install",
                        "systemctl", "sysrc", "service", "mv", "chown", "touch", "journalctl"):
            executable = self.bin / command
            executable.write_text(MOCK)
            executable.chmod(0o755)
        for command in ("dirname", "cut", "grep", "tr", "mktemp", "rm", "sed", "head", "awk", "cat", "sleep", "python3"):
            (self.bin / command).symlink_to(shutil.which(command))
        for command in ("7zz", "rizin", "upx", "innoextract"):
            executable = self.bin / command
            executable.write_text("#!/bin/sh\nexit 0\n")
            executable.chmod(0o755)
        # Every filesystem mutation and command stays inside this fixture.
        for script in ("deploy.sh", "deploy-freebsd.sh"):
            path = self.scripts / script
            source = path.read_text()
            for old, new in (("/var/lib/atomdrift/postdoc", self.home), ("/var/db/atomdrift/postdoc", self.home),
                             ("/usr/local/etc/postdoc", self.config), ("/etc/postdoc", self.config),
                             ("/etc/systemd/system", self.units), ("/usr/local/etc/rc.d", self.units)):
                source = source.replace(old, str(new))
            source = source.replace('PATH="/usr/local/sbin:/usr/sbin:/sbin:$PATH"', 'PATH="$PATH"')
            path.write_text(source)
        helper = self.scripts / "deploy-deps.sh"
        helper.write_text(helper.read_text().replace("SERVICE_PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "SERVICE_PATH=" + str(self.bin)))
        self.env = {k: v for k, v in os.environ.items() if k not in ("HOPPER_TOKEN_FILE", "SCAN_TOKEN_FILE", "LLM_TOKEN_FILE", "TOKEN_FILE", "SCAN_LLM_KEY", "OPENROUTER_TOKEN_FILE", "ISOTOPE13_TOKEN_FILE", "ISOTOPE13_TOKEN", "ENRICHED", "WORKERS", "IDLE", "BIND", "ALLOW_CIDR", "DATA_DIR", "MAX_RSS_GB")}
        self.env.update(PATH=str(self.bin), HOME=str(self.operator), MOCK_ROOT=str(self.root),
                        MOCK_PLATFORM=platform, LLM="https://llm.example/v1", LLM_MODEL="", MEMORY_MAX="1G", HEALTH_WAIT="1")

    def run(self, mode="serve", url="https://hopper.example", **env):
        script = "deploy.sh" if self.platform == "Linux" else "deploy-freebsd.sh"
        return subprocess.run(["/bin/sh", str(self.scripts / script), mode, url, str(self.build)],
                              env={**self.env, **env}, capture_output=True, text=True)

    def state(self):
        return json.loads((self.root / "state.json").read_text())

    def close(self):
        self.temp.cleanup()


class DeployTests(unittest.TestCase):
    def host(self, **kwargs):
        host = Host(**kwargs)
        self.addCleanup(host.close)
        return host

    def test_redeploy_does_not_restart_and_rotation_does(self):
        for platform in ("Linux", "FreeBSD"):
            with self.subTest(platform=platform):
                host = self.host(platform=platform)
                first = host.run()
                self.assertEqual(first.returncode, 0, first.stderr)
                self.assertNotIn("ENRICHED", json.loads((host.config / "serve.json").read_text()))
                second = host.run()
                self.assertEqual(second.returncode, 0, second.stderr)
                self.assertEqual(host.state()["starts"], 1)
                (host.operator / ".tok/scan").write_text("rotated API token\n")
                self.assertEqual(host.run().returncode, 0)
                self.assertEqual(host.state()["starts"], 2)

    def test_blank_tokens_fail_before_any_service_changes(self):
        for name in ("scan", "hopper"):
            host = self.host()
            (host.operator / ".tok" / name).write_text(" \t\n")
            result = host.run()
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(any(command in ("install", "useradd", "service", "systemctl") for command, _ in host.state()["calls"]))

    def test_installed_credentials_survive_missing_sources(self):
        host = self.host()
        self.assertEqual(host.run().returncode, 0)
        for name in ("scan", "hopper", "llm"):
            (host.operator / ".tok" / name).unlink()
        result = host.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(host.state()["starts"], 1)

    def test_explicit_settings_reach_unit_and_manifest(self):
        host = self.host()
        result = host.run(WORKERS="8", IDLE="0", BIND="0.0.0.0:50123", ALLOW_CIDR="10.0.0.0/8", MAX_RSS_GB="-1")
        self.assertEqual(result.returncode, 0, result.stderr)
        unit = (host.units / "postdoc.service").read_text()
        args = shlex.split(next(line.split("=", 1)[1] for line in unit.splitlines() if line.startswith("ExecStart=")))
        self.assertEqual(args[args.index("--workers") + 1], "8")
        self.assertEqual(args[args.index("--idle-worker-slots") + 1], "0")
        saved = json.loads((host.config / "serve.json").read_text())
        self.assertEqual(saved["BIND"], "0.0.0.0:50123")
        self.assertNotIn("SCAN_LLM_KEY", saved)
        self.assertIn("WorkingDirectory=" + str(host.home) + "\n", unit)

    def test_worker_paths_with_spaces_and_freebsd_llm_clear(self):
        host = self.host(platform="FreeBSD", existing=True)
        result = host.run("worker", DATA_DIR="/samples/data dir", WORKERS="2", LLM="", LLM_MODEL="a model with spaces")
        self.assertEqual(result.returncode, 0, result.stderr)
        script = (host.units / "postdoc_worker").read_text()
        # Execute the generated rc.d shell with a stub rc.subr to inspect argv.
        stub = host.root / "rc.subr"
        stub.write_text('load_rc_config() { :; }\nrun_rc_command() { printf "%s" "$command_args"; }\n')
        fixture = host.root / "generated.rc"
        fixture.write_text(script.replace("/etc/rc.subr", str(stub)))
        parsed = subprocess.run(["/bin/sh", str(fixture), "start"], capture_output=True, text=True)
        self.assertEqual(parsed.returncode, 0, parsed.stderr)
        self.assertIn("/samples/data dir", shlex.split(parsed.stdout))
        self.assertIn("SCAN_LLM_MODEL=a model with spaces", shlex.split(parsed.stdout))
        self.assertEqual(host.state()["rcconf"]["postdoc_worker_llm"], "")

    def test_enriched_rules_get_the_flag_and_a_private_token(self):
        for platform in ("Linux", "FreeBSD"):
            with self.subTest(platform=platform):
                host = self.host(platform=platform)
                (host.operator / ".tok/isotope13").write_text("i13_test_secret\n")
                result = host.run(ENRICHED="1")
                self.assertEqual(result.returncode, 0, result.stderr)
                saved = json.loads((host.config / "serve.json").read_text())
                self.assertEqual(saved["ENRICHED"], "1")
                if platform == "Linux":
                    unit = (host.units / "postdoc.service").read_text()
                    self.assertIn('"--enriched"', unit)
                else:
                    self.assertIn("--enriched", (host.units / "postdoc").read_text())
                token = host.home / ".config/atomdrift/isotope13-api.json"
                self.assertEqual(json.loads(token.read_text()), {"token": "i13_test_secret"})
                self.assertEqual(token.stat().st_mode & 0o777, 0o600)
                self.assertNotIn("i13_test_secret", json.dumps(host.state()))
                # rollout.sh re-exports the saved manifest, ENRICHED included; an
                # unchanged redeploy then restarts nothing.
                again = host.run(ENRICHED=saved["ENRICHED"])
                self.assertEqual(again.returncode, 0, again.stderr)
                self.assertEqual(host.state()["starts"], 1)

    def test_enriched_rules_without_a_token_fail_before_any_service_changes(self):
        host = self.host()
        result = host.run(ENRICHED="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(command in ("install", "useradd", "service", "systemctl") for command, _ in host.state()["calls"]))
        self.assertNotEqual(host.run(ENRICHED="yes").returncode, 0)

    def test_enriched_token_is_automatically_provisioned_and_reused(self):
        for platform in ("Linux", "FreeBSD"):
            for mode in ("serve", "worker"):
                with self.subTest(platform=platform, mode=mode):
                    host = self.host(platform=platform)
                    source = host.operator / ".tok/isotope13"
                    source.write_text("i13_auto_secret\n")
                    first = host.run(mode=mode)
                    self.assertEqual(first.returncode, 0, first.stderr)
                    saved = json.loads((host.config / (mode + ".json")).read_text())
                    self.assertEqual(saved["ENRICHED"], "1")
                    unit = host.units / ({("Linux", "serve"): "postdoc.service",
                                          ("Linux", "worker"): "postdoc-worker.service",
                                          ("FreeBSD", "serve"): "postdoc",
                                          ("FreeBSD", "worker"): "postdoc_worker"}[(platform, mode)])
                    self.assertIn("--enriched", unit.read_text())
                    installed = host.home / ".config/atomdrift/isotope13-api.json"
                    self.assertEqual(json.loads(installed.read_text()), {"token": "i13_auto_secret"})
                    self.assertEqual(installed.stat().st_mode & 0o777, 0o600)
                    source.write_text("i13_rotated_secret\n")
                    rotated = host.run(mode=mode)
                    self.assertEqual(rotated.returncode, 0, rotated.stderr)
                    self.assertEqual(host.state()["starts"], 2)
                    self.assertEqual(json.loads(installed.read_text()), {"token": "i13_rotated_secret"})
                    source.unlink()
                    again = host.run(mode=mode)
                    self.assertEqual(again.returncode, 0, again.stderr)
                    self.assertEqual(host.state()["starts"], 2)
                    self.assertNotIn("i13_auto_secret", json.dumps(host.state()))
                    self.assertNotIn("i13_rotated_secret", json.dumps(host.state()))

    def test_enriched_auto_detection_honors_sources_and_explicit_opt_out(self):
        for source in ("environment", "file"):
            with self.subTest(source=source):
                host = self.host()
                token = host.operator / "subscription-token"
                token.write_text("i13_custom_secret\n")
                override = ({"ISOTOPE13_TOKEN": "i13_custom_secret"} if source == "environment"
                            else {"ISOTOPE13_TOKEN_FILE": str(token)})
                result = host.run(**override)
                self.assertEqual(result.returncode, 0, result.stderr)
                installed = host.home / ".config/atomdrift/isotope13-api.json"
                self.assertEqual(json.loads(installed.read_text()), {"token": "i13_custom_secret"})
                self.assertNotIn("i13_custom_secret", json.dumps(host.state()))
        host = self.host()
        (host.operator / ".tok/isotope13").write_text("i13_unused_secret\n")
        result = host.run(ENRICHED="0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((host.home / ".config/atomdrift/isotope13-api.json").exists())
        self.assertNotIn("--enriched", (host.units / "postdoc.service").read_text())

    def test_enriched_auto_detection_rejects_blank_source_before_service_changes(self):
        host = self.host()
        (host.operator / ".tok/isotope13").write_text(" \t\n")
        result = host.run()
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any(command in ("install", "useradd", "service", "systemctl") for command, _ in host.state()["calls"]))

    def test_openrouter_key_is_installed_without_argv_disclosure(self):
        host = self.host()
        result = host.run(LLM="openrouter", SCAN_LLM_KEY="secret-test-key")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((host.home / ".tok/openrouter").read_text(), "secret-test-key\n")
        self.assertNotIn("secret-test-key", json.dumps(host.state()))

    def test_freebsd_endpoint_chain_is_one_literal_argument(self):
        host = self.host(platform="FreeBSD")
        endpoint = "https://llm.example/o'neil/v1, openrouter"
        result = host.run(LLM=endpoint)
        self.assertEqual(result.returncode, 0, result.stderr)
        stub = host.root / "rc.subr"
        stub.write_text('load_rc_config() { :; }\nrun_rc_command() { printf "%s" "$command_args"; }\n')
        fixture = host.root / "generated.rc"
        fixture.write_text((host.units / "postdoc").read_text().replace("/etc/rc.subr", str(stub)))
        parsed = subprocess.run(["/bin/sh", str(fixture), "start"], capture_output=True, text=True)
        self.assertEqual(parsed.returncode, 0, parsed.stderr)
        self.assertIn("SCAN_LLM=" + endpoint, shlex.split(parsed.stdout))

    def test_readiness_failure_fails_deploy(self):
        host = self.host()
        result = host.run(MOCK_HEALTH_FAIL="1")
        self.assertNotEqual(result.returncode, 0)

    def test_bootstrap_installs_missing_build_tools(self):
        host = self.host()
        (host.root / "Cargo.toml").write_text('rust-version = "1.97"\n')
        for name, body in (("cargo", "exit 0"), ("rustc", "echo 'rustc 1.99.1 (test)'")):
            path = host.bin / name
            path.write_text("#!/bin/sh\n" + body + "\n")
            path.chmod(0o755)
        (host.bin / "pacman").write_text(MOCK)
        (host.bin / "pacman").chmod(0o755)
        script = host.scripts / "deploy-bootstrap.sh"
        script.write_text(script.read_text().replace('PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$PATH"', 'PATH="$PATH"'))
        result = subprocess.run(["/bin/sh", "-c", '. ./scripts/deploy-bootstrap.sh; command -v cc; command -v pkg-config'],
                                cwd=host.root, env=host.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((host.bin / "cc").exists())
        self.assertTrue((host.bin / "c++").exists())
        self.assertTrue((host.bin / "pkg-config").exists())

    def test_existing_home_with_spaces_is_used(self):
        host = self.host(existing=True)
        custom = host.root / "custom home"
        state = host.state()
        state["home"] = str(custom)
        (host.root / "state.json").write_text(json.dumps(state))
        result = host.run()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((custom / "bin/postdoc").exists())
        self.assertIn("WorkingDirectory=" + str(custom) + "\n", (host.units / "postdoc.service").read_text())

    def test_bootstrap_updates_an_old_toolchain_for_this_build(self):
        host = self.host()
        (host.root / "Cargo.toml").write_text('rust-version = "1.97"\n')
        for name in ("cargo", "cc", "c++", "pkg-config", "git", "curl"):
            path = host.bin / name
            path.write_text("#!/bin/sh\nexit 0\n")
            path.chmod(0o755)
        (host.bin / "rustc").write_text("#!/bin/sh\necho 'rustc 1.20.0 (old)'\n")
        (host.bin / "rustc").chmod(0o755)
        (host.bin / "rustup").write_text(MOCK)
        (host.bin / "rustup").chmod(0o755)
        script = host.scripts / "deploy-bootstrap.sh"
        script.write_text(script.read_text().replace('PATH="$HOME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:$PATH"', 'PATH="$PATH"'))
        result = subprocess.run(["/bin/sh", "-c", '. ./scripts/deploy-bootstrap.sh; printf "%s" "$RUSTUP_TOOLCHAIN"'],
                                cwd=host.root, env=host.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(result.stdout.endswith("stable"))
        self.assertIn(["rustup", ["toolchain", "install", "stable", "--profile", "minimal"]], host.state()["calls"])


if __name__ == "__main__":
    unittest.main()
