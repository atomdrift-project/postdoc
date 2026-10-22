#!/usr/bin/env python3
"""Replay foreground requests with the companion idle worker disabled.

Run outside a cgroup being measured. A dedicated transient systemd unit holds
only postdoc and its helpers; streaming HTTP clients stay outside it. Run the
worker benchmark separately. Requires the worker benchmark's doas/systemd setup.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import getpass
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time

from bench_host_memory import read_memory_state


def manifest(root):
    rows = []
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        path.name.encode("ascii")
        with path.open("rb") as stream:
            sha = hashlib.file_digest(stream, "sha256").hexdigest()
        rows.append({"path": str(path.relative_to(root)), "bytes": path.stat().st_size,
                     "sha256": sha})
    if not rows or len({r["sha256"] for r in rows}) != len(rows):
        raise ValueError("each dataset must contain nonempty, unique content hashes")
    return rows


def unit_state(unit):
    result = subprocess.run(["systemctl", "show", unit, "-p", "SubState", "-p",
                             "ExecMainStatus", "-p", "ControlGroup", "-p", "MainPID"],
                            capture_output=True, text=True, check=True)
    return dict(line.split("=", 1) for line in result.stdout.splitlines())


def memory(group):
    stats = dict(line.split() for line in (group / "memory.stat").read_text().splitlines())
    events = dict(line.split() for line in (group / "memory.events").read_text().splitlines())
    control = dict(line.split() for line in (group / "cgroup.events").read_text().splitlines())
    return {"current_bytes": int((group / "memory.current").read_text()),
            "peak_bytes": int((group / "memory.peak").read_text()),
            "limit": (group / "memory.max").read_text().strip(),
            "frozen": int(control.get("frozen", 0)),
            **{k + "_bytes": int(stats.get(k, 0)) for k in ("anon", "shmem", "file", "kernel")},
            "events": {k: int(v) for k, v in events.items()}}


def cpu(group):
    return {key: int(value) for key, value in
            (line.split() for line in (group / "cpu.stat").read_text().splitlines())}


def host_load():
    fields = Path("/proc/stat").read_text().splitlines()[0].split()[1:9]
    return {"cpu_ticks": [int(value) for value in fields],
            "ticks_per_second": os.sysconf("SC_CLK_TCK"),
            **read_memory_state()}


def upload(port, root, row, output, timeout):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    started = time.monotonic()
    try:
        with (root / row["path"]).open("rb") as body:
            connection.request("POST", "/v1/analyze?full=1&force=1", body=body,
                               headers={"Content-Type": "application/octet-stream",
                                        "Content-Length": str(row["bytes"]),
                                        "X-Filename": Path(row["path"]).name})
            response = connection.getresponse()
            with (output / (row["sha256"] + ".json")).open("wb") as target:
                while chunk := response.read(65536):
                    target.write(chunk)
            if response.status != 200:
                raise RuntimeError(f"HTTP {response.status}: {row['path']}")
        finished = time.monotonic()
        return {"sha256": row["sha256"], "elapsed_s": finished - started,
                "finished_at": finished}
    finally:
        connection.close()


def get_json(port, route):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request("GET", route)
        response = connection.getresponse()
        if response.status != 200:
            raise RuntimeError(f"{route}: HTTP {response.status}")
        return json.loads(response.read())
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dataset", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--binary", type=Path, default=Path("target/release/postdoc"))
    parser.add_argument("--models", type=Path, required=True)
    parser.add_argument("--traits", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--workers", type=int, default=192)
    parser.add_argument("--clients", type=int, default=20)
    parser.add_argument("--timeout", type=float, default=7200)
    parser.add_argument("--memory-gb", type=int, default=64)
    parser.add_argument("--max-rss-gb", type=int, default=54)
    parser.add_argument("--tmp-gb", type=int, default=16)
    parser.add_argument("--gate", type=int)
    args = parser.parse_args()
    if min(args.workers, args.clients, args.memory_gb, args.tmp_gb) < 1:
        parser.error("worker/client counts and memory sizes must be positive")
    if args.timeout <= 0:
        parser.error("timeout must be positive")
    if args.gate is not None and args.gate < 1:
        parser.error("gate must be positive")
    for path in (args.binary,):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"not executable: {path}")
    for path in (args.models, args.traits, args.cache):
        if not path.is_dir():
            parser.error(f"not a directory: {path}")
    foreground = manifest(args.dataset)
    output = args.output.resolve()
    root = args.dataset.resolve()
    if output == root or root in output.parents:
        parser.error("output must be outside the input dataset")
    output.mkdir(parents=True, exist_ok=False)
    responses = output / "foreground-responses"
    responses.mkdir()
    (output / "manifest.json").write_text(json.dumps(foreground, indent=2) + "\n")
    unit = f"postdoc-bench-service-{os.getpid()}-{time.time_ns()}"
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    env = {"SCAN_NO_ANALYSIS_CACHE": "1", "SCAN_NO_UPDATE_CHECK": "1", "SCAN_NO_UPDATE": "1",
           "TMPDIR": "/tmp", "TMP": "/tmp", "TEMP": "/tmp",
           "SCAN_FETCH": "none", "SCAN_FOLLOW": "none", "SCAN_ANALYSIS_TIMEOUT": "0",
           "SCAN_HEARTBEAT_SECS": "5",
           "SCAN_MODELS_DIR": str(args.models.resolve()), "CLEAVE_TRAITS_DIR": str(args.traits.resolve()),
           "XDG_CACHE_HOME": str(args.cache.resolve()),
           "_RJEM_MALLOC_CONF": "retain:true,muzzy_decay_ms:0,prof:false"}
    if args.gate:
        env["SCAN_CLEAVE_CONCURRENCY"] = str(args.gate)
    config = {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()}
    config["host_memory_guard"] = os.environ.get("POSTDOC_BENCH_HOST_GUARD")
    with args.binary.open("rb") as stream:
        config["binary_sha256"] = hashlib.file_digest(stream, "sha256").hexdigest()
    config.update(unit=unit, env=env, idle_worker_enabled=False)
    (output / "config.json").write_text(json.dumps(config, indent=2) + "\n")
    executor = group = None
    launched = False
    started = time.monotonic()
    try:
        command = ["doas", "systemd-run", "--unit=" + unit, "-p", "User=" + getpass.getuser(),
                   "-p", "WorkingDirectory=" + str(Path.cwd()),
                   "-p", f"MemoryMax={args.memory_gb}G", "-p", "MemorySwapMax=0",
                   "-p", "RemainAfterExit=yes", "-p", "LimitCORE=0",
                   "-p", f"TemporaryFileSystem=/tmp:rw,mode=1777,size={args.tmp_gb}G",
                   "-p", "UnsetEnvironment=SCAN_HOPPER SCAN_IDLE_WORKER_SLOTS SCAN_LLM SCAN_INTERPRET SCAN_TOKEN_FILE HOPPER_TOKEN_FILE HOPPER_TOKEN SCAN_HEAP_PROFILE_DIR"]
        command.extend("--setenv=" + k + "=" + v for k, v in env.items())
        command.extend([str(args.binary.resolve()), "serve", "--bind", f"127.0.0.1:{port}",
                        "--workers", str(args.workers), "--max-rss-gb", str(args.max_rss_gb),
                        "--analysis-timeout", "0",
                        "--no-update", "--mode", "slow"])
        launched = True
        subprocess.run(command, check=True)
        service_started = time.monotonic()
        ready_at = None
        futures = None
        foreground_started = None
        foreground_finished = None
        foreground_cpu_start = None
        peak_by_type = {}
        with (output / "memory.jsonl").open("w") as samples:
            while True:
                elapsed = time.monotonic() - service_started
                if elapsed > args.timeout:
                    raise TimeoutError("foreground service benchmark timed out")
                state = unit_state(unit)
                if state["SubState"] in ("dead", "failed", "exited"):
                    raise RuntimeError(f"server exited: {state}")
                if state["SubState"] != "running" or not state.get("ControlGroup"):
                    time.sleep(.1)
                    continue
                group = Path("/sys/fs/cgroup") / state["ControlGroup"].lstrip("/")
                phase = "startup" if futures is None else "foreground"
                sample = {"elapsed_s": elapsed, "phase": phase, "service": memory(group),
                          "cpu": cpu(group), "host": host_load()}
                if (group / "idle").exists():
                    raise RuntimeError("unexpected idle worker cgroup during foreground benchmark")
                for key in ("anon_bytes", "shmem_bytes", "file_bytes", "kernel_bytes"):
                    peak_by_type[key] = max(peak_by_type.get(key, 0), sample["service"][key])
                samples.write(json.dumps(sample) + "\n")
                samples.flush()
                if any(sample["service"]["events"].get(k, 0) for k in ("oom", "oom_kill")):
                    raise RuntimeError("OOM event during foreground service benchmark")
                if ready_at is None:
                    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
                    try:
                        conn.request("GET", "/_/health")
                        if conn.getresponse().status == 200:
                            ready_at = time.monotonic()
                    except OSError:
                        pass
                    finally:
                        conn.close()
                if ready_at is not None and futures is None:
                    stats = get_json(port, "/_/stats")
                    if stats["idle_worker"]["running"]:
                        raise RuntimeError("idle worker must be disabled for foreground benchmarks")
                    (output / "stats-before.json").write_text(json.dumps(stats, indent=2) + "\n")
                    executor = ThreadPoolExecutor(max_workers=args.clients)
                    foreground_cpu_start = cpu(group)
                    foreground_started = time.monotonic()
                    futures = [executor.submit(upload, port, root, row, responses, args.timeout)
                               for row in foreground]
                if futures is not None and foreground_finished is None:
                    for future in futures:
                        if future.done() and future.exception() is not None:
                            future.result()
                    if all(f.done() for f in futures):
                        foreground_rows = [f.result() for f in futures]
                        # Keep the one-second telemetry polling interval out
                        # of the wall-clock result, including small fixtures.
                        foreground_finished = max(row.pop("finished_at") for row in foreground_rows)
                        (output / "foreground-timings.json").write_text(json.dumps(foreground_rows, indent=2) + "\n")
                if foreground_finished is not None:
                    break
                time.sleep(1)
        stats = get_json(port, "/_/stats")
        if stats["idle_worker"]["running"]:
            raise RuntimeError("idle worker started during foreground benchmark")
        (output / "stats-after.json").write_text(json.dumps(stats, indent=2) + "\n")
        (output / "memory-after.json").write_text(json.dumps(get_json(port, "/_/memory"), indent=2) + "\n")
        summary = {"foreground_jobs": len(foreground), "idle_worker_enabled": False,
                   "ready_s": ready_at - service_started,
                   "foreground_wall_s": foreground_finished - foreground_started,
                   "service_wall_s": time.monotonic() - service_started,
                   "cgroup_final": memory(group), "sampled_peak_by_type": peak_by_type,
                   "foreground_cpu_start": foreground_cpu_start,
                   "foreground_cpu_end": cpu(group)}
        with (output / "foreground-fingerprint.json").open("w") as fp:
            subprocess.run([sys.executable, str(Path(__file__).with_name("detect-fingerprint.py")),
                            str(responses), "--content"], stdout=fp, check=True)
        entries = json.loads((output / "foreground-fingerprint.json").read_text())
        if {row["file"] for row in entries} != {row["sha256"] for row in foreground}:
            raise RuntimeError("incomplete or unexpected foreground result hashes")
        if any("error" in row for row in entries):
            raise RuntimeError("analysis errors in foreground results")
        (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(json.dumps(summary, indent=2))
    except BaseException as error:
        failure = {"error": str(error), "wall_s": time.monotonic() - started}
        if group is not None and group.exists():
            failure["cgroup_final"] = memory(group)
        (output / "failure.json").write_text(json.dumps(failure, indent=2) + "\n")
        raise
    finally:
        cleanup_error = None
        if launched:
            with (output / "service.log").open("w") as log:
                subprocess.run(["doas", "journalctl", "-u", unit, "--no-pager"], stdout=log)
            stopped = subprocess.run(["doas", "systemctl", "stop", unit], capture_output=True, text=True)
            if stopped.returncode:
                (output / "cleanup-error.txt").write_text(stopped.stderr)
                print(f"failed to stop {unit}: {stopped.stderr}", file=sys.stderr)
                cleanup_error = RuntimeError(f"failed to stop foreground benchmark unit {unit}")
        if executor:
            executor.shutdown(wait=True, cancel_futures=True)
        if cleanup_error is not None:
            (output / "summary.json").unlink(missing_ok=True)
            (output / "failure.json").write_text(json.dumps({"error": str(cleanup_error)}, indent=2) + "\n")
            raise cleanup_error


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    main()
