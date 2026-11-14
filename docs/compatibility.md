# Version compatibility

ty-extended and official ty are installed as separate distributions. ty-extended uses public CLI and LSP interfaces; it does not link Ty's semantic
engine or build a modified checker. Five published Ruff crates provide parsing,
text positions, notebook handling and diagnostics. Their registry versions are
pinned in Cargo.lock; the plugin host, protocol and SDK live in this repository.
There is no fork submodule or vendored Ruff source.

## Version policy

Starting with ty-extended 0.87.0, versions are independent. The package accepts a
group of official ty versions within the declared dependency range. Updating ty
inside that range does not require rebuilding or republishing ty-extended.

The tested-version ledger is recorded with the release's compatibility evidence.
Tests identify each official executable by version and SHA-256 and run the same
ty-extended binary against it. Endpoint checks alone do not establish the whole
range. The release gate also checks installed wheels, real editor sessions and
the Django conformance corpus.

Official ty is pre-1.0. A later release can change configuration, diagnostic names,
expected-type inference, bundled stubs or notebook responses. Scheduled compatibility
checks detect those changes. A failing upstream version needs investigation and,
where necessary, a ty-extended fix or dependency restriction; future versions are
not presented as already tested. ty-extended releases are driven by those fixes and
features, rather than every upstream version number.

## Qualified versions

The [0.90.0 ledger](qualification/compatibility-0.90.0.json) records one unchanged
optimized wheel executable against **all 21 official Ty releases from 0.0.84
through 0.0.104**. Every release passes all 18 CLI, editor, notebook, plugin and SDK
gates: **378 passing gates**. Native rule identities normalize documented pure
renames for internal classification and historical comparisons; consumer output
preserves official diagnostic IDs. Newly split implicit-return and generator
rules retain their distinct identities.

The independent workspace also passes all hosted runtime gates on Linux, macOS
and Windows, nine wheel/source packaging jobs, both Docker architectures and both
five-workload performance matrices. [The migration receipt](qualification/upstream-ruff-migration.json)
keeps this evidence separate from subsequent public-package acceptance.

### Earlier qualification

The [earlier native Django candidate](qualification/native-django-latest-v28.json)
passes its expanded source suite against every Ty release from **0.0.84 through
0.0.101**. The same ty-extended executable passes 236 regular Rust tests and three
additional integration tests in each projection mode on each version, 172 Django
corpus cases, SDK field and native API controls, and eight custom-primary-key
cases in each mode. Internal classification handles renamed native diagnostic
rules while consumer output preserves the official rule IDs. This source sweep
does not qualify installed wheels or extend the published release's ledger. The
candidate also fails its required 3-times native performance ceiling. That receipt predates the accepted performance implementation and the current
0.90.0 qualification; it is retained as historical evidence.

The [0.89.0 CI-wheel ledger](qualification/compatibility-0.89.0.json) records
one unchanged installed executable against **every official ty release from
0.0.84 through 0.0.99**: **288 passing generic gates**. Django-ty 0.7.0 also
passes 16 complete CLI conformance runs and 16 editor runs across that group.
Each CLI run checks 166 Django assertions and 83 independent runtime contracts;
editor runs verify all 28 baseline diagnostics, unsaved semantic edits and
restoration. The ledger identifies the wheel, executable, checkers, test sources
and archived reports by SHA-256.

The Django-ty 0.7 wheel additionally passes all 21 supported Django/Python pairs,
including Django 6.1.3. Python 3.15 has a separate qualification against Django's
166 assertions, 83 runtime contracts, nine QueryDict cases and installed editor
behavior. Its pinned MyPy baseline runs on a separately supported interpreter;
this is not a version-matched Python 3.15 comparator pair.

Protocol 0.9's batch path is checked against the individual full-index reference:
**2,582 class results and 3,012 batches** across 27 projects match through embedded
Monty 1.5.0. ty-extended passes 208 Rust tests on Linux/macOS and 211 on Windows,
50 Python harness tests and 83 SDK/host tests, together with 152 Django responses
through both CPython and Monty. Windows includes a dedicated drive/UNC
projection-boundary test. Tests cover batch limits, opt-in negotiation, individual-hook fallback, cache
invalidation and preservation of plugin indexes.

The final source passes Linux, macOS and Windows CI, including forced timestamp
collisions and Windows editor reconfiguration. The full version sweep uses the
exact CI wheel. Additional Django matrix results retain their earlier ty-extended
identity in the ledger and are repeated against the public ty-extended before the
Django release. Published-package acceptance remains a separate release gate.
Measurements against MyPy and bare
official ty are recorded in [the performance report](performance.md), including
the failed 0.88.0 follow-up that motivated this release.

The [published 0.89.0 receipt](qualification/ty-extended-0.89.0.json) verifies
all eight wheel/native archive pairs, source payloads and signed provenance, plus
SDK/protocol crates 0.0.11. The public macOS runtime matches the CI wheel used for
the full 16-version sweep after normalizing identified build metadata. Fresh
installed SDK, CLI and editor checks pass on Python 3.8, 3.9 and 3.13; the native
installer and an independent published-SDK consumer pass as well. All five public
performance workloads meet the MyPy median ceiling. Django's complete 21-pair
matrix and separate Python 3.15 controls pass against the published ty-extended.

### Previous releases

The [0.88.0 CI-wheel ledger](https://github.com/regularkevvv/ty-extended/blob/main/companion/evidence/compatibility-2026-11-07.json)
records 288 passing generic gates over the same 16 official versions, together
with Django CLI and editor coverage. Its Linux, macOS and Windows CI gates pass.

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

Qualification covers ty-extended's documented CLI, editor and plugin surface.
Official ty's new diagnostic-baseline options are preview features and are not
part of this qualification; baseline matching against transformed plugin views
needs its own validation. Checker diagnostics and rule severities remain owned
by the installed official version. New optional checker features can need a
ty-extended adapter even when existing checking workflows remain compatible.

## Plugin compatibility

ty-extended 0.90.0 embeds Monty **1.5.0**. The SDK and host tests execute with
the runtime selected by ty-extended lockfile, including sandbox and execution
limit controls. Plugin users do not install a separate Monty interpreter or
Python worker.

ty-extended runs embedded Monty plugins using protocol 0.9 and accepts existing
protocol 0.7 and 0.8 plugins. Protocol 0.9 adds opt-in bounded class batches;
protocol 0.8 adds optional class-specific indexes and read-only index lookups.
These additions do not change the official checker interfaces. Plugin package
requirements constrain ty-extended versions. The legacy `ty-compatibility` manifest
field remains part of the format, but does not select or install official ty.
Protocol negotiation and capability validation remain separate from checker
compatibility tests.

WASM and worker runtimes are outside ty-extended. The historical SDK ledger
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
