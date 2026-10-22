#!/usr/bin/env python3
"""Require complete worker-result parity before accepting a throughput gain."""

import argparse
import json
import math
from pathlib import Path
import statistics
import subprocess
import sys


def read(directory, name):
    return json.loads((directory / name).read_text())


def host_observation(directory):
    path = directory / "memory.jsonl"
    if not path.exists():
        return None
    first = last = None
    minimum = math.inf
    minimum_effective = math.inf
    maximum_arc_credit = 0
    with path.open() as samples:
        for line in samples:
            row = json.loads(line)
            if "host" in row and "cpu" in row.get("cgroup", {}):
                first = first or row
                last = row
                minimum = min(minimum, row["host"]["available_bytes"])
                minimum_effective = min(minimum_effective, row["host"].get(
                    "effective_available_bytes", row["host"]["available_bytes"]))
                maximum_arc_credit = max(maximum_arc_credit, row["host"].get("zfs_arc_credit_bytes", 0))
    if first is None or last["elapsed_s"] <= first["elapsed_s"]:
        return None
    elapsed = last["elapsed_s"] - first["elapsed_s"]
    ticks = [end - start for start, end in zip(first["host"]["cpu_ticks"], last["host"]["cpu_ticks"])]
    # Linux's first eight fields: user, nice, system, idle, iowait, irq,
    # softirq, steal. Exclude idle/iowait; no guest double-counting.
    busy = sum(ticks[:3]) + sum(ticks[5:])
    own_cpu = (last["cgroup"]["cpu"]["usage_usec"] - first["cgroup"]["cpu"]["usage_usec"]) / 1e6
    host_cores = busy / first["host"]["ticks_per_second"] / elapsed
    return {"sample_window_s": elapsed, "host_busy_cores": host_cores,
            "benchmark_busy_cores": own_cpu / elapsed,
            "other_busy_cores_estimate": host_cores - own_cpu / elapsed,
            "minimum_available_gib": minimum / 2**30,
            "minimum_effective_available_gib": minimum_effective / 2**30,
            "maximum_zfs_arc_credit_gib": maximum_arc_credit / 2**30}


def inspect(directory):
    if (directory / "failure.json").exists():
        raise ValueError(f"{directory}: failed benchmark")
    if (directory / "external-profile.json").exists():
        raise ValueError(f"{directory}: externally profiled run cannot establish throughput")
    summary = read(directory, "summary.json")
    config = read(directory, "config.json")
    if not config.get("binary_sha256"):
        raise ValueError(f"{directory}: missing executable hash")
    manifest = read(directory, "manifest.json")
    hashes = [row["sha256"] for row in manifest]
    if not hashes or len(hashes) != len(set(hashes)):
        raise ValueError(f"{directory}: corpus must have unique content hashes")
    if summary["done"] != summary["jobs"] or summary["done"] != len(hashes):
        raise ValueError(f"{directory}: incomplete replay")
    wall = summary["process_wall_s"]
    if not math.isfinite(wall) or wall <= 0:
        raise ValueError(f"{directory}: invalid wall time")
    if "harness_wall_s" not in summary:
        raise ValueError(f"{directory}: old timing format includes result validation")
    if config["heap_sampling"]:
        raise ValueError(f"{directory}: profiled runs cannot establish throughput")
    group = summary.get("cgroup", {}).get("final", {})
    if not group:
        raise ValueError(f"{directory}: missing whole-cgroup accounting")
    if "peak_bytes" not in group:
        raise ValueError(f"{directory}: missing whole-cgroup peak memory")
    if any(group["events"].get(k, 0) for k in ("oom", "oom_kill", "oom_group_kill")):
        raise ValueError(f"{directory}: OOM event")
    # Regenerate from retained response bodies; a saved fingerprint alone can
    # be stale or cover a partial replay. Include errors and every raw field.
    result = subprocess.run([sys.executable, str(Path(__file__).with_name("detect-fingerprint.py")),
                             str(directory / "responses"), "--content"],
                            capture_output=True, text=True, check=True)
    entries = json.loads(result.stdout)
    fingerprints = {entry["file"]: entry for entry in entries}
    if len(entries) != len(hashes) or set(fingerprints) != set(hashes):
        raise ValueError(f"{directory}: missing, duplicate, or unexpected outcomes")
    return manifest, config, summary, fingerprints


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("--before-repeat", type=Path, action="append", default=[],
                        help="additional complete baseline replay (repeat for three-run medians)")
    parser.add_argument("--after-repeat", type=Path, action="append", default=[],
                        help="additional complete candidate replay")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--target", type=float, default=1.4)
    parser.add_argument("--candidate-max-peak-gib", type=float, default=48,
                        help="candidate whole-cgroup memory budget for acceptance (default: 48 GiB)")
    args = parser.parse_args()
    if not math.isfinite(args.target) or args.target < 1:
        parser.error("target must be finite and at least 1")
    if not math.isfinite(args.candidate_max_peak_gib) or args.candidate_max_peak_gib <= 0:
        parser.error("candidate memory budget must be finite and positive")
    paths = [[args.before, *args.before_repeat], [args.after, *args.after_repeat]]
    if len(paths[0]) != len(paths[1]):
        raise ValueError("baseline and candidate must have the same number of runs")
    if len({path.resolve() for group in paths for path in group}) != sum(map(len, paths)):
        raise ValueError("each repeat must be a distinct replay directory")
    groups = [[inspect(path) for path in group] for group in paths]
    before, after = groups[0][0], groups[1][0]
    changed = set()
    runs = []
    for label, directories, group in zip(("before", "after"), paths, groups):
        for directory, replay in zip(directories, group):
            if before[0] != replay[0]:
                raise ValueError(f"{directory}: input manifests differ")
            for key in ("workers", "max_rss_gb", "fetch", "allocator_config",
                        "models_dir", "traits_dir", "cache_dir", "tuning_env", "host_memory_guard"):
                if before[1].get(key) != replay[1].get(key):
                    raise ValueError(f"{directory}: benchmark settings differ: {key}")
            if replay[1]["binary_sha256"] != group[0][1]["binary_sha256"]:
                raise ValueError(f"{directory}: executable changed within {label} runs")
            differences = [sha for sha in sorted(before[3]) if before[3][sha] != replay[3][sha]]
            changed.update(differences)
            final = replay[2]["cgroup"]["final"]
            cpu_start = replay[2]["cgroup"].get("start", {}).get("cpu", {}).get("usage_usec")
            cpu_end = final.get("cpu", {}).get("usage_usec")
            runs.append({"version": label, "directory": str(directory),
                         "wall_s": replay[2]["process_wall_s"],
                         "peak_gib": final["peak_bytes"] / 2**30,
                         "cpu_s": (cpu_end - cpu_start) / 1e6 if cpu_start is not None and cpu_end is not None else None,
                         "host_observation": host_observation(directory),
                         "changed_outcomes": differences})
    walls = [statistics.median(replay[2]["process_wall_s"] for replay in group) for group in groups]
    ratio = walls[0] / walls[1]
    parity = not changed
    candidate_peak = max(run[2]["cgroup"]["final"]["peak_bytes"] / 2**30 for run in groups[1])
    memory_ok = candidate_peak <= args.candidate_max_peak_gib
    report = {"before": str(args.before), "after": str(args.after),
              "inputs": len(before[0]), "unchanged_outcomes": len(before[0]) - len(changed),
              "changed_outcomes": sorted(changed), "complete_content_parity": parity,
              "before_wall_s": walls[0], "after_wall_s": walls[1],
              "runs_per_version": len(paths[0]), "aggregation": "median wall time", "runs": runs,
              "throughput_ratio": ratio, "throughput_gain_percent": (ratio - 1) * 100,
              "required_ratio": args.target, "target_met": ratio >= args.target and parity and memory_ok,
              "candidate_peak_gib": candidate_peak,
              "candidate_max_peak_gib": args.candidate_max_peak_gib,
              "candidate_memory_within_budget": memory_ok,
              "before_tuning": before[1].get("tuning_env", {}),
              "after_tuning": after[1].get("tuning_env", {}),
              "note": "Full raw and ML content, excluding only ML analyzed_at. Errors are outcomes. Host interference and unchanged asset contents require separate evidence."}
    with args.report.open("x") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(json.dumps(report, indent=2))
    return 0 if report["target_met"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
