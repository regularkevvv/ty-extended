#!/usr/bin/env python3
"""Compare public companion, MyPy + django-stubs, and bare official ty commands.

Creates a disposable, registered Django application. Every timed run uses fresh
processes and an empty MyPy cache. Filesystem caches may be warm. Bare ty lacks
the Django plugin semantics, so its expected ORM errors are recorded separately.
"""

from __future__ import annotations

import argparse
import collections
import contextlib
import hashlib
import json
import math
import os
import platform
import shutil
import signal
import statistics
import subprocess
import threading
import time
from pathlib import Path


def source_hash(project):
    digest = hashlib.sha256()
    for path in sorted(project.rglob("*.py")):
        digest.update(path.relative_to(project).as_posix().encode() + b"\0")
        digest.update(path.read_bytes())
    return digest.hexdigest()


def performance_gate(samples, max_ty_ratio=3.0):
    """Require the median and every rotated pair to meet the native ceiling."""
    if not math.isfinite(max_ty_ratio) or max_ty_ratio <= 0:
        raise ValueError("The Ty ratio limit must be finite and positive")
    if set(samples) != {"companion", "bare-ty", "mypy"}:
        raise ValueError("All three checker samples are required")
    counts = {len(values) for values in samples.values()}
    if len(counts) != 1 or counts == {0}:
        raise ValueError("Checker samples must have equal nonzero counts")
    if any(not math.isfinite(t) or t <= 0 for ts in samples.values() for t in ts):
        raise ValueError("Timings must be finite and positive")
    medians = {name: statistics.median(ts) for name, ts in samples.items()}
    ratios = [a / b for a, b in zip(samples["companion"], samples["bare-ty"])]
    median_ratio = medians["companion"] / medians["bare-ty"]
    ty_met = median_ratio <= max_ty_ratio and max(ratios) <= max_ty_ratio
    mypy_met = medians["companion"] <= medians["mypy"]
    return {
        "max_ty_ratio": max_ty_ratio,
        "median_ty_ratio": median_ratio,
        "paired_ty_ratios": ratios,
        "max_paired_ty_ratio": max(ratios),
        "ty_ceiling_met": ty_met,
        "mypy_ceiling_met": mypy_met,
        "one_second_goal_met": max(samples["companion"]) <= 1.0,
        "passed": ty_met and mypy_met,
    }


def memory(pid):
    output = subprocess.check_output(["ps", "-axo", "pid=,ppid=,rss="], text=True)
    rows = [list(map(int, row.split())) for row in output.splitlines()]
    owned = {pid}
    while True:
        following = owned | {child for child, parent, _ in rows if parent in owned}
        if following == owned:
            return sum(rss for child, _, rss in rows if child in owned)
        owned = following


def invoke(args, backend, command, label):
    folder = args.output / "runs" / label
    folder.mkdir(parents=True)
    scratch = folder / "scratch"
    scratch.mkdir()
    env = {
        **os.environ,
        "TY_UV": "0",
        "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONNOUSERSITE": "1",
        "TMPDIR": str(scratch),
        "XDG_CACHE_HOME": str(scratch / "cache"),
    }
    for key in (
        "TY_MAX_PARALLELISM",
        "RAYON_NUM_THREADS",
        "TY_CONFIG_FILE",
        "TY_COMPANION_TY",
        "DJANGO_SETTINGS_MODULE",
        "MYPYPATH",
        "PYTHONPATH",
    ):
        env.pop(key, None)
    if args.threads:
        env["TY_MAX_PARALLELISM"] = str(args.threads)
    if backend == "mypy":
        command = [*command, "--cache-dir", str(scratch / "mypy-cache")]
    command = list(map(str, command))
    before = source_hash(args.project)
    done = threading.Event()
    peak = 0
    stopped = None
    monitor_error = None
    load = os.getloadavg()
    with (folder / "stdout.json").open("wb") as stdout, (folder / "stderr.log").open(
        "wb"
    ) as stderr:
        started = time.perf_counter()
        process = subprocess.Popen(
            command,
            cwd=args.project,
            env=env,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )

        def monitor():
            nonlocal peak, stopped, monitor_error
            try:
                while not done.is_set():
                    peak = max(peak, memory(process.pid))
                    if (
                        peak > args.max_memory_mib * 1024
                        or time.perf_counter() - started > args.timeout
                    ):
                        stopped = (
                            "memory-limit"
                            if peak > args.max_memory_mib * 1024
                            else "timeout"
                        )
                        os.killpg(process.pid, signal.SIGKILL)
                        return
                    done.wait(0.1)
            except ProcessLookupError:
                pass
            except Exception as error:
                monitor_error = repr(error)
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)

        worker = threading.Thread(target=monitor)
        worker.start()
        try:
            _, status, usage = os.wait4(process.pid, 0)
            elapsed = time.perf_counter() - started
            process.returncode = os.waitstatus_to_exitcode(status)
        finally:
            done.set()
            worker.join()
            if process.returncode is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            shutil.rmtree(scratch)
    text = (folder / "stdout.json").read_text()
    parse_error = None
    try:
        diagnostics = (
            [json.loads(line) for line in text.splitlines() if line.strip()]
            if backend == "mypy"
            else json.loads(text)
        )
        if not isinstance(diagnostics, list) or not all(
            isinstance(item, dict) for item in diagnostics
        ):
            raise ValueError("expected a diagnostic array")
    except ValueError as error:
        parse_error = str(error)
        diagnostics = []
    codes = collections.Counter(
        item.get("code", item.get("check_name")) for item in diagnostics
    )
    row = {
        "backend": backend,
        "label": label,
        "command": command,
        "seconds": elapsed,
        "exit": process.returncode,
        "diagnostic_codes": dict(codes),
        "user_cpu_seconds": usage.ru_utime,
        "system_cpu_seconds": usage.ru_stime,
        "sampled_process_tree_peak_rss_kib": peak,
        "stopped": stopped,
        "monitor_error": monitor_error,
        "parse_error": parse_error,
        "load_before": load,
        "load_after": os.getloadavg(),
        "source_sha256": before,
        "source_preserved": before == source_hash(args.project),
    }
    (folder / "result.json").write_text(json.dumps(row, indent=2) + "\n")
    if stopped or monitor_error or parse_error or not row["source_preserved"]:
        raise RuntimeError(f"Invalid benchmark run: {row}")
    print(f"{label}: {elapsed:.3f}s, {peak / 1024:.1f} MiB, {dict(codes)}", flush=True)
    return row


def prepare(args):
    args.project = args.output / "project"
    app = args.project / "benchapp"
    app.mkdir(parents=True)
    (app / "__init__.py").write_text("")
    modules = [["from django.db import models\n"] for _ in range(args.model_modules)]
    exports = []
    for index in range(args.models):
        module = index % args.model_modules
        classes = modules[module]
        classes.append(
            f"class Item{index:04}(models.Model):\n    name = models.CharField(max_length=100)\n"
        )
        if index:
            classes.append(
                f'    parent = models.ForeignKey("Item{index - 1:04}", on_delete=models.CASCADE)\n'
            )
        exports.append(
            f"from .part{module:04} import Item{index:04} as Item{index:04}\n"
        )
    if args.model_modules == 1:
        (app / "models.py").write_text("\n".join(modules[0]))
        infrastructure_files = 4
    else:
        package = app / "models"
        package.mkdir()
        (package / "__init__.py").write_text("".join(exports))
        for index, classes in enumerate(modules):
            (package / f"part{index:04}.py").write_text("\n".join(classes))
        infrastructure_files = args.model_modules + 4
    (args.project / "settings.py").write_text(
        'INSTALLED_APPS = ["benchapp"]\nDEFAULT_AUTO_FIELD = "django.db.models.BigAutoField"\n'
    )
    for index in range(args.files - infrastructure_files):
        (args.project / f"filler{index:05}.py").write_text("value: int = 1\n")
    checks = "from typing import assert_type\nfrom benchapp import models\n"
    for index in range(args.models - args.queries, args.models):
        checks += f"assert_type(models.Item{index:04}.objects.get(id=1).name, str)\n"
    (args.project / "checks.py").write_text(checks)

    def q(path):
        return json.dumps(str(path))

    config = f"[environment]\npython={q(args.python)}\npython-version={q(args.python_version)}\n"
    (args.output / "bare.toml").write_text(config)
    config += (
        "[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\n"
        'id="django-ty"\nruntime="monty"\ntrusted=true\n'
        f"path={q(args.plugin / 'monty.py')}\nmanifest-path={q(args.plugin / 'ty-plugin.json')}\n"
        f"stub-overlay-path={q(args.plugin / 'stubs')}\n"
        '[plugins.plugin.config]\ndjango-settings-module="settings"\n'
    )
    (args.output / "companion.toml").write_text(config)
    (args.output / "mypy.ini").write_text(
        f"[mypy]\nplugins=mypy_django_plugin.main\npython_version={args.python_version}\n[mypy.plugins.django-stubs]\ndjango_settings_module=settings\n"
    )
    common = [
        "check",
        "--python",
        args.python,
        "--output-format",
        "gitlab",
        "--no-progress",
        "--color",
        "never",
        args.project,
    ]
    bare_common = list(common)
    if args.official_python:
        bare_common[2] = args.official_python
    return {
        "companion": [
            args.companion,
            *common,
            "--ty",
            args.official,
            "--config-file",
            args.output / "companion.toml",
        ],
        "bare-ty": [
            args.official,
            *bare_common,
            "--config-file",
            args.output / "bare.toml",
        ],
        "mypy": [
            args.mypy,
            "--config-file",
            args.output / "mypy.ini",
            "--output",
            "json",
            "--no-incremental",
            args.project,
        ],
    }


def verify_negative_controls(args, backend, row):
    text = (args.output / "runs" / row["label"] / "stdout.json").read_text()
    diagnostics = (
        [json.loads(line) for line in text.splitlines() if line.strip()]
        if backend == "mypy"
        else json.loads(text)
    )
    locations = []
    for item in diagnostics:
        if backend == "mypy":
            path, line = item["file"], item["line"]
        else:
            path = item["location"]["path"]
            line = item["location"]["positions"]["begin"]["line"]
        locations.append((str((args.project / path).resolve()), line))
    expected = sorted(
        [
            (str((args.project / "checks.py").resolve()), args.queries + 3),
            (str((args.project / "filler00000.py").resolve()), 1),
        ]
    )
    code = "assignment" if backend == "mypy" else "invalid-assignment"
    if (
        row["exit"] != 1
        or row["diagnostic_codes"] != {code: 2}
        or sorted(locations) != expected
    ):
        raise RuntimeError(f"Lost an ORM or ordinary Python error: {row}: {locations}")
    row["verified_negative_locations"] = [
        {"path": str(Path(path).relative_to(args.project.resolve())), "line": line}
        for path, line in expected
    ]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("companion", "official", "mypy", "python", "plugin", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--python-version", default="3.13")
    parser.add_argument("--models", type=int, default=2501)
    parser.add_argument("--model-modules", type=int, default=1)
    parser.add_argument("--files", type=int, default=7082)
    parser.add_argument("--queries", type=int, default=100)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--max-ty-ratio", type=float, default=3.0)
    parser.add_argument(
        "--official-python",
        type=Path,
        help="Bare Ty's environment containing django-types; enables native Django expectations",
    )
    parser.add_argument(
        "--threads", type=int, default=0, help="0 uses each checker's default"
    )
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--max-memory-mib", type=int, default=4096)
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("Process-tree RSS and wait4 timing require POSIX")
    if not (
        args.files >= 5
        and args.models >= args.queries >= 1
        and 1 <= args.model_modules <= args.models
        and args.files > (4 if args.model_modules == 1 else args.model_modules + 4)
        and args.repeats >= 1
        and args.threads >= 0
        and args.timeout > 0
        and args.max_memory_mib > 0
        and math.isfinite(args.max_ty_ratio)
        and args.max_ty_ratio > 0
    ):
        parser.error("Invalid workload or resource limits")
    for name in ("companion", "official", "mypy", "python", "plugin", "output"):
        # Preserve venv interpreter symlinks: resolving them changes sys.prefix.
        setattr(args, name, getattr(args, name).absolute())
    if args.official_python:
        args.official_python = args.official_python.absolute()
    args.output.mkdir(parents=True, exist_ok=False)
    commands = prepare(args)
    report = {
        "passed": False,
        "performance_target_met": False,
        "host": platform.platform(),
        "threads": args.threads or "default",
        "bare_ty_provider": "django-types" if args.official_python else "none",
        "workload": {
            key: getattr(args, key)
            for key in ("files", "models", "model_modules", "queries")
        },
        "source_sha256": source_hash(args.project),
        "method": "Fresh processes; warm filesystem caches; MyPy --no-incremental and disposable cache; rotated serial runs; process-tree RSS sampled every 100ms.",
        "binaries": {
            name: {
                "path": str(getattr(args, name)),
                "version": subprocess.check_output(
                    [str(getattr(args, name)), "--version"], text=True
                ).strip(),
                "sha256": hashlib.sha256(getattr(args, name).read_bytes()).hexdigest(),
            }
            for name in ("companion", "official", "mypy")
        },
        "records": [],
    }
    metadata_code = "import json,sys; from importlib.metadata import distributions; print(json.dumps({'python':sys.version,'packages':{d.metadata['Name']:d.version for d in distributions()}}))"
    report["environments"] = {
        name: json.loads(
            subprocess.check_output([str(python), "-c", metadata_code], text=True)
        )
        for name, python in (
            ("companion", args.python),
            ("mypy", args.mypy.parent / "python"),
            ("bare-ty", args.official_python or args.python),
        )
    }
    manifest = json.loads((args.plugin / "ty-plugin.json").read_text())
    report["plugin"] = {
        key: manifest[key] for key in ("id", "version", "protocol-version")
    }
    report["plugin_sha256"] = {
        str(path.relative_to(args.plugin)): hashlib.sha256(
            path.read_bytes()
        ).hexdigest()
        for path in sorted(args.plugin.rglob("*"))
        if path.is_file()
        and (path.suffix in (".py", ".pyi", ".json") or path.name == "py.typed")
    }
    try:
        backends = list(commands)
        for repeat in range(args.repeats):
            offset = repeat % len(backends)
            for backend in backends[offset:] + backends[:offset]:
                row = invoke(args, backend, commands[backend], f"{repeat}/{backend}")
                report["records"].append(row)
                expected = (
                    [
                        {"type-assertion-failure": args.queries, code: args.queries}
                        for code in ("unresolved-attribute", "missing-attribute")
                    ]
                    if backend == "bare-ty" and not args.official_python
                    else [{}]
                )
                if row["diagnostic_codes"] not in expected or row["exit"] != int(
                    backend == "bare-ty" and not args.official_python
                ):
                    raise RuntimeError(f"Unexpected diagnostics: {row}")
        report["median_seconds"] = {
            backend: statistics.median(
                row["seconds"] for row in report["records"] if row["backend"] == backend
            )
            for backend in backends
        }
        report["performance_gate"] = performance_gate(
            {
                backend: [
                    row["seconds"]
                    for row in report["records"]
                    if row["backend"] == backend
                ]
                for backend in backends
            },
            args.max_ty_ratio,
        )
        report["performance_target_met"] = report["performance_gate"]["passed"]
        checks = args.project / "checks.py"
        filler = args.project / "filler00000.py"
        original = checks.read_text(), filler.read_text()
        try:
            checks.write_text(
                original[0]
                + f"wrong: int = models.Item{args.models - 1:04}.objects.get(id=1).name\n"
            )
            filler.write_text("wrong: int = 'ordinary Python error'\n")
            report["negative_controls"] = []
            for backend in (
                ("companion", "bare-ty", "mypy")
                if args.official_python
                else ("companion", "mypy")
            ):
                row = invoke(args, backend, commands[backend], f"negative/{backend}")
                report["negative_controls"].append(row)
                verify_negative_controls(args, backend, row)
        finally:
            checks.write_text(original[0])
            filler.write_text(original[1])
        report["passed"] = True
    finally:
        (args.output / "benchmark.json").write_text(json.dumps(report, indent=2) + "\n")
    print(
        json.dumps(
            {
                key: report[key]
                for key in (
                    "passed",
                    "performance_target_met",
                    "median_seconds",
                    "performance_gate",
                )
            },
            indent=2,
        )
    )
    return 0 if report["performance_target_met"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
