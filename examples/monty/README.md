# Monty plugin example

A minimal end-to-end [ty plugin](https://docs.astral.sh/ty/) written in Python and
executed inside the embedded [Monty](https://github.com/pydantic/monty) sandbox —
no Rust toolchain required.

## Layout

```text
.ty/plugins/
  tokens.py            # the plugin (plain Python, ambient ty.plugin_sdk prelude)
  tokens.plugin.json   # manifest: claims, capabilities, runtime spec
example.py             # a library module the plugin models
app.py                 # call site where the hook fires
pyproject.toml         # enables plugins and registers the artifact
```

## What it does

`example.issue_token` is left unannotated, so without a plugin its return type is
`Unknown`. The manifest in `tokens.plugin.json` claims
`example.issue_token` for the `call-return` capability; when ty checks the call
in `app.py`, it dispatches an `adjust-call-return` request to the plugin. The
plugin answers with a patch whose `return-type` is the annotation
`example.Token`, and ty applies it.

## Running it

Install `ty-extended`, then run:

```sh
ty check
```

The standard package runs plugins in the embedded interpreter and needs no external worker.
For optional worker execution, install `ty-extended[monty-workers]` and add
`monty-mode = "worker"` under `[tool.ty.plugins]`. See the
[runtime guide](../../docs/plugin-runtime.md#monty-worker-processes) for installation and platform
support.

Expected output:

```text
info[revealed-type]: Revealed type
 --> app.py:5:13
  |
5 | reveal_type(token)
  |             ^^^^^ `Token`
```

`Token` here is `example.Token` — supplied entirely by the Python plugin.

## Authoring notes

- Plugin artifacts are plain Python run inside Monty's restricted interpreter:
    no third-party imports, no filesystem or environment access. The
    `ty.plugin_sdk` API is prepended by the runner, so hook decorators
    (`@on_call_return`), response builders (`call_return_patch`, `type_expr`), and
    manifest helpers (`manifest`, `capabilities`, `set_manifest`) are ambient
    names.
- The manifest's `runtime.kind` must be `"monty"` and `runtime.artifact` names
    the plugin file; `path` in `pyproject.toml` points at that file and
    `manifest-path` at the JSON manifest.
- `trusted = true` is required for ty to execute local plugin artifacts.
- Plugin files are excluded under `[tool.ty.src]` because ambient SDK names do
    not resolve outside the sandbox.
