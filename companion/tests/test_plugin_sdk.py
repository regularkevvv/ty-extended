"""Wire compatibility of the Python SDK shipped with the companion."""

from __future__ import annotations

import ast
import importlib.util
import json
import unittest
from pathlib import Path


class PluginSdkTests(unittest.TestCase):
    def setUp(self):
        root = Path(__file__).resolve().parents[2]
        path = root / "python/ty_extended/plugin_sdk.py"
        spec = importlib.util.spec_from_file_location("tested_plugin_sdk", path)
        self.sdk = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.sdk)
        embedded = root / "crates/ty_plugin_host/python/ty_plugin_sdk.py"
        # The generated package adjusts the module's introductory prose.
        self.assertEqual(
            [ast.dump(node) for node in ast.parse(path.read_text()).body[1:]],
            [ast.dump(node) for node in ast.parse(embedded.read_text()).body[1:]],
        )

    def test_class_indexes_are_optional_opaque_replacements(self):
        self.assertNotIn("class-indexes", self.sdk.project_index({"full": True}))
        self.assertNotIn(
            "class-indexes", self.sdk.project_index({"full": True}, class_indexes={})
        )
        scopes = {"app.Row": {"qualified-name": "opaque.Row"}, "app.Empty": None}
        response = self.sdk.project_index({"full": True}, class_indexes=scopes)
        self.assertEqual(response["class-indexes"], scopes)
        self.assertEqual(response["plugin-index"], {"full": True})

    def test_dispatch_compacts_json_without_altering_plugin_strings(self):
        expected = self.sdk.project_index(
            {"text": 'spaces : , Unicode ñ and "quotes"', "nested": [None, True, 42]},
            class_indexes={"app.Row": ["opaque", {"a": 1}]},
        )

        @self.sdk.on_project_index
        def index(request):
            return expected

        response = self.sdk.__ty_handle__('{"kind":"build-project-index"}')
        self.assertEqual(json.loads(response), expected)
        self.assertEqual(response, json.dumps(expected, separators=(",", ":")))
        self.assertLess(len(response), len(json.dumps(expected)))

    def test_index_lookups_preserve_null_keys_and_return_independent_values(self):
        request = {
            "project-index": {
                "a/b": {"qualified-name": "opaque.Row", "value": [1]},
                "null": None,
            }
        }
        self.assertIsNone(self.sdk.project_index_value(request, ["null"], "missing"))
        self.assertEqual(
            self.sdk.project_index_value(request, ["absent"], "missing"), "missing"
        )
        self.assertEqual(
            self.sdk.project_index_keys(request, ["a/b"]), ["qualified-name", "value"]
        )
        self.assertEqual(self.sdk.project_index_keys(request, ["null"]), [])
        value = self.sdk.project_index_value(request, ["a/b"])
        value["value"].append(2)
        self.assertEqual(self.sdk.project_index_value(request, ["a/b", "value"]), [1])


if __name__ == "__main__":
    unittest.main()
