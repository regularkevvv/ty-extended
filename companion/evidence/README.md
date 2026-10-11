# Companion evidence

Validated source: `09015e694ca7897271961c075fb1be71162d7c82`. Executable SHA-256:
`c4f681887b358944318e01c73db864bf0de76e00ab24d15417e07d9ed2a4f8f2`.
The macOS arm64 executable is 14,948,064 bytes, or 6,576,615 bytes with gzip.
Including separately installed official ty, executables total 41,284,016 bytes,
versus 37,518,400 bytes for the fork. These are executable sizes, not wheel sizes.

- `artifact.json`, `size-comparison.json` and `provenance.json`: binary, source,
    checker and dependency identities.
- `matrix.json` and `diagnostics.json`: both complete 18-pair Django matrices,
    unchanged expectations and exact diagnostic arrays.
- `generic.json`: all three checker modes, including locations, inferred types
    and rejection controls.
- `supplemental.json`: CLI/SDK/editor gates, all five CLI formats, original SDK
    inventory accounting, and public CLI/editor/previous-binary timing samples.
- `protocol-inputs/`: the original 150-request Django corpus, replayed under both
    response-limit policies, including the expected 8 MiB rejection.
- `benchmark.json`: internal `project` timings with every sample, counter, warmup
    and backend order.

The SDK ledger distinguishes 75 unchanged original tests from 90 equivalent
public-contract adapters. Its 29 boundaries comprise 23 runtime/build-internal
cases, four runtime-policy cases and two private interfaces. Embedded Monty is
supported; WASM and worker execution are excluded. Original method-hook/index
counter differences are recorded, not claimed as literal test passes.

Public `check` measurements retain 36 verified invocations: two warmups and seven
measurements per backend/format. Editor measurements retain three warmups and
seven measured revisions per edit kind. The previous/final binary comparison
retains 60 calls across conformance and both e2e workloads, alternating execution
order with three warmups and seven measured pairs per workload. Every invocation
must match accepted output; artifact and input hashes are checked throughout.

All final samples were retained, with other task tests/builds/scans paused.
Measurements use an ordinary desktop session and may reuse OS caches. Fresh CLI,
internal `project`, and persistent-editor timings remain separate; none establishes
cold-disk timing or universal performance. See [results](../STATUS.md).

Checker pins: Django uses official 0.0.84 / fork 0.84.4; generic/editor
differentials use official 0.0.86 / fork 0.86.0. Separate reference failures:

- [`known-upgrade-failure.json`](known-upgrade-failure.json): fork 0.86 reports
    `invalid-attribute-override` for `forms.model-forms/inherited-meta`
    (`cases/extras.py:18`). The original expectation remains unchanged.
- [`known-reference-editor-failure.json`](known-reference-editor-failure.json):
    fork 0.84.4 returns `-32802` after edits; logs record a Salsa cycle-head panic.
    No warm-editor comparison ratio is claimed.

Reproduce the inherited-`Meta` failure with the unchanged Django `baseline` gate
and fork 0.86. Reproduce the editor failure with
`scripts/lsp_django.py --fork /path/to/ty-extended-0.84.4` and the accepted Django
project. [Gate commands and inputs](../tests/DJANGO_ACCEPTANCE.md) describe the setup.

`source:`, `binary:` and `artifact:` labels replace local paths. Exact original
reports and mappings are retained separately; earlier measurements remain in
history and local proof archives. These fixtures bound the compatibility claim.
[Hosted platform checks](https://github.com/regularkevvv/ty-extended/pull/41/checks)
are separate from the recorded local artifact runs.
