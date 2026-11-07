# Current compatibility qualification

Companion 0.89.0 adds opt-in bounded class batching and avoids rescanning all
diagnostics for every file. Its [performance receipt](../docs/qualification/performance-0.89.0.json)
records five local workloads below the MyPy median ceiling. The corrected local
candidate checks the largest project in 6.551 seconds versus MyPy's 6.685 seconds,
with exact ORM assertions, both negative controls and source preservation. The
largest case remains close to parity. Earlier candidate results and the failed
public 0.88 follow-up remain available in the same receipt.

The final-version suite passes **208 Rust unit tests** on Linux/macOS and
**211 on Windows**, **50 Python harness tests**, and **83 original SDK/host tests**
with embedded Monty 1.5.0. The SDK suite retains
one explicitly excluded WASM doctest. Django's independent native baseline
matches 152 responses in CPython and standalone Monty; embedded replay also checks
the unchanged production response limit and the exact packaged manifest.
Across 27 project indexes, **2,582 scoped class results and 3,012 batches** match
individual full-index results, including varied batch boundaries and order.

Generated disk projections receive distinct filesystem revisions when timestamps
collide. The [regression receipt](evidence/projection-revisions-2026-11-07.json)
forces equal file timestamps and equal directory timestamps during import
deletion and restoration, including nested packages. These checks retain the
checker process and preserve original external source bytes and modification times.
Fresh checkers record the completed projection without sending initial file-change
events. Revision tracking also covers restored underlying files when a projection
is removed and later reinstated.
On Windows, generated-workspace boundaries compare ordinary and canonical
verbatim drive or network-share prefixes without rewriting filesystem paths.
The lifecycle regression uses those mixed spellings for both checker constructors.
The final published binary receives separate compatibility and performance checks.

Django-ty 0.7 passes its 166-assertion and 83-runtime-contract CLI corpus across
all 16 official ty releases from 0.0.84 through 0.0.99 with the installed 0.89 CI
wheel. Additional qualification covers all 21 Django/Python pairs; its earlier
companion identity remains in the ledger and the matrix is repeated against the
public companion before the Django release. The current CI wheel also passes
Python 3.15's contracts, nine QueryDict cases and an installed editor session. See the
[compatibility ledger](../docs/compatibility.md) for executable identities,
the generic version sweep, editor checks, hosted CI and public-package acceptance.
Those checks are recorded separately from local candidate measurements.

## Previous 0.88.0 qualification

Companion 0.88.0's [2026-11-07 ledger](evidence/compatibility-2026-11-07.json)
records **288 passing gates** with one installed CI wheel across official ty
0.0.84–0.0.99. Django-ty 0.7.0 passes its full 166-assertion and 83-runtime-contract
CLI corpus on all 16 versions, plus 16 complete editor sessions with unsaved
semantic changes and exact diagnostic spans. Python 3.8, 3.9 and 3.13 installed
SDK/CLI/LSP checks pass. Cross-platform companion CI passes 43 Linux, 31 macOS
and 31 Windows gates.

The refactor retains cast metadata probes, descriptor diagnostic origins and
cached model projections after editor type-query preflights. The regression suite
passes 205 Rust unit tests, including the three live-checker units, and 50 Python
harness tests. The [embedded Monty 1.5.0 protocol/SDK suite](evidence/embedded-runtime-2026-11-07.json) passes 82 tests and
retains its explicit WASM documentation exclusion. Django's independent native
oracle matches 152 responses through both CPython and standalone Monty 1.5.0.
Class-specific indexes also preserve the full-index results for 2,582 classes
across 27 projects through CPython and embedded Monty.

The [performance receipts](evidence/performance-2026-11-07.json) compare public
commands on identical files with exact ORM types and negative controls. On the
largest Mac workload, published 0.87/Django-ty 0.6 takes 80.788 seconds; the
0.88/Django-ty 0.7 CI wheel takes 6.963 seconds, versus MyPy's 7.035 seconds.
This is 11.6× faster than the previous pair and effectively at MyPy parity.
The four initial workloads meet the MyPy median ceiling on both macOS and Linux CI.
An additional Mac workload spreads all 2,501 models across 250 modules and passes
at 6.315 seconds versus MyPy's 6.966 seconds. CI now covers all five shapes.
See [methodology and limits](../docs/performance.md). Published-artifact acceptance
is a separate release check; these measurements identify the tested CI binary.

## Previous 0.87.0 qualification

The [2026-11-06 ledger](evidence/compatibility-2026-11-06.json) records the same
companion's 288 generic gates across official ty 0.0.84–0.0.99, 32 complete Django
checker-version runs, and 42 Django/Python matrix runs including Django 6.1.3.
The measured wheel embeds Monty **1.5.0**. The current regression suite passes 187 Rust unit tests, three live-checker
units, 18 integration tests and 42 Python harness tests.
[Final-wheel checks](evidence/final-wheel-checks-2026-11-06.json) also record
installed SDK/CLI/editor smoke tests on Python 3.8, 3.9 and 3.13.
Clean installed-wheel and source-archive rebuilds pass SDK, CLI and editor smoke
checks with official ty 0.0.99 installed separately. The unchanged SDK/runtime
suite passes 75 tests and three documentation examples with the selected Monty
version; its build directory is cleaned after the gate. The complete Django editor
corpus also passes on ty 0.0.99 with Django 6.1.3, including unsaved edits and exact source spans.

These are local receipts. Cross-platform CI and installed published artifacts
are separate release acceptance checks, linked from the release. The full
installed-wheel version sweep passes; the ledger identifies its wheel and
executable hashes. Packaging and performance changes receive separate checks.

The [directory-revision followup](evidence/overlay-directory-revisions-2026-11-06.json)
also checks the actual CI wheel across all 16 checker versions. Independent
macOS and Windows probes force a delete/recreate to share a directory timestamp:
the legacy reference retains a stale listing, while the companion restores the
partial stub overlay with both ty endpoints. The comparison fixture now gives
structural edits distinct directory revisions, retains bounded diagnostic pulls,
and verifies Windows definition URIs against the exact physical source. Diagnostic
and negative-control assertions remain unchanged. The collision probe runs in CI.

The [installed-wheel scalability regression](evidence/scalability-2026-11-06.json)
checks 7,082 Python files and 2,501 related Django models with positive and negative
typing controls. Both pass in approximately 58 seconds, with sampled combined
companion/checker RSS below 2.8 GiB. Each run preserves source bytes and enforces
180-second/3-GiB test limits. These measurements qualify this generated workload;
they are not a global memory limit or proof about an unavailable private project.

Repeated descriptor shapes now share method bodies while retaining distinct
field subclasses and class-access identities. Diagnostic names are shared for
the immutable revision, and diagnostics are partitioned by source path once,
removing repeated per-file scans and name-map copies.

## Earlier optimization evidence

The following measurements describe the earlier source and binary recorded below.

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
