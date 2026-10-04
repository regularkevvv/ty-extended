# FAQ

This page covers ty-extended and its semantic plugin system. For typing behavior inherited from
ty, see [ty's upstream typing FAQ](https://docs.astral.sh/ty/reference/typing-faq/).

## What Is the Difference Between ty and ty-extended?

ty-extended is a fork of ty that adds a public semantic plugin protocol, Rust and Python authoring
SDKs, plugin configuration, and sandboxed execution through Wasmtime and Monty. It otherwise keeps
the `ty` command, language server, project discovery, configuration, and type-system behavior.

## Why Is the Executable Still Named `ty`?

The Python distribution is named `ty-extended`, but it installs the `ty` executable so existing
commands and editor configuration continue to work.

## What Does ty-extended Publish?

There are three public distributions:

- `ty-extended` on PyPI: the checker and language server with plugin hosting;
- `ty_plugin_sdk` on crates.io: the API Rust plugin authors use;
- `ty_plugin_protocol` on crates.io: the serialized data contract used by the SDK and host.

The Rust SDK re-exports the protocol crate, so Rust plugin implementations normally depend only on
`ty_plugin_sdk`. Python plugins use the `ty.plugin_sdk` API included in the `ty-extended` wheel.

## Are Plugins Enabled by Default?

No. Installed plugin packages require `plugins.auto-discover = true`. Manually configured
artifacts require `plugins.enabled = true`, an explicit plugin entry, and `trusted = true`.

## Which Plugin Runtimes Are Supported?

Rust plugins compile to WebAssembly and run in Wasmtime. Python plugins run in Monty's restricted
interpreter without a compilation step. Both runtimes use the same serialized protocol and
manifest format, and neither exposes checker internals, filesystem, environment, clock, or network
access. Execution limits and crash protections differ between the runtimes; see the
[runtime safety model](./plugin-runtime.md#safety-model).

## Do Python Plugins Need an External Monty Runtime?

No. The standard `ty-extended` package includes embedded Monty and runs Python plugins in-process
by default. It does not require the `pydantic-monty` Python client library or an external worker.

For a separate process and hard timeouts, install `ty-extended[monty-workers]` and select
`plugins.monty-mode = "worker"`. The extra supplies `pydantic-monty-runtime` on supported platforms;
installing it alone does not change the default mode. See
[Monty worker processes](./plugin-runtime.md#monty-worker-processes) for installation and platform
details.

## What Can a Plugin Change?

A plugin declares claims and capabilities in its manifest. At claimed semantic points it can
return declarative patches for class shapes, members, call signatures, return types, project
indexes, dependencies, mutation diagnostics, or stub overlays. The host validates those patches
before applying them.

## Can a Plugin Access ty Internals?

No. Plugins receive serialized summaries and return protocol patches. They do not receive Salsa
keys, AST ids, checker-owned type objects, or direct access to `ty_python_semantic`.

## Is the Plugin API Stable?

The protocol and SDK are pre-1.0 and versioned independently from ty-extended. Hosts negotiate the
protocol version declared by a plugin and reject incompatible manifests. Plugin packages
should also declare a narrow ty compatibility range and test against every supported release.

## Where Should I Report a Problem?

Report plugin loading, Wasmtime or Monty runtime, SDK, protocol, or ty-extended packaging issues in the
[ty-extended issue tracker](https://github.com/regularkevvv/ty-extended/issues). Check the
[upstream ty documentation](https://docs.astral.sh/ty/) for behavior shared unchanged with ty.
