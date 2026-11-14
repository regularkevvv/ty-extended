"""Verify that builds use upstream libraries and contain no checker fork."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main() -> None:
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT
        )
    )
    packages = metadata["packages"]
    forbidden = {"ty_python_semantic", "ty_project", "salsa", "wasmtime", "monty-pool"}
    found = forbidden & {package["name"] for package in packages}
    if found:
        raise RuntimeError(f"Unexpected build dependencies: {sorted(found)}")
    for package in packages:
        if "ruff-extended" in (package.get("source") or ""):
            raise RuntimeError("Build still depends on ruff-extended")
        if package["name"].startswith("ruff_"):
            if not (package.get("source") or "").startswith("registry+"):
                raise RuntimeError(
                    f"Ruff library is not a registry dependency: {package['name']}"
                )
            repository = (package.get("repository") or "").removesuffix(".git")
            if repository != "https://github.com/astral-sh/ruff":
                raise RuntimeError(f"Unexpected Ruff library origin: {repository}")
    if (ROOT / ".gitmodules").exists():
        raise RuntimeError("This workspace must not require Git submodules")
    local = sorted(package["name"] for package in packages if package["source"] is None)
    print(
        f"Verified upstream registry dependencies and local packages: {', '.join(local)}"
    )


if __name__ == "__main__":
    main()
