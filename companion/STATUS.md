# Rust companion validation

The companion runs Monty semantic plugins in Rust and delegates type checking to
a separate, unchanged official ty executable. CLI and editor requests use the
same persistent project model. Source projections never replace project files;
explicit `--fix` and `--add-ignore` edits map back to the originals.

## Implemented

- Plugin discovery, multiple plugin configurations, virtual types, class/member
    hooks, call signatures/returns, mutation hooks and object state tracking.
- Configuration, source selection, extra import paths, Python targets, watch
    mode, safe fixes, ignore comments and notebook files.
- Persistent LSP sessions with unsaved files, notebooks, multiple workspaces,
    diagnostics, completion, hover, navigation, rename and code actions. Locations
    and edits translate between UTF-8, UTF-16 and UTF-32.
- Cached parsing and plugin responses, dependency-based incremental projection,
    conservative invalidation, and convergence checks sized to the program.

Embedded Monty is the supported runtime. WASM and the Python worker runtime are
not included. Official ty remains a separate installation. Parser and plugin SDK
crates come from the pinned Ruff submodule; the companion does not link ty's
semantic engine, Salsa or Wasmtime.

## Validation status

The final release build, complete Django matrix and hosted Linux/macOS/Windows
checks are in progress. **This branch is not yet marked complete.**

Current local checks pass: 112 Rust tests, 19 harness tests, real CLI/watch/fix
checks, and editor differentials across all three position encodings. The
unchanged Django contract uses official ty 0.0.84 and ty-extended 0.84.4; the
generic SDK/editor comparisons use official 0.0.86 and ty-extended 0.86.0.

The existing [evidence directory](evidence/README.md) records the earlier prototype.
It must be replaced with results from the final binary before this work is
reported complete. [Reproduction instructions](tests/DJANGO_ACCEPTANCE.md) describe
the full Django and protocol gates.
