# Rust companion validation

The companion runs embedded Monty semantic plugins with unchanged official ty.
CLI and LSP map diagnostics, positions and edits back to original files. WASM and
the optional Python worker are outside this build.

Validated source: `a4753158c11354d7042ac8d701d162981815369f`. Executable SHA-256:
`54a5c41f3b595b2a053f1d5d676cff72c0a73e03639bb6d38a52aaafa9dedbe0`.

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

Additional checks: 172 default Rust tests, two live-checker tests, 37 Python harness
tests, Clippy and repository checks. Both 150-request Django protocol replays pass:
8 MiB rejects one oversized response as expected; 16 MiB accepts all.
Windows path normalization has a CLI/LSP regression for nominal and generic
classes imported from their own file, including a genuine type-error control.

Django comparisons pin **official ty 0.0.84 / ty-extended 0.84.4**; generic/editor
comparisons pin **official ty 0.0.86 / ty-extended 0.86.0**. Compatibility is bounded
by this corpus and the embedded-Monty runtime.

Fresh-process public `check` medians on the Django conformance project:

| Output    | Companion | Reference fork | Companion / fork |
| --------- | --------- | -------------- | ---------------- |
| `full`    | 4.350 s   | 0.735 s        | 5.92×            |
| `concise` | 4.110 s   | 0.730 s        | 5.63×            |

These are seven measured runs after two warmups; all 36 invocations preserve
accepted output. Persistent-editor medians are **34 ms for comment edits** and
**735 ms for semantic edits**, with seven measurements after three warmups.
The measured trailing-comment edits make no Monty calls or native hint queries.

An alternating comparison against the binary before the final `NoneType` query prefetch retained all 60
invocations: prefetching `NoneType` with the other native queries reduced positive/negative e2e medians by
**17.2% / 17.3%**; conformance changed by **-0.39%**. Fresh-process overhead remains:
internal `project` e2e-negative medians are 3.154 s versus 0.312 s for the fork
(10.11×); persistent-checker mode within each invocation takes 3.118 s (9.99×).
These desktop measurements do not establish cold-disk or universally optimal performance.

The macOS arm64 executable is **14,948,064 bytes** (gzip: **6,576,170 bytes**).
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
