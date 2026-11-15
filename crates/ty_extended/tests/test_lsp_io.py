"""Editor fixtures remain UTF-8 when Python uses a legacy Windows locale."""

from __future__ import annotations

import io
import json
import queue
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
    def read_protocol_stream(self, stream):
        client = lsp_acceptance.Client.__new__(lsp_acceptance.Client)
        client.process = SimpleNamespace(stdout=stream)
        client.messages = queue.Queue()
        client._read()
        return client.messages

    def test_truncated_frame_is_eof_for_the_startup_exit_check(self):
        for body in (b"", b'{"id":'):
            with self.subTest(body=body):
                messages = self.read_protocol_stream(
                    io.BytesIO(b"Content-Length: 30\r\n\r\n" + body)
                )
                self.assertIsInstance(messages.get_nowait(), EOFError)

    def test_complete_invalid_json_remains_a_protocol_failure(self):
        messages = self.read_protocol_stream(
            io.BytesIO(b"Content-Length: 8\r\n\r\nnot json")
        )
        self.assertIsInstance(messages.get_nowait(), json.JSONDecodeError)

    def test_fragmented_utf8_frame_is_read_completely(self):
        class Fragmented(io.BytesIO):
            def read(self, size=-1):
                return super().read(min(size, 2))

        expected = {"id": 1, "result": "😀"}
        payload = json.dumps(expected, ensure_ascii=False).encode()
        messages = self.read_protocol_stream(
            Fragmented(f"Content-Length: {len(payload)}\r\n\r\n".encode() + payload)
        )
        self.assertEqual(messages.get_nowait(), expected)
        self.assertIsInstance(messages.get_nowait(), EOFError)

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
