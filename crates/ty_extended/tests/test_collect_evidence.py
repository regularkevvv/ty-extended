"""Portable evidence must not turn incomplete or unrelated runs into proof."""

from __future__ import annotations

import json
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

    def test_supplemental_binary_identity_is_required_and_all_claims_must_match(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "gate.json"
            for identity in (
                {"companion_sha256": "old"},
                {"binary_sha256": "old"},
                {"artifact": "/binary", "sha256": "old"},
                {"companion": {"sha256": "old"}},
                {"companion_binary": {"sha256": "old"}},
                {"binaries": {"companion": {"sha256": "old"}}},
                {},
                {"companion_sha256": "final", "binary_sha256": "old"},
            ):
                with self.subTest(identity=identity):
                    path.write_text(json.dumps({"passed": True, **identity}))
                    with self.assertRaises(evidence.Failure):
                        evidence.supplemental_json(path, "final")

    def test_companion_fingerprint_does_not_replace_other_artifact_identities(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "gate.json"
            report = {
                "passed": True,
                "companion_sha256": "final",
                "binary_sha256": "final",
                "artifact": "/binary",
                "sha256": "final",
                "source_sha256": "source",
                "official_sha256": "official",
                "fork_sha256": "fork",
                "binaries": {
                    "companion": {"sha256": "final"},
                    "official": {"sha256": "official"},
                    "fork": {"sha256": "fork"},
                },
            }
            path.write_text(json.dumps(report))
            self.assertEqual(evidence.supplemental_json(path, "final"), report)

    def test_explicit_path_labels_do_not_cover_unrelated_siblings(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            labeled = root / "gate"
            labeled.mkdir()
            labels = {}
            evidence.path_label(labels, f"editor={labeled}")
            self.assertEqual(
                evidence.portable(
                    {
                        "report": str(labeled / "summary.json"),
                        "uri": f"file://{labeled}/demo.py:3",
                    },
                    labels,
                ),
                {
                    "report": "artifact:editor/summary.json",
                    "uri": "file://artifact:editor/demo.py:3",
                },
            )
            for unknown in (root / "gate-other" / "summary.json", root / "elsewhere"):
                with self.subTest(unknown=unknown), self.assertRaises(evidence.Failure):
                    evidence.portable(str(unknown), labels)

    def test_path_labels_reject_missing_paths_and_conflicting_identities(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            first, second = root / "first", root / "second"
            first.mkdir()
            second.mkdir()
            labels = {}
            evidence.path_label(labels, f"first={first}")
            for specification in (
                f"missing={root / 'missing'}",
                f"other={first}",
                f"first={second}",
                f"Not_a_label={second}",
                "empty=",
            ):
                with self.subTest(specification=specification), self.assertRaises(
                    evidence.Failure
                ):
                    evidence.path_label(labels, specification)
            self.assertEqual(labels, {str(first): "artifact:first"})

    def test_unknown_absolute_path_is_not_published(self):
        with self.assertRaises(evidence.Failure):
            evidence.portable({"artifact": "/private/tmp/unregistered/output"}, {})

    def test_windows_labels_have_portable_separators_without_rewriting_prose(self):
        labels = {r"C:\runner\evidence": "artifact:windows"}
        for path in (
            r"C:\runner\evidence\nested\summary.json",
            "C:/runner/evidence/nested/summary.json",
        ):
            with self.subTest(path=path):
                self.assertEqual(
                    evidence.portable(path, labels),
                    "artifact:windows/nested/summary.json",
                )
        self.assertEqual(
            evidence.portable(r"C:\runner\evidence\sub dir\a file.json", labels),
            "artifact:windows/sub dir/a file.json",
        )
        self.assertEqual(
            evidence.portable("file:///C:/runner/evidence/nested/summary.json", labels),
            "file://artifact:windows/nested/summary.json",
        )
        text = r"Report C:\runner\evidence\summary.json; expected Literal['a\nb']"
        self.assertEqual(
            evidence.portable(text, labels),
            r"Report artifact:windows/summary.json; expected Literal['a\nb']",
        )
        literal = r"Expected Literal['C:\unregistered\file.py'] and regex \w+"
        self.assertEqual(evidence.portable(literal, {}), literal)

    def test_unlabeled_windows_anchors_cannot_bypass_path_rejection(self):
        labels = {r"C:\runner\evidence": "artifact:windows"}
        for path in (
            r"C:\unregistered\file.json",
            "C:/unregistered/file.json",
            r"\\server\share\file.json",
            r"\unregistered\file.json",
            r"C:relative\file.json",
            "file:///C:/unregistered/file.json",
            r"C:\runner\evidence-other\file.json",
        ):
            with self.subTest(path=path), self.assertRaises(evidence.Failure):
                evidence.portable(path, labels)


if __name__ == "__main__":
    unittest.main()
