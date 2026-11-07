# Companion performance

The companion checks generated analysis copies with unchanged official ty and
runs Django hooks in embedded Monty. Its total time includes discovery, plugin
execution, projection, type queries and the final checker run.

## Version 0.89.0

The [0.89.0 local receipts](qualification/performance-0.89.0.json) record five
rotated serial runs on macOS arm64 with the final-version companion executable,
Django-ty 0.7.0, official ty 0.0.99, MyPy 2.4.1, django-stubs 6.1.2 and Django 6.1.3.
All five workloads meet the MyPy median ceiling, preserve source bytes and pass
exact ORM assertions plus both intentional-error controls.

| Python files / models / model modules | Companion | MyPy + django-stubs | Bare ty |
| ------------------------------------- | --------: | ------------------: | ------: |
| 100 / 25 / 5                          |   0.514 s |             1.567 s | 0.062 s |
| 1,000 / 250 / 25                      |   1.219 s |             2.122 s | 0.086 s |
| 7,082 / 250 / 25                      |   3.394 s |             4.048 s | 0.164 s |
| 7,082 / 2,501 / 1                     |   6.366 s |             6.542 s | 0.257 s |
| 7,082 / 2,501 / 250                   |   5.878 s |             6.412 s | 0.195 s |

The largest single-module case improves from the published 0.87/Django-ty 0.6
pair's 80.788 seconds to 6.366 seconds, approximately 12.7 times faster. Its margin
against MyPy is only 2.7%; this is close to parity, not a large speed advantage.
Smaller projects show larger gains. These measurements identify a local release
candidate binary; hosted CI and public-package acceptance are separate checks.
The installed macOS CI wheel separately passes the largest workload in five
rotated runs: **6.282 seconds versus MyPy's 6.577 seconds** (bare ty: 0.259 seconds),
with all typing and source-preservation controls passing. Its exact wheel and
executable hashes are recorded in the same receipt.

The same receipt preserves a follow-up with **published 0.88.0**: 6.593 seconds
versus MyPy's 6.487 seconds. Correctness passed, but that run **failed** the strict
performance ceiling. That failure prompted the class batching in 0.89; it is not
replaced by an earlier passing CI-wheel measurement.

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
