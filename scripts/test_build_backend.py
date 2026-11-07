"""Release regressions for standalone wheels and their Python ABI boundary."""

from __future__ import annotations

import base64
import csv
import hashlib
import importlib.util
import io
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

from wheel.wheelfile import WheelFile

SPEC = importlib.util.spec_from_file_location(
    "build_backend", Path(__file__).resolve().parents[1] / "build_backend.py"
)
backend = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(backend)


class BinaryWheelTests(unittest.TestCase):
    def test_an_active_python_dependency_cannot_lose_its_abi(self):
        for dependency in (
            "pyo3",
            "pyo3-ffi",
            "cpython",
            "uniffi",
            "ty_python_semantic",
            "wasmtime",
        ):
            with (
                self.subTest(dependency=dependency),
                patch.object(
                    backend.subprocess,
                    "check_output",
                    return_value=f"ty-extended v0.87.0\n{dependency} v1.0.0\n",
                ),
                self.assertRaises(RuntimeError),
            ):
                backend.audit_binary_graph()

    def test_binary_payload_and_record_survive_abi_correction(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "ty_extended-0.87.0-cp313-cp313-linux_x86_64.whl"
            info = "ty_extended-0.87.0.dist-info"
            binary = "ty_extended-0.87.0.data/scripts/ty-extended"
            payload = b"standalone executable bytes\x00\xff"
            with WheelFile(path, "w") as archive:
                archive.writestr(binary, payload)
                archive.writestr(
                    "ty_extended/plugin_sdk.py",
                    "PROTOCOL_VERSION = {'major': 0, 'minor': 7}\n",
                )
                archive.writestr(
                    info + "/METADATA",
                    "Metadata-Version: 2.4\nName: ty-extended\nVersion: 0.87.0\n",
                )
                archive.writestr(
                    info + "/WHEEL",
                    "Wheel-Version: 1.0\nGenerator: regression\nRoot-Is-Purelib: false\nTag: cp313-cp313-linux_x86_64\n",
                )
            filename = backend.standalone_wheel(path)
            self.assertEqual(filename, "ty_extended-0.87.0-py3-none-linux_x86_64.whl")
            self.assertFalse(path.exists())
            with zipfile.ZipFile(path.parent / filename) as archive:
                self.assertEqual(archive.read(binary), payload)
                self.assertIn(
                    b"Tag: py3-none-linux_x86_64", archive.read(info + "/WHEEL")
                )
                for name, digest, size in csv.reader(
                    io.StringIO(archive.read(info + "/RECORD").decode())
                ):
                    if not digest:
                        continue
                    data = archive.read(name)
                    actual = (
                        base64.urlsafe_b64encode(hashlib.sha256(data).digest())
                        .rstrip(b"=")
                        .decode()
                    )
                    self.assertEqual(digest, "sha256=" + actual)
                    self.assertEqual(int(size), len(data))

    def test_an_extension_library_cannot_be_retagged_as_standalone(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "unsafe.whl"
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr("unsafe.data/scripts/ty-extended", b"binary")
                archive.writestr("ty_extended/extension.so", b"CPython extension")
            with self.assertRaises(RuntimeError):
                backend.standalone_wheel(path)


if __name__ == "__main__":
    unittest.main()
