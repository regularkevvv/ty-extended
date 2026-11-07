# Version compatibility

The companion and official ty are installed as separate distributions. The
companion uses public CLI and LSP interfaces; it does not link ty's semantic
engine or build a modified checker. The `ruff` submodule pins source dependencies
for parsing, notebook handling and the plugin host, independently of official ty.

## Version policy

Starting with companion 0.87.0, versions are independent. The package accepts a
group of official ty versions within the declared dependency range. Updating ty
inside that range does not require rebuilding or republishing the companion.

The tested-version ledger is recorded with the release's compatibility evidence.
Tests identify each official executable by version and SHA-256 and run the same
companion binary against it. Endpoint checks alone do not establish the whole
range. The release gate also checks installed wheels, real editor sessions and
the Django conformance corpus.

Official ty is pre-1.0. A later release can change configuration, diagnostic names,
expected-type inference, bundled stubs or notebook responses. Scheduled compatibility
checks detect those changes. A failing upstream version needs investigation and,
where necessary, a companion fix or dependency restriction; future versions are
not presented as already tested. Companion releases are driven by those fixes and
features, rather than every upstream version number.

## Qualified versions

The [0.88.0 CI-wheel ledger](https://github.com/regularkevvv/ty-extended/blob/main/companion/evidence/compatibility-2026-11-07.json)
records the same installed executable against every official ty release from
**0.0.84 through 0.0.99**: **288 passing generic gates**. Django-ty 0.7.0 also
passes 16 complete CLI conformance runs and 16 editor runs across that group.
Each CLI run checks 166 Django assertions and 83 independent runtime contracts;
editor runs verify the complete diagnostic corpus, unsaved semantic edits and
restoration. Installed SDK, CLI and LSP smoke checks pass on Python 3.8, 3.9 and
3.13. The companion's Linux, macOS and Windows CI gates also pass.

The Django-ty 0.7 wheel additionally passes all 21 supported Django/Python pairs,
including Django 6.1.3. Python 3.15 has a separate qualification against Django's
166 assertions, 83 runtime contracts, nine QueryDict cases and installed editor
behavior. Its pinned MyPy baseline runs on a separately supported interpreter;
this is not a version-matched Python 3.15 comparator pair.

The [published 0.88.0 receipt](qualification/ty-extended-0.88.0.json) verifies all
eight PyPI wheel/native archive pairs against release checksums, the source tree
and signed provenance. Public SDK crates 0.0.10 match the pinned source. Fresh
installed SDK, CLI and LSP checks pass on Python 3.8, 3.9 and 3.13; the native
installer also passes typing and editor controls without changing shell profiles.

The published macOS executable has identical machine code and data to the
qualified CI wheel after normalizing only the allocator's compile-time string,
the Mach-O UUID and the ad hoc code signature. The receipt records both original
hashes and the complete normalized comparison. The full version sweep belongs to
the CI wheel; public installation and artifact checks are recorded separately.
Measurements against MyPy and bare official ty are recorded in
[the performance report](performance.md).

### Previous release

The [published 0.87.0 receipt](qualification/ty-extended-0.87.0.json) verifies
all eight PyPI wheel/native archive pairs against release checksums and signed
provenance. Installed package checks pass on Python 3.8, 3.9 and 3.13; the native
installer also passes CLI and editor controls. The published macOS executable
matches the CI-built executable used for the complete checker-version sweep.

The 0.87.0 qualification checks every official ty release from **0.0.84 through
0.0.99**, inclusive. The same executable passed all 18 generic CLI, editor,
notebook, plugin and SDK gates on each version: **288 passing gates**. Django's
unchanged 166-assertion corpus, positive and negative projects, 83 runtime contracts
and 9 QueryDict cases also passed in CLI and persistent editor modes on every
version: **32 complete Django runs**, using Django 6.1.1 and Python 3.13.
A second sweep against ty 0.0.99 passed **42 complete runs** across all 18
Django/Python combinations in Django's version matrix and Django 6.1.3 on
Python 3.12, 3.13 and 3.14. Each combination runs both checker modes.

The CI-built wheel independently passes the same 288-gate version sweep.
Additional [filesystem revision probes](https://github.com/regularkevvv/ty-extended/blob/main/companion/evidence/overlay-directory-revisions-2026-11-06.json)
verify partial-overlay deletion and restoration under forced directory timestamp
collisions on macOS and Windows, using both endpoints of the qualified range.

The dependency range is `ty>=0.0.84,<0.1`. Versions after 0.0.99 within that range
are accepted by the resolver but are not covered by this recorded sweep. Weekly
CI installs the newest available version and repeats the regression gates.

## Scope of checker compatibility

Qualification covers the companion's documented CLI, editor and plugin surface.
Official ty's new diagnostic-baseline options are preview features and are not
part of this qualification; baseline matching against transformed plugin views
needs its own validation. Checker diagnostics and rule severities remain owned
by the installed official version. New optional checker features can need a
companion adapter even when existing checking workflows remain compatible.

## Plugin compatibility

Companion 0.89.0 embeds Monty **1.5.0**. The SDK and host tests execute with
the runtime selected by the companion lockfile, including sandbox and execution
limit controls. Plugin users do not install a separate Monty interpreter or
Python worker.

The companion runs embedded Monty plugins using protocol 0.9 and accepts existing
protocol 0.7 and 0.8 plugins. Protocol 0.9 adds opt-in bounded class batches;
protocol 0.8 adds optional class-specific indexes and read-only index lookups.
These additions do not change the official checker interfaces. Plugin package
requirements constrain companion versions. The legacy `ty-compatibility` manifest
field remains part of the format, but does not select or install official ty.
Protocol negotiation and capability validation remain separate from checker
compatibility tests.

WASM and worker runtimes are outside the companion. The historical SDK ledger
includes explicit private/runtime boundaries, rather than claiming every old
checker integration test executes unchanged. Consult [the evidence](https://github.com/regularkevvv/ty-extended/blob/main/companion/STATUS.md)
for tested contracts, rejection controls and known limitations.

## Reproduction

Install each official release in its own environment, then run:

```shell
python3 companion/scripts/checker_compatibility.py \
  --companion /path/to/ty-extended --fork /path/to/reference/ty \
  --official 0.0.86=/path/to/official086/ty \
  --official 0.0.99=/path/to/official099/ty \
  --output /tmp/new-compatibility-report
```

Repeat `--official` for every version being qualified. `--gate` creates a partial
run; its report marks `complete = false`. A partial run cannot establish release
compatibility. The reference distribution is used only by tests and is not a
runtime dependency.
