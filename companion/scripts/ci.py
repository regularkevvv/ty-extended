#!/usr/bin/env python3
"""Run companion regression gates with separately installed reference checkers."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import urllib.request
import venv
from pathlib import Path


def run(command: list[str], *, env: dict[str, str]) -> None:
    print("+", " ".join(command), flush=True)
    subprocess.run(command, check=True, env=env)


def install(root: Path, package: str, env: dict[str, str]) -> Path:
    venv.EnvBuilder(with_pip=True).create(root)
    scripts = root / ("Scripts" if os.name == "nt" else "bin")
    python = scripts / ("python.exe" if os.name == "nt" else "python")
    run(
        [str(python), "-m", "pip", "install", "--disable-pip-version-check", package],
        env=env,
    )
    return scripts / ("ty.exe" if os.name == "nt" else "ty")


def reference_checker(output: Path, env: dict[str, str]) -> Path:
    if os.name != "nt":
        return install(output / "fork", "ty-extended==0.86.0", env)
    # The 0.86.0 publication has no Windows wheel. Build the exact pinned
    # reference engine instead of silently testing a different release.
    target = output / "reference-target"
    run(
        [
            "cargo",
            "+1.98.0",
            "build",
            "--locked",
            "--release",
            "--manifest-path",
            "ruff/Cargo.toml",
            "--package",
            "ty",
            "--features",
            "plugins-monty",
        ],
        env={**env, "CARGO_TARGET_DIR": str(target)},
    )
    return target / "release/ty.exe"


def django_gate(output: Path, binary: Path, env: dict[str, str]) -> None:
    """The real Django contract on Linux; the full version matrix also runs locally."""
    source = output / "django-source"
    run(["git", "init", str(source)], env=env)
    run(
        [
            "git",
            "-C",
            str(source),
            "fetch",
            "--depth",
            "1",
            "https://github.com/regularkevvv/django-ty.git",
            "9996e1b3e8a0656c988fd2b8deedb726b12fe575",
        ],
        env=env,
    )
    run(["git", "-C", str(source), "checkout", "--detach", "FETCH_HEAD"], env=env)
    oracle = output / "django-oracle.jsonl"
    with gzip.open("companion/evidence/protocol-inputs/oracle.jsonl.gz", "rb") as data:
        oracle.write_bytes(data.read())
    run(
        [
            "cargo",
            "+1.98.0",
            "test",
            "--locked",
            "--release",
            "--manifest-path",
            "companion/Cargo.toml",
            "--test",
            "django_monty_parity",
            "--",
            "--ignored",
            "--nocapture",
        ],
        env={**env, "DJANGO_TY_SOURCE": str(source), "DJANGO_TY_ORACLE": str(oracle)},
    )
    official = install(output / "official084", "ty==0.0.84", env)
    archive = output / "reference084.tar.gz"
    urllib.request.urlretrieve(
        "https://github.com/regularkevvv/ty-extended/releases/download/v0.84.4/ty-x86_64-unknown-linux-gnu.tar.gz",
        archive,
    )
    actual = hashlib.sha256(archive.read_bytes()).hexdigest()
    expected = "3bb73d2dc9b7734b41e91bf154776f5849c81e45340c174bcc2971411d9e626b"
    if actual != expected:
        raise RuntimeError(f"reference checker archive hash changed: {actual}")
    reference = output / "reference084"
    with tarfile.open(archive) as compressed:
        compressed.extractall(reference, filter="data")
    fork = reference / "ty-x86_64-unknown-linux-gnu/ty"
    environment = output / "django-environment"
    venv.EnvBuilder(with_pip=True).create(environment)
    python = environment / "bin/python"
    run(
        [
            str(python),
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "-r",
            str(source / "conformance/candidate-requirements.txt"),
        ],
        env=env,
    )
    for mode in ("cli", "push"):
        run(
            [
                sys.executable,
                "companion/scripts/django_acceptance.py",
                "compare",
                "--django",
                str(source),
                "--output",
                str(output / f"django-{mode}"),
                "--fork",
                str(fork),
                "--official",
                str(official),
                "--companion",
                str(binary),
                "--python",
                str(environment),
                "--python-version",
                "3.13",
                "--checker-mode",
                mode,
            ],
            env=env,
        )
    run(
        [
            sys.executable,
            "companion/scripts/lsp_django.py",
            "--django",
            str(source),
            "--acceptance",
            str(output / "django-push"),
            "--output",
            str(output / "django-editor"),
            "--official",
            str(official),
            "--companion",
            str(binary),
            "--python",
            str(environment),
            "--python-version",
            "3.13",
        ],
        env=env,
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    output = args.output.resolve()
    env = {**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"}
    run(
        [
            sys.executable,
            "-m",
            "unittest",
            "discover",
            "-s",
            "companion/tests",
            "-p",
            "test_*.py",
        ],
        env=env,
    )
    manifest = Path("companion/Cargo.toml").resolve()
    cargo = ["cargo", "+1.98.0"]
    run([*cargo, "fmt", "--manifest-path", str(manifest), "--check"], env=env)
    run(
        [
            *cargo,
            "clippy",
            "--locked",
            "--manifest-path",
            str(manifest),
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        env=env,
    )
    official = install(output / "official", "ty==0.0.86", env)
    fork = reference_checker(output, env)
    env.update(
        TY_COMPANION_TEST_OFFICIAL=str(official), TY_COMPANION_TEST_FORK=str(fork)
    )
    run([*cargo, "test", "--locked", "--manifest-path", str(manifest)], env=env)
    run(
        [
            *cargo,
            "test",
            "--locked",
            "--manifest-path",
            str(manifest),
            "--bin",
            "ty-companion-rust",
            "--",
            "--ignored",
        ],
        env=env,
    )
    for test in (
        "generic_project_semantics",
        "sdk_extended_semantics",
        "sdk_multi_semantics",
        "cli_acceptance",
        "cli_edits",
        "cli_watch",
    ):
        run(
            [
                *cargo,
                "test",
                "--locked",
                "--manifest-path",
                str(manifest),
                "--test",
                test,
                "--",
                "--ignored",
            ],
            env=env,
        )
    run(
        [*cargo, "build", "--locked", "--release", "--manifest-path", str(manifest)],
        env=env,
    )
    target = Path(env.get("CARGO_TARGET_DIR", "companion/target"))
    binary = (
        target
        / "release"
        / ("ty-companion-rust.exe" if os.name == "nt" else "ty-companion-rust")
    ).resolve()
    for mode in ("cli", "lsp", "push"):
        run(
            [
                str(binary),
                "suite",
                str(Path("companion/tests/fixtures/generic").resolve()),
                str(output / f"generic-{mode}"),
                mode,
                str(official),
                str(fork),
            ],
            env=env,
        )
    for script in (
        "lsp_acceptance",
        "lsp_differential",
        "lsp_lifecycle",
        "lsp_notebook_differential",
    ):
        command = [
            sys.executable,
            f"companion/scripts/{script}.py",
            "--companion",
            str(binary),
            "--official",
            str(official),
            "--output",
            str(output / script),
        ]
        if script in {"lsp_differential", "lsp_notebook_differential"}:
            command += ["--fork", str(fork)]
        run(command, env=env)
    if sys.platform == "linux":
        django_gate(output, binary, env)
    evidence = {
        "platform": sys.platform,
        "python": sys.version,
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "official_sha256": hashlib.sha256(official.read_bytes()).hexdigest(),
        "fork_sha256": hashlib.sha256(fork.read_bytes()).hexdigest(),
        "passed": True,
    }
    (output / "ci.json").write_text(json.dumps(evidence, indent=2) + "\n")
    # Only retain portable reports for the CI artifact, not installed wheels or
    # generated copies of the fixtures.
    shutil.rmtree(output / "official")
    if (output / "fork").exists():
        shutil.rmtree(output / "fork")


if __name__ == "__main__":
    main()
