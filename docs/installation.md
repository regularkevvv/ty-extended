# Installing the Monty companion

The `ty-extended` distribution installs the `ty-extended` command and depends on
Astral's official `ty` package. They coexist in one environment.

```shell
uv add --dev ty-extended
uv run ty-extended check
```

Use `uvx ty-extended check` for an isolated run, `uv tool install ty-extended` for a
persistent tool environment, or `pip install ty-extended` in an existing environment.
`python -m ty_extended check` provides the same companion entry point.

The companion finds official ty through `--ty PATH`, `TY_COMPANION_TY`, an adjacent
installed `ty` executable, or `PATH`. The adjacent lookup supports isolated tool
environments without requiring a global ty installation.

```shell
ty-extended check --ty /path/to/official/ty
```

For editors, run `ty-extended server`; see [editor setup](editors.md).
Enable trusted installed Monty plugins with `[plugins] auto-discover = true` in
`ty.toml`, or `[tool.ty.plugins] auto-discover = true` in `pyproject.toml`.

## Native archives and source builds

[GitHub releases](https://github.com/regularkevvv/ty-extended/releases) provide native
archives and shell/PowerShell installers for eight targets:

| System       | Architectures | Archive   |
| ------------ | ------------- | --------- |
| macOS        | arm64, x86-64 | `.tar.gz` |
| Linux glibc  | arm64, x86-64 | `.tar.gz` |
| Linux musl   | arm64, x86-64 | `.tar.gz` |
| Windows MSVC | arm64, x86-64 | `.zip`    |

Native archives contain `ty-extended`; install official ty separately and make it
available through one of the paths above. Archive names start with
`ty-extended-`, and installers are named `ty-extended-installer.sh` and
`ty-extended-installer.ps1`. Checksums accompany the release assets.

Building the PyPI source distribution requires Rust 1.98 or later and a native
C/C++ build toolchain. The source archive includes its pinned parser and Monty
host sources, schema, Python SDK and wheel build backend.

## Migrating from the old distribution

Versions through 0.86.x installed a modified `ty` binary and the Python package
`ty`. Remove that distribution before installing the companion and official ty,
so an old installation cannot overwrite Astral's executable or module.

```shell
uv remove --dev ty-extended
uv add --dev ty-extended
uv run ty --version
uv run ty-extended --version
```

With pip, use `python -m pip uninstall ty-extended`, then
`python -m pip install --upgrade --force-reinstall ty-extended ty`.

Change plugin-enabled commands and editor launchers from `ty` to `ty-extended`.
Existing checker settings and Monty registrations remain in `ty.toml` or
`[tool.ty]`; the companion separates plugin settings before passing checker
configuration to official ty. Update Python SDK imports to `ty_extended.plugin_sdk`.

WASM artifacts and worker execution are unsupported by the companion. Install a
Monty version of each plugin. The [historical runtime guide](history/0.86/plugin-runtime.md)
describes the older package, and [compatibility](compatibility.md) describes the
current version policy.
