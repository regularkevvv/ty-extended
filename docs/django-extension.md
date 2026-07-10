# Django Extension

`django-ty` is the reference semantic extension for Django projects. It is distributed as a PyPI
package, not as a crates.io crate.

Install it in a Python project:

```shell
uv add --dev django-ty
uv run django-ty install
```

The installer writes:

- `.ty/plugins/django_ty.wasm`
- `.ty/plugins/django-ty.plugin.json`

Enable it in `ty.toml`:

```toml
[plugins]
enabled = true

[[plugins.plugin]]
id = "django-ty"
path = ".ty/plugins/django_ty.wasm"
runtime = "wasm"
manifest-path = ".ty/plugins/django-ty.plugin.json"
trusted = true
```

## Scope

The current extension targets Django model typing:

- field descriptor and constructor typing
- `id`, `pk`, `objects`, and `_default_manager`
- `ForeignKey`, `OneToOneField`, and `ManyToManyField` relation targets
- reverse relation contributions
- `AUTH_USER_MODEL` and static settings references
- manager and queryset return types for common ORM methods
- `values()`, `values_list()`, `annotate()`, and lookup validation

The extension does not import checker internals and does not run `django.setup()`. It uses static
project summaries supplied by the ty extension protocol.

## Release

Configure PyPI trusted publishing for:

- Project: `django-ty`
- Owner: `regularkevvv`
- Repository: `django-ty`
- Workflow: `publish-pypi.yml`
- Environment: `release`

The release workflow builds the WASM target, stages it into the Python package, builds the wheel,
and publishes through PyPI OIDC.
