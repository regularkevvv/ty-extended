#!/usr/bin/env python3
"""Time the existing django-ty stress generator with identical before/after inputs."""

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

ROOT = Path(os.environ.get("TY_COMPANION_REPO", Path.cwd())).resolve()
sys.path.insert(0, str(ROOT / "companion/scripts"))
from django_acceptance import (  # noqa: E402 - selected repository harness
    checker_run,
    digest,
    environment_info,
    fixture_files,
    normalize,
)

p = argparse.ArgumentParser()
for key in [
    "before",
    "after",
    "fork",
    "official",
    "django",
    "python",
    "plan",
    "output",
]:
    p.add_argument("--" + key, type=Path, required=True)
p.add_argument("--kind", choices=["public", "cli", "push"], default="public")
p.add_argument("--models", type=int, nargs="+", default=[0, 25, 100])
p.add_argument(
    "--backends",
    nargs="+",
    choices=["fork", "before", "after"],
    default=["fork", "before", "after"],
)
p.add_argument("--warmups", type=int, default=2)
p.add_argument("--repetitions", type=int, default=7)
p.add_argument("--timeout", type=int, default=300)
a = p.parse_args()
assert a.warmups >= 0 and a.repetitions >= 1 and not a.output.exists()
a.output.mkdir(parents=True)
plan = json.loads(a.plan.read_text())
assert digest(Path(plan["generator"])) == plan["generator_sha256"]
workloads = [w for w in plan["workloads"] if w["extra_models"] in a.models]
assert len(workloads) == len(set(a.models))


def check_sources():
    for w in workloads:
        root = Path(w["project"])
        actual = {
            str(p.relative_to(root)): digest(p)
            for p in root.rglob("*")
            if p.is_file() and "__pycache__" not in p.parts and p.suffix != ".pyc"
        }
        assert actual == w["source_hashes"], w["extra_models"]


check_sources()
plugin_inputs = {
    str(relative): digest(path)
    for relative, path in fixture_files(a.django / "python/django_ty").items()
}
harness_sha = digest(ROOT / "companion/scripts/django_acceptance.py")
report = {
    "passed": False,
    "plugin_sha256": plugin_inputs,
    "harness_sha256": harness_sha,
    "kind": a.kind,
    "runner_sha256": digest(Path(__file__)),
    "plan_sha256": digest(a.plan),
    "workloads": workloads,
    "binaries": {
        k: {"path": str(getattr(a, k)), "sha256": digest(getattr(a, k))}
        for k in ["before", "after", "fork", "official"]
    },
    "environment": environment_info(a.python),
    "method": {
        "warmups": a.warmups,
        "repetitions": a.repetitions,
        "order": "Rotate fork/before/after each round and model count",
        "parallelism": "TY_MAX_PARALLELISM=1",
        "backends": a.backends,
        "checks": "Exact empty normalized diagnostics and zero exit from all three binaries; all original e2e positive files and generated assert_type contracts checked; every source byte/hash preserved",
    },
    "records": [],
}


def write():
    (a.output / "comparison.json").write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n"
    )


write()
counters = {}
output_baselines = {}
try:
    for phase, count in [("warmup", a.warmups), ("measured", a.repetitions)]:
        for iteration in range(count):
            for wi, w in enumerate(workloads):
                round_no = iteration + (a.warmups if phase == "measured" else 0)
                order = a.backends.copy()
                offset = (round_no + wi) % len(order)
                order = order[offset:] + order[:offset]
                for backend in order:
                    root = Path(w["project"])
                    label = (
                        f"{phase}-{iteration:02}-models-{w['extra_models']}-{backend}"
                    )
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
                        checker_mode=a.kind if a.kind != "public" else "cli",
                        timeout=a.timeout,
                    )
                    if a.kind == "public":
                        package = a.django / "python/django_ty"
                        entry = {
                            "id": "django-ty",
                            "runtime": "monty",
                            "trusted": True,
                            "path": str(package / "monty.py"),
                            "manifest-path": str(package / "ty-plugin-monty.json"),
                            "stub-overlay-path": str(package / "stubs"),
                        }
                        table = (
                            ", ".join(
                                f"{key} = {json.dumps(value)}"
                                for key, value in entry.items()
                            )
                            + ', config = {django-settings-module = "config.settings"}'
                        )
                        command = [
                            str(getattr(a, backend)),
                            "check",
                            *w["paths"],
                            "--project",
                            str(root),
                            "--python",
                            str(a.python),
                            "--python-version",
                            "3.13",
                            "--output-format",
                            "gitlab",
                            "--color",
                            "never",
                            "--no-progress",
                            "--config",
                            "plugins.enabled=true",
                            "--config",
                            "plugins.auto-discover=false",
                            "--config",
                            "plugins.plugin=[{" + table + "}]",
                            "--config",
                            "terminal.error-on-warning=true",
                        ]
                        if backend != "fork":
                            command += ["--ty", str(a.official)]
                        start = time.perf_counter()
                        proc = subprocess.run(
                            command,
                            cwd=root,
                            env=dict(
                                os.environ,
                                TY_MAX_PARALLELISM="1",
                                TY_UV="0",
                                PYTHONDONTWRITEBYTECODE="1",
                            ),
                            capture_output=True,
                            timeout=a.timeout,
                        )
                        elapsed = time.perf_counter() - start
                        dest = a.output / "runs" / label
                        dest.mkdir(parents=True)
                        (dest / "stdout").write_bytes(proc.stdout)
                        (dest / "stderr").write_bytes(proc.stderr)
                        assert proc.returncode == 0, (
                            label,
                            proc.returncode,
                            proc.stdout,
                            proc.stderr,
                        )
                        actual = normalize(json.loads(proc.stdout), root)
                        assert actual == [], (label, actual)
                        output_key = (
                            w["extra_models"],
                            "fork" if backend == "fork" else "companion",
                        )
                        output = (proc.stdout, proc.stderr)
                        if output_key in output_baselines:
                            assert output == output_baselines[output_key], (
                                label,
                                "raw stdout/stderr changed",
                            )
                        output_baselines[output_key] = output
                        details = {
                            "command": command,
                            "exit": proc.returncode,
                            "stdout_sha256": hashlib.sha256(proc.stdout).hexdigest(),
                            "stderr_sha256": hashlib.sha256(proc.stderr).hexdigest(),
                            "diagnostics": 0,
                        }
                    else:
                        actual = checker_run(
                            opts,
                            root,
                            w["paths"],
                            w["settings"],
                            label,
                            backend != "fork",
                        )
                        assert (
                            actual["passed"]
                            and actual["exit"] == 0
                            and actual["diagnostics"] == []
                        ), (label, actual)
                        elapsed = actual["seconds"]
                        metrics = actual["metrics"]
                        counts = {
                            k: v
                            for k, v in metrics.items()
                            if k
                            in [
                                "projection_rounds",
                                "oracle_rounds",
                                "monty_executions",
                                "monty_cache_hits",
                            ]
                        }
                        key = (w["extra_models"], backend)
                        if key in counters:
                            assert counters[key] == counts, (key, counters[key], counts)
                        counters[key] = counts
                        details = {
                            "exit": actual["exit"],
                            "diagnostics": 0,
                            "metrics": metrics,
                        }
                    report["records"].append(
                        {
                            "phase": phase,
                            "iteration": iteration,
                            "extra_models": w["extra_models"],
                            "backend": backend,
                            "seconds": elapsed,
                            "passed": True,
                            **details,
                        }
                    )
                    write()
                    print(label, f"{elapsed:.6f}", flush=True)
    check_sources()
    for key in ["before", "after", "fork", "official"]:
        assert digest(getattr(a, key)) == report["binaries"][key]["sha256"]
    assert environment_info(a.python) == report["environment"]
    assert plugin_inputs == {
        str(relative): digest(path)
        for relative, path in fixture_files(a.django / "python/django_ty").items()
    }
    assert digest(ROOT / "companion/scripts/django_acceptance.py") == harness_sha
    groups = defaultdict(list)
    for r in report["records"]:
        if r["phase"] == "measured":
            groups[(r["extra_models"], r["backend"])].append(r["seconds"])
    summary = {}
    for (models, backend), samples in groups.items():
        summary.setdefault(models, {})[backend] = {
            "median_seconds": statistics.median(samples),
            "samples": samples,
        }
    for rows in summary.values():
        if "before" in rows and "after" in rows:
            rows["after"]["speedup_over_before"] = (
                rows["before"]["median_seconds"] / rows["after"]["median_seconds"]
            )
        if "fork" in rows and "after" in rows:
            rows["after"]["ratio_over_fork"] = (
                rows["after"]["median_seconds"] / rows["fork"]["median_seconds"]
            )
    report.update(passed=True, summary=summary)
except BaseException as error:
    report["error"] = repr(error)
    write()
    raise
write()
print(json.dumps(summary, indent=2))
