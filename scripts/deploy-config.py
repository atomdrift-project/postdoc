#!/usr/bin/env python3
"""Deployment settings, argument serialization and readiness checks."""
import functools
import json
import os
from pathlib import Path
import re
import shlex
import sys
import time
import urllib.error
import urllib.request

KEYS = ("BIND", "ALLOW_CIDR", "WORKERS", "DATA_DIR", "MAX_RSS_GB", "IDLE",
        "LLM", "LLM_MODEL", "MEMORY_MAX", "MEMORY_LOW", "IDLE_MEMORY_MAX", "NICE", "ENRICHED",
        "SCANNERS")


def arguments(mode, url, home):
    for key in KEYS:
        if "\n" in os.environ.get(key, "") or "\r" in os.environ.get(key, ""):
            raise ValueError(key + " must fit on one line")
    for key in ("WORKERS", "IDLE"):
        if os.environ.get(key) and (not os.environ[key].isdigit() or (key == "WORKERS" and int(os.environ[key]) == 0)):
            raise ValueError(key + " must be a valid slot count")
    if os.environ.get("MAX_RSS_GB") and int(os.environ["MAX_RSS_GB"]) < -1:
        raise ValueError("MAX_RSS_GB must be -1 or nonnegative")
    if os.environ.get("ENRICHED", "0") not in ("0", "1"):
        raise ValueError("ENRICHED must be 0 or 1")
    args = [mode]
    # The isotope13 zero-day rule feed; the token is installed beside the
    # service. Named explicitly so a refused token stops startup rather than
    # quietly falling back to another feed.
    if os.environ.get("ENRICHED") == "1":
        args += ["--feed", "zero-day"]
    if mode == "worker":
        args += ["--url", url, "--nice", os.environ.get("WORKER_NICE", "0")]
    else:
        args += ["--token-file", home + "/.tok/scan"]
        if url:
            args += ["--hopper", url]
        for key, flag in (("BIND", "--bind"), ("ALLOW_CIDR", "--allow-cidr")):
            if os.environ.get(key):
                args += [flag, os.environ[key]]
        if os.environ.get("IDLE"):
            args += ["--idle-worker-slots", os.environ["IDLE"]]
    for key, flag in (("WORKERS", "--workers"), ("MAX_RSS_GB", "--max-rss-gb")):
        if os.environ.get(key):
            args += [flag, os.environ[key]]
    if mode == "worker" and os.environ.get("DATA_DIR"):
        args += ["--data-dir", os.environ["DATA_DIR"]]
    # Absent means postdoc's default: every scanner, each checked at startup.
    if os.environ.get("SCANNERS"):
        args += ["--scanners", os.environ["SCANNERS"]]
    return args


@functools.cache
def memory_total():
    return int(re.search(r"MemTotal:\s+(\d+)", Path("/proc/meminfo").read_text())[1]) * 1024


def memory_bytes(value):
    if value in ("max", "infinity"):
        return memory_total()
    if value.endswith("%"):
        return int(memory_total() * float(value[:-1]) / 100)
    match = re.fullmatch(r"(\d+(?:\.\d+)?)([KMGT]?)B?", value, re.I)
    if not match:
        raise ValueError("invalid memory size: " + value)
    return int(float(match[1]) * 1024 ** ("KMGT".index(match[2].upper()) + 1 if match[2] else 0))


def local_base(bind):
    host, port = bind.rsplit(":", 1)
    host = {"0.0.0.0": "127.0.0.1", "[::]": "[::1]", "::": "[::1]"}.get(host, host)
    return "http://" + host + ":" + port


def read_token(token_file):
    return next(line.strip() for line in Path(token_file).read_text().splitlines() if line.strip())


def health_word(base):
    """/_/health's one word, whatever the status: 503 still names the reason."""
    try:
        with urllib.request.urlopen(base + "/_/health", timeout=5) as response:
            return response.read().decode().strip()
    except urllib.error.HTTPError as error:
        return error.read().decode().strip()


def stats(base, token):
    request = urllib.request.Request(base + "/_/stats", headers={"Authorization": "Bearer " + token})
    with urllib.request.urlopen(request, timeout=5) as response:
        return json.load(response)


def health(token_file, bind, timeout, expect_idle="0"):
    base = local_base(bind)
    token = read_token(token_file)
    deadline = time.monotonic() + int(timeout)
    last = "no response"
    while time.monotonic() < deadline:
        try:
            word = health_word(base)
            if word == "ok":
                # Readiness is public; this proves the installed token works
                # too, and reads whether the idle worker came up.
                if not stats(base, token)["idle_worker"]["running"] and expect_idle == "1":
                    last = "idle worker not running"
                    time.sleep(1)
                    continue
                print("==> API ready")
                return
            last = word or "empty response"
            if word == "failed":
                break
        except (OSError, ValueError, KeyError, urllib.error.HTTPError) as error:
            last = str(error)
        time.sleep(1)
    raise SystemExit("error: API did not become ready: " + last)


def exports(settings):
    for key in (*KEYS, "HOPPER", "URL"):
        if key in settings:
            print("export " + key + "=" + shlex.quote(str(settings[key])))


def main():
    command, *args = sys.argv[1:]
    if command == "args":
        # Double quotes work for both ExecStart and rc.d's command_args value.
        print(" ".join(json.dumps(word, ensure_ascii=False) for word in arguments(*args)))
    elif command == "args-rc":
        mode, url, home, binary = args
        text = shlex.join([binary, *arguments(mode, url, home)])
        print(text.replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\$").replace("`", "\\`"))
    elif command == "rc-model":
        text = shlex.quote("SCAN_LLM_MODEL=" + os.environ["LLM_MODEL"]) if os.environ.get("LLM_MODEL") else ""
        print(text.replace("\\", "\\\\").replace('"', '\\"').replace("$", "\\$").replace("`", "\\`"))
    elif command == "shell-value":
        print(shlex.quote(args[0]))
    elif command == "unit-env":
        home, service_path, idle = args
        for name, value in (("HOME", home), ("PATH", service_path), ("SCAN_LLM", os.environ.get("LLM", "")),
                            ("SCAN_LLM_MODEL", os.environ.get("LLM_MODEL", "")), ("POSTDOC_IDLE_MEMORY_MAX", idle),
                            # Panics keep their backtrace; ordinary errors stop
                            # capturing one each (anyhow/std capture when only
                            # RUST_BACKTRACE is set), a cost seen on hot paths.
                            ("RUST_BACKTRACE", "1"), ("RUST_LIB_BACKTRACE", "0")):
            if name == "SCAN_LLM_MODEL" and not value:
                continue
            print("Environment=" + json.dumps(name + "=" + value.replace("%", "%%"), ensure_ascii=False))
    elif command == "save":
        mode, url = args
        settings = {key: os.environ[key] for key in KEYS if key in os.environ}
        settings["HOPPER" if mode == "serve" else "URL"] = url or "none"
        print(json.dumps(settings, sort_keys=True, indent=2))
    elif command == "export":
        path = Path(args[0])
        if path.exists():
            exports(json.loads(path.read_text()))
        else:
            # Upgrade from pre-manifest units. Parse literal flags without evaluating shell code.
            text = Path(args[1]).read_text()
            flags = {"--hopper": "HOPPER", "--url": "URL", "--bind": "BIND",
                     "--workers": "WORKERS", "--data-dir": "DATA_DIR", "--max-rss-gb": "MAX_RSS_GB",
                     "--idle-worker-slots": "IDLE", "--allow-cidr": "ALLOW_CIDR"}
            settings = {}
            for flag, key in flags.items():
                match = re.search(re.escape(flag) + r"[= ]+(\"[^\"]*\"|'[^']*'|[^\s\";]+)", text)
                if match:
                    settings[key] = shlex.split(match[1])[0]
            if "worker" not in Path(args[1]).name:
                settings.setdefault("HOPPER", "none")
            exports(settings)
    elif command == "memory":
        # Host memory is read only when a default or a percentage needs it.
        if os.environ.get("MEMORY_MAX"):
            maximum = memory_bytes(os.environ["MEMORY_MAX"])
        else:
            maximum = memory_total() * 80 // 100
            if not args or args[0] == "serve":
                maximum = min(maximum, 128 * 1024 ** 3)
        if maximum <= 0:
            raise ValueError("MEMORY_MAX must be positive")
        low = memory_bytes(os.environ.get("MEMORY_LOW") or str(maximum // 2 if not args or args[0] == "serve" else 0))
        idle = memory_bytes(os.environ.get("IDLE_MEMORY_MAX") or str(maximum // 2))
        print(f"MEMORY_MAX={maximum}\nMEMORY_LOW={min(low, maximum)}\nIDLE_MEMORY_MAX={min(idle, maximum // 2)}")
    elif command == "health":
        health(*args)
    elif command == "probe":
        candidates = [Path("/etc/postdoc/serve.json"), Path("/usr/local/etc/postdoc/serve.json")]
        if os.environ.get("ProgramData"):
            candidates.insert(0, Path(os.environ["ProgramData"]) / "Atomdrift/postdoc/deploy-serve.json")
        settings = next((json.loads(path.read_text()) for path in candidates if path.exists()), {})
        bind = (args[0] if args and args[0] else settings.get("BIND")) or "127.0.0.1:49999"
        # The rollout gate's two facts: readiness, which is public, and uptime,
        # which proves the new process is the one answering and is behind the
        # token. The token is the one this account deploys from.
        base = local_base(bind)
        token = read_token(os.environ.get("SCAN_TOKEN_FILE") or Path.home() / ".tok" / "scan")
        print(json.dumps({"status": health_word(base), "uptime_secs": stats(base, token)["uptime_secs"]},
                         separators=(",", ":")))
    else:
        raise SystemExit("unknown deploy-config command")


if __name__ == "__main__":
    main()
