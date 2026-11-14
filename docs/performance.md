# ty-extended performance

ty-extended checks analysis copies with unchanged official Ty. Total time includes discovery, plugin execution, projection, type queries and the final checker run.

## Current native comparison

The [upstream Ruff migration receipt](qualification/upstream-ruff-migration.json) records ten rotated fresh-process runs per checker across five workload shapes on macOS arm64. Both Ty stacks use official **Ty 0.0.104** and Django **6.1.3**. Native Ty uses **django-types 0.24.0**; MyPy **2.4.1** uses **django-stubs 6.1.2**. The Django plugin source is pinned by commit in the receipt.

| Files / models / model modules | ty-extended mean | Native Ty mean | MyPy mean | Ratio of means |
| ------------------------------ | ---------------: | -------------: | --------: | -------------: |
| 100 / 25 / 5                   |          0.105 s |        0.039 s |   1.519 s |          2.72x |
| 1,000 / 250 / 25               |          0.193 s |        0.065 s |   2.160 s |          2.97x |
| 7,082 / 250 / 25               |          0.581 s |        0.207 s |   4.416 s |          2.81x |
| 7,082 / 2,501 / 1              |          0.747 s |        0.340 s |   7.474 s |          2.20x |
| 7,082 / 2,501 / 250            |          0.789 s |        0.226 s |   7.485 s |          3.49x |

All five workloads pass the current mean and median gates. The distributed-stress median is **0.735 seconds**, versus **0.219 seconds** for native Ty. Its largest recorded sample is **1.090 seconds** and its worst paired ratio is **4.90x**. These results establish neither a one-second worst-case bound nor a three-times bound on every run.

Ten alternating comparisons per workload against the accepted binary, using identical Ty and Django plugin inputs, show no material regression: candidate/baseline mean ratios range from 0.949 to 1.006. All timings, including slow samples, remain in the receipt. The dedicated [30-repeat hosted series](qualification/performance-ci-30-0.90.0.json) passes all five workloads on Linux and macOS. The largest ratios of means are 3.38x and 3.09x respectively; individual slow observations remain recorded. A preceding 10-repeat CI series failed the ratio gate on two runners; [both complete failed series](qualification/performance-ci-repeat-0.90.0.json) are retained. The ceiling remains 3.5x. Public-package checks remain separate from source/CI measurements.

A separate resource fixture with 7,082 files and a standalone module containing 2,501 chained models takes 2.11 seconds versus native Ty's 0.43 seconds in one default-thread comparison, and 2.82 versus 0.60 seconds with one thread. Its import preflight prevents reuse of a speculative checker result, triggering the conservative second-check path. Earlier process-monitored runs took 12.66 and 12.72 seconds; the later phase trace does not explain that difference. This fixture passes correctness and memory bounds but exceeds the timing target. The five-workload gate does not establish that every project is subsecond or within 3.5x.

## Correctness

The generated workloads check exact ORM result types, then introduce an ordinary Python error and an ORM assignment error at known source locations. A separate 20-case extension exercises behavior beyond the timed assertions: ty-extended matches **20/20**, MyPy **19/20**, and native Ty **8/20**. This is a bounded corpus, not a score for every Django API.

Every comparison preserves source and provider bytes and modification times. Failed processes, malformed output, stray diagnostics, timeouts and memory-limit violations invalidate the run. Fresh processes are used throughout, with an empty MyPy cache each time; operating-system filesystem caches may be warm. Commands, versions, source and executable hashes, process-tree memory and host-load observations are retained.

## Accepted baseline and policy

The maintainer accepted the [30-run baseline](qualification/native-django-accepted-baseline-v62.json), averaging **0.776 seconds**, against native Ty's **0.237 seconds** and MyPy's **7.451 seconds** on distributed stress. Its ratios are 3.28x by means and 3.40x by medians.

The current gate requires **both mean and median** to be at most **3.5x** native Ty and no slower than MyPy. Every paired observation is also reported. Earlier receipts that failed the strict three-times/every-pair policy retain that result; the explicit baseline acceptance superseded that policy.

## Why the earlier run took over a minute

The original path repeatedly copied and serialized a whole-project index for each model. Scoped indexes and bounded class batches removed that repeated work. Native Django field inference, retained provider declarations, shared snapshots, lazy row construction, fewer checker probes and moving owned JSON values reduced the remaining work.

Rust alone does not remove duplicate analysis or subprocess startup. Official Ty still runs, while ty-extended also discovers plugins, indexes source and applies the additional Django behavior. Persistent editors reuse valid state between edits; these measurements cover fresh CLI processes.

## Reproduce

Build the candidate, then run the pinned five-shape comparison:

```shell
python3 companion/scripts/native_django_benchmark.py \
  --companion /path/to/ty-extended --production-defaults --matrix \
  --repeats 10 --output /tmp/new-native-comparison
```

The harness installs separate candidate, native and MyPy environments and pins the Django plugin source. Native Ty includes django-types. For an independently installed plugin wheel:

```shell
python3 companion/scripts/compare_checkers.py \
  --companion /path/to/candidate/bin/ty-extended \
  --official /path/to/candidate/bin/ty \
  --official-python /path/to/native/bin/python \
  --python /path/to/candidate/bin/python \
  --mypy /path/to/reference/bin/mypy \
  --plugin /path/to/candidate/lib/python3.13/site-packages/django_ty \
  --files 7082 --models 2501 --model-modules 250 --queries 100 \
  --repeats 10 --production-defaults --max-ty-ratio 3.5 --aggregate-ratios \
  --output /tmp/new-comparison
```

The native environment needs Django and django-types, with the same official Ty executable used by both Ty stacks. The reference environment needs Django, MyPy and django-stubs. Process monitoring requires POSIX. The hosted `ty-extended` workflow runs a required, dedicated 30-repeat matrix on Linux and macOS, cleans Cargo before timing and removes temporary environments afterward. Compatibility jobs retain the semantic checks; they do not duplicate performance timing after a full integration build. Earlier 10-repeat failures remain recorded alongside the subsequent series.

## Earlier measurements

Historical receipts remain available, including the [native Django comparison](qualification/official-django-comparison.json), [owned JSON optimization](qualification/native-django-owned-json-v62.json), [0.89 performance ledger](qualification/performance-0.89.0.json), and [published 0.89 acceptance](qualification/ty-extended-0.89.0.json). Failed prototype gates remain marked failed in their receipts.

The published 0.89 stack with Django-ty 0.7 took **6.594 seconds** on the largest single-module case, versus MyPy's **6.806 seconds**. Earlier bare-Ty comparisons omitted django-types and therefore do not measure native Django support. Absolute results from separate machines or sessions are not controlled comparisons between implementations.
