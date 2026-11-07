#!/usr/bin/env python3
"""Run unchanged SDK/host tests with the companion's selected Monty runtime."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
from pathlib import Path

import tomllib


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    root = Path(__file__).resolve().parents[2]
    source = root / "ruff"
    packages = (
        "ty_plugin_protocol",
        "ty_plugin_sdk",
        "ty_plugin_examples",
        "ty_plugin_host",
    )
    locked = tomllib.loads((root / "companion/Cargo.lock").read_text())
    version = next(p["version"] for p in locked["package"] if p["name"] == "monty")
    workspace = output / "workspace"
    workspace.mkdir()
    hashes = {}
    for package in packages:
        directory = source / "crates" / package
        shutil.copytree(directory, workspace / "crates" / package)
        for path in sorted(directory.rglob("*")):
            if path.is_file():
                hashes[str(path.relative_to(source))] = hashlib.sha256(
                    path.read_bytes()
                ).hexdigest()
    manifest = (source / "Cargo.toml").read_text()
    for package in ("monty", "monty-types", "monty-pool"):
        manifest, count = re.subn(
            rf'(?m)^({package} = \{{ version = ")\S+?(".*)$',
            rf"\g<1>={version}\2",
            manifest,
        )
        if count != 1:
            raise RuntimeError(f"Expected one {package} workspace dependency")
    manifest += '\n[profile.runtime-tests]\ninherits = "dev"\ndebug = 0\n'
    (workspace / "Cargo.toml").write_text(manifest)
    shutil.copy2(root / "companion/Cargo.lock", workspace / "Cargo.lock")
    env = {
        **os.environ,
        "CARGO_TARGET_DIR": str(output / "target"),
        "CARGO_BUILD_JOBS": "2",
        "CARGO_INCREMENTAL": "0",
    }
    cargo = ["cargo", "+1.98.0"]
    report = {"passed": False, "monty_version": version, "source_sha256": hashes}
    try:
        # Resolve only this test workspace. The pinned submodule and its lockfile
        # remain unchanged, including the historical reference checker's runtime.
        subprocess.run(
            [*cargo, "update", "--package", "monty", "--precise", version],
            cwd=workspace,
            env=env,
            check=True,
        )
        runtime_lock = (workspace / "Cargo.lock").read_bytes()
        report["lock_sha256"] = hashlib.sha256(runtime_lock).hexdigest()
        versions = {
            p["name"]: p["version"]
            for p in tomllib.loads(runtime_lock.decode())["package"]
            if p["name"] in {"monty", "monty-types", "monty-macros"}
        }
        if set(versions.values()) != {version}:
            raise RuntimeError(f"Runtime versions differ: {versions}")
        subprocess.run(
            [
                *cargo,
                "test",
                "--locked",
                "--workspace",
                "--profile",
                "runtime-tests",
                "--no-default-features",
                "--features",
                "ty_plugin_host/plugins-monty,ty_plugin_host/mock",
            ],
            cwd=workspace,
            env=env,
            check=True,
        )
        report["passed"] = True
    finally:
        (output / "evidence.json").write_text(json.dumps(report, indent=2) + "\n")
        subprocess.run([*cargo, "clean"], cwd=workspace, env=env, check=True)


if __name__ == "__main__":
    main()
