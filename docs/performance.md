# ty-extended performance

ty-extended checks analysis copies with unchanged official Ty. Total time includes discovery, plugin execution, projection, type queries and the final checker run.

## Current native comparison

The [final qualification receipt](qualification/final-0.90.0.json) records ten rotated fresh-process runs per checker across five workloads on macOS arm64, with **Monty 1.6.0**, official **Ty 0.0.104**, Django **6.1.3**, Django-ty 0.8, native **django-types 0.24.0**, MyPy **2.4.1** and **django-stubs 6.1.2**.

| Workload           | ty-extended mean | Native Ty mean | MyPy mean | Ratio of means |
| ------------------ | ---------------: | -------------: | --------: | -------------: |
| small              |           0.115s |         0.039s |    1.419s |         2.963x |
| distributed        |           0.200s |         0.065s |    2.010s |         3.092x |
| many-files         |           0.486s |         0.169s |    4.128s |         2.882x |
| stress             |           0.711s |         0.313s |    6.830s |         2.270x |
| distributed-stress |           0.717s |         0.211s |    6.786s |         3.401x |

The local distributed-stress timing gate **fails**: **3.401x by means** and **3.568x by medians**, at **0.717 seconds** on average. Every correctness, negative and source-preservation control passes. The computed timing result remains unchanged; the controlled comparison and required hosted series below answer separate questions.

A separate controlled comparison rotates the preceding and final Monty 1.6 executables and native Ty over the identical distributed-stress project for **30 runs each**. Means are **0.693, 0.688 and 0.212 seconds**; medians are **0.690, 0.676 and 0.203 seconds**. The final/preceding ratio of means is **0.993**. Every observation is retained.

The final [30-repeat hosted qualification](qualification/performance-ci-monty16-0.90.0.json) passes all five workloads on **Linux and macOS**. Linux's largest ratios are **3.443x by means** and **3.446x by medians**. Macos's largest ratios are **2.925x by means** and **2.959x by medians**. These are workload qualifications, not bounds for every individual run or project.

The [preceding Monty 1.6 hosted series](qualification/performance-ci-monty16-before-0.90.0.json) failed the Linux distributed-stress gate at **3.539x by means and 3.587x by medians**. Its local series also failed at **3.561x and 3.597x**. Those failures are retained. The final source removes unused explicit-file selection preparation and a duplicate project attachment. Earlier [discarded-check results](qualification/native-check-cancellation-0.90.0.json), [failed CI series](qualification/performance-ci-repeat-0.90.0.json) and [passing preceding candidate](qualification/performance-ci-cancel-0.90.0.json) remain historical evidence.

A separate resource fixture uses 7,082 files and a standalone module with 2,501 chained models. Its one-thread run originally took about **12.7 seconds**. Nearly11 seconds were spent waiting for a speculative native check before rejecting its result; the fix cancels and reaps known-invalid work promptly. The final executable averages **2.298 seconds** with one thread, versus native Ty's **0.561 seconds**. The final executable averages **2.121 seconds** with default parallelism, versus native Ty's **0.410 seconds**. Rendering requires a `models` import outside the early check's coverage, so the validated second-check path remains necessary. This fixture passes correctness and resource bounds but exceeds the timing target. An earlier exploratory comparison selected an older Ty from PATH and is explicitly marked invalid in the migration receipt.

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
