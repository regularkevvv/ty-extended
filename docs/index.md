# ty-extended

A Monty plugin companion for unchanged official ty. The companion owns plugin
execution and translates source locations; Astral's checker owns type checking.

```shell
uv add --dev ty-extended
uv run ty-extended check
```

`ty-extended server` starts the plugin-aware editor server. `ty` remains Astral's
command. The companion has independent releases and supports groups of ty versions;
see [compatibility](compatibility.md) for the tested range and upgrade policy.

- [Installation and migration](installation.md)
- [Editor setup](editors.md)
- [Monty plugin authoring](plugin-authoring.md)
- [Runtime behavior](plugin-runtime.md)
- [FAQ](faq.md)
- [Publishing](release-publishing.md)

[`django-ty`](https://github.com/regularkevvv/django-ty) supplies Django ORM semantics.
For Python typing features and core checker behavior, use
[Astral's documentation](https://docs.astral.sh/ty/).

Versions through 0.86.x shipped a modified checker. Their WASM and worker runtimes
are described in the [historical guide](history/0.86/plugin-runtime.md).
