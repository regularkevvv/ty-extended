# Author a Monty plugin

ty-extended executes Python plugins in embedded Monty. The serialized protocol
and ambient SDK cover class/member patches, call signatures and return types,
object state, project indexes, dependencies, mutation diagnostics and stub overlays.
See [runtime behavior](plugin-runtime.md) and the [compatibility policy](compatibility.md).

The protocol version is validated by the host. Declare required ty-extended versions
in your Python package dependencies. The legacy `ty-compatibility` manifest field
is retained for format compatibility; it does not select the official checker.

## Author in Python

Python plugins run inside the embedded [Monty](https://github.com/pydantic/monty) interpreter, a
restricted Python subset. The `ty_extended.plugin_sdk` module — shipped inside the `ty-extended` wheel — is
prepended to the plugin source by the runner, so its decorators, builders, and manifest helpers
are ambient names in the plugin file. This is the public plugin SDK. The Rust SDK
and protocol crates are internal workspace dependencies and are not published.
A complete Monty plugin is a single `.py` file:

```python
from __future__ import annotations

import typing

if typing.TYPE_CHECKING:
    # IDE-only import — inside Monty the names are ambient instead.
    from ty_extended.plugin_sdk import (
        Request,
        Response,
        call_arguments,
        call_return_patch,
        capabilities,
        claims,
        literal_value,
        manifest,
        on_call_return_of,
        set_manifest,
        symbol_claim,
        type_expr,
    )

set_manifest(
    manifest(
        id="example.tokens",
        name="example-tokens",
        version="0.1.0",
        capabilities=capabilities(call_return=True),
        claims=claims(functions=[symbol_claim("example.issue_token")]),
    )
)


@on_call_return_of("example.issue_token")
def adjust_issue_token(request: Request) -> Response:
    if literal_value(call_arguments(request)[0]) == "token":
        return call_return_patch(type_expr("example.Token", mode="annotation"))
    return None  # declines; another handler or `no-change` answers
```

The Python SDK provides:

- hook decorators: `on_class_transform`, `on_class_batch`, `on_class_member`, `on_instance_member`,
    `on_call_signature`, `on_call_return`, `on_call_state`, `on_project_index`, `on_dependencies`, `on_mutation`,
    `on_manifest`. Each kind accepts several handlers — they run in registration order and the
    first non-`None` result wins;
- filtered decorators for the common case of handling specific symbols:
    `on_class_transform_of`, `on_class_member_of`, `on_instance_member_of`,
    `on_call_signature_of`, `on_call_return_of`, `on_call_state_of`, `on_mutation_of`;
- request accessors: `context`, `plugin_config`, `strict_settings`, `speculative`, `callee`,
    `receiver`, `owner`, `member_name`, `class_summary`, `project_index_of`, `call_arguments`,
    `positional_arguments`, `keyword_arguments`, `argument_type`, and `literal_value` — which
    unwraps an argument's `LiteralValue` into plain Python data (`UNKNOWN` marks values the
    host could not summarize);
- manifest helpers: `manifest`, `capabilities`, `claims`, `set_manifest`, `stub_overlay`, and
    one claim builder per claim kind — `module_claim`, `class_claim_exact`,
    `class_claim_subclass_of`, `symbol_claim`, `method_claim_exact`,
    `method_claim_on_subclass_of`, `method_claim_on_subclass_of_matching`,
    `attribute_claim_exact`, `attribute_claim_on_subclass_of`,
    `attribute_claim_contribution_target`, `settings_claim`;
- shared builders: `type_expr`, `type_annotation`, `type_stub`, `import_binding`, `parameter`
    (`positional_or_keyword`, `keyword_only`, `optional`), `callable_signature`, `diagnostic`,
    `dependency`, `text_position`, `location`, `symbol_source`;
- member and class builders: `member`, `callable_member`, `descriptor_member`, `member_value`,
    `member_descriptor`, `member_callable`, `member_patch`, `member_response`, `field`,
    `init_field`, `field_patch`, `class_patch`, `class_batch`;
- project-index builders: `contribution` with `class_target`/`instance_target`/
    `constructor_target` and `member_contribution`/`field_contribution`/
    `constructor_contribution`/`diagnostic_contribution`, plus `virtual_type` with
    `virtual_class`/`virtual_typed_dict`/`virtual_named_tuple`/`virtual_field`;
- `TypeSnapshot` builders and helpers for transforming host-supplied types:
    `snapshot_expression`, `snapshot_nominal`, `snapshot_tuple`, `snapshot_typed_dict`,
    `snapshot_union`, `snapshot_plugin_class`, `snapshot_self`, `snapshot_annotated`,
    `snapshot_callable`, `snapshot_parameter`, `snapshot_type_param`,
    `snapshot_field`, `snapshot_metadata`, `snapshot_name`, `snapshot_to_expression`,
    `type_expr_from_snapshot`, `type_expr_expression`, `type_expr_snapshot`;
- response builders: `call_signature_patch`, `call_return_patch`, `call_state_patch`, `project_index`,
    `dependencies`, `mutation_diagnostics`, `manifest_response`, `no_change`, and `error`.

The manifest runtime entry names the Python artifact:

```json
{
  "runtime": {
    "kind": "monty",
    "artifact": "tokens.py"
  }
}
```

and the project registers it with `runtime = "monty"`:

```toml
[[tool.ty.plugins.plugin]]
id = "example.tokens"
path = ".ty/plugins/tokens.py"
runtime = "monty"
manifest-path = ".ty/plugins/tokens.plugin.json"
trusted = true
```

Plugin artifacts live outside the checked source tree, so exclude them:
`[tool.ty.src] exclude = [".ty"]`. A runnable project with this layout is at
`examples/monty/`. Historical WASM examples are outside ty-extended runtime.

### Imports and editor support

The `typing.TYPE_CHECKING` guard is the recommended pattern: type checkers resolve
`from ty_extended.plugin_sdk import ...` against the wheel module, so signatures and
`Literal` value hints work while authoring, while inside Monty the block is
skipped and the ambient prelude names apply. Do not import `ty_extended.plugin_sdk`
unguarded — unresolvable module names bind to stub namespaces in the sandbox and
shadow the ambient names.

### Python sandbox constraints

Monty executes a Python subset. Plugin code cannot import third-party modules, read the
filesystem, or access the environment or clock — `time`/`random` return fixed values so plugin
answers stay deterministic — and project data comes from protocol requests and
read-only lookups into the plugin's own index. No class inheritance, `match`, or `yield`.
`print` output is captured and logged for debugging.

### Keep class indexes small

Protocol 0.8 adds `project_index(..., class_indexes={...})`. Each key is a
class's qualified name. Its value completely replaces `project-index` in that
class's `analyze-class` request; it is not merged with the full index. Include
all settings and related symbols that the class hook needs. Unlisted classes
and other semantic hooks continue to receive the full `plugin_index` value.

```python
return project_index(
    {"models": all_models, "settings": settings},
    class_indexes={
        name: {"model": model, "settings": relevant_settings(model)}
        for name, model in all_models.items()
    },
)
```

The host treats each value as opaque JSON. An explicit `None` selects no index;
omitting a key selects the full index. Changing or removing a scoped index
invalidates the associated plugin's cached semantic responses. Hook invocations
remain isolated, so mutating an index inside one hook cannot change later hooks.
The normal response-size limit includes these additional indexes.

This avoids serializing a complete project into every class hook. Plugins that
use the new field must declare protocol minor version 8 or newer; older plugins
continue to use the existing full-index behavior.

### Look up index entries on demand

For large indexes, protocol 0.8 also supports the manifest capability
`"project-index-lookup": true`. Opted-in plugins read their index through
`project_index_value(request, path, default=None)` and
`project_index_keys(request, path)`:

```python
model = project_index_value(request, ["models", qualified_model_name])
model_names = project_index_keys(request, ["models"])
```

Paths contain literal dictionary keys, including dots or slashes. A missing key
uses the supplied default; a present JSON null remains `None`. Key enumeration
returns an empty list for missing values and scalars. Each value lookup returns
a fresh JSON copy. Mutating that copy does not change the retained index or a
later lookup, including within the same invocation.

The embedded Monty host can keep the full index outside the interpreter and
transfer only requested entries. The helpers also work with ordinary inline
indexes, including class-specific replacements and runtimes without callbacks.
When opting in, use these helpers instead of reading `request["project-index"]`
directly. Fetching the entire index with an empty path is supported but restores
the cost of transferring it.

The callback reads only the index selected for the current plugin and request.
It grants no filesystem, network, or other-plugin access. An invocation can make
at most 1,000 lookups. Lookup paths are bounded to 128 keys and 64 KiB of encoded
JSON; replies use the configured response-size
limit. The interpreter and callback loop remain subject to the invocation's time
limit. Other external operations are rejected. Every invocation still starts
with fresh plugin module state.

### Batch independent class hooks

Protocol 0.9 adds the optional `"class-batch": true` capability and
`@on_class_batch` hook. Keep the individual class hook for fallback:

```python
@on_class_batch
def analyze_classes(request: Request) -> Response:
    return class_batch([analyze_class(item) for item in request["requests"]])
```

Each item has the same tagged request, semantic context and class-specific index
as an individual `analyze-class` invocation. Return one class patch, abstention
or error per item, in request order. Results must be independent of item order
and batch boundaries. Module state is shared within a batch and starts fresh
for the next invocation; do not let one item's mutations affect another result.

ty-extended batches only claimed classes with explicit class-specific indexes.
It limits a batch to 128 classes and 1 MiB of serialized requests; the ordinary
execution and response-size limits still apply. Oversized or unlisted classes use
individual hooks. An unavailable, invalid or wrong-length batch response also
falls back to individual calls. Validated results enter the ordinary per-request
cache, so editor invalidation follows the same index and source revisions.

Plugins that opt in require protocol minor version 9 or newer and must also
enable class transforms. The Python SDK provides `on_class_batch` and
`class_batch`.

### Callable type snapshots

Protocol 0.10 gives function-typed arguments and return types a structured
`"kind": "callable"` snapshot instead of an opaque `expression` string:

```json
{
  "kind": "callable",
  "display": "def app.add(a: int, b: int) -> int",
  "type-params": [{"name": "T", "kind": "type-var", "bound": {"kind": "..."}}],
  "parameters": [
    {"name": "a", "kind": "positional-or-keyword",
     "type-snapshot": {"kind": "nominal", "qualified-name": "builtins.int"},
     "required": true}
  ],
  "return-type": {"kind": "nominal", "qualified-name": "builtins.int"}
}
```

`parameters` preserves names, kinds (`positional-only`, `positional-or-keyword`,
`keyword-only`, `var-args`, `kwargs`, `param-spec`) and defaults
(`required: false`); `type-params` preserves PEP 695 `def f[T](...)`, `*Ts` and
`**P` declarations with bounds, constraints and defaults. `ellipsis: true` is
`Callable[..., R]` / `(...) -> R`. `display` keeps the checker's original
spelling for messages.

Build them with `snapshot_callable`, `snapshot_parameter` and
`snapshot_type_param`, and render them back with `snapshot_to_expression`.
Returning a `callable` snapshot in a patch lowers to `Callable[...]` for plain
positional signatures and to a generated `Protocol` `__call__` for named,
keyword-only or generic signatures.

Plugins declaring protocol minor version 9 or older still receive the original
`expression` spelling on the wire, so existing plugins are unaffected.

### Decorator applications

Bare decorators such as `@curry` do not create an ordinary call site. When a
plugin adds `symbol_claim`s under `claims.decorators`, the host synthesizes a
`decorator(function)` application for each claimed name or attribute decorator:
the decorated function's signature arrives as the sole positional argument — a
`callable` snapshot under 0.10, an `expression` under older protocols — and the
request carries `"decorator": true`. Decorators apply bottom-up, matching
Python's evaluation order, and each `call-return-patch` retypes the definition
through the decorator's own application, so `reveal_type` and later calls see
the patched callable. Diagnostics from the hook attach to the decorator
expression. Manifests without `claims.decorators` incur no extra requests;
decorators with call arguments (`@factory(...)`) already produce ordinary call
sites and keep their `functions`/`methods` routing.

### Generator-expression arguments

A call argument written as a generator expression also carries a `generator`
payload describing the comprehension so plugins can inspect clauses that are
invisible to the inferred type:

```json
{
  "generator": {
    "is-async": false,
    "clauses": [
      {"iter-type": {"kind": "..."}, "iter-source": {"file-path": "...", "start": ..., "end": ...},
       "conditions": 1, "is-async": false}
    ],
    "yield-type": {"kind": "..."},
    "yield-source": {"file-path": "...", "start": ..., "end": ...}
  }
}
```

Each clause records the iterated expression's type snapshot and source span,
the number of `if` filters, and `async for`; `yield-type`/`yield-source`
describe the element expression. This mirrors what AST-walking plugins in other
hosts use for `do`-notation-style APIs (`Container.do(x for x in seq)`).

### Diagnostic locations

`diagnostic(..., location=...)` accepts a source span of the same shape as
`iter-source`/`yield-source` (`{"file-path", "start", "end"}`). When present,
the emitted diagnostic attaches to that span instead of the hooked call,
decorator or mutation expression — needed for comprehension-clause errors and
other diagnostics that legitimately point inside an argument. Explicit
locations are protocol 0.10; a manifest declaring an older minor keeps every
diagnostic at the hooked expression.

## Test and publish

Run the plugin through the installed ty-extended in CLI and editor sessions. Check
positive and negative cases, plugin discovery from a built wheel, original source
locations, and the supported checker versions. Package the Monty source, manifest
and any stub overlays together; no WASM build is needed.

Declare ty-extended requirements in package dependencies and the protocol version
in the manifest. New protocol features require a compatible host. The protocol
version is separate from the package version; the Python SDK ships and is
versioned with ty-extended.
