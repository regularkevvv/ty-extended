# Companion performance

The companion checks generated analysis copies with unchanged official ty and
runs Django hooks in embedded Monty. Its total time includes discovery, plugin
execution, projection, type queries and the final checker run.

## Acceptance limits

The companion must take at most **3 times** bare official Ty on identical
source, using the same official Ty executable. Compare with Ty's documented
`django-types` setup so native Django support participates. Preserve the
correctness assertions, ordinary Python and ORM error controls, and focused
Django contracts. The companion must also remain at least as fast as MyPy with
its Django plugin. A largest sample below one second remains an additional goal.

`companion/scripts/compare_checkers.py` enforces both the median ratio and every
rotated companion/native sample pair, with a default `--max-ty-ratio 3`.
`--official-python` selects the bare Ty environment containing `django-types`;
without it the script retains the historical no-provider diagnostic expectations.
The JSON report separates valid correctness evidence from performance acceptance,
and the command exits unsuccessfully when either performance ceiling fails.

The [v25 receipt](qualification/native-django-parallel-v25.json) records a further
reduction from 1.786 to **1.619 seconds**, with a largest candidate sample of
1.674 seconds. Original-source revision checks remove redundant reads and the
single-revision CLI avoids constructing an unused incremental context. These are
paired companion measurements on Ty 0.0.99, rather than a new native comparison.
They do **not** establish the 3-times ceiling. Merge and release remain gated on
meeting the current performance and correctness requirements together.

## Current native comparison

The [Ty 0.0.101 receipt](qualification/native-django-latest-v28.json) uses the
same official executable for both Ty stacks. Five rotated fresh processes per
stack check the unchanged 7,082 files, 2,501 models and 100 assertions:

| Stack                                 |  Median |
| ------------------------------------- | ------: |
| Companion with Django hooks in Monty  | 1.626 s |
| Official Ty with django-types         | 0.286 s |
| MyPy with django-stubs and its plugin | 6.680 s |

The companion is **5.68 times** native Ty; the largest paired ratio is **5.76**.
The required 3-times gate **fails**. All stacks pass the 100 assertions and both
error controls. The additional focused cases match 20/20 for the companion,
19/20 for MyPy and 8/20 for native Ty, with no unmarked diagnostics. All 21
observations meet the declared source, host-load and resource limits. Their
temporary projections are removed after each run, and both Cargo targets were
cleaned before timing. These are local shared-machine measurements with warm
filesystem caches permitted.

The earlier candidate handles native diagnostic renames introduced in Ty 0.0.100.
Internal classification recognizes both spellings while CLI and editor output
retain the official checker's raw rule IDs. Full Rust tests and SDK/CLI suites
pass against 0.0.101. The expanded source matrix passes every release from
0.0.84 through 0.0.101, including all 236 regular Rust tests in each mode,
172 Django cases, SDK/API controls and eight primary-key cases in each mode.
These results establish neither installed-artifact acceptance nor release
readiness. Later performance prototypes below have only been qualified against
0.0.101; they still require a fresh version matrix.

Two later experiments also fail the release limit. The
[fused Django index receipt](qualification/native-django-fused-v31b.json)
measures 1.595 seconds against native Ty's 0.314 seconds, or 5.08 times. Its
Rust initializer optimization was reverted because it did not improve the
measured flow phase; the equivalent fused Django index remains under evaluation.
The [native-return experiment](qualification/native-django-native-returns-v32b.json)
removes 100 redundant generated casts but measures 1.663 seconds against
0.296 seconds, or 5.61 times. Both preserve all 100 assertions, two error controls
and 20 focused Django cases. The second receipt retains two rejected host-load
observations as well as every accepted observation. Neither experiment establishes
performance acceptance or release readiness.

The [declaration traversal experiment](qualification/native-django-declaration-v35c.json)
also fails: 1.671 seconds versus native Ty's 0.376 seconds, or 4.45 times, with
a largest paired ratio of 5.12. Its previous companion comparison is 1.666
seconds, so it establishes no material improvement. The declaration shortcut
and snapshot path shortcut were removed. The receipt retains all 28 accepted
observations, the correctness controls and the preceding admission attempt that
accepted no samples. Source and build artifacts are preserved for review.

The [concurrent native-check prototype](qualification/native-django-concurrent-v36b.json)
is explicitly opt-in. It shares immutable source snapshots with a disposable
native check while source and plugin indexing proceed, waits before writing
projections, discards the initial diagnostics, and checks the synchronized final
state. Source/index discovery that needs another probe namespace and newly
required external roots retain fresh checker initialization.

On Ty 0.0.101 it passes 239 regular Rust tests and three additional tests in
each mode, plus 36 SDK/CLI suite runs covering native fields and concurrency
enabled and disabled. Generated row creation, type changes and deletion produce
the same diagnostics as a fresh native checker. Django's 172 cases, runtime
contracts, primary-key cases and larger example pass. The stored full-conformance
receipt remains stale. The large probe engages concurrency with 7,082 source
snapshots and zero rereads. All 19 broader SDK/editor suites also pass in each
native-fields mode against Ty 0.0.101; generated benchmark projections are
verified, compressed and removed after each suite.

A later quiet window admitted all 28 local observations. Five rotated repeats
measure **1.197 seconds** for the concurrency candidate, **1.493 seconds** for
the preceding companion and **0.291 seconds** for native Ty. This is a 19.8%
companion improvement but still **4.11 times** native Ty, with a largest paired
ratio of 4.46. All 100 assertions, both error controls and all 20 companion focus
cases pass. The isolated Linux run independently measures **1.137 seconds**
versus **0.262 seconds**, or **4.35 times**, and preserves the same correctness
controls. Isolated macOS measures **1.512 seconds** against **0.452 seconds**,
or **3.34 times**, with a largest paired ratio of 5.09. All three runs fail the
strict performance gate. Both CI jobs preserve their correctness evidence and
confirm cleanup of their temporary Python environments.

The [index ownership experiment](qualification/native-django-index-ownership-v37.json)
lets a fresh memory CLI check consume opaque project and class indexes directly.
It avoids retaining a response cache and returning duplicate indexes that this
single check never reads. Editors, persistent sessions and explicit project
artifacts keep their existing behavior. One- and two-plugin tests preserve class
scopes, opaque data, virtual namespaces and subsequent revisions.

Five rotated local repeats measure **1.180 seconds** against the preceding
companion's **1.218 seconds**, a 3.1% improvement in the same comparison. Native
Ty measures **0.329 seconds** and MyPy **7.084 seconds**. The companion is still
**3.59 times** native Ty, with a largest paired ratio of 4.04, so the gate fails.
All 100 assertions, both controls and 20 companion focus cases pass. This source
also passes 240 regular Rust tests plus three additional tests in each mode,
36 compiled SDK/CLI suite runs and all 172 Django corpus cases. Build targets
were cleaned before timing; all 28 observations meet the declared admission
limits. The broader editor/version matrices remain prerequisites for release.

The [parallel summary experiment](qualification/native-django-summary-parallel-v38.json)
preserves sequential merge order while computing independent class summaries
on multiple workers. It measures **1.245 seconds** against the preceding
companion's **1.277 seconds** and native Ty's **0.370 seconds**. Its median
ratio is **3.37**, and its largest paired ratio is **4.06**: the gate fails.

The [collection experiment](qualification/native-django-native-collections-v39.json)
shares immutable native inputs with the worker, uses hash maps for lookup-only
data and builds ordered file collections in bulk. It measures **1.115 seconds**
against the preceding companion's **1.173 seconds**, native Ty's **0.294 seconds**
and MyPy's **7.000 seconds**. The 5.0% paired companion improvement still leaves
a **3.79** median ratio and **3.93** largest paired ratio, so the gate fails.
Each experiment preserves all 100 assertions, both error controls and 20
companion focus cases. Both pass 241 regular Rust tests and three additional
tests in each mode, 36 SDK/CLI suite runs and the 172 Django corpus cases;
their build targets were cleaned before timing. Ratios from different benchmark
sessions must not be interpreted as a controlled comparison between prototypes.

The [borrowed overlay experiment](qualification/native-django-borrowed-overlays-v40d.json)
avoids copying eligible provider files, reuses canonical source paths from the
fresh CLI snapshot, and honors explicit signature-method selection. It measures
**0.995 seconds** against the preceding companion's **1.168 seconds**, native
Ty's **0.327 seconds** and MyPy's **7.157 seconds**. All 100 assertions, both
controls and 20 companion focus cases pass, but the **3.05** median ratio and
**3.28** largest paired ratio still fail the strict gate. The largest positive
sample is 1.024 seconds, so the separate one-second goal also fails.

This source passes 243 regular Rust tests plus three additional tests in each
mode, 72 SDK/CLI suite runs, the 172 Django corpus cases and the runtime/example
checks. Its new provider regression caught stale imported types during native
warmup; cross-workspace invalidation fixes that failure while preserving provider
bytes and timestamps. Both Cargo targets and the temporary SDK target were
cleaned. The current Django relation bug, full-conformance receipt, broader
editor checks, version sweep and installed-artifact acceptance remain open.

The [direct warmup experiment](qualification/native-django-direct-warmup-v41.json)
skips buffer preparation and report mapping for the discarded initial check.
It preserves complete synchronization and mapping for the real check. In its
own paired run it measures **1.099 seconds**, against **1.129 seconds** for the
preceding companion, **0.402 seconds** for native Ty and **7.229 seconds** for
MyPy. Its **2.73** median ratio passes, but the **3.18** largest paired ratio
fails, so this experiment is also rejected. Absolute times across these shared
machine sessions are not a controlled comparison between prototypes.

All 100 assertions, both error controls and 20 companion focus cases pass.
The receipt retains 28 accepted observations and one rejected host-load sample.
All provider-file bytes and modification times remain unchanged. This source
passes 244 regular Rust tests and three additional tests in each mode, 72
SDK/CLI suite runs and the same Django qualification, with build targets cleaned
before timing. The known correctness and broader qualification gaps remain.

The [early warmup and path-compaction experiment](qualification/native-django-early-warmup-v42.json)
measures **0.974 seconds** against **1.045 seconds** for its preceding companion,
**0.306 seconds** for native Ty and **7.036 seconds** for MyPy. Its **3.18** median
ratio and **3.30** largest paired ratio fail the gate. All 100 assertions, both
controls and 20 companion focus cases pass; the report preserves 28 accepted
observations and one rejected host-load observation. It passes 246 regular Rust
tests and three additional tests in each mode, plus 72 SDK/CLI suite runs.

The [discovery-catalog candidate](qualification/native-django-discovery-catalog-v43.json)
also reuses the identity of ordinary entries found below canonical directories.
Both snapshot readers retain full resolution for symlinks. Its reference test
compares discovered paths, bytes and revisions with the previous loader,
including internal and external symlinks. It passes 247 regular Rust tests and
three additional tests in each mode, 72 SDK/CLI suite runs and the Django source
qualification. Optimized and test builds have matching source hashes and were
cleaned. Local timing and isolated Linux/macOS acceptance are pending; no
performance or release acceptance is claimed for this candidate.

The manual `Companion` workflow's `native_performance_only` input runs the
identical 7,082-file, 2,501-model comparison on isolated Linux and macOS runners.
It pins the experimental Django source, uses the same official Ty executable
for both Ty stacks, installs django-types for bare Ty, preserves the strict
3-times gate, and separately checks the 20 focused ORM cases. Its build target
is cleaned before timing and temporary Python environments are removed afterward.

## Parallel source loading experiment

The [v24 receipt](qualification/native-django-parallel-v24.json) compares the
two-worker companion with a candidate using up to eight available workers for
initial parsing and independent declaration copies. Both use identical Django
code, declarations and official Ty 0.0.99 on the same 7,082-file, 2,501-model
workload. `TY_MAX_PARALLELISM` bounds the companion workers as well as native Ty;
small inputs remain sequential. Installation and error propagation retain input
order.

Five alternating fresh-process samples per binary measure **1.881 seconds**
before and **1.788 seconds** after, a 4.9% reduction. The largest candidate sample
is **1.813 seconds**, so the one-second target remains unmet. Both binaries pass
100 assertions, two error controls and 20 focused ORM cases. Every observation
satisfies the declared source-preservation, host-load and resource checks.
Filesystem caches may be warm; these are local shared-machine measurements.

Three additional fresh-process samples at each thread limit measure:

| `TY_MAX_PARALLELISM` | Median total | Median initial source loading |
| -------------------- | -----------: | ----------------------------: |
| 1                    |      2.227 s |                       0.244 s |
| 2                    |      1.935 s |                       0.160 s |
| 4                    |      1.814 s |                       0.113 s |
| 8                    |      1.798 s |                       0.081 s |

These limits apply to both the companion and native Ty, so the total-time column
does not isolate parsing. The source-loading column shows the relevant phase.
The eight-thread result leaves serial work: one representative median candidate
run spends 0.400 seconds on the Django project index, 0.314 seconds on initial
lowering plus flow/render, and 0.527 seconds on native checking and its
preparation. More source-loader threads alone cannot remove those costs.

The identical Django code uses a compact private index. A separate, preceding
same-binary comparison reduced its opaque payload from 2,853,625 to 1,100,506
bytes, with medians of 2.040 seconds before and 1.949 seconds after. Decoded model
and hook responses match in 27,511 comparisons, and the legacy representation
remains available. This primarily reduces metadata size; it does not remove the
whole project-index pass.

The v24 source passes 236 regular Rust tests and three additional integration
tests in each projection mode, formatting and Clippy, and 19 broader SDK/editor
suites in each mode. Every official Ty version from 0.0.84 through 0.0.99 passes
172 Django contract cases, SDK field and native API controls in both modes, and
eight focused regressions. The larger Django example has zero positive-case
diagnostics and all nine expected negative diagnostics in both modes. Python
tests are 50/51; the stored full-conformance artifact still fails its stale-source
gate. Installed artifacts, full API and Django/Python matrices, hosted CI and
release acceptance remain open. Both build targets were cleaned before timing;
completed generated benchmark projections were verified, compressed and removed.

## Native Django delegation prototype

The [v20d experimental receipt](qualification/native-django-hybrid-v20d.json)
records a companion prototype using an adapted `django-types` 0.24.0 declaration
base and unchanged official Ty 0.0.99. Monty retains constructor and query
validation, reverse relations, precise result rows, and model-state checks.
Native declarations handle ordinary fields. Eligible literal `get()` calls avoid
user type probes; unknown arguments, custom managers and other unsupported cases
retain native probing. A complete native check is reused only when its final
source, selection and generated dependency context are unchanged.

Five accepted rotated fresh-process samples per stack use identical source:
7,082 files, 2,501 models and 100 assertions.

| Stack                                 |  Median |
| ------------------------------------- | ------: |
| Native Django hybrid prototype        | 2.076 s |
| Previous companion candidate          | 5.525 s |
| MyPy with django-stubs and its plugin | 7.136 s |
| Official Ty with django-types         | 0.323 s |

All four stacks pass the 100 assertions and both error controls. The additional
20 focused ORM cases match 20 for the hybrid, 20 for the previous companion,
19 for MyPy, and eight for native Ty. Source bytes and modification times are
preserved. All 28 observations satisfy the predeclared host-load and resource
limits. The receipt retains every observation. Timing acceptance depends on host
load and resource/source checks, never the observed execution time.
Filesystem caches may be warm; these are shared-machine local measurements.

On every official Ty release from 0.0.84 through 0.0.99, the current source
passes 172 Django contract and row cases in memory mode, the generic SDK field
fixture and runtime-backed API checks in both projection modes, and seven focused
projection and source-loading regressions. This uses Python 3.13 and Django 6.1.3. Four
model-name ambiguity cases also pass on Ty 0.0.99. All 235 regular Rust tests and
three additional native integration tests pass in each of default and native
modes; 19 broader SDK/editor suites pass in both modes. These source results
qualify the recorded contracts, not future Ty releases or every Django API.

The larger Django example passes with zero positive-case diagnostics and all
nine negative-case diagnostics in both modes. Its failing and passing evidence
is retained: nested receiver views previously collided, nullable relationship
reads lost branch narrowing, and field contributions needed their existing native
assignment behavior. The regression checks include aliases, inheritance,
constructor diagnostics, branch joins and mutation invalidation.

Instrumentation confirms 100 early returns, one inference round and native-check
reuse on the timed fixture. It also confirms 7,080 untouched files skip flow/render
passes and 182 external declaration modules enter the companion syntax index.
All physical stubs remain available to the native checker. Optimized median
phase timers show where the remaining work goes:

| Stage                                                 | Median phase time |
| ----------------------------------------------------- | ----------------: |
| Source and declaration index                          |           0.259 s |
| Plugin project index, including host work             |           0.411 s |
| Initial lowering plus flow/render                     |           0.364 s |
| Complete native check, including preflight validation |           0.620 s |

Setup, support work, diagnostic mapping and cleanup also contribute. Phase
medians do not add up exactly to a median wall time. The largest accepted hybrid
sample is **2.358 s**: **the one-second target remains unmet**. The earlier
[v11 series](qualification/native-django-hybrid-v11.json) measured 2.710 seconds;
all stacks were slower in that earlier series. The current receipt identifies
executables, source hashes, commands, resource measurements and every sample.

Large initial snapshots now read and parse with at most two workers, install
results in input order, and move parsed syntax instead of cloning it. A
[source-loading development comparison](qualification/native-source-loading-v19.json)
checks duplicate-module precedence, first-error order, incomplete syntax and
external sources. Its development timings are separate from the optimized
comparison above. Classes without a class-transform plugin, contribution or
nontrivial patch retain native declarations; calls and member hooks still use
the existing analysis. The large fixture retains 7,080 passthrough units, so this
change does not bypass the model file or its plugin observations.

The previous [v17b series](qualification/native-django-hybrid-v17b.json) measured
2.206 seconds. Shared-host variation means these samples do not establish a
worst-case bound. The current build removed its 700 MiB target before timing;
qualification removes or compresses completed projections and cleans Cargo
targets after each stage.

This experiment has no release acceptance. The complete static API audit, fresh
full conformance receipt, automatic consumer activation, installed-wheel and
Django editor checks, Django/Python matrices and hosted CI remain open. Django's
Python tests are 47/48: the stored full-conformance receipt correctly fails its
stale-source check. It has not been rewritten to make the gate green. The
prototype requires unreleased companion and Django code plus experimental flags;
its local Ty matrix does not qualify installed wheels or the full declaration base.

## Official Ty with native Django support

The [2026-11-08 comparison receipt](qualification/official-django-comparison.json)
compares official Ty 0.0.99 with `django-types` 0.24.0, the unreleased companion
and Django candidates, and MyPy 2.4.1 with django-stubs 6.1.2. Every stack uses
Python 3.13.0 and Django 6.1.3. The native environment follows
[Ty's documented Django setup](https://docs.astral.sh/ty/features/third-party/django/)
and contains neither django-stubs nor Django-ty. A separate field-inference and
invalid-assignment smoke test verifies that native Django support is active.

The original 7,082-file, 2,501-model, single-module fixture is unchanged:

| Stack                                        |  Median | ORM assertions | Error controls |
| -------------------------------------------- | ------: | -------------: | -------------: |
| Official Ty with django-types                | 0.297 s |        100/100 |            2/2 |
| Candidate companion and candidate Django-ty  | 5.163 s |        100/100 |            2/2 |
| MyPy with django-stubs and its Django plugin | 6.793 s |        100/100 |            2/2 |

These are five rotated serial fresh-process samples per stack, with an empty
MyPy cache each time. Filesystem caches may be warm. Every run preserves source
bytes and modification times and remains below the recorded host-load limit.
The receipt retains every sample, executable identity, environment, command,
resource measurement and diagnostic. The sampled process-tree RSS medians are
approximately 93 MiB for native Ty, 1,158 MiB for the companion and 495 MiB for
MyPy. These samples are local measurements, not worst-case bounds.

Native Ty is about 17.4 times faster than the companion on this fixture while
passing the same basic assertions and intentional errors. The earlier bare-Ty
comparisons omitted `django-types`; their failed assertions do not describe the
documented native Django setup measured here. The extra companion coverage
below does not establish that five seconds is necessary for this basic fixture.

### Correctness beyond the timed assertions

A separate correctness extension replaces one filler file with 20 probes using
public Django APIs and standard-library typing. It retains exactly 7,082 files,
2,501 models and the original 100 assertions. Each stack checks identical source.
Its one-shot timings are recorded separately from the medians above.

| Stack               | Valid cases accepted | Invalid cases rejected | Total matched |
| ------------------- | -------------------: | ---------------------: | ------------: |
| Official Ty         |                 6/10 |                   2/10 |          8/20 |
| Candidate companion |                10/10 |                  10/10 |         20/20 |
| MyPy + django-stubs |                10/10 |                   9/10 |         19/20 |

Native Ty misses string-target relationship types, reverse relationships, precise
`values()` / `values_list()` results, and constructor/query validation in these
probes. MyPy misses the invalid lookup `name__missing`; the companion rejects it.
The receipt preserves one original test correction: an unsaved foreign-key ID
was incorrectly expected to be exactly `int`. Django's runtime returns `None`
when it is omitted. The final probe tests rejection of a `str` assignment without
requiring that state-dependent exact type. Original source, scores and diagnostics
are retained; no implementation changed to improve these results.

The unchanged 166-case project corpus separately matches 86 expectations for
native Ty, 166 for the companion and 124 for MyPy. Native Ty also emits nine
diagnostics outside marked cases; the other stacks emit none. This corpus includes
state-aware primary-key expectations and django-stubs annotation conventions,
including some that differ from django-types. These are counts against this
project's contract expectations, not general Django accuracy percentages. The
receipt includes all cases, diagnostics and feature totals for inspection.

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

### Field access follow-up

The [field projection receipt](qualification/field-access-projection.json) records
corrections for all six differences found by the original twelve-case
[class-field audit](qualification/field-class-access-audit.json). A larger regression
covers 31 type assertions and five diagnostic controls in both projection modes,
including properties, owner-dependent descriptors, class rebinding, inherited
annotations, containers and decorated ancestors. It preserves original class
access where supported while applying the plugin's instance getter and setter.
The receipt identifies conservative fallback cases and the audited function-type
display difference; it does not claim universal SDK equivalence.

All 221 unit tests, the 166-assertion Django corpus with six error controls, and
the expanded field regression pass on each of the 16 official ty versions from
0.0.84 through 0.0.99. The same source passes all 19 broader CLI/editor/SDK suites,
nine integration binaries, formatting and Rust lint checks. Hosted CI and
published-artifact acceptance are separate from these local results.

| Stack                                                             |  Median |
| ----------------------------------------------------------------- | ------: |
| Field corrections plus candidate Django manager declarations      | 5.187 s |
| Preceding index/cache candidate plus the same Django declarations | 4.806 s |
| Published companion and Django-ty                                 | 6.197 s |
| MyPy plus django-stubs                                            | 6.560 s |
| Bare official ty                                                  | 0.256 s |

These are three rotated fresh-process runs per stack on the unchanged 7,082-file,
2,501-model, 100-assertion fixture. The receipt also includes the corrected
companion with the published Django plugin. All companion/MyPy assertions and
intentional error controls pass, and source bytes and modification times are
preserved. Bare ty fails the Django assertions. Host load stays below the recorded
comparison limit, but the machine is shared and these are not worst-case bounds.

The field corrections cost time compared with the preceding performance-only
candidate. They are a compatibility improvement, not another speedup. The
one-second target remains unmet. A Django annotation shortcut showed no end-to-end
benefit in its paired series. Moving every Django class patch into the project
index was rejected because its response exceeded the existing 8 MiB bound and
changed precedence with other plugins. Neither experiment was adopted.

### Factory specialization follow-up

The [factory receipt](qualification/factory-specialization.json) qualifies source
`86c3a21` after sharing declarations for built-in field getter types. The
original class-access type stays generic. For these closed getters, the factory
no longer needs two casts and two additional type variables inferred at every
field. Other types retain the general factory. A separate module-import prototype
showed no clear independent benefit and was not adopted.

All 222 unit tests, 166 Django assertions with six error controls, and 31 field
assertions with five controls pass on every official ty version from 0.0.84
through 0.0.99. The candidate also passes all 19 broader CLI/editor/SDK suites,
nine integration binaries, formatting and Rust lint checks. The original field
audit still matches the historical diagnostics exactly.

| Stack                                                                 |  Median |
| --------------------------------------------------------------------- | ------: |
| Specialized factories plus candidate Django declarations              | 5.124 s |
| Preceding field-corrected companion plus the same Django declarations | 5.181 s |
| Published companion and Django-ty                                     | 6.403 s |
| MyPy plus django-stubs                                                | 6.715 s |
| Bare official ty                                                      | 0.265 s |

These are three rotated fresh-process samples per stack on the unchanged
7,082-file, 2,501-model, 100-assertion project, with the same error controls and
source preservation checks. The receipt retains all samples and the corrected
companion with the published Django plugin. Bare ty fails the ORM assertions.
The end-to-end median improves by about 1.1%; this is a small gain,
not a solution to the overall latency target. The one-second target remains unmet;
these shared-host timings are not worst-case bounds or published-release acceptance.

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
speed guarantee. The bare-Ty environment in that series omitted `django-types`
and did not provide the Django checking used there.

The additional 250-module project checks the same large model population spread
across files. It preserves all 100 ORM type assertions and both negative controls.
CI enforces the MyPy median ceiling on all five project shapes.

## What is compared

The historical `companion/scripts/compare_checkers.py` series creates a registered
Django application and runs three public commands over identical Python files:

- `ty-extended check`, with Django-ty's Monty plugin and packaged stubs;
- MyPy with django-stubs and its Django plugin;
- bare official `ty check`, without `django-types`, a companion or Django plugin.

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
run. The historical performance gate required the companion's median to be no
slower than MyPy's median on that workload. The current harness additionally
enforces the 3-times native ceiling described above.

In that setup, bare Ty reports unresolved Django model managers and failed type
assertions. Its timings show the underlying checker baseline; the native Django
comparison above supplies the required `django-types` package and measures its
coverage separately. These fresh-process measurements also do not compare
MyPy's persistent daemon or either checker's incremental editor performance.

## Reproduce

Use separate environments for the two plugin implementations, with the same
Python and Django versions. Install `mypy` and `django-stubs` in the reference
environment, and the candidate `ty-extended` and `django-ty` in the companion
environment. Install Django and `django-types` in a separate native environment.
Use the same official Ty executable for both Ty commands. The current native
delegation prototype additionally requires the experimental environment flags
recorded in its receipt.

```shell
python3 companion/scripts/compare_checkers.py \
  --companion /path/to/candidate/bin/ty-extended \
  --official /path/to/candidate/bin/ty \
  --official-python /path/to/native/bin/python \
  --python /path/to/candidate/bin/python \
  --mypy /path/to/reference/bin/mypy \
  --plugin /path/to/candidate/lib/python3.13/site-packages/django_ty \
  --files 7082 --models 2501 --queries 100 --repeats 5 --max-ty-ratio 3 \
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
