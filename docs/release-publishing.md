# Publishing ty-extended

The `ty-extended` PyPI distribution packages the native `tyx` executable, its
small `ty-extended` compatibility launcher, and the
`ty_extended` Python SDK. It depends on Astral's official `ty` distribution.
Native archives contain both commands. Neither artifact installs
or replaces Astral's `ty` executable or Python module.

## Versioning and source

ty-extended versions are independent of official ty starting with 0.87.0. Bump the
Python project, ty-extended Cargo package and `dist-workspace.toml` together. Update
the changelog and the tested compatibility ledger. A routine official ty update
does not require a ty-extended release.

Ruff parsing and notebook libraries are pinned registry dependencies. Our host,
protocol and SDK crates live in this workspace; no fork checkout is required.
All Rust workspace crates are internal and set `publish = false`. Releases
publish the application and bundled Python SDK; they do not publish Rust SDK
or protocol crates to crates.io. Previously published crate versions remain
available for historical consumers. Plugin authors use `ty_extended.plugin_sdk`
and write Python plugins executed by Monty.

## Release gates

1. Qualify the same ty-extended binary across the declared tested ty releases.
1. Run Rust, SDK, Django, runtime and real editor regression gates.
1. Build wheels and the source distribution; install them into clean environments
    alongside official ty and verify plugin discovery, CLI and editor behavior.
1. Require green Linux, macOS and Windows CI for the exact commit being merged.
1. Merge the reviewed change, wait for CI on the merged main commit to pass, then
    dispatch `release.yml` with its version tag.
1. Monitor every release job, then install the published PyPI artifacts and repeat
    end-to-end smoke checks. GitHub release creation alone is not package acceptance.

The existing PyPI trusted publisher uses repository `regularkevvv/ty-extended`,
workflow `release.yml`, environment `release`. Publishing uses OIDC, without
reading or storing a long-lived package token.

## Workflow maintenance

Parts of `release.yml` are cargo-dist generated. Change `dist-workspace.toml` or a
referenced reusable workflow, regenerate with cargo-dist 0.31.0, and inspect the
diff. Custom build jobs package the application in `crates/ty_extended/`.
Reviewed release gates and download/checksum steps are stored in
`.github/release-customizations.json`. Regenerate the workflow reproducibly with:

```shell
uv run --only-group docs python scripts/generate_release_workflow.py --dist /path/to/dist
```

Use `--check` to verify regeneration without changing the workflow. The build
backend audits Cargo's active graph before marking standalone binary wheels as
`py3-none`: Maturin's metadata detection otherwise mistakes jiter's inactive
optional PyO3 dependency for a Python ABI requirement. The audit rejects active
Python bindings, semantic-engine dependencies and extension libraries.

Keep build artifacts bounded: disable incremental builds, limit local compiler
jobs, preserve required binaries and reports outside `target`, and run
`cargo clean --manifest-path crates/ty_extended/Cargo.toml` after each build/test phase.

## Publishing the django-ty plugin

The `django-ty` PyPI distribution packages the Monty plugin under
`plugins/django-ty/`. It versions independently of `tyx`: `v*` tags belong to
ty-extended releases, while plugin releases use `django-ty-v*` tags.

To release a plugin version:

1. Bump `version` in `plugins/django-ty/pyproject.toml`, then regenerate the
    packaged manifest with `python plugins/django-ty/scripts/build-monty.py`.
    The script rewrites `python/django_ty/ty-plugin.json`,
    `python/django_ty/ty-plugin-monty.json`, and the manifest embedded in
    `python/django_ty/monty.py` from `ty-plugin.json`. Its `--check` mode —
    run by `scripts/build-wheel.sh` inside the publish workflow — fails unless
    the manifest `version` matches the `pyproject.toml` version and all three
    generated outputs are current.
1. Merge the reviewed change and wait for the `.github/workflows/django-ty.yml`
    push run on the merged main commit to succeed.
1. Dispatch `.github/workflows/publish-django-ty.yml` with the `version`
    input set to the new version. The workflow refuses to publish when the
    input does not equal the `pyproject.toml` version or when main CI has not
    passed for the exact commit, then builds, smoke-tests, attests, and
    publishes the wheel and sdist before pushing the `django-ty-v<version>`
    tag and a `django-ty <version>` GitHub release.

The `django-ty` PyPI project needs its own trusted publisher: repository
`regularkevvv/ty-extended`, workflow `publish-django-ty.yml`, environment
`release`. Like the `ty-extended` publisher, it uses OIDC without a
long-lived package token.

## Historical distributions

Versions through 0.86.x used checker-based version numbers and shipped a modified
`ty` binary. Their release notes and artifacts retain that historical identity.
See [migration](installation.md#migrating-from-the-old-distribution) before replacing
an old installation.
