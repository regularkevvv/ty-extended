"""Check installed-wheel Monty defaults and optional worker execution on POSIX."""

# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from ty import find_ty_bin

EXAMPLE = Path(__file__).resolve().parent.parent / "examples" / "monty"


def check_plugin(
    binary: str, worker: Path, *, worker_mode: bool = False, succeeds: bool = True
) -> subprocess.CompletedProcess[str]:
    args = [binary, "check", "--color", "never"]
    if worker_mode:
        args.extend(["--config", 'plugins.monty-mode="worker"'])
    result = subprocess.run(
        args,
        cwd=EXAMPLE,
        env={**os.environ, "TY_MONTY_BIN": str(worker)},
        capture_output=True,
        text=True,
        timeout=60,
    )
    output = result.stdout + result.stderr
    if succeeds:
        if result.returncode != 0 or "`Token`" not in output:
            raise AssertionError(f"Monty plugin did not infer Token:\n{output}")
    elif result.returncode != 1 or "failed to start monty worker pool" not in output:
        raise AssertionError(
            f"Missing worker did not produce a plugin error:\n{output}"
        )
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workers", action="store_true")
    args = parser.parse_args()
    binary = find_ty_bin()

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        receipt = root / "worker-started"
        launcher = root / "monty"
        launcher.write_text(
            f"#!{sys.executable}\n"
            f"open({str(receipt)!r}, 'w').close()\n"
            "raise SystemExit(3)\n",
            encoding="utf-8",
        )
        launcher.chmod(0o700)
        check_plugin(binary, launcher)
        if receipt.exists():
            raise AssertionError("Default execution attempted to start a worker")
        check_plugin(binary, root / "missing-worker")
        check_plugin(binary, root / "missing-worker", worker_mode=True, succeeds=False)

        if args.workers:
            worker = shutil.which("monty", path=str(Path(sys.executable).parent))
            if worker is None:
                raise AssertionError(
                    "The worker extra did not supply a Monty executable"
                )
            launcher.write_text(
                f"#!{sys.executable}\n"
                "import os, sys\n"
                f"open({str(receipt)!r}, 'w').close()\n"
                f"os.execv({worker!r}, [{worker!r}, *sys.argv[1:]])\n",
                encoding="utf-8",
            )
            check_plugin(binary, launcher, worker_mode=True)
            if not receipt.is_file():
                raise AssertionError("Worker mode did not execute the worker")

    print("Installed wheel: embedded default and explicit worker selection passed")


if __name__ == "__main__":
    main()
