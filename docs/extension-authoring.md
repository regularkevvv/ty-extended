# Authoring Semantic Extensions

ty-extended can load trusted semantic extensions that participate in type inference through a
stable JSON protocol. Extension authors write a Rust crate against the public SDK, compile it to
WebAssembly, and configure `ty` to load the resulting `.wasm` artifact.

The public crates are:

- [`ty_plugin_protocol`](https://crates.io/crates/ty_plugin_protocol): serialized manifests,
    requests, responses, claims, patches, diagnostics, and type expressions.
- [`ty_plugin_sdk`](https://crates.io/crates/ty_plugin_sdk): `ManifestBuilder`, the `Plugin`
    trait, typed DSL helpers, JSON dispatch, and the WASM export macro.

Extension crates should depend on `ty_plugin_sdk` only. The SDK re-exports the protocol as
`ty_plugin_sdk::protocol`. Do not depend on checker internals such as `ty_python_semantic`, Salsa,
AST ids, or `ty_plugin_host`; needing those is a protocol gap.

## Crate Layout

```toml
[package]
name = "my-ty-extension"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["rlib", "cdylib"]

[dependencies]
ty_plugin_sdk = "0.0.1"
```

```rust
use ty_plugin_sdk::protocol::{CallRequest, PluginManifest, PluginResponse, TypeExpr};
use ty_plugin_sdk::{dsl, ManifestBuilder, Plugin};

#[derive(Default)]
pub struct MyExtension;

impl Plugin for MyExtension {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("my-extension", "My ty extension", "0.1.0")
            .claim_call_return("my_framework.Field")
            .build()
    }

    fn adjust_call_return(&self, _request: &CallRequest) -> PluginResponse {
        dsl::call_return(TypeExpr::annotation("str"))
    }
}

ty_plugin_sdk::export_plugin!(MyExtension::default());
```

Build the WASM artifact:

```shell
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
```

## Manifests

An extension manifest declares:

- identity: `id`, `name`, `version`
- compatibility: protocol version and ty version requirement
- runtime: usually `wasm`
- capabilities: which hooks the extension implements
- claims: which classes, functions, methods, attributes, settings, or overlays the extension owns

`ManifestBuilder` keeps capabilities and claims in sync. For example,
`claim_call_return_method_on_subclass` both records the method claim and enables the `call-return`
capability.

## Hooks

Extensions receive summarized semantic data and return patches. They never receive internal checker
types.

| Hook                      | Use it for                                                                            | Response             |
| ------------------------- | ------------------------------------------------------------------------------------- | -------------------- |
| `analyze_class`           | Add fields, members, constructors, diagnostics, or virtual types for claimed classes. | `ClassPatch`         |
| `resolve_class_member`    | Provide class members after normal lookup misses.                                     | `MemberPatch`        |
| `resolve_instance_member` | Provide instance members after normal lookup misses.                                  | `MemberPatch`        |
| `adjust_call_signature`   | Replace the signature bound at a claimed call site before argument checking.          | `CallSignaturePatch` |
| `adjust_call_return`      | Override the inferred return type at a claimed call site.                             | `CallReturnPatch`    |
| `build_project_index`     | Build extension-owned project data and cross-symbol contributions.                    | `ProjectIndexPatch`  |
| `additional_dependencies` | Add files that should fingerprint extension results.                                  | `DependencyPatch`    |

Return `PluginResponse::NoChange` when the extension has nothing to add.

## Type Expressions

The protocol uses `TypeExpr` instead of exposing ty's internal type representation. A `TypeExpr`
contains source text plus a mode: expression, annotation, or stub. Prefer SDK constructors:

```rust
TypeExpr::annotation("str")
TypeExpr::expression("my_framework.Model")
```

The host accepts unknown or unsupported type expressions by falling back to `Unknown`; invalid
extension output must not crash type checking.

## Project Configuration

A project enables a trusted extension in `ty.toml`:

```toml
[plugins]
enabled = true

[[plugins.plugin]]
id = "my-extension"
path = ".ty/plugins/my_extension.wasm"
runtime = "wasm"
manifest-path = ".ty/plugins/my-extension.plugin.json"
trusted = true
```

The `.wasm` file and manifest are normally installed by the extension's package manager wrapper.
For Python framework extensions, publish a PyPI package that carries the WASM artifact and writes
the `.ty/plugins` files for the project.

## Publishing an Extension

The recommended packaging shape is:

1. Keep the Rust extension crate private to the extension repository.
1. Build the crate as `wasm32-unknown-unknown` in release automation.
1. Package the `.wasm` artifact and manifest in the framework ecosystem's package format.
1. Provide an installer command that writes `.ty/plugins/<extension>.wasm`, the manifest JSON, and
    the matching `ty.toml` snippet.

For a Python framework, publish a PyPI package rather than a crates.io crate. The Rust crate is an
implementation detail; users should install the extension with the Python dependency manager they
already use for the project.

## Versioning

The protocol is pre-1.0. Manifests declare the protocol version they were built against. Hosts
reject extensions with a different major version or with a newer minor version than the host
supports.

Use a narrow compatibility range when publishing framework extensions, and test against the
ty-extended release you target.
