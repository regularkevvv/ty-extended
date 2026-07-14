# Publishing This Fork

This fork publishes three things:

- `ty-extended` on PyPI. The installed executable and Python module remain named `ty`.
- `ty_plugin_protocol` and `ty_plugin_sdk` on crates.io for extension authors.
- Release artifacts from the `ruff` submodule source, which is hosted as
    `regularkevvv/ruff-extended`.

The PyPI project name `ty` belongs to Astral. Publishing this fork under that exact project name
requires being added as an owner or maintainer of the existing PyPI project. The fork-owned path is
to publish the distribution as `ty-extended` and keep the binary name as `ty`.

## Versioning

`ty-extended` uses SemVer-compatible fork versioning that records the upstream ty base:

- upstream `0.0.58` maps to the initial `ty-extended 0.58.0` release;
- later fork releases on the same upstream base increment the patch, such as `0.58.1` and `0.58.2`;
- upstream `0.1.50` maps to `ty-extended 0.150.0`;
- once upstream reaches `1.0.0`, ty-extended follows that shape directly as `1.0.x`.

The SDK crates are versioned independently. `ty_plugin_protocol` and `ty_plugin_sdk` start at
`0.0.1` and only bump when their public protocol or SDK surface changes.

## Trusted Publishers

Use trusted publishing instead of long-lived release tokens.

PyPI trusted publisher:

- Project: `ty-extended`
- Owner: `regularkevvv`
- Repository: `ty-extended`
- Workflow: `release.yml`
- Environment: `release`

crates.io trusted publishers:

- Crates: `ty_plugin_protocol`, `ty_plugin_sdk`
- Owner: `regularkevvv`
- Repository: `ty-extended`
- Workflow: `release.yml`
- Environment: `release`

The top-level release workflow calls `.github/workflows/publish-pypi.yml` and
`.github/workflows/publish-sdk-crates.yml`. Both jobs request `id-token: write` and run in the
`release` environment. The SDK crates are released from the `ruff` submodule source, but their
trusted publisher is the top-level `regularkevvv/ty-extended` release workflow.

## One-Time crates.io Setup

Before the first SDK release, reserve and configure the crate names from the `ruff` submodule:

```shell
cd ruff
CARGO_REGISTRY_TOKEN=<token-with-publish-new-and-trusted-publishing> \
  uv run --script scripts/setup-crates-io-publish.py \
    --package ty_plugin_protocol \
    --package ty_plugin_sdk \
    --dry-run
CARGO_REGISTRY_TOKEN=<token-with-publish-new-and-trusted-publishing> \
  uv run --script scripts/setup-crates-io-publish.py \
    --package ty_plugin_protocol \
    --package ty_plugin_sdk
```

The setup script publishes a `0.0.0` placeholder for new crates, creates the trusted publisher
configuration for `regularkevvv/ty-extended/.github/workflows/release.yml`, enables
`trustpub_only`, and records configured crate names in `ruff/.known-crates`.

## Release

1. Ensure `.gitmodules` points `ruff` at `https://github.com/regularkevvv/ruff-extended`.
1. Push the signed `ruff` commit to `regularkevvv/ruff-extended`.
1. Push the signed `ty-extended` commit that records that submodule commit.
1. In PyPI, create the pending publisher for `ty-extended`.
1. In crates.io, complete the one-time SDK crate setup above.
1. Dispatch `.github/workflows/release.yml` with the release tag.
1. Confirm the release artifacts include the `ty-extended` wheels and both SDK crate versions.

The `publish-sdk-crates` job verifies the `ruff` submodule URL before minting the crates.io OIDC
token, so a release cannot silently publish SDK crates from the upstream Astral submodule.
