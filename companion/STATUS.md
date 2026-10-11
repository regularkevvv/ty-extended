# Rust companion validation

The companion runs embedded Monty semantic plugins with unchanged official ty.
CLI and LSP map diagnostics, positions and edits back to original files. WASM and
the optional Python worker are outside this build.

Validated source: `09015e694ca7897271961c075fb1be71162d7c82`. Executable SHA-256:
`c4f681887b358944318e01c73db864bf0de76e00ab24d15417e07d9ed2a4f8f2`.

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

Additional checks: 172 default Rust tests, one configuration test, 31 Python harness
tests, Clippy and repository checks. Both 150-request Django protocol replays pass:
8 MiB rejects one oversized response as expected; 16 MiB accepts all.

Django comparisons pin **official ty 0.0.84 / ty-extended 0.84.4**; generic/editor
comparisons pin **official ty 0.0.86 / ty-extended 0.86.0**. Compatibility is bounded
by this corpus and the embedded-Monty runtime.

Fresh-process public `check` medians on the Django conformance project:

| Output    | Companion | Reference fork | Companion / fork |
| --------- | --------- | -------------- | ---------------- |
| `full`    | 4.174 s   | 0.703 s        | 5.93×            |
| `concise` | 3.940 s   | 0.701 s        | 5.62×            |

These are seven measured runs after two warmups; all 36 invocations preserve
accepted output. Persistent-editor medians are **34 ms for comment edits** and
**736 ms for semantic edits**, with seven measurements after three warmups.
The measured trailing-comment edits make no Monty calls or native hint queries.

An alternating comparison against the preceding binary retained all 60
invocations: batching native queries reduced positive/negative e2e medians by
**17.3% / 17.8%**; conformance changed by **+0.05%**. Fresh-process overhead remains:
internal `project` e2e-negative medians are 3.012 s versus 0.298 s for the fork
(10.10×); persistent-checker mode within each invocation takes 2.980 s (9.99×).
These desktop measurements do not establish cold-disk or universally optimal performance.

The macOS arm64 executable is **14,948,064 bytes** (gzip: **6,576,615 bytes**).
With separately installed official ty, executables total **41,284,016 bytes**,
versus **37,518,400 bytes** for the fork. Distribution is independent; total
installed size is larger. The companion includes Ruff parser/SDK crates and Monty,
excluding ty's semantic engine, Salsa, Wasmtime and the Python worker.

Two reference failures remain documented without changing expectations: fork
0.86 rejects inherited ModelForm `Meta`; fork 0.84.4's warm editor hits a Salsa
panic, preventing a valid warm-editor speed ratio.

[Evidence and known failures](evidence/README.md),
[reproduction](tests/DJANGO_ACCEPTANCE.md), and
[Linux/macOS/Windows CI](https://github.com/regularkevvv/ty-extended/pull/41/checks).
