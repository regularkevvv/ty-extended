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
from performance_reference import MAX_REFERENCE_RATIO, REFERENCE_COMMIT

DJANGO_COMMIT = "6e55703a95b48015f6fead73452b0c92cba99f32"
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
    parser.add_argument("--performance-reference", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--production-defaults", action="store_true")
    parser.add_argument("--repeats", type=int, default=10)
    parser.add_argument(
        "--matrix", action="store_true", help="Run all five existing workload shapes"
    )
    args = parser.parse_args()
    if args.repeats < 1:
        parser.error("--repeats must be positive")
    args.output.mkdir(parents=True, exist_ok=False)
    output = args.output.resolve()
    flags = (
        "TY_COMPANION_NATIVE_FIELDS",
        "TY_COMPANION_MEMORY_PROJECTION",
        "TY_COMPANION_NATIVE_WARMUP",
        "TY_COMPANION_BORROW_OVERLAYS",
    )
    for key in flags:
        if args.production_defaults:
            os.environ.pop(key, None)
        else:
            os.environ[key] = "1"
    os.environ["TY_COMPANION_TRACE_PHASES"] = "1"
    report = {
        "passed": False,
        "django_commit": DJANGO_COMMIT,
        "companion_sha256": hashlib.sha256(args.companion.read_bytes()).hexdigest(),
        "production_defaults": args.production_defaults,
    }
    if args.performance_reference:
        provenance = json.loads(
            args.performance_reference.with_suffix(".provenance.json").read_text()
        )
        assert provenance["source_commit"] == REFERENCE_COMMIT
        assert (
            provenance["binary_sha256"]
            == hashlib.sha256(args.performance_reference.read_bytes()).hexdigest()
        )
        report["performance_reference"] = provenance
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
                "ty==0.0.104",
                "typing-extensions>=4.11",
                "types-PyYAML>=6.0.12",
            ],
        )
        native = environment(environments[1], ["Django==6.1.3", "django-types==0.24.0"])
        reference = environment(
            environments[2], ["Django==6.1.3", "mypy==2.4.1", "django-stubs==6.1.2"]
        )
        workloads = (
            [
                ("small", 100, 25, 5, 25),
                ("distributed", 1000, 250, 25, 100),
                ("many-files", 7082, 250, 25, 100),
                ("stress", 7082, 2501, 1, 100),
                ("distributed-stress", 7082, 2501, 250, 100),
            ]
            if args.matrix
            else [("stress", 7082, 2501, 1, 100)]
        )
        report["workloads"] = {}
        for name, files, models, modules, queries in workloads:
            current = {}
            target = output / name if args.matrix else output / "comparison"
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
                str(files),
                "--models",
                str(models),
                "--model-modules",
                str(modules),
                "--queries",
                str(queries),
                "--repeats",
                str(args.repeats),
                "--threads",
                "0",
                "--max-ty-ratio",
                "3.5",
                "--aggregate-ratios",
                "--production-defaults"
                if args.production_defaults
                else "--native-django",
                "--output",
                target,
            ]
            if args.performance_reference:
                command.extend(
                    [
                        "--performance-reference",
                        args.performance_reference.resolve(),
                        "--max-reference-ratio",
                        str(MAX_REFERENCE_RATIO),
                    ]
                )
            result = subprocess.run(list(map(str, command)), check=False)
            benchmark = json.loads((target / "benchmark.json").read_text())
            assert benchmark["passed"], (
                "Workload correctness or process monitoring failed"
            )
            current["benchmark_exit"] = result.returncode
            current["performance_gate"] = benchmark["performance_gate"]
            current["performance_policy"] = benchmark["performance_policy"]
            if args.performance_reference:
                current["reference_performance_gate"] = benchmark[
                    "reference_performance_gate"
                ]
            phases = collections.defaultdict(list)
            native_cli_reuses = 0
            for row in benchmark["records"]:
                if (
                    row["backend"] != "companion"
                    or not row["label"].split("/")[0].isdigit()
                ):
                    continue
                sample = collections.defaultdict(float)
                stderr = (target / "runs" / row["label"] / "stderr.log").read_text()
                if args.production_defaults:
                    assert "companion native fields: true" in stderr
                    assert "companion borrowed overlay: true" in stderr
                    assert (
                        "companion native warmup initialize:" in stderr
                        or "companion native cli check:" in stderr
                    )
                native_cli_reuses += "companion native cli reuse: true" in stderr
                for phase_name, seconds in re.findall(
                    r"companion phase (\S+): ([\d.]+)s",
                    stderr,
                ):
                    sample[phase_name] += float(seconds)
                for phase_name, seconds in sample.items():
                    phases[phase_name].append(seconds)
            current["native_cli_reuse_positive_samples"] = native_cli_reuses
            current["phase_medians_seconds"] = {
                name: statistics.median(samples)
                for name, samples in sorted(phases.items())
            }
            if name == "stress":
                current["focus"] = focus(target, benchmark)
            current["passed"] = (
                result.returncode == 0 and benchmark["performance_target_met"]
            )
            current["median_seconds"] = benchmark["median_seconds"]
            current["source_sha256"] = benchmark["source_sha256"]
            current["production_path_verified"] = args.production_defaults
            report["workloads"][name] = current
            (output / "native-django.json").write_text(
                json.dumps(report, indent=2) + "\n"
            )
        report["provider_preserved"] = provider_state(provider) == provider_before
        assert report["provider_preserved"], "Provider source bytes or mtimes changed"
        report.update(report["workloads"]["stress"])
        report["passed"] = all(row["passed"] for row in report["workloads"].values())
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
