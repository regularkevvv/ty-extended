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
    projects = classes = 0
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("x") as output:
        for line in args.oracle.read_text().splitlines():
            request = json.loads(line)["request"]
            if request["kind"] != "build-project-index":
                continue
            response = namespace["build_django_index"](request)
            scopes = response["class-indexes"]
            before = json.dumps(response, sort_keys=True)
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
                classes += 1
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
