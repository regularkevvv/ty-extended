#!/usr/bin/env python3
"""Replay original SDK class-summary contracts 154-155 with unchanged Python."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import textwrap
from pathlib import Path

from plugin_configuration_differential import diagnostics

PLUGIN = """
@on_project_index
def index(request):
    classes = {item["qualified-name"]: item for item in request.get("classes", [])}
    cls = classes.get("models.Book", {})
    errors = []
    def require(condition, message):
        if not condition:
            errors.append(message)
    require(cls.get("qualified-name") == "models.Book", "qualified class name")
    if CASE == 154:
        require(len(cls.get("decorators", [])) == 1, "decorator count")
        require(cls.get("metaclass") is not None, "metaclass summary")
        fields = {field["name"]: field for field in cls.get("fields", [])}
        title = fields.get("title", {}).get("assigned-value", {})
        require(title.get("kind") == "call", "field call kind")
        require(title.get("callee", {}).get("qualified-name") == "CharField", "unresolved CharField callee")
        arguments = title.get("arguments", [])
        require(len(arguments) == 2, "title argument count")
        if len(arguments) == 2:
            require(arguments[0].get("name") == "max_length" and arguments[0].get("value") == {"kind":"int","value":100}, "max_length literal")
            require(arguments[1].get("name") == "null" and arguments[1].get("value") == {"kind":"bool","value":True}, "null literal")
        author = fields.get("author", {}).get("assigned-value", {})
        require(author.get("callee", {}).get("qualified-name") == "ForeignKey", "unresolved ForeignKey callee")
        arguments = author.get("arguments", [])
        require(len(arguments) == 3, "author argument count")
        if len(arguments) == 3:
            require(arguments[0].get("value") == {"kind":"str","value":"library.Author"}, "relation target")
            require(arguments[1].get("name") == "related_name" and arguments[1].get("value") == {"kind":"str","value":"books"}, "relation name")
            require(arguments[2].get("name") == "on_delete" and arguments[2].get("value") == {"kind":"enum-ref","qualified-name":"models.CASCADE"}, "enum-ref identity")
        typed = fields.get("typed", {})
        require(typed.get("annotation", {}).get("expression") == "int", "typed annotation")
        require(typed.get("assigned-value") == {"kind":"literal","value":{"kind":"int","value":1}}, "typed literal assignment")
    else:
        source = cls.get("source", {})
        require(source.get("qualified-name") == "models.Book", "source qualified name")
        require(source.get("module") == "models", "source module")
        require(source.get("start") is not None, "source position")
        constants = {item["name"]:item["value"] for item in cls.get("class-constants", [])}
        require(constants.get("STATUS") == {"kind":"str","value":"draft"}, "class constant")
        nested = {item["name"]:item for item in cls.get("nested-classes", [])}
        meta = nested.get("Meta", {})
        require(meta.get("qualified-name") == "models.Book.Meta", "nested qualified name")
        constants = {item["name"]:item["value"] for item in meta.get("class-constants", [])}
        require(constants.get("abstract") == {"kind":"bool","value":True}, "abstract constant")
        require(constants.get("app_label") == {"kind":"str","value":"library"}, "app_label constant")
    return project_index(plugin_index={"errors":errors})

@on_call_return
def check(request):
    errors = request.get("project-index", {}).get("errors", ["missing index"])
    return call_return_patch(type_annotation("typing.Literal[True]" if not errors else "typing.Literal[False]"), diagnostics=[{"id":"summary-issue","message":message,"severity":"error"} for message in errors])
"""


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser()
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    for name in ("companion", "official", "fork", "output"):
        setattr(args, name, getattr(args, name).resolve())
    args.output.mkdir(parents=True)
    original = (
        Path(__file__).resolve().parents[2]
        / "ruff/crates/ty_python_semantic/src/types/class/static_literal.rs"
    )
    raw = original.read_text(encoding="utf-8")
    report = {
        "passed": False,
        "artifact": str(args.companion),
        "sha256": digest(args.companion),
        "original_source_sha256": digest(original),
        "source_sha256": digest(Path(__file__)),
        "fork_sha256": digest(args.fork),
        "official_sha256": digest(args.official),
        "cases": [],
    }
    try:
        for number, name in [
            (154, "plugin_class_summary_includes_django_like_assignment_calls"),
            (155, "plugin_class_summary_includes_nested_meta_constants_and_source"),
        ]:
            match = re.search(r"fn " + name + r'\(\).*?r#"(.*?)"#', raw, re.S)
            assert match
            text = textwrap.dedent(match[1]).strip() + "\n"
            case = args.output / str(number)
            root = case / "project"
            root.mkdir(parents=True)
            (root / "models.py").write_text(text, encoding="utf-8")
            (root / "audit.py").write_text(
                "def check() -> bool: return False\n", encoding="utf-8"
            )
            (root / "main.py").write_text(
                "from typing import assert_type, Literal\nfrom audit import check\nassert_type(check(), Literal[True])\n",
                encoding="utf-8",
            )
            value = {
                "id": "original.class-summary",
                "name": "Original class summary",
                "version": "0.0.0",
                "protocol-version": {"major": 0, "minor": 7},
                "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
                "runtime": {"kind": "monty", "artifact": "plugin.py"},
                "capabilities": {"project-index": True, "call-return": True},
                "claims": {"functions": [{"qualified-name": "audit.check"}]},
            }
            save(case / "plugin.json", value)
            (case / "plugin.py").write_text(
                "CASE="
                + str(number)
                + "\nset_manifest("
                + repr(value)
                + ")\n"
                + PLUGIN,
                encoding="utf-8",
            )
            (root / "ty.toml").write_text(
                '[environment]\npython-version="3.13"\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid="original.class-summary"\nruntime="monty"\ntrusted=true\npath="../plugin.py"\nmanifest-path="../plugin.json"\n',
                encoding="utf-8",
            )
            row = {
                "inventory_id": number,
                "source_sha256": digest(root / "models.py"),
                "runs": [],
            }
            report["cases"].append(row)
            expected = None
            for backend in ["fork", "companion", "project-cli", "project-push"]:
                if backend.startswith("project-"):
                    command = [
                        str(args.companion),
                        "project",
                        str(root),
                        "--ty",
                        str(args.official),
                        "--out",
                        str(case / backend),
                        "--checker-mode",
                        backend.removeprefix("project-"),
                        "--python-version",
                        "3.13",
                    ]
                else:
                    command = [
                        str(args.fork if backend == "fork" else args.companion),
                        "check",
                        "--project",
                        ".",
                        "--output-format",
                        "gitlab",
                    ]
                    if backend == "companion":
                        command += ["--ty", str(args.official)]
                save(case / (backend + ".command.json"), command)
                run = subprocess.run(
                    command,
                    cwd=root,
                    env={**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"},
                    text=True,
                    encoding="utf-8",
                    capture_output=True,
                    timeout=120,
                )
                (case / (backend + ".stdout")).write_text(run.stdout, encoding="utf-8")
                (case / (backend + ".stderr")).write_text(run.stderr, encoding="utf-8")
                assert run.returncode in [0, 1], (number, backend, run.stderr)
                result = json.loads(run.stdout)
                observed = diagnostics(
                    result["diagnostics"] if isinstance(result, dict) else result, root
                )
                save(case / (backend + ".diagnostics.json"), observed)
                assert not any(
                    item["location"]["path"] == "main.py" for item in observed
                ), (number, backend, observed)
                if expected is None:
                    expected = observed
                equal = observed == expected
                row["runs"].append(
                    {
                        "backend": backend,
                        "equal": equal,
                        "exit_code": run.returncode,
                        "diagnostics": len(observed),
                    }
                )
                assert equal, (number, backend, "diagnostic mismatch", str(case))
            assert digest(root / "models.py") == row["source_sha256"]
        assert digest(args.companion) == report["sha256"]
        report["passed"] = True
    finally:
        save(args.output / "summary.json", report)
    print(
        json.dumps(
            {
                "passed": True,
                "cases": len(report["cases"]),
                "summary": str(args.output / "summary.json"),
            }
        )
    )


if __name__ == "__main__":
    main()
