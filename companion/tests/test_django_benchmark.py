"""Reject misleading benchmark samples before reporting timing comparisons."""

from __future__ import annotations

import copy
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import django_benchmark as benchmark


class BenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.expected = {
            "exit": 1,
            "diagnostics": [
                {
                    "path": "case.py",
                    "begin": {"line": 53, "column": 4},
                    "end": {"line": 53, "column": 8},
                    "code": "invalid-assignment",
                    "severity": "major",
                    "message": "Object of type `str` is not assignable to `int`",
                }
            ],
        }
        self.actual = {
            **copy.deepcopy(self.expected),
            "passed": True,
            "metrics": {
                "checker_mode": "push",
                "plugin_runtime": "embedded-monty",
                "projection_rounds": 3,
                "oracle_rounds": 2,
                "monty_executions": 15,
                "monty_cache_hits": 10,
                "suppressed_generated": 1,
                "monty_seconds": 0.05,
            },
        }

    def test_original_positions_are_part_of_verified_hash(self):
        original_hash = benchmark.diagnostic_hash(self.expected["diagnostics"])
        self.actual["diagnostics"][0]["begin"]["line"] = 51
        self.assertNotEqual(
            benchmark.diagnostic_hash(self.actual["diagnostics"]), original_hash
        )
        with self.assertRaises(benchmark.Failure):
            benchmark.verify_result(self.actual, self.expected, "push", None)

    def test_exit_status_must_match_despite_identical_diagnostics(self):
        self.actual["exit"] = 0
        with self.assertRaises(benchmark.Failure):
            benchmark.verify_result(self.actual, self.expected, "push", None)

    def test_native_work_cannot_be_missing_from_faster_sample(self):
        self.actual["metrics"]["monty_executions"] = 0
        with self.assertRaises(benchmark.Failure):
            benchmark.verify_result(self.actual, self.expected, "push", None)

    def test_counter_change_invalidates_repeat(self):
        previous = benchmark.verify_result(self.actual, self.expected, "push", None)
        self.actual["metrics"]["oracle_rounds"] -= 1
        with self.assertRaises(benchmark.Failure):
            benchmark.verify_result(self.actual, self.expected, "push", previous)

    def test_actual_suppression_arrays_are_counted_and_retained(self):
        suppressed = [{"check_name": "revealed-type", "description": "generated probe"}]
        self.actual["metrics"]["suppressed_generated"] = suppressed
        counters = benchmark.verify_result(self.actual, self.expected, "push", None)
        self.assertEqual(counters["suppressed_generated"], 1)
        self.assertIs(self.actual["metrics"]["suppressed_generated"], suppressed)
        suppressed.append(
            {"check_name": "unresolved-import", "description": "generated import"}
        )
        with self.assertRaises(benchmark.Failure):
            benchmark.verify_result(self.actual, self.expected, "push", counters)

    def test_wrong_checker_mode_cannot_be_labeled_push(self):
        self.actual["metrics"]["checker_mode"] = "cli"
        with self.assertRaises(benchmark.Failure):
            benchmark.verify_result(self.actual, self.expected, "push", None)

    def test_timing_summary_excludes_first_observed_and_warmup(self):
        records = [
            {
                "suite": "conformance",
                "backend": backend,
                "phase": phase,
                "seconds": seconds,
            }
            for backend, phase, seconds in [
                ("fork", "first-observed", 50),
                ("push", "warmup", 100),
                ("fork", "measured", 2),
                ("push", "measured", 4),
            ]
        ]
        summary = benchmark.summarize(records)["conformance"]
        self.assertEqual(summary["push"]["native_over_fork_ratio"], 2)
        self.assertEqual(summary["fork"]["runs"], 1)
        self.assertEqual(summary["push"]["runs"], 1)


if __name__ == "__main__":
    unittest.main()
