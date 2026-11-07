#!/usr/bin/env python3
"""Replay the pinned fork's original object-state source fixtures unchanged."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import textwrap
from pathlib import Path

from lsp_acceptance import Client


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def fixtures(source: str) -> tuple[str, dict[str, str]]:
    records = re.search(r'"/src/records.py",\s*r#"(.*?)"#', source, re.S)
    assert records, "original records fixture moved"
    blocks = re.split(r"#\[test\]\s*fn (\w+)\(\)", source)
    cases = {}
    for name, block in zip(blocks[1::2], blocks[2::2], strict=True):
        found = re.findall(r'r#"(.*?)"#', block, re.S)
        if found:
            assert len(found) == 1, (name, "original source changed")
            cases[name] = textwrap.dedent(found[0]).strip() + "\n"
    assert len(cases) == 15, "audit original source fixture changes"
    cases["long_straight_line_history"] = (
        "from typing_extensions import assert_type\n"
        "from records import Record\nrecord = Record()\n"
        + "".join(f"value_{index} = {index}\n" for index in range(2000))
        + "assert_type(record.key, None)\nrecord.set_key()\n"
        "assert_type(record.key, int)\n"
    )
    return textwrap.dedent(records[1]).strip() + "\n", cases


def manifest() -> dict:
    return {
        "id": "state",
        "name": "Original object-state fixtures",
        "version": "0.0.0",
        "protocol-version": {"major": 0, "minor": 7},
        "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
        "runtime": {"kind": "monty", "artifact": "plugin.py"},
        "capabilities": {"call-state": True},
        "claims": {
            "functions": [
                {"qualified-name": f"records.{name}"}
                for name in ("load", "shared", "touch")
            ],
            "methods": [
                {
                    "kind": "on-subclass-of",
                    "base-qualified-name": "records.Record",
                    "method-name": name,
                }
                for name in ("set_key", "clear_key")
            ],
            "constructors": [
                {"kind": "exact", "qualified-name": "records.Record"},
                {"kind": "subclass-of", "base-qualified-name": "records.Record"},
            ],
        },
    }


PLUGIN = """
@on_call_state
def state(request):
    name = request["callee"]["expression"]
    receiver = {}
    result = {}
    fresh = False
    preserve = True
    if name in ["records.Record", "records.Child"]:
        annotation = "None"
        for argument in request.get("arguments", []):
            if (argument.get("type-expr") or {}).get("expression", "None") != "None":
                annotation = "int"
        result = {"key": type_annotation(annotation), "pk": type_annotation(annotation)}
        fresh = True
    elif name in ["records.load", "records.shared"]:
        result = {"key": type_annotation("int"), "pk": type_annotation("int")}
        fresh = name == "records.load"
        preserve = fresh
    elif name == "records.touch":
        preserve = False
    elif name in ["records.Record.set_key", "records.Record.clear_key"]:
        annotation = "int" if name.endswith("set_key") else "None"
        receiver = {"key": type_annotation(annotation), "pk": type_annotation(annotation)}
    else:
        return None
    return {"kind": "call-state-patch", "receiver-members": receiver,
            "result-members": result, "fresh-result": fresh,
            "preserves-other-objects": preserve}
"""


def main() -> int:
    parser = argparse.ArgumentParser()
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    repo = Path(__file__).resolve().parents[2]
    original = repo / "ruff/crates/ty_python_semantic/src/types/object_state.rs"
    records, cases = fixtures(original.read_text(encoding="utf-8"))
    environment = {**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"}
    result = {
        "passed": False,
        "original_source": str(original),
        "original_source_sha256": digest(original),
        "binaries": {
            name: {
                "path": str(getattr(args, name)),
                "sha256": digest(getattr(args, name)),
            }
            for name in ("companion", "official", "fork")
        },
        "cases": [],
        "failure_contract": "plugin_error_differential.py",
    }

    def setup(root: Path, enabled: bool = True) -> None:
        root.mkdir()
        (root / "records.py").write_text(records, encoding="utf-8")
        write(root / "plugin.json", manifest())
        (root / "plugin.py").write_text(
            "set_manifest(" + repr(manifest()) + ")\n" + PLUGIN, encoding="utf-8"
        )
        config = '[environment]\npython-version="3.13"\n'
        if enabled:
            config += (
                "[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\n"
                'id="state"\nruntime="monty"\ntrusted=true\n'
                'path="plugin.py"\nmanifest-path="plugin.json"\n'
            )
        (root / "ty.toml").write_text(config, encoding="utf-8")

    for name, source in cases.items():
        root = args.output / name
        setup(root, name != "disabled_state_hook_leaves_declared_members")
        (root / "main.py").write_text(source, encoding="utf-8")
        row = {"name": name, "source_sha256": digest(root / "main.py"), "runs": []}
        for backend, command in (
            ("fork", [str(args.fork)]),
            ("companion", [str(args.companion)]),
        ):
            command += [
                "check",
                "main.py",
                "--project",
                ".",
                "--output-format",
                "gitlab",
            ]
            if backend == "companion":
                command += ["--ty", str(args.official)]
            completed = subprocess.run(
                command, cwd=root, env=environment, capture_output=True
            )
            (root / f"{backend}.stdout").write_bytes(completed.stdout)
            (root / f"{backend}.stderr").write_bytes(completed.stderr)
            passed = completed.returncode == 0 and json.loads(completed.stdout) == []
            row["runs"].append(
                {"backend": backend, "exit": completed.returncode, "passed": passed}
            )
            write(
                args.output / "summary.json",
                result | {"cases": result["cases"] + [row]},
            )
        result["cases"].append(row)

    root = args.output / "state_changes_after_an_edit"
    setup(root)
    document = root / "main.py"
    document.write_text("", encoding="utf-8")
    edit_row = {"name": "state_changes_after_an_edit", "runs": []}
    for backend, command in (
        ("fork", [str(args.fork), "server"]),
        ("companion", [str(args.companion), "server", "--ty", str(args.official)]),
    ):
        client = Client(command, root, root / f"{backend}.stderr")
        try:
            client.request(
                "initialize",
                {
                    "rootUri": root.as_uri(),
                    "capabilities": {"textDocument": {"diagnostic": {}}},
                },
            )
            client.notify("initialized")
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": document.as_uri(),
                        "version": 0,
                        "languageId": "python",
                        "text": "",
                    }
                },
            )
            reports = []
            for version, (constructor, annotation) in enumerate(
                (("Record()", "None"), ("Record(key=1)", "int"), ("Record()", "None")),
                1,
            ):
                source = f"from typing_extensions import assert_type\nfrom records import Record\nrecord = {constructor}\nassert_type(record.key, {annotation})\n"
                client.notify(
                    "textDocument/didChange",
                    {
                        "textDocument": {"uri": document.as_uri(), "version": version},
                        "contentChanges": [{"text": source}],
                    },
                )
                report = client.request(
                    "textDocument/diagnostic",
                    {"textDocument": {"uri": document.as_uri()}},
                )
                reports.append({"source": source, "report": report})
                write(root / f"{backend}.json", {"reports": reports})
                assert report["items"] == [], (backend, report)
            edit_row["runs"].append(
                {"backend": backend, "passed": True, "revisions": len(reports)}
            )
        finally:
            client.close()
    result["cases"].append(edit_row)
    result["passed"] = all(
        run["passed"] for case in result["cases"] for run in case["runs"]
    )
    write(args.output / "summary.json", result)
    assert result["passed"], (
        f"Original object-state failures; see {args.output / 'summary.json'}"
    )
    print(
        f"Passed {len(result['cases'])} original object-state contracts on fork and companion"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
