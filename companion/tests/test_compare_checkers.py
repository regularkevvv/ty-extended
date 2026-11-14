"""Failed or destructive checker runs must never become benchmark evidence."""

from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import compare_checkers


class PerformanceGateTests(unittest.TestCase):
    def test_exact_three_times_passes_and_reports_one_second_goal_separately(self):
        gate = compare_checkers.performance_gate(
            {"companion": [1.5, 1.8], "bare-ty": [0.5, 0.6], "mypy": [5, 6]}
        )
        self.assertTrue(gate["passed"])
        self.assertFalse(gate["one_second_goal_met"])

    def test_fast_median_cannot_hide_one_pair_over_the_limit(self):
        gate = compare_checkers.performance_gate(
            {"companion": [0.6, 0.6, 1.2], "bare-ty": [0.3] * 3, "mypy": [5] * 3}
        )
        self.assertEqual(gate["median_ty_ratio"], 2)
        self.assertEqual(gate["max_paired_ty_ratio"], 4)
        self.assertFalse(gate["passed"])

    def test_mypy_remains_an_independent_ceiling(self):
        gate = compare_checkers.performance_gate(
            {"companion": [0.9], "bare-ty": [0.4], "mypy": [0.8]}
        )
        self.assertTrue(gate["ty_ceiling_met"])
        self.assertFalse(gate["passed"])

    def test_accepted_aggregate_policy_retains_and_reports_slow_pairs(self):
        gate = compare_checkers.performance_gate(
            {"companion": [0.6, 0.6, 1.2], "bare-ty": [0.3] * 3, "mypy": [5] * 3},
            3.5,
            aggregate_ratios=True,
        )
        self.assertTrue(gate["passed"])
        self.assertEqual(gate["paired_ty_ratios"], [2, 2, 4])
        self.assertEqual(gate["max_paired_ty_ratio"], 4)

    def test_fast_median_cannot_hide_excessive_mean_under_aggregate_policy(self):
        gate = compare_checkers.performance_gate(
            {"companion": [0.6, 0.6, 4.0], "bare-ty": [0.3] * 3, "mypy": [5] * 3},
            3.5,
            aggregate_ratios=True,
        )
        self.assertEqual(gate["median_ty_ratio"], 2)
        self.assertGreater(gate["mean_ty_ratio"], 3.5)
        self.assertFalse(gate["passed"])

    def test_incomplete_or_invalid_observations_cannot_pass(self):
        for samples in (
            {"companion": [], "bare-ty": [], "mypy": []},
            {"companion": [1], "bare-ty": [1, 2], "mypy": [1]},
            {"companion": [1], "bare-ty": [0], "mypy": [1]},
            {"companion": [float("nan")], "bare-ty": [1], "mypy": [1]},
            {"companion": [1], "bare-ty": [1]},
        ):
            with self.subTest(samples=samples), self.assertRaises(ValueError):
                compare_checkers.performance_gate(samples)


@unittest.skipUnless(os.name == "posix", "requires POSIX process monitoring")
class ComparisonTests(unittest.TestCase):
    def run_rejected(self, source, *, timeout=5, max_memory_mib=4096):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            project = output / "project"
            project.mkdir()
            (project / "demo.py").write_text("value: int = 1\n")
            args = SimpleNamespace(
                output=output,
                project=project,
                threads=0,
                timeout=timeout,
                max_memory_mib=max_memory_mib,
            )
            with self.assertRaisesRegex(RuntimeError, "Invalid benchmark run"):
                compare_checkers.invoke(
                    args, "companion", [sys.executable, "-c", source], "failed"
                )
            run = output / "runs/failed"
            self.assertFalse((run / "scratch").exists())
            return json.loads((run / "result.json").read_text())

    def test_timeout_kills_checker_and_removes_its_scratch(self):
        row = self.run_rejected("import time; time.sleep(30)", timeout=0.1)
        self.assertEqual(row["stopped"], "timeout")
        self.assertLess(row["exit"], 0)

    def test_memory_guard_kills_checker_and_removes_its_scratch(self):
        row = self.run_rejected("import time; time.sleep(30)", max_memory_mib=1)
        self.assertEqual(row["stopped"], "memory-limit")

    def test_malformed_output_cannot_look_like_zero_diagnostics(self):
        row = self.run_rejected("print('{}')")
        self.assertIsNotNone(row["parse_error"])

    def test_source_mutation_invalidates_the_sample(self):
        row = self.run_rejected(
            "from pathlib import Path; Path('demo.py').write_text(''); print('[]')"
        )
        self.assertFalse(row["source_preserved"])

    def test_duplicate_python_error_cannot_replace_missing_orm_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "runs/negative/mypy"
            output.mkdir(parents=True)
            error = {"file": "filler00000.py", "line": 1, "code": "assignment"}
            (output / "stdout.json").write_text((json.dumps(error) + "\n") * 2)
            args = SimpleNamespace(output=root, project=root / "project", queries=100)
            row = {
                "label": "negative/mypy",
                "exit": 1,
                "diagnostic_codes": {"assignment": 2},
            }
            with self.assertRaisesRegex(RuntimeError, "Lost an ORM"):
                compare_checkers.verify_negative_controls(args, "mypy", row)


if __name__ == "__main__":
    unittest.main()
