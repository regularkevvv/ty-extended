"""Windows path spellings must preserve diagnostic and navigation assertions."""

from __future__ import annotations

import copy
import sys
import tempfile
import unittest
from pathlib import Path, PureWindowsPath

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from harness_paths import file_uri_matches, relative_path
from plugin_configuration_differential import diagnostics
from plugin_error_differential import cli_diagnostics


class HarnessPathsTests(unittest.TestCase):
    def test_windows_drive_verbatim_rooted_relative_and_unc(self):
        project = PureWindowsPath(r"D:\a\project")
        for value in (
            r"D:\a\project\nested\demo.py",
            r"\\?\D:\a\project\nested\demo.py",
            r"d:\A\PROJECT\nested\demo.py",
            r"\a\project\nested\demo.py",
            r"nested\demo.py",
        ):
            with self.subTest(value=value):
                self.assertEqual(relative_path(value, project), "nested/demo.py")
        self.assertEqual(
            relative_path(
                r"\\?\UNC\server\share\project\demo.py",
                PureWindowsPath(r"\\server\share\project"),
            ),
            "demo.py",
        )

    def test_external_and_ambiguous_paths_are_rejected(self):
        project = PureWindowsPath(r"D:\a\project")
        for value in (
            r"D:\a\project-other\demo.py",
            r"\\?\D:\outside\demo.py",
            r"C:\a\project\demo.py",
            r"D:demo.py",
            r"..\demo.py",
            r"\\?\Volume{unknown}\demo.py",
            r"\\other\share\demo.py",
        ):
            with self.subTest(value=value), self.assertRaises(ValueError):
                relative_path(value, project)

    def test_native_paths_resolve_relative_and_absolute_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "demo.py").write_text("value: int = 'bad'\n", encoding="utf-8")
            self.assertEqual(relative_path("demo.py", root), "demo.py")
            self.assertEqual(relative_path(str(root / "demo.py"), root), "demo.py")
            with self.assertRaises(ValueError):
                relative_path(str(root.parent / "outside.py"), root)

    def test_only_explicit_fixture_manifest_can_be_external(self):
        root = PureWindowsPath(r"D:\case\project")
        manifest = PureWindowsPath(r"D:\case\plugin.json")
        self.assertEqual(
            relative_path(
                r"\\?\D:\case\plugin.json", root, allowed_external=(manifest,)
            ),
            "../plugin.json",
        )
        with self.assertRaises(ValueError):
            relative_path(r"\\?\D:\case\other.json", root, allowed_external=(manifest,))

    def test_diagnostic_adapters_preserve_exact_nonpath_fields(self):
        root = PureWindowsPath(r"D:\project")
        value = {
            "check_name": "invalid-assignment",
            "description": 'invalid-assignment: found `Literal["C:\\keep\\text"]`',
            "severity": "major",
            "location": {
                "path": r"\\?\D:\project\demo.py",
                "positions": {
                    "begin": {"line": 53, "column": 4},
                    "end": {"line": 53, "column": 9},
                },
            },
        }
        original = copy.deepcopy(value)
        expected = copy.deepcopy(value)
        expected["location"]["path"] = "demo.py"
        self.assertEqual(diagnostics([value], root), [expected])
        self.assertEqual(cli_diagnostics([value], root), [expected])
        self.assertEqual(value, original)
        value["location"]["path"] = r"D:\project\other.py"
        with self.assertRaises(AssertionError):
            cli_diagnostics([value], root)
        value["location"]["path"] = r"D:\outside\demo.py"
        with self.assertRaises(ValueError):
            diagnostics([value], root)

    def test_file_uri_identity_preserves_casing_and_escaping_semantics(self):
        path = PureWindowsPath(r"D:\project\Lib\site-packages\café name.py")
        self.assertTrue(
            file_uri_matches(
                "file:///D:/project/lib/site-packages/caf%C3%A9%20name.py", path
            )
        )
        self.assertTrue(
            file_uri_matches(
                "file://server/share/project/demo.py",
                PureWindowsPath(r"\\?\UNC\server\share\project\demo.py"),
            )
        )
        self.assertFalse(file_uri_matches("file:///D:/outside/demo.py", path))
        for uri in (
            "https://example.com/demo.py",
            "file:///D:/demo.py#fragment",
            "file:demo.py",
        ):
            with self.subTest(uri=uri), self.assertRaises(ValueError):
                file_uri_matches(uri, path)


if __name__ == "__main__":
    unittest.main()
