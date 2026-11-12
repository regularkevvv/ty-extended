#!/usr/bin/env python3
"""Run the native Django comparison on an isolated CI runner."""

from __future__ import annotations

import argparse
import collections
import hashlib
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import venv
from pathlib import Path
from types import SimpleNamespace

from compare_checkers import invoke, source_hash

DJANGO_COMMIT = "c5a3eab7cb4416fc0f0202103f310422b5273c1c"
SCRIPTS = Path(__file__).resolve().parent


def run(command: list[str | Path]) -> None:
    print("+", " ".join(map(str, command)), flush=True)
    subprocess.run(list(map(str, command)), check=True)


def environment(path: Path, packages: list[str]) -> Path:
    venv.EnvBuilder(with_pip=True).create(path)
    python = path / "bin/python"
    run([python, "-m", "pip", "install", "--no-cache-dir", *packages])
    return python


def provider_state(path: Path) -> dict:
    return {
        file.relative_to(path).as_posix(): {
            "sha256": hashlib.sha256(file.read_bytes()).hexdigest(),
            "mtime_ns": file.stat().st_mtime_ns,
        }
        for file in sorted(path.rglob("*"))
        if file.is_file()
    }


def focus(output: Path, benchmark: dict) -> dict:
    project = output / "project"
    path = project / "filler00001.py"
    original = path.read_bytes(), path.stat().st_atime_ns, path.stat().st_mtime_ns
    before = source_hash(project)
    probe = SCRIPTS.parent / "tests/fixtures/django_native_focus.py"
    markers = {
        line: "expect=fail" in text
        for line, text in enumerate(probe.read_text().splitlines(), 1)
        if "# conformance:" in text
    }
    assert len(markers) == 20
    args = SimpleNamespace(
        output=output,
        project=project,
        threads=0,
        timeout=180,
        max_memory_mib=4096,
        queries=100,
    )
    results = {}
    try:
        path.write_bytes(probe.read_bytes())
        for backend in ("companion", "bare-ty", "mypy"):
            command = list(
                next(
                    r["command"]
                    for r in benchmark["records"]
                    if r["backend"] == backend
                )
            )
            if backend == "mypy":
                index = command.index("--cache-dir")
                del command[index : index + 2]
            row = invoke(args, backend, command, "focus/" + backend)
            text = (output / "runs" / row["label"] / "stdout.json").read_text()
            diagnostics = (
                [json.loads(line) for line in text.splitlines() if line.strip()]
                if backend == "mypy"
                else json.loads(text)
            )
            grouped = collections.defaultdict(list)
            unmarked = []
            for diagnostic in diagnostics:
                if backend == "mypy":
                    location, line = diagnostic["file"], diagnostic["line"]
                else:
                    location = diagnostic["location"]["path"]
                    line = diagnostic["location"]["positions"]["begin"]["line"]
                location = Path(location)
                if not location.is_absolute():
                    location = project / location
                if location == path and line in markers:
                    grouped[line].append(diagnostic)
                else:
                    unmarked.append(diagnostic)
            cases = [
                {
                    "line": line,
                    "expect_failure": failure,
                    "matched": bool(grouped[line]) == failure,
                    "diagnostics": grouped[line],
                }
                for line, failure in markers.items()
            ]
            results[backend] = {
                "run": row,
                "matched": sum(c["matched"] for c in cases),
                "total": 20,
                "cases": cases,
                "unmarked": unmarked,
            }
            assert not unmarked, results[backend]
            if backend == "companion":
                assert results[backend]["matched"] == 20, results[backend]
    finally:
        path.write_bytes(original[0])
        os.utime(path, ns=original[1:])
        assert source_hash(project) == before
        (output / "focus.json").write_text(json.dumps(results, indent=2) + "\n")
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--companion", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    output = args.output.resolve()
    os.environ.update(
        TY_COMPANION_NATIVE_FIELDS="1",
        TY_COMPANION_MEMORY_PROJECTION="1",
        TY_COMPANION_NATIVE_WARMUP="1",
        TY_COMPANION_BORROW_OVERLAYS="1",
        TY_COMPANION_TRACE_PHASES="1",
    )
    report = {
        "passed": False,
        "django_commit": DJANGO_COMMIT,
        "companion_sha256": hashlib.sha256(args.companion.read_bytes()).hexdigest(),
    }
    environments = [output / name for name in ("candidate", "native", "reference")]
    try:
        source = output / "django-source"
        run(["git", "init", source])
        run(
            [
                "git",
                "-C",
                source,
                "fetch",
                "--depth",
                "1",
                "https://github.com/regularkevvv/django-ty.git",
                DJANGO_COMMIT,
            ]
        )
        run(["git", "-C", source, "checkout", "--detach", "FETCH_HEAD"])
        actual = subprocess.check_output(
            ["git", "-C", source, "rev-parse", "HEAD"], text=True
        ).strip()
        assert actual == DJANGO_COMMIT
        provider = source / "python/django_ty/stubs"
        provider_before = provider_state(provider)
        report["provider_snapshot_before"] = provider_before
        candidate = environment(
            environments[0],
            [
                "Django==6.1.3",
                "ty==0.0.101",
                "typing-extensions>=4.11",
                "types-PyYAML>=6.0.12",
            ],
        )
        native = environment(environments[1], ["Django==6.1.3", "django-types==0.24.0"])
        reference = environment(
            environments[2], ["Django==6.1.3", "mypy==2.4.1", "django-stubs==6.1.2"]
        )
        target = output / "comparison"
        command = [
            sys.executable,
            SCRIPTS / "compare_checkers.py",
            "--companion",
            args.companion.resolve(),
            "--official",
            candidate.parent / "ty",
            "--python",
            candidate,
            "--official-python",
            native,
            "--mypy",
            reference.parent / "mypy",
            "--plugin",
            source / "python/django_ty",
            "--files",
            "7082",
            "--models",
            "2501",
            "--queries",
            "100",
            "--repeats",
            "5",
            "--threads",
            "0",
            "--max-ty-ratio",
            "3",
            "--native-django",
            "--output",
            target,
        ]
        result = subprocess.run(list(map(str, command)), check=False)
        benchmark = json.loads((target / "benchmark.json").read_text())
        assert benchmark["passed"], "Workload correctness or process monitoring failed"
        report["benchmark_exit"] = result.returncode
        report["performance_gate"] = benchmark["performance_gate"]
        phases = collections.defaultdict(list)
        for row in benchmark["records"]:
            if (
                row["backend"] != "companion"
                or not row["label"].split("/")[0].isdigit()
            ):
                continue
            sample = collections.defaultdict(float)
            for name, seconds in re.findall(
                r"companion phase (\S+): ([\d.]+)s",
                (target / "runs" / row["label"] / "stderr.log").read_text(),
            ):
                sample[name] += float(seconds)
            for name, seconds in sample.items():
                phases[name].append(seconds)
        report["phase_medians_seconds"] = {
            name: statistics.median(samples) for name, samples in sorted(phases.items())
        }
        report["focus"] = focus(target, benchmark)
        report["provider_preserved"] = provider_state(provider) == provider_before
        assert report["provider_preserved"], "Provider source bytes or mtimes changed"
        report["passed"] = (
            result.returncode == 0 and benchmark["performance_gate"]["passed"]
        )
    finally:
        for path in environments:
            shutil.rmtree(path, ignore_errors=True)
        report["temporary_environments_cleaned"] = all(
            not p.exists() for p in environments
        )
        (output / "native-django.json").write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
