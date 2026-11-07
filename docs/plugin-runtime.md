# Monty companion runtime

`ty-extended` is a frontend around an unchanged official ty process. The companion
owns plugin discovery, Monty execution, project summaries, analysis copies and source
maps. Astral's checker owns Python type checking and its language server.

The companion does not link ty's semantic engine, Salsa or Wasmtime. It uses parser,
notebook, protocol and plugin-host crates pinned by the `ruff` submodule. Keeping
those source dependencies pinned does not pin the separately installed checker.

| Component                                 | Responsibility                                               |
| ----------------------------------------- | ------------------------------------------------------------ |
| `companion/`                              | CLI, persistent editor sessions, projections and source maps |
| Official `ty`                             | Type queries, final checking and language services           |
| Embedded Monty                            | Restricted Python plugin execution                           |
| `ty_extended.plugin_sdk`                  | Author-facing Python SDK and type declarations               |
| `ty_plugin_protocol` and `ty_plugin_host` | Serialized requests, routing and runtime validation          |

## Configuration and trust

Read project configuration from `ty.toml` or `[tool.ty]` in `pyproject.toml`.
Installed plugins are discovered only when enabled:

```toml
[plugins]
auto-discover = true
```

Explicit registrations select a Monty artifact and manifest:

```toml
[plugins]
enabled = true
auto-discover = false

[[plugins.plugin]]
id = "example"
runtime = "monty"
trusted = true
path = "plugin.py"
manifest-path = "ty-plugin.json"
```

Monty plugins receive serialized project information and return declarative patches.
They have no filesystem, environment or network access. Source is summarized
statically; the companion does not import or execute a project's Python code.
Protocol validation and runtime limits still apply. In-process memory isolation
is not a process sandbox; see the resource-limit implementation and validation
boundaries in [companion status](https://github.com/regularkevvv/ty-extended/blob/main/companion/STATUS.md).

## CLI and editor behavior

`ty-extended check` runs plugins, asks official ty to check temporary analysis copies,
and maps diagnostics back to original paths and spans. Temporary CLI files are
removed after checking. `--fix` and `--add-ignore` apply mapped edits to original
files; an edit that cannot be mapped safely is rejected.

`ty-extended server` retains checker processes, plugin runtimes and parsed projects
between edits. It handles unsaved text, notebooks, configuration changes and
multiple workspaces. Positions are translated for UTF-8, UTF-16 and UTF-32 clients.
Generated declarations are hidden from navigation and edit results.

WASM and external Monty workers are outside this distribution. The
[historical 0.86.x guide](history/0.86/plugin-runtime.md) documents those runtimes.
See [compatibility](compatibility.md) for supported checker versions and protocol
boundaries, and [plugin authoring](plugin-authoring.md) for Monty examples.
