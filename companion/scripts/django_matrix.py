#!/usr/bin/env python3
"""Run the unchanged Django contract on every declared Python/Django pair."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

import tomllib
from django_acceptance import write_json


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["prepare", "baseline", "compare"])
    parser.add_argument("--django", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument(
        "--interpreters",
        required=True,
        type=Path,
        help="JSON mapping Python minor versions to interpreter paths",
    )
    parser.add_argument("--fork", type=Path)
    parser.add_argument("--companion", type=Path)
    parser.add_argument("--official", type=Path)
    parser.add_argument("--checker-mode", choices=["cli", "push"], default="cli")
    parser.add_argument("--uv", default="uv")
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument(
        "--reuse",
        action="store_true",
        help="Use prepared environments after verifying every pinned dependency; performs no installation",
    )
    args = parser.parse_args()
    args.django = args.django.resolve()
    args.output = args.output.resolve()
    interpreters = json.loads(args.interpreters.read_text())
    matrix = tomllib.loads(
        (args.django / "compatibility/django-versions.toml").read_text()
    )["matrix"]
    pairs = [
        {"django": probe["django"], "python": python}
        for probe in matrix["probe"]
        for python in probe["python"]
    ]
    selected = [
        pair
        for pair in pairs
        if not args.only or any(pair["django"].startswith(value) for value in args.only)
    ]
    dependencies = [
        line.strip()
        for line in (args.django / "conformance/candidate-requirements.txt")
        .read_text()
        .splitlines()
        if line.strip() and not line.startswith(("#", "Django=="))
    ]
    args.output.mkdir(parents=True, exist_ok=True)
    evidence_name = (
        f"evidence-{args.checker_mode}" if args.mode == "compare" else "evidence"
    )
    result_name = (
        f"compare-{args.checker_mode}" if args.mode == "compare" else args.mode
    )
    results = []
    for pair in selected:
        label = f"django-{pair['django']}-python-{pair['python']}"
        root = args.output / label
        environment = root / "environment"
        python = environment / "bin/python"
        root.mkdir(parents=True, exist_ok=True)
        if args.reuse and not python.is_file():
            raise RuntimeError(
                f"--reuse requires a prepared environment: {environment}"
            )
        if not python.is_file():
            subprocess.run(
                [
                    interpreters[pair["python"]],
                    "-m",
                    "venv",
                    "--without-pip",
                    str(environment),
                ],
                check=True,
            )
        if not args.reuse:
            install = subprocess.run(
                [
                    args.uv,
                    "pip",
                    "install",
                    "--python",
                    str(python),
                    "--cache-dir",
                    str(args.output / "uv-cache"),
                    f"Django=={pair['django']}",
                    *dependencies,
                ],
                text=True,
                capture_output=True,
                check=False,
            )
            (root / "install.log").write_text(install.stdout + install.stderr)
            if install.returncode:
                raise RuntimeError(f"cannot prepare {label}: {install.stderr}")
        record = json.loads(
            subprocess.check_output(
                [
                    str(python),
                    "-c",
                    "import importlib.metadata as m,json,sys; print(json.dumps({'python':'.'.join(map(str,sys.version_info[:2])),'django':m.version('Django'),'distributions':{d.metadata['Name']:d.version for d in m.distributions()}}))",
                ],
                text=True,
            )
        )
        if record["python"] != pair["python"] or record["django"] != pair["django"]:
            raise RuntimeError(f"incorrect environment for {label}: {record}")
        installed = {
            name.lower().replace("-", "_"): value
            for name, value in record["distributions"].items()
        }
        for requirement in dependencies:
            name, version = requirement.split("==")
            if installed.get(name.lower().replace("-", "_")) != version:
                raise RuntimeError(
                    f"{label} does not contain exact requirement {requirement}"
                )
        forbidden = {name.lower() for name in record["distributions"]} & {
            "mypy",
            "django-stubs",
            "django-stubs-ext",
            "pydantic-monty",
        }
        if forbidden:
            raise RuntimeError(
                f"candidate environment contains unexpected packages: {forbidden}"
            )
        write_json(root / "environment.json", record)
        result = {**pair, "environment": str(environment), "prepared": True}
        if args.mode != "prepare":
            if args.fork is None:
                parser.error("baseline/compare requires --fork")
            command = [
                sys.executable,
                str(Path(__file__).with_name("django_acceptance.py")),
                args.mode,
                "--django",
                str(args.django),
                "--output",
                str(root / evidence_name),
                "--fork",
                str(args.fork.resolve()),
                "--python",
                str(environment),
                "--python-version",
                pair["python"],
            ]
            if args.mode == "compare":
                if args.companion is None or args.official is None:
                    parser.error("compare requires --companion and --official")
                command += [
                    "--companion",
                    str(args.companion.resolve()),
                    "--official",
                    str(args.official.resolve()),
                    "--checker-mode",
                    args.checker_mode,
                ]
            outcome = subprocess.run(
                command,
                env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
                text=True,
                capture_output=True,
                check=False,
            )
            log_name = (
                f"acceptance-{args.checker_mode}.log"
                if args.mode == "compare"
                else "acceptance.log"
            )
            (root / log_name).write_text(outcome.stdout + outcome.stderr)
            result["passed"] = outcome.returncode == 0
            result["exit"] = outcome.returncode
            result["evidence"] = str(root / evidence_name / "acceptance.json")
        results.append(result)
        summary = {
            "mode": args.mode,
            "checker_mode": args.checker_mode if args.mode == "compare" else None,
            "complete_matrix_selected": len(selected) == len(pairs),
            "required_pairs": len(pairs),
            "selected_pairs": len(selected),
            "results": results,
            "all_prepared": len(results) == len(selected),
            "compatibility_passed": args.mode == "compare"
            and len(results) == len(pairs)
            and all(item.get("passed") for item in results),
        }
        write_json(args.output / f"{result_name}.json", summary)
        print(
            f"{label}: {'PREPARED' if args.mode == 'prepare' else 'PASS' if result['passed'] else 'FAIL'}",
            flush=True,
        )
    return 0 if all(item.get("passed", True) for item in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
