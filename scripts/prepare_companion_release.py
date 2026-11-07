"""Bump the companion's independent version without moving its source dependencies."""

from __future__ import annotations

import argparse
import re
import subprocess
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version")
    version = parser.parse_args().version
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        parser.error("version must be MAJOR.MINOR.PATCH")
    root = Path(__file__).resolve().parent.parent
    for filename in ("pyproject.toml", "companion/Cargo.toml", "dist-workspace.toml"):
        path = root / filename
        text, count = re.subn(
            r'^version = "[^"]+"$',
            f'version = "{version}"',
            path.read_text(),
            count=1,
            flags=re.MULTILINE,
        )
        if count != 1:
            raise RuntimeError(f"Expected one project version in {filename}")
        path.write_text(text)
    subprocess.run(
        [
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--manifest-path",
            str(root / "companion/Cargo.toml"),
        ],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    print(
        f"Prepared companion {version}; update CHANGELOG.md and regenerate release.yml with cargo-dist 0.31.0."
    )


if __name__ == "__main__":
    main()
