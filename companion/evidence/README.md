# Companion evidence

Frozen release source:
`a7ac9fc5bb46a92225eb35ae43c7578ded258185`, SHA-256
`59e0634f6219b32190b1efce851775de1ef80f3ca667443a267a2231340cba63`.
Its macOS arm64 size is 13,972,992 bytes, or 6,223,690 bytes with gzip.
The separately installed official ty executable is excluded. Combined, the two
executables occupy **40,308,944 bytes**, versus **37,518,400 bytes** for the reference
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

The final timings retain all seven samples after the scheduled warmups, with no
other companion tests running. Measurements come from a normal desktop session;
they are not dedicated-lab or cold-disk results. Earlier builds and measurements
remain in repository history and the retained local proof archives.

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
