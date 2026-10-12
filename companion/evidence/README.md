# Companion evidence

Measured source: `ce12535f2c1be7f56945b1be15d6ef020ea86ec6`. Executable SHA-256:
`ca3e6c4b0cd20f4cab5fbca46949ac2331ff017aa59f817e7228344c0c4f3c2c`.
The macOS arm64 executable is 15,196,128 bytes, or 6,694,058 bytes with gzip.
Including separately installed official ty, executables total 41,532,080 bytes,
versus 37,518,400 bytes for the fork. These are executable sizes, not wheel sizes.

The latest optimization evidence is in [`speedup/`](speedup/):

- `artifact.json`: exact source, binary, runner and dependency identities; the
    compressed raw-report archive's checksum and verified file manifest.
- `validation.json`: 183 Rust unit tests, two live-checker unit tests, integration
    gates, 18 SDK/editor script gates, and all 18 Django/Python pairs in both modes.
- `timings.json`: every timing sample, warmup, execution order, output hash and
    work counter for public CLI, internal checker, editor and scaling comparisons.
- [`overlay-revision-followup.json`](speedup/overlay-revision-followup.json):
    a watched-file fixture correction and regression test, bringing the Python
    harness suite from 37 to 38 tests. A Windows reference check returned stale
    diagnostics. Equal modification timestamps reproduce the failure; normal
    and forced-collision runs pass after ensuring a distinct file revision.
    Diagnostic expectations, production code and timed runners are unchanged.
- [`runners/README.md`](speedup/runners/README.md): commands and portable runners.

CLI and scaling comparisons use two warmups and seven measured runs per backend
and workload, rotating execution order. Editor comparisons alternate three pairs
of before/after sessions: each has three warmups and seven measurements per edit
kind. Complete diagnostic items and report kinds match across those sessions.
All scheduled samples are retained; builds and other local checks are paused
during timing. Ordinary desktop activity and OS caches remain possible.

The earlier evidence remains intact:

- [`windows-path-followup/`](windows-path-followup/) records the pre-optimization
    binary `2bd50e24`, source `b054f63`, configuration-path regressions and timings.
    That same binary is the latest comparison's **before** backend.
- Root `artifact.json`, `provenance.json`, `matrix.json`, `diagnostics.json`,
    `generic.json`, `supplemental.json` and `benchmark.json` retain the original
    corpus under source `a4753158c11354d7042ac8d701d162981815369f` and binary
    `54a5c41f3b595b2a053f1d5d676cff72c0a73e03639bb6d38a52aaafa9dedbe0`.
- `protocol-inputs/` preserves the original 150-request Django corpus. Both
    response-limit replays pass: 8 MiB rejects one oversized response as expected;
    16 MiB accepts all requests.

The original SDK ledger distinguishes **75 unchanged tests** from **90 equivalent
public-contract adapters** and **29 excluded runtime/private-interface cases**.
These are not 194 executed companion tests. Embedded Monty is supported; WASM
and worker execution are excluded. Internal hook/index counter differences are
recorded separately from public behavior.

Django comparisons pin official ty 0.0.84 / fork 0.84.4; generic/editor
differentials pin official ty 0.0.86 / fork 0.86.0. Recorded reference failures:

- [`known-upgrade-failure.json`](known-upgrade-failure.json): fork 0.86 rejects
    inherited ModelForm `Meta` in `forms.model-forms/inherited-meta`.
- [`known-reference-editor-failure.json`](known-reference-editor-failure.json):
    fork 0.84.4 hits a Salsa panic after edits. No warm-editor fork ratio is claimed.

Expectations remain unchanged. [Gate commands](../tests/DJANGO_ACCEPTANCE.md)
describe reproduction; [results](../STATUS.md) report current medians.
Portable path labels replace machine-specific paths. Full raw reports are
retained in the separately checksummed archive.
[Hosted platform checks](https://github.com/regularkevvv/ty-extended/pull/41/checks)
are separate from local artifact validation. These fixtures bound compatibility
and performance claims.
