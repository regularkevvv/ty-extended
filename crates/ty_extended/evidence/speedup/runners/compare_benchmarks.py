#!/usr/bin/env python3
"""Verify and time before/after binaries on identical unchanged Django fixtures."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import statistics
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path
from types import SimpleNamespace

REPO = Path(os.environ.get("TY_COMPANION_REPO", Path.cwd())).resolve()
sys.path.insert(0, str(REPO / "companion/scripts"))
from django_acceptance import (  # noqa: E402 - selected repository harness
    checker_cases,
    checker_run,
    digest,
    environment_info,
    fixture_files,
    inventory,
    load_module,
)

p = argparse.ArgumentParser()
for name in (
    "before",
    "after",
    "fork",
    "official",
    "django",
    "python",
    "acceptance",
    "output",
):
    p.add_argument("--" + name, type=Path, required=True)
p.add_argument("--kind", choices=["internal", "public"], required=True)
p.add_argument(
    "--suites", nargs="+", default=["conformance", "e2e-positive", "e2e-negative"]
)
p.add_argument("--modes", nargs="+", default=["cli", "push"])
p.add_argument("--formats", nargs="+", default=["full", "concise"])
p.add_argument("--warmups", type=int, default=2)
p.add_argument("--repetitions", type=int, default=7)
a = p.parse_args()
assert a.warmups >= 0 and a.repetitions >= 3
assert not a.output.exists(), a.output
a.output.mkdir(parents=True)
for key, value in vars(a).items():
    if isinstance(value, Path):
        setattr(a, key, value.resolve())
report_path = a.output / "comparison.json"


def write():
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")


report = {
    "passed": False,
    "kind": a.kind,
    "binaries": {
        n: {"path": str(getattr(a, n)), "sha256": digest(getattr(a, n))}
        for n in ("before", "after", "fork", "official")
    },
    "environment": environment_info(a.python),
    "runner_sha256": digest(Path(__file__)),
    "harness_sha256": {
        name: digest(REPO / "companion/scripts" / name)
        for name in ["django_acceptance.py"]
    },
    "method": {
        "warmups": a.warmups,
        "repetitions": a.repetitions,
        "order": "Rotate all backends across rounds and workloads; retain every invocation",
        "parallelism": "TY_MAX_PARALLELISM=1",
        "timer": "perf_counter around subprocess; includes process startup and full production operation; fixture staging and verification outside timer",
    },
    "records": [],
}
accepted = json.loads((a.acceptance / "acceptance.json").read_text())
assert accepted["compatibility_passed"] and accepted["passed"]
assert accepted["environment"] == report["environment"]
for key in ("fork", "official"):
    assert accepted["binaries"][key]["sha256"] == report["binaries"][key]["sha256"]
expected = {
    suite: {key: row["fork"][key] for key in ("diagnostics", "exit")}
    for suite, row in accepted["checks"].items()
}
sys.path.insert(0, str(a.django / "scripts"))
evaluator = load_module(
    "speed_django_evaluator", a.django / "scripts/evaluate_differential_conformance.py"
)
inputs = inventory(a.django, evaluator)
assert (
    inputs["source_sha256"]
    == json.loads((a.acceptance / "inputs.json").read_text())["source_sha256"]
)
report["input_sha256"] = inputs["source_sha256"]
report["acceptance_sha256"] = digest(a.acceptance / "acceptance.json")
cases = [case for case in checker_cases(a.django, a.output) if case[0] in a.suites]
assert len(cases) == len(set(a.suites))
original_staged = {
    str(root): {str(rel): digest(path) for rel, path in fixture_files(root).items()}
    for _, root, _, _ in cases
}
env = dict(os.environ, TY_MAX_PARALLELISM="1", TY_UV="0", PYTHONDONTWRITEBYTECODE="1")
metrics_baselines = {}
output_baselines = {}
write()
try:
    for phase, count in [("warmup", a.warmups), ("measured", a.repetitions)]:
        for iteration in range(count):
            round_no = iteration + (a.warmups if phase == "measured" else 0)
            workloads = [
                (case, mode)
                for case in cases
                for mode in (a.modes if a.kind == "internal" else a.formats)
            ]
            for workload_no, (case, mode) in enumerate(workloads):
                suite, root, paths, settings = case
                backends = ["fork", "before", "after"]
                offset = (round_no + workload_no) % 3
                backends = backends[offset:] + backends[:offset]
                for backend in backends:
                    label = f"{phase}-{iteration:02}-{suite}-{mode}-{backend}"
                    load_before = os.getloadavg()
                    if a.kind == "internal":
                        opts = SimpleNamespace(
                            django=a.django,
                            output=a.output,
                            companion=getattr(a, backend)
                            if backend != "fork"
                            else a.before,
                            official=a.official,
                            fork=a.fork,
                            python=a.python,
                            python_version="3.13",
                            checker_mode=mode,
                            timeout=300,
                        )
                        actual = checker_run(
                            opts, root, paths, settings, label, backend != "fork"
                        )
                        assert actual.get("passed"), actual
                        assert (
                            actual["exit"] == expected[suite]["exit"]
                            and actual["diagnostics"] == expected[suite]["diagnostics"]
                        ), label
                        metrics = actual["metrics"]
                        counters = {
                            k: len(v)
                            if k == "suppressed_generated" and isinstance(v, list)
                            else v
                            for k, v in metrics.items()
                            if k
                            in [
                                "projection_rounds",
                                "oracle_rounds",
                                "monty_executions",
                                "monty_cache_hits",
                                "suppressed_generated",
                                "checker_mode",
                                "plugin_runtime",
                            ]
                        }
                        if backend != "fork":
                            assert (
                                counters["checker_mode"] == mode
                                and counters["plugin_runtime"] == "embedded-monty"
                            ), counters
                            for key in [
                                "projection_rounds",
                                "oracle_rounds",
                                "monty_executions",
                            ]:
                                assert (
                                    isinstance(counters[key], int) and counters[key] > 0
                                ), counters
                            key = (suite, mode, backend)
                            if key in metrics_baselines:
                                assert counters == metrics_baselines[key], (
                                    key,
                                    counters,
                                    metrics_baselines[key],
                                )
                            metrics_baselines[key] = counters
                        elapsed = actual["seconds"]
                        returncode = actual["exit"]
                        details = {
                            "diagnostics": len(actual["diagnostics"]),
                            "diagnostic_sha256": hashlib.sha256(
                                json.dumps(
                                    actual["diagnostics"], sort_keys=True
                                ).encode()
                            ).hexdigest(),
                            "metrics": metrics,
                        }
                    else:
                        meta = json.loads(
                            (
                                a.acceptance
                                / "runs"
                                / suite
                                / "fork/check.command.json"
                            ).read_text()
                        )
                        command = meta["command"].copy()
                        command[0] = str(a.fork)
                        command[command.index("--project") + 1] = str(root)
                        command[command.index("--output-format") + 1] = mode
                        if backend != "fork":
                            command = [
                                str(getattr(a, backend)),
                                *command[1:],
                                "--ty",
                                str(a.official),
                            ]
                        start = time.perf_counter()
                        result = subprocess.run(
                            command, cwd=root, env=env, capture_output=True, timeout=300
                        )
                        elapsed = time.perf_counter() - start
                        returncode = result.returncode
                        assert returncode == expected[suite]["exit"], (
                            label,
                            result.stderr,
                        )
                        # Exact output and status parity between old/new companion, plus exact stable output per reference.
                        key = (
                            suite,
                            mode,
                            "companion" if backend != "fork" else "fork",
                        )
                        if key in output_baselines:
                            assert (result.stdout, result.stderr) == output_baselines[
                                key
                            ], (label, "output mismatch")
                        else:
                            output_baselines[key] = (result.stdout, result.stderr)
                        dest = a.output / "outputs" / label
                        dest.parent.mkdir(exist_ok=True)
                        dest.with_suffix(".stdout").write_bytes(result.stdout)
                        dest.with_suffix(".stderr").write_bytes(result.stderr)
                        details = {
                            "stdout_sha256": hashlib.sha256(result.stdout).hexdigest(),
                            "stderr_sha256": hashlib.sha256(result.stderr).hexdigest(),
                            "command": command,
                            "cwd": str(root),
                        }
                    row = {
                        "phase": phase,
                        "iteration": iteration,
                        "suite": suite,
                        "mode": mode,
                        "backend": backend,
                        "seconds": elapsed,
                        "exit": returncode,
                        "load_before": load_before,
                        "load_after": os.getloadavg(),
                        "passed": True,
                        **details,
                    }
                    report["records"].append(row)
                    write()
                    print(label, f"{elapsed:.6f}", flush=True)
    assert inputs == inventory(a.django, evaluator)
    for name in ("before", "after", "fork", "official"):
        assert digest(getattr(a, name)) == report["binaries"][name]["sha256"]
    assert report["environment"] == environment_info(a.python)
    for name, sha in report["harness_sha256"].items():
        assert digest(REPO / "companion/scripts" / name) == sha, name
    assert original_staged == {
        str(root): {str(rel): digest(path) for rel, path in fixture_files(root).items()}
        for _, root, _, _ in cases
    }
    groups = defaultdict(list)
    for row in report["records"]:
        if row["phase"] == "measured":
            groups[(row["suite"], row["mode"], row["backend"])].append(row["seconds"])
    summary = {}
    for (suite, mode, backend), samples in groups.items():
        summary.setdefault(suite, {}).setdefault(mode, {})[backend] = {
            "median_seconds": statistics.median(samples),
            "samples": samples,
        }
    for modes in summary.values():
        for backends in modes.values():
            backends["after"]["speedup_over_before"] = (
                backends["before"]["median_seconds"]
                / backends["after"]["median_seconds"]
            )
            backends["after"]["fraction_seconds_reduced"] = (
                1
                - backends["after"]["median_seconds"]
                / backends["before"]["median_seconds"]
            )
            backends["after"]["ratio_over_fork"] = (
                backends["after"]["median_seconds"] / backends["fork"]["median_seconds"]
            )
    report.update(passed=True, summary=summary)
except BaseException as error:
    report["error"] = repr(error)
    write()
    raise
write()
print(json.dumps(summary, indent=2))
