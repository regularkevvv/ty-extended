#!/usr/bin/env sh
#
# Generate files and copy documentation from Ruff.
#
# Usage
#
#   ./scripts/autogenerate-files.sh
#
set -eu

script_root="$(realpath "$(dirname "$0")")"
project_root="$(dirname "$script_root")"
cd "$project_root"

echo "Updating lockfile..."
uv lock --no-locked --default-index https://pypi.org/simple

echo "Copying reference documentation from Ruff..."
cp ./ruff/crates/ty/docs/cli.md ./docs/reference/
cp ./ruff/crates/ty/docs/configuration.md ./docs/reference/
cp ./ruff/crates/ty/docs/rules.md ./docs/reference/
cp ./ruff/crates/ty/docs/environment.md ./docs/reference/

echo "Documentation has been copied from Ruff submodule"

echo "Copying the plugin SDK into the ty wheel..."
cp ./ruff/crates/ty_plugin_host/python/ty_plugin_sdk.py ./python/ty/plugin_sdk.py

echo "Plugin SDK has been copied from Ruff submodule"
