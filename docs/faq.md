# FAQ

## How does ty-extended relate to ty?

The current package is a companion, not a fork. It runs semantic plugins in Monty
and asks a separately installed official ty to check analysis copies. It installs
`ty-extended` and `ty_extended`; official ty owns `ty` and its Python module.

## Must the companion release with every ty update?

No. Companion, plugin protocol and official checker versions have separate roles.
A companion release supports a group of ty releases. Tests qualify upgrades through
real CLI and editor sessions. Upstream changes to configuration, diagnostics, type
queries or LSP behavior can require a compatibility fix; see [the policy](compatibility.md).

## How do I enable plugins?

Set `plugins.auto-discover = true` to discover installed plugins. Explicit artifact
registrations also require `plugins.enabled = true` and `trusted = true`.
See [runtime configuration](plugin-runtime.md#configuration-and-trust).

## Which runtimes are included?

Embedded Monty. It does not need an external interpreter, `pydantic-monty`, a worker
package or a compilation step for the plugin. WASM and external workers belong to
[historical releases](history/0.86/plugin-runtime.md).

## Can plugins access checker internals or execute my project?

Plugins receive serialized summaries and return declarative patches. They do not
receive checker-owned objects or direct filesystem, environment or network access.
The companion summarizes project Python statically. Read the
[runtime boundaries](plugin-runtime.md) before treating the embedded interpreter
as a process sandbox.

## Where do I report an issue?

Report companion, SDK, plugin loading and packaging problems in the
[ty-extended tracker](https://github.com/regularkevvv/ty-extended/issues). Report a
plugin's library behavior in that plugin's repository. Include both `ty --version`
and `ty-extended --version`, the configuration and a small reproduction.
