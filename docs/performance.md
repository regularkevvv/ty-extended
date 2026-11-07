# Companion performance

The companion checks generated analysis copies with unchanged official ty and
runs Django hooks in embedded Monty. Its total time includes discovery, plugin
execution, projection, type queries and the final checker run.

## Measured results

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
disposable cache; operating-system filesystem caches may be warm. Three serial
rounds rotate command order. Reports retain all samples, commands, package
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

Protocol 0.8 lets plugins supply the data each class needs and request individual
entries from their own retained project index. Django-ty 0.7 uses both interfaces.
The host preserves fresh interpreter state, bounded execution and read-only
access to the selected index.

The companion shares generated descriptors and names, packs virtual declarations
into a bounded set of modules, and materializes required declarations. It reuses
projections whose type evidence remains valid, skips unchanged source copies and
batches final CLI checks for large projects. File-selection, error-mapping,
notebook and editor regression tests cover the affected paths.
