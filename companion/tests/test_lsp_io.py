"""Editor fixtures remain UTF-8 when Python uses a legacy Windows locale."""

from __future__ import annotations

import io
import sys
import tempfile
import unittest
from contextlib import ExitStack
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import lsp_acceptance
import lsp_differential
import lsp_lifecycle
import lsp_notebook_differential


class FixtureReady(Exception):
    """Stop after real fixture I/O, before starting an external checker."""


class LspIoTests(unittest.TestCase):
    def test_unicode_fixtures_ignore_legacy_default_encoding(self):
        text_encoding = io.text_encoding

        def legacy_encoding(encoding, *args):
            return "cp1252" if encoding is None else text_encoding(encoding, *args)

        with tempfile.TemporaryDirectory() as temporary:
            for name, module, client in (
                ("acceptance", lsp_acceptance, "Client"),
                ("differential", lsp_differential, "DifferentialClient"),
                ("notebook", lsp_notebook_differential, "DifferentialClient"),
                ("lifecycle", lsp_lifecycle, "Client"),
            ):
                with self.subTest(driver=name):
                    output = Path(temporary) / name
                    args = SimpleNamespace(
                        output=output,
                        companion=Path("unused-checker"),
                        official=Path("unused-checker"),
                        fork=Path("unused-checker"),
                    )
                    with ExitStack() as stack:
                        stack.enter_context(
                            patch("io.text_encoding", side_effect=legacy_encoding)
                        )
                        stack.enter_context(
                            patch.object(module, client, side_effect=FixtureReady)
                        )
                        stack.enter_context(
                            patch.object(
                                lsp_notebook_differential,
                                "DEFINITIONS",
                                lsp_notebook_differential.DEFINITIONS
                                + "# Unicode: 😀\n",
                            )
                        )
                        stack.enter_context(self.assertRaises(FixtureReady))
                        if name == "acceptance":
                            module.gate(args, "utf-16", True)
                        elif name == "lifecycle":
                            with patch.object(
                                sys,
                                "argv",
                                [
                                    "lsp_lifecycle.py",
                                    "--companion",
                                    "unused-checker",
                                    "--official",
                                    "unused-checker",
                                    "--output",
                                    str(output),
                                ],
                            ):
                                module.main()
                        else:
                            module.run(args, "utf-16")
                    sources = list(output.rglob("demo.py"))
                    self.assertEqual(len(sources), 1)
                    self.assertIn("😀", sources[0].read_bytes().decode("utf-8"))


if __name__ == "__main__":
    unittest.main()
