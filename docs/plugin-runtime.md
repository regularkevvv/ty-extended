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
enabled = true
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

## Companion execution profiles

The unreleased native Django profile selects these host options through the
manifest's `default-config`. They describe the plugin's requirements; they do
not change the external checker's version or configuration interfaces.

- `companion-native-fields = true` delegates ordinary fields to official ty.
    The default activates only when every registered plugin opts in. Explicit
    environment overrides retain either strategy.
- `companion-parallel-calls = true` permits bounded prefetch of independent
    literal-argument hooks on fresh Monty snapshots. Responses enter the ordinary
    cache under their complete request keys. Changed arguments, contexts or indexes
    use the normal execution path, and speculative host failures are retried there.
- `companion-native-cli = true` prefers an overlapping official CLI check when
    every plugin selects it and ordinary fields use native typing. Temporary
    partial overlays retain changed provider declarations. Native diagnostics
    are reused only when the final projections, selected paths, source revisions,
    import validation and checker configuration still match. Inferred-type queries
    and mismatched inputs use the existing checker transport.
- `companion-stateless-call-batches = true` permits eligible ordinary call hooks
    for the same plugin to share a bounded Monty execution. Authors attest that
    these handlers depend only on each request and its fixed project index,
    without observable mutable module state or ambient state. Exact request
    keys and first-owner routing remain unchanged. Each response is validated;
    a failed group takes the ordinary individual retry path. The shared run
    retains the ordinary time, response and suspension limits. Non-opted
    requests keep fresh module globals. Timing reports separate batch execution
    time from unbatched per-hook time.
- `companion-early-project-contributions = true` permits an extra ordinary
    project-index call with settings and context, before full model indexing.
    Every plugin must opt in. Authors use this when provider declarations can be
    produced from those inputs alone. The complete project index still runs and
    replaces the setup index. The startup prototype reads only settings and
    contributed provider declarations before launching native Ty, then loads the
    complete project concurrently. It captures source/provider file and directory
    revisions before launch, requires later host reads to match those revisions,
    and validates them again before reuse. Eligible sources are read and parsed
    in one wave, with the open file verified against its pre-launch revision
    after reading. Each native attempt uses a fresh provider overlay directory;
    discarded attempts cannot retain patches in a retry. Unknown revisions and
    unsupported inputs retain normal checking. Reuse also requires identical final source
    and provider projections. A successful early probe containing only absolute imports can
    cover a subset of those imports and their package prefixes. Additional
    imports, executable probe code or probe diagnostics retain normal checking.
- `companion-source-class-lookups = 512` permits project-index construction to
    retain references to immutable class summaries when the request has at least
    that many declarations. `true` enables all sizes; `false` disables the option.
    The plugin must already have the project-index lookup capability and must
    explicitly return `{"use": true}` in the private
    `__ty_companion_source_classes__` index entry. Reads remain bounded, read-only
    and isolated to that plugin's filtered input. Duplicate class identities or
    a class exceeding the 64 KiB lookup bound retain the ordinary representation.
    Hosts without the option or lookup capability also retain ordinary inputs.
    The source snapshot participates in index generation and cache invalidation.
- `companion-third-party-overlays = ["django"]` declares top-level overlay
    modules that the plugin author guarantees are third-party packages throughout
    its supported checker and Python versions. The companion can omit the separate
    stdlib precedence probe for these names only when every registered plugin
    agrees. Names must be ASCII Python module identifiers. Undeclared modules and
    every custom typeshed retain resolution through official ty. This declaration
    does not remove overlay files from the checker or change source selection.

These options retain per-request Monty resource limits and plugin index isolation.
CLI memory projection and provider borrowing apply only to eligible fresh checks;
notebooks and persistent editor updates retain their existing projection paths.

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
