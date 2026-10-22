#!/usr/bin/env python3
"""Check throughput acceptance against complete results and memory peaks."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class ThroughputAcceptanceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="postdoc-throughput-check-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.paths = {label: [] for label in ("before", "after")}
        sha = "a" * 64
        for label, wall in (("before", 14), ("after", 10)):
            for repeat in range(3):
                path = self.root / f"{label}-{repeat}"
                (path / "responses").mkdir(parents=True)
                self.paths[label].append(path)
                self.write(path / "manifest.json", [{"path": "fixture", "sha256": sha, "bytes": 1}])
                self.write(path / "config.json", {
                    "workers": 1, "max_rss_gb": 54, "fetch": "none",
                    "heap_sampling": False, "binary_sha256": ("a" if label == "before" else "b") * 64,
                })
                self.write(path / "summary.json", {
                    "jobs": 1, "done": 1, "process_wall_s": wall, "harness_wall_s": wall + 1,
                    "cgroup": {"final": {"events": {"oom": 0}, "peak_bytes": 2**30}},
                })
                self.write(path / "responses" / f"{sha}.json", {
                    "raw": {"evidence": ["unchanged"]},
                    "ml": {"lvl": 0, "files": [], "analyzed_at": label},
                })

    @staticmethod
    def write(path, value):
        path.write_text(json.dumps(value))

    def peak(self, repeat, gib):
        path = self.paths["after"][repeat] / "summary.json"
        value = json.loads(path.read_text())
        value["cgroup"]["final"]["peak_bytes"] = int(gib * 2**30)
        self.write(path, value)

    def compare(self, *extra):
        report = self.root / "comparison.json"
        command = [sys.executable, str(Path(__file__).with_name("compare-throughput.py")),
                   str(self.paths["before"][0]), str(self.paths["after"][0]),
                   "--report", str(report), *extra]
        for label in ("before", "after"):
            for path in self.paths[label][1:]:
                command += [f"--{label}-repeat", str(path)]
        result = subprocess.run(command, capture_output=True, text=True)
        self.assertIn(result.returncode, (0, 1), result.stderr)
        return result.returncode, json.loads(report.read_text())

    def test_accepts_target_at_memory_boundary(self):
        self.peak(0, 48)
        code, report = self.compare()
        self.assertEqual(code, 0)
        self.assertTrue(report["target_met"])
        self.assertTrue(report["candidate_memory_within_budget"])
        self.assertEqual(report["candidate_peak_gib"], 48)

    def test_rejects_one_oversized_repeat_despite_fast_median(self):
        self.peak(2, 49)
        code, report = self.compare()
        self.assertEqual(code, 1)
        self.assertTrue(report["complete_content_parity"])
        self.assertEqual(report["throughput_ratio"], 1.4)
        self.assertFalse(report["candidate_memory_within_budget"])
        self.assertFalse(report["target_met"])

    def test_memory_budget_is_explicit(self):
        self.peak(1, 25)
        code, report = self.compare("--candidate-max-peak-gib", "24")
        self.assertEqual(code, 1)
        self.assertEqual(report["candidate_max_peak_gib"], 24)
        self.assertFalse(report["target_met"])

    def test_memory_success_cannot_hide_changed_raw_output(self):
        path = next((self.paths["after"][1] / "responses").iterdir())
        value = json.loads(path.read_text())
        value["raw"]["evidence"] = ["changed"]
        self.write(path, value)
        code, report = self.compare()
        self.assertEqual(code, 1)
        self.assertTrue(report["candidate_memory_within_budget"])
        self.assertFalse(report["complete_content_parity"])
        self.assertFalse(report["target_met"])


if __name__ == "__main__":
    unittest.main()
