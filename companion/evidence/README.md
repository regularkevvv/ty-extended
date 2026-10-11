# Verified experiment evidence

These reports were exported only after both 18-pair Django matrices, all three generic checker modes, protocol replay, unit tests, and verified Django benchmarks passed. This proves the recorded corpus; it does not claim every plugin or editor workflow is supported.

`provenance.json` pins sources, checker binaries and raw gate logs. `matrix.json` retains installed dependency fingerprints and every contract case. `diagnostics.json` deduplicates complete normalized diagnostic arrays by hash. Generic non-reveal wording is not its parity contract; both actual message arrays are retained.

`benchmark.json` retains every first-observed, warmup and measured sample plus native counters. Each invocation starts fresh processes; push mode reuses its official checker within an invocation only.

`source:` and `binary:` labels identify inputs; `artifact:` labels identify original local reports. The unmodified originals and the label/path mapping remain under the acceptance root. Reproduction commands are documented in ../tests/DJANGO_ACCEPTANCE.md and the exporter accepts `--help`. Generated projections and build caches are intentionally absent.

`benchmark-method-note.json` records the actual per-workload order and desktop load; repetitions were not order-balanced. `known-upgrade-failure.json` retains the separate 0.86 reference-fork failure. `artifact.json` records the preserved executable size/hash. The original hook corpus is in `protocol-inputs/`.

`cleanup.json` records generated-output deletion, unchanged Git worktrees, and allocated versus observed free-space measurements.
