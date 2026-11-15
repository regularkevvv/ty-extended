#!/usr/bin/env python3
"""Run the upstream pytest-mypy-plugins YAML corpus through tyx.

Each case's `main` source is written to a temporary project that registers
the returns-ty plugin, then `tyx check` output is classified:

- parity: every expected reveal produced a reveal diagnostic and every
  expected error line produced a diagnostic
- divergence: the case ran cleanly but produced different diagnostics
- error: the plugin or tyx crashed

Results are compared against expected.json; --check fails on regressions.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
PLUGIN = ROOT.parent
REPO = PLUGIN.parent.parent
CASES = ROOT / "cases"
EXPECTED = ROOT / "expected.json"

NOTE = re.compile(r"#\s*N:\s*Revealed type is", re.IGNORECASE)
ERROR = re.compile(r"#\s*E:", re.IGNORECASE)
REVEAL = re.compile(r"revealed-type.*line (\d+)|Revealed type", re.IGNORECASE)


def tyx_binary() -> str:
    if binary := os.environ.get("TYX"):
        return binary
    for candidate in (
        REPO / "crates/ty_extended/target/debug/tyx",
        REPO / "target/debug/tyx",
        REPO / "target/release/tyx",
    ):
        if candidate.is_file():
            return str(candidate)
    raise SystemExit("no tyx binary found; build it or set TYX")


def load_cases() -> list[tuple[str, str, str]]:
    try:
        import yaml
    except ImportError:
        raise SystemExit("run with `uv run --with pyyaml` so PyYAML is available")
    cases = []
    for path in sorted(CASES.glob("*.yml")):
        for document in yaml.safe_load_all(path.read_text()):
            for case in document or []:
                if isinstance(case, dict) and case.get("main") and not case.get("skip"):
                    cases.append((path.name, case.get("case", "unnamed"), case["main"]))
    return cases


def run_case(tyx: str, venv: Path, directory: Path, main: str) -> dict:
    (directory / "main.py").write_text(main)
    (directory / "pyproject.toml").write_text(
        '[project]\nname = "returns-ty-case"\nrequires-python = ">=3.13"\n'
        f'[tool.ty.environment]\npython = "{venv}/bin/python"\n'
        "[tool.ty.plugins]\nenabled = true\nauto-discover = false\n"
        '[[tool.ty.plugins.plugin]]\nid = "returns-ty"\nruntime = "monty"\n'
        "trusted = true\n"
        f'path = "{PLUGIN}/python/returns_ty/monty.py"\n'
        f'manifest-path = "{PLUGIN}/python/returns_ty/ty-plugin.json"\n'
    )
    completed = subprocess.run(
        [tyx, "check"],
        cwd=directory,
        capture_output=True,
        text=True,
        timeout=120,
    )
    output = completed.stdout + completed.stderr
    reveal_lines: set[int] = set()
    diagnostics: set[int] = set()
    pending: str | None = None
    for line in output.splitlines():
        if "revealed-type" in line:
            pending = "reveal"
        elif re.search(r"(?:error|warning)\[", line):
            pending = "diagnostic"
        location = re.search(r"main\.py:(\d+):\d+", line)
        if location and pending == "reveal":
            reveal_lines.add(int(location.group(1)))
        elif location and pending == "diagnostic":
            diagnostics.add(int(location.group(1)))
    crashed = "plugin" in output.lower() and (
        "panic" in output.lower() or "failed to" in output.lower()
    )
    return {
        "output": output,
        "reveal_lines": reveal_lines,
        "diagnostic_lines": diagnostics,
        "crashed": crashed or completed.returncode > 5,
    }


def classify(main: str, result: dict) -> str:
    if result["crashed"]:
        return "error"
    expected_reveals = {
        number
        for number, line in enumerate(main.splitlines(), start=1)
        if NOTE.search(line)
    }
    expected_errors = {
        number
        for number, line in enumerate(main.splitlines(), start=1)
        if ERROR.search(line)
    }
    if expected_reveals - result["reveal_lines"]:
        return "divergence"
    if expected_errors - result["diagnostic_lines"]:
        return "divergence"
    return "parity"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check", action="store_true", help="fail on regressions vs expected.json"
    )
    parser.add_argument("--update", action="store_true", help="rewrite expected.json")
    parser.add_argument(
        "--filter", default="", help="only run cases matching this substring"
    )
    parser.add_argument(
        "--verbose", action="store_true", help="print tyx output per case"
    )
    args = parser.parse_args()

    tyx = tyx_binary()
    cases = load_cases()
    workdir = Path(tempfile.mkdtemp(prefix="returns-ty-conformance-"))
    venv = workdir / ".venv"
    subprocess.run(["uv", "venv", "--python", "3.13", str(venv)], check=True)
    subprocess.run(
        ["uv", "pip", "install", "--python", str(venv / "bin/python"), "returns"],
        check=True,
    )

    expected = json.loads(EXPECTED.read_text()) if EXPECTED.exists() else {}
    results: dict[str, str] = {}
    regressions: list[str] = []
    for filename, name, main_source in cases:
        key = f"{filename}::{name}"
        if args.filter and args.filter not in key:
            continue
        directory = workdir / re.sub(r"[^A-Za-z0-9_]+", "_", key)
        directory.mkdir(parents=True, exist_ok=True)
        try:
            result = run_case(tyx, venv, directory, main_source)
            verdict = classify(main_source, result)
        except Exception:
            verdict = "error"
            result = None
        if args.verbose and result is not None:
            print(result["output"])
        if args.verbose and result is None:
            import traceback

            traceback.print_exc()
        results[key] = verdict
        if args.check and verdict != expected.get(key, "divergence"):
            if verdict == "error" or expected.get(key) == "parity":
                regressions.append(key)
        print(f"{verdict:>10}  {key}")

    if args.update or not EXPECTED.exists():
        EXPECTED.write_text(json.dumps(results, indent=2, sort_keys=True) + "\n")

    counts = {}
    for verdict in results.values():
        counts[verdict] = counts.get(verdict, 0) + 1
    print(f"\nsummary: {counts}")
    if args.check and regressions:
        print(f"regressions: {regressions}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
