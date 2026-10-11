# Rust companion compatibility result

Branch: `experiment/native-companion-django`.

The Rust companion runs the unchanged django-ty Monty plugin with a separate,
unchanged official ty executable. All recorded local gates pass on macOS arm64.
This verifies the corpus below; full Python, SDK and editor compatibility remains
unfinished. This experiment has not been released or validated in hosted CI.

## Verified behavior

| Gate | Result |
| --- | --- |
| Django version matrix | 18/18 pairs in CLI and push modes: 36 complete runs |
| Original Django contract | 166 assertions across 38 features, including 49 model-ID cases, in every run |
| Original E2E | Positive project and all six negative controls pass; exact primary messages, severities, original paths/spans and exit statuses match |
| Django runtime | 83 proofs and nine QueryDict runtime/checker controls per pair/mode |
| Rust unit/integration | 53 passed, including real official-checker tests |
| Additional differentials | Django model identity, aliases, Self, async/await, loop/else and exception flow; virtual descriptors and inherited generics; CLI and push |
| Generic regression | 17 differential cases and four explicit rejection controls in each of CLI, pull-LSP and push; verified scaling through 1,400 assertions |
| Hook replay | All 150 responses plus manifest match at an explicit 16 MiB limit; default 8 MiB matches 149 and correctly rejects one oversized response |
| Harness and quality | 19 Python tests; strict Clippy, rustfmt and Python lint pass |

Django uses **official ty 0.0.84 / reference ty-extended 0.84.4**, matching the
plugin's declared support. The unchanged Monty checkout is
`9996e1b3e8a0656c988fd2b8deedb726b12fe575`. The separate generic suite uses
**official 0.0.86 / reference 0.86.0+147**. The newer reference fork itself fails
one unchanged inherited `ModelForm.Meta` assertion; Django parity on that version
is an open upgrade gate, retained in [known-upgrade-failure.json](evidence/known-upgrade-failure.json).

The branch contains [portable evidence](evidence/README.md), source and binary
hashes, complete normalized diagnostics and the compressed hook corpus.
[Reproduction instructions](tests/DJANGO_ACCEPTANCE.md) specify the gates.

## Size and performance

The companion executable is **11,603,904 bytes (11.1 MiB)**; deterministic gzip is
**5,142,898 bytes (4.9 MiB)**. Official ty is installed separately and excluded
from those numbers. No forked semantic engine, Salsa or Wasmtime is linked.
Building still uses parser and plugin SDK crates from the pinned Ruff submodule.
The preserved binary SHA256 is
`10009752a3051df7a08023eaafb41484c127f89e6533297f0cd35b9e8b7aac02`.

Django 6.1.1 / Python 3.13, seven measured runs after two warmups:

| Workload | Fork | Companion CLI | Companion push |
| --- | ---: | ---: | ---: |
| Conformance | 0.739 s | 2.008 s | 2.029 s |
| E2E positive | 0.728 s | 1.513 s | 1.517 s |
| E2E negative | 0.312 s | 1.479 s | 1.508 s |

All 90 first-observed, warmup and measured invocations reproduce accepted
messages, positions, exits and work counters. Native was **2.08–4.83× slower**.
Processes start fresh; push reuses ty only within one invocation. These are
local desktop observations: one-minute load ranged 6.18–8.42 on 10 logical CPUs.
Backend order rotates by workload but repeats for that workload across runs.
They are not isolated-machine or cold-cache measurements. See the
[records](evidence/benchmark.json) and [method note](evidence/benchmark-method-note.json).

## Remaining work

- Merge existing `ty.toml`/`pyproject.toml` rules, exclusions, search paths and
  platform configuration. Today the companion uses explicit flags and its
  plugin target context is `darwin`; other platforms are unverified.
- Plugin discovery, multiple-plugin composition and an editor-facing LSP proxy.
- `match` is rejected. Class-body/nested-class state flow and comprehension or
  generator binding/control flow remain incomplete or unverified.
- Direct class-member and additional-dependency hooks, result metadata, and
  attribute/delete/augmented mutation operations. Plain item assignment is
  implemented; dynamic member descriptor responses are unsupported.
- Broader SDK programs and oracle-aware signature hooks. Project inference stops
  after 12 rounds; the older single-file interface stops after eight.

Virtual descriptors match the fork's current behavior: reads combine class and
instance types; writes use the class type when present; setter/read-only metadata
is ignored. The differential tests preserve this behavior rather than imposing
stronger semantics. Diagnostic mapping and probe calibration reject detected
mapping/probe failures in checked files. A fresh `--out` directory is required.

## Cleanup

Removed all 4,211 selected paths: compiler output, temporary environments, caches
and generated analysis copies. Their allocated size was **10,008,932,352 bytes
(9.32 GiB)**. All 16 Git worktrees were unchanged by cleanup, and the final
executable, sources and evidence remain.

Measured disk free-space gain during cleanup was **417,247,232 bytes (398 MiB)**,
not 9.32 GiB. Other builds were active; free space fell by another 1.39 GB after
cleanup ended. APFS reports no data-volume snapshots. Allocated file size and
shared-filesystem free-space change are separate measurements. See
[cleanup.json](evidence/cleanup.json).
