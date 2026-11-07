#!/usr/bin/env python3
"""Alternate independently initialized before/after editor sessions; verify every edit."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import statistics
import subprocess
import sys
from pathlib import Path

ROOT = Path(os.environ.get("TY_COMPANION_REPO", Path.cwd())).resolve()
sys.path.insert(0, str(ROOT / "companion/scripts"))
from django_acceptance import (  # noqa: E402 - selected repository harness
    digest,
    environment_info,
    fixture_files,
    inventory,
    load_module,
)

p = argparse.ArgumentParser()
for name in ["before", "after", "official", "django", "python", "acceptance", "output"]:
    p.add_argument("--" + name, type=Path, required=True)
p.add_argument("--pairs", type=int, default=3)
a = p.parse_args()
assert a.pairs >= 1 and not a.output.exists()
a.output.mkdir(parents=True)
harnesses = {
    str(ROOT / "companion/scripts" / name): digest(ROOT / "companion/scripts" / name)
    for name in ["lsp_django.py", "lsp_acceptance.py", "django_acceptance.py"]
}
worker = Path(__file__).with_name("editor_capture_worker.py")
harnesses[str(worker)] = digest(worker)
report = {
    "passed": False,
    "harness_sha256": harnesses,
    "runner_sha256": digest(Path(__file__)),
    "binaries": {
        k: {"path": str(getattr(a, k)), "sha256": digest(getattr(a, k))}
        for k in ["before", "after", "official"]
    },
    "environment": environment_info(a.python),
    "method": {
        "order": "before/after then after/before alternating by session pair",
        "sessions_per_binary": a.pairs,
        "each_session": "Full unchanged Django editor conformance; ten comment edits then ten semantic edits; first three edits per kind excluded from medians; every edit verified; shutdown verified",
        "parallelism": "TY_MAX_PARALLELISM=1",
        "python_hash_seed": "0 for stable fixture-open traversal",
        "timer": "Before didChange notification write through the matching diagnostic response; all warmup and measured updates retained",
    },
    "sessions": [],
}
accepted = json.loads((a.acceptance / "acceptance.json").read_text())
assert accepted["passed"] and accepted["compatibility_passed"]
assert (
    accepted["environment"] == report["environment"]
    and accepted["binaries"]["official"]["sha256"]
    == report["binaries"]["official"]["sha256"]
)
sys.path.insert(0, str(a.django / "scripts"))
evaluator = load_module(
    "speed_editor_evaluator", a.django / "scripts/evaluate_differential_conformance.py"
)
inputs = inventory(a.django, evaluator)
assert (
    inputs["source_sha256"]
    == json.loads((a.acceptance / "inputs.json").read_text())["source_sha256"]
)
report["input_sha256"] = inputs["source_sha256"]
report["acceptance_sha256"] = digest(a.acceptance / "acceptance.json")
source_root = a.acceptance / "inputs/conformance"
staged = {str(rel): digest(path) for rel, path in fixture_files(source_root).items()}
payload_baseline = None
counter_baselines = {}


def work_counters(statistics):
    def counters(value):
        return {
            key: item
            for key, item in value.items()
            if isinstance(item, (int, bool))
            or key in {"checker_mode", "checker_mode_effective", "reuse_reason"}
        }

    return [
        {
            **counters(update),
            "projects": [counters(project) for project in update["projects"]],
        }
        for update in statistics["updates"]
    ]


def write():
    (a.output / "comparison.json").write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n"
    )


write()
try:
    for pair in range(a.pairs):
        for backend in ["before", "after"] if pair % 2 == 0 else ["after", "before"]:
            out = a.output / f"{pair:02}-{backend}"
            cmd = [
                sys.executable,
                "-B",
                str(worker),
                "--companion",
                str(getattr(a, backend)),
                "--official",
                str(a.official),
                "--django",
                str(a.django),
                "--python",
                str(a.python),
                "--acceptance",
                str(a.acceptance),
                "--output",
                str(out),
                "--python-version",
                "3.13",
            ]
            with (a.output / f"{pair:02}-{backend}.log").open("w") as stream:
                result = subprocess.run(
                    cmd,
                    env=dict(
                        os.environ,
                        TY_MAX_PARALLELISM="1",
                        TY_UV="0",
                        PYTHONDONTWRITEBYTECODE="1",
                        PYTHONHASHSEED="0",
                        TY_SPEED_EDITOR_CAPTURE=str(
                            a.output / f"{pair:02}-{backend}-payloads.json"
                        ),
                    ),
                    stdout=stream,
                    stderr=subprocess.STDOUT,
                    timeout=600,
                )
            assert result.returncode == 0, (backend, result.returncode)
            evidence = json.loads((out / "evidence.json").read_text())
            assert (
                evidence["passed"]
                and evidence["companion_sha256"]
                == report["binaries"][backend]["sha256"]
            )
            counters = work_counters(evidence["statistics"])
            if backend in counter_baselines:
                assert counters == counter_baselines[backend], (
                    pair,
                    backend,
                    "editor work counters changed between sessions",
                )
            counter_baselines[backend] = counters
            payload_file = a.output / f"{pair:02}-{backend}-payloads.json"
            payloads = json.loads(payload_file.read_text())
            assert payloads
            update_file = payload_file.with_name(payload_file.stem + "-updates.json")
            update_samples = json.loads(update_file.read_text())
            assert [sample["version"] for sample in update_samples] == list(
                range(2, 22)
            )
            for kind in ["comment", "semantic"]:
                for phase, count in [("warmup", 3), ("measured", 7)]:
                    assert (
                        sum(
                            sample["kind"] == kind and sample["phase"] == phase
                            for sample in update_samples
                        )
                        == count
                    )
            if payload_baseline is None:
                payload_baseline = payloads
            else:
                assert payloads == payload_baseline, (
                    pair,
                    backend,
                    "exact editor diagnostic payload sequence mismatch",
                )
            row = {
                "payloads_sha256": digest(payload_file),
                "payload_count": len(payloads),
                "update_samples_sha256": digest(update_file),
                "update_samples": update_samples,
                "counter_sequence_sha256": hashlib.sha256(
                    json.dumps(counters, sort_keys=True).encode()
                ).hexdigest(),
                "pair": pair,
                "backend": backend,
                "command": cmd,
                "evidence_sha256": digest(out / "evidence.json"),
                "evidence": evidence,
            }
            report["sessions"].append(row)
            write()
            print(
                pair,
                backend,
                evidence["warm_comment_median_seconds"],
                evidence["warm_semantic_median_seconds"],
                flush=True,
            )
    for key in ["before", "after", "official"]:
        assert digest(getattr(a, key)) == report["binaries"][key]["sha256"]
    assert environment_info(a.python) == report["environment"]
    assert inputs == inventory(a.django, evaluator)
    assert staged == {
        str(rel): digest(path) for rel, path in fixture_files(source_root).items()
    }
    for path, sha in harnesses.items():
        assert digest(Path(path)) == sha, path
    summary = {}
    for kind in ["comment", "semantic"]:
        summary[kind] = {}
        for backend in ["before", "after"]:
            samples = [
                sample["seconds"]
                for row in report["sessions"]
                if row["backend"] == backend
                for sample in row["update_samples"]
                if sample["kind"] == kind and sample["phase"] == "measured"
            ]
            summary[kind][backend] = {
                "samples": samples,
                "median_seconds": statistics.median(samples),
            }
        summary[kind]["after"]["speedup_over_before"] = (
            summary[kind]["before"]["median_seconds"]
            / summary[kind]["after"]["median_seconds"]
        )
    report.update(passed=True, summary=summary)
except BaseException as error:
    report["error"] = repr(error)
    write()
    raise
write()
print(json.dumps(summary, indent=2))
