# Companion performance

The companion checks generated analysis copies with unchanged official ty and
runs Django hooks in embedded Monty. Its total time includes discovery, plugin
execution, projection, type queries and the final checker run.

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
and an ordinary Python assignment. Source bytes must remain unchanged. A failed
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
