# Companion performance

The companion checks generated analysis copies with unchanged official ty and
runs Django hooks in embedded Monty. Its total time includes discovery, plugin
execution, projection, type queries and the final checker run.

## Unreleased sparse projection candidate

The [local candidate receipt](qualification/performance-sparse-projection.json)
records three rotated fresh-process runs on the same 7,082-file, 2,501-model,
100-assertion project. All positive companion and MyPy runs pass every assertion.
Both candidate variants and MyPy also detect the two intentionally introduced
errors. All runs preserve source bytes and modification times. Filesystem caches
may be warm; MyPy receives an empty cache for every run.

| Stack                                                         |  Median |
| ------------------------------------------------------------- | ------: |
| Candidate companion and candidate Django manager declarations | 5.029 s |
| Candidate companion and published Django-ty 0.7.0             | 5.298 s |
| Published companion 0.89.0 and Django-ty 0.7.0                | 6.375 s |
| MyPy 2.4.1 and django-stubs 6.1.2                             | 6.794 s |
| Bare official ty 0.0.99                                       | 0.257 s |

Bare ty reports all 100 Django assertions as failures and 100 unresolved
attributes, so it measures the native startup/checking baseline with different
semantic coverage. The candidate is about 21% faster than the published stack
and 26% faster than MyPy in this comparison. **The one-second fresh-process
target is not met.** These are local development results, not hosted CI or
published-artifact acceptance.

The receipt also retains a separate run of all five existing workload shapes
in both projection modes. All ten combinations pass the positive assertions
and negative controls. On the 100-file shape, sparse projection measures
0.509 seconds versus MyPy's 1.640 seconds; on the 7,082-file, 250-model shape,
it measures 2.474 seconds versus 4.147 seconds. Host load rose from about 9 to
nearly 90 during the last shapes. Five-sample follow-ups for the affected cases
are recorded separately below. Their one-minute host load ranges from 7.5 to
19.2; these runs are less contended, but the machine still has other work. No
samples are discarded.

| Models / model modules | Projection | Companion | MyPy + django-stubs | Bare ty |
| ---------------------- | ---------- | --------: | ------------------: | ------: |
| 2,501 / 1              | Memory     |   5.488 s |             6.826 s | 0.269 s |
| 2,501 / 250            | Disk       |   5.823 s |             6.425 s | 0.205 s |
| 2,501 / 250            | Memory     |   4.835 s |             6.664 s | 0.202 s |

Each follow-up checks 7,082 files, preserves the positive assertions and catches
both intentional errors. The single-module candidate's median varies from
5.03 to 5.49 seconds across the recorded series; the faster number is not a
worst-case guarantee. A native-only experiment splitting descriptor declarations
across 8, 32 or 128 modules preserved diagnostics but showed no speed improvement,
so that change was rejected.

The candidate keeps original files in place and opens only changed analysis
buffers through official ty's public LSP. It reuses that process for the final
workspace diagnostic request, combines import/schema/type-query preflights,
and shares repeated descriptor specializations
while preserving each field's nominal identity. The Django candidate uses
`Manager[Model]` for ordinary managers and retains generated manager classes for
custom QuerySet methods. Imported model identities stay fully qualified instead
of being recovered from abbreviated native display strings.

Sparse projection currently requires `TY_COMPANION_MEMORY_PROJECTION=1` for a
fresh CLI check. Watch, source edits, editor sessions and Windows retain the
existing transport. Projects with per-file rule overrides and notebook projects
use disk projection; explicit notebooks outside the project are unsupported in
this experimental mode. This flag is a development experiment, not a released
performance guarantee.

The remaining time spans discovery/indexing, Monty index and class hooks,
projection, native type queries, and final checking. The final native check
alone takes about 1.1 seconds on this expanded program. The earlier 60–80 second
behavior came from repeatedly copying and serializing the whole project index
for each model; scoped indexes and class batches removed that repeated work in
0.88 and 0.89. Further progress requires reducing the remaining work across
several stages, not merely changing implementation language.

### Index and cache follow-up

The [follow-up receipt](qualification/performance-cache-admission.json) records
the same 7,082-file, 2,501-model workload after combining initial summary builds,
buffering request hashing, and limiting individual cache entries. Three rotated
fresh-process runs measure 5.081 seconds for the candidate, 5.227 seconds for the
preceding candidate, 6.635 seconds for the published stack, 7.795 seconds for MyPy
and 0.282 seconds for bare ty. All 100 ORM assertions, both error controls and
source byte/mtime checks pass. These measurements remain well above one second.

The cache change addresses a separate editor bottleneck. A project-index response
occupies approximately 35 MB under the cache's accounting, while reusable class
and call responses occupy another 34 MB. Repeated insertion into a 64 MiB shared
cache evicted class results during sequential scans. Individual cached responses
are now limited to 16 MiB; larger responses are still consumed normally. Ordinary
edits consequently execute one Monty request instead of 521.

| Full-workspace edit |  Before |   After |
| ------------------- | ------: | ------: |
| Trailing comment    | 0.519 s | 0.559 s |
| Ordinary assignment | 2.746 s | 2.263 s |
| ORM assignment      | 2.910 s | 2.319 s |
| Model field schema  | 6.889 s | 6.647 s |

These are separate retained-process editor series, not fresh CLI timings or
worst-case guarantees. Both series preserve the checker process, detect ordinary
and ORM errors, propagate unsaved model-field changes to a closed file, and
preserve original bytes and modification times. Schema edits still invalidate
the project-wide model index.

A separate [class-field audit](qualification/field-class-access-audit.json)
found six compatibility differences already present in the published companion.
They remain open. Passing the existing benchmark and Django corpus does not
qualify those additional cases, and no new release acceptance is claimed.

## Version 0.89.0

The [0.89.0 local receipts](qualification/performance-0.89.0.json) record five
rotated serial runs on macOS arm64 with the release candidate, Django-ty 0.7.0,
official ty 0.0.99, MyPy 2.4.1, django-stubs 6.1.2 and Django 6.1.3. This candidate
includes the filesystem revision and Windows path corrections. All five workloads
meet the MyPy median ceiling, preserve source bytes and pass exact ORM assertions
plus both intentional-error controls.

| Python files / models / model modules | Companion | MyPy + django-stubs | Bare ty |
| ------------------------------------- | --------: | ------------------: | ------: |
| 100 / 25 / 5                          |   0.535 s |             1.600 s | 0.064 s |
| 1,000 / 250 / 25                      |   1.252 s |             2.172 s | 0.084 s |
| 7,082 / 250 / 25                      |   3.577 s |             4.166 s | 0.165 s |
| 7,082 / 2,501 / 1                     |   6.551 s |             6.685 s | 0.270 s |
| 7,082 / 2,501 / 250                   |   6.065 s |             6.545 s | 0.195 s |

The largest single-module case improves from the published 0.87/Django-ty 0.6
pair's 80.788 seconds to 6.551 seconds, approximately 12.3 times faster. Its margin
against MyPy is only 2.0%; this is close to parity, not a large speed advantage.
Smaller projects show larger gains. These measurements identify a local release
candidate binary; hosted CI and public-package acceptance are separate checks.
The receipt links the earlier candidate measurements and retains the public
0.88.0 failure below, so results from different executables remain distinguishable.

The installed CI wheel separately passes the largest case in five rotated runs:
**6.424 seconds versus MyPy's 6.691 seconds** (bare ty:
0.271 seconds). All five Linux CI workloads also meet the MyPy median
ceiling. Their complete samples and executable identities are in the same receipt.

The same receipt preserves a follow-up with **published 0.88.0**: 6.593 seconds
versus MyPy's 6.487 seconds. Correctness passed, but that run **failed** the strict
performance ceiling. That failure prompted the class batching in 0.89; it is not
replaced by an earlier passing CI-wheel measurement.

The [published 0.89.0 acceptance](qualification/ty-extended-0.89.0.json)
repeats all five workloads with the downloaded PyPI executable and the same Django
0.7 candidate wheel. Every workload passes correctness and the MyPy median ceiling.
The largest case takes **6.594 seconds** versus **6.806 seconds**
for MyPy and **0.271 seconds** for bare ty. The receipt preserves all five
rotated samples per workload, source hashes, error controls and memory measurements.

## Previous 0.88.0 measurements

The [2026-11-07 receipts](https://github.com/regularkevvv/ty-extended/blob/main/companion/evidence/performance-2026-11-07.json)
identify the CI-built 0.88.0 wheel, Django-ty 0.7.0, official ty 0.0.99,
MyPy 2.4.1, django-stubs 6.1.2 and Django 6.1.3. Each value is the median
of three serial fresh-process runs; all positive and negative typing controls pass.

| Host            | Python files / models / model modules | Companion | MyPy + django-stubs | Bare ty |
| --------------- | ------------------------------------- | --------: | ------------------: | ------: |
| macOS arm64     | 100 / 25 / 5                          |   0.547 s |             1.690 s | 0.064 s |
| macOS arm64     | 1,000 / 250 / 25                      |   1.299 s |             2.221 s | 0.089 s |
| macOS arm64     | 7,082 / 250 / 25                      |   3.597 s |             4.368 s | 0.166 s |
| macOS arm64     | 7,082 / 2,501 / 1                     |   6.963 s |             7.035 s | 0.281 s |
| macOS arm64     | 7,082 / 2,501 / 250                   |   6.315 s |             6.966 s | 0.200 s |
| Linux x86_64 CI | 100 / 25 / 5                          |   0.300 s |             1.164 s | 0.046 s |
| Linux x86_64 CI | 1,000 / 250 / 25                      |   0.732 s |             1.688 s | 0.061 s |
| Linux x86_64 CI | 7,082 / 250 / 25                      |   1.695 s |             3.343 s | 0.105 s |
| Linux x86_64 CI | 7,082 / 2,501 / 1                     |   4.299 s |             5.888 s | 0.171 s |

The published 0.87.0 companion with Django-ty 0.6.0 takes **80.788 seconds**
on the exact same largest project on the Mac. Version 0.88.0 with Django-ty 0.7.0
takes **6.963 seconds**, an **11.6× improvement**. Maximum sampled process-tree
RSS across the three positive runs falls from 2,871 MiB to 1,358 MiB. Both stacks
preserve source bytes and catch the two deliberately introduced errors.

The companion meets the median-at-most-MyPy gate on all nine recorded host/workload
combinations. The largest Mac workload is effectively at parity with MyPy;
its approximately one-percent margin is too small to claim a meaningful speed
advantage. The other Mac workloads are 1.2–3.1× faster, and Linux CI workloads
are 1.4–3.9× faster. These are workload-specific observations, not a universal
speed guarantee. Bare ty does not provide the Django checking used here.

The additional 250-module project checks the same large model population spread
across files. It preserves all 100 ORM type assertions and both negative controls.
CI enforces the MyPy median ceiling on all five project shapes.

## What is compared

`companion/scripts/compare_checkers.py` creates a registered Django application
and runs three public commands over identical Python files:

- `ty-extended check`, with Django-ty's Monty plugin and packaged stubs;
- MyPy with django-stubs and its Django plugin;
- bare official `ty check`, with no companion or Django plugin.

Every sample starts fresh processes. MyPy uses `--no-incremental` and an empty,
disposable cache; operating-system filesystem caches may be warm. Serial rounds rotate command order (five for the 0.89 local suite; three for
the earlier measurements and CI). Reports retain all samples, commands, package
versions, executable and source hashes, CPU time, sampled process-tree memory
and system load. Temporary analysis and cache directories are removed after each
sample.

The generated projects assert exact ORM result types. Both plugin-enabled
checkers must accept them, then reject an intentionally wrong ORM assignment
and an ordinary Python assignment at their exact source locations. Source bytes
must remain unchanged. A failed
process, malformed output, timeout or memory-limit violation invalidates the
run. The performance gate separately requires the companion's median to be no
slower than MyPy's median on that workload.

Bare ty reports unresolved Django model managers and failed type assertions.
Its timings show the cost of the underlying checker, but do not establish
equivalent Django checking. These fresh-process measurements also do not compare
MyPy's persistent daemon or either checker's incremental editor performance.

## Reproduce

Use separate environments for the two plugin implementations, with the same
Python and Django versions. Install `mypy` and `django-stubs` in the reference
environment, and `ty-extended` and `django-ty` in the companion environment.

```shell
python3 companion/scripts/compare_checkers.py \
  --companion /path/to/candidate/bin/ty-extended \
  --official /path/to/candidate/bin/ty \
  --python /path/to/candidate/bin/python \
  --mypy /path/to/reference/bin/mypy \
  --plugin /path/to/candidate/lib/python3.13/site-packages/django_ty \
  --files 7082 --models 2501 --queries 100 --repeats 3 \
  --output /tmp/new-comparison
```

The harness requires a POSIX system with process monitoring. `--model-modules`
spreads models across modules and exercises cross-module relations. Smaller
workloads and many ordinary files with fewer models separate startup, project
discovery and plugin costs. A median on these generated projects is evidence
for those projects, not an upper bound for arbitrary applications or hardware.

## Implementation

Protocol 0.9 adds explicitly opted-in class batches bounded to 128 items and
1 MiB of requests. Each result is cached under the unchanged individual request
key. Failed, unavailable or invalid batches fall back to individual calls.
Fresh interpreter state is preserved between invocations; plugins must preserve
independent results within a batch.

Protocol 0.8 lets plugins supply the data each class needs and request individual
entries from their own retained project index. Django-ty 0.7 uses both interfaces.
The host preserves fresh interpreter state, bounded execution and read-only
access to the selected index.

The companion shares generated descriptors and names, packs virtual declarations
into a bounded set of modules, and materializes required declarations. It reuses
projections whose type evidence remains valid, skips unchanged source copies and
batches final CLI checks for large projects. File-selection, error-mapping,
notebook and editor regression tests cover the affected paths.
