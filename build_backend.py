"""Build standalone Rust wheels, including an audit before Python ABI tagging.

Maturin sees jiter's inactive optional PyO3 dependency in Cargo metadata and
incorrectly selects an interpreter ABI. Cargo's actual build graph is authoritative.
Never remove an ABI requirement when the executable really embeds Python.
"""

from __future__ import annotations

import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def audit_binary_graph() -> None:
    graph = subprocess.check_output(
        [
            "cargo",
            "tree",
            "--manifest-path",
            str(ROOT / "companion" / "Cargo.toml"),
            "--locked",
            "--target",
            "all",
            "--edges",
            "normal,build",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ],
        text=True,
    )
    forbidden = {
        "pyo3",
        "pyo3-ffi",
        "cpython",
        "uniffi",
        "ty_python_semantic",
        "wasmtime",
    }
    found = {line.split()[0] for line in graph.splitlines()} & forbidden
    if found:
        raise RuntimeError(
            f"Standalone companion has forbidden dependencies: {sorted(found)}"
        )


def standalone_wheel(path: Path) -> str:
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        executables = [
            name
            for name in names
            if name.endswith(
                (".data/scripts/ty-extended", ".data/scripts/ty-extended.exe")
            )
        ]
        if len(executables) != 1:
            raise RuntimeError(f"Expected one companion executable: {executables}")
        if any(name.endswith((".so", ".pyd", ".dylib", ".dll")) for name in names):
            raise RuntimeError(
                "A standalone wheel must not contain extension libraries"
            )
        if any(name.startswith("ty/") for name in names):
            raise RuntimeError(
                "The companion must not overwrite official ty's namespace"
            )
        if "ty_extended/plugin_sdk.py" not in names:
            raise RuntimeError("The companion SDK is missing")
    # wheel updates WHEEL and RECORD and removes the original archive. The
    # platform tag remains unchanged; only the verified-absent Python ABI is removed.
    return subprocess.check_output(
        [
            sys.executable,
            "-m",
            "wheel",
            "tags",
            "--python-tag",
            "py3",
            "--abi-tag",
            "none",
            "--remove",
            str(path),
        ],
        text=True,
    ).strip()


def build_wheel(wheel_directory, config_settings=None, metadata_directory=None):
    import maturin

    audit_binary_graph()
    filename = maturin.build_wheel(wheel_directory, config_settings, metadata_directory)
    return standalone_wheel(Path(wheel_directory) / filename)


def build_sdist(sdist_directory, config_settings=None):
    import maturin

    return maturin.build_sdist(sdist_directory, config_settings)


if __name__ == "__main__":
    audit_binary_graph()
    for directory in sys.argv[1:]:
        wheels = sorted(Path(directory).glob("*.whl"))
        if not wheels:
            raise RuntimeError(f"No wheels in {directory}")
        for wheel in wheels:
            print(standalone_wheel(wheel))
