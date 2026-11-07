"""Negative controls for the acceptance gate itself, not plugin correctness."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path, PureWindowsPath
from types import SimpleNamespace

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/django_acceptance.py"
SPEC = importlib.util.spec_from_file_location("django_acceptance", SCRIPT)
assert SPEC and SPEC.loader
ACCEPTANCE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ACCEPTANCE)


class AcceptanceTests(unittest.TestCase):
    def evaluator(self, expect="pass", required_message=None):
        marker = SimpleNamespace(
            feature="models",
            case="example",
            path="case.py",
            line=3,
            expect=expect,
            key=("case.py", 3),
        )
        return SimpleNamespace(
            collect_markers=lambda _: [marker],
            load_contracts=lambda: {
                "review": {"models/example": {"candidate_diagnostic": required_message}}
            },
        )

    def diagnostic(self, line=3, message="expected plugin diagnostic"):
        return {
            "path": "case.py",
            "begin": {"line": line, "column": 1},
            "message": message,
        }

    def test_unexpected_error_cannot_pass_positive_assertion(self):
        report = ACCEPTANCE.score_contract(
            [self.diagnostic()], Path("."), self.evaluator()
        )
        self.assertFalse(report["passed"])

    def test_unrelated_error_cannot_satisfy_required_negative_diagnostic(self):
        report = ACCEPTANCE.score_contract(
            [self.diagnostic(message="unknown import")],
            Path("."),
            self.evaluator("fail", "expected plugin diagnostic"),
        )
        self.assertFalse(report["passed"])

    def test_unowned_diagnostic_cannot_be_ignored(self):
        report = ACCEPTANCE.score_contract(
            [self.diagnostic(line=4)], Path("."), self.evaluator()
        )
        self.assertFalse(report["passed"])
        self.assertEqual(len(report["unowned_diagnostics"]), 1)

    def test_positive_missing_diagnostics_does_not_pass_negative_assertion(self):
        report = ACCEPTANCE.score_contract([], Path("."), self.evaluator("fail"))
        self.assertFalse(report["passed"])

    def test_external_diagnostic_path_is_rejected(self):
        diagnostic = {
            "location": {
                "path": "/outside/case.py",
                "positions": {
                    "begin": {"line": 3, "column": 1},
                    "end": {"line": 3, "column": 4},
                },
            },
            "check_name": "invalid-type",
            "severity": "major",
            "description": "error",
        }
        with self.assertRaises(ACCEPTANCE.Failure):
            ACCEPTANCE.normalize([diagnostic], Path("/project"))

    def test_windows_diagnostic_paths_must_belong_to_the_project(self):
        diagnostic = {
            "location": {
                "path": "case.py",
                "positions": {
                    "begin": {"line": 3, "column": 1},
                    "end": {"line": 3, "column": 4},
                },
            },
            "check_name": "invalid-type",
            "severity": "major",
            "description": r"Expected Literal['a\nb']",
        }
        project = PureWindowsPath(r"C:\project")
        for path in (
            r"C:\project\case.py",
            r"\project\case.py",
            "/project/case.py",
            "case.py",
        ):
            with self.subTest(path=path):
                diagnostic["location"]["path"] = path
                actual = ACCEPTANCE.normalize([diagnostic], project)
                self.assertEqual(actual[0]["path"], "case.py")
                self.assertEqual(actual[0]["message"], diagnostic["description"])
        for path in (
            "/outside/case.py",
            r"\outside\case.py",
            r"D:\project\case.py",
            r"C:project\case.py",
            r"C:case.py",
            r"..\outside\case.py",
            r"\\server\share\case.py",
            r"C:\project-other\case.py",
        ):
            diagnostic["location"]["path"] = path
            with self.subTest(path=path), self.assertRaises(ACCEPTANCE.Failure):
                ACCEPTANCE.normalize([diagnostic], project)
        diagnostic["location"]["path"] = r"\\server\share\project\case.py"
        self.assertEqual(
            ACCEPTANCE.normalize(
                [diagnostic], PureWindowsPath(r"\\server\share\project")
            )[0]["path"],
            "case.py",
        )

    def test_additional_staged_source_cannot_be_silently_injected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repository = root / "repository"
            (repository / "conformance").mkdir(parents=True)
            (repository / "conformance/case.py").write_text("value = 1\n")
            staged = ACCEPTANCE.stage(repository, root / "output", "conformance")
            (staged / "unexpected.py").write_text("value = 2\n")
            with self.assertRaises(ACCEPTANCE.Failure):
                ACCEPTANCE.stage(repository, root / "output", "conformance")


if __name__ == "__main__":
    unittest.main()
