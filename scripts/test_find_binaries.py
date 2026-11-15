"""Check installed command discovery without relying on the developer's PATH."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "find_binary",
    Path(__file__).resolve().parents[1] / "python/ty_extended/_find_ty.py",
)
finder = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(finder)


class BinaryLookupTests(unittest.TestCase):
    def test_primary_and_legacy_are_distinct_and_never_use_official_ty(self):
        for suffix in ("", ".exe"):
            with (
                self.subTest(suffix=suffix),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                with (
                    patch.object(finder.sysconfig, "get_path", return_value=directory),
                    patch.object(
                        finder.sysconfig, "get_config_var", return_value=suffix
                    ),
                    patch.object(finder, "_module_path", return_value=None),
                ):
                    (root / ("ty" + suffix)).touch()
                    for lookup in (finder.find_tyx_bin, finder.find_ty_extended_bin):
                        with self.assertRaises(finder.TyNotFound):
                            lookup()
                    (root / ("tyx" + suffix)).touch()
                    self.assertEqual(
                        finder.find_tyx_bin(), str(root / ("tyx" + suffix))
                    )
                    with self.assertRaises(finder.TyNotFound):
                        finder.find_ty_extended_bin()
                    (root / ("ty-extended" + suffix)).touch()
                    self.assertEqual(
                        finder.find_ty_extended_bin(),
                        str(root / ("ty-extended" + suffix)),
                    )


if __name__ == "__main__":
    unittest.main()
