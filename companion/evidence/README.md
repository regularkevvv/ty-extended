# Companion evidence

Frozen release source:
`5e4b65cd9081b1281d05ae2035bfd75120eda326`, SHA-256
`9cc9b5aecb5df2fff1bc5e709383a5d193eee8536fe362586d6f0e1b82b75e05`.
Its macOS arm64 size is 13,972,912 bytes, or 6,221,558 bytes with gzip.
The separately installed official ty executable is excluded. Combined, the two
executables occupy **40,308,864 bytes**, versus **37,518,400 bytes** for the reference
fork. The companion artifact is smaller; the combined installation is larger.

- `artifact.json` and `provenance.json`: executable, source, checker and dependency identities.
- `matrix.json` and `diagnostics.json`: both complete 18-pair Django matrices, unchanged expectations and exact diagnostic arrays.
- `generic.json`: all three checker modes, including source locations, inferred types and rejection controls.
- `supplemental.json`: CLI/SDK gates; all five CLI formats; editor/notebook/lifecycle checks; actual public CLI and persistent-editor timing samples.
- `protocol-inputs/`: the original 150-request Django plugin corpus. Both response-limit policies are checked, including the expected 8 MiB rejection.
- `benchmark.json`: internal `project` command timings, including every accepted sample, counter, warmup and backend order.

Public `check` timings include full-format notes: 36 verified invocations, two
warmups then seven measurements per backend/format. CLI checks cover `full`,
`concise`, GitLab, GitHub and JUnit, including 28 primary diagnostics/locations
and external note paths. Internal `project` timings are separate from public CLI
performance. Editor measurements reuse the checker; fresh invocations may reuse
OS caches. Neither establishes cold-disk timing.

Earlier runs overlapping another project's tests are retained separately in
`supplemental.json`. The final timings retain all seven samples after the two
scheduled warmups. The old and new executables had comparable timings in an
interleaved comparison after that load ended.

Checker pins: Django uses official 0.0.84 / fork 0.84.4; generic/editor
differentials use official 0.0.86 / fork 0.86.0. Separate reference failures:

- [`known-upgrade-failure.json`](known-upgrade-failure.json): fork 0.86 reports
    `invalid-attribute-override` for `forms.model-forms/inherited-meta`
    (`cases/extras.py:18`). The original expectation remains unchanged.
- [`known-reference-editor-failure.json`](known-reference-editor-failure.json): fork
    0.84.4 returns `-32802` after comment or semantic edits; server logs record a
    Salsa cycle-head panic. No warm-editor comparison ratio is claimed.

Reproduce the inherited-`Meta` failure with the unchanged Django `baseline` gate
and the 0.86 fork. Reproduce the editor failure with
`scripts/lsp_django.py --fork /path/to/ty-extended-0.84.4` and the same accepted Django
project. [Gate commands and required inputs](../tests/DJANGO_ACCEPTANCE.md) document
the setup; each script exposes its complete arguments through `--help`.

`source:`, `binary:` and `artifact:` labels replace local paths; original reports
and the mapping are retained by the exporter. These fixtures bound the compatibility
claim. Hosted platform results remain available in [PR #41’s checks](https://github.com/regularkevvv/ty-extended/pull/41/checks).
