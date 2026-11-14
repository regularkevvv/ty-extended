#!/usr/bin/env python3
"""Build the immutable accepted timing reference, then clean its Cargo target."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
from pathlib import Path

REFERENCE_COMMIT = "7c3e9516b43016dea8a0f2396cd7fcbed742e63e"
MAX_REFERENCE_RATIO = 1.05
REPOSITORY = "https://github.com/regularkevvv/ty-extended.git"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.absolute()
    work = args.work_dir.absolute()
    if output.exists() or output.is_relative_to(work):
        parser.error("Use a new output path outside the disposable build directory")
    work.mkdir(parents=True, exist_ok=False)
    source = work / "source"
    target = work / "target"
    env = {
        **os.environ,
        "CARGO_TARGET_DIR": str(target),
        "CARGO_BUILD_JOBS": "2",
        "CARGO_INCREMENTAL": "0",
    }

    def run(command):
        subprocess.run(list(map(str, command)), env=env, check=True)

    manifest = source / "companion/Cargo.toml"
    try:
        run(["git", "init", source])
        run(
            ["git", "-C", source, "fetch", "--depth", "1", REPOSITORY, REFERENCE_COMMIT]
        )
        run(["git", "-C", source, "checkout", "--detach", "FETCH_HEAD"])
        actual = subprocess.check_output(
            ["git", "-C", str(source), "rev-parse", "HEAD"], text=True
        ).strip()
        assert actual == REFERENCE_COMMIT, actual
        run(
            [
                "cargo",
                "+1.98.0",
                "build",
                "--locked",
                "--release",
                "--manifest-path",
                manifest,
                "--bin",
                "ty-extended",
            ]
        )
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(target / "release/ty-extended", output)
        output.with_suffix(".provenance.json").write_text(
            json.dumps(
                {
                    "source_commit": actual,
                    "binary_sha256": hashlib.sha256(output.read_bytes()).hexdigest(),
                    "cargo_lock_sha256": hashlib.sha256(
                        (source / "Cargo.lock").read_bytes()
                    ).hexdigest(),
                    "toolchain": "1.98.0",
                },
                indent=2,
            )
            + "\n"
        )
    finally:
        # Keep evidence and the executable outside this task-owned directory.
        # A failed clean remains a failure and preserves the directory for review.
        if manifest.exists():
            run(["cargo", "+1.98.0", "clean", "--manifest-path", manifest])
        shutil.rmtree(work)


if __name__ == "__main__":
    main()
