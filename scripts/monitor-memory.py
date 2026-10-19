#!/usr/bin/env python3
"""Sample a Linux service's whole cgroup, including helpers and tmpfs charges."""

import argparse
import json
from pathlib import Path
import time


def counters(path):
    return {key: int(value) for key, value in
            (line.split() for line in path.read_text().splitlines())}


def sample(group):
    result = {"time": time.time(), "cgroup_inode": group.stat().st_ino}
    for name in ("current", "peak"):
        result[name] = int((group / f"memory.{name}").read_text())
    result["memory_stat"] = counters(group / "memory.stat")
    result["events"] = counters(group / "memory.events")
    result["cpu"] = counters(group / "cpu.stat")
    result["groups"] = {}
    result["processes"] = []
    for procs in [group / "cgroup.procs", *group.rglob("*/cgroup.procs")]:
        try:
            child = procs.parent
            result["groups"][str(child.relative_to(group))] = {
                "current": int((child / "memory.current").read_text()),
                "events": counters(child / "memory.events"),
            }
            pids = procs.read_text().split()
        except FileNotFoundError:
            continue
        for pid in pids:
            try:
                proc = Path("/proc") / pid
                status = dict(line.split(":", 1) for line in
                              (proc / "status").read_text().splitlines())
                entry = {"pid": int(pid), "group": str(child.relative_to(group)),
                         "name": status["Name"].strip()}
                for name in ("VmRSS", "VmHWM", "RssAnon", "RssFile", "RssShmem"):
                    entry[name + "_kib"] = int(status.get(name, "0 kB").split()[0])
                if entry["name"] == "postdoc":
                    with (proc / "maps").open() as mappings:
                        entry["mappings"] = sum(1 for _ in mappings)
                result["processes"].append(entry)
            except (FileNotFoundError, ProcessLookupError):
                continue
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--cgroup", type=Path,
                        default=Path("/sys/fs/cgroup/system.slice/postdoc.service"))
    parser.add_argument("--duration", type=float, default=1200)
    parser.add_argument("--interval", type=float, default=15)
    args = parser.parse_args()
    if args.duration <= 0 or args.interval <= 0:
        parser.error("duration and interval must be positive")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    with args.output.open("x") as output:
        while time.monotonic() - started < args.duration:
            tick = time.monotonic()
            output.write(json.dumps(sample(args.cgroup)) + "\n")
            output.flush()
            remaining = args.duration - (time.monotonic() - started)
            delay = min(args.interval - (time.monotonic() - tick), remaining)
            if delay > 0:
                time.sleep(delay)


if __name__ == "__main__":
    main()
