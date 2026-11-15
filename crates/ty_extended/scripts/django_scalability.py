#!/usr/bin/env python3
"""Bounded POSIX regression for django-ty issue #8: large real checker projects."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import signal
import subprocess
import time
from pathlib import Path

from checker_compatibility import identity


def source_hash(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.is_file():
            digest.update(str(path.relative_to(root)).encode())
            digest.update(b"\0")
            digest.update(path.read_bytes())
    return digest.hexdigest()


def process_memory(pid: int) -> tuple[int, list[list[int]]]:
    data = subprocess.check_output(["ps", "-axo", "pid=,ppid=,rss="], text=True)
    rows = [list(map(int, line.split())) for line in data.splitlines()]
    owned = {pid}
    while True:
        following = owned | {child for child, parent, _ in rows if parent in owned}
        if following == owned:
            break
        owned = following
    selected = [row for row in rows if row[0] in owned]
    return sum(row[2] for row in selected), selected


def check(
    command: list[str], root: Path, output: Path, *, timeout: int, max_mib: int
) -> dict:
    scratch = output / "scratch"
    scratch.mkdir()
    stdout = output / "stdout.json"
    stderr = output / "stderr.log"
    before = source_hash(root)
    started = time.perf_counter()
    peak = 0
    processes = []
    stopped = None
    env = {
        **os.environ,
        "TY_UV": "0",
        "TY_MAX_PARALLELISM": "1",
        "TMPDIR": str(scratch),
    }
    with stdout.open("wb") as stream, stderr.open("wb") as errors:
        process = subprocess.Popen(
            command,
            cwd=root,
            env=env,
            stdout=stream,
            stderr=errors,
            start_new_session=True,
        )
        try:
            while process.poll() is None:
                memory, current = process_memory(process.pid)
                if memory > peak:
                    peak, processes = memory, current
                if memory > max_mib * 1024 or time.perf_counter() - started > timeout:
                    stopped = "memory-limit" if memory > max_mib * 1024 else "timeout"
                    os.killpg(process.pid, signal.SIGKILL)
                    break
                time.sleep(0.2)
            code = process.wait()
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            # Includes projections left by a killed process. Only this run's
            # private scratch directory is removed, never shared build state.
            shutil.rmtree(scratch)
    return {
        "command": command,
        "exit": code,
        "seconds": time.perf_counter() - started,
        "peak_process_tree_rss_kib": peak,
        "peak_processes_pid_ppid_rss_kib": processes,
        "stopped": stopped,
        "source_sha256": before,
        "source_preserved": before == source_hash(root),
        "stdout": str(stdout),
        "stderr": str(stderr),
    }


def generate(args) -> tuple[Path, str]:
    root = args.output / "project"
    root.mkdir()
    package = args.django / "python/django_ty"

    def q(path):
        return json.dumps(str(path))

    (root / "ty.toml").write_text(
        f"[environment]\npython = {q(args.python)}\npython-version = {q(args.python_version)}\n"
        "[plugins]\nenabled = true\nauto-discover = false\n[[plugins.plugin]]\n"
        'id = "django-ty"\nruntime = "monty"\ntrusted = true\n'
        f"path = {q(package / 'monty.py')}\nmanifest-path = {q(package / 'ty-plugin-monty.json')}\n"
        f'stub-overlay-path = {q(package / "stubs")}\n[plugins.plugin.config]\ndjango-settings-module = "settings"\n'
    )
    (root / "settings.py").write_text(
        'INSTALLED_APPS = []\nDEFAULT_AUTO_FIELD = "django.db.models.BigAutoField"\n'
    )
    classes = ["from django.db import models\n"]
    for index in range(args.models):
        classes.append(
            f"class Item{index:04}(models.Model):\n    name = models.CharField(max_length=100)\n"
        )
        if index:
            classes.append(
                f'    parent = models.ForeignKey("Item{index - 1:04}", on_delete=models.CASCADE)\n'
            )
    (root / "models.py").write_text("\n".join(classes))
    last = f"Item{args.models - 1:04}"
    positive = f"from typing import assert_type\nfrom models import {last}\nassert_type({last}.objects.get(id=1).name, str)\n"
    (root / "checks.py").write_text(positive)
    for index in range(args.files - 3):
        (root / f"filler{index:05}.py").write_text("value: int = 1\n")
    return root, positive


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("companion", "official", "django", "python", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--python-version", default="3.13")
    parser.add_argument("--files", type=int, default=7082)
    parser.add_argument("--models", type=int, default=2501)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--max-memory-mib", type=int, default=3072)
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("RSS qualification requires POSIX ps and process groups")
    if args.files < 3 or args.models < 1 or args.timeout < 1 or args.max_memory_mib < 1:
        parser.error(
            "files must be at least 3; models, timeout and memory must be positive"
        )
    for name in ("companion", "official", "django", "python"):
        setattr(args, name, getattr(args, name).resolve(strict=True))
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    root, positive = generate(args)
    report = {
        "passed": False,
        "complete": False,
        "files": args.files,
        "models": args.models,
        "companion": identity(args.companion),
        "official": identity(args.official),
        "timeout_seconds": args.timeout,
        "max_memory_mib": args.max_memory_mib,
        "runs": [],
    }
    summary = args.output / "scalability.json"

    def save():
        summary.write_text(json.dumps(report, indent=2) + "\n")

    save()
    try:
        for negative in (False, True):
            if negative:
                last = f"Item{args.models - 1:04}"
                (root / "checks.py").write_text(
                    positive + f"bad: int = {last}.objects.get(id=1).name\n"
                )
            output = args.output / ("negative" if negative else "positive")
            output.mkdir()
            command = [
                str(args.companion),
                "check",
                "--ty",
                str(args.official),
                "--color",
                "never",
                "--no-progress",
                "--output-format",
                "gitlab",
                str(root),
            ]
            row = check(
                command, root, output, timeout=args.timeout, max_mib=args.max_memory_mib
            )
            row["negative"] = negative
            try:
                diagnostics = json.loads(Path(row["stdout"]).read_text())
                codes = [item["check_name"] for item in diagnostics]
                row["diagnostic_codes"] = codes
                row["passed"] = (
                    row["exit"] == int(negative)
                    and row["stopped"] is None
                    and row["source_preserved"]
                    and (
                        codes == ["invalid-assignment"]
                        and diagnostics[0]["location"]["positions"]["begin"]["line"]
                        == len(positive.splitlines()) + 1
                        if negative
                        else diagnostics == []
                    )
                )
            except (ValueError, KeyError, TypeError) as error:
                row.update(passed=False, error=f"{type(error).__name__}: {error}")
            report["runs"].append(row)
            save()
            print(
                f"{'negative' if negative else 'positive'}: {'PASS' if row['passed'] else 'FAIL'}; {row['seconds']:.2f}s; {row['peak_process_tree_rss_kib'] / 1024:.1f} MiB combined peak RSS",
                flush=True,
            )
            if not row["passed"]:
                break
        if (
            identity(args.companion) != report["companion"]
            or identity(args.official) != report["official"]
        ):
            raise RuntimeError("an executable changed during qualification")
        report["complete"] = len(report["runs"]) == 2
        report["passed"] = report["complete"] and all(
            row["passed"] for row in report["runs"]
        )
    except BaseException as error:
        report.update(passed=False, error=f"{type(error).__name__}: {error}")
        raise
    finally:
        save()
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
