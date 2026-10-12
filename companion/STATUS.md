# Rust companion validation

The companion runs embedded Monty semantic plugins with unchanged official ty.
CLI and LSP map diagnostics, positions and edits back to original files. WASM and
the optional Python worker are outside this build.

Measured source: `ce12535f2c1be7f56945b1be15d6ef020ea86ec6`. Executable SHA-256:
`ca3e6c4b0cd20f4cab5fbca46949ac2331ff017aa59f817e7228344c0c4f3c2c`.

This build repeats the complete Django matrix and SDK/editor gates below. It
also passes 183 Rust unit tests, two live-checker unit tests, all integration
suites, 38 Python harness tests, formatting and Clippy.

A [validation followup](evidence/speedup/overlay-revision-followup.json) makes
watched-file edits produce distinct modification timestamps. A Windows reference
check returned stale diagnostics; equal timestamps reproduce that behavior.
Normal and forced-collision runs pass with unchanged diagnostic expectations.
This fixture change leaves the measured executable and timings unchanged.

The optimization shares immutable metadata, caches type expressions and class
ancestry, batches imported modules, and reuses unchanged projection passes.
Renderers retain lightweight name sets instead of old project maps. Regression
tests cover cache invalidation, aliases, project edits, failed import retries,
protocol decoding and metadata lifetime.

| Passed gates  | Coverage                                                                                                                                                                                                 |
| ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Django        | All 18 supported Django/Python pairs in CLI and persistent-checker modes; each runs 166 assertions across 38 features, 83 runtime proofs, nine QueryDict comparisons, and positive/negative e2e projects |
| Public CLI    | All five output formats; exact diagnostics, exit status, original source spans and external related-note locations                                                                                       |
| Generic SDK   | 17 differential cases and four rejection controls in each of three checker modes; seven frozen CLI/SDK integration suites, two supplemental suites, and ten SDK contract inventories                     |
| Editor        | Six acceptance sessions; 105 ordinary and 66 notebook comparisons across UTF-8/16/32; configuration, multiple workspaces, unsaved files, lifecycle and native watch                                      |
| Django editor | 28 baseline diagnostics with exact spans; unsaved negative controls; ten comment and ten semantic revisions, preserving original files                                                                   |

The original 194-test SDK inventory is fully accounted for: **75 original tests
ran unchanged**, **90 public contracts passed through companion adapters**, and
**29 runtime/private-interface cases are explicit boundaries**. These are not
194 executed companion tests. The ledger records differences in internal hook
and index counters; public results match the accepted fixtures.

Both 150-request Django protocol replays pass:
8 MiB rejects one oversized response as expected; 16 MiB accepts all.
Windows regressions cover CLI/LSP self-import type identity and exact diagnostic
paths, including nested Unicode names, UNC and paths requiring verbatim prefixes.

Django comparisons pin **official ty 0.0.84 / ty-extended 0.84.4**; generic/editor
comparisons pin **official ty 0.0.86 / ty-extended 0.86.0**. Compatibility is bounded
by this corpus and the embedded-Monty runtime.

Fresh-process public `check` medians on the Django conformance project:

| Output    | Before  | After   | Reference fork | Speedup |
| --------- | ------- | ------- | -------------- | ------- |
| `full`    | 4.336 s | 2.256 s | 0.736 s        | 1.92×   |
| `concise` | 4.137 s | 2.045 s | 0.733 s        | 2.02×   |

These are seven measured runs after two warmups, with rotating backend order.
All 54 invocations preserve accepted output; the two companion builds have
identical stdout, stderr and exit status. Startup remains 2.79–3.07× the fork on
this workload. Full output includes another official checker session to recover
related diagnostic notes while preserving primary diagnostic wording.

Persistent-editor semantic edits improved from **747 ms to 441 ms** (1.69×).
Comment edits remain around **33 ms**. Three alternating before/after session
pairs retain 21 measured edits per kind and backend after warmup, with identical
diagnostic items and report kinds. Measured trailing-comment edits make no Monty
calls or native hint queries.

The internal checker workloads improve by **1.83–2.07×** across CLI and persistent
modes. The negative-e2e case remains 5.45× the fork: 1.726 s versus 0.317 s.
All 162 invocations preserve diagnostics and exit status.

Scaling uses django-ty's existing generator with eight character fields and three
queries per added model, plus constructor/save/load/delete/alias assertions:

| Added models | Before   | After    | Reference fork | Speedup |
| ------------ | -------- | -------- | -------------- | ------- |
| 0            | 3.231 s  | 1.732 s  | 0.784 s        | 1.87×   |
| 25           | 10.254 s | 4.513 s  | 1.846 s        | 2.27×   |
| 100          | 35.870 s | 16.191 s | 9.110 s        | 2.22×   |

All 81 invocations pass with zero diagnostics. Measurements use one macOS arm64
host, fixed parallelism, two warmups and seven measured runs. They do not measure
cold-disk performance or establish a universal speed ceiling.

The macOS arm64 executable is **15,196,128 bytes** (gzip: **6,694,058 bytes**).
With separately installed official ty, executables total **41,532,080 bytes**,
versus **37,518,400 bytes** for the fork. Distribution is independent; total
installed size is larger. The companion includes Ruff parser/SDK crates and Monty,
excluding ty's semantic engine, Salsa, Wasmtime and the Python worker.

Two reference failures remain documented without changing expectations: fork
0.86 rejects inherited ModelForm `Meta`; fork 0.84.4's warm editor hits a Salsa
panic, preventing a valid warm-editor speed ratio.

[Evidence and known failures](evidence/README.md),
[reproduction](tests/DJANGO_ACCEPTANCE.md), and
[Linux/macOS/Windows CI](https://github.com/regularkevvv/ty-extended/pull/41/checks).
