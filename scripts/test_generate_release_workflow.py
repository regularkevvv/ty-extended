"""Regression for GitHub workflow scalars that YAML 1.1 silently changes."""

from __future__ import annotations

import unittest

from generate_release_workflow import load


class WorkflowScalarTests(unittest.TestCase):
    def test_event_names_and_manylinux_versions_are_preserved(self):
        data = load(
            "on:\n  pull_request:\nwith:\n  manylinux: 2_17\n  enabled: false\n  retries: 10\n"
        )
        self.assertIn("on", data)
        self.assertEqual(data["with"]["manylinux"], "2_17")
        self.assertIs(data["with"]["enabled"], False)
        self.assertEqual(data["with"]["retries"], 10)

    def test_decimal_integers_do_not_become_octal_or_sexagesimal(self):
        self.assertEqual(load("value: 0123")["value"], 123)
        self.assertEqual(load("value: 1:20")["value"], "1:20")


if __name__ == "__main__":
    unittest.main()
