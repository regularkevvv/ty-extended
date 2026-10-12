"""Run the unchanged editor harness, retaining every full diagnostic item payload."""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

ROOT = Path(os.environ.get("TY_COMPANION_REPO", Path.cwd())).resolve()
sys.path.insert(0, str(ROOT / "companion/scripts"))
import lsp_django  # noqa: E402 - selected repository harness

OriginalClient = lsp_django.Client
capture = []
update_timings = []


class CapturingClient(OriginalClient):
    def notify(self, method, params=None):
        if method == "textDocument/didChange":
            assert getattr(self, "pending_change", None) is None
            self.pending_change = (time.perf_counter(), params["textDocument"])
        return super().notify(method, params)

    def request(self, method, params=None, *, retries=2):
        depth = getattr(self, "capture_request_depth", 0)
        self.capture_request_depth = depth + 1
        try:
            result = super().request(method, params, retries=retries)
        finally:
            self.capture_request_depth = depth
        if depth == 0 and method == "textDocument/diagnostic":
            pending = getattr(self, "pending_change", None)
            if pending is not None:
                started, document = pending
                assert params["textDocument"]["uri"] == document["uri"]
                version = document["version"]
                semantic = version >= 12
                update_timings.append(
                    {
                        "version": version,
                        "kind": "semantic" if semantic else "comment",
                        "phase": "measured"
                        if version >= (15 if semantic else 5)
                        else "warmup",
                        "seconds": time.perf_counter() - started,
                    }
                )
                self.pending_change = None
            # Keep existing object references; serialization occurs after all measured edits.
            capture.append(
                {
                    "params": params,
                    "items": result.get("items"),
                    "kind": result.get("kind"),
                }
            )
        return result


lsp_django.Client = CapturingClient
try:
    lsp_django.main()
finally:
    destination = Path(os.environ["TY_SPEED_EDITOR_CAPTURE"])
    destination.write_text(json.dumps(capture, indent=2, sort_keys=True) + "\n")
    destination.with_name(destination.stem + "-updates.json").write_text(
        json.dumps(update_timings, indent=2, sort_keys=True) + "\n"
    )
