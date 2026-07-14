"""Transform the README.md to support a specific deployment target.

By default, we assume that our README.md will be rendered on GitHub. However,
PyPI includes the README with different rendering.
"""

from __future__ import annotations

import re
import tomllib
import urllib.parse
from pathlib import Path

# The benchmark SVG includes a CSS media query that adapts to light/dark mode.
# PyPI doesn't support this, so we replace it with a light-only version.
# See: https://github.com/pypi/warehouse/issues/11251
BENCHMARK_URL = "https://raw.githubusercontent.com/astral-sh/ty/main/docs/assets/ty-benchmark-cli.svg"
BENCHMARK_URL_LIGHT = "https://raw.githubusercontent.com/astral-sh/ty/main/docs/assets/ty-benchmark-cli-light.svg"
REPOSITORY = "regularkevvv/ty-extended"


def transform(content: str, version: str) -> str:
    """Return README content suitable for the released PyPI package."""
    release_ref = f"v{version}"

    # Keep supporting the upstream benchmark if it is restored later, but do
    # not require fork-owned READMEs to contain it.
    content = content.replace(BENCHMARK_URL, BENCHMARK_URL_LIGHT)

    # Replace relative src="./..." attributes with absolute GitHub raw URLs.
    def replace_src(match: re.Match) -> str:
        path = match.group(1).removeprefix("./")
        return (
            f'src="https://raw.githubusercontent.com/{REPOSITORY}/{release_ref}/{path}"'
        )

    content = re.sub(r'src="(\./[^"]+)"', replace_src, content)

    # Replace relative Markdown links with links to the fork's release tag.
    def replace_url(match: re.Match) -> str:
        url = match.group(1)
        parsed = urllib.parse.urlsplit(url)
        if not parsed.scheme and not url.startswith("//"):
            url = urllib.parse.urljoin(
                f"https://github.com/{REPOSITORY}/blob/{release_ref}/README.md",
                url,
            )
        return f"]({url})"

    content = re.sub(r"]\(([^)]+)\)", replace_url, content)

    # PyPI does not support GitHub's admonition marker syntax.
    def replace_admonition(match: re.Match) -> str:
        name = match.group(1)
        return f"> {name}:"

    return re.sub(r"> \[!(\w*)\]", replace_admonition, content)


def main() -> None:
    """Modify the README.md to support PyPI."""
    # Read the current version from the `dist-workspace.toml`.
    with Path("dist-workspace.toml").open(mode="rb") as fp:
        # Parse the TOML.
        dist_workspace = tomllib.load(fp)
        if "workspace" in dist_workspace and "version" in dist_workspace["workspace"]:
            version = dist_workspace["workspace"]["version"]
        else:
            raise ValueError("Version not found in dist-workspace.toml")

    content = transform(Path("README.md").read_text(encoding="utf8"), version)

    with Path("README.md").open("w", encoding="utf8") as fp:
        fp.write(content)


if __name__ == "__main__":
    main()
