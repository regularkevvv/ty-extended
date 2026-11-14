# `ty_plugin_sdk`

[![crates.io](https://img.shields.io/crates/v/ty_plugin_sdk.svg)](https://crates.io/crates/ty_plugin_sdk)
[![docs.rs](https://docs.rs/ty_plugin_sdk/badge.svg)](https://docs.rs/ty_plugin_sdk)

Typed protocol helpers and native regression tests for
[ty-extended](https://github.com/regularkevvv/ty-extended) semantic plugins.

Current ty-extended releases are Monty companions for unchanged official ty.
Their plugins are Python source using `ty_extended.plugin_sdk`; the Rust SDK
provides typed protocol tooling and independent native test implementations.
The companion does not load Rust libraries or WASM modules.

Use this crate to declare what a plugin owns, implement typed semantic hooks and return
declarative patches. It re-exports all wire
types from `ty_plugin_protocol` as `ty_plugin_sdk::protocol`, so a plugin normally has only one
ty dependency.

## Architecture Boundary

A plugin does not link to `ty_python_semantic`, Salsa, AST ids, or checker-owned type objects.
The host sends a serialized request for a manifest claim; the plugin returns a serialized patch
that the host validates and applies.

```text
companion -> typed protocol request -> Monty hook -> declarative patch -> companion
```

A native `Plugin` implementation can act as a regression oracle for the corresponding Monty hooks.

## Quick Start

Create a native regression library:

```toml
[package]
name = "my-ty-plugin"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["rlib"]

[dependencies]
ty_plugin_sdk = "0.0.11"
```

Implement `Plugin` and claim the matching hook in the corresponding Monty manifest:

```rust
use ty_plugin_sdk::protocol::{
    CallRequest, PluginManifest, PluginResponse, MontyRuntimeSpec, RuntimeSpec, TypeExpr,
};
use ty_plugin_sdk::{dsl, ManifestBuilder, Plugin};

#[derive(Default)]
struct MyPlugin;

impl Plugin for MyPlugin {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("my-plugin", "My plugin", env!("CARGO_PKG_VERSION"))
            .ty_compatibility(">=0.89.0,<0.90.0")
            .runtime(RuntimeSpec::Monty(MontyRuntimeSpec {
                artifact: "my_plugin.py".to_string(),
                sha256: None,
            }))
            .claim_call_return("my_library.Field")
            .build()
    }

    fn adjust_call_return(&self, _request: &CallRequest) -> PluginResponse {
        dsl::call_return(TypeExpr::annotation("str"))
    }
}
```

Every hook has a `PluginResponse::NoChange` default. An implementation only overrides the hooks it
uses.

## WASM exports

`export_plugin!` and the WASM wire types remain available for hosts that implement
that runtime and negotiate the declared protocol version. They are outside the
current ty-extended companion. Older WASM examples and releases are documented in
[the historical guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/history/0.86/plugin-authoring.md).

## Manifests and Claims

`ManifestBuilder` starts with protocol defaults and keeps capability flags synchronized with
claims. For example, `claim_call_return` adds a function claim and enables the `call-return`
capability.

Claims keep routing precise: ty invokes a plugin only for the classes, functions, methods,
attributes, settings, or mutations it declared. Useful builder methods include:

- `claim_class_transform` and `claim_subclass_transform`;
- `claim_class_member`, `claim_instance_member`, and subclass member claims;
- `claim_call_signature`, `claim_call_return`, and `claim_call_state`, including method variants;
- `project_index`, settings claims, and cross-symbol contribution targets;
- `claim_mutations` and `claim_mutations_on_subclass`;
- `stub_overlay`, `config_schema`, and `default_config`.

Declare the required companion version in the Python package dependencies. The legacy
`ty_compatibility` field remains a manifest field; it does not select official ty.
The protocol and SDK are versioned independently from the companion and checker.

## Hook Reference

| `Plugin` method           | Capability                | Typical response        |
| ------------------------- | ------------------------- | ----------------------- |
| `analyze_class`           | `class-transform`         | `ClassPatch`            |
| `analyze_classes`         | `class-batch`             | `ClassBatchResponse`    |
| `resolve_class_member`    | `class-member`            | `MemberPatch`           |
| `resolve_instance_member` | `instance-member`         | `MemberPatch`           |
| `adjust_call_signature`   | `call-signature`          | `CallSignaturePatch`    |
| `adjust_call_return`      | `call-return`             | `CallReturnPatch`       |
| `adjust_call_state`       | `call-state`              | `CallStatePatch`        |
| `build_project_index`     | `project-index`           | `ProjectIndexResponse`  |
| `additional_dependencies` | `additional-dependencies` | `Vec<PluginDependency>` |
| `validate_mutation`       | `mutation-validation`     | `MutationResponse`      |

The `dsl` module provides small constructors for fields, parameters, signatures, members, class
patches, and call responses. For shapes not covered by a helper, use the re-exported protocol types
directly.

## Type Expressions

Types cross the boundary as `TypeExpr`, never as checker internals:

```rust
use ty_plugin_sdk::protocol::TypeExpr;

let annotation = TypeExpr::annotation("list[str]");
let expression = TypeExpr::expression("my_library.Model");

assert_eq!(annotation.expression, "list[str]");
assert_eq!(expression.expression, "my_library.Model");
```

Choose annotation mode for type syntax, expression mode for runtime symbol expressions, and stub
mode for a complete generated stub declaration.

## Test a Plugin

Test the native reference and the corresponding Monty implementation together:

1. Call native hook methods directly in Rust unit tests.
1. Call `Plugin::handle` or `Plugin::handle_json` to verify dispatch and wire serialization.
1. Replay the same requests through the Python hooks in Monty and compare their responses.
1. Run the packaged Python plugin through `ty-extended check` and `ty-extended server` against fixture projects.

The native reference is a development tool. Consumers install the Python plugin and
the companion, which embeds Monty.

## Package and Configure It

Ship the Python hook source and its Monty `PluginManifest` in the library's Python
package. Name the manifest `ty-plugin.json`, set its runtime to `monty`, and point
its artifact at the Python source. The Rust reference crate is not a consumer
dependency. Users enable installed-package discovery:

```toml
# ty.toml
[plugins]
auto-discover = true
```

For an explicitly managed artifact:

```toml
# ty.toml
[plugins]
enabled = true

[[plugins.plugin]]
id = "my-plugin"
path = ".ty/plugins/my_plugin.py"
runtime = "monty"
manifest-path = ".ty/plugins/my-plugin.json"
trusted = true
```

See the [ty-extended plugin authoring
guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/plugin-authoring.md) for the
end-to-end packaging workflow and the [runtime
guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/plugin-runtime.md) for host
loading, sandboxing, and failure behavior.

## Object state

Protocol 0.6 supports `adjust_call_state` independently of a call's return type. Return
`dsl::call_state(CallStatePatch { receiver_members, ..Default::default() })` to describe
members after successful synchronous completion. `result_members` requires `fresh_result = true`.
Set `preserves_other_objects = true` only when the call cannot mutate another existing object.

Name aliases share receiver facts; branches union them. Unknown calls discard facts. Inherited
state claims exclude overridden implementations. See the [object member state guide](https://github.com/regularkevvv/ty-extended/blob/main/docs/plugin-authoring.md#object-member-state)
for contracts and conservative fallbacks.

Protocol 0.7 adds constructor claims: `claim_call_state_constructor("pkg.Record")` selects
one class, and `claim_call_state_constructors_on_subclass("pkg.Record")` selects its
subclasses, including the base itself. Constructor hooks receive the constructed class name
and arguments; they must account for custom initialization before guaranteeing fresh results.
