# Changelog

ty-extended tracks upstream [ty](https://github.com/astral-sh/ty) and adds semantic plugin
support on top of it. This file records what the fork changes.

Entries below 0.74.0 say "extension" where they now would say "plugin"; the term was settled on
"plugin" in 0.74.0 and released notes are left as they were published.

## 0.84.4

- Add protocol 0.7 constructor state claims for exact classes and subclasses.
- Ship Python constructor claims and Rust protocol/SDK crates 0.0.8.
- Retain upstream ty 0.0.84.

## 0.84.3

Built on [ty 0.0.84](https://github.com/astral-sh/ty/releases/tag/0.0.84).

- Plugins can describe receiver member changes after a successful call, independently of its
    return type. Constructor and loader hooks can also describe members on fresh results.
- Member facts follow explicit aliases and join across branches. Unknown calls, possible aliases,
    writes, exceptions, suspension, loops, and unclaimed overrides invalidate assumptions
    conservatively. Container aliases and facts across scopes are not inferred.
- Protocol `0.6` adds `adjust-call-state` and `CallStatePatch`; Rust and Python SDKs expose the
    new hook. `ty_plugin_protocol` and `ty_plugin_sdk` advance to `0.0.7`.

Domain rules remain in plugins. See [plugin authoring](docs/plugin-authoring.md#object-member-state)
for the hook contracts and limitations.

## 0.84.2

Built on [ty 0.0.84](https://github.com/astral-sh/ty/releases/tag/0.0.84). No upstream changes; this
is a fork-only patch release.

`ty_plugin_protocol` and `ty_plugin_sdk` are bumped to `0.0.6`.

### Python plugins on the Monty runtime

Plugins can now be authored in Python and executed inside the embedded
[Monty](https://github.com/pydantic/monty) sandbox — no Rust toolchain required. The wire
protocol moves from `0.4` to `0.5` for the new `monty` runtime kind; the change is additive, so
plugins built against earlier releases continue to load unchanged.

- A new `monty` plugin runtime executes a `.py` artifact inside Monty's restricted Python
    subset: no third-party imports, filesystem, environment, or clock access. The `ty.plugin_sdk`
    module shipped inside the `ty` wheel is prepended to the plugin source, so its hook
    decorators (`@on_call_return`, ...), manifest helpers, and response builders are ambient
    names in plugin files.
- Standard builds include WASM, embedded Monty, and optional Monty worker-process support.
    Python plugins run in-process by default, without an external runtime dependency. Install
    `ty-extended[monty-workers]` and set `plugins.monty-mode = "worker"` to enable crash isolation
    on supported platforms with Python 3.10+. Workers are resolved from `TY_MONTY_BIN`, then
    `monty` on `PATH`; worker startup failures report a plugin error.
- Semantic-hook requests now carry the plugin's resolved configuration in `context.config`,
    matching what `build-project-index` requests already sent.
- Project configuration accepts `runtime = "monty"` for `[[tool.ty.plugins.plugin]]` entries, and
    installed-package discovery maps `{"kind": "monty"}` manifests. Feature-disabled builds
    report `monty` as an unsupported runtime through the existing settings diagnostic.
- `PluginResponse::Dependencies` now serializes through a `DependenciesResponse` wrapper — the
    previous internally-tagged newtype sequence could not round-trip on the wire, so no plugin
    could have emitted it before.
- A runnable end-to-end project lives at `examples/monty/`; authoring guidance is in
    `docs/plugin-authoring.md`. `examples/minidjango/` ports the reference `MiniDjangoPlugin`
    (class transforms, project indexing, cross-symbol contributions, virtual types, settings
    claims, lookup diagnostics) to Python over an identical project, so `monty` and `wasm`
    runs can be diffed side by side.

### Rust plugin API

- `SemanticContext` struct literals must include the new `config` field.
- Construct `PluginResponse::Dependencies` with a `DependenciesResponse` wrapper.

## 0.84.1

Built on [ty 0.0.84](https://github.com/astral-sh/ty/releases/tag/0.0.84). No upstream changes; this
is a fork-only patch release.

`ty_plugin_protocol` and `ty_plugin_sdk` are bumped to `0.0.5`, and the wire protocol moves from
`0.3` to `0.4`. The changes are additive, so plugins built against earlier releases continue to
load unchanged.

### Plugin behaviour

- `BuildProjectIndexRequest` now includes summaries of module-level decorated functions —
    qualified names, decorator summaries, inferred post-decoration types, and source metadata.
    Traversal covers module-level compound statements (`if`, `try`, loops, `with`, `match`) but
    does not descend into function bodies or class bodies.
- Plugins can claim method names by `*`-only glob pattern via the `on-subclass-of-matching` claim.
    `*` matches every method; prefix, suffix, and contains patterns are supported. Exact claims and
    pattern claims are evaluated independently, and pattern claims route both call-signature and
    call-return hooks.

## 0.84.0

Built on [ty 0.0.84](https://github.com/astral-sh/ty/releases/tag/0.0.84).

This upstream release fixes
[GHSA-vxvm-j4xq-q7m4](https://github.com/astral-sh/ty/security/advisories/GHSA-vxvm-j4xq-q7m4),
a use-after-free during incremental type checking that can result in arbitrary code execution when
analyzing a specially crafted Python project. Anyone running ty-extended on untrusted code should
use this release or newer.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- The upstream merge was textually clean: no plugin surface needed porting. The release is
    inference correctness work and LSP improvements — closed-document requests, external-file
    project selection — plus the security fix above, which applies to plugin sessions equally.

## 0.83.0

Built on [ty 0.0.83](https://github.com/astral-sh/ty/releases/tag/0.0.83).

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- The upstream merge was textually clean: no plugin surface needed porting. The release is
    inference correctness work — bound-method fidelity, partial-application hangs, alias cycles —
    which reaches plugins as more precise types in the same hook requests.
- Upstream added suspicious-condition diagnostics for `Callable`, `Iterable`, `Iterator`, and
    `Generator` used in boolean contexts; plugin-provided types are subject to these checks like
    any other type.

## 0.82.0

Built on [ty 0.0.82](https://github.com/astral-sh/ty/releases/tag/0.0.82).

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- Upstream rewrote the file watcher as a bounded reconciliation loop over a salsa-tracked
    `watch_paths` set (for script-dependency watching in CLI watch mode and the language server).
    Plugin manifest and reload paths are now registered through `watch_paths`, so they are part of
    the watched-path cache key and remain watched exactly as before.
- `BoundMethod::function` and `SubclassOfType::try_from_instance` changed signature upstream
    (bound-method receiver comparison, and `type[A & B]` support). Plugin callee routing and
    plugin type-expression parsing were ported to the new signatures; observable plugin behaviour
    is unchanged.
- The release is inference work — recursive implicit type aliases, tuple and intersection
    precision, constraint-set assignability — which reaches plugins as more precise types in the
    same hook requests.

## 0.81.0

Built on [ty 0.0.81](https://github.com/astral-sh/ty/releases/tag/0.0.81). Released 2026-09-26.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- Upstream refactored module resolution, moving file resolution into `ModuleDirectory`.
    Plugin stub overlays were ported to the new structure: overlay search paths resolve only
    `.pyi` stubs, unchanged from before.
- The release is inference and metaclass work — MRO ordering, enum member reassignment,
    descriptor handling — which reaches plugins as more precise types in the same hook requests.

## 0.80.0

Built on [ty 0.0.80](https://github.com/astral-sh/ty/releases/tag/0.0.80). Released 2026-09-26.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- `BoundMethodType::function` now returns `Option<FunctionType>`: a bound method can wrap a
    callable that is not a plain function. Plugin call routing (`plugin_callee`) follows
    upstream and skips those methods; plugin `call_return` hooks see the same surface as before.
- The release is otherwise inference stability and performance work — tuple variance, ParamSpec
    forwarding, callable identity — which reaches plugins as more precise types in the same hook
    requests.

## 0.79.0

Built on [ty 0.0.79](https://github.com/astral-sh/ty/releases/tag/0.0.79). Released 2026-09-26.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- Upstream added always-truthy/always-falsy diagnostics and several new diagnostic fixes
    (`Literal` annotations, `await` on redundant conditions, `assert_never` on final `elif`).
    These are produced before plugin hooks run; plugin-provided diagnostics and types are
    unaffected.

### Fork infrastructure

- Upstream now embeds archive checksums in the shell installer. The release pipeline gained a
    `generate-checksum-manifest` job (upstream `3c907bc`) that bakes the built archives' real
    sha256 sidecars into the local dist manifest, and the fork adopted upstream's Dockerised
    `release-smoke-test` action for wheel/sdist smoke tests.
- Upstream started publishing Docker images; the fork does not, so the `custom-build-docker`
    job is intentionally not wired in.

## 0.78.0

Built on [ty 0.0.78](https://github.com/astral-sh/ty/releases/tag/0.0.78). Released 2026-09-26.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- Upstream now reports deprecated property accessors on intersections. Member-lookup results
    carry an internal `ResolvedMember` wrapper for deprecation metadata; plugin-provided members
    participate as ordinary members and are unaffected.
- Upstream added the `disjoint-cast` rule and continued stabilising cyclic inference. These
    reach plugins as more precise types in the same hook requests; the protocol shape and the set
    of hooks are unchanged.
- Upstream refreshes uv workspace metadata when dependencies change. The plugin-runtime rebuild
    added in 0.76.0 covers that path as well, so plugin configuration stays in sync.

## 0.77.0

Built on [ty 0.0.77](https://github.com/astral-sh/ty/releases/tag/0.0.77). Released 2026-09-26.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- Upstream hardened inference under recursion: bounded protocol traversal in generic
    intersections, preserved recursion guards across nested type operations, and stabilised
    receiver inference and condition truthiness in cyclic cases. These reach plugins as more
    reliable types in the same hook requests; the protocol shape and the set of hooks are
    unchanged.

## 0.76.0

Built on [ty 0.0.76](https://github.com/astral-sh/ty/releases/tag/0.0.76). Released 2026-09-26.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- Upstream reworked project reloads: settings resolution moved into `Project::rediscover`, which
    is used both when a structural change reloads the project and when refreshed uv metadata is
    applied in the background. The fork's semantic-plugin runtime rebuild moved with it: a new
    `Db::set_semantic_plugin_runtime` hook keeps `[[plugins.plugin]]` configuration changes
    taking effect on both paths, including the deferred uv-sync path.
- Upstream extended dependency awareness: a `missing-direct-dependency` rule, direct-dependency
    checks in PEP 723 scripts, and synchronised script environments in the language server.
    Plugins running on such files see the script's resolved environment.
- Upstream added "Find references" support for pytest fixtures and continued inference and
    constraint-solver work. These reach plugins as more precise types in the same hook requests;
    the protocol shape and the set of hooks are unchanged.

## 0.75.0

Built on [ty 0.0.75](https://github.com/astral-sh/ty/releases/tag/0.0.75). Released 2026-09-26.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- Upstream added PEP 723 script-environment support: ty now initialises and refreshes a standalone
    script's own environment (in the CLI, and in watch mode on bounded workers). Plugins running on
    such scripts see the script's resolved environment rather than the enclosing project's.
- Upstream renamed `ValueSource::UvWorkspace` to `UvMetadata`, widening it to cover script
    metadata. The fork's plugin-configuration diagnostics follow the rename and describe those
    values as provided by "uv metadata".
- Upstream added a dedicated `missing-slot` diagnostic for `__slots__` classes. The ordering
    established in 0.74.0 is unchanged: on a slotted class that cannot carry an attribute, the
    instance-member lookup ends before any plugin is consulted.
- The rest of upstream's inference and constraint-solver work reaches plugins as more precise
    types in the same hook requests. The protocol shape and the set of hooks are unchanged.

## 0.74.0

Built on [ty 0.0.74](https://github.com/astral-sh/ty/releases/tag/0.0.74). Released 2026-08-23.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so plugins built against 0.60.0 continue to
load unchanged.

### Plugin behaviour

- Upstream added `__slots__` support, and that reaches instance-member plugins. On a class that
    declares `__slots__`, does not list the requested name, and inherits no instance `__dict__`,
    the instance-member lookup now ends before any plugin is consulted. Such an attribute cannot
    exist at runtime, so a plugin claiming it was claiming something Python would refuse; this
    aligns the plugin path with the runtime. Class-scope members are unaffected, as are classes
    that do not use `__slots__` or that keep a `__dict__`.
- Upstream's inference and constraint-solver work in this release reaches plugins as more precise
    types in the same hook requests. The protocol shape and the set of hooks are unchanged.

### Release infrastructure

- The binary build workflow sets `ACTIONS_CACHE_MODE: none` again. Upstream added it when it
    disabled cache-aware actions in release workflows; the 0.73.0 merge resolved that hunk by
    taking the fork's side wholesale and dropped the setting along with upstream's package name.
    Release builds no longer read from the Actions cache.
- Upstream's `astral-sh/setup-uv` pin moves to v10.0.1 across the workflows.

### Documentation

- The plugin authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.74.0,<0.75.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.73.0

Built on [ty 0.0.73](https://github.com/astral-sh/ty/releases/tag/0.0.73). Released 2026-08-22.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

### This release follows 0.70.0

There is no ty-extended 0.71.0 or 0.72.0. Upstream ty 0.0.71 began aborting with a stack overflow
on deeply nested expressions, and 0.0.72 still did; 0.0.73 is the first upstream release where that
case checks cleanly again. Both of those upstream versions were merged so the history stays
continuous, but neither was released, because a release that crashes on ordinary code is worse than
no release. Anyone on 0.70.0 should move straight to this one.

### Plugin behaviour

- Plugin settings resolve their paths against the configuration they came from rather than always
    against the project root. Upstream made options resolvable for a standalone script as well as a
    project, so a `plugin-stub-overlay-paths` entry written in a script's inline metadata resolves
    relative to that script's directory, exactly as every other path setting does. Project
    configuration is unaffected.
- Plugin configuration diagnostics can point at a script's inline metadata as their source,
    alongside config files, the CLI, editor settings, and uv workspace metadata.
- Upstream reworked member lookup to carry a result rather than a bare place, so a lookup can
    report why it failed. A member supplied by an extension enters that flow as a successful
    lookup; extensions see no change in the members they contribute.
- Upstream's inference improvements across these four releases reach extensions as more precise
    types in the same hook requests. The protocol shape is unchanged.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.73.0,<0.74.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.70.0

Built on [ty 0.0.70](https://github.com/astral-sh/ty/releases/tag/0.0.70). Released 2026-08-22.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

### Plugin behaviour

- Upstream reworked member lookup to carry a result rather than a bare place, so a lookup can now
    report why it failed. A member supplied by an extension enters that flow as a successful
    lookup, which is what it is; extensions see no change in the members they contribute or in the
    requests they receive.
- Upstream's inference improvements reach extensions as more precise types in the same hook
    requests. The protocol shape is unchanged.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.70.0,<0.71.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.69.0

Built on [ty 0.0.69](https://github.com/astral-sh/ty/releases/tag/0.0.69). Released 2026-08-22.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

### Plugin behaviour

- Upstream added a `strict-generic-narrowing` setting, which controls whether `isinstance()`,
    `issubclass()`, and `match` class patterns narrow unspecialized generic classes to their top
    materialization. Narrowed types reach extensions through hook requests, so a project that turns
    this on sends different — more conservative — type arguments for narrowed generics.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.69.0,<0.70.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.68.0

Built on [ty 0.0.68](https://github.com/astral-sh/ty/releases/tag/0.0.68). Released 2026-08-22.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

### Plugin behaviour

- Upstream's inference and diagnostic changes reach extensions as more precise types in the same
    hook requests. No plugin-facing API changed, and this release needed no fork adaptation at all:
    the plugin machinery now takes the same environment-threading shape as upstream's own code.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.68.0,<0.69.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.67.0

Built on [ty 0.0.67](https://github.com/astral-sh/ty/releases/tag/0.0.67). Released 2026-08-22.

No breaking changes for extensions, and no change to the wire protocol. `ty_plugin_protocol` and
`ty_plugin_sdk` stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against
0.60.0 continue to load unchanged.

### Plugin behaviour

- Plugin configuration now lives in its own tracked input rather than on ty's program. Upstream
    replaced the single global program with a per-project value resolved through each file, so the
    plugin registry, which is project-wide, was moved onto an input of its own. Changing the
    configured plugins still invalidates every check that consulted them, and extensions see no
    difference; the change keeps the plugin machinery independent of ty's program representation.
- Plugin hooks resolve types against the environment of what they are anchored to — the class being
    transformed, or the file a call or mutation occurs in — rather than a single global program.
    On a project with one Python version, which is every project ty supports today, this resolves
    exactly as before.
- Upstream's generic inference improvements (`TypedDict`s inferred through synthesized
    constructors and unpacked `TypedDict`s, class type parameters preserved through generic
    decorators and constructor inference, and others) change the inferred types that reach
    extensions through hook requests. The protocol shape is unchanged.
- Upstream now rejects unhashable objects for `Hashable` protocols, unsupported
    `dataclass_transform` parameters, and `Self` with incompatible explicit receiver annotations.
    Code upstream now rejects no longer reaches a plugin's hooks as valid.

### Configuration

- Upstream removed the deprecated `src.root` setting in favour of `environment.root`. The fork's
    `plugin-stub-overlay-paths` setting is unaffected and continues to sit alongside it.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.67.0,<0.68.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.66.0

Built on [ty 0.0.66](https://github.com/astral-sh/ty/releases/tag/0.0.66). Released 2026-08-22.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

### Plugin behaviour

- Upstream now treats user-provided extra search paths as capable of holding third-party code, and
    recognizes Pydantic models installed there. Classes reached through an extra search path now
    take the same third-party path as site-packages classes, so extensions receive class-transform
    and member requests for models they previously never saw.
- Upstream's inference improvements (exact numeric types preserved in covariant collections, bounds
    and constraints respected in generic materializations, `TypeVarTuple` context preserved during
    `Generic` recovery, and others) change the inferred types that reach extensions through hook
    requests. The protocol shape is unchanged; extensions see more precise types in the same
    representations as before.
- Upstream now rejects `ClassVar` and `Final` qualifiers in `NamedTuple` fields and diagnoses
    specializing a non-generic class. Code upstream now rejects no longer reaches a plugin's
    class-transform hook as a valid class.
- Plugin requests resolve types against the program's configured Python version. Upstream replaced
    the single global program with per-file environments in this release; plugin queries are
    project-wide rather than anchored to one file, so they continue to use the program version,
    which is the version they already saw.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.66.0,<0.67.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.65.0

Built on [ty 0.0.65](https://github.com/astral-sh/ty/releases/tag/0.0.65). Released 2026-07-31.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

### Plugin behaviour

- Upstream's frozen-dataclass work changes how attribute assignment and deletion on frozen
    dataclasses are validated, including delegation through `super()` for non-fields. Mutations that
    upstream now rejects or resolves differently can change which writes reach an extension's
    `validate_mutation` hook.
- Upstream's inference improvements (tagged-union narrowing across all type kinds, constrained
    TypeVar solutions, constructor-overload filtering, and others) change the inferred types that
    reach extensions through hook requests. The protocol shape is unchanged; extensions see more
    precise types in the same representations as before.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.65.0,<0.66.0`.
- The `ruff` submodule gains an agent skill (`aligning-with-upstream-versions`) documenting the
    upstream alignment and release procedure used by this fork.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

Upstream's changes are not duplicated here. Every release below names the upstream ty version it is
built on and links to that release's notes.

## 0.64.0

Built on [ty 0.0.64](https://github.com/astral-sh/ty/releases/tag/0.0.64). Released 2026-07-29.

No breaking changes, and no change to the wire protocol. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

### Plugin behaviour

- Upstream now discovers uv workspace roots, adding uv workspace metadata as a configuration
    source. Plugin configuration diagnostics report this origin, and a Python environment supplied
    by uv workspace metadata is used when resolving the site-packages directories that extension
    discovery searches.
- Upstream's inference improvements (identity narrowing for NewTypes, tagged-union narrowing,
    `Self` preserved in `__new__` calls, and others) change the inferred types that reach
    extensions through hook requests. The protocol shape is unchanged; extensions see more precise
    types in the same representations as before.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.64.0,<0.65.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.63.0

Built on [ty 0.0.63](https://github.com/astral-sh/ty/releases/tag/0.0.63). Released 2026-07-29.

No breaking changes, and no change to plugin behaviour. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

### Plugin behaviour

- Upstream widened where intersection types are inferred: equality narrowing now applies across
    non-final classes, and narrowing a `match` subject to `A & B` now infers `x.attr` as the
    intersection of the two attribute types. The protocol has no intersection form, so such a type
    reaches an extension as an opaque type expression rendered from ty's display output, such as
    `A & B`, rather than a structured snapshot. This is the pre-existing representation for
    intersections; only the number of positions that produce one has grown. Extensions that match
    on structured snapshots see these types fall through to their expression fallback.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.63.0,<0.64.0`.
- The installation guide continues to omit upstream's mise and Docker sections, which install
    Astral's `ty` rather than this fork.

## 0.62.0

Built on [ty 0.0.62](https://github.com/astral-sh/ty/releases/tag/0.0.62). Released 2026-07-22.

No breaking changes, and no change to plugin behaviour. `ty_plugin_protocol` and `ty_plugin_sdk`
stay at `0.0.4`, and the wire protocol stays at `0.3`, so extensions built against 0.60.0 continue
to load unchanged.

Upstream added per-file `rules` and `analysis` overrides sourced from PEP 723 script metadata.
Plugin configuration is resolved per project rather than per file, so extensions continue to apply
to scripts that carry their own inline metadata.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.62.0,<0.63.0`.
- The installation guide omits upstream's new mise and Docker sections. Both install Astral's `ty`
    rather than this fork, which publishes neither a mise package nor a container image.

## 0.61.0

Built on [ty 0.0.61](https://github.com/astral-sh/ty/releases/tag/0.0.61). Released 2026-07-21.

No breaking changes. `ty_plugin_protocol` and `ty_plugin_sdk` stay at `0.0.4`, and the wire
protocol stays at `0.3`, so extensions built against 0.60.0 continue to load unchanged.

### Plugin behaviour

- Upstream generalised a tuple's variable-length segment so it may be an unpacked `TypeVarTuple`,
    as in `tuple[*Ts]`. The protocol has no representation for a type variable, so a segment of
    that kind is now reported to extensions as a variable-length tuple whose element type is
    `object`. Reporting it as a fixed-length tuple instead would have told extensions the wrong
    arity.

### Documentation

- The extension authoring guide and both plugin crate READMEs show a `ty_compatibility` range of
    `>=0.61.0,<0.62.0`.

## 0.60.0

Built on [ty 0.0.60](https://github.com/astral-sh/ty/releases/tag/0.0.60). Released 2026-07-21.

### Breaking changes

- The `ty_plugin_protocol` and `ty_plugin_sdk` crates are released as `0.0.4`.
    `PluginRequest::ValidateMutation` and `PluginResponse::Manifest` now hold their payloads in a
    `Box`, because each variant was several times larger than its siblings and every request and
    response paid for it. Extension authors must recompile, but **the wire format is unchanged** —
    `Box` is transparent to serde — so `CURRENT_PROTOCOL_VERSION` stays at `0.3` and manifests
    published against earlier SDK versions continue to negotiate successfully.

### Documentation

- The `plugins` configuration options now appear in the configuration reference and the JSON
    schema. They had been missing since the options were introduced.
- The extension authoring guide and both plugin crate READMEs showed an example
    `ty_compatibility` range of `>=0.59.0,<0.60.0`, which excludes this release; an extension
    following them would have been rejected during protocol negotiation. They now show
    `>=0.60.0,<0.61.0`.
- The standalone installer URLs in the installation guide had been stale since `0.58.2`.

## Earlier releases

These predate this changelog, so only their upstream base is recorded.

| ty-extended                                                                | Upstream base                                                    | Released   |
| -------------------------------------------------------------------------- | ---------------------------------------------------------------- | ---------- |
| [0.59.1](https://github.com/regularkevvv/ty-extended/releases/tag/v0.59.1) | [ty 0.0.59](https://github.com/astral-sh/ty/releases/tag/0.0.59) | 2026-07-14 |
| [0.59.0](https://github.com/regularkevvv/ty-extended/releases/tag/v0.59.0) | [ty 0.0.59](https://github.com/astral-sh/ty/releases/tag/0.0.59) | 2026-07-14 |
| [0.58.2](https://github.com/regularkevvv/ty-extended/releases/tag/v0.58.2) | [ty 0.0.58](https://github.com/astral-sh/ty/releases/tag/0.0.58) | 2026-07-14 |
| [0.58.1](https://github.com/regularkevvv/ty-extended/releases/tag/v0.58.1) | [ty 0.0.58](https://github.com/astral-sh/ty/releases/tag/0.0.58) | 2026-07-11 |
| [0.58.0](https://github.com/regularkevvv/ty-extended/releases/tag/v0.58.0) | [ty 0.0.58](https://github.com/astral-sh/ty/releases/tag/0.0.58) | 2026-07-10 |
