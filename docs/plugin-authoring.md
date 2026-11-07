# Author a Monty plugin

The companion executes Python plugins in embedded Monty. The serialized protocol
and ambient SDK cover class/member patches, call signatures and return types,
object state, project indexes, dependencies, mutation diagnostics and stub overlays.
See [runtime behavior](plugin-runtime.md) and the [compatibility policy](compatibility.md).

The protocol version is validated by the host. Declare required companion versions
in your Python package dependencies. The legacy `ty-compatibility` manifest field
is retained for format compatibility; it does not select the official checker.

## Author in Python

Python plugins run inside the embedded [Monty](https://github.com/pydantic/monty) interpreter, a
restricted Python subset. The `ty_extended.plugin_sdk` module — shipped inside the `ty-extended` wheel — is
prepended to the plugin source by the runner, so its decorators, builders, and manifest helpers
are ambient names in the plugin file. A complete Monty plugin is a single `.py` file:

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

The ambient API mirrors the Rust SDK surface:

- hook decorators: `on_class_transform`, `on_class_member`, `on_instance_member`,
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
    `init_field`, `field_patch`, `class_patch`;
- project-index builders: `contribution` with `class_target`/`instance_target`/
    `constructor_target` and `member_contribution`/`field_contribution`/
    `constructor_contribution`/`diagnostic_contribution`, plus `virtual_type` with
    `virtual_class`/`virtual_typed_dict`/`virtual_named_tuple`/`virtual_field`;
- `TypeSnapshot` builders and helpers for transforming host-supplied types:
    `snapshot_expression`, `snapshot_nominal`, `snapshot_tuple`, `snapshot_typed_dict`,
    `snapshot_union`, `snapshot_plugin_class`, `snapshot_self`, `snapshot_annotated`,
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
`examples/monty/`. Historical WASM examples are outside the companion runtime.

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
answers stay deterministic — and project data arrives only through protocol requests
(`build-project-index`, call requests, and so on). No class inheritance, `match`, or `yield`.
`print` output is captured and logged for debugging.

## Test and publish

Run the plugin through the installed companion in CLI and editor sessions. Check
positive and negative cases, plugin discovery from a built wheel, original source
locations, and the supported checker versions. Package the Monty source, manifest
and any stub overlays together; no WASM build is needed.

Declare companion requirements in package dependencies and the protocol version
in the manifest. New protocol features require a compatible host. The protocol
and SDK remain pre-1.0 and independently versioned.
