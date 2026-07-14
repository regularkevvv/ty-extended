# Extension Runtime

ty-extended separates the public extension API from checker internals.

The source for the checker-side implementation lives in the `ruff` submodule, backed by
[`regularkevvv/ruff-extended`](https://github.com/regularkevvv/ruff-extended). The wrapper
repository publishes the `ty-extended` Python distribution and the public SDK crates.

## Components

| Component            | Audience                   | Responsibility                                                                 |
| -------------------- | -------------------------- | ------------------------------------------------------------------------------ |
| `ty_plugin_protocol` | Extension authors and host | Stable serialized protocol types.                                              |
| `ty_plugin_sdk`      | Extension authors          | Ergonomic manifest, hooks, DSL, JSON dispatch, WASM export.                    |
| `ty_plugin_host`     | Host only                  | Manifest validation, routing, protocol negotiation, runtime runner.            |
| `ty_python_semantic` | Host only                  | Applies extension responses during type inference.                             |
| `ty_project`         | Host only                  | Reads project config, fingerprints extension environment, wires runtime state. |

Extension crates should use only `ty_plugin_sdk`.

## Loading Flow

1. `ty_project` reads `[plugins]` entries from `ty.toml` (or `[tool.ty.plugins]` from
    `pyproject.toml`) and optionally discovers installed `ty-plugin.json` manifests.
1. Trusted WASM entries load their manifest JSON and artifact bytes.
1. `ty_plugin_host` validates manifest compatibility and builds routing tables from claims.
1. The WASM runner registers each artifact and executes requests through JSON.
1. `ty_python_semantic` asks the host for hook responses at claimed semantic points.
1. Runtime failures become diagnostics and inference falls back to normal ty behavior where
    possible.

## Safety Model

Extensions are disabled by default and must be explicitly trusted per project. WASM extensions run
without filesystem, environment, clock, or network access. The host applies fuel, memory, and
response-size limits per call.

The runtime treats extension output as data. A bad type expression, oversized response, trap, or
unsupported runtime reports an actionable diagnostic instead of crashing the checker.

## Release Ownership

`ty-extended` publishes:

- PyPI project `ty-extended`, which installs the `ty` executable.
- crates.io project `ty_plugin_sdk`, the author-facing extension API.
- crates.io project `ty_plugin_protocol`, the serialized contract used by the SDK and host.

`ruff-extended` is the source repository for the implementation crates. The top-level
`ty-extended` release workflow records the exact `ruff` submodule commit and publishes SDK crates
from that source.
