#!/usr/bin/env python3
"""Measure the real download/queue/analysis/post path against a local hopper.

Unlike --data-dir runs, inputs arrive over HTTP so download-buffer retention
is included. Sampling and timing must use separate runs. No production hopper
or LLM credentials are inherited. Fetch=all may contact package registries.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def stop(process):
    if process is None:
        return
    # Include helper processes, even if the parent already exited.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=20)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dataset", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--binary", type=Path, default=Path("target/release/postdoc"))
    parser.add_argument("--hopper", type=Path, default=Path("target/release/postdoc-bench-hopper"))
    parser.add_argument("--workers", type=int, default=16)
    parser.add_argument("--fetch", default="none", choices=["none", "all"])
    parser.add_argument("--timeout", type=float, default=3600)
    parser.add_argument("--max-rss-gb", type=int, default=-1,
                        help="worker admission setting; use the same value for both runs")
    parser.add_argument("--allocator-config", default="",
                        help="jemalloc options for allocator tuning experiments")
    parser.add_argument("--heap", action="store_true")
    args = parser.parse_args()
    if not args.dataset.is_dir() or args.workers < 1:
        parser.error("dataset must be a directory and workers must be positive")
    for path in (args.binary, args.hopper):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"not executable: {path}")
    output = args.output.resolve()
    if output == args.dataset.resolve() or args.dataset.resolve() in output.parents:
        parser.error("output must be outside the input dataset")
    output.mkdir(parents=True, exist_ok=False)
    files = sorted(p for p in args.dataset.rglob("*") if p.is_file() and not p.is_symlink())
    manifest = []
    for path in files:
        with path.open("rb") as stream:
            digest = hashlib.file_digest(stream, "sha256").hexdigest()
        manifest.append({"path": str(path.relative_to(args.dataset)), "bytes": path.stat().st_size, "sha256": digest})
    if not manifest:
        parser.error("dataset has no regular files")
    (output / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    env = os.environ.copy()
    for name in ("SCAN_HOPPER", "SCAN_TOKEN_FILE", "SCAN_LLM", "SCAN_INTERPRET", "SCAN_HEAP_PROFILE_DIR", "HOPPER_TOKEN_FILE", "HOPPER_TOKEN"):
        env.pop(name, None)
    env.update(SCAN_NO_ANALYSIS_CACHE="1", SCAN_NO_UPDATE_CHECK="1", SCAN_NO_UPDATE="1",
               SCAN_FETCH=args.fetch, SCAN_FOLLOW=args.fetch, SCAN_ANALYSIS_TIMEOUT="0",
               SCAN_HEARTBEAT_SECS="5", HOPPER_TOKEN="local-benchmark")
    allocator_prefix = args.allocator_config.rstrip(",")
    if allocator_prefix:
        allocator_prefix += ","
    env["_RJEM_MALLOC_CONF"] = allocator_prefix + "prof:false"
    if args.heap:
        (output / "heap").mkdir()
        env["_RJEM_MALLOC_CONF"] = allocator_prefix + f"prof:true,prof_active:true,lg_prof_sample:21,lg_prof_interval:34,prof_final:true,prof_prefix:{output}/heap/jeprof"
    # Keep benchmark extraction separate from the live service's /tmp. Both
    # A/B runs must use this same filesystem; this measures heap, not tmpfs.
    scratch = output / "scratch"
    scratch.mkdir()
    env.update(TMPDIR=str(scratch), TMP=str(scratch), TEMP=str(scratch))
    (output / "config.json").write_text(json.dumps({
        "binary": str(args.binary.resolve()), "workers": args.workers,
        "max_rss_gb": args.max_rss_gb,
        "allocator_config": args.allocator_config,
        "fetch": args.fetch, "heap_sampling": args.heap, "scratch": str(scratch),
        "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
        "models_dir": env.get("SCAN_MODELS_DIR"), "traits_dir": env.get("CLEAVE_TRAITS_DIR"),
        "cache_dir": env.get("XDG_CACHE_HOME"),
    }, indent=2) + "\n")
    hopper = worker = None
    try:
        with (output / "hopper.log").open("w") as log, (output / "hopper.err").open("w") as err:
            hopper = subprocess.Popen([str(args.hopper.resolve()), "--dataset", str(args.dataset.resolve()),
                "--port", "0", "--order", "mixed", "--summary", str(output / "hopper-summary.json"),
                "--dump", str(output / "completed.txt"), "--dump-bodies", str(output / "responses")],
                stdout=log, stderr=err, env=env, start_new_session=True)
            port = None
            deadline = time.monotonic() + 60
            while time.monotonic() < deadline and hopper.poll() is None:
                for line in (output / "hopper.log").read_text().splitlines():
                    if line.startswith("PORT="):
                        port = int(line[5:])
                if port:
                    break
                time.sleep(.1)
            if not port:
                raise RuntimeError("mock hopper failed to start; see hopper.err")
            command = [str(args.binary.resolve()), "worker", "--url", f"http://127.0.0.1:{port}",
                "--exit-if-empty", "--nice", "0", "--no-update", "--no-validate", "--workers", str(args.workers),
                "--max-rss-gb", str(args.max_rss_gb)]
            started = time.monotonic()
            peak_kib = 0
            peak_mappings = 0
            last_mapping_check = -10.0
            with (output / "worker.log").open("w") as worker_log, (output / "memory.jsonl").open("w") as memory:
                worker = subprocess.Popen(command, stdout=worker_log, stderr=subprocess.STDOUT, env=env, start_new_session=True)
                while worker.poll() is None:
                    elapsed = time.monotonic() - started
                    if elapsed > args.timeout:
                        raise TimeoutError(f"benchmark exceeded {args.timeout}s")
                    try:
                        status = dict(line.split(":", 1) for line in Path(f"/proc/{worker.pid}/status").read_text().splitlines())
                        rss = int(status.get("VmRSS", "0 kB").split()[0])
                        peak_kib = max(peak_kib, int(status.get("VmHWM", "0 kB").split()[0]))
                        sample = {"elapsed_s": elapsed, "rss_kib": rss, "peak_kib": peak_kib}
                        if elapsed - last_mapping_check >= 10:
                            with Path(f"/proc/{worker.pid}/maps").open() as mappings:
                                count = sum(1 for _ in mappings)
                            sample["mappings"] = count
                            peak_mappings = max(peak_mappings, count)
                            last_mapping_check = elapsed
                        memory.write(json.dumps(sample) + "\n")
                        memory.flush()
                    except FileNotFoundError:
                        pass
                    time.sleep(.2)
            if worker.returncode:
                raise RuntimeError(f"worker exited {worker.returncode}; see worker.log")
            summary = json.loads((output / "hopper-summary.json").read_text())
            if summary["done"] != len(manifest):
                raise RuntimeError(f"incomplete run: {summary['done']} of {len(manifest)}")
            with (output / "fingerprint.json").open("w") as fp:
                subprocess.run([sys.executable, str(Path(__file__).with_name("detect-fingerprint.py")), str(output / "responses")], stdout=fp, check=True)
            fingerprint = json.loads((output / "fingerprint.json").read_text())
            summary.update(process_wall_s=time.monotonic() - started, peak_rss_mib=peak_kib / 1024,
                           peak_sampled_mappings=peak_mappings, heap_sampling=args.heap,
                           analysis_errors=sum("error" in entry for entry in fingerprint))
            (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
            print(json.dumps(summary, indent=2))
    finally:
        stop(worker)
        stop(hopper)


if __name__ == "__main__":
    # Turn termination into unwinding so finally cleans up both process groups.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    main()
