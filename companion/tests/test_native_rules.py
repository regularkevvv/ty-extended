"""Rule renames must not hide changed diagnostic behavior in comparisons."""

from __future__ import annotations

import copy
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from native_rules import RENAMED_RULES, semantic_id
from plugin_configuration_differential import diagnostics, lsp_diagnostics


class NativeRuleTests(unittest.TestCase):
    def test_lsp_aliases_preserve_messages_positions_severity_and_input(self):
        for new, old in RENAMED_RULES.items():
            original = {
                "code": new,
                "message": "Exact message",
                "severity": 1,
                "range": {
                    "start": {"line": 1, "character": 2},
                    "end": {"line": 1, "character": 4},
                },
            }
            before = copy.deepcopy(original)
            expected = {**original, "code": old}
            self.assertEqual(lsp_diagnostics([original]), lsp_diagnostics([expected]))
            self.assertEqual(original, before)
            for key, changed in [
                ("message", "Changed message"),
                ("severity", 2),
                (
                    "range",
                    {
                        "start": {"line": 2, "character": 2},
                        "end": {"line": 2, "character": 4},
                    },
                ),
            ]:
                self.assertNotEqual(
                    lsp_diagnostics([{**original, key: changed}]),
                    lsp_diagnostics([expected]),
                )

    def test_cli_only_normalizes_the_leading_rule_prefix(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "demo.py"
            source.write_text("value = 1\n")
            new = {
                "check_name": "missing-attribute",
                "description": "missing-attribute: Exact text mentioning missing-attribute",
                "severity": "major",
                "location": {
                    "path": str(source),
                    "positions": {
                        "begin": {"line": 1, "column": 1},
                        "end": {"line": 1, "column": 2},
                    },
                },
            }
            before = copy.deepcopy(new)
            old = {
                **new,
                "check_name": "unresolved-attribute",
                "description": "unresolved-attribute: Exact text mentioning missing-attribute",
            }
            self.assertEqual(diagnostics([new], root), diagnostics([old], root))
            self.assertEqual(new, before)
            self.assertNotEqual(
                diagnostics(
                    [{**new, "description": new["description"] + " changed"}], root
                ),
                diagnostics([old], root),
            )

    def test_split_or_unknown_rules_keep_their_identity(self):
        for code in [
            "invalid-typed-dict-key",
            "missing-typed-dict-key",
            "invalid-implicit-return",
            "invalid-generator-return-type",
            "plugin-configuration",
            "future-rule",
            42,
        ]:
            self.assertEqual(semantic_id(code), code)


if __name__ == "__main__":
    unittest.main()
