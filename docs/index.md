# ty-extended

Extend [Ty](https://github.com/astral-sh/ty) type checker with plugins.
Plugins use the Python subset supported by Monty.
ty-extended runs plugins and translates source locations; the separately installed,
unchanged official ty performs type checking.

```shell
uv add --dev ty-extended
uv run ty-extended check
```

`ty-extended server` starts the plugin-aware editor server. `ty` remains Astral's
command. ty-extended has independent releases and supports groups of ty versions;
see [compatibility](compatibility.md) for the tested range and upgrade policy.

- [Installation and migration](installation.md)
- [Editor setup](editors.md)
- [Plugin authoring](plugin-authoring.md)
- [Runtime behavior](plugin-runtime.md)
- [FAQ](faq.md)
- [Publishing](release-publishing.md)

[`django-ty`](https://github.com/regularkevvv/django-ty) supplies Django ORM semantics.
For Python typing features and core checker behavior, use
[Astral's documentation](https://docs.astral.sh/ty/).

Versions through 0.86.x shipped a modified checker. Their WASM and worker runtimes
are described in the [historical guide](history/0.86/plugin-runtime.md).
