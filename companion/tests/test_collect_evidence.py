"""Portable evidence must not turn incomplete or unrelated runs into proof."""
from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import collect_evidence as evidence


class EvidenceTests(unittest.TestCase):
    def test_checker_release_preserves_release_and_allows_build_metadata(self):
        self.assertEqual(evidence.checker_release("ty 0.0.86"), "0.0.86")
        self.assertEqual(
            evidence.checker_release("ty 0.86.0+147 (4429f1e88 2026-10-10)"),
            "0.86.0",
        )
        self.assertNotEqual(evidence.checker_release("ty 0.86.1+147"), "0.86.0")
        for version in ("ty 0.86.0rc1", "ty 0.86.0-beta", "ty 0.86.0+", "other 0.86.0"):
            with self.subTest(version=version), self.assertRaises(evidence.Failure):
                evidence.checker_release(version)

    def matrix(self):
        return {
            "mode": "compare",
            "checker_mode": "cli",
            "complete_matrix_selected": True,
            "compatibility_passed": True,
            "required_pairs": 18,
            "selected_pairs": 18,
            "results": [
                {"django": f"5.2.{index}", "python": "3.13", "passed": True, "exit": 0}
                for index in range(18)
            ],
        }

    def test_partial_matrix_is_rejected_even_if_marked_green(self):
        matrix = self.matrix()
        matrix["results"].pop()
        with self.assertRaises(evidence.Failure):
            evidence.validate_matrix(matrix, "cli")

    def test_failed_pair_is_rejected_even_if_summary_is_green(self):
        matrix = self.matrix()
        matrix["results"][2]["passed"] = False
        with self.assertRaises(evidence.Failure):
            evidence.validate_matrix(matrix, "cli")

    def test_different_binary_is_rejected(self):
        with self.assertRaises(evidence.Failure):
            evidence.validate_binary_hashes(
                {"companion": {"sha256": "old"}},
                {"companion": {"sha256": "final"}},
            )

    def test_supplemental_failure_cannot_hide_behind_passed_flag(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "gate.json"
            path.write_text('{"passed":true,"status":"failed"}')
            with self.assertRaises(evidence.Failure):
                evidence.supplemental_json(path)

    def test_unknown_absolute_path_is_not_published(self):
        with self.assertRaises(evidence.Failure):
            evidence.portable({"artifact": "/private/tmp/unregistered/output"}, {})


if __name__ == "__main__":
    unittest.main()
