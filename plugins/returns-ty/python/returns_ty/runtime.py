"""Configure the packaged returns plugin for ty-extended."""

from __future__ import annotations

import json
from pathlib import Path


def config_overrides(package_dir: Path | None = None) -> list[str]:
    directory = (package_dir or Path(__file__).parent).resolve()
    entry = {
        "id": "returns-ty",
        "runtime": "monty",
        "trusted": True,
        "path": str(directory / "monty.py"),
        "manifest-path": str(directory / "ty-plugin.json"),
    }
    table = ", ".join(f"{key} = {json.dumps(value)}" for key, value in entry.items())
    return [
        "plugins.enabled = true",
        "plugins.auto-discover = false",
        f"plugins.plugin = [{{{table}}}]",
    ]


def checker_options() -> list[str]:
    return [part for override in config_overrides() for part in ("--config", override)]
