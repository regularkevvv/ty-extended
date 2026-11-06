"""Compatibility evidence must fail closed on subprocess failure or timeout."""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from checker_compatibility import run


class CompatibilityTests(unittest.TestCase):
    def test_failures_preserve_output_and_do_not_report_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "gate.log"
            result = run(
                [sys.executable, "-c", "print('regression'); raise SystemExit(3)"],
                log,
                dict(os.environ),
                10,
            )
            self.assertFalse(result["passed"])
            self.assertEqual(result["exit"], 3)
            self.assertFalse(result["timed_out"])
            self.assertIn("regression", log.read_text())

    def test_hung_checker_is_terminated_and_reported_as_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / "gate.log"
            result = run(
                [
                    sys.executable,
                    "-c",
                    "import time; print('started', flush=True); time.sleep(60)",
                ],
                log,
                dict(os.environ),
                1,
            )
            self.assertFalse(result["passed"])
            self.assertTrue(result["timed_out"])
            self.assertNotEqual(result["exit"], 0)
            self.assertLess(result["seconds"], 10)
            self.assertIn("started", log.read_text())


if __name__ == "__main__":
    unittest.main()
