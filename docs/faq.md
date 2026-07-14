# FAQ

This page covers ty-extended and its semantic extension system. For typing behavior inherited from
ty, see [ty's upstream typing FAQ](https://docs.astral.sh/ty/reference/typing-faq/).

## What Is the Difference Between ty and ty-extended?

ty-extended is a fork of ty that adds a public semantic extension protocol, a Rust authoring SDK,
plugin configuration, and sandboxed WASM execution. It otherwise keeps the `ty` command, language
server, project discovery, configuration, and type-system behavior.

## Why Is the Executable Still Named `ty`?

The Python distribution is named `ty-extended`, but it installs the `ty` executable so existing
commands and editor configuration continue to work.

## What Does ty-extended Publish?

There are three public distributions:

- `ty-extended` on PyPI: the checker and language server with extension hosting;
- `ty_plugin_sdk` on crates.io: the API most extension authors use;
- `ty_plugin_protocol` on crates.io: the serialized data contract used by the SDK and host.

The SDK re-exports the protocol crate, so extension implementations normally depend only on
`ty_plugin_sdk`.

## Are Extensions Enabled by Default?

No. Installed extension packages require `plugins.auto-discover = true`. Manually configured
artifacts require `plugins.enabled = true`, an explicit plugin entry, and `trusted = true`.

## Why Use WebAssembly?

WASM keeps extensions behind a serialized boundary instead of exposing unstable checker internals.
ty-extended runs WASM with Wasmtime and does not provide WASI, filesystem, environment, clock, or
network access. Each call also has fuel, memory, and response-size limits.

## What Can an Extension Change?

An extension declares claims and capabilities in its manifest. At claimed semantic points it can
return declarative patches for class shapes, members, call signatures, return types, project
indexes, dependencies, mutation diagnostics, or stub overlays. The host validates those patches
before applying them.

## Can an Extension Access ty Internals?

No. Extensions receive serialized summaries and return protocol patches. They do not receive Salsa
keys, AST ids, checker-owned type objects, or direct access to `ty_python_semantic`.

## Is the Extension API Stable?

The protocol and SDK are pre-1.0 and versioned independently from ty-extended. Hosts negotiate the
protocol version declared by an extension and reject incompatible manifests. Extension packages
should also declare a narrow ty compatibility range and test against every supported release.

## Where Should I Report a Problem?

Report extension loading, WASM runtime, SDK, protocol, or ty-extended packaging issues in the
[ty-extended issue tracker](https://github.com/regularkevvv/ty-extended/issues). Check the
[upstream ty documentation](https://docs.astral.sh/ty/) for behavior shared unchanged with ty.
