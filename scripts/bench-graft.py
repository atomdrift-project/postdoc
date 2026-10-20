#!/usr/bin/env python3
"""Compare graft-memory binaries on fixed generated reports, including full parity.

This isolates retained-finding copies, excluding queues, fetching and tmpfs.
It cannot establish whole-service memory savings or throughput.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import statistics
import subprocess


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--rules", type=Path, default=Path("tests/fixtures/graft-memory"))
    parser.add_argument("--members", type=int, default=512)
    parser.add_argument("--evidence-bytes", type=int, default=65536)
    parser.add_argument("--declaring", type=int, default=8)
    parser.add_argument("--repeats", type=int, default=4)
    args = parser.parse_args()
    if min(args.members, args.declaring, args.repeats) < 1 or args.evidence_bytes < 0:
        parser.error("counts must be positive and evidence size nonnegative")
    if not args.rules.is_dir():
        parser.error("rules directory is missing")
    args.output.mkdir(parents=True, exist_ok=False)
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    config = {
        "binaries": {name: {"path": str(path), "sha256": digest(path)}
                     for name, path in binaries.items()},
        "rules": {str(p.relative_to(args.rules)): digest(p)
                  for p in sorted(args.rules.rglob("*.yaml"))},
        "members": args.members, "evidence_bytes": args.evidence_bytes,
        "declaring": args.declaring, "repeats": args.repeats,
    }
    (args.output / "config.json").write_text(json.dumps(config, indent=2) + "\n")
    env = os.environ.copy()
    env.pop("CLEAVE_SKIP_TRAITS", None)
    rows = {name: [] for name in binaries}
    expected = None
    for repeat in range(args.repeats):
        order = ("before", "after") if repeat % 2 == 0 else ("after", "before")
        for name in order:
            stem = args.output / f"{name}-{repeat}"
            report = stem.with_suffix(".report.json")
            with stem.with_suffix(".log").open("w") as log:
                result = subprocess.run(
                    [str(binaries[name]), str(args.rules.resolve()), str(args.members),
                     str(args.evidence_bytes), str(args.declaring), str(report.resolve())],
                    env=env, stdout=subprocess.PIPE, stderr=log, text=True,
                    timeout=120, check=True,
                )
            row = json.loads(result.stdout)
            stem.with_suffix(".stats.json").write_text(json.dumps(row, indent=2) + "\n")
            actual = json.loads(report.read_text())
            if expected is None:
                expected = actual
            elif actual != expected:
                raise RuntimeError(f"full report mismatch: {report}")
            rows[name].append(row)
    fields = ("allocated_bytes", "allocation_calls", "elapsed_ms", "seed_rss_kib", "peak_rss_kib")
    medians = {name: {k: statistics.median(row[k] for row in values) for k in fields}
               for name, values in rows.items()}
    summary = {"full_reports_equal": True, "medians": medians,
               "after_over_before": {k: medians["after"][k] / medians["before"][k]
                                     for k in fields if medians["before"][k]},
               "scope": "Synthetic public graft API; caller-thread cumulative allocations; fixture-process RSS."}
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
