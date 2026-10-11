# Rust companion validation

The companion runs Monty semantic plugins and delegates checking to unchanged
official ty. CLI and LSP map projected locations and edits back to original files.

The macOS arm64 executable is **13,972,992 bytes** (gzip: **6,223,690 bytes**).
Official ty is separate: together they occupy **40,308,944 bytes**, versus
**37,518,400 bytes** for the reference fork. Our artifact is smaller; total installed
executables are larger. This build uses Ruff parser/SDK crates and embedded Monty;
it excludes ty’s semantic engine, Salsa, Wasmtime and the Python worker.

Release artifact: source `a7ac9fc5bb46a92225eb35ae43c7578ded258185`, SHA-256
`59e0634f6219b32190b1efce851775de1ef80f3ca667443a267a2231340cba63`.

| Passed release gates    | Coverage                                                                                                                                                                                                               |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Django compatibility    | All 18 supported Django/Python pairs in both CLI and persistent-checker modes; 166 assertions across 38 features, including 49 model-ID cases; positive/negative e2e; 83 runtime proofs and nine QueryDict comparisons |
| Public CLI              | `full`, `concise`, GitLab, GitHub and JUnit: all 28 primary diagnostics/locations checked; four external note locations verified; positive and negative projects checked                                               |
| Generic plugin behavior | 17 differential cases and four rejection controls in each of three checker modes; eight CLI/SDK integration suites, including multiple plugins, virtual types, fixes and native watch                                  |
| Editor behavior         | Six acceptance sessions, 105 ordinary and 66 notebook comparisons across UTF-8/16/32; multiworkspace/configuration/unsaved-file lifecycle checks                                                                       |
| Django editor           | 28 baseline diagnostics with exact source spans; unsaved negative controls; ten comment and ten semantic revisions, preserving original files                                                                          |

Source checks: 125 Rust unit/configuration checks, 31 Python harness tests, and
150 replayed Django requests. The 8 MiB limit rejects one oversized response as
expected; the explicit 16 MiB limit accepts all.

Django comparisons use **official ty 0.0.84 / ty-extended 0.84.4**. Generic and
editor differentials use **official ty 0.0.86 / ty-extended 0.86.0**. Coverage is limited
to the recorded corpus, not every Python program or third-party plugin.

On the Django conformance project, fresh-process public `check` medians were:

| Output    | Companion | Reference fork | Companion / fork |
| --------- | --------- | -------------- | ---------------- |
| `full`    | 3.262 s   | 0.700 s        | 4.66×            |
| `concise` | 3.045 s   | 0.709 s        | 4.30×            |

Seven measured runs follow two warmups. All 36 invocations preserve accepted
diagnostics and exit status; `full` includes the secondary checker for notes.

Persistent-editor medians: **34 ms comment edits**, **488 ms semantic edits**
(seven measured revisions after three warmups). Comment edits make no Monty calls
or native hint queries.

A matched, interleaved comparison of the pre-fix and final binaries found a
0.04% difference in CLI medians; all 20 invocations preserved accepted results.

Two reference-fork failures remain separate from companion results: 0.86 rejects
the unchanged inherited ModelForm `Meta` case; 0.84.4’s warm Django editor benchmark
hits a Salsa panic after edits. The latter prevents a valid warm-editor speed ratio.
See [recorded failures and reproduction](evidence/README.md).

[Evidence](evidence/README.md) pins binaries, sources and results.
[Reproduction](tests/DJANGO_ACCEPTANCE.md) describes the gates.
Hosted Linux/macOS/Windows results are on [PR #41’s checks](https://github.com/regularkevvv/ty-extended/pull/41/checks).
