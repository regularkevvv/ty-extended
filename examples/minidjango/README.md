# Mini-Django: one plugin, two runtimes

This example ships the same realistic semantic plugin twice:

|            | `monty/`                        | `wasm/`                                              |
| ---------- | ------------------------------- | ---------------------------------------------------- |
| Language   | Python (`ty.plugin_sdk`)        | Rust (`ty_plugin_sdk`)                               |
| Runtime    | `monty` (sandboxed interpreter) | `wasm` (Wasmtime)                                    |
| Artifact   | `.ty/plugins/minidjango.py`     | `.ty/plugins/minidjango.wasm`                        |
| Build step | none                            | `./build-plugin.sh` (needs `wasm32-unknown-unknown`) |

The project under check lives once in `shared/` (`minidjango.py`,
`minidjango_settings.py`, `models.py`, `app.py`); both runtime directories
symlink to it, so the sources cannot drift. Only the plugin implementation,
its manifest, and `pyproject.toml` differ. The monty plugin is a port of the
reference `MiniDjangoPlugin` in `ruff/crates/ty_plugin_examples`.

## What the plugin does

- **`analyze-class`** — `Model` subclasses gain `id`/`pk`, a per-model virtual
    `Manager` as `objects`/`_default_manager`, keyword-only constructor
    parameters per field, and `<fk>_id` columns for `ForeignKey`s.
- **`build-project-index`** — indexes model fields, declares virtual types
    (`Manager`, `ValuesRow` TypedDict, `ValuesListRow` NamedTuple), contributes
    reverse relations (`user.posts` from `Post.author`) to FK targets,
    and reports relation diagnostics.
- **`adjust-call-return`** — `filter`/`get`/`get_or_create`/`first`/`count`/
    `exists`/`values`/`values_list`/`annotate` return model- and row-aware types,
    and `field__path__lookup` arguments are validated against indexed fields.
- **`settings` claims** — `ForeignKey(minidjango_settings.AUTH_USER_MODEL)`
    resolves through the summarized settings module.

## Try it

```console
$ cd monty && path/to/ty check
$ cd ../wasm && ./build-plugin.sh && path/to/ty check
$ diff <(cd monty && ty check) <(cd wasm && ty check)   # identical
```

On Windows, symlinked checkouts need Git's `core.symlinks` support; without it,
copy `shared/*.py` into each runtime directory instead.

Expected diagnostics (all intentional): `minidjango.unknown-lookup`,
`minidjango.invalid-lookup-value`, `minidjango.reverse-relation-conflict`, and
`minidjango.unknown-relation-target`.

## Authoring notes

The plugin source imports `ty.plugin_sdk` names under
`typing.TYPE_CHECKING` — editors and `ty` resolve them for completions and
type errors, while inside Monty the same names are ambient (the SDK prelude
runs before the plugin) and the block never executes.
