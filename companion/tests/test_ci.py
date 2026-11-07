"""A failed gate must leave evidence and must not prevent independent checks."""

from __future__ import annotations

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import ci


class GateAggregationTests(unittest.TestCase):
    def test_failed_gate_does_not_stop_next_gate_and_summary_fails(self):
        with tempfile.TemporaryDirectory() as directory, contextlib.redirect_stdout(
            io.StringIO()
        ):
            root = Path(directory)
            gates = ci.Gates(root)
            gates.run(
                "failed",
                [sys.executable, "-c", "print('failure detail'); raise SystemExit(7)"],
                env=dict(os.environ),
            )
            gates.run(
                "next",
                [sys.executable, "-c", "print('independent check ran')"],
                env=dict(os.environ),
            )
            self.assertEqual(gates.finish(), 1)
            report = json.loads((root / "ci.json").read_text())
            self.assertFalse(report["passed"])
            self.assertEqual(report["failures"], ["failed"])
            self.assertEqual([gate["exit"] for gate in report["gates"]], [7, 0])
            self.assertIn("failure detail", (root / "gates/failed.log").read_text())
            self.assertIn(
                "independent check ran", (root / "gates/next.log").read_text()
            )

    def test_setup_failure_is_fatal_and_records_partial_summary(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "ci-output"
            with patch.object(
                sys, "argv", ["ci.py", "--output", str(root)]
            ), patch.object(ci.Gates, "run") as gate, patch.object(
                ci, "run", side_effect=RuntimeError("compiler setup failed")
            ), self.assertRaisesRegex(RuntimeError, "compiler setup failed"):
                ci.main()
            self.assertEqual(gate.call_count, 1)
            report = json.loads((root / "ci.json").read_text())
            self.assertFalse(report["passed"])
            self.assertEqual(
                report["setup_error"], "RuntimeError: compiler setup failed"
            )

    def test_successful_gates_finish_green(self):
        with tempfile.TemporaryDirectory() as directory, contextlib.redirect_stdout(
            io.StringIO()
        ):
            gates = ci.Gates(Path(directory))
            gates.run("passed", [sys.executable, "-c", "pass"], env=dict(os.environ))
            self.assertEqual(gates.finish(), 0)
            self.assertTrue(
                json.loads((Path(directory) / "ci.json").read_text())["passed"]
            )


if __name__ == "__main__":
    unittest.main()
