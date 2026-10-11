#!/usr/bin/env python3
"""Benchmark real Django projects only after their exact acceptance gate passes.

Each timed invocation starts fresh processes. In push mode the official checker
is reused across projection rounds within that invocation. Repeated invocations
can benefit from warm filesystem caches; they are not a persistent warm process.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import statistics
import subprocess
import sys
from collections import defaultdict
from pathlib import Path
from types import SimpleNamespace
from typing import Any

from django_acceptance import (
    Failure,
    checker_cases,
    checker_run,
    digest,
    environment_info,
    inventory,
    load_module,
    score_contract,
    write_json,
)

COUNTERS = (
    "projection_rounds",
    "oracle_rounds",
    "monty_executions",
    "monty_cache_hits",
    "suppressed_generated",
)


def diagnostic_hash(diagnostics: list[dict]) -> str:
    canonical = json.dumps(diagnostics, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(canonical.encode()).hexdigest()


def verify_result(
    actual: dict,
    expected: dict,
    backend: str,
    previous_counters: dict | None,
) -> dict:
    if not actual.get("passed"):
        raise Failure(f"{backend} execution failed: {actual}")
    if (
        actual["diagnostics"] != expected["diagnostics"]
        or actual["exit"] != expected["exit"]
    ):
        raise Failure(f"{backend} changed accepted diagnostics or exit status")
    metrics = actual["metrics"]
    counters = {}
    if backend != "fork":
        if metrics.get("checker_mode") != backend:
            raise Failure(f"requested {backend}, checker reported {metrics}")
        if metrics.get("plugin_runtime") != "embedded-monty":
            raise Failure(f"unexpected plugin runtime: {metrics}")
        for name in COUNTERS:
            value = metrics.get(name)
            if name == "suppressed_generated" and isinstance(value, list):
                value = len(value)
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise Failure(f"missing or invalid native counter {name}: {metrics}")
            counters[name] = value
        for name in ("projection_rounds", "oracle_rounds", "monty_executions"):
            if counters[name] == 0:
                raise Failure(f"native checker did no {name}: {metrics}")
        if not isinstance(metrics.get("monty_seconds"), (int, float)):
            raise Failure(f"missing native Monty timing: {metrics}")
        if previous_counters is not None and counters != previous_counters:
            raise Failure(
                f"{backend} counters changed: {previous_counters} -> {counters}"
            )
    return counters


def summarize(records: list[dict]) -> dict:
    groups = defaultdict(list)
    for record in records:
        if record["phase"] == "measured":
            groups[(record["suite"], record["backend"])].append(record["seconds"])
    result = {}
    for (suite, backend), seconds in groups.items():
        result.setdefault(suite, {})[backend] = {
            "runs": len(seconds),
            "median_seconds": statistics.median(seconds),
            "min_seconds": min(seconds),
            "max_seconds": max(seconds),
            "samples_seconds": seconds,
        }
    for backends in result.values():
        baseline = backends["fork"]["median_seconds"]
        for backend, values in backends.items():
            if backend != "fork":
                values["native_over_fork_ratio"] = values["median_seconds"] / baseline
    return result


def benchmark(args: Any) -> dict:
    sys.path.insert(0, str(args.django / "scripts"))
    evaluator = load_module(
        "django_benchmark_existing_evaluator",
        args.django / "scripts/evaluate_differential_conformance.py",
    )
    original_inputs = inventory(args.django, evaluator)
    write_json(args.output / "inputs.json", original_inputs)
    binaries = {
        name: {"path": str(getattr(args, name)), "sha256": digest(getattr(args, name))}
        for name in ("fork", "official", "companion")
    }
    versions = {}
    for name, required in (("fork", "0.84.4"), ("official", "0.0.84")):
        version = subprocess.check_output(
            [str(getattr(args, name)), "--version"], text=True
        ).strip()
        if len(version.split()) < 2 or version.split()[1] != required:
            raise Failure(f"{name} must be version {required}: {version}")
        versions[name] = version
    environment = environment_info(args.python)
    expected = {}
    acceptance_records = {}
    for mode in args.checker_mode:
        path = getattr(args, f"acceptance_{mode}")
        if path is None:
            raise Failure(f"--checker-mode {mode} requires --acceptance-{mode}")
        report = json.loads(path.read_text())
        if not report.get("compatibility_passed") or report.get("checker_mode") != mode:
            raise Failure(
                f"requires a complete, green {mode} acceptance report: {path}"
            )
        for name, binary in binaries.items():
            if report["binaries"][name]["sha256"] != binary["sha256"]:
                raise Failure(f"{name} binary differs from accepted binary: {path}")
        if report["environment"] != environment:
            raise Failure(f"benchmark environment differs from acceptance: {path}")
        if report.get("python_version") != args.python_version:
            raise Failure(f"benchmark Python target differs from acceptance: {path}")
        accepted_inputs = json.loads(path.with_name("inputs.json").read_text())
        if accepted_inputs["source_sha256"] != original_inputs["source_sha256"]:
            raise Failure(f"benchmark source differs from accepted source: {path}")
        for suite, row in report["checks"].items():
            if not row["passed"] or not row.get("exact_diagnostics_equal"):
                raise Failure(f"unverified {suite} in acceptance report: {path}")
            baseline = {key: row["fork"][key] for key in ("diagnostics", "exit")}
            if suite in expected and expected[suite] != baseline:
                raise Failure(f"acceptance modes disagree on {suite}")
            expected[suite] = baseline
        acceptance_records[mode] = {"path": str(path), "sha256": digest(path)}
    cases = checker_cases(args.django, args.output)
    if set(expected) != {case[0] for case in cases}:
        raise Failure("acceptance report must contain all three project workloads")
    records = []
    report = {
        "verified": False,
        "scope": "unchanged Django conformance, e2e positive, and e2e negative projects",
        "binaries": binaries,
        "versions": versions,
        "environment": environment,
        "acceptance": acceptance_records,
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "logical_cpus": os.cpu_count(),
        },
        "method": {
            "timer": "perf_counter around subprocess.run; includes startup, subprocess output capture, and all Rust projection/checking; excludes input preparation, verification and evidence writes",
            "processes": "fresh companion and checker per invocation; push reuses the checker within that invocation only",
            "first_observed": "recorded separately; no OS cache flush, so not evidence of physically cold disk caches",
            "warmup": "discarded from summary but fully verified; only OS/filesystem caches can survive across invocations",
            "order": "backend order rotates across rounds and workloads",
            "parallelism": "TY_MAX_PARALLELISM=1; sequential invocations",
            "warmups": args.warmups,
            "repetitions": args.repetitions,
            "checker_modes": args.checker_mode,
        },
        "expected": {
            suite: {
                "diagnostic_sha256": diagnostic_hash(value["diagnostics"]),
                "diagnostics": len(value["diagnostics"]),
                "exit": value["exit"],
            }
            for suite, value in expected.items()
        },
        "records": records,
    }
    write_json(args.output / "benchmark.json", report)
    counter_baselines = {}
    backends = ["fork", *args.checker_mode]
    phases = [
        ("first-observed", 1),
        ("warmup", args.warmups),
        ("measured", args.repetitions),
    ]
    order = 0
    for phase, repetitions in phases:
        for repetition in range(repetitions):
            for suite, root, paths, settings in cases:
                offset = order % len(backends)
                order += 1
                for backend in backends[offset:] + backends[:offset]:
                    invocation = SimpleNamespace(**vars(args))
                    invocation.checker_mode = backend if backend != "fork" else "cli"
                    label = f"{phase}-{repetition:02d}/{suite}-{backend}"
                    load_before = os.getloadavg()
                    actual = checker_run(
                        invocation,
                        root,
                        paths,
                        settings,
                        label,
                        native=backend != "fork",
                    )
                    try:
                        counters = verify_result(
                            actual,
                            expected[suite],
                            backend,
                            counter_baselines.get((suite, backend)),
                        )
                        if (
                            suite == "conformance"
                            and not score_contract(
                                actual["diagnostics"], args.django, evaluator
                            )["passed"]
                        ):
                            raise Failure("unchanged conformance expectations failed")
                    except Failure as error:
                        report["failure"] = {"label": label, "error": str(error)}
                        write_json(args.output / "benchmark.json", report)
                        raise
                    counter_baselines[(suite, backend)] = counters
                    record = {
                        "phase": phase,
                        "repetition": repetition,
                        "suite": suite,
                        "backend": backend,
                        "seconds": actual["seconds"],
                        "exit": actual["exit"],
                        "diagnostic_sha256": diagnostic_hash(actual["diagnostics"]),
                        "diagnostics": len(actual["diagnostics"]),
                        "verified": True,
                        "counters": counters,
                        "native_metrics": actual["metrics"],
                        "load_average_before": load_before,
                        "load_average_after": os.getloadavg(),
                        "evidence": str(args.output / "runs" / label),
                    }
                    records.append(record)
                    write_json(args.output / "benchmark.json", report)
                    print(
                        f"{label}: {actual['seconds']:.3f}s verified {record['diagnostics']} diagnostics",
                        flush=True,
                    )
    if inventory(args.django, evaluator) != original_inputs:
        raise Failure("benchmark source changed during the run")
    for name, binary in binaries.items():
        if digest(getattr(args, name)) != binary["sha256"]:
            raise Failure(f"{name} binary changed during the run")
    if environment_info(args.python) != environment:
        raise Failure(
            "installed dependency files or interpreter changed during the run"
        )
    report["verified"] = True
    report["summary"] = summarize(records)
    write_json(args.output / "benchmark.json", report)
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("django", "output", "fork", "official", "companion", "python"):
        parser.add_argument(f"--{name}", required=True, type=Path)
    parser.add_argument("--python-version", default="3.13")
    parser.add_argument("--checker-mode", choices=["cli", "push"], action="append")
    parser.add_argument("--acceptance-cli", type=Path)
    parser.add_argument("--acceptance-push", type=Path)
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--repetitions", type=int, default=7)
    parser.add_argument("--timeout", type=int, default=300)
    args = parser.parse_args()
    if args.warmups < 0 or args.repetitions < 3:
        parser.error(
            "requires nonnegative warmups and at least three measured repetitions"
        )
    args.checker_mode = list(dict.fromkeys(args.checker_mode or ["cli", "push"]))
    for name, value in vars(args).items():
        if isinstance(value, Path):
            setattr(args, name, value.resolve())
    if args.output.exists() and any(args.output.iterdir()):
        parser.error(
            "--output must be new or empty to preserve prior benchmark evidence"
        )
    args.output.mkdir(parents=True, exist_ok=True)
    try:
        report = benchmark(args)
    except (Failure, OSError, ValueError, KeyError) as error:
        write_json(
            args.output / "failure.json", {"verified": False, "error": str(error)}
        )
        print(str(error), file=sys.stderr)
        return 1
    print(json.dumps(report["summary"], indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
