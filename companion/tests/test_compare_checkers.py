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


if __name__ == "__main__":
    unittest.main()
