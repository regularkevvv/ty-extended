#!/usr/bin/env python3
"""Compare the native companion with django-ty's unchanged acceptance corpus.

Python orchestrates the existing Python test fixtures only. The production
frontend, plugin execution, projection, and diagnostic mapping remain Rust.
No expected diagnostic is removed, rewritten, or accepted from a snapshot.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib


class Failure(Exception):
    pass


IGNORED_FIXTURE_DIRECTORIES = {"__pycache__", ".mypy_cache", ".venv"}


def fixture_files(root: Path) -> dict[Path, Path]:
    return {
        path.relative_to(root): path
        for path in root.rglob("*")
        if path.is_file()
        and not IGNORED_FIXTURE_DIRECTORIES.intersection(path.relative_to(root).parts)
        and path.suffix != ".pyc"
    }


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def load_module(name: str, path: Path) -> Any:
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


def run(command: list[str], cwd: Path, evidence: Path, timeout: int = 300) -> dict:
    environment = dict(os.environ)
    environment.update(TY_MAX_PARALLELISM="1", TY_UV="0", PYTHONDONTWRITEBYTECODE="1")
    started = time.perf_counter()
    try:
        result = subprocess.run(
            command,
            cwd=cwd,
            env=environment,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
        elapsed = time.perf_counter() - started
    except subprocess.TimeoutExpired as error:
        write_json(
            evidence.with_suffix(".command.json"),
            {
                "command": command,
                "cwd": str(cwd),
                "timeout_seconds": timeout,
                "timed_out": True,
            },
        )
        raise Failure(f"command timed out: {command[0]}") from error
    evidence.parent.mkdir(parents=True, exist_ok=True)
    evidence.with_suffix(".stdout").write_text(result.stdout)
    evidence.with_suffix(".stderr").write_text(result.stderr)
    record = {
        "command": command,
        "cwd": str(cwd),
        "exit": result.returncode,
        "seconds": elapsed,
    }
    write_json(evidence.with_suffix(".command.json"), record)
    return {**record, "stdout": result.stdout, "stderr": result.stderr}


def inventory(repository: Path, evaluator: Any) -> dict:
    markers = evaluator.collect_markers(repository / "conformance")
    source_roots = [repository / "conformance", repository / "e2e/django5_fixture"]
    inputs = [path for root in source_roots for path in fixture_files(root).values()]
    inputs += [
        repository / "compatibility/django-official-contracts.toml",
        repository / "compatibility/django-versions.toml",
        repository / "scripts/check_django_runtime_contracts.py",
        repository / "scripts/check_querydict_runtime.py",
        repository / "scripts/evaluate_differential_conformance.py",
        repository / "scripts/django_official_contracts.py",
        repository / "scripts/runtime_config.py",
    ]
    inputs += list(fixture_files(repository / "python/django_ty").values())
    matrix = tomllib.loads(
        (repository / "compatibility/django-versions.toml").read_text()
    )
    return {
        "repository": str(repository),
        "source_sha256": {
            str(path.relative_to(repository)): digest(path) for path in sorted(inputs)
        },
        "assertions": len(markers),
        "features": dict(sorted(Counter(marker.feature for marker in markers).items())),
        "matrix": [
            {"django": probe["django"], "python": python}
            for probe in matrix["matrix"]["probe"]
            for python in probe["python"]
        ],
    }


def stage(repository: Path, output: Path, suite: str) -> Path:
    original = repository / (
        "conformance" if suite == "conformance" else "e2e/django5_fixture"
    )
    destination = output / "inputs" / suite
    if destination.exists():
        # Existing staged input must still be byte-identical. Never replace
        # altered test input and silently change what an earlier run checked.
        expected = fixture_files(original)
        actual = fixture_files(destination)
        if expected.keys() != actual.keys():
            raise Failure(f"staged fixture file set differs: {destination}")
        for relative, path in expected.items():
            if actual[relative].read_bytes() != path.read_bytes():
                raise Failure(f"staged fixture differs: {actual[relative]}")
    else:
        shutil.copytree(
            original,
            destination,
            ignore=shutil.ignore_patterns(*IGNORED_FIXTURE_DIRECTORIES, "*.pyc"),
        )
    return destination


def normalize(items: Any, project: Path) -> list[dict]:
    if not isinstance(items, list):
        raise Failure("checker did not return a diagnostic array")
    result = []
    for item in items:
        try:
            location = item["location"]
            path = Path(location["path"])
            if path.is_absolute():
                path = path.relative_to(project)
            if ".." in path.parts:
                raise ValueError("diagnostic outside project")
            positions = location["positions"]
            result.append(
                {
                    "path": path.as_posix().removeprefix("./"),
                    "begin": positions["begin"],
                    "end": positions["end"],
                    "code": item["check_name"],
                    "severity": item["severity"],
                    "message": item["description"],
                }
            )
        except (KeyError, TypeError, ValueError) as error:
            raise Failure(f"invalid original-source diagnostic: {item}") from error
    return sorted(result, key=lambda value: json.dumps(value, sort_keys=True))


def score_contract(diagnostics: list[dict], repository: Path, evaluator: Any) -> dict:
    markers = evaluator.collect_markers(repository / "conformance")
    by_location = defaultdict(list)
    for item in diagnostics:
        by_location[(item["path"], item["begin"]["line"])].append(item)
    contracts = evaluator.load_contracts()
    results = []
    marker_locations = {marker.key for marker in markers}
    for marker in markers:
        actual = by_location[marker.key]
        matches = bool(actual) == (marker.expect == "fail")
        expected_message = (
            contracts["review"]
            .get(f"{marker.feature}/{marker.case}", {})
            .get("candidate_diagnostic")
        )
        if expected_message:
            matches &= len(actual) == 1 and expected_message in actual[0]["message"]
        results.append(
            {
                "case": f"{marker.feature}/{marker.case}",
                "path": marker.path,
                "line": marker.line,
                "expect": marker.expect,
                "passed": matches,
                "diagnostics": actual,
            }
        )
    unowned = [
        item
        for item in diagnostics
        if (item["path"], item["begin"]["line"]) not in marker_locations
    ]
    return {
        "passed": all(case["passed"] for case in results) and not unowned,
        "matched": sum(case["passed"] for case in results),
        "total": len(results),
        "unowned_diagnostics": unowned,
        "cases": results,
    }


def checker_run(
    args: Any,
    project: Path,
    paths: list[str],
    settings: str | None,
    label: str,
    native: bool,
) -> dict:
    package = args.django / "python/django_ty"
    destination = args.output / "runs" / label / ("native" if native else "fork")
    destination.mkdir(parents=True, exist_ok=True)
    config = {"django-settings-module": settings} if settings else {}
    write_json(destination / "config.json", config)
    write_json(destination / "files.json", paths)
    if native:
        command = [
            str(args.companion),
            "project",
            str(project),
            "--plugin",
            str(package / "monty.py"),
            "--manifest",
            str(package / "ty-plugin-monty.json"),
            "--ty",
            str(args.official),
            "--out",
            str(destination / "projection"),
            "--config",
            str(destination / "config.json"),
            "--stubs",
            str(package / "stubs"),
            "--python-version",
            args.python_version,
            "--python",
            str(args.python),
            "--files",
            str(destination / "files.json"),
            "--checker-mode",
            args.checker_mode,
        ]
    else:
        entry = {
            "id": "django-ty",
            "runtime": "monty",
            "trusted": True,
            "path": str(package / "monty.py"),
            "manifest-path": str(package / "ty-plugin-monty.json"),
            "stub-overlay-path": str(package / "stubs"),
        }
        table = ", ".join(
            f"{key} = {json.dumps(value)}" for key, value in entry.items()
        )
        if settings:
            table += (
                ", config = {django-settings-module = " + json.dumps(settings) + "}"
            )
        command = [
            str(args.fork),
            "check",
            *paths,
            "--project",
            str(project),
            "--python",
            str(args.python),
            "--python-version",
            args.python_version,
            "--output-format",
            "gitlab",
            "--color",
            "never",
            "--no-progress",
            "--config",
            "plugins.enabled = true",
            "--config",
            "plugins.auto-discover = false",
            "--config",
            "plugins.plugin = [{" + table + "}]",
            "--config",
            "terminal.error-on-warning = true",
        ]
    record = run(command, project, destination / "check", args.timeout)
    if record["exit"] not in {0, 1}:
        return {"passed": False, "execution_error": True, "record": record}
    try:
        value = json.loads(record["stdout"])
        items = value["diagnostics"] if native else value
        diagnostics = normalize(items, project)
    except (ValueError, TypeError, KeyError, Failure) as error:
        return {"passed": False, "execution_error": str(error), "record": record}
    write_json(destination / "diagnostics.json", diagnostics)
    return {
        "passed": True,
        "exit": record["exit"],
        "diagnostics": diagnostics,
        "seconds": record["seconds"],
        "metrics": {key: item for key, item in value.items() if key != "diagnostics"}
        if native
        else {},
    }


def checker_cases(repository: Path, output: Path) -> list[tuple]:
    project = stage(repository, output, "conformance")
    e2e = stage(repository, output, "e2e")
    return [
        (
            "conformance",
            project,
            ["conformance_project", "conformance_models", "cases"],
            "conformance_project.settings",
        ),
        (
            "e2e-positive",
            e2e,
            [
                "accounts",
                "library",
                "commerce",
                "auditlog",
                "typechecks/positive.py",
                "typechecks/static_api.py",
            ],
            "config.settings",
        ),
        (
            "e2e-negative",
            e2e,
            [
                "accounts",
                "library",
                "commerce",
                "auditlog",
                "caveats",
                "typechecks/negative.py",
            ],
            "config.settings",
        ),
    ]


def suite(args: Any, evaluator: Any) -> dict:
    checks = checker_cases(args.django, args.output)
    required_suites = {check[0] for check in checks}
    if args.only:
        checks = [check for check in checks if check[0] in args.only]
    binaries = {"fork": args.fork}
    if args.mode == "compare":
        binaries.update(companion=args.companion, official=args.official)
    environment = environment_info(args.python)
    report = {
        "mode": args.mode,
        "checker_mode": args.checker_mode if args.mode == "compare" else None,
        "python_version": args.python_version,
        "runtime_gates_requested": not args.skip_runtime,
        "complete_suites_selected": {check[0] for check in checks} == required_suites,
        "environment": environment,
        "binaries": {
            name: {"path": str(path), "sha256": digest(path)}
            for name, path in binaries.items()
        },
        "checks": {},
    }
    for name, root, paths, settings in checks:
        baseline = checker_run(args, root, paths, settings, name, native=False)
        row = {"fork": baseline}
        if baseline["passed"]:
            if name == "conformance":
                row["fork_contract"] = score_contract(
                    baseline["diagnostics"], args.django, evaluator
                )
                row["passed"] = row["fork_contract"]["passed"]
            elif name == "e2e-positive":
                row["passed"] = baseline["exit"] == 0 and not baseline["diagnostics"]
            else:
                expected = [
                    "django-ty.unknown-relation-target",
                    "django-ty.reverse-relation-conflict",
                    "django-ty.invalid-lookup-value",
                    "django-ty.unknown-lookup",
                    "Expected `str`, found `Literal[123]`",
                    "Object of type `str` is not assignable to `int",
                ]
                messages = "\n".join(
                    item["message"] for item in baseline["diagnostics"]
                )
                row["missing_negative_controls"] = [
                    message for message in expected if message not in messages
                ]
                row["passed"] = (
                    baseline["exit"] == 1 and not row["missing_negative_controls"]
                )
        else:
            row["passed"] = False
        if args.mode == "compare":
            candidate = checker_run(args, root, paths, settings, name, native=True)
            row["native"] = candidate
            if candidate["passed"] and baseline["passed"]:
                row["exact_diagnostics_equal"] = (
                    candidate["diagnostics"] == baseline["diagnostics"]
                )
                row["exit_equal"] = candidate["exit"] == baseline["exit"]
                row["passed"] &= row["exact_diagnostics_equal"] and row["exit_equal"]
                if name == "conformance":
                    row["native_contract"] = score_contract(
                        candidate["diagnostics"], args.django, evaluator
                    )
                    row["passed"] &= row["native_contract"]["passed"]
            else:
                row["passed"] = False
        report["checks"][name] = row
        write_json(args.output / "acceptance.json", report)
        print(f"{name}: {'PASS' if row['passed'] else 'FAIL'}", flush=True)
    if not args.skip_runtime:
        report["runtime"] = runtime_checks(args)
        report["querydict"] = querydict_checks(args)
    report["passed"] = all(row["passed"] for row in report["checks"].values())
    report["passed"] &= all(
        report.get(name, {"passed": True})["passed"]
        for name in ("runtime", "querydict")
    )
    report["compatibility_passed"] = (
        args.mode == "compare"
        and not args.skip_runtime
        and report["complete_suites_selected"]
        and report["passed"]
    )
    write_json(args.output / "acceptance.json", report)
    return report


def environment_python(environment: Path) -> Path:
    return environment / "bin/python" if environment.is_dir() else environment


def environment_info(environment: Path) -> dict:
    # Hash installed files themselves, not just RECORD declarations or versions.
    # Bytecode is interpreter-generated and does not define the installed input.
    program = """import hashlib, importlib.metadata as metadata, json, sys
from pathlib import Path
packages = {}
for distribution in metadata.distributions():
    entries = []
    if distribution.files is None:
        raise RuntimeError(f"distribution has no file inventory: {distribution}")
    for relative in distribution.files:
        if relative.suffix == ".pyc" or "__pycache__" in relative.parts:
            continue
        path = distribution.locate_file(relative)
        entries.append((str(relative), hashlib.sha256(path.read_bytes()).hexdigest()))
    packages[distribution.metadata['Name']] = {
        'version': distribution.version,
        'files': len(entries),
        'sha256': hashlib.sha256(json.dumps(sorted(entries), separators=(',', ':')).encode()).hexdigest(),
    }
print(json.dumps({
    'python': sys.version,
    'python_binary_sha256': hashlib.sha256(Path(sys.executable).read_bytes()).hexdigest(),
    'django': metadata.version('Django'),
    'distributions': {name: value['version'] for name, value in packages.items()},
    'installed_files': packages,
}))
"""
    return json.loads(
        subprocess.check_output(
            [str(environment_python(environment)), "-c", program], text=True
        )
    )


def runtime_checks(args: Any) -> dict:
    record = run(
        [
            str(environment_python(args.python)),
            str(args.django / "scripts/check_django_runtime_contracts.py"),
        ],
        args.django,
        args.output / "runtime/contracts",
        args.timeout,
    )
    if record["exit"] != 0:
        return {"passed": False, "record": record}
    value = json.loads(record["stdout"])
    value["passed"] = bool(value.get("cases")) and all(
        item.get("passed") for item in value["cases"].values()
    )
    print(
        f"Django runtime contracts: {len(value['cases'])} {'PASS' if value['passed'] else 'FAIL'}",
        flush=True,
    )
    return value


def querydict_checks(args: Any) -> dict:
    original = load_module(
        "django_acceptance_querydict",
        args.django / "scripts/check_querydict_runtime.py",
    )
    runtime = (
        original.IMPORTS
        + """from django.conf import settings
settings.configure(DEFAULT_CHARSET="utf-8", SECRET_KEY="querydict-test")
import json
outcomes = {}
"""
    )
    runtime += f"cases = {original.CASES!r}\n"
    runtime += """for name, expression in cases.items():
    querydict = eval(expression)
    try:
        querydict["page"] = "1"
    except AttributeError:
        outcomes[name] = "reject"
    else:
        assert querydict["page"] == "1"
        outcomes[name] = "accept"
print(json.dumps(outcomes))
"""
    record = run(
        [str(environment_python(args.python)), "-c", runtime],
        args.django,
        args.output / "runtime/querydict",
    )
    if record["exit"] != 0:
        return {"passed": False, "record": record}
    actual = json.loads(record["stdout"])
    expected = {
        name: "reject"
        if name
        in {
            "framework-get",
            "framework-post",
            "asgi-get",
            "asgi-post",
            "default-querydict",
        }
        else "accept"
        for name in original.CASES
    }
    if actual != expected:
        return {"passed": False, "actual": actual, "expected": expected}
    results = {}
    for name, expression in original.CASES.items():
        project = args.output / "inputs/querydict" / name
        project.mkdir(parents=True, exist_ok=True)
        path = project / "case.py"
        path.write_text(
            original.IMPORTS + f'querydict = {expression}\nquerydict["page"] = "1"\n'
        )
        baseline = checker_run(
            args, project, ["case.py"], None, f"querydict-{name}", native=False
        )
        row = {"fork": baseline, "runtime": actual[name], "passed": baseline["passed"]}
        if baseline["passed"]:
            if actual[name] == "accept":
                row["passed"] &= baseline["exit"] == 0 and not baseline["diagnostics"]
            else:
                row["passed"] &= (
                    baseline["exit"] == 1
                    and len(baseline["diagnostics"]) == 1
                    and "django-ty.immutable-querydict-write"
                    in baseline["diagnostics"][0]["message"]
                )
        if args.mode == "compare":
            candidate = checker_run(
                args, project, ["case.py"], None, f"querydict-{name}", native=True
            )
            row["native"] = candidate
            row["passed"] &= candidate["passed"]
            if candidate["passed"] and baseline["passed"]:
                row["passed"] &= (
                    candidate["diagnostics"] == baseline["diagnostics"]
                    and candidate["exit"] == baseline["exit"]
                )
        results[name] = row
    print(
        f"QueryDict controls: {sum(item['passed'] for item in results.values())}/{len(results)}",
        flush=True,
    )
    return {
        "passed": all(item["passed"] for item in results.values()),
        "cases": results,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["inventory", "baseline", "compare"])
    parser.add_argument("--django", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--fork", type=Path)
    parser.add_argument("--companion", type=Path)
    parser.add_argument("--official", type=Path)
    parser.add_argument("--python", type=Path)
    parser.add_argument("--python-version", default="3.13")
    parser.add_argument("--checker-mode", choices=["cli", "push"], default="cli")
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument(
        "--skip-runtime",
        action="store_true",
        help="Run only checker suites; the report records that runtime gates were omitted",
    )
    parser.add_argument(
        "--only",
        choices=["conformance", "e2e-positive", "e2e-negative"],
        action="append",
        help="Select a checker suite for iteration; partial runs never report full compatibility",
    )
    args = parser.parse_args()
    for name in ["django", "output", "fork", "companion", "official", "python"]:
        if getattr(args, name) is not None:
            setattr(args, name, getattr(args, name).resolve())
    if args.mode != "inventory" and (args.fork is None or args.python is None):
        parser.error("baseline and compare require --fork and --python")
    if args.mode == "compare" and (args.companion is None or args.official is None):
        parser.error("compare requires --companion and --official")
    sys.path.insert(0, str(args.django / "scripts"))
    evaluator = load_module(
        "django_acceptance_existing_evaluator",
        args.django / "scripts/evaluate_differential_conformance.py",
    )
    initial = inventory(args.django, evaluator)
    write_json(args.output / "inputs.json", initial)
    if args.mode == "inventory":
        print(
            json.dumps(
                {
                    "assertions": initial["assertions"],
                    "features": len(initial["features"]),
                    "matrix_pairs": len(initial["matrix"]),
                }
            )
        )
        return 0
    try:
        report = suite(args, evaluator)
    except Failure as error:
        print(str(error), file=sys.stderr)
        return 2
    if inventory(args.django, evaluator) != initial:
        raise Failure("acceptance inputs changed during the run")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
