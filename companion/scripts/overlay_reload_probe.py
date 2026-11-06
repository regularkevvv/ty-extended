#!/usr/bin/env python3
"""Observe partial-overlay reloads independently with existing native binaries."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import time
from pathlib import Path

from lsp_acceptance import Client
from sdk_overlay_inventory import CASES, diagnostics, put, setup


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument(f"--{name}", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    result = {"passed": False, "binaries": {}, "runs": []}
    for backend in ("companion", "official", "fork"):
        path = getattr(args, backend).resolve()
        setattr(args, backend, path)
        result["binaries"][backend] = hashlib.sha256(path.read_bytes()).hexdigest()

    def save():
        (args.output / "probe.json").write_text(json.dumps(result, indent=2) + "\n")

    def start(backend, root, label):
        command = [str(getattr(args, backend)), "server"]
        if backend == "companion":
            command += ["--ty", str(args.official)]
        client = Client(command, root, root.parent / f"{label}.stderr")
        client.request(
            "initialize",
            {
                "rootUri": root.as_uri(),
                "workspaceFolders": [{"uri": root.as_uri(), "name": "project"}],
                "capabilities": {
                    "textDocument": {"diagnostic": {}},
                    "workspace": {
                        "didChangeWatchedFiles": {"dynamicRegistration": True}
                    },
                },
            },
        )
        client.notify("initialized")
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": (root / "main.py").as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": (root / "main.py").read_text() + "# unsaved buffer\n",
                }
            },
        )
        return client

    def pulls(client, path, accept):
        deadline = time.monotonic() + 2
        rows = []
        while True:
            items = diagnostics(client, path)
            rows.append(items)
            if accept(items) or time.monotonic() >= deadline:
                return rows
            time.sleep(0.1)

    name, sources, stubs, installed, main_source = CASES[3]
    for backend in ("companion", "fork"):
        for mode in ("normal", "distinct-marker-time", "delay"):
            case = args.output / f"{backend}-{mode}" / name
            root = setup(case, sources, stubs, installed, main_source)
            path = root / "main.py"
            marker = case / "stubs/foo/py.typed"
            initial_stamp = marker.stat().st_mtime_ns
            row = {"backend": backend, "mode": mode, "initial_marker_ns": initial_stamp}
            result["runs"].append(row)
            for checker in ("fork", "companion"):
                command = [str(getattr(args, checker)), "check"]
                if checker == "companion":
                    command += ["--ty", str(args.official)]
                subprocess.run(
                    [
                        *command,
                        "main.py",
                        "--project",
                        ".",
                        "--output-format",
                        "gitlab",
                    ],
                    cwd=root,
                    check=True,
                    capture_output=True,
                )
            client = start(backend, root, "warm")
            try:
                row["initial"] = pulls(client, path, lambda items: items == [])
                marker.unlink()
                client.notify(
                    "workspace/didChangeWatchedFiles",
                    {"changes": [{"uri": marker.as_uri(), "type": 3}]},
                )
                row["removed"] = pulls(client, path, bool)
                if mode == "delay":
                    time.sleep(2.1)
                put(marker, "partial\n")
                if mode == "distinct-marker-time":
                    os.utime(
                        marker,
                        ns=(marker.stat().st_atime_ns, initial_stamp + 2_000_000_000),
                    )
                client.notify(
                    "workspace/didChangeWatchedFiles",
                    {"changes": [{"uri": marker.as_uri(), "type": 1}]},
                )
                row["restored"] = pulls(client, path, lambda items: items == [])
                row["restored_marker_ns"] = marker.stat().st_mtime_ns
                row["normal_events_passed"] = (
                    row["initial"][-1] == []
                    and any(
                        d.get("code") in {"unresolved-import", "import-not-found"}
                        for d in row["removed"][-1]
                    )
                    and row["restored"][-1] == []
                )
                if row["restored"][-1]:
                    client.notify(
                        "workspace/didChangeWatchedFiles",
                        {"changes": [{"uri": marker.parent.as_uri(), "type": 2}]},
                    )
                    row["after_parent_event"] = pulls(
                        client, path, lambda items: items == []
                    )
            finally:
                client.close()
                save()
            if row["restored"][-1]:
                cold = start(backend, root, "cold")
                try:
                    row["after_restart"] = pulls(cold, path, lambda items: items == [])
                finally:
                    cold.close()
                    save()
            print(backend, mode, row["normal_events_passed"], flush=True)
    result["passed"] = all(
        row["normal_events_passed"]
        for row in result["runs"]
        if row["backend"] == "companion"
    )
    save()
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
