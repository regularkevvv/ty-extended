# `ty_plugin_protocol`

[![crates.io](https://img.shields.io/crates/v/ty_plugin_protocol.svg)](https://crates.io/crates/ty_plugin_protocol)
[![docs.rs](https://docs.rs/ty_plugin_protocol/badge.svg)](https://docs.rs/ty_plugin_protocol)

The stable serialized contract between
[ty-extended](https://github.com/regularkevvv/ty-extended) and semantic plugins.

This crate intentionally contains data types only. It does not depend on checker internals, Salsa,
AST ids, the plugin host, or a WASM engine. Manifests, requests, responses, claims, patches,
diagnostics, and type expressions can therefore be serialized independently of ty's implementation.

Rust protocol tooling and native regression implementations can depend on
[`ty_plugin_sdk`](https://crates.io/crates/ty_plugin_sdk), which re-exports this crate as
`ty_plugin_sdk::protocol` and adds the `Plugin` trait, `ManifestBuilder`, typed patch helpers,
dispatch, and legacy WASM exports. Current companion plugins use the bundled
Python `ty_extended.plugin_sdk` and execute in Monty. The companion delegates
checking to unchanged official ty; it is not a checker fork.

Depend on `ty_plugin_protocol` directly when implementing a host, validating manifests, inspecting
wire messages, or building protocol tooling.

## Add the Dependency

```toml
[dependencies]
ty_plugin_protocol = "0.0.11"
```

## Protocol Model

The main types are:

- `PluginManifest`: identity, compatibility, runtime, capabilities, claims, configuration, and stub overlays;
- `PluginRequest`: the tagged request enum sent by a host;
- `PluginResponse`: the tagged response enum returned by a plugin;
- request summaries such as `AnalyzeClassRequest`, `CallRequest`, and `ResolveMemberRequest`;
- declarative outputs such as `ClassPatch`, `MemberPatch`, `CallSignaturePatch`, and `ProjectIndexResponse`;
- `TypeExpr`: source-level type data with expression, annotation, or stub mode;
- `ProtocolVersion`: compatibility negotiation between a host and plugin.

The wire format is JSON with kebab-case field and variant names. Invocations receive protocol
data and optional read-only access to their own project index. Responses are data; neither side
shares memory objects from the checker.

## Compatibility Negotiation

The protocol is pre-1.0. A host accepts the same protocol major and any plugin minor version no
newer than its own:

```rust
use ty_plugin_protocol::{ProtocolCompatibility, ProtocolVersion};

let host = ProtocolVersion { major: 0, minor: 3 };
let plugin = ProtocolVersion { major: 0, minor: 2 };

assert_eq!(
    host.negotiate(plugin),
    ProtocolCompatibility::Compatible,
);
```

Unknown JSON fields remain parseable for forward transport, but successful deserialization is not
permission to use unsupported behavior. Always negotiate the version before dispatching requests.

## Example Manifest Fragment

```json
{
  "id": "my-plugin",
  "name": "My plugin",
  "version": "0.1.0",
  "protocol-version": { "major": 0, "minor": 9 },
  "ty-compatibility": { "requirement": ">=0.89.0,<0.90.0" },
  "runtime": {
    "kind": "monty",
    "artifact": "my_plugin.py"
  },
  "capabilities": {
    "call-return": true
  },
  "claims": {
    "functions": [
      { "qualified-name": "my_library.Field" }
    ]
  }
}
```

Native tooling can use `ty_plugin_sdk::ManifestBuilder` to keep claims and
capability flags aligned. Python companion plugins use the bundled
`ty_extended.plugin_sdk` manifest helpers.

See the [plugin authoring
guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/plugin-authoring.md) to build a
complete Monty plugin and the [`ty_plugin_sdk` API documentation](https://docs.rs/ty_plugin_sdk)
for the author-facing interface.

## Call state (protocol 0.6)

`AdjustCallState(CallRequest)` returns `CallStatePatch`. `receiver-members` describes member
values after successful synchronous completion. `result-members` is applied only with
`fresh-result: true`, a guarantee that the result is distinct from all existing objects.
`preserves-other-objects` defaults to false; the checker discards existing facts before applying
the patch. See the [object member state guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/plugin-authoring.md#object-member-state).

Protocol 0.7 adds constructor claims: `claim_call_state_constructor("pkg.Record")` selects
one class, and `claim_call_state_constructors_on_subclass("pkg.Record")` selects its
subclasses, including the base itself. Constructor hooks receive the constructed class name
and arguments; they must account for custom initialization before guaranteeing fresh results.

## Class-scoped project indexes (protocol 0.8)

`ProjectIndexResponse.class_indexes` optionally maps qualified class names to opaque JSON.
For a matching `AnalyzeClassRequest`, its value replaces the complete `project_index`; it
is not merged with that index. An explicit JSON null selects no index. Unlisted classes
and all other hooks receive the complete index. An absent or empty map preserves the
previous wire format and behavior.

The plugin must include everything its class hook needs in each replacement. A host must
keep these values private to their plugin and invalidate cached semantic responses when
either the complete index or the scoped values change. Plugins requiring this behavior
must advertise protocol 0.8 so older hosts reject them instead of silently discarding it.

## Project index lookups (protocol 0.8)

The optional `project-index-lookup` capability lets a runtime retain the selected
opaque index outside the interpreter. Python plugins then access dictionary paths
through `project_index_value(request, path, default=None)` and enumerate keys with
`project_index_keys(request, path)`. These helpers also support inline indexes.
Opted-in plugins must use the helpers instead of direct request dictionary access.

Values are fresh JSON copies; mutations cannot affect another lookup. Missing
keys use the supplied default, while explicit null values remain null. Paths are
literal dictionary keys, never filesystem paths or cross-plugin references.
The selected index and its revision remain part of semantic cache identity.
The Monty callback supplies only the current invocation's plugin-owned index,
preserves fresh module state, and applies the invocation's resource limits.

## Class batches (protocol 0.9)

The optional `class-batch` capability adds `AnalyzeClasses` and `ClassBatch`.
Each request item is an ordinary tagged `analyze-class` request with its own
semantic context and complete class-scoped index. Results correspond to inputs
in order and may contain only a class patch, no-change, or protocol error.
Batches cannot contain other hook kinds or nested batches.

Opting in means that class results do not depend on shared mutable module state,
item order, or batch boundaries. Module state is fresh for each batch invocation;
items within a batch share that invocation. Keep the ordinary class hook: hosts
can use it for unlisted classes, oversized requests, or a batch that abstains or
returns an invalid response. A malformed batch must not commit partial results.

The companion batches only claimed classes with explicit `class-indexes`, limits
each batch to 128 items and 1 MiB of request JSON, and retains the existing Monty
execution and response limits. Cached item responses retain the individual
request's plugin identity, index revision, source context, and class summary.
