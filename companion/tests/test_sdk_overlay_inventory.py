"""Watched stub edits must change the revision used by the native checker."""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import sdk_overlay_inventory as inventory


class OverlayInventoryTests(unittest.TestCase):
    def test_same_timestamp_rewrite_changes_content_and_file_revision(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "foo.pyi"
            before_text = "def value() -> int: ...\n"
            after_text = "def value() -> str: ...\n"
            inventory.put(path, before_text)
            before_ns = path.stat().st_mtime_ns
            original_put = inventory.put

            def colliding_write(target: Path, text: str) -> None:
                original_put(target, text)
                os.utime(target, ns=(target.stat().st_atime_ns, before_ns))
                self.assertEqual(target.stat().st_mtime_ns, before_ns)

            with patch.object(inventory, "put", side_effect=colliding_write):
                revision = inventory.put_watched(path, after_text)

            self.assertEqual(len(before_text), len(after_text))
            self.assertEqual(path.read_text(encoding="utf-8"), after_text)
            self.assertEqual(revision["before_ns"], before_ns)
            self.assertEqual(revision["write_ns"], before_ns)
            self.assertNotEqual(revision["final_ns"], before_ns)
            self.assertEqual(revision["final_ns"], path.stat().st_mtime_ns)


if __name__ == "__main__":
    unittest.main()
