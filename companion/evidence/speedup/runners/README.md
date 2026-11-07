These scripts compare two companion binaries against the same Django fixtures and reference checker. Run them from the ty-extended repository root, or set `TY_COMPANION_REPO` to that root. Python only orchestrates the checks; the measured checker and plugin host are Rust.

Inputs used for this comparison:

- Django source: [`9996e1b3`](https://github.com/regularkevvv/django-ty/tree/9996e1b3e8a0656c988fd2b8deedb726b12fe575), including its original fixtures, Monty plugin, stubs, and `scripts/benchmark_runtimes.py` generator.
- Python 3.13 and the exact packages in that checkout's `conformance/candidate-requirements.txt`.
- Official `ty==0.0.84` and the [ty-extended 0.84.4 release binary](https://github.com/regularkevvv/ty-extended/releases/tag/v0.84.4). The SDK regression gates separately use official 0.0.86 and fork 0.86.0.
- Before/after companion binaries identified by the evidence's source commits and SHA-256 hashes.

Install reference packages into separate virtual environments. With uv, use `--no-config` so this repository's `exclude-newer` setting does not change which pinned release is available. Keep Django's environment free of django-stubs, mypy, and pydantic-monty; this experiment uses the vendored stubs and embedded Monty.

First run `companion/scripts/django_acceptance.py compare` with the before binary, `--checker-mode push`, and a new output directory. Supply `--django`, `--fork`, `--official`, `--companion`, `--python`, and `--python-version 3.13`. That output directory is `--acceptance` below. Repeat the acceptance gate for the after binary before benchmarking it.

```sh
python3 companion/evidence/speedup/runners/compare_benchmarks.py \
  --kind public --suites conformance \
  --before "$BEFORE" --after "$AFTER" --fork "$FORK" --official "$OFFICIAL" \
  --django "$DJANGO" --python "$DJANGO_ENV" --acceptance "$ACCEPTANCE" \
  --output "$WORK/public" --warmups 2 --repetitions 7
```

Use `--kind internal` without `--suites` to time all three workloads through both internal CLI and persistent-checker paths. The runners rotate backend order, retain every sample, verify diagnostics and exit status on every invocation, and verify input, interpreter, package, and binary hashes before and after. Public output must match exactly between the two companion binaries. Internal counters may differ between binaries but must remain stable within each workload/backend.

```sh
python3 companion/evidence/speedup/runners/editor_compare.py \
  --before "$BEFORE" --after "$AFTER" --official "$OFFICIAL" \
  --django "$DJANGO" --python "$DJANGO_ENV" --acceptance "$ACCEPTANCE" \
  --output "$WORK/editor" --pairs 3
```

Editor sessions alternate before/after order. Each opens the unchanged conformance project, verifies all diagnostics, then checks ten comment edits and ten semantic edits. The first three edits of each kind are warmups. `editor_capture_worker.py` records every notification-through-response elapsed value and complete diagnostic items and report kind. The runner compares the recorded sequences exactly and requires stable work counters across sessions of the same binary. Serialization and comparison happen after the timed edits.

```sh
python3 companion/evidence/speedup/runners/prepare_stress.py \
  --django "$DJANGO" --output "$WORK/stress"
python3 companion/evidence/speedup/runners/scaling_benchmarks.py \
  --before "$BEFORE" --after "$AFTER" --fork "$FORK" --official "$OFFICIAL" \
  --django "$DJANGO" --python "$DJANGO_ENV" --plan "$WORK/stress/plan.json" \
  --output "$WORK/scaling" --warmups 2 --repetitions 7
```

Scaling uses the existing Django generator with 0, 25, and 100 extra models, eight character fields, three queries per model, and shallow relations. It preserves the original positive checks and adds constructor, save, loaded-ID, deletion, and alias assertions. Every backend must return zero diagnostics and exit successfully.

Run timed commands sequentially, with builds and other local checks stopped. The helpers set `TY_MAX_PARALLELISM=1`; editor traversal also fixes `PYTHONHASHSEED=0`. Samples include process startup where applicable, and filesystem caches are not flushed. Every output directory must be new. Full raw reports remain in the compressed archive identified by the parent evidence files.
