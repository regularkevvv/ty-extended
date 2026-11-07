#!/usr/bin/env python3
"""Replay original method-pattern SDK fixtures, including routed request records."""

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


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


PLUGIN = """
def trace(request):
    hook = "signature" if request["kind"] == "adjust-call-signature" else "return"
    receiver = (request.get("receiver") or {}).get("nominal-class", "")
    record = PLUGIN_ID + ":" + hook + ":" + request["callee"]["expression"] + "@" + receiver
    print("SDK_CALL|" + record + "|")
    return [{"id":"routing-trace", "message": record, "severity":"info"}]
@on_call_return
def returned(request):
    return call_return_patch(type_annotation("int"), diagnostics=trace(request))
@on_call_signature
def signature(request):
    assert request["callee"]["expression"] in ["abcs.Base.run_a", "abcs.Base.run_b"]
    assert request["receiver"]["nominal-class"] == "abcs.Sub"
    return call_signature_patch({"parameters":[{"name":"token","kind":"positional-or-keyword","type-expr":type_annotation("int"),"required":True}],"return-type":type_annotation("int")},diagnostics=trace(request))
"""


def claim(base, pattern, exact=False):
    return {
        "kind": "on-subclass-of" if exact else "on-subclass-of-matching",
        "base-qualified-name": base,
        "method-name" if exact else "method-name-pattern": pattern,
    }


CASES = {
    "pattern_claim_routes_all_methods_on_subclasses": (
        [("runner", False, [claim("abcs.Base", "*")])],
        4,
        [
            ("runner", "return", "run_a", "Sub"),
            ("runner", "return", "run_b", "Sub"),
            ("runner", "return", "stop", "Sub"),
            ("runner", "return", "run_a", "DeeperSub"),
        ],
    ),
    "pattern_claim_prefix_and_first_plugin_wins": (
        [
            ("plugin-a", False, [claim("abcs.Base", "run_*")]),
            ("plugin-b", False, [claim("abcs.Base", "*")]),
        ],
        0,
        [
            ("plugin-a", "return", "run_a", "Sub"),
            ("plugin-a", "return", "run_b", "Sub"),
            ("plugin-b", "return", "stop", "Sub"),
        ],
    ),
    "pattern_claim_earlier_plugin_shadows_later": (
        [
            ("broad", False, [claim("abcs.Base", "*")]),
            ("narrow", False, [claim("abcs.Base", "run_*")]),
        ],
        0,
        [("broad", "return", "run_a", "Sub"), ("broad", "return", "stop", "Sub")],
    ),
    "exact_and_pattern_claims_coexist": (
        [
            ("exact", False, [claim("abcs.Base", "stop", True)]),
            ("pattern", False, [claim("abcs.Base", "run_a")]),
        ],
        0,
        [("exact", "return", "stop", "Sub"), ("pattern", "return", "run_a", "Sub")],
    ),
    "pattern_claim_routes_call_signature_hook": (
        [("sig-hooks", True, [claim("abcs.Base", "run_*")])],
        2,
        [
            ("sig-hooks", "signature", "run_a", "Sub"),
            ("sig-hooks", "signature", "run_b", "Sub"),
            ("sig-hooks", "signature", "run_a", "Sub"),
        ],
    ),
    "pattern_claim_on_unresolvable_base_never_routes": (
        [("missing-base", False, [claim("missing.Base", "*")])],
        0,
        [],
    ),
    "empty_pattern_claim_never_routes": (
        [("empty-pattern", False, [claim("abcs.Base", "")])],
        0,
        [],
    ),
    "pattern_claim_executes_once_per_call_site": (
        [("stress", False, [claim("abcs.Base", "*")])],
        0,
        [
            ("stress", "return", ("run_a", "run_b", "stop")[i % 3], "Sub")
            for i in range(200)
        ],
    ),
}


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
    fixture = re.search(r'fn write_claim_fixture\(.*?r#"(.*?)"#', raw, re.S)
    assert fixture
    abcs = textwrap.dedent(fixture[1]).strip() + "\n"
    report = {
        "passed": False,
        "artifact": str(args.companion),
        "sha256": digest(args.companion),
        "original_source": str(original),
        "original_source_sha256": digest(original),
        "source_sha256": digest(Path(__file__)),
        "fork_sha256": digest(args.fork),
        "official_sha256": digest(args.official),
        "cases": [],
        "adapter": "Original Rust request recorder transported through info diagnostics; Python source unchanged after original write_dedented normalization.",
    }
    try:
        for name, (plugins, error_count, expected_records) in CASES.items():
            case = args.output / name
            root = case / "project"
            root.mkdir(parents=True)
            block = re.search(
                r"fn " + name + r"\(\).*?(?=\n    #\[test\]|\Z)", raw, re.S
            )
            assert block, name
            if name == "pattern_claim_executes_once_per_call_site":
                source = (
                    "from abcs import Sub\nsub = Sub()\n"
                    + "\n".join(
                        f"v{i}: int = sub.{('run_a', 'run_b', 'stop')[i % 3]}()"
                        for i in range(200)
                    )
                    + "\n"
                )
            else:
                strings = re.findall(r'r#"(.*?)"#', block[0], re.S)
                assert len(strings) == 1, (name, len(strings))
                source = textwrap.dedent(strings[0]).strip() + "\n"
            (root / "abcs.py").write_text(abcs, encoding="utf-8")
            (root / "main.py").write_text(source, encoding="utf-8")
            config = '[environment]\npython-version="3.13"\n[plugins]\nenabled=true\nauto-discover=false\n'
            for ident, signature, claims in plugins:
                value = {
                    "id": ident,
                    "name": ident,
                    "version": "0.0.1",
                    "protocol-version": {"major": 0, "minor": 7},
                    "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
                    "runtime": {"kind": "monty", "artifact": ident + ".py"},
                    "capabilities": {
                        "call-signature" if signature else "call-return": True
                    },
                    "claims": {"methods": claims},
                }
                save(case / (ident + ".json"), value)
                (case / (ident + ".py")).write_text(
                    "PLUGIN_ID="
                    + repr(ident)
                    + "\nset_manifest("
                    + repr(value)
                    + ")\n"
                    + PLUGIN,
                    encoding="utf-8",
                )
                config += (
                    "[[plugins.plugin]]\nid="
                    + json.dumps(ident)
                    + '\nruntime="monty"\ntrusted=true\npath='
                    + json.dumps("../" + ident + ".py")
                    + "\nmanifest-path="
                    + json.dumps("../" + ident + ".json")
                    + "\n"
                )
            (root / "ty.toml").write_text(config, encoding="utf-8")
            save(case / "selected.json", [str(root / "main.py")])
            row = {
                "name": name,
                "source_sha256": digest(root / "main.py"),
                "records": [],
                "runs": [],
            }
            report["cases"].append(row)
            expected = None
            for backend in ("fork", "companion", "project-cli", "project-push"):
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
                        "--explicit-files",
                        json.dumps([str(root / "main.py")]),
                        "--files",
                        str(case / "selected.json"),
                    ]
                else:
                    command = [
                        str(args.fork if backend == "fork" else args.companion),
                        "check",
                        "main.py",
                        "--project",
                        ".",
                        "--output-format",
                        "gitlab",
                    ]
                    if backend == "companion":
                        command += ["--ty", str(args.official)]
                    else:
                        command += ["-vv"]
                result = subprocess.run(
                    command,
                    cwd=root,
                    capture_output=True,
                    encoding="utf-8",
                    timeout=180,
                    env={**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"},
                )
                save(
                    case / (backend + ".command.json"),
                    {"argv": command, "cwd": str(root)},
                )
                (case / (backend + ".stdout")).write_text(
                    result.stdout, encoding="utf-8"
                )
                (case / (backend + ".stderr")).write_text(
                    result.stderr, encoding="utf-8"
                )
                assert result.returncode in (0, 1), (command, result.stderr)
                value = json.loads(result.stdout)
                metrics = value if isinstance(value, dict) else None
                items = value["diagnostics"] if metrics else value
                items = diagnostics(items, root)
                records = [
                    item for item in items if "routing-trace" in item["description"]
                ]
                records.sort(
                    key=lambda item: (
                        item["location"]["positions"]["begin"]["line"],
                        item["location"]["positions"]["begin"]["column"],
                    )
                )
                observed = [
                    item["description"]
                    .removeprefix("plugin-configuration: ")
                    .removesuffix(": routing-trace")
                    for item in records
                ]
                wanted = [
                    f"{ident}:{hook}:abcs.Base.{method}@abcs.{receiver}"
                    for ident, hook, method, receiver in expected_records
                ]
                if backend == "fork":
                    traced = re.findall(r"SDK_CALL\|([^|]+)\|", result.stderr)
                    assert traced == wanted, (
                        name,
                        backend,
                        "native execution records",
                        traced,
                        wanted,
                    )
                emitted = (
                    [] if name == "pattern_claim_routes_call_signature_hook" else wanted
                )
                assert observed == emitted, (name, backend, observed, emitted)
                errors = [item for item in items if item["severity"] == "major"]
                assert len(errors) == error_count, (name, backend, errors)
                if expected is None:
                    expected = items
                assert items == expected, (name, backend, items, expected)
                if metrics and name == "pattern_claim_executes_once_per_call_site":
                    assert metrics["monty_executions"] == 3, metrics
                    assert metrics["monty_cache_hits"] >= 197, metrics
                elif metrics and name != "pattern_claim_routes_call_signature_hook":
                    assert metrics["monty_executions"] == len(wanted), (
                        name,
                        backend,
                        metrics["monty_executions"],
                        len(wanted),
                    )
                row["runs"].append(
                    {
                        "backend": backend,
                        "passed": True,
                        "exit_code": result.returncode,
                        "diagnostics": len(items),
                        "monty_executions": metrics["monty_executions"]
                        if metrics
                        else None,
                        "monty_cache_hits": metrics["monty_cache_hits"]
                        if metrics
                        else None,
                    }
                )
                row["records"] = observed
                if name == "pattern_claim_executes_once_per_call_site":
                    row["execution_contract"] = {
                        "native": "One hook execution per original call site (200).",
                        "companion": "All 200 source-local effects preserved; immutable identical requests reuse three responses across projection passes.",
                        "literal_execution_count_equal": False,
                    }
            assert digest(root / "main.py") == row["source_sha256"]
        assert digest(args.companion) == report["sha256"]
        report["passed"] = True
    finally:
        save(args.output / "summary.json", report)
    print(
        json.dumps(
            {
                "passed": True,
                "cases": len(report["cases"]),
                "runs": sum(len(case["runs"]) for case in report["cases"]),
                "summary": str(args.output / "summary.json"),
            }
        )
    )


if __name__ == "__main__":
    main()
