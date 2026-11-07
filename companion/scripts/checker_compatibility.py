#!/usr/bin/env python3
"""Qualify unchanged companion binaries against explicitly identified ty releases."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

SCRIPTS = (
    "lsp_acceptance",
    "lsp_configuration",
    "lsp_differential",
    "lsp_lifecycle",
    "lsp_notebook_differential",
    "plugin_error_differential",
    "plugin_configuration_differential",
    "sdk_state_inventory",
    "sdk_function_inventory",
    "sdk_overlay_inventory",
    "sdk_cli_inventory",
    "sdk_class_inventory",
    "sdk_method_inventory",
    "sdk_class_summary_inventory",
    "sdk_minidjango_inventory",
)
WITHOUT_FORK = {"lsp_acceptance", "lsp_configuration", "lsp_lifecycle"}
GATES = ("generic-cli", "generic-lsp", "generic-push", *SCRIPTS)


def identity(binary: Path) -> dict:
    result = subprocess.run(
        [str(binary), "--version"],
        capture_output=True,
        text=True,
        check=True,
        timeout=30,
    )
    return {
        "path": str(binary),
        "sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "version": result.stdout.strip(),
    }


def run(command: list[str], log: Path, env: dict[str, str], timeout: int) -> dict:
    started = time.perf_counter()
    timed_out = False
    with log.open("wb") as stream:
        process = subprocess.Popen(
            command,
            stdout=stream,
            stderr=subprocess.STDOUT,
            env=env,
            start_new_session=os.name != "nt",
        )
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            if os.name == "nt":
                process.kill()
            else:
                os.killpg(process.pid, signal.SIGKILL)
            code = process.wait()
    return {
        "command": command,
        "exit": code,
        "timed_out": timed_out,
        "passed": code == 0 and not timed_out,
        "seconds": time.perf_counter() - started,
        "log": str(log),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--companion", required=True, type=Path)
    parser.add_argument("--fork", required=True, type=Path)
    parser.add_argument(
        "--official",
        action="append",
        required=True,
        metavar="VERSION=EXECUTABLE",
        help="Repeat for every released checker to qualify; executables are not rebuilt",
    )
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--gate", action="append", choices=GATES)
    parser.add_argument("--timeout", type=int, default=1200)
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    checkers = {}
    for entry in args.official:
        version, separator, binary = entry.partition("=")
        if not separator or not re.fullmatch(r"0\.0\.\d+", version):
            parser.error("--official requires 0.0.NUMBER=EXECUTABLE")
        if version in checkers:
            parser.error(f"duplicate checker version {version}")
        checkers[version] = Path(binary).resolve(strict=True)
    args.companion = args.companion.resolve(strict=True)
    args.fork = args.fork.resolve(strict=True)
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    repository = Path(__file__).resolve().parents[2]
    os.chdir(repository)
    env = {**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"}
    selected = args.gate or GATES
    report = {
        "passed": False,
        "complete": False,
        "requested_versions": list(checkers),
        "runner_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "companion": identity(args.companion),
        "fork": identity(args.fork),
        "source": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], text=True
        ).strip(),
        "diff_sha256": hashlib.sha256(
            subprocess.check_output(["git", "diff", "HEAD"])
        ).hexdigest(),
        "platform": sys.platform,
        "versions": [],
    }
    summary = args.output / "compatibility.json"

    def save():
        summary.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    save()
    try:
        for version, binary in checkers.items():
            root = args.output / version
            root.mkdir()
            checker = identity(binary)
            if not re.match(rf"ty {re.escape(version)}(?:\s|$)", checker["version"]):
                raise ValueError(f"checker version mismatch: {version}: {checker}")
            row = {"version": version, "checker": checker, "passed": False, "gates": []}
            report["versions"].append(row)
            save()
            for gate in selected:
                if gate.startswith("generic-"):
                    command = [
                        str(args.companion),
                        "suite",
                        str(repository / "companion/tests/fixtures/generic"),
                        str(root / gate),
                        gate.removeprefix("generic-"),
                        str(binary),
                        str(args.fork),
                    ]
                else:
                    command = [
                        sys.executable,
                        str(repository / f"companion/scripts/{gate}.py"),
                        "--companion",
                        str(args.companion),
                        "--official",
                        str(binary),
                        "--output",
                        str(root / gate),
                    ]
                    if gate not in WITHOUT_FORK:
                        command += ["--fork", str(args.fork)]
                print(f"{version} {gate}: running", flush=True)
                result = {
                    "name": gate,
                    **run(command, root / f"{gate}.log", env, args.timeout),
                }
                row["gates"].append(result)
                print(
                    f"{version} {gate}: {'PASS' if result['passed'] else 'FAIL'} ({result['seconds']:.1f}s)",
                    flush=True,
                )
                save()
            if identity(binary) != checker:
                raise RuntimeError(
                    f"official ty {version} changed during qualification"
                )
            row["passed"] = all(gate["passed"] for gate in row["gates"])
            save()
        report["passed"] = all(row["passed"] for row in report["versions"])
        if identity(args.companion) != report["companion"]:
            raise RuntimeError("companion executable changed during qualification")
        if identity(args.fork) != report["fork"]:
            raise RuntimeError("reference executable changed during qualification")
        report["complete"] = set(selected) == set(GATES)
    except BaseException as error:
        report["passed"] = False
        report["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        save()
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
