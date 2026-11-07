#!/usr/bin/env python3
"""Compare full and scoped Django class inputs and emit native replay requests."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--django", required=True, type=Path)
    parser.add_argument("--oracle", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    sdk = Path(__file__).resolve().parents[2] / "python/ty_extended/plugin_sdk.py"
    plugin = args.django / "python/django_ty/monty.py"
    namespace = {}
    exec(
        compile(sdk.read_text() + "\n" + plugin.read_text(), str(plugin), "exec"),
        namespace,
    )
    projects = classes = batches = 0
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output:
        for line in args.oracle.read_text().splitlines():
            request = json.loads(line)["request"]
            if request["kind"] != "build-project-index":
                continue
            response = namespace["build_django_index"](request)
            scopes = response["class-indexes"]
            before = json.dumps(response, sort_keys=True)
            records = []
            for cls in request["classes"]:
                source = cls.get("source", {})
                context = {
                    key: request["context"][key]
                    for key in ("python-version", "platform", "config")
                }
                context.update(
                    module=source.get(
                        "module", cls["qualified-name"].rsplit(".", 1)[0]
                    ),
                    **{
                        "file-path": source.get("file-path", "/project/models.py"),
                        "speculative": False,
                    },
                )
                call = {
                    "kind": "analyze-class",
                    "context": context,
                    "class": cls,
                    "project-index": response["plugin-index"],
                }
                # CPython supplies the reference; the emitted requests are
                # subsequently replayed in the production Monty host.
                expected = namespace["analyze_django_class"](call)
                call["project-index"] = scopes[cls["qualified-name"]]
                actual = namespace["analyze_django_class"](call)
                if actual != expected:
                    raise RuntimeError(
                        f"Scope lost class inputs: {cls['qualified-name']}"
                    )
                output.write(
                    json.dumps(
                        {"request": call, "expected": expected or {"kind": "no-change"}}
                    )
                    + "\n"
                )
                records.append((call, expected or {"kind": "no-change"}))
                classes += 1
            if "analyze_django_classes" in namespace:
                for width, ordered in [
                    (1, records),
                    (7, list(reversed(records))),
                    (128, records[::2] + records[1::2]),
                ]:
                    for offset in range(0, len(ordered), width):
                        chunk = ordered[offset : offset + width]
                        batch = {
                            "kind": "analyze-classes",
                            "requests": [call for call, _ in chunk],
                        }
                        expected = {
                            "kind": "class-batch",
                            "responses": [expected for _, expected in chunk],
                        }
                        if namespace["analyze_django_classes"](batch) != expected:
                            raise RuntimeError(
                                "Class batching changed an individual result"
                            )
                        output.write(
                            json.dumps({"request": batch, "expected": expected}) + "\n"
                        )
                        batches += 1
            if json.dumps(response, sort_keys=True) != before:
                raise RuntimeError("A class hook modified the shared project index")
            projects += 1
    if projects < 26 or classes < 2579:
        raise RuntimeError(
            f"Regression corpus shrank: {projects} projects, {classes} classes"
        )
    report = {
        "projects": projects,
        "classes": classes,
        "batches": batches,
        "batch_widths": [1, 7, 128] if batches else [],
        "full_and_scoped_equal": True,
        "index_unmodified": True,
        "sha256": {
            str(path): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (sdk, plugin, args.oracle, args.output)
        },
    }
    args.output.with_suffix(".json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
