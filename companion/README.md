# Rust companion experiment

A Rust frontend that runs semantic plugins in embedded Monty and checks generated
analysis files with a separately installed, unchanged official `ty` executable.
It does not modify the original Python files. Diagnostics are mapped back to them.

This is an experimental project checker, not a released replacement for
ty-extended. See [STATUS.md](STATUS.md) for verified gates and remaining work.
The crate uses the existing Rust plugin host and protocol, but does not link ty's
semantic engine, language server, Salsa, or Wasmtime.

```sh
cargo +1.98.0 build --release --manifest-path companion/Cargo.toml

companion/target/release/ty-companion-rust project /path/to/project \
  --plugin /path/to/plugin.py --manifest /path/to/manifest.json \
  --stubs /path/to/stubs --ty /path/to/official/ty \
  --config /path/to/plugin-config.json --python /path/to/environment \
  --python-version 3.13 --checker-mode push --out /tmp/generated-analysis
```

`--config` contains the plugin's JSON configuration. Optional `--files` contains a
JSON array of project-relative paths to check. Use a fresh output directory
outside the original project. `push` reuses one official ty server; `cli` starts
batch checks separately. Both perform a final check without type probes.

The current project interface accepts one explicit plugin. It does not implement
ty-extended's plugin discovery or an editor-facing LSP proxy. It uses the explicit
flags above; it does not merge an existing `ty.toml` or `pyproject.toml` checker
configuration. Only macOS has been verified, and plugin target context currently
uses `darwin`. See [STATUS.md](STATUS.md) for the remaining SDK and Python limits.

The Django gate uses the original fixtures, plugin, expected diagnostics, and
declared version matrix. Instructions are in
[tests/DJANGO_ACCEPTANCE.md](tests/DJANGO_ACCEPTANCE.md).

The earlier generic regression fixtures are included. Their manifest targets the
0.86 reference fork and official ty 0.0.86:

```sh
companion/target/release/ty-companion-rust suite \
  "$PWD/companion/tests/fixtures/generic" /tmp/generic-proof push \
  /path/to/official-ty-0.0.86 /path/to/ty-extended-0.86.0
```

That gate checks diagnostic codes and original positions, revealed types, exit
status, and explicit rejection of unsupported input in the earlier single-file
interface. The Django project gate additionally requires exact primary messages
and severities. LSP explanatory notes remain in `lsp_message`; `description`
matches the CLI's concise diagnostic format.
