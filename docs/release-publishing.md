# Publishing ty-extended

The `ty-extended` PyPI distribution packages the Rust executable and the
`ty_extended` Python SDK. It depends on Astral's official `ty` distribution.
Native archives contain the `ty-extended` executable. Neither artifact installs
or replaces Astral's `ty` executable or Python module.

## Versioning and source

ty-extended versions are independent of official ty starting with 0.87.0. Bump the
Python project, ty-extended Cargo package and `dist-workspace.toml` together. Update
the changelog and the tested compatibility ledger. A routine official ty update
does not require a ty-extended release.

Ruff parsing and notebook libraries are pinned registry dependencies. Our host,
protocol and SDK crates live in this workspace; no fork checkout is required.
Rust SDK and protocol crates remain independently versioned. Publishing
ty-extended does not require republishing unchanged SDK crates.

## Release gates

1. Qualify the same ty-extended binary across the declared tested ty releases.
1. Run Rust, SDK, Django, runtime and real editor regression gates.
1. Build wheels and the source distribution; install them into clean environments
    alongside official ty and verify plugin discovery, CLI and editor behavior.
1. Require green Linux, macOS and Windows CI for the exact commit being merged.
1. Merge the reviewed change and dispatch `release.yml` with its version tag.
1. Monitor every release job, then install the published PyPI artifacts and repeat
    end-to-end smoke checks. GitHub release creation alone is not package acceptance.

The existing PyPI trusted publisher uses repository `regularkevvv/ty-extended`,
workflow `release.yml`, environment `release`. Publishing uses OIDC, without
reading or storing a long-lived package token.

## Workflow maintenance

Parts of `release.yml` are cargo-dist generated. Change `dist-workspace.toml` or a
referenced reusable workflow, regenerate with cargo-dist 0.31.0, and inspect the
diff. Custom build jobs package the application in `companion/`.
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
`cargo clean --manifest-path companion/Cargo.toml` after each build/test phase.

## Historical distributions

Versions through 0.86.x used checker-based version numbers and shipped a modified
`ty` binary. Their release notes and artifacts retain that historical identity.
See [migration](installation.md#migrating-from-the-old-distribution) before replacing
an old installation.
