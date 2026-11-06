"""Generate public companion files from its pinned source dependencies."""

from __future__ import annotations

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def main() -> None:
    sdk = (ROOT / "ruff/crates/ty_plugin_host/python/ty_plugin_sdk.py").read_text()
    sdk = sdk.replace(
        "Authoring surface for `ty` semantic plugins",
        "Authoring surface for companion Monty plugins",
    )
    sdk = sdk.replace(
        "inside the sandbox `ty` prepends", "inside the sandbox the companion prepends"
    )
    (ROOT / "python/ty_extended/plugin_sdk.py").write_text(sdk)
    source = (ROOT / "companion/src/cli.rs").read_text()
    match = re.search(r'pub const HELP: &str = ("(?:[^"\\]|\\.)*");', source)
    if match is None:
        raise RuntimeError("companion CLI help declaration not found")
    help_text = json.loads(match[1])
    (ROOT / "docs/reference/cli.md").write_text(
        "# Companion CLI\n\nGenerated from the companion's public `check --help`. "
        "Use `ty-extended server` for the editor server and `ty-extended --version` "
        "for the companion version.\n\n```text\n" + help_text + "```\n"
    )
    references = {
        "configuration": "https://docs.astral.sh/ty/reference/configuration/",
        "rules": "https://docs.astral.sh/ty/reference/rules/",
        "environment": "https://docs.astral.sh/ty/reference/environment/",
    }
    for name, url in references.items():
        (ROOT / f"docs/reference/{name}.md").write_text(
            f"# Official ty {name} reference\n\n"
            f"The companion delegates checker behavior to official ty. Consult the [current upstream {name} reference]({url}) "
            "for the checker version you installed.\n\n"
            "Companion plugin settings are described in [runtime configuration](../plugin-runtime.md). "
            "Its own command line is described in the [companion CLI](cli.md).\n"
        )


if __name__ == "__main__":
    main()
