#!/usr/bin/env python3
"""Exercise original SDK function summaries through real native/plugin checkers."""

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


PLUGIN = """
@on_project_index
def index(request):
    functions = {value["qualified-name"]: value for value in request.get("functions", [])}
    problems = []
    for expected in ["entries.handle_alpha", "entries.helper", "entries.outer", "entries.conditional_entry", "marking.mark"]:
        if expected not in functions:
            problems.append("missing " + expected)
    for excluded in ["entries.nested", "entries.SomeClass.method_def"]:
        if excluded in functions:
            problems.append("unexpected " + excluded)
    handle = functions.get("entries.handle_alpha", {})
    decorators = handle.get("decorators", [])
    if len(decorators) != 1 or decorators[0].get("callee", {}).get("qualified-name") != "mark":
        problems.append("decorator symbol")
    else:
        actual = [arg.get("value") for arg in decorators[0].get("arguments", [])]
        expected = [{"kind":"symbol-ref", "qualified-name":"Alpha"}, {"kind":"symbol-ref", "qualified-name":"Beta"}]
        if actual != expected:
            problems.append("decorator arguments")
    if "Token" not in (handle.get("inferred-type") or {}).get("expression", ""):
        problems.append("decorator-applied type")
    if (handle.get("inferred-type") or {}).get("snapshot", {}).get("qualified-name") != "marking.Token":
        problems.append("decorator-applied nominal identity")
    helper = functions.get("entries.helper", {})
    if helper.get("decorators") or "-> int" not in (helper.get("inferred-type") or {}).get("expression", ""):
        problems.append("callable type")
    for function in functions.values():
        if function.get("source", {}).get("qualified-name") != function["qualified-name"]:
            problems.append("source identity")
    if request["context"]["config"].get("compound", False):
        for name in ["in_if", "in_else", "in_while", "in_while_else", "in_for", "in_for_else", "in_with", "in_try", "in_except", "in_try_else", "in_finally", "in_match", "documented"]:
            if "entries." + name not in functions:
                problems.append("missing compound " + name)
        duplicate = [f for f in request["functions"] if f["qualified-name"] == "entries.duplicate"]
        if len(duplicate) != 2 or "-> int" not in (duplicate[0].get("inferred-type") or {}).get("expression", "") or "-> str" not in (duplicate[1].get("inferred-type") or {}).get("expression", ""):
            problems.append("per-definition binding type")
        documented = (functions.get("entries.documented", {}).get("inferred-type") or {}).get("expression", "")
        if "-> bytes" not in documented or "documentation" in documented:
            problems.append("hover documentation leaked into type")
    return project_index(plugin_index={"valid": not problems, "problems": problems})

@on_call_return
def check(request):
    data = request.get("project-index", {})
    return call_return_patch(type_annotation("typing.Literal[True]" if data.get("valid") else "bool"))
"""

COMPOUND = '''
from contextlib import nullcontext
from typing import TYPE_CHECKING
if TYPE_CHECKING:
    def in_if() -> int: return 0
else:
    def in_else() -> int: return 0
while False:
    def in_while() -> int: return 0
else:
    def in_while_else() -> int: return 0
for value in []:
    def in_for() -> int: return 0
else:
    def in_for_else() -> int: return 0
with nullcontext():
    def in_with() -> int: return 0
try:
    def in_try() -> int: return 0
except Exception:
    def in_except() -> int: return 0
else:
    def in_try_else() -> int: return 0
finally:
    def in_finally() -> int: return 0
match 1:
    case 1:
        def in_match() -> int: return 0
if TYPE_CHECKING:
    def duplicate() -> int: return 0
else:
    def duplicate() -> str: return ""
def documented() -> bytes:
    """Some documentation with ``` fences and non-ASCII: café."""
    return b""
'''


def main() -> int:
    parser = argparse.ArgumentParser()
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    repo = Path(__file__).resolve().parents[2]
    original = repo / "ruff/crates/ty_python_semantic/src/types/class/static_literal.rs"
    block = original.read_text(encoding="utf-8").split(
        "fn project_index_includes_module_level_functions()", 1
    )[1]
    sources = {
        name: textwrap.dedent(raw).strip() + "\n"
        for name, raw in re.findall(r'"/src/([^"]+)",\s*r#"(.*?)"#', block, re.S)
    }
    assert set(sources) == {"marking.py", "things.py", "entries.py"}
    manifest = {
        "id": "index-recorder",
        "name": "Original function summaries",
        "version": "0.0.0",
        "protocol-version": {"major": 0, "minor": 7},
        "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
        "runtime": {"kind": "monty", "artifact": "plugin.py"},
        "capabilities": {"project-index": True, "call-return": True},
        "claims": {"functions": [{"qualified-name": "audit.check"}]},
    }
    (args.output / "plugin.json").write_text(json.dumps(manifest), encoding="utf-8")
    (args.output / "plugin.py").write_text(
        "set_manifest(" + repr(manifest) + ")\n" + PLUGIN, encoding="utf-8"
    )
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
    }
    environment = {**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"}
    for compound in (False, True):
        root = args.output / ("compound" if compound else "original")
        root.mkdir()
        for name, text in sources.items():
            (root / name).write_text(
                text + (COMPOUND if compound and name == "entries.py" else ""),
                encoding="utf-8",
            )
        (root / "audit.py").write_text(
            "def check() -> bool: return False\n", encoding="utf-8"
        )
        main_source = "from typing import assert_type, Literal\nfrom audit import check\nassert_type(check(), Literal[True])\n"
        main_path = root / "main.py"
        main_path.write_text(main_source, encoding="utf-8")
        (root / "ty.toml").write_text(
            '[environment]\npython-version="3.13"\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid="index-recorder"\nruntime="monty"\ntrusted=true\npath="../plugin.py"\nmanifest-path="../plugin.json"\n[plugins.plugin.config]\ncompound='
            + str(compound).lower()
            + "\n",
            encoding="utf-8",
        )
        for backend, binary in (("fork", args.fork), ("companion", args.companion)):
            command = [
                str(binary),
                "check",
                "--project",
                str(root),
                "--output-format",
                "gitlab",
            ]
            if backend == "companion":
                command += ["--ty", str(args.official)]
            process = subprocess.run(
                command, cwd=root, env=environment, capture_output=True
            )
            (root / f"{backend}.stdout").write_bytes(process.stdout)
            (root / f"{backend}.stderr").write_bytes(process.stderr)
            result["cases"].append(
                {
                    "compound": compound,
                    "backend": backend,
                    "transport": "cli",
                    "exit": process.returncode,
                    "passed": process.returncode == 0
                    and json.loads(process.stdout) == [],
                }
            )
            for markup in ("plaintext", "markdown"):
                client = Client(
                    [str(binary), "server"]
                    + (["--ty", str(args.official)] if backend == "companion" else []),
                    root,
                    root / f"{backend}-{markup}.stderr",
                )
                try:
                    client.request(
                        "initialize",
                        {
                            "rootUri": root.as_uri(),
                            "capabilities": {
                                "textDocument": {
                                    "hover": {"contentFormat": [markup]},
                                    "diagnostic": {},
                                }
                            },
                        },
                    )
                    client.notify("initialized")
                    # Native editor project-index summaries include files in
                    # its active diagnostic scope. Open every audited source.
                    for document in sorted(root.glob("*.py")):
                        client.notify(
                            "textDocument/didOpen",
                            {
                                "textDocument": {
                                    "uri": document.as_uri(),
                                    "version": 1,
                                    "languageId": "python",
                                    "text": document.read_text(encoding="utf-8"),
                                }
                            },
                        )
                    diagnostics = client.request(
                        "textDocument/diagnostic",
                        {"textDocument": {"uri": main_path.as_uri()}},
                    )
                    (root / f"{backend}-{markup}.json").write_text(
                        json.dumps(diagnostics, indent=2) + "\n", encoding="utf-8"
                    )
                    result["cases"].append(
                        {
                            "compound": compound,
                            "backend": backend,
                            "transport": markup,
                            "passed": diagnostics.get("items") == [],
                        }
                    )
                    entries = root / "entries.py"
                    text = entries.read_text(encoding="utf-8")
                    changed = text.replace(
                        "def helper() -> int:\n    return 0",
                        'def helper() -> str:\n    return "changed"',
                    )
                    assert changed != text
                    for version, content, expected in (
                        (2, changed, ["type-assertion-failure"]),
                        (3, text, []),
                    ):
                        client.notify(
                            "textDocument/didChange",
                            {
                                "textDocument": {
                                    "uri": entries.as_uri(),
                                    "version": version,
                                },
                                "contentChanges": [{"text": content}],
                            },
                        )
                        diagnostics = client.request(
                            "textDocument/diagnostic",
                            {"textDocument": {"uri": main_path.as_uri()}},
                        )
                        (
                            root / f"{backend}-{markup}-revision-{version}.json"
                        ).write_text(
                            json.dumps(diagnostics, indent=2) + "\n", encoding="utf-8"
                        )
                        codes = [
                            item.get("code") for item in diagnostics.get("items", [])
                        ]
                        result["cases"].append(
                            {
                                "compound": compound,
                                "backend": backend,
                                "transport": markup,
                                "revision": version,
                                "codes": codes,
                                "passed": codes == expected,
                            }
                        )
                finally:
                    client.close()
        (args.output / "summary.json").write_text(
            json.dumps(result, indent=2) + "\n", encoding="utf-8"
        )
    result["passed"] = all(row["passed"] for row in result["cases"])
    (args.output / "summary.json").write_text(
        json.dumps(result, indent=2) + "\n", encoding="utf-8"
    )
    assert result["passed"], result["cases"]
    print(json.dumps({"passed": True, "checks": len(result["cases"])}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
